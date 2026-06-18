//! Copy / move engine: the byte-shifting behind paste.
//!
//! Unlike a stat or a listing, a transfer can run for minutes, must
//! report progress, and must be cancellable mid-flight. It is also the
//! one piece of IO that deliberately does *not* go through the [`Pool`]:
//! navigation bumps the pool generation and would cancel queued work,
//! but a paste has to land regardless of where the user browses next.
//! So the engine here is plain functions plus a [`CancelToken`]; the
//! caller spawns its own thread and owns the cancel flag.
//!
//! The shape mirrors how a file manager paste actually works:
//!
//! - [`scan`] walks the sources once, off-thread, to total up the bytes
//!   and files (the progress denominator) and to flag which top-level
//!   items already exist at the destination. The caller resolves those
//!   collisions *before* [`run`] starts — one blanket [`ConflictPolicy`]
//!   for the whole paste — so the worker never has to stop and ask.
//! - [`run`] does the work. A same-filesystem move is a single atomic
//!   `rename(2)` (instant, no byte copy); only a cross-device move falls
//!   back to copy-then-unlink. Copies go in chunks so a large file can
//!   be cancelled partway, and only fully-copied sources are unlinked on
//!   a move — a cancelled or failed transfer never loses data.
//!
//! [`Pool`]: crate::Pool

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared abort flag. Cloning shares the same flag, so the UI thread can
/// hold one handle and the worker another. Checked between files and
/// between chunks of a large file.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Whether paste leaves the sources in place (copy) or removes them once
/// they have fully landed (move).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    Copy,
    Move,
}

/// What to do with a top-level source whose name already exists at the
/// destination. Chosen once, up front, and applied to every colliding
/// item. (`Cancel` is handled before [`run`] — the caller simply doesn't
/// start the transfer.)
///
/// A colliding **directory** is replaced as a whole tree, not merged:
/// `Replace` removes the existing destination directory first. Per-file
/// tree merging is a deliberate non-goal for this version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Overwrite the existing destination (removing a directory tree).
    Replace,
    /// Leave the existing destination untouched; don't copy this item.
    Skip,
    /// Copy alongside under a freed-up name — `foo` → `foo (copy)`.
    Rename,
}

/// A running tally the worker fills in and the UI reads to draw the
/// progress strip. Totals come from [`scan`]; the `_done` fields climb
/// as [`run`] proceeds.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    /// The file currently being copied, for the strip's label.
    pub current: Option<PathBuf>,
}

/// One top-level thing being pasted (a file, a directory, or a symlink),
/// with the size/count its subtree contributes — enough to advance the
/// progress bar by a whole item when a same-filesystem move renames it
/// in one syscall.
#[derive(Debug, Clone)]
pub struct PlanItem {
    /// The absolute source path.
    pub src_root: PathBuf,
    /// `src_root`'s final component — the name it takes at the dest.
    pub name: std::ffi::OsString,
    /// Bytes across the subtree (0 for an empty dir or a symlink).
    pub bytes: u64,
    /// Files across the subtree (a lone file or symlink counts as 1).
    pub files: u64,
    /// `dest/name` already exists — the caller must pick a policy.
    pub collides: bool,
}

/// The fully-measured paste, ready to hand to [`run`]. Cheap to hold: a
/// per-top-level summary, not every leaf (the copy walks live).
#[derive(Debug, Clone)]
pub struct Plan {
    pub mode: TransferMode,
    pub dest: PathBuf,
    pub items: Vec<PlanItem>,
    pub total_bytes: u64,
    pub total_files: u64,
    /// `scan` was cancelled before it finished — don't run this plan.
    pub cancelled: bool,
}

impl Plan {
    /// True if any item's name already exists at the destination, i.e.
    /// the caller should resolve a [`ConflictPolicy`] before running.
    pub fn has_collisions(&self) -> bool {
        self.items.iter().any(|i| i.collides)
    }
}

