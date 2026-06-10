//! `org.freedesktop.impl.portal.FileChooser` — the portal *backend*
//! xdg-desktop-portal delegates open/save dialogs to. Every
//! portal-using app's file dialog becomes a prism-explorer picker
//! window in this process.
//!
//! Shape of a request: xdg-desktop-portal calls e.g. `OpenFile` and
//! expects the reply only once the user has chosen — so the handlers
//! are async and await the picker's answer while zbus keeps
//! dispatching (in particular `Close` on the per-request Request
//! object, which is how callers abort a dialog). The picker itself
//! runs in a window of the main host loop: the handler posts
//! [`HostCommand::OpenWindow`] with a [`PickerApp`] and a reply
//! closure; accept/cancel/window-close all funnel into that closure
//! exactly once (dropping the app unanswered counts as cancel).
//!
//! Deliberately unimplemented for now: `choices` (ignored),
//! `multiple` (one URI comes back), modal-to-parent (`parent_window`
//! is ignored — the dialog is a plain toplevel), and the
//! `current_filter` result key (the filter active at accept time is
//! not reported back).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use winit::event_loop::EventLoopProxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use explorer_io::{Notifier, Pool};
use explorer_previews::Registry;
use explorer_thumbs::ThumbCache;

use crate::app::ExplorerApp;
use crate::host::{HostCommand, WindowSpec};
use crate::model::FileFilter;
use crate::picker::{PickerApp, PickerKind, PickerRequest};

/// Portal response codes (`org.freedesktop.portal.Request`).
const RESPONSE_OK: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_OTHER: u32 = 2;

/// Everything needed to assemble a picker window per request. The IO
/// pool is *not* here on purpose — each picker spawns its own (pool
/// generations are bumped per navigation, so sharing one across
/// windows would cancel each other's jobs).
pub struct PickerDeps {
    pub notifier: Notifier,
    pub registry: Arc<Registry>,
    pub thumbs: Arc<ThumbCache>,
    pub proxy: EventLoopProxy<HostCommand>,
}

/// Workers per picker dialog: enough to overlap stats and a decode,
/// small enough that transient dialogs stay cheap.
const PICKER_WORKERS: usize = 4;

struct FileChooser {
    deps: PickerDeps,
    /// Window tokens for HostCommand::{Open,Close}Window.
    next_token: AtomicU64,
}

/// The picker's eventual answer, plus how the dialog ended.
type Answer = Option<Vec<PathBuf>>;

