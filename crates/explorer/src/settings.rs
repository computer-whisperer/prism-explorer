//! Persisted user preferences — a small JSON file under
//! `$XDG_CONFIG_HOME` holding the handful of browser options the user
//! sets explicitly (show hidden files, sort order, view mode). Distinct
//! from [`crate::state`], which is an append-only *history* log under
//! `$XDG_STATE_HOME`: this is *config* (what the user chose), that is
//! *state* (where they've been). XDG keeps the two in separate trees.
//!
//! Loaded once at startup with a single small read and written by a
//! dedicated thread — the same off-the-UI-thread discipline as the
//! history log and the thumbnail cache, since the config dir may live
//! on a slow mount. Every field defaults and deserializes leniently, so
//! an older or newer build reads what it understands and falls back to
//! defaults for the rest rather than failing the load.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The persisted preferences. Sort and view are stored as their stable
/// string forms (`SortMode`'s `Display`, and `"list"`/`"grid"`) so the
/// on-disk file stays legible and an unrecognized value degrades to the
/// default at the parse boundary in `app.rs` rather than here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Show dotfiles in listings.
    #[serde(default)]
    pub show_hidden: bool,
    /// Sort mode as its [`crate::model::SortMode`] `Display` string
    /// (e.g. `"name-asc"`).
    #[serde(default = "default_sort")]
    pub sort: String,
    /// View mode: `"list"` or `"grid"`.
    #[serde(default = "default_view")]
    pub view: String,
}

fn default_sort() -> String {
    "name-asc".to_string()
}

fn default_view() -> String {
    "list".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            show_hidden: false,
            sort: default_sort(),
            view: default_view(),
        }
    }
}

/// Shared, cheap-to-clone handle to the live preferences. Reads
/// snapshot the current value; [`save`](Store::save) replaces it and
/// hands the disk write off-thread.
pub struct Store {
    current: Mutex<Settings>,
    /// Snapshot sink for the writer thread. `None` for an ephemeral
    /// store (tests, or no resolvable config dir) — settings still work
    /// in memory for the session, they just never touch disk.
    writer: Option<Sender<Settings>>,
}

impl Store {
    /// Load preferences from disk, or start from defaults. Spawns the
    /// writer thread. Call once at startup, before the event loop.
    pub fn load() -> Arc<Store> {
        let Some(path) = settings_path() else {
            tracing::debug!("no config path; explorer settings will not persist");
            return Store::ephemeral();
        };
        let current = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
            .unwrap_or_default();
        let writer = spawn_writer(path);
        Arc::new(Store {
            current: Mutex::new(current),
            writer,
        })
    }

    /// An in-memory-only store that never touches disk — for tests and
    /// any context without a config directory.
    pub fn ephemeral() -> Arc<Store> {
        Arc::new(Store {
            current: Mutex::new(Settings::default()),
            writer: None,
        })
    }

    /// A snapshot of the current preferences.
    pub fn get(&self) -> Settings {
        self.current.lock().unwrap().clone()
    }

    /// Replace the preferences and persist. A no-op write (value
    /// unchanged) is dropped so idempotent callers don't churn the
    /// disk; the writer coalesces any real burst into one write.
    pub fn save(&self, settings: Settings) {
        let mut current = self.current.lock().unwrap();
        if *current == settings {
            return;
        }
        *current = settings.clone();
        if let Some(writer) = &self.writer {
            let _ = writer.send(settings);
        }
    }
}

/// Spawn the background writer. Returns its sink, or `None` if the
/// thread can't start (settings then stay in-memory for the session).
fn spawn_writer(path: PathBuf) -> Option<Sender<Settings>> {
    let (tx, rx) = channel::<Settings>();
    let spawned = std::thread::Builder::new()
        .name("settings-writer".into())
        .spawn(move || {
            // Block for a snapshot, then drain any queued behind it so a
            // burst of saves collapses to one write.
            while let Ok(mut latest) = rx.recv() {
                while let Ok(next) = rx.try_recv() {
                    latest = next;
                }
                if let Err(e) = write_atomic(&path, &latest) {
                    tracing::warn!(error = %e, "failed to persist explorer settings");
                }
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::warn!(error = %e, "settings-writer thread failed to spawn");
            None
        }
    }
}

/// Serialize to a sibling temp file and rename over the target, so a
/// reader never sees a half-written file (and a crash mid-write leaves
/// the previous good copy intact).
fn write_atomic(path: &Path, settings: &Settings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(settings)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

/// `$XDG_CONFIG_HOME/prism-explorer/settings.json`, falling back to
/// `$HOME/.config/...`. `None` when neither resolves.
fn settings_path() -> Option<PathBuf> {
    let dir = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if Path::new(&x).is_absolute() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(dir.join("prism-explorer").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_get_round_trips_in_memory() {
        let store = Store::ephemeral();
        assert_eq!(store.get(), Settings::default());
        let next = Settings {
            show_hidden: true,
            sort: "size-desc".into(),
            view: "grid".into(),
        };
        store.save(next.clone());
        assert_eq!(store.get(), next);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // An older file that only knew about `show_hidden` still loads;
        // the unknown-to-it fields take their defaults.
        let parsed: Settings = serde_json::from_slice(br#"{"show_hidden": true}"#).unwrap();
        assert_eq!(
            parsed,
            Settings {
                show_hidden: true,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // A newer file with extra keys is read for what we understand.
        let parsed: Settings =
            serde_json::from_slice(br#"{"sort": "type-asc", "future": 1}"#).unwrap();
        assert_eq!(parsed.sort, "type-asc");
    }
}