/// What actually happened. Failures are collected, not fatal: a bad
/// item is recorded and the rest still run (matching the batch-delete
/// behaviour).
#[derive(Debug, Default)]
pub struct Report {
    /// Top-level items that fully landed.
    pub copied: u64,
    /// Items skipped on a `Skip` collision policy.
    pub skipped: u64,
    /// `(source, message)` for items that failed.
    pub failed: Vec<(PathBuf, String)>,
    /// The user cancelled partway; completed items stand.
    pub cancelled: bool,
}

/// Walk `sources` to total their size and file count and flag which ones
/// collide at `dest`. Off-thread; checks `cancel` between entries and
/// bails early (with `Plan::cancelled`) if asked to stop.
pub fn scan(sources: &[PathBuf], dest: &Path, mode: TransferMode, cancel: &CancelToken) -> Plan {
    let mut items = Vec::with_capacity(sources.len());
    let mut total_bytes = 0;
    let mut total_files = 0;
    let mut cancelled = false;

    for src in sources {
        if cancel.is_cancelled() {
            cancelled = true;
            break;
        }
        let Some(name) = src.file_name().map(std::ffi::OsString::from) else {
            continue;
        };
        let (bytes, files) = measure(src, cancel);
        total_bytes += bytes;
        total_files += files;
        let collides = dest.join(&name).symlink_exists();
        items.push(PlanItem {
            src_root: src.clone(),
            name,
            bytes,
            files,
            collides,
        });
    }

    Plan {
        mode,
        dest: dest.to_path_buf(),
        items,
        total_bytes,
        total_files,
        cancelled,
    }
}

/// `(bytes, files)` across `src`'s subtree. A symlink counts as one file
/// of zero bytes (we recreate the link, never follow it).
fn measure(src: &Path, cancel: &CancelToken) -> (u64, u64) {
    let Ok(meta) = std::fs::symlink_metadata(src) else {
        return (0, 0);
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        (0, 1)
    } else if ft.is_dir() {
        let mut bytes = 0;
        let mut files = 0;
        let Ok(entries) = std::fs::read_dir(src) else {
            return (0, 0);
        };
        for entry in entries.flatten() {
            if cancel.is_cancelled() {
                break;
            }
            let (b, f) = measure(&entry.path(), cancel);
            bytes += b;
            files += f;
        }
        (bytes, files)
    } else {
        (meta.len(), 1)
    }
}

/// Execute `plan`, applying `policy` to any colliding item, reporting
/// progress through `on_progress`. Stops promptly when `cancel` fires,
/// leaving already-completed items in place.
pub fn run(
    plan: &Plan,
    policy: ConflictPolicy,
    cancel: &CancelToken,
    on_progress: &mut dyn FnMut(&Progress),
) -> Report {
    let mut report = Report::default();
    let mut progress = Progress {
        bytes_total: plan.total_bytes,
        files_total: plan.total_files,
        ..Progress::default()
    };

    for item in &plan.items {
        if cancel.is_cancelled() {
            report.cancelled = true;
            break;
        }

        // Resolve the destination path under the chosen policy.
        let plain = plan.dest.join(&item.name);
        let target = if item.collides {
            match policy {
                ConflictPolicy::Skip => {
                    report.skipped += 1;
                    // Account for the skipped bytes/files so the bar
                    // still reaches 100%.
                    progress.bytes_done += item.bytes;
                    progress.files_done += item.files;
                    on_progress(&progress);
                    continue;
                }
                ConflictPolicy::Replace => {
                    if let Err(e) = remove_any(&plain) {
                        report.failed.push((item.src_root.clone(), e.to_string()));
                        continue;
                    }
                    plain
                }
                ConflictPolicy::Rename => unique_name(&plan.dest, &item.name),
            }
        } else {
            plain
        };

        match transfer_item(item, &target, plan.mode, cancel, &mut progress, on_progress) {
            Ok(()) => report.copied += 1,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // Cancelled mid-item: drop the partial destination so we
                // don't leave a half-written file/tree behind.
                let _ = remove_any(&target);
                report.cancelled = true;
                break;
            }
            Err(e) => {
                let _ = remove_any(&target);
                report.failed.push((item.src_root.clone(), e.to_string()));
            }
        }
    }

    report
}