impl FileChooser {
    /// Open a picker window and await its answer. Registers a
    /// `Request` object at `handle` for the duration so the caller
    /// can abort the dialog.
    async fn run_picker(
        &self,
        connection: &zbus::Connection,
        handle: ObjectPath<'_>,
        title: &str,
        request: PickerRequest,
    ) -> (u32, Answer) {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let proxy = self.deps.proxy.clone();
        let (tx, rx) = async_channel::bounded::<Answer>(1);

        let pool = Pool::spawn(PICKER_WORKERS, &format!("picker{token}-io"));
        let explorer = ExplorerApp::new(
            request.start_dir.clone(),
            pool.clone(),
            self.deps.notifier.clone(),
            self.deps.registry.clone(),
            self.deps.thumbs.clone(),
        );
        let app = PickerApp::new(
            request,
            explorer,
            Some(pool),
            Box::new(move |answer| {
                // Called once, on the UI thread; the channel holds one
                // slot so this never blocks.
                let _ = tx.try_send(answer);
            }),
            {
                let proxy = proxy.clone();
                Arc::new(move || {
                    let _ = proxy.send_event(HostCommand::CloseWindow { token });
                })
            },
        );

        // The Request object: callers Close() it to abort the dialog.
        let closed_by_caller = Arc::new(AtomicBool::new(false));
        let request_obj = Request {
            proxy: proxy.clone(),
            token,
            closed: closed_by_caller.clone(),
        };
        let handle = OwnedObjectPath::from(handle.to_owned());
        let registered = connection
            .object_server()
            .at(handle.clone(), request_obj)
            .await
            .unwrap_or(false);

        let sent = proxy.send_event(HostCommand::OpenWindow {
            token,
            spec: WindowSpec {
                title: title.to_string(),
                width: 1100.0,
                height: 760.0,
                app: Box::new(app),
            },
        });

        let answer = if sent.is_ok() {
            // Window-open failures drop the app, which answers None
            // through the same channel — no separate error path.
            rx.recv().await.ok().flatten()
        } else {
            tracing::warn!("host loop gone; failing picker request");
            None
        };

        if registered {
            let _ = connection
                .object_server()
                .remove::<Request, _>(&handle)
                .await;
        }

        let response = match (&answer, closed_by_caller.load(Ordering::Relaxed)) {
            (Some(_), _) => RESPONSE_OK,
            (None, true) => RESPONSE_OTHER,
            (None, false) => RESPONSE_CANCELLED,
        };
        (response, answer)
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    async fn open_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: ObjectPath<'_>,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let directory = bool_option(&options, "directory");
        if bool_option(&options, "multiple") {
            tracing::debug!("OpenFile: multiple requested; returning a single choice");
        }
        let (filters, current_filter) = filter_options(&options);
        let request = PickerRequest {
            kind: PickerKind::Open { directory },
            accept_label: accept_label(&options, "Open"),
            start_dir: start_dir(&options),
            current_name: String::new(),
            filters,
            current_filter,
        };
        tracing::info!(%title, directory, "portal OpenFile");
        let (response, answer) = self.run_picker(connection, handle, &title, request).await;
        (response, uris_result(answer))
    }

    async fn save_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: ObjectPath<'_>,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        // current_file (an existing file being re-saved) wins over the
        // current_folder + current_name pair when present.
        let (start_dir, current_name) = match path_option(&options, "current_file") {
            Some(file) => (
                file.parent().map(PathBuf::from).unwrap_or_else(home_dir),
                file.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            None => (
                start_dir(&options),
                string_option(&options, "current_name").unwrap_or_default(),
            ),
        };
        let (filters, current_filter) = filter_options(&options);
        let request = PickerRequest {
            kind: PickerKind::Save,
            accept_label: accept_label(&options, "Save"),
            start_dir,
            current_name,
            filters,
            current_filter,
        };
        tracing::info!(%title, "portal SaveFile");
        let (response, answer) = self.run_picker(connection, handle, &title, request).await;
        (response, uris_result(answer))
    }

    /// Batch save: the caller supplies the file *names*; the user
    /// chooses the directory they land in.
    async fn save_files(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: ObjectPath<'_>,
        _app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let names: Vec<OsString> = options
            .get("files")
            .and_then(|v| <Vec<Vec<u8>>>::try_from(v.try_clone().ok()?).ok())
            .map(|files| files.into_iter().map(bytes_to_os).collect())
            .unwrap_or_default();
        // No filters: this picker chooses the *directory* the supplied
        // names land in (SaveFiles has no filter options in the spec).
        let request = PickerRequest {
            kind: PickerKind::Open { directory: true },
            accept_label: accept_label(&options, "Save"),
            start_dir: start_dir(&options),
            current_name: String::new(),
            filters: Vec::new(),
            current_filter: 0,
        };
        tracing::info!(%title, files = names.len(), "portal SaveFiles");
        let (response, answer) = self.run_picker(connection, handle, &title, request).await;
        let answer = answer.map(|dirs| {
            let dir = dirs.into_iter().next().unwrap_or_else(home_dir);
            names.iter().map(|n| dir.join(n)).collect()
        });
        (response, uris_result(answer))
    }
}

/// Per-request handle the caller can `Close()` to abort the dialog
/// (`org.freedesktop.impl.portal.Request`).
struct Request {
    proxy: EventLoopProxy<HostCommand>,
    token: u64,
    closed: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl Request {
    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        // Closing the window drops the picker, whose unanswered reply
        // resolves the pending method call.
        let _ = self
            .proxy
            .send_event(HostCommand::CloseWindow { token: self.token });
    }
}

