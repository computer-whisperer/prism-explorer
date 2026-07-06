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
//! Deliberately unimplemented for now: `choices` (ignored) and
//! modal-to-parent (`parent_window` is ignored — the dialog is a plain
//! toplevel). `multiple` is honored — accept returns every marked file —
//! and the filter active at accept is reported back in `current_filter`.

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
use crate::picker::{PickerApp, PickerKind, PickerOutcome, PickerRequest};
use crate::state::{Chosen, RequestKind};

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
    /// Shared last-used-location store, also held by the browser
    /// window. Seeds `start_dir` (per-app memory, then the global
    /// fallback) and, on accept, records where each app last chose.
    pub store: Arc<crate::state::Store>,
    /// Persisted browser preferences (show hidden, sort, view), shared
    /// with the standalone browser so a change in either sticks for both.
    pub settings: Arc<crate::settings::Store>,
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

/// The picker's eventual answer: the accepted outcome, or `None` for a
/// cancel / closed window.
type Answer = Option<PickerOutcome>;

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
            // The real history store — the picker *reads* it (the "Recent"
            // sidebar, the start-dir/filter seeding) but `record_visits:
            // false` keeps its browsing out of the log. The dialog's only
            // contribution is the request event recorded on completion.
            self.deps.store.clone(),
            self.deps.settings.clone(),
            false,
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

    /// Log one completed request to the history. `paths` are the final
    /// returned paths (already name-joined for SaveFiles); `filter` is the
    /// active one, if any. The directory context is the parent of the
    /// first path — for a file that's its folder, for a chosen directory
    /// (SaveFiles) that's the directory itself. Cancels (`paths` `None`)
    /// are recorded too, with their request context.
    fn log_request(
        &self,
        app_id: &str,
        kind: RequestKind,
        filters: Vec<String>,
        paths: Option<&[PathBuf]>,
        filter: Option<&FileFilter>,
    ) {
        let chosen = paths.map(|paths| Chosen {
            dir: paths
                .first()
                .and_then(|p| p.parent())
                .map(|d| d.to_path_buf()),
            paths: paths.to_vec(),
            filter: filter.map(|f| f.name.clone()),
        });
        self.deps
            .store
            .record_request(app_id, kind, filters, chosen);
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    async fn open_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: ObjectPath<'_>,
        app_id: String,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let directory = bool_option(&options, "directory");
        let multiple = bool_option(&options, "multiple");
        let kind = if directory {
            RequestKind::OpenDir
        } else {
            RequestKind::OpenFile
        };
        let (filters, current_filter) = filter_options(&options, &app_id, kind, &self.deps.store);
        let filter_names: Vec<String> = filters.iter().map(|f| f.name.clone()).collect();
        let request = PickerRequest {
            kind: PickerKind::Open {
                directory,
                multiple,
            },
            accept_label: accept_label(&options, "Open"),
            start_dir: start_dir(&options, &app_id, &self.deps.store),
            current_name: String::new(),
            filters,
            current_filter,
        };
        tracing::info!(%title, directory, multiple, "portal OpenFile");
        let (response, answer) = self.run_picker(connection, handle, &title, request).await;
        let filter = answer.as_ref().and_then(|o| o.filter.clone());
        let paths = answer.map(|o| o.paths);
        self.log_request(
            &app_id,
            kind,
            filter_names,
            paths.as_deref(),
            filter.as_ref(),
        );
        (response, result_map(paths, filter))
    }

    async fn save_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: ObjectPath<'_>,
        app_id: String,
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
                start_dir(&options, &app_id, &self.deps.store),
                string_option(&options, "current_name").unwrap_or_default(),
            ),
        };
        let (filters, current_filter) =
            filter_options(&options, &app_id, RequestKind::SaveFile, &self.deps.store);
        let filter_names: Vec<String> = filters.iter().map(|f| f.name.clone()).collect();
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
        let filter = answer.as_ref().and_then(|o| o.filter.clone());
        let paths = answer.map(|o| o.paths);
        self.log_request(
            &app_id,
            RequestKind::SaveFile,
            filter_names,
            paths.as_deref(),
            filter.as_ref(),
        );
        (response, result_map(paths, filter))
    }

    /// Batch save: the caller supplies the file *names*; the user
    /// chooses the directory they land in.
    async fn save_files(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: ObjectPath<'_>,
        app_id: String,
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
            kind: PickerKind::Open {
                directory: true,
                multiple: false,
            },
            accept_label: accept_label(&options, "Save"),
            start_dir: start_dir(&options, &app_id, &self.deps.store),
            current_name: String::new(),
            filters: Vec::new(),
            current_filter: 0,
        };
        tracing::info!(%title, files = names.len(), "portal SaveFiles");
        let (response, answer) = self.run_picker(connection, handle, &title, request).await;
        // Join the caller's names onto the chosen directory, then log the
        // resulting file paths (their shared parent — the chosen dir — is
        // the directory context). SaveFiles has no filters.
        let paths: Option<Vec<PathBuf>> = answer.map(|outcome| {
            let dir = outcome.paths.into_iter().next().unwrap_or_else(home_dir);
            names.iter().map(|n| dir.join(n)).collect()
        });
        self.log_request(
            &app_id,
            RequestKind::SaveFiles,
            Vec::new(),
            paths.as_deref(),
            None,
        );
        (response, result_map(paths, None))
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
/// but GTK callers occasionally send a detached one. When the caller
/// gives no `current_filter`, fall back to the filter this app last
/// accepted with in the same `kind` of dialog (matched by name); the
/// caller's explicit choice always wins, mirroring how `current_folder`
/// beats the remembered directory.
fn filter_options(
    options: &HashMap<String, OwnedValue>,
    app_id: &str,
    kind: RequestKind,
    store: &crate::state::Store,
) -> (Vec<FileFilter>, usize) {
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
        // No explicit choice: pre-select the app's remembered filter if
        // it's still offered, else the first.
        None => store
            .app_last_filter(app_id, kind)
            .and_then(|name| filters.iter().position(|f| f.name == name))
            .unwrap_or(0),
    };
    (filters, idx)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The directory a picker opens in: the caller's `current_folder` when
