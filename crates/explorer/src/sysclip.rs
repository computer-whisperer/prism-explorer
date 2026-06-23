//! System-clipboard file interop — copy/paste files to and from other
//! file managers (Nautilus, Dolphin, Nemo, …).
//!
//! A file copy/cut is not plain text on the clipboard: it is a set of
//! typed MIME targets that carry the file list *and* the copy-vs-cut
//! intent. The host's `arboard` clipboard only speaks text/image, so the
//! file targets go through `wl-clipboard-rs` directly:
//!
//! - `x-special/gnome-copied-files` — `copy\n`/`cut\n` + `file://` URIs;
//!   the GNOME-family format (Nautilus, Nemo, Caja), and the one that
//!   carries the move intent we round-trip.
//! - `text/uri-list` — RFC 2483 CRLF-joined `file://` URIs; the generic
//!   target KDE/Dolphin and drag-targets read. KDE marks a cut with a
//!   separate `application/x-kde-cutselection` of `1`.
//! - `text/plain;charset=utf-8` — the bare paths, so a text field that
//!   pastes our clipboard gets something readable.
//!
//! This is **Wayland only**. X11 typed-selection ownership is a much
//! larger lift (own an event loop, become a selection owner) and arboard
//! won't expose it, so off Wayland every entry point no-ops and the app
//! falls back to its internal clipboard (own copy→paste still works, no
//! cross-app interop).
//!
//! Both the publish (serve) and read sides talk to the local compositor,
//! not the filesystem — the CephFS "IO off the UI thread" rule is about
//! stalls on the share. A read blocks on a pipe from the owning app, so
//! callers run it on a worker thread; preparing a publish is a fast local
//! round-trip done inline (the same place the host writes arboard text).

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use percent_encoding::{percent_decode, percent_encode, AsciiSet, NON_ALPHANUMERIC};

use explorer_io::Notifier;

/// A file selection read off the system clipboard: the paths and whether
/// the originating app marked it a cut (move) rather than a copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileList {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

/// The RFC 3986 `pchar` set for a URI path: percent-encode everything
/// that isn't an unreserved character or the path separator.
const PATH_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// Whether we're on a Wayland session — the only place this module does
/// anything. Off Wayland every entry point degrades to a no-op.
fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

// ---- target formatting (pure) --------------------------------------------

/// `file://` + the percent-encoded absolute path. Operates on raw bytes,
/// so non-UTF-8 paths round-trip exactly.
fn path_to_uri(path: &Path) -> String {
    format!(
        "file://{}",
        percent_encode(path.as_os_str().as_bytes(), PATH_SET)
    )
}

/// Parse one `file://` URI back into a path, decoding percent-escapes.
/// Drops an optional `//host` authority. `None` for any non-`file` URI.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.trim().strip_prefix("file://")?;
    // The path begins at the first '/'; anything before it is a host
    // (usually empty, as in `file:///path`).
    let start = rest.find('/')?;
    let decoded = percent_decode(&rest.as_bytes()[start..]).collect::<Vec<u8>>();
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

/// The GNOME `x-special/gnome-copied-files` body: an operation line
/// followed by one `file://` URI per path, newline-separated.
fn format_gnome_copied(paths: &[PathBuf], cut: bool) -> Vec<u8> {
    let mut s = String::from(if cut { "cut" } else { "copy" });
    for p in paths {
        s.push('\n');
        s.push_str(&path_to_uri(p));
    }
    s.into_bytes()
}

/// An RFC 2483 `text/uri-list`: CRLF-terminated `file://` URIs.
fn format_uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut s = String::new();
    for p in paths {
        s.push_str(&path_to_uri(p));
        s.push_str("\r\n");
    }
    s.into_bytes()
}

/// The bare absolute paths, newline-joined, for plain-text consumers.
fn format_plain_paths(paths: &[PathBuf]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// Parse `x-special/gnome-copied-files`: first line is `copy`/`cut`, the
/// rest are `file://` URIs. `None` if the leading operation is missing.
fn parse_gnome_copied(bytes: &[u8]) -> Option<FileList> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.split('\n');
    let cut = match lines.next()?.trim() {
        "cut" => true,
        "copy" => false,
        _ => return None,
    };
    let paths = lines.filter_map(uri_to_path).collect();
    Some(FileList { paths, cut })
}

/// Parse a `text/uri-list`: one URI per line, `#` comment lines and
/// blanks skipped (RFC 2483).
fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(uri_to_path)
        .collect()
}

// ---- Wayland clipboard (publish + read) ----------------------------------

