//! Streaming directory reads.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::{EntryKind, RawEntry};

/// One chunk of a streaming listing. Batches grow geometrically (the
/// first paint should happen after a few dozen entries; a 100k-entry
/// directory shouldn't pay 100k channel sends) and flush on a timer so
/// a trickling Ceph readdir still shows progress.
#[derive(Debug)]
pub struct ListingUpdate {
    pub batch: Vec<RawEntry>,
    /// Final update for this listing — no more batches follow.
    pub done: bool,
    /// Opening or reading the directory failed; `done` is set.
    pub error: Option<String>,
}

/// First flush after this many entries…
const FIRST_BATCH: usize = 64;
/// …doubling per flush up to this cap.
const MAX_BATCH: usize = 4096;
/// Flush whatever has accumulated when entries trickle in slowly.
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Read `dir`, calling `emit` with batches as they accumulate and a
/// final update with `done = true`. Runs on a pool worker — this
/// function blocks in `readdir` and is exactly the thing the UI thread
/// must never call.
///
/// `emit` returns whether to keep reading: a `false` (the user
/// navigated away — typically `pool.generation()` no longer matches)
/// abandons the iteration immediately, without a final `done` update.
/// In-flight work on a 100k-entry Ceph directory shouldn't outlive the
/// view it was for.
///
/// Entry kinds come from `d_type` (`DirEntry::file_type`), which costs
/// no extra round-trip on CephFS; symlinks are reported as
/// [`EntryKind::Symlink`] unresolved. No per-entry stat happens here.
pub fn read_dir_streaming(dir: &Path, mut emit: impl FnMut(ListingUpdate) -> bool) {
    let iter = match std::fs::read_dir(dir) {
        Ok(iter) => iter,
        Err(e) => {
            emit(ListingUpdate {
                batch: Vec::new(),
                done: true,
                error: Some(format!("opening {}: {e}", dir.display())),
            });
            return;
        }
    };

    let mut batch = Vec::new();
    let mut cap = FIRST_BATCH;
    let mut last_flush = Instant::now();
    for entry in iter {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                // Mid-iteration error: surface what we have, then fail.
                emit(ListingUpdate {
                    batch: std::mem::take(&mut batch),
                    done: true,
                    error: Some(format!("reading {}: {e}", dir.display())),
                });
                return;
            }
        };
        let kind = match entry.file_type() {
            Ok(t) if t.is_dir() => EntryKind::Dir,
            Ok(t) if t.is_file() => EntryKind::File,
            Ok(t) if t.is_symlink() => EntryKind::Symlink,
            Ok(_) => EntryKind::Other,
            // d_type unknown and the fallback stat failed — keep the
            // name; the lazy stat pass will report the real problem.
            Err(_) => EntryKind::Other,
        };
        batch.push(RawEntry {
            name: entry.file_name(),
            kind,
        });
        if batch.len() >= cap || last_flush.elapsed() >= FLUSH_INTERVAL {
            let keep_going = emit(ListingUpdate {
                batch: std::mem::take(&mut batch),
                done: false,
                error: None,
            });
            if !keep_going {
                return;
            }
            cap = (cap * 2).min(MAX_BATCH);
            last_flush = Instant::now();
        }
    }
    emit(ListingUpdate {
        batch,
        done: true,
        error: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("explorer-io-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_kinds_and_completes() {
        let dir = scratch_dir("listing");
        std::fs::write(dir.join("a.txt"), b"hi").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();

        let mut entries = Vec::new();
        let mut done = false;
        read_dir_streaming(&dir, |u| {
            assert!(!done, "updates after done");
            assert!(u.error.is_none(), "{:?}", u.error);
            entries.extend(u.batch);
            done |= u.done;
            true
        });
        assert!(done);

        let kind_of = |name: &str| {
            entries
                .iter()
                .find(|e| e.name == std::ffi::OsStr::new(name))
                .map(|e| e.kind)
        };
        assert_eq!(kind_of("a.txt"), Some(EntryKind::File));
        assert_eq!(kind_of("sub"), Some(EntryKind::Dir));
        #[cfg(unix)]
        assert_eq!(kind_of("link"), Some(EntryKind::Symlink));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_dir_reports_error() {
        let mut saw_error = false;
        read_dir_streaming(Path::new("/nonexistent/explorer-io-test"), |u| {
            assert!(u.done);
            saw_error |= u.error.is_some();
            true
        });
        assert!(saw_error);
    }

    /// Returning `false` from `emit` abandons the iteration: exactly
    /// one update arrives even though more entries remain.
    #[test]
    fn emit_false_stops_reading() {
        let dir = scratch_dir("stop");
        for i in 0..(super::FIRST_BATCH * 3) {
            std::fs::write(dir.join(format!("f{i:04}")), b"").unwrap();
        }
        let mut updates = 0;
        read_dir_streaming(&dir, |u| {
            assert!(!u.done, "should stop before the final update");
            updates += 1;
            false
        });
        assert_eq!(updates, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
