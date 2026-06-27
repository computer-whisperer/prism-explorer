//! `org.freedesktop.FileManager1` — the freedesktop "show me this in
//! the file manager" service. Browsers' *Open containing folder*,
//! download managers, chat clients, etc. all call it.
//!
//! Served from the running explorer on a detached thread (zbus's
//! blocking API; the bus connection lives as long as the thread).
//! Method calls translate to [`Msg::OpenLocation`] posted to the UI
//! thread — the same navigate-and-select path the keyboard uses, so a
//! `ShowItems` on a file ends with that file selected and previewed.
//!
//! Only one process can own the name; if another file manager holds
//! it, we log and carry on as a plain browser.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use explorer_io::Notifier;

use crate::model::Msg;

struct FileManager1 {
    tx: Sender<Msg>,
    notify: Notifier,
}

impl FileManager1 {
    fn open(&self, dir: PathBuf, select: Option<OsString>) {
        tracing::info!(dir = %dir.display(), select = ?select, "FileManager1 request");
        let _ = self.tx.send(Msg::OpenLocation { dir, select });
        (self.notify)();
    }

    /// First parseable URI wins — the UI has one window, and one
    /// navigation is more useful than none for a multi-URI call.
    fn first_path<'a>(&self, uris: impl IntoIterator<Item = &'a String>) -> Option<PathBuf> {
        let mut uris = uris.into_iter();
        let path = uris.find_map(|u| match file_uri_to_path(u) {
            Some(p) => Some(p),
            None => {
                tracing::warn!(uri = %u, "FileManager1: ignoring unusable URI");
                None
            }
        })?;
        if uris.next().is_some() {
            tracing::debug!("FileManager1: multiple URIs, showing the first");
        }
        Some(path)
    }
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager1 {
    fn show_folders(&self, uris: Vec<String>, _startup_id: String) {
        if let Some(dir) = self.first_path(&uris) {
            self.open(dir, None);
        }
    }

    fn show_items(&self, uris: Vec<String>, _startup_id: String) {
        if let Some(item) = self.first_path(&uris) {
            let Some(parent) = item.parent().map(PathBuf::from) else {
                // An item with no parent is `/` itself.
                return self.open(item, None);
            };
            let select = item.file_name().map(OsString::from);
            self.open(parent, select);
        }
    }

    /// No properties dialog (yet) — fall back to showing the items,
    /// which is strictly more useful than silence.
    fn show_item_properties(&self, uris: Vec<String>, startup_id: String) {
        self.show_items(uris, startup_id);
    }
}

/// Own the name and serve until process exit. Failure (no session bus,
/// name taken by another file manager) downgrades to a log line.
pub fn spawn(tx: Sender<Msg>, notify: Notifier) {
    let spawned = std::thread::Builder::new()
        .name("filemanager1".into())
        .spawn(move || match serve(tx, notify) {
            Ok(_conn) => {
                tracing::info!("serving org.freedesktop.FileManager1");
                // The connection's executor handles calls; this thread
                // just keeps it alive.
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "FileManager1 unavailable (continuing without it)");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "FileManager1 thread failed to spawn");
    }
}

fn serve(tx: Sender<Msg>, notify: Notifier) -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::session()?
        .name("org.freedesktop.FileManager1")?
        .serve_at("/org/freedesktop/FileManager1", FileManager1 { tx, notify })?
        .build()
}

/// `file://` URI → local path: reject other schemes and non-local
/// authorities, percent-decode into raw bytes (paths aren't UTF-8).
/// Shared with the places sidebar's GTK-bookmarks reader.
pub(crate) fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    let rest = uri.strip_prefix("file://")?;
    // Authority component: empty or localhost means this machine.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) if rest[..i].eq_ignore_ascii_case("localhost") => &rest[i..],
        _ => return None,
    };

    let mut bytes = Vec::with_capacity(path.len());
    let mut it = path.bytes();
    while let Some(b) = it.next() {
        if b != b'%' {
            bytes.push(b);
            continue;
        }
        let hi = it.next()?;
        let lo = it.next()?;
        let hex = |c: u8| (c as char).to_digit(16);
        bytes.push((hex(hi)? * 16 + hex(lo)?) as u8);
    }
    if bytes.is_empty() || bytes[0] != b'/' || bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_parsing() {
        assert_eq!(
            file_uri_to_path("file:///ceph/photos/cat.jxr"),
            Some(PathBuf::from("/ceph/photos/cat.jxr"))
        );
        assert_eq!(
            file_uri_to_path("file://localhost/tmp/x"),
            Some(PathBuf::from("/tmp/x"))
        );
        // Percent-decoding, including non-UTF-8 bytes.
        assert_eq!(
            file_uri_to_path("file:///tmp/with%20space%2Bplus"),
            Some(PathBuf::from("/tmp/with space+plus"))
        );
        assert_eq!(
            file_uri_to_path("file:///tmp/%FF").map(|p| p.into_os_string().len()),
            Some(6)
        );

        // Rejected: other schemes, remote hosts, truncated escapes,
        // NUL smuggling, empty.
        assert_eq!(file_uri_to_path("https://example.com/x"), None);
        assert_eq!(file_uri_to_path("file://nas/share/x"), None);
        assert_eq!(file_uri_to_path("file:///tmp/bad%2"), None);
        assert_eq!(file_uri_to_path("file:///tmp/%00"), None);
        assert_eq!(file_uri_to_path("file://"), None);
    }
}