/// Publish `paths` to the system clipboard as a file copy (or cut),
/// served from a background thread so external apps can paste them.
///
/// Returns an ownership flag that stays `true` while we hold the
/// selection and flips to `false` — with a `notify()` redraw poke — once
/// another application takes the clipboard. `None` off Wayland or when
/// the compositor lacks `wlr-data-control` (the caller then treats its
/// internal clipboard as authoritative). Preparing the data source is a
/// quick local round-trip done inline; only the open-ended `serve()`
/// loop runs on the thread.
pub fn publish(paths: &[PathBuf], cut: bool, notify: Notifier) -> Option<Arc<AtomicBool>> {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, ServeRequests, Source};

    // Unit tests run against the developer's real Wayland session; never
    // take over their actual clipboard from a test.
    if cfg!(test) || !is_wayland() || paths.is_empty() {
        return None;
    }

    let mk = |bytes: Vec<u8>, mime: &str| MimeSource {
        source: Source::Bytes(bytes.into_boxed_slice()),
        mime_type: MimeType::Specific(mime.to_string()),
    };
    let sources = vec![
        mk(
            format_gnome_copied(paths, cut),
            "x-special/gnome-copied-files",
        ),
        mk(format_uri_list(paths), "text/uri-list"),
        mk(
            if cut { b"1".to_vec() } else { b"0".to_vec() },
            "application/x-kde-cutselection",
        ),
        mk(format_plain_paths(paths), "text/plain;charset=utf-8"),
    ];

    let mut opts = Options::new();
    opts.serve_requests(ServeRequests::Unlimited);
    // We serve the requests ourselves on the thread below; `foreground`
    // tells wl-clipboard-rs not to fork a server of its own (forking this
    // multithreaded wgpu process would be unsound), and `prepare_copy_*`
    // in fact asserts it is set.
    opts.foreground(true);
    // We offer our own explicit text target; don't let wl-clipboard-rs
    // bolt on STRING/UTF8_STRING/etc. around it.
    opts.omit_additional_text_mime_types(true);
    let prepared = match opts.prepare_copy_multi(sources) {
        Ok(prepared) => prepared,
        Err(e) => {
            tracing::warn!(error = %e, "could not take the system clipboard");
            return None;
        }
    };

    let owned = Arc::new(AtomicBool::new(true));
    let owned_thread = owned.clone();
    let spawned = std::thread::Builder::new()
        .name("sysclip-serve".into())
        .spawn(move || {
            // Blocks until another app takes the clipboard (or error).
            if let Err(e) = prepared.serve() {
                tracing::warn!(error = %e, "clipboard serve ended with an error");
            }
            owned_thread.store(false, Ordering::SeqCst);
            notify();
        });
    match spawned {
        Ok(_) => Some(owned),
        Err(e) => {
            tracing::error!(error = %e, "sysclip serve thread failed to spawn");
            None
        }
    }
}

/// Read a file selection off the system clipboard, preferring the GNOME
/// target (which carries the cut intent) and falling back to a
/// `text/uri-list` plus the KDE cut marker. `None` if the clipboard
/// holds no file targets (or off Wayland). Blocks on the owning app —
/// run it on a worker thread.
pub fn read() -> Option<FileList> {
    if cfg!(test) || !is_wayland() {
        return None;
    }
    if let Some(bytes) = read_target("x-special/gnome-copied-files") {
        if let Some(list) = parse_gnome_copied(&bytes) {
            if !list.paths.is_empty() {
                return Some(list);
            }
        }
    }
    if let Some(bytes) = read_target("text/uri-list") {
        let paths = parse_uri_list(&bytes);
        if !paths.is_empty() {
            let cut = read_target("application/x-kde-cutselection")
                .is_some_and(|b| b.iter().find(|c| !c.is_ascii_whitespace()) == Some(&b'1'));
            return Some(FileList { paths, cut });
        }
    }
    None
}

/// Fetch one clipboard MIME target's bytes, or `None` if it isn't
/// offered / the clipboard is empty.
fn read_target(mime: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    use wl_clipboard_rs::paste::{get_contents, ClipboardType, Error, MimeType, Seat};

    match get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        MimeType::Specific(mime),
    ) {
        Ok((mut pipe, _actual)) => {
            let mut buf = Vec::new();
            pipe.read_to_end(&mut buf).ok()?;
            Some(buf)
        }
        // Empty / no such target / no seat are ordinary "nothing here".
        Err(Error::NoMimeType) | Err(Error::ClipboardEmpty) | Err(Error::NoSeats) => None,
        Err(e) => {
            tracing::debug!(error = %e, mime, "clipboard read failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_uri_round_trips_spaces_and_unicode() {
        let p = PathBuf::from("/home/a b/€.txt");
        let uri = path_to_uri(&p);
        assert_eq!(uri, "file:///home/a%20b/%E2%82%AC.txt");
        assert_eq!(uri_to_path(&uri), Some(p));
    }

    #[test]
    fn uri_to_path_drops_host_and_rejects_other_schemes() {
        assert_eq!(
            uri_to_path("file://localhost/etc/hosts"),
            Some(PathBuf::from("/etc/hosts"))
        );
        assert_eq!(uri_to_path("file:///plain"), Some(PathBuf::from("/plain")));
        assert_eq!(uri_to_path("http://example.com/x"), None);
        assert_eq!(uri_to_path("file://nohost"), None);
    }

    #[test]
    fn gnome_copied_round_trips_with_intent() {
        let paths = vec![PathBuf::from("/a/one"), PathBuf::from("/a/two")];
        let copied = format_gnome_copied(&paths, false);
        let list = parse_gnome_copied(&copied).unwrap();
        assert!(!list.cut);
        assert_eq!(list.paths, paths);

        let cut = parse_gnome_copied(&format_gnome_copied(&paths, true)).unwrap();
        assert!(cut.cut);
        assert_eq!(cut.paths, paths);
    }

    #[test]
    fn gnome_copied_requires_an_operation_line() {
        assert!(parse_gnome_copied(b"file:///a\nfile:///b").is_none());
        assert!(parse_gnome_copied(b"").is_none());
    }

    #[test]
    fn uri_list_skips_comments_and_blanks() {
        let body = b"# the comment\r\nfile:///a/one\r\n\r\nfile:///a/two\r\n";
        assert_eq!(
            parse_uri_list(body),
            vec![PathBuf::from("/a/one"), PathBuf::from("/a/two")]
        );
    }

    #[test]
    fn uri_list_format_is_crlf_terminated() {
        let paths = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert_eq!(format_uri_list(&paths), b"file:///a\r\nfile:///b\r\n");
    }
}