/// Own the portal backend name and serve until process exit. Returns
/// whether the name was acquired (the caller leaves the host
/// resident only when it was). Failure downgrades to a log line —
/// the explorer is still a browser without it.
pub fn spawn(deps: PickerDeps) -> bool {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("filechooser".into())
        .spawn(move || match serve(deps) {
            Ok(_conn) => {
                tracing::info!("serving org.freedesktop.impl.portal.FileChooser");
                let _ = ready_tx.send(true);
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "portal FileChooser unavailable (continuing without it)");
                let _ = ready_tx.send(false);
            }
        });
    match spawned {
        Ok(_) => ready_rx.recv().unwrap_or(false),
        Err(e) => {
            tracing::warn!(error = %e, "portal FileChooser thread failed to spawn");
            false
        }
    }
}

fn serve(deps: PickerDeps) -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::session()?
        .name("org.freedesktop.impl.portal.desktop.prism")?
        .serve_at(
            "/org/freedesktop/portal/desktop",
            FileChooser {
                deps,
                next_token: AtomicU64::new(1),
            },
        )?
        .build()
}

// ---- option / result helpers -------------------------------------------

fn bool_option(options: &HashMap<String, OwnedValue>, key: &str) -> bool {
    options
        .get(key)
        .and_then(|v| bool::try_from(v.try_clone().ok()?).ok())
        .unwrap_or(false)
}

fn string_option(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options
        .get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
}

/// `ay` path options arrive as NUL-terminated raw bytes.
fn path_option(options: &HashMap<String, OwnedValue>, key: &str) -> Option<PathBuf> {
    let bytes: Vec<u8> = options
        .get(key)
        .and_then(|v| <Vec<u8>>::try_from(v.try_clone().ok()?).ok())?;
    let os = bytes_to_os(bytes);
    (!os.is_empty()).then(|| PathBuf::from(os))
}

fn bytes_to_os(mut bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt as _;
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    OsString::from_vec(bytes)
}

/// The caller's accept label, with GTK `_` mnemonic markers stripped
/// ("_Open" → "Open", "__" → "_").
fn accept_label(options: &HashMap<String, OwnedValue>, default: &str) -> String {
    let Some(label) = string_option(options, "accept_label") else {
        return default.to_string();
    };
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        if c == '_' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        default.to_string()
    } else {
        out
    }
}

/// Wire shape of one portal filter: `(sa(us))` — display name plus
/// (kind, pattern) alternatives, kind 0 = glob, 1 = mimetype.
type RawFilter = (String, Vec<(u32, String)>);

fn parse_filter((name, patterns): RawFilter) -> FileFilter {
    let mut filter = FileFilter {
        name,
        globs: Vec::new(),
        mimes: Vec::new(),
    };
    for (kind, pattern) in patterns {
        match kind {
            // Globs are matched case-insensitively against lowercased
            // names — lower the pattern once here.
            0 => filter.globs.push(pattern.to_lowercase()),
            1 => filter.mimes.push(pattern),
            unknown => tracing::debug!(unknown, %pattern, "skipping unknown filter kind"),
        }
    }
    filter
}

