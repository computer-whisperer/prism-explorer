//! Persisted picker/browser state — the small on-disk memory that lets
//! windows reopen where you left off. Holds the global last-visited
//! directory and per-app memory (where each requesting app last
//! accepted); it's the seed for further heuristic recommendations
//! (recents, per-filter) planned on top, which is why it's a structured
//! store and not a single line of text.
//!
//! Stored as JSON at `$XDG_STATE_HOME/prism-explorer/state.json`
//! (a last-used location is *state*, not config). The file is loaded
//! once at startup with a single small synchronous read — the way the
//! thumbnail cache is opened. Writes are handed to a dedicated thread:
//! the browsed filesystem, and possibly `$HOME`, can be a slow network
//! mount, so a `record_dir` from the UI thread (or the portal-dispatch
//! thread) must never block on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// On-disk schema. Every field is `#[serde(default)]` so a file written
/// by an older or newer build still loads — missing keys fall back to
/// the default and unknown keys are ignored. Add fields here as the
/// heuristic layer grows; never reuse a removed field's meaning.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Persisted {
    /// The last directory browsed in the *standalone browser*. Seeds the
    /// browser when launched with no path argument, and is the cross-app
    /// fallback for a dialog with no `current_folder` hint and no
    /// per-app memory. Picker windows do not touch this (they carry an
    /// ephemeral store), so a cancelled dialog leaves it untouched.
    #[serde(default)]
    last_dir: Option<PathBuf>,
    /// Per-requesting-app memory, keyed by the portal `app_id`. Seeds a
    /// dialog ahead of the global `last_dir` so each app reopens where
    /// it last accepted.
    #[serde(default)]
    per_app: HashMap<String, AppState>,
}

/// What we remember for one requesting application.
#[derive(Clone, Default, Serialize, Deserialize)]
struct AppState {
    /// The directory this app last *accepted* a file/folder from.
    #[serde(default)]
    last_dir: Option<PathBuf>,
    /// Display name of the file-type filter this app last accepted with.
    /// Matched by name against a later request's filter list (indices
    /// are meaningless across requests with different filter sets).
    #[serde(default)]
    last_filter: Option<String>,
}

/// Shared, cheap-to-clone handle to the persisted state. Reads are an
/// in-memory mutex lock; [`record_dir`](Store::record_dir) updates
/// memory and hands the disk write off-thread.
pub struct Store {
    state: Mutex<Persisted>,
    /// Snapshot sink for the writer thread. `None` for an ephemeral
    /// store (tests, or no resolvable home) — the store still works,
    /// it just never touches disk.
    writer: Option<Sender<Persisted>>,
}

impl Store {
    /// Load the store from disk, or start empty if there's no file yet
    /// (or it's unreadable / corrupt). Spawns the writer thread. Call
    /// once at startup, before the event loop.
    pub fn load() -> Arc<Store> {
        let Some(path) = state_path() else {
            tracing::debug!("no state path; explorer state will not persist");
            return Store::ephemeral();
        };
        let state = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .unwrap_or_default();
        let writer = spawn_writer(path);
        Arc::new(Store {
            state: Mutex::new(state),
            writer,
        })
    }

    /// An in-memory-only store that never touches disk — for tests and
    /// any context without a home directory.
    pub fn ephemeral() -> Arc<Store> {
        Arc::new(Store {
            state: Mutex::new(Persisted::default()),
            writer: None,
        })
    }

    /// The last directory recorded, if any. Best-effort: the path is
    /// not stat'd here (it may be on a slow mount), so a caller that
    /// needs it to exist should be ready for a listing error.
    pub fn last_dir(&self) -> Option<PathBuf> {
        self.state.lock().unwrap().last_dir.clone()
    }

    /// Remember `dir` as the most-recent location and persist it. Cheap
    /// on the calling thread: it updates memory and queues a snapshot;
    /// the writer thread does the disk write. A no-op when `dir` is
    /// already the recorded location.
    pub fn record_dir(&self, dir: &Path) {
        // Only an absolute path is meaningful as a "last location". A
        // relative one (e.g. an unsupported `~user` typed into the
        // location bar) would mis-seed the next dialog or the browser's
        // start directory, so drop it here at the single write point.
        if !dir.is_absolute() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.last_dir.as_deref() == Some(dir) {
            return;
        }
        state.last_dir = Some(dir.to_path_buf());
        self.persist(&state);
    }

