//! Lazy per-entry metadata.

use std::path::Path;
use std::time::SystemTime;

use crate::EntryKind;

/// What one stat round-trip buys. Fetched per entry, prioritized by
/// what's actually on screen — never eagerly for a whole directory.
#[derive(Debug, Clone, Copy)]
pub struct EntryMeta {
    /// File size in bytes (target size for symlinks). Meaningless for
    /// directories — CephFS reports recursive sizes, ext4 reports
    /// block counts; the UI shows neither.
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Kind with symlinks followed; a broken symlink stays
    /// [`EntryKind::Symlink`].
    pub kind: EntryKind,
    /// The entry itself is a symlink (`kind` describes its target).
    pub is_symlink: bool,
}

/// Stat `path`, following symlinks; falls back to the link's own
/// metadata when the target is gone. Runs on a pool worker.
pub fn stat_entry(path: &Path) -> Result<EntryMeta, String> {
    let link_meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let is_symlink = link_meta.file_type().is_symlink();
    let meta = if is_symlink {
        match std::fs::metadata(path) {
            Ok(target) => target,
            Err(_) => {
                // Broken link: report the link itself.
                return Ok(EntryMeta {
                    size: 0,
                    modified: link_meta.modified().ok(),
                    kind: EntryKind::Symlink,
                    is_symlink: true,
                });
            }
        }
    } else {
        link_meta
    };
    let t = meta.file_type();
    let kind = if t.is_dir() {
        EntryKind::Dir
    } else if t.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Ok(EntryMeta {
        size: meta.len(),
        modified: meta.modified().ok(),
        kind,
        is_symlink,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn symlinks_resolve_and_broken_links_survive() {
        let dir = std::env::temp_dir().join(format!("explorer-io-stat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("target.txt"), b"1234").unwrap();
        std::os::unix::fs::symlink("target.txt", dir.join("good")).unwrap();
        std::os::unix::fs::symlink("missing.txt", dir.join("bad")).unwrap();

        let good = stat_entry(&dir.join("good")).unwrap();
        assert_eq!(good.kind, EntryKind::File);
        assert_eq!(good.size, 4);
        assert!(good.is_symlink);

        let bad = stat_entry(&dir.join("bad")).unwrap();
        assert_eq!(bad.kind, EntryKind::Symlink);
        assert!(bad.is_symlink);

        let plain = stat_entry(&dir.join("target.txt")).unwrap();
        assert!(!plain.is_symlink);
        assert_eq!(plain.kind, EntryKind::File);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