/// given, else where this app (`app_id`) last accepted, else the global
/// last-browsed location, else home. Remembered paths aren't stat'd — if
/// one has since vanished the listing surfaces the error and the user
/// navigates away.
fn start_dir(
    options: &HashMap<String, OwnedValue>,
    app_id: &str,
    store: &crate::state::Store,
) -> PathBuf {
    if let Some(dir) = path_option(options, "current_folder").filter(|d| d.is_absolute()) {
        return dir;
    }
    store
        .app_last_dir(app_id)
        .or_else(|| store.last_dir())
        .unwrap_or_else(home_dir)
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

/// Serialize a [`FileFilter`] back to the portal `(sa(us))` wire shape —
/// the inverse of [`parse_filter`]. Globs come back lowercased (we
/// lowered them on the way in for case-insensitive matching); the name,
/// which is the filter's identity to the caller, round-trips exactly.
fn filter_to_raw(filter: &FileFilter) -> RawFilter {
    let mut patterns = Vec::with_capacity(filter.globs.len() + filter.mimes.len());
    patterns.extend(filter.globs.iter().map(|g| (0u32, g.clone())));
    patterns.extend(filter.mimes.iter().map(|m| (1u32, m.clone())));
    (filter.name.clone(), patterns)
}

/// Build the portal result: the chosen `uris`, plus the `current_filter`
/// that was active at accept so the caller learns which of its filters
/// the user ended on. Either is omitted when absent.
fn result_map(
    paths: Option<Vec<PathBuf>>,
    filter: Option<FileFilter>,
) -> HashMap<String, OwnedValue> {
    let mut results = HashMap::new();
    if let Some(paths) = paths {
        let uris: Vec<String> = paths.iter().map(|p| path_to_file_uri(p)).collect();
        if let Ok(value) = OwnedValue::try_from(Value::from(uris)) {
            results.insert("uris".to_string(), value);
        }
    }
    if let Some(filter) = filter {
        if let Ok(value) = OwnedValue::try_from(Value::from(filter_to_raw(&filter))) {
            results.insert("current_filter".to_string(), value);
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Record an accepted request for `app` choosing from `dir` with
    /// `filter` active — the seam the resolution queries read from.
    fn accept_kind(
        store: &crate::state::Store,
        app: &str,
        kind: RequestKind,
        dir: Option<&str>,
        filter: Option<&str>,
    ) {
        store.record_request(
            app,
            kind,
            Vec::new(),
            Some(Chosen {
                dir: dir.map(PathBuf::from),
                paths: Vec::new(),
                filter: filter.map(String::from),
            }),
        );
    }

    fn accept(store: &crate::state::Store, app: &str, dir: Option<&str>, filter: Option<&str>) {
        accept_kind(store, app, RequestKind::OpenFile, dir, filter);
    }

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
    fn start_dir_resolution_order() {
        let store = crate::state::Store::ephemeral();
        let opts = HashMap::new();

        // Nothing remembered → home.
        assert_eq!(start_dir(&opts, "org.app", &store), home_dir());

        // A browser visit is the cross-app fallback (global last_dir).
        store.record_visit(std::path::Path::new("/ceph/global"));
        assert_eq!(
            start_dir(&opts, "org.app", &store),
            PathBuf::from("/ceph/global")
        );

        // A per-app accept overrides the global for that app only.
        accept(&store, "org.app", Some("/ceph/app"), None);
        assert_eq!(
            start_dir(&opts, "org.app", &store),
            PathBuf::from("/ceph/app")
        );
        assert_eq!(
            start_dir(&opts, "org.other", &store),
            PathBuf::from("/ceph/global")
        );
        // Empty app_id never matches per-app → global.
        assert_eq!(start_dir(&opts, "", &store), PathBuf::from("/ceph/global"));

        // An explicit current_folder wins over everything remembered.
        let mut explicit = HashMap::new();
        explicit.insert(
            "current_folder".to_string(),
            OwnedValue::try_from(Value::from(b"/tmp/explicit\0".to_vec())).unwrap(),
        );
        assert_eq!(
            start_dir(&explicit, "org.app", &store),
            PathBuf::from("/tmp/explicit")
        );
    }

    #[test]
    fn filter_option_parsing() {
        // No store memory for these cases: empty app_id never matches.
        let store = crate::state::Store::ephemeral();
        // No options at all → no filters, index 0.
        assert_eq!(filter_options(&HashMap::new(), "", RequestKind::OpenFile, &store), (Vec::new(), 0));

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
        let (filters, idx) = filter_options(&options, "", RequestKind::OpenFile, &store);
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
        assert_eq!(filter_options(&options, "", RequestKind::OpenFile, &store).1, 1);

        // …and a detached one is appended and selected.
        let detached: RawFilter = ("Detached".into(), vec![(0, "*.x".into())]);
        options.insert(
            "current_filter".to_string(),
            OwnedValue::try_from(Value::from(detached)).unwrap(),
        );
        let (filters, idx) = filter_options(&options, "", RequestKind::OpenFile, &store);
        assert_eq!((filters.len(), idx), (4, 3));
        assert_eq!(filters[3].name, "Detached");
    }

    #[test]
    fn filter_to_raw_is_the_inverse_of_parse() {
        let filter = FileFilter {
            name: "Images".into(),
            globs: vec!["*.png".into(), "*.jpg".into()],
            mimes: vec!["image/webp".into()],
        };
        let raw = filter_to_raw(&filter);
        assert_eq!(raw.0, "Images");
        assert_eq!(
            raw.1,
            vec![
                (0u32, "*.png".to_string()),
                (0, "*.jpg".to_string()),
                (1, "image/webp".to_string()),
            ]
        );
        // Round-trips: parse_filter reconstructs the same FileFilter
        // (globs are already lowercase here).
        assert_eq!(parse_filter(raw), filter);
    }

    #[test]
    fn result_map_reports_uris_and_current_filter() {
        let paths = Some(vec![PathBuf::from("/ceph/a.png")]);
        let filter = FileFilter {
            name: "Images".into(),
            globs: vec!["*.png".into()],
            mimes: vec![],
        };
        let result = result_map(paths.clone(), Some(filter));
        assert!(result.contains_key("uris"));
        // current_filter round-trips back to the same wire filter.
        let raw = RawFilter::try_from(result["current_filter"].try_clone().unwrap()).unwrap();
        assert_eq!(
            raw,
            ("Images".to_string(), vec![(0u32, "*.png".to_string())])
        );

        // No filter → no current_filter key (e.g. SaveFiles).
        let result = result_map(paths, None);
        assert!(result.contains_key("uris"));
        assert!(!result.contains_key("current_filter"));

        // Cancel → empty result.
        assert!(result_map(None, None).is_empty());
    }

    #[test]
    fn filter_memory_fills_in_only_without_explicit_current() {
        let store = crate::state::Store::ephemeral();
        let raw: Vec<RawFilter> = vec![
            ("Images".into(), vec![(0, "*.png".into())]),
            ("All files".into(), vec![(0, "*".into())]),
        ];
        let mut options = HashMap::new();
        options.insert(
            "filters".to_string(),
            OwnedValue::try_from(Value::from(raw)).unwrap(),
        );

        // No memory yet → first filter.
        assert_eq!(filter_options(&options, "org.app", RequestKind::OpenFile, &store).1, 0);

        // Memory is kind-scoped: a filter accepted in this app's *Save*
        // dialog must not pre-filter its Open dialogs.
        accept_kind(
            &store,
            "org.app",
            RequestKind::SaveFile,
            None,
            Some("All files"),
        );
        assert_eq!(filter_options(&options, "org.app", RequestKind::OpenFile, &store).1, 0);

        // Remembered filter pre-selects by name when no current_filter.
        accept(&store, "org.app", None, Some("All files"));
        assert_eq!(filter_options(&options, "org.app", RequestKind::OpenFile, &store).1, 1);
        // A different app is unaffected.
        assert_eq!(filter_options(&options, "org.other", RequestKind::OpenFile, &store).1, 0);
        // A remembered filter no longer offered falls back to the first.
        accept(&store, "org.app", None, Some("Vanished"));
        assert_eq!(filter_options(&options, "org.app", RequestKind::OpenFile, &store).1, 0);

        // An explicit current_filter always wins over memory.
        accept(&store, "org.app", None, Some("All files"));
        let images: RawFilter = ("Images".into(), vec![(0, "*.png".into())]);
        options.insert(
            "current_filter".to_string(),
            OwnedValue::try_from(Value::from(images)).unwrap(),
        );
        assert_eq!(filter_options(&options, "org.app", RequestKind::OpenFile, &store).1, 0);
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