/// Move or copy a single top-level item to `target`. A same-filesystem
/// move is one `rename(2)`; everything else copies the subtree, then (on
/// a move) removes the source once it has fully landed.
fn transfer_item(
    item: &PlanItem,
    target: &Path,
    mode: TransferMode,
    cancel: &CancelToken,
    progress: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> std::io::Result<()> {
    if mode == TransferMode::Move && same_filesystem(&item.src_root, target) {
        std::fs::rename(&item.src_root, target)?;
        progress.bytes_done += item.bytes;
        progress.files_done += item.files;
        on_progress(progress);
        return Ok(());
    }

    copy_tree(&item.src_root, target, cancel, progress, on_progress)?;
    if mode == TransferMode::Move {
        // Only reached on full success — safe to drop the source.
        remove_any(&item.src_root)?;
    }
    Ok(())
}

/// Recursively copy `src` to `dst`: directories are recreated and walked,
/// regular files are chunk-copied, symlinks are recreated (never
/// followed). Permissions are preserved on Unix.
fn copy_tree(
    src: &Path,
    dst: &Path,
    cancel: &CancelToken,
    progress: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> std::io::Result<()> {
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    let meta = std::fs::symlink_metadata(src)?;
    let ft = meta.file_type();

    if ft.is_symlink() {
        copy_symlink(src, dst)?;
        progress.files_done += 1;
        on_progress(progress);
    } else if ft.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            if cancel.is_cancelled() {
                return Err(cancelled_error());
            }
            let entry = entry?;
            copy_tree(
                &entry.path(),
                &dst.join(entry.file_name()),
                cancel,
                progress,
                on_progress,
            )?;
        }
        preserve_permissions(&meta, dst);
    } else {
        copy_file(src, dst, &meta, cancel, progress, on_progress)?;
        progress.files_done += 1;
        on_progress(progress);
    }
    Ok(())
}

/// Chunk-copy a regular file, checking `cancel` between chunks so a large
/// file aborts promptly, and ticking `bytes_done` as it goes.
fn copy_file(
    src: &Path,
    dst: &Path,
    meta: &std::fs::Metadata,
    cancel: &CancelToken,
    progress: &mut Progress,
    on_progress: &mut dyn FnMut(&Progress),
) -> std::io::Result<()> {
    const CHUNK: usize = 1 << 20; // 1 MiB

    progress.current = Some(src.to_path_buf());
    let mut reader = std::fs::File::open(src)?;
    let mut writer = std::fs::File::create(dst)?;
    let mut buf = vec![0u8; CHUNK];

    loop {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        progress.bytes_done += n as u64;
        on_progress(progress);
    }
    writer.flush()?;
    preserve_permissions(meta, dst);
    Ok(())
}

fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = std::fs::read_link(src)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dst)
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlink copy is only supported on Unix",
        ))
    }
}

fn preserve_permissions(src_meta: &std::fs::Metadata, dst: &Path) {
    let _ = std::fs::set_permissions(dst, src_meta.permissions());
}

