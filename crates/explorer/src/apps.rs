//! The XDG desktop-application database behind "Open with…".
//!
//! Enumerates installed `.desktop` apps and resolves which ones can
//! open a given MIME type — declared `MimeType=` associations plus the
//! user's `mimeapps.list` (defaults / added / removed). Built once off
//! the UI thread at startup ([`AppDb::load`]) and cached in an `Arc`;
//! [`AppDb::candidates`] is then a pure in-memory lookup. Launching
//! shells out to `gtk-launch` (which handles Exec field codes, the
//! `Terminal=true` case, and D-Bus activation), falling back to the
//! entry's own parsed `Exec`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use freedesktop_desktop_entry::{desktop_entries, get_languages_from_env, DesktopEntry};

/// One candidate application, ready for the chooser UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Desktop file id *with* the `.desktop` suffix
    /// (`org.gnome.gedit.desktop`) — what `gtk-launch` and
    /// `mimeapps.list` both key on.
    pub id: String,
    pub name: String,
    /// The user's default handler for the queried type.
    pub is_default: bool,
}

/// Resolved associations from the desktop-entry database.
pub struct AppDb {
    /// Displayable `Type=Application` entries, keyed by `{appid}.desktop`.
    entries: HashMap<String, DesktopEntry>,
    /// MIME type → desktop ids that handle it (declared `MimeType=`
    /// plus `mimeapps.list` `[Added Associations]`), in discovery order.
    by_mime: HashMap<String, Vec<String>>,
    /// MIME type → default desktop id (`[Default Applications]`,
    /// highest-precedence file wins).
    defaults: HashMap<String, String>,
    /// MIME type → desktop ids the user dissociated
    /// (`[Removed Associations]`).
    removed: HashMap<String, HashSet<String>>,
    locales: Vec<String>,
}

impl AppDb {
    /// Scan the desktop-entry database and `mimeapps.list`. Blocking
    /// disk IO — call on a worker thread, never the UI thread.
    pub fn load() -> AppDb {
        let locales = get_languages_from_env();
        let mut entries = HashMap::new();
        let mut by_mime: HashMap<String, Vec<String>> = HashMap::new();
        for entry in desktop_entries(&locales) {
            // Only launchable, user-visible applications.
            if entry.type_() != Some("Application")
                || entry.no_display()
                || entry.hidden()
                || entry.exec().is_none()
            {
                continue;
            }
            let id = format!("{}.desktop", entry.appid);
            if let Some(mimes) = entry.mime_type() {
                for mime in mimes {
                    push_unique(by_mime.entry(mime.to_string()).or_default(), &id);
                }
            }
            entries.insert(id, entry);
        }

        let mime_apps = MimeApps::load();
        // Fold user-added associations in after the declared handlers,
        // so an app that declares the type keeps priority but apps the
        // user explicitly associated still appear.
        for (mime, ids) in mime_apps.added {
            let list = by_mime.entry(mime).or_default();
            for id in ids {
                push_unique(list, &id);
            }
        }

        AppDb {
            entries,
            by_mime,
            defaults: mime_apps.defaults,
            removed: mime_apps.removed,
            locales,
        }
    }

    /// True once any apps are known — the probe may not have finished
    /// (or the system may have none).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Build a database from explicit `(desktop-id, display-name)` apps
    /// and `mime → ids` associations, bypassing disk IO. For the
    /// `dump_bundles` scenes and tests that must not touch the real
    /// desktop database.
    pub(crate) fn from_fixture(apps: &[(&str, &str)], by_mime: &[(&str, &[&str])]) -> AppDb {
        let mut entries = HashMap::new();
        for (id, name) in apps {
            let mut entry = DesktopEntry::from_appid(id.trim_end_matches(".desktop").to_string());
            entry.add_desktop_entry("Name".to_string(), name.to_string());
            entries.insert(id.to_string(), entry);
        }
        AppDb {
            entries,
            by_mime: by_mime
                .iter()
                .map(|(m, ids)| (m.to_string(), ids.iter().map(|s| s.to_string()).collect()))
                .collect(),
            defaults: HashMap::new(),
            removed: HashMap::new(),
            locales: Vec::new(),
        }
    }