/// Parse `filters` + `current_filter` into the list the picker shows
/// and the index to start on. A `current_filter` that isn't in the
/// list (compared by name) is appended — per the spec it should match,
/// but GTK callers occasionally send a detached one.
fn filter_options(options: &HashMap<String, OwnedValue>) -> (Vec<FileFilter>, usize) {
    let mut filters: Vec<FileFilter> = options
        .get("filters")
        .and_then(|v| <Vec<RawFilter>>::try_from(v.try_clone().ok()?).ok())
        .map(|raw| raw.into_iter().map(parse_filter).collect())
        .unwrap_or_default();
    let current = options
        .get("current_filter")
        .and_then(|v| RawFilter::try_from(v.try_clone().ok()?).ok())
        .map(parse_filter);
    let idx = match current {
        Some(cur) => match filters.iter().position(|f| f.name == cur.name) {
            Some(i) => i,
            None => {
                filters.push(cur);
                filters.len() - 1
            }
        },
        None => 0,
    };
    (filters, idx)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn start_dir(options: &HashMap<String, OwnedValue>) -> PathBuf {
    match path_option(options, "current_folder") {
        Some(dir) if dir.is_absolute() => dir,
        _ => home_dir(),
    }
}

/// `file://` URI with sub-delim-safe percent-encoding (the inverse of
/// filemanager1's parser; paths are raw bytes, not UTF-8).
fn path_to_file_uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        // RFC 3986 unreserved + '/' stay literal; everything else
        // (spaces, '#', '?', '%', non-ASCII bytes) is escaped.
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(b as char)
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

fn uris_result(answer: Answer) -> HashMap<String, OwnedValue> {
    let mut results = HashMap::new();
    if let Some(paths) = answer {
        let uris: Vec<String> = paths.iter().map(|p| path_to_file_uri(p)).collect();
        if let Ok(value) = OwnedValue::try_from(Value::from(uris)) {
            results.insert("uris".to_string(), value);
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encoding() {
        assert_eq!(
            path_to_file_uri(std::path::Path::new("/ceph/photos/cat.jxr")),
            "file:///ceph/photos/cat.jxr"
        );
        assert_eq!(
            path_to_file_uri(std::path::Path::new("/tmp/with space+plus#frag")),
            "file:///tmp/with%20space%2Bplus%23frag"
        );
        use std::os::unix::ffi::OsStrExt as _;
        let raw = std::ffi::OsStr::from_bytes(b"/tmp/\xff");
        assert_eq!(
            path_to_file_uri(std::path::Path::new(raw)),
            "file:///tmp/%FF"
        );
    }

    #[test]
    fn filter_option_parsing() {
        // No options at all → no filters, index 0.
        assert_eq!(filter_options(&HashMap::new()), (Vec::new(), 0));

        let raw: Vec<RawFilter> = vec![
            (
                "Images".into(),
                vec![(0, "*.PNG".into()), (1, "image/jpeg".into())],
            ),
            ("All files".into(), vec![(0, "*".into())]),
            ("Weird".into(), vec![(7, "???".into())]),
        ];
        let mut options = HashMap::new();
        options.insert(
            "filters".to_string(),
            OwnedValue::try_from(Value::from(raw.clone())).unwrap(),
        );
        let (filters, idx) = filter_options(&options);
        assert_eq!(idx, 0);
        assert_eq!(filters.len(), 3);
        // Globs lowered; mimetypes kept; unknown kinds dropped.
        assert_eq!(filters[0].globs, ["*.png"]);
        assert_eq!(filters[0].mimes, ["image/jpeg"]);
        assert!(filters[2].globs.is_empty() && filters[2].mimes.is_empty());

        // current_filter selects by name…
        let all: RawFilter = ("All files".into(), vec![(0, "*".into())]);
        options.insert(
            "current_filter".to_string(),
            OwnedValue::try_from(Value::from(all)).unwrap(),
        );
        assert_eq!(filter_options(&options).1, 1);

        // …and a detached one is appended and selected.
        let detached: RawFilter = ("Detached".into(), vec![(0, "*.x".into())]);
        options.insert(
            "current_filter".to_string(),
            OwnedValue::try_from(Value::from(detached)).unwrap(),
        );
        let (filters, idx) = filter_options(&options);
        assert_eq!((filters.len(), idx), (4, 3));
        assert_eq!(filters[3].name, "Detached");
    }

    #[test]
    fn accept_label_mnemonics() {
        let mut options = HashMap::new();
        assert_eq!(accept_label(&options, "Open"), "Open");
        options.insert(
            "accept_label".to_string(),
            OwnedValue::try_from(Value::from("_Pick this")).unwrap(),
        );
        assert_eq!(accept_label(&options, "Open"), "Pick this");
    }
}