/// Remove a path whatever it is: a directory tree, a file, or a symlink
/// (the link itself, never its target). Missing is success.
fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// True when `src` and the eventual location of `target` sit on the same
/// filesystem, so `rename(2)` will work. Compares the device of `src`
/// against `target`'s parent directory (target itself may not exist yet).
#[cfg(unix)]
fn same_filesystem(src: &Path, target: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dest_dir = target.parent().unwrap_or(target);
    match (std::fs::metadata(src), std::fs::metadata(dest_dir)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_filesystem(_src: &Path, _target: &Path) -> bool {
    false
}

/// Find the first free `name (copy)` / `name (copy 2)` … under `dir`,
/// keeping the extension. Used by [`ConflictPolicy::Rename`].
fn unique_name(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let original = Path::new(name);
    let stem = original
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = original
        .extension()
        .map(|e| e.to_string_lossy().into_owned());

    // Dotfiles (".bashrc") have no real extension — file_stem swallows
    // the whole name, so rebuild from the original string in that case.
    let (stem, ext) = if stem.is_empty() {
        (name.to_string_lossy().into_owned(), None)
    } else {
        (stem, ext)
    };

    for n in 1..usize::MAX {
        let tag = if n == 1 {
            " (copy)".to_string()
        } else {
            format!(" (copy {n})")
        };
        let candidate = match &ext {
            Some(ext) => format!("{stem}{tag}.{ext}"),
            None => format!("{stem}{tag}"),
        };
        let path = dir.join(&candidate);
        if !path.symlink_exists() {
            return path;
        }
    }
    // Astronomically unreachable; fall back to the plain join.
    dir.join(name)
}

fn cancelled_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")
}

/// `Path::try_exists` treats a broken symlink as "doesn't exist" (it
/// follows the link). For collision checks we care whether the *name* is
/// taken, broken link or not — so test the link itself.
trait SymlinkExists {
    fn symlink_exists(&self) -> bool;
}

impl SymlinkExists for Path {
    fn symlink_exists(&self) -> bool {
        std::fs::symlink_metadata(self).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "explorer-io-transfer-{}-{}-{tag}",
            std::process::id(),
            thread_tag(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn thread_tag() -> String {
        format!("{:?}", std::thread::current().id()).replace(|c: char| !c.is_alphanumeric(), "")
    }

    /// `scan` then `run` with a never-firing cancel and a no-op progress
    /// sink — the common test path.
    fn transfer(
        sources: &[PathBuf],
        dest: &Path,
        mode: TransferMode,
        policy: ConflictPolicy,
    ) -> Report {
        let cancel = CancelToken::new();
        let plan = scan(sources, dest, mode, &cancel);
        run(&plan, policy, &cancel, &mut |_| {})
    }

    #[test]
    fn copy_file_leaves_source_and_reports_totals() {
        let root = scratch("copy-file");
        let src = root.join("a.txt");
        let dst = root.join("dest");
        std::fs::write(&src, b"hello").unwrap();
        std::fs::create_dir(&dst).unwrap();

        let cancel = CancelToken::new();
        let plan = scan(
            std::slice::from_ref(&src),
            &dst,
            TransferMode::Copy,
            &cancel,
        );
        assert_eq!(plan.total_bytes, 5);
        assert_eq!(plan.total_files, 1);
        assert!(!plan.has_collisions());

        let report = run(&plan, ConflictPolicy::Replace, &cancel, &mut |_| {});
        assert_eq!(report.copied, 1);
        assert!(report.failed.is_empty());
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert!(src.exists(), "copy keeps the source");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn move_same_filesystem_removes_source() {
        let root = scratch("move-file");
        let src = root.join("a.txt");
        let dst = root.join("dest");
        std::fs::write(&src, b"bye").unwrap();
        std::fs::create_dir(&dst).unwrap();

        let report = transfer(
            std::slice::from_ref(&src),
            &dst,
            TransferMode::Move,
            ConflictPolicy::Replace,
        );
        assert_eq!(report.copied, 1);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"bye");
        assert!(!src.exists(), "move removes the source");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_directory_tree_replicates_structure() {
        let root = scratch("copy-tree");
        let src = root.join("tree");
        std::fs::create_dir_all(src.join("inner")).unwrap();
        std::fs::write(src.join("top.txt"), b"top").unwrap();
        std::fs::write(src.join("inner/leaf.txt"), b"leaf").unwrap();
        let dst = root.join("dest");
        std::fs::create_dir(&dst).unwrap();

        let cancel = CancelToken::new();
        let plan = scan(
            std::slice::from_ref(&src),
            &dst,
            TransferMode::Copy,
            &cancel,
        );
        assert_eq!(plan.total_files, 2);
        assert_eq!(plan.total_bytes, 7);

        let report = run(&plan, ConflictPolicy::Replace, &cancel, &mut |_| {});
        assert_eq!(report.copied, 1);
        assert_eq!(std::fs::read(dst.join("tree/top.txt")).unwrap(), b"top");
        assert_eq!(
            std::fs::read(dst.join("tree/inner/leaf.txt")).unwrap(),
            b"leaf"
        );
        assert!(src.join("top.txt").exists(), "copy keeps the source tree");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn skip_policy_leaves_existing_and_rename_keeps_both() {
        let root = scratch("conflict");
        let src = root.join("a.txt");
        std::fs::write(&src, b"new").unwrap();
        let dst = root.join("dest");
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(dst.join("a.txt"), b"old").unwrap();

        // Skip: existing content survives, nothing new written.
        let report = transfer(
            std::slice::from_ref(&src),
            &dst,
            TransferMode::Copy,
            ConflictPolicy::Skip,
        );
        assert_eq!(report.skipped, 1);
        assert_eq!(report.copied, 0);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"old");

        // Rename: existing untouched, copy lands beside it.
        let report = transfer(
            std::slice::from_ref(&src),
            &dst,
            TransferMode::Copy,
            ConflictPolicy::Rename,
        );
        assert_eq!(report.copied, 1);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read(dst.join("a (copy).txt")).unwrap(), b"new");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn replace_policy_overwrites_existing() {
        let root = scratch("replace");
        let src = root.join("a.txt");
        std::fs::write(&src, b"new").unwrap();
        let dst = root.join("dest");
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(dst.join("a.txt"), b"old").unwrap();

        let cancel = CancelToken::new();
        let plan = scan(
            std::slice::from_ref(&src),
            &dst,
            TransferMode::Copy,
            &cancel,
        );
        assert!(plan.has_collisions());
        let report = run(&plan, ConflictPolicy::Replace, &cancel, &mut |_| {});
        assert_eq!(report.copied, 1);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"new");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn precancelled_token_copies_nothing() {
        let root = scratch("cancel");
        let src = root.join("a.txt");
        std::fs::write(&src, b"data").unwrap();
        let dst = root.join("dest");
        std::fs::create_dir(&dst).unwrap();

        let cancel = CancelToken::new();
        let plan = scan(
            std::slice::from_ref(&src),
            &dst,
            TransferMode::Copy,
            &cancel,
        );
        cancel.cancel();
        let report = run(&plan, ConflictPolicy::Replace, &cancel, &mut |_| {});
        assert!(report.cancelled);
        assert_eq!(report.copied, 0);
        assert!(!dst.join("a.txt").exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn progress_climbs_to_totals() {
        let root = scratch("progress");
        let src = root.join("big.bin");
        // Two chunks plus a tail, so on_progress fires several times.
        std::fs::write(&src, vec![7u8; (1 << 20) * 2 + 13]).unwrap();
        let dst = root.join("dest");
        std::fs::create_dir(&dst).unwrap();

        let cancel = CancelToken::new();
        let plan = scan(
            std::slice::from_ref(&src),
            &dst,
            TransferMode::Copy,
            &cancel,
        );
        let mut max_seen = 0;
        let mut ticks = 0;
        run(&plan, ConflictPolicy::Replace, &cancel, &mut |p| {
            max_seen = max_seen.max(p.bytes_done);
            ticks += 1;
        });
        assert!(ticks >= 2, "progress reported per chunk");
        assert_eq!(max_seen, plan.total_bytes);

        std::fs::remove_dir_all(&root).unwrap();
    }
}