    /// Apps that can open `mime`: the user's default first, then
    /// declared and added handlers — deduped, with removed
    /// associations and uninstalled ids dropped.
    pub fn candidates(&self, mime: &str) -> Vec<Candidate> {
        let removed = self.removed.get(mime);
        let is_removed = |id: &String| removed.is_some_and(|r| r.contains(id));

        let mut out = Vec::new();
        let mut seen: HashSet<&String> = HashSet::new();

        let default = self
            .defaults
            .get(mime)
            .filter(|id| self.entries.contains_key(*id) && !is_removed(id));
        if let Some(id) = default {
            seen.insert(id);
            out.push(self.candidate(id, true));
        }
        if let Some(ids) = self.by_mime.get(mime) {
            let mut rest = Vec::new();
            for id in ids {
                if is_removed(id) || !self.entries.contains_key(id) {
                    continue;
                }
                if seen.insert(id) {
                    rest.push(self.candidate(id, false));
                }
            }
            // Filesystem scan order isn't stable across runs; present
            // the non-default handlers alphabetically.
            rest.sort_by_key(|c| c.name.to_lowercase());
            out.append(&mut rest);
        }
        out
    }

    fn candidate(&self, id: &str, is_default: bool) -> Candidate {
        let name = self
            .entries
            .get(id)
            .and_then(|e| e.name(&self.locales))
            .map(|n| n.into_owned())
            .unwrap_or_else(|| id.trim_end_matches(".desktop").to_string());
        Candidate {
            id: id.to_string(),
            name,
            is_default,
        }
    }

    /// Launch desktop app `id` on `path`. Prefers `gtk-launch` (full
    /// Exec/terminal/D-Bus-activation handling); on a missing
    /// `gtk-launch` it falls back to the entry's parsed `Exec`.
    /// Detached — the child runs in its own process.
    pub fn launch(&self, id: &str, path: &Path) {
        match std::process::Command::new("gtk-launch")
            .arg(id)
            .arg(path)
            .spawn()
        {
            Ok(_) => return,
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(app = id, error = %e, "gtk-launch failed");
                return;
            }
            Err(_) => {} // not installed — fall back to the raw Exec
        }
        self.launch_via_exec(id, path);
    }

    fn launch_via_exec(&self, id: &str, path: &Path) {
        let Some(entry) = self.entries.get(id) else {
            tracing::warn!(app = id, "no Exec fallback: unknown desktop id");
            return;
        };
        // Field codes get a bare path. Apps with `%f`/`%F` want exactly
        // that; the rarer `%u`/`%U` (URI) apps generally accept a local
        // path too. gtk-launch — the primary path — handles both
        // precisely, so this only matters when it's missing.
        let path_str = path.to_string_lossy();
        let argv = match entry.parse_exec_with_uris(&[path_str.as_ref()], &self.locales) {
            Ok(argv) => argv,
            Err(e) => {
                tracing::warn!(app = id, error = %e, "Exec parse failed");
                return;
            }
        };
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        if let Err(e) = std::process::Command::new(program).args(args).spawn() {
            tracing::warn!(app = id, error = %e, "Exec launch failed");
        }
    }
}

fn push_unique(list: &mut Vec<String>, id: &str) {
    if !list.iter().any(|existing| existing == id) {
        list.push(id.to_string());
    }
}

/// Parsed `mimeapps.list` associations, merged across the XDG search
/// path. Defaults take the highest-precedence file's value;
/// added/removed associations accumulate. This is a pragmatic subset
/// of the full association-precedence algorithm — enough to populate a
/// correct candidate list in the common case.
struct MimeApps {
    defaults: HashMap<String, String>,
    added: HashMap<String, Vec<String>>,
    removed: HashMap<String, HashSet<String>>,
}

