//! CephFS metadata via virtual extended attributes.
//!
//! CephFS exposes per-inode information through "virtual" xattrs that
//! never show up in a `listxattr` but can be fetched by name. Two are
//! worth surfacing:
//!
//! - **The storage pool** (`ceph.{dir,file}.layout.pool`) — which RADOS
//!   pool the data lives on. Here that's a tier: `data_ssd` vs
//!   `data_hdd`. A file always resolves an effective pool; a directory
//!   only carries the xattr where a layout is *explicitly* set on it, so
//!   we walk up to the nearest ancestor that has one (the mount root
//!   always does).
//! - **Recursive rollups** (`ceph.dir.r*`) — the MDS keeps a running
//!   total of bytes, files and subdirs under every directory, so a
//!   directory gets a real recursive size for free (no tree walk).
//!
//! Reading an xattr is a syscall against the mount, so callers run
//! [`info`] / [`is_ceph_dir`] on a worker, never the UI thread — the
//! same rule as `stat`. Off CephFS every `getxattr` fails with
//! `ENOTSUP` and these all yield `None` / `false`.

use std::path::Path;

/// What CephFS tells us about one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CephInfo {
    /// The effective storage pool (`data_ssd` / `data_hdd`), resolved up
    /// the tree for directories that inherit it. `None` only if even the
    /// mount root carries no layout — not expected in practice.
    pub pool: Option<String>,
    /// The MDS's recursive rollups; `Some` for directories, `None` for
    /// files.
    pub recursive: Option<RecursiveStats>,
}

/// CephFS recursive directory totals (`ceph.dir.rbytes` and friends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecursiveStats {
    pub bytes: u64,
    pub files: u64,
    pub subdirs: u64,
}

/// Read CephFS info for `path`, following symlinks to the target. A
/// directory is recognized by its recursive byte count (present on every
/// CephFS dir); a file by its layout pool. `None` when `path` isn't on
/// CephFS, or for the rare entry that carries neither (sockets, etc.).
pub fn info(path: &Path) -> Option<CephInfo> {
    // A directory: the MDS always reports recursive bytes, even when no
    // explicit layout is set, so this both detects "dir on CephFS" and
    // gives us the rollups.
    if let Some(bytes) = get_u64(path, "ceph.dir.rbytes") {
        return Some(CephInfo {
            pool: resolve_dir_pool(path),
            recursive: Some(RecursiveStats {
                bytes,
                files: get_u64(path, "ceph.dir.rfiles").unwrap_or(0),
                subdirs: get_u64(path, "ceph.dir.rsubdirs").unwrap_or(0),
            }),
        });
    }
    // A file: its layout always resolves an effective pool on CephFS.
    if let Some(pool) = get_string(path, "ceph.file.layout.pool") {
        return Some(CephInfo {
            pool: Some(pool),
            recursive: None,
        });
    }
    None
}

/// Whether `dir` lives on CephFS, by one cheap `getxattr`. Used once per
/// directory to gate the per-entry [`info`] fetches so non-CephFS mounts
/// pay nothing.
pub fn is_ceph_dir(dir: &Path) -> bool {
    // `ceph.dir.entries` exists on every CephFS directory regardless of
    // its layout, and is a single attribute rather than a rollup walk.
    get(dir, "ceph.dir.entries").is_some()
}

/// The effective pool for a directory: its own layout pool, or the
/// nearest ancestor's (CephFS inherits the layout downward, so a dir
/// without an explicit one reports nothing until we walk up).
fn resolve_dir_pool(dir: &Path) -> Option<String> {
    let mut cur = Some(dir);
    while let Some(p) = cur {
        if let Some(pool) = get_string(p, "ceph.dir.layout.pool") {
            return Some(pool);
        }
        cur = p.parent();
    }
    None
}

/// Fetch a virtual xattr's raw bytes, mapping every failure (missing
/// attribute, `ENOTSUP` off CephFS, permissions) to `None`.
fn get(path: &Path, name: &str) -> Option<Vec<u8>> {
    xattr::get(path, name).ok().flatten()
}

/// A virtual xattr as a trimmed, non-empty UTF-8 string.
fn get_string(path: &Path, name: &str) -> Option<String> {
    let bytes = get(path, name)?;
    let s = String::from_utf8_lossy(&bytes).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

/// A virtual xattr parsed as a decimal `u64` (the rollup counters).
fn get_u64(path: &Path, name: &str) -> Option<u64> {
    get_string(path, name)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off CephFS (a plain temp dir) every probe degrades to nothing,
    /// rather than erroring — the gate that keeps non-Ceph mounts free.
    #[test]
    fn non_ceph_paths_report_nothing() {
        let dir = std::env::temp_dir().join(format!("explorer-io-ceph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, b"x").unwrap();

        assert!(!is_ceph_dir(&dir));
        assert_eq!(info(&dir), None);
        assert_eq!(info(&file), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
