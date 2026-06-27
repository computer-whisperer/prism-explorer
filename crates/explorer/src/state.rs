//! Persisted picker/browser history — an append-only event log under
//! `$XDG_STATE_HOME` recording each completed portal request (what app
//! asked, what filters were offered, what was chosen or cancelled) and
//! each browser directory visit. The picker heuristics
//! (reopen-where-you-were, per-app directory/filter, recent locations)
//! are all *derived queries* over this log, and the full history is
//! retained (bounded) as the substrate for richer prediction later.
//!
//! Stored as JSON, loaded once at startup with a single small read (the
//! way the thumbnail cache is opened) and written by a dedicated thread:
//! the browsed filesystem, and possibly `$HOME`, can be a slow network
//! mount, so a record from the UI thread (or the portal-dispatch thread)
//! must never block on disk. Events parse leniently — one malformed or
//! future-shaped entry is skipped, never wiping the whole log.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Newest events kept; older ones are pruned on append.
const MAX_EVENTS: usize = 500;

const KIND_BROWSER_VISIT: &str = "browser_visit";

/// One logged event: a completed portal request, or a browser visit.
/// Flat and string-typed on purpose — the format must tolerate older and
/// newer builds (and stay legible to external tooling), so an unknown
/// `kind` degrades to "ignored by queries" rather than failing the
/// parse. Every field but `time`/`kind` defaults, so new ones can be
/// added freely.
#[derive(Clone, Serialize, Deserialize)]
struct Event {
    /// Unix seconds when recorded.
    time: i64,
    /// Requesting app's portal id; empty for a browser visit or an
    /// unsandboxed caller.
    #[serde(default)]
    app_id: String,
    /// One of the kind strings (`browser_visit` or a [`RequestKind`]).
    kind: String,
    /// Filter display names the request offered (portal `filters`).
    #[serde(default)]
    filters: Vec<String>,
    /// Whether the request was accepted (`false` = cancelled). Always
    /// `true` for a browser visit.
    #[serde(default)]
    accepted: bool,
    /// Directory context: where the picker was browsing / the folder
    /// chosen / the visited directory.
    #[serde(default)]
    dir: Option<PathBuf>,
    /// Files/folders chosen on accept (richer history; not all queries
    /// use them yet).
    #[serde(default)]
    paths: Vec<PathBuf>,
    /// Filter active at accept (portal `current_filter`), by name.
    #[serde(default)]
    filter: Option<String>,
}

/// The portal request that produced an event; maps to the `kind` string.
#[derive(Clone, Copy)]
pub enum RequestKind {
    OpenFile,
    OpenDir,
    SaveFile,
    SaveFiles,
}

impl RequestKind {
    fn as_str(self) -> &'static str {
        match self {
            RequestKind::OpenFile => "open_file",
            RequestKind::OpenDir => "open_dir",
            RequestKind::SaveFile => "save_file",
            RequestKind::SaveFiles => "save_files",
        }
    }
}

/// What an accepted request chose. `None` to [`Store::record_request`]
/// means the request was cancelled.
pub struct Chosen {
    /// Directory context — typically the parent of the first path.
    pub dir: Option<PathBuf>,
    pub paths: Vec<PathBuf>,
    pub filter: Option<String>,
}

/// On-disk schema: the event log, wrapped so fields can be added later.
#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    events: Vec<Event>,
}

/// Shared, cheap-to-clone handle to the history. Reads lock the mutex
/// and scan; records append and hand the disk write off-thread.
pub struct Store {
    state: Mutex<Persisted>,
    /// Snapshot sink for the writer thread. `None` for an ephemeral
    /// store (tests, or no resolvable home) — the log still works in
    /// memory, it just never touches disk.
    writer: Option<Sender<Persisted>>,
}