    /// The directory `app_id` last accepted from, if remembered. Seeds a
    /// dialog ahead of the global [`last_dir`](Store::last_dir).
    pub fn app_last_dir(&self, app_id: &str) -> Option<PathBuf> {
        self.state
            .lock()
            .unwrap()
            .per_app
            .get(app_id)?
            .last_dir
            .clone()
    }

    /// Remember that `app_id` accepted a file/folder from `dir`. A no-op
    /// for an empty `app_id` (unsandboxed callers) or a non-absolute
    /// `dir`, and when the value is unchanged.
    pub fn record_app_dir(&self, app_id: &str, dir: &Path) {
        if app_id.is_empty() || !dir.is_absolute() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        {
            let entry = state.per_app.entry(app_id.to_string()).or_default();
            if entry.last_dir.as_deref() == Some(dir) {
                return;
            }
            entry.last_dir = Some(dir.to_path_buf());
        }
        self.persist(&state);
    }

    /// The display name of the filter `app_id` last accepted with, if
    /// remembered. Used to pre-select a filter in this app's next dialog
    /// when the caller doesn't specify a `current_filter`.
    pub fn app_last_filter(&self, app_id: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .per_app
            .get(app_id)?
            .last_filter
            .clone()
    }

    /// Remember the filter (by display name) `app_id` last accepted with.
    /// A no-op for an empty `app_id` or `filter`, and when unchanged.
    pub fn record_app_filter(&self, app_id: &str, filter: &str) {
        if app_id.is_empty() || filter.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        {
            let entry = state.per_app.entry(app_id.to_string()).or_default();
            if entry.last_filter.as_deref() == Some(filter) {
                return;
            }
            entry.last_filter = Some(filter.to_string());
        }
        self.persist(&state);
    }

    /// Hand the writer a snapshot, called while still holding the state
    /// lock so the writer observes updates in the same order memory did
    /// (concurrent callers: the UI thread and the portal-dispatch
    /// thread). The unbounded send never blocks; the writer coalesces
    /// bursts and always persists the latest. A disconnected writer
    /// (thread gone) is harmless — memory stays authoritative.
    fn persist(&self, state: &Persisted) {
        if let Some(writer) = &self.writer {
            let _ = writer.send(state.clone());
        }
    }
}

