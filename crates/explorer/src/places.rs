//! Sidebar places: home, root, XDG user dirs, network mounts, plus the
//! user's GTK bookmarks (shared with Nautilus and the GTK file chooser).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
    pub icon: &'static str,
    /// A user GTK bookmark (rendered under a separate "Bookmarks"
    /// group) rather than a built-in location.
    pub bookmark: bool,
}

/// Build the places list. Runs on a pool worker at startup: it stats
/// candidate directories and — when home itself lives on a network
/// mount — even "is `~/Pictures` there" can stall.
pub fn probe() -> Vec<Place> {
    let mut places = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |label: String, path: PathBuf, icon: &'static str, bookmark: bool| {
        if seen.insert(path.clone()) {
            places.push(Place {
                label,
                path,
                icon,
                bookmark,
            });
        }
    };

    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home {
        push("Home".into(), home.clone(), "folder", false);
        for dir in xdg_user_dirs(home) {
            if dir != *home && std::fs::metadata(&dir).map(|m| m.is_dir()).unwrap_or(false) {
                let label = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                push(label, dir, "folder", false);
            }
        }
    }
    push("Root".into(), PathBuf::from("/"), "folder", false);
    for mount in network_mounts() {
        let label = mount.display().to_string();
        push(label, mount, "activity", false);
    }
    // User bookmarks last, in their own group. Deliberately not stat'd:
    // a bookmark can point at an unmounted or slow remote path, and the
    // probe must not stall on it — a stale one simply errors when
    // opened, the way GTK's own chooser tolerates them.
    if let Some(home) = &home {
        // The "Bookmarks" group header distinguishes these; damascene's
        // icon set has no bookmark/star glyph, and they are folders.
        for (label, path) in gtk_bookmarks(home) {
            push(label, path, "folder", true);
        }
    }
    places
}

/// Parse `~/.config/gtk-3.0/bookmarks` — the file Nautilus and the GTK
/// file chooser share. Each line is a `file://` URI with an optional
/// trailing custom label (`file:///ceph/photos Work photos`); non-file
/// schemes (sftp://, smb://) are skipped. Missing file → no bookmarks.
fn gtk_bookmarks(home: &Path) -> Vec<(String, PathBuf)> {
    let Ok(content) = std::fs::read_to_string(home.join(".config/gtk-3.0/bookmarks")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A real space in the path is percent-encoded, so the first
        // whitespace separates the URI from an optional display label.
        let (uri, label) = match line.split_once(char::is_whitespace) {
            Some((uri, label)) => (uri, label.trim()),
            None => (line, ""),
        };
        let Some(path) = crate::filemanager1::file_uri_to_path(uri) else {
            continue;
        };
        let label = if label.is_empty() {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        } else {
            label.to_string()
        };
        out.push((label, path));
    }
    out
}

/// Parse `~/.config/user-dirs.dirs` (`XDG_PICTURES_DIR="$HOME/Pictures"`
/// lines). Missing file or unparsable lines just mean fewer places.
fn xdg_user_dirs(home: &std::path::Path) -> Vec<PathBuf> {
    let Ok(content) = std::fs::read_to_string(home.join(".config/user-dirs.dirs")) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.starts_with("XDG_") || !key.ends_with("_DIR") {
            continue;
        }
        let value = value.trim_matches('"');
        let path = if let Some(rest) = value.strip_prefix("$HOME/") {
            home.join(rest)
        } else if value.starts_with('/') {
            PathBuf::from(value)
        } else {
            continue;
        };
        dirs.push(path);
    }
    dirs
}

/// Mount points of network filesystems worth a sidebar slot, from
/// `/proc/mounts`. Ceph first-class; NFS/SMB ride along.
fn network_mounts() -> Vec<PathBuf> {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_dev), Some(mount), Some(fstype)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if matches!(
            fstype,
            "ceph" | "fuse.ceph-fuse" | "nfs" | "nfs4" | "cifs" | "smb3"
        ) {
            // /proc/mounts octal-escapes spaces; rare for mount points,
            // skip the unescape until it matters.
            out.push(PathBuf::from(mount));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtk_bookmarks_parses_uris_labels_and_skips_non_file() {
        let home = std::env::temp_dir().join(format!("prism-places-test-{}", std::process::id()));
        let cfg = home.join(".config/gtk-3.0");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("bookmarks"),
            "file:///ceph/photos\n\
             file:///ceph/work%20files Work\n\
             sftp://nas/remote\n\
             \n\
             file:///home/me/Projects   Code  \n",
        )
        .unwrap();

        let got = gtk_bookmarks(&home);
        assert_eq!(
            got,
            vec![
                // No label → basename.
                ("photos".to_string(), PathBuf::from("/ceph/photos")),
                // Custom label kept; %20 decoded in the path.
                ("Work".to_string(), PathBuf::from("/ceph/work files")),
                // sftp:// and the blank line are skipped.
                ("Code".to_string(), PathBuf::from("/home/me/Projects")),
            ]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn gtk_bookmarks_missing_file_is_empty() {
        let home = std::env::temp_dir().join("prism-places-test-absent-xyz");
        assert!(gtk_bookmarks(&home).is_empty());
    }
}