impl Store {
    /// Load the log from disk, or start empty. Spawns the writer thread.
    /// Call once at startup, before the event loop.
    pub fn load() -> Arc<Store> {
        let Some(path) = state_path() else {
            tracing::debug!("no state path; explorer history will not persist");
            return Store::ephemeral();
        };
        let events = std::fs::read(&path)
            .ok()
            .map(|bytes| load_events(&bytes))
            .unwrap_or_default();
        let writer = spawn_writer(path);
        Arc::new(Store {
            state: Mutex::new(Persisted { events }),
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

    // ---- recording -----------------------------------------------------

    /// Log that the browser navigated to `dir`. Coalesced: a repeat
    /// visit to the same directory (the last event) is dropped, and a
    /// non-absolute path is ignored.
    pub fn record_visit(&self, dir: &Path) {
        if !dir.is_absolute() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if let Some(last) = state.events.last() {
            if last.kind == KIND_BROWSER_VISIT && last.dir.as_deref() == Some(dir) {
                return;
            }
        }
        state.events.push(Event {
            time: now_unix(),
            app_id: String::new(),
            kind: KIND_BROWSER_VISIT.to_string(),
            filters: Vec::new(),
            accepted: true,
            dir: Some(dir.to_path_buf()),
            paths: Vec::new(),
            filter: None,
        });
        self.prune_and_persist(&mut state);
    }

    /// Log a completed portal request. `chosen` is `None` for a cancel —
    /// still recorded (which app asked for what is signal in itself).
    pub fn record_request(
        &self,
        app_id: &str,
        kind: RequestKind,
        filters: Vec<String>,
        chosen: Option<Chosen>,
    ) {
        let (accepted, dir, paths, filter) = match chosen {
            Some(c) => (true, c.dir, c.paths, c.filter),
            None => (false, None, Vec::new(), None),
        };
        let mut state = self.state.lock().unwrap();
        state.events.push(Event {
            time: now_unix(),
            app_id: app_id.to_string(),
            kind: kind.as_str().to_string(),
            filters,
            accepted,
            dir,
            paths,
            filter,
        });
        self.prune_and_persist(&mut state);
    }

    /// Prune to the newest `MAX_EVENTS` and hand the writer a snapshot,
    /// still under the lock so it observes appends in order. The
    /// unbounded send never blocks; a disconnected writer is harmless —
    /// memory stays authoritative.
    fn prune_and_persist(&self, state: &mut Persisted) {
        let len = state.events.len();
        if len > MAX_EVENTS {
            state.events.drain(0..len - MAX_EVENTS);
        }
        if let Some(writer) = &self.writer {
            let _ = writer.send(Persisted {
                events: state.events.clone(),
            });
        }
    }

    // ---- derived queries -----------------------------------------------

    /// Most recent browser visit — where the standalone browser reopens,
    /// and the cross-app fallback for a dialog with no hint and no
    /// per-app memory.
    pub fn last_dir(&self) -> Option<PathBuf> {
        let state = self.state.lock().unwrap();
        state
            .events
            .iter()
            .rev()
            .find(|e| e.kind == KIND_BROWSER_VISIT)
            .and_then(|e| e.dir.clone())
    }

    /// The directory `app_id` last accepted from, if any.
    pub fn app_last_dir(&self, app_id: &str) -> Option<PathBuf> {
        if app_id.is_empty() {
            return None;
        }
        let state = self.state.lock().unwrap();
        state
            .events
            .iter()
            .rev()
            .find(|e| e.app_id == app_id && e.accepted && e.dir.is_some())
            .and_then(|e| e.dir.clone())
    }

    /// The filter (by display name) `app_id` last accepted with, if any.
    pub fn app_last_filter(&self, app_id: &str) -> Option<String> {
        if app_id.is_empty() {
            return None;
        }
        let state = self.state.lock().unwrap();
        state
            .events
            .iter()
            .rev()
            .find(|e| e.app_id == app_id && e.accepted && e.filter.is_some())
            .and_then(|e| e.filter.clone())
    }

    /// Up to `limit` distinct recently-used directories, most-recent
    /// first. Browser visits and accepted picks both contribute their
    /// `dir`.
    pub fn recent_dirs(&self, limit: usize) -> Vec<PathBuf> {
        let state = self.state.lock().unwrap();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for event in state.events.iter().rev() {
            if let Some(dir) = &event.dir {
                if seen.insert(dir.clone()) {
                    out.push(dir.clone());
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        out
    }
}

/// Current time in Unix seconds (0 if the clock is before the epoch).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse the `events` array leniently: skip any entry that doesn't
/// deserialize (an older or newer shape) rather than discarding the
/// whole log on one bad element.
fn load_events(bytes: &[u8]) -> Vec<Event> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Vec::new();
    };
    let Some(array) = value.get("events").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|v| serde_json::from_value::<Event>(v.clone()).ok())
        .collect()
}

/// Spawn the background writer. Returns its sink, or `None` if the
/// thread can't start (the log then stays in-memory).
fn spawn_writer(path: PathBuf) -> Option<Sender<Persisted>> {
    let (tx, rx) = channel::<Persisted>();
    let spawned = std::thread::Builder::new()
        .name("state-writer".into())
        .spawn(move || {
            // Block for a snapshot, then drain any queued behind it so a
            // burst of records collapses to one write.
            while let Ok(mut latest) = rx.recv() {
                while let Ok(next) = rx.try_recv() {
                    latest = next;
                }
                if let Err(e) = write_atomic(&path, &latest) {
                    tracing::warn!(error = %e, "failed to persist explorer history");
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
    fn browser_visits_drive_last_dir_and_coalesce() {
        let store = Store::ephemeral();
        assert_eq!(store.last_dir(), None);
        store.record_visit(Path::new("/ceph/a"));
        store.record_visit(Path::new("/ceph/a")); // coalesced (no-op)
        store.record_visit(Path::new("/ceph/b"));
        assert_eq!(store.last_dir(), Some(PathBuf::from("/ceph/b")));
        // Relative visits are ignored.
        store.record_visit(Path::new("relative"));
        assert_eq!(store.last_dir(), Some(PathBuf::from("/ceph/b")));
    }

    #[test]
    fn per_app_dir_and_filter_derive_from_accepted_requests() {
        let store = Store::ephemeral();
        // A cancel records context but no dir/filter.
        store.record_request("org.x", RequestKind::SaveFile, vec!["All".into()], None);
        assert_eq!(store.app_last_dir("org.x"), None);

        store.record_request(
            "org.x",
            RequestKind::SaveFile,
            vec!["Images".into(), "All".into()],
            Some(Chosen {
                dir: Some(PathBuf::from("/ceph/x")),
                paths: vec![PathBuf::from("/ceph/x/a.png")],
                filter: Some("Images".into()),
            }),
        );
        assert_eq!(store.app_last_dir("org.x"), Some(PathBuf::from("/ceph/x")));
        assert_eq!(store.app_last_filter("org.x"), Some("Images".to_string()));
        // Isolation: another app and empty id see nothing.
        assert_eq!(store.app_last_dir("org.y"), None);
        assert_eq!(store.app_last_dir(""), None);
        // Browser visits don't count as a per-app accept.
        store.record_visit(Path::new("/ceph/elsewhere"));
        assert_eq!(store.app_last_dir("org.x"), Some(PathBuf::from("/ceph/x")));
    }

    #[test]
    fn recent_dirs_are_distinct_mru_from_visits_and_picks() {
        let store = Store::ephemeral();
        store.record_visit(Path::new("/ceph/a"));
        store.record_request(
            "org.x",
            RequestKind::SaveFile,
            Vec::new(),
            Some(Chosen {
                dir: Some(PathBuf::from("/ceph/b")),
                paths: vec![PathBuf::from("/ceph/b/f")],
                filter: None,
            }),
        );
        store.record_visit(Path::new("/ceph/a")); // revisit moves a to front
        assert_eq!(
            store.recent_dirs(8),
            vec![PathBuf::from("/ceph/a"), PathBuf::from("/ceph/b")]
        );
        // Limit is honored.
        assert_eq!(store.recent_dirs(1), vec![PathBuf::from("/ceph/a")]);
    }

    #[test]
    fn log_is_pruned_to_the_cap() {
        let store = Store::ephemeral();
        for i in 0..(MAX_EVENTS + 50) {
            store.record_visit(&PathBuf::from(format!("/ceph/d{i}")));
        }
        let state = store.state.lock().unwrap();
        assert_eq!(state.events.len(), MAX_EVENTS);
        // Oldest dropped, newest kept.
        assert_eq!(
            state.events.last().unwrap().dir,
            Some(PathBuf::from(format!("/ceph/d{}", MAX_EVENTS + 49)))
        );
    }

    #[test]
    fn load_events_skips_malformed_entries() {
        // Two good events, one missing the required `kind`, one not an
        // object — only the well-formed ones survive.
        let json = br#"{"events":[
            {"time":1,"kind":"browser_visit","dir":"/ceph/a","accepted":true},
            {"time":2,"dir":"/ceph/bad"},
            "garbage",
            {"time":3,"kind":"browser_visit","dir":"/ceph/b","accepted":true}
        ]}"#;
        let events = load_events(json);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].dir, Some(PathBuf::from("/ceph/a")));
        assert_eq!(events[1].dir, Some(PathBuf::from("/ceph/b")));
    }

    #[test]
    fn write_atomic_creates_parent_and_reloads() {
        let dir = std::env::temp_dir().join(format!("prism-state-test-{}", std::process::id()));
        let path = dir.join("nested").join("state.json");
        let state = Persisted {
            events: vec![Event {
                time: 1,
                app_id: "org.x".into(),
                kind: "save_file".into(),
                filters: vec!["Images".into()],
                accepted: true,
                dir: Some(PathBuf::from("/ceph/work")),
                paths: vec![PathBuf::from("/ceph/work/a.png")],
                filter: Some("Images".into()),
            }],
        };
        write_atomic(&path, &state).unwrap();
        let reloaded = load_events(&std::fs::read(&path).unwrap());
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].dir, Some(PathBuf::from("/ceph/work")));
        assert_eq!(reloaded[0].filter, Some("Images".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