/// Spawn the background writer. Returns its sink, or `None` if the
/// thread can't start (the store then stays in-memory).
fn spawn_writer(path: PathBuf) -> Option<Sender<Persisted>> {
    let (tx, rx) = channel::<Persisted>();
    let spawned = std::thread::Builder::new()
        .name("state-writer".into())
        .spawn(move || {
            // Block for a snapshot, then drain any others queued behind
            // it so a burst of navigations collapses to one write.
            while let Ok(mut latest) = rx.recv() {
                while let Ok(next) = rx.try_recv() {
                    latest = next;
                }
                if let Err(e) = write_atomic(&path, &latest) {
                    tracing::warn!(error = %e, "failed to persist explorer state");
                }
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::warn!(error = %e, "state-writer thread failed to spawn");
            None
        }
    }
}

/// Serialize to a sibling temp file and rename over the target, so a
/// reader never sees a half-written file (and a crash mid-write leaves
/// the previous good copy intact).
fn write_atomic(path: &Path, state: &Persisted) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(state)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

/// `$XDG_STATE_HOME/prism-explorer/state.json`, falling back to
/// `$HOME/.local/state/...`. `None` when neither resolves.
fn state_path() -> Option<PathBuf> {
    let dir = match std::env::var_os("XDG_STATE_HOME") {
        Some(x) if Path::new(&x).is_absolute() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(dir.join("prism-explorer").join("state.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_store_round_trips_in_memory() {
        let store = Store::ephemeral();
        assert_eq!(store.last_dir(), None);
        store.record_dir(Path::new("/ceph/photos"));
        assert_eq!(store.last_dir(), Some(PathBuf::from("/ceph/photos")));
        // Re-recording the same dir is a no-op but leaves it set.
        store.record_dir(Path::new("/ceph/photos"));
        assert_eq!(store.last_dir(), Some(PathBuf::from("/ceph/photos")));
    }

    #[test]
    fn record_dir_ignores_relative_paths() {
        let store = Store::ephemeral();
        store.record_dir(Path::new("relative/dir"));
        store.record_dir(Path::new("~user"));
        assert_eq!(store.last_dir(), None);
        // An absolute path after the relative ones still records.
        store.record_dir(Path::new("/ceph/work"));
        assert_eq!(store.last_dir(), Some(PathBuf::from("/ceph/work")));
    }

    #[test]
    fn per_app_dir_records_isolated_and_layered() {
        let store = Store::ephemeral();
        assert_eq!(store.app_last_dir("org.x"), None);
        store.record_app_dir("org.x", Path::new("/ceph/x"));
        store.record_app_dir("org.y", Path::new("/ceph/y"));
        // Each app keeps its own; an unseen app has none.
        assert_eq!(store.app_last_dir("org.x"), Some(PathBuf::from("/ceph/x")));
        assert_eq!(store.app_last_dir("org.y"), Some(PathBuf::from("/ceph/y")));
        assert_eq!(store.app_last_dir("org.z"), None);
        // Per-app records are independent of the global last_dir.
        assert_eq!(store.last_dir(), None);
    }

    #[test]
    fn record_app_dir_ignores_empty_id_and_relative() {
        let store = Store::ephemeral();
        store.record_app_dir("", Path::new("/ceph/x"));
        store.record_app_dir("org.x", Path::new("relative"));
        assert_eq!(store.app_last_dir(""), None);
        assert_eq!(store.app_last_dir("org.x"), None);
    }

    #[test]
    fn per_app_filter_records_isolated_with_guards() {
        let store = Store::ephemeral();
        store.record_app_filter("org.x", "Images");
        store.record_app_filter("org.y", "Documents");
        assert_eq!(store.app_last_filter("org.x"), Some("Images".to_string()));
        assert_eq!(
            store.app_last_filter("org.y"),
            Some("Documents".to_string())
        );
        assert_eq!(store.app_last_filter("org.z"), None);
        // Empty app_id or empty filter name are ignored; dir and filter
        // are independent slots on the same app entry.
        store.record_app_filter("", "Images");
        store.record_app_filter("org.x", "");
        assert_eq!(store.app_last_filter(""), None);
        assert_eq!(store.app_last_filter("org.x"), Some("Images".to_string()));
        store.record_app_dir("org.x", Path::new("/ceph/x"));
        assert_eq!(store.app_last_dir("org.x"), Some(PathBuf::from("/ceph/x")));
        assert_eq!(store.app_last_filter("org.x"), Some("Images".to_string()));
    }

    #[test]
    fn persisted_json_tolerates_unknown_and_missing_keys() {
        // A future build's extra key loads (ignored); missing keys
        // default rather than failing the parse.
        let with_extra: Persisted =
            serde_json::from_str(r#"{"last_dir":"/a/b","future_field":42}"#).unwrap();
        assert_eq!(with_extra.last_dir, Some(PathBuf::from("/a/b")));
        let empty: Persisted = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.last_dir, None);
        assert!(empty.per_app.is_empty());
        // per_app round-trips.
        let with_app: Persisted =
            serde_json::from_str(r#"{"per_app":{"org.x":{"last_dir":"/ceph/x"}}}"#).unwrap();
        assert_eq!(
            with_app
                .per_app
                .get("org.x")
                .and_then(|a| a.last_dir.clone()),
            Some(PathBuf::from("/ceph/x"))
        );
    }

    #[test]
    fn write_atomic_creates_parent_and_reloads() {
        let dir = std::env::temp_dir().join(format!("prism-state-test-{}", std::process::id()));
        let path = dir.join("nested").join("state.json");
        let state = Persisted {
            last_dir: Some(PathBuf::from("/ceph/work")),
            ..Default::default()
        };
        write_atomic(&path, &state).unwrap();
        let back: Persisted = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.last_dir, Some(PathBuf::from("/ceph/work")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