impl MimeApps {
    fn load() -> MimeApps {
        let mut apps = MimeApps {
            defaults: HashMap::new(),
            added: HashMap::new(),
            removed: HashMap::new(),
        };
        // Highest precedence first; `merge_file` keeps the first
        // default it sees per type.
        for path in mimeapps_paths() {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                apps.merge_file(&contents);
            }
        }
        apps
    }

    fn merge_file(&mut self, contents: &str) {
        let mut section = "";
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = match name {
                    "Default Applications" => "default",
                    "Added Associations" => "added",
                    "Removed Associations" => "removed",
                    _ => "",
                };
                continue;
            }
            let Some((mime, value)) = line.split_once('=') else {
                continue;
            };
            let mime = mime.trim();
            let ids = value.split(';').map(str::trim).filter(|s| !s.is_empty());
            match section {
                "default" => {
                    // First (highest-precedence) default per type wins.
                    if let Some(id) = ids.clone().next() {
                        self.defaults
                            .entry(mime.to_string())
                            .or_insert_with(|| id.to_string());
                    }
                }
                "added" => {
                    let list = self.added.entry(mime.to_string()).or_default();
                    for id in ids {
                        push_unique(list, id);
                    }
                }
                "removed" => {
                    let set = self.removed.entry(mime.to_string()).or_default();
                    for id in ids {
                        set.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }
}

/// `mimeapps.list` search path, highest precedence first.
fn mimeapps_paths() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut paths = Vec::new();

    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    if let Some(dir) = &config_home {
        paths.push(dir.join("mimeapps.list"));
    }
    let config_dirs = std::env::var("XDG_CONFIG_DIRS").unwrap_or_else(|_| "/etc/xdg".into());
    for dir in config_dirs.split(':').filter(|s| !s.is_empty()) {
        paths.push(Path::new(dir).join("mimeapps.list"));
    }

    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    if let Some(dir) = &data_home {
        paths.push(dir.join("applications/mimeapps.list"));
    }
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for dir in data_dirs.split(':').filter(|s| !s.is_empty()) {
        paths.push(Path::new(dir).join("applications/mimeapps.list"));
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_entry(appid: &str) -> DesktopEntry {
        DesktopEntry::from_appid(appid.to_string())
    }

    /// Build an `AppDb` from synthetic state, bypassing disk IO.
    fn db(
        installed: &[&str],
        by_mime: &[(&str, &[&str])],
        defaults: &[(&str, &str)],
        removed: &[(&str, &[&str])],
    ) -> AppDb {
        AppDb {
            entries: installed
                .iter()
                .map(|id| (id.to_string(), stub_entry(id.trim_end_matches(".desktop"))))
                .collect(),
            by_mime: by_mime
                .iter()
                .map(|(m, ids)| (m.to_string(), ids.iter().map(|s| s.to_string()).collect()))
                .collect(),
            defaults: defaults
                .iter()
                .map(|(m, id)| (m.to_string(), id.to_string()))
                .collect(),
            removed: removed
                .iter()
                .map(|(m, ids)| (m.to_string(), ids.iter().map(|s| s.to_string()).collect()))
                .collect(),
            locales: Vec::new(),
        }
    }

    #[test]
    fn candidates_put_default_first_and_dedupe() {
        let db = db(
            &["a.desktop", "b.desktop", "c.desktop"],
            &[("text/plain", &["a.desktop", "b.desktop", "c.desktop"])],
            &[("text/plain", "b.desktop")],
            &[],
        );
        let ids: Vec<_> = db
            .candidates("text/plain")
            .into_iter()
            .map(|c| (c.id, c.is_default))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("b.desktop".into(), true),
                ("a.desktop".into(), false),
                ("c.desktop".into(), false),
            ]
        );
    }

    #[test]
    fn candidates_drop_removed_and_uninstalled() {
        let db = db(
            &["a.desktop", "b.desktop"], // c is referenced but not installed
            &[("text/plain", &["a.desktop", "b.desktop", "c.desktop"])],
            &[],
            &[("text/plain", &["b.desktop"])],
        );
        let ids: Vec<_> = db
            .candidates("text/plain")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["a.desktop".to_string()]);
    }

    #[test]
    fn unknown_mime_has_no_candidates() {
        let db = db(&["a.desktop"], &[("text/plain", &["a.desktop"])], &[], &[]);
        assert!(db.candidates("application/x-nonesuch").is_empty());
    }

    #[test]
    #[ignore = "touches the real desktop database"]
    fn smoke_load_finds_text_handlers() {
        let db = AppDb::load();
        let c = db.candidates("text/plain");
        eprintln!("text/plain → {} candidates: {:?}", c.len(), c);
        assert!(!db.is_empty(), "expected installed apps");
    }

    #[test]
    fn mimeapps_merge_precedence() {
        let mut apps = MimeApps {
            defaults: HashMap::new(),
            added: HashMap::new(),
            removed: HashMap::new(),
        };
        // Highest-precedence file first.
        apps.merge_file(
            "[Default Applications]\ntext/plain=high.desktop;\n\
             [Added Associations]\ntext/plain=extra.desktop;\n\
             [Removed Associations]\ntext/html=bad.desktop;\n",
        );
        // Lower-precedence file: its default must NOT override.
        apps.merge_file("[Default Applications]\ntext/plain=low.desktop;\n");

        assert_eq!(apps.defaults.get("text/plain").unwrap(), "high.desktop");
        assert_eq!(apps.added.get("text/plain").unwrap(), &["extra.desktop"]);
        assert!(apps
            .removed
            .get("text/html")
            .unwrap()
            .contains("bad.desktop"));
    }
}
