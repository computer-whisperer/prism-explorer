//! Live CephFS smoke check: confirms the `xattr` crate actually reads
//! CephFS *virtual* xattrs (pool + recursive rollups) end-to-end, which
//! a plain unit test can't exercise. Ignored by default and self-skips
//! when no `/ceph` mount is present, so CI and non-Ceph machines stay
//! green. Run with:
//!
//!   cargo test -p explorer-io --test ceph_live -- --ignored --nocapture

use std::path::Path;

#[test]
#[ignore]
fn reads_pool_and_recursive_from_real_mount() {
    let root = Path::new("/ceph");
    if !root.exists() {
        eprintln!("no /ceph mount; skipping");
        return;
    }
    assert!(
        explorer_io::ceph::is_ceph_dir(root),
        "/ceph should be CephFS"
    );
    let info = explorer_io::ceph::info(root).expect("CephInfo for /ceph");
    let r = info.recursive.expect("a directory has recursive stats");
    eprintln!(
        "pool={:?} rbytes={} rfiles={} rsubdirs={}",
        info.pool, r.bytes, r.files, r.subdirs
    );
    assert!(info.pool.is_some(), "the mount root resolves a pool");
    assert!(r.bytes > 0, "a non-empty tree has recursive bytes");
}
