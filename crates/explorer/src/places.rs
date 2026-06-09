//! Sidebar places: home, root, XDG user dirs, network mounts.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
    pub icon: &'static str,
}

/// Build the places list. Runs on a pool worker at startup: it stats
/// candidate directories and — when home itself lives on a network
/// mount — even "is `~/Pictures` there" can stall.
pub fn probe() -> Vec<Place> {
    let mut places = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |label: String, path: PathBuf, icon: &'static str| {
        if seen.insert(path.clone()) {
            places.push(Place { label, path, icon });
        }
    };

    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home {
        push("Home".into(), home.clone(), "folder");
        for dir in xdg_user_dirs(home) {
            if dir != *home && std::fs::metadata(&dir).map(|m| m.is_dir()).unwrap_or(false) {
                let label = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                push(label, dir, "folder");
            }
        }
    }
    push("Root".into(), PathBuf::from("/"), "folder");
    for mount in network_mounts() {
        let label = mount.display().to_string();
        push(label, mount, "activity");
    }
    places
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
