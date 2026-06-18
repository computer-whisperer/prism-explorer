//! Mutating file operations — create folder, rename, delete.
//!
//! These break the browser's otherwise read-only contract, so they obey
//! two rules. Like every filesystem touch they run off the UI thread (a
//! CephFS rename can stall for hundreds of milliseconds). But unlike
//! previews and stats they must *not* be cancelled when the user
//! navigates away — a trash or rename has to land regardless. The pool's
//! generation-cancellation is therefore the wrong tool: each op runs on
//! its own detached thread (the idiom the places / app-db probes already
//! use) and posts an [`OpOutcome`] back as `Msg::OpDone`.
//!
//! Scope is deliberately the safe, near-instant metadata operations.
//! Copy / move (byte-shifting, progress, conflict resolution) is a
//! separate, larger chunk.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use explorer_io::{Report, TransferMode};

/// What a finished copy / move reports back to the UI. The worker owns
/// absolute paths, so (like [`OpOutcome`]) it stays valid wherever the
/// user has navigated to by the time it lands.
pub struct TransferOutcome {
    pub mode: TransferMode,
    /// The directory pasted into — refreshed if the user is still there.
    pub dest: PathBuf,
    /// The directory the moved sources came from — also refreshed (a
    /// move empties it). `None` for a copy or when sources span dirs.
    pub source_dir: Option<PathBuf>,
    pub report: Report,
}

/// A pending mutating operation, described in full so it can run on a
/// worker thread with no access to app state.
pub enum FileOp {
    /// Create directory `name` under `parent`.
    NewFolder { parent: PathBuf, name: String },
    /// Rename `from` to `to` within the same directory.
    Rename { from: PathBuf, to: PathBuf },
    /// Move every path to the XDG trash (recoverable). One op per
    /// multi-selection: the paths share a directory, so a single
    /// post-op refresh covers them all.
    Trash { paths: Vec<PathBuf> },
    /// Permanently remove every path — a file, a symlink (not its
    /// target), or a directory tree. Irreversible.
    DeletePermanent { paths: Vec<PathBuf> },
}

/// What the UI should do once an op finishes. Always produced — failures
/// ride in `error`, the op never panics the worker.
pub struct OpOutcome {
    /// The directory the op acted in. The UI refreshes its listing only
    /// if the user is still there.
    pub dir: PathBuf,
    /// A name to focus after the refresh (the created / renamed entry).
    /// `None` for deletions.
    pub select: Option<OsString>,
    /// Human-readable failure, or `None` on success.
    pub error: Option<String>,
}

impl FileOp {
    /// The directory whose listing a successful op invalidates — used to
    /// scope the post-op refresh.
    pub fn dir(&self) -> PathBuf {
        match self {
            FileOp::NewFolder { parent, .. } => parent.clone(),
            FileOp::Rename { from, .. } => parent_of(from),
            FileOp::Trash { paths } | FileOp::DeletePermanent { paths } => {
                paths.first().map(|p| parent_of(p)).unwrap_or_default()
            }
        }
    }

    /// Run the operation (blocking IO). The outcome carries the parent
    /// directory to refresh, the entry to re-select, and any error —
    /// prefixed with what was being attempted so the modal names it. A
    /// batch op (trash / delete) runs every path even if one fails, and
    /// reports a summary of the failures.
    pub fn run(self) -> OpOutcome {
        let dir = self.dir();
        let (select, error) = match self {
            FileOp::NewFolder { parent, name } => {
                let target = parent.join(&name);
                // create_dir (not _all) refuses to clobber an existing
                // entry — a single atomic syscall, no check-then-act gap.
                let error = std::fs::create_dir(&target)
                    .err()
                    .map(|e| format!("Couldn't create the folder \u{201c}{name}\u{201d}: {e}"));
                (Some(OsString::from(&name)), error)
            }
            FileOp::Rename { from, to } => {
                let select = to.file_name().map(OsString::from);
                // Refuse to silently replace an existing destination
                // (fs::rename would overwrite a file in place). This is a
                // best-effort check-then-rename, not atomic: a racing
                // create between the two could still be clobbered. std
                // has no `renameat2(RENAME_NOREPLACE)`; for a local
                // single-user browser the window is acceptable.
                let result = match to.try_exists() {
                    Ok(true) => Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "a file with that name already exists",
                    )),
                    Ok(false) => std::fs::rename(&from, &to),
                    Err(e) => Err(e),
                };
                let error = result.err().map(|e| {
                    format!(
                        "Couldn't rename to \u{201c}{}\u{201d}: {e}",
                        file_name_str(&to)
                    )
                });
                (select, error)
            }
            FileOp::Trash { paths } => (
                None,
                batch_error(&paths, "move", " to Trash", |p| {
                    trash::delete(p).map_err(|e| std::io::Error::other(e.to_string()))
                }),
            ),
            FileOp::DeletePermanent { paths } => {
                (None, batch_error(&paths, "delete", "", delete_permanent))
            }
        };
        OpOutcome { dir, select, error }
    }
}

/// Run `run_one` over every path, then summarise any failures into one
/// message. `None` means everything succeeded. The phrasing matches the
/// single-item wording when there's exactly one path (`Couldn't move
/// "x" to Trash: …`) and counts otherwise (`Couldn't move 2 of 5 items
/// to Trash: …`), always naming the first error.
fn batch_error(
    paths: &[PathBuf],
    verb: &str,
    suffix: &str,
    run_one: impl Fn(&Path) -> std::io::Result<()>,
) -> Option<String> {
    let mut first: Option<(String, std::io::Error)> = None;
    let mut failed = 0usize;
    for path in paths {
        if let Err(e) = run_one(path) {
            failed += 1;
            first.get_or_insert_with(|| (file_name_str(path), e));
        }
    }
    let (name, err) = first?;
    Some(if paths.len() == 1 {
        format!("Couldn't {verb} \u{201c}{name}\u{201d}{suffix}: {err}")
    } else {
        format!(
            "Couldn't {verb} {failed} of {} items{suffix}: {err}",
            paths.len()
        )
    })
}

/// Permanently remove `path`. Directories go recursively; a symlink is
/// removed itself, never followed.
fn delete_permanent(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// The final component of `path` for display, lossily.
fn file_name_str(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A name typed into the New Folder / Rename field is usable as a single
/// path component: non-empty, no separator, not a directory alias.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && name != "." && name != ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "explorer-ops-{}-{}-{tag}",
            std::process::id(),
            // Distinct per call within a process: the thread id is unique
            // among live threads, and tests run on their own threads.
            thread_tag(),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn thread_tag() -> String {
        format!("{:?}", std::thread::current().id()).replace(|c: char| !c.is_alphanumeric(), "")
    }

    #[test]
    fn new_folder_creates_and_refuses_clobber() {
        let dir = scratch("newfolder");
        let out = FileOp::NewFolder {
            parent: dir.clone(),
            name: "sub".into(),
        }
        .run();
        assert!(out.error.is_none(), "{:?}", out.error);
        assert_eq!(out.dir, dir);
        assert_eq!(out.select.as_deref(), Some(std::ffi::OsStr::new("sub")));
        assert!(dir.join("sub").is_dir());

        // Second create over the same name is refused, not silently
        // merged.
        let again = FileOp::NewFolder {
            parent: dir.clone(),
            name: "sub".into(),
        }
        .run();
        assert!(again.error.is_some());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_moves_and_refuses_existing_destination() {
        let dir = scratch("rename");
        std::fs::write(dir.join("a.txt"), b"hi").unwrap();
        std::fs::write(dir.join("b.txt"), b"bye").unwrap();

        let out = FileOp::Rename {
            from: dir.join("a.txt"),
            to: dir.join("renamed.txt"),
        }
        .run();
        assert!(out.error.is_none(), "{:?}", out.error);
        assert_eq!(
            out.select.as_deref(),
            Some(std::ffi::OsStr::new("renamed.txt"))
        );
        assert!(!dir.join("a.txt").exists());
        assert!(dir.join("renamed.txt").exists());

        // Renaming onto an existing file must not overwrite it.
        let clobber = FileOp::Rename {
            from: dir.join("renamed.txt"),
            to: dir.join("b.txt"),
        }
        .run();
        assert!(clobber.error.is_some());
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"bye");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_permanent_handles_files_and_trees() {
        let dir = scratch("delete");
        std::fs::write(dir.join("file"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("tree/inner")).unwrap();
        std::fs::write(dir.join("tree/inner/leaf"), b"y").unwrap();

        assert!(FileOp::DeletePermanent {
            paths: vec![dir.join("file")],
        }
        .run()
        .error
        .is_none());
        assert!(!dir.join("file").exists());

        assert!(FileOp::DeletePermanent {
            paths: vec![dir.join("tree")],
        }
        .run()
        .error
        .is_none());
        assert!(!dir.join("tree").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn trash_and_delete_run_every_path_and_summarise_failures() {
        let dir = scratch("batch");
        std::fs::write(dir.join("a"), b"a").unwrap();
        std::fs::write(dir.join("b"), b"b").unwrap();

        // Two real entries plus one that doesn't exist: the present ones
        // are removed and the single failure is reported with a count.
        let out = FileOp::DeletePermanent {
            paths: vec![dir.join("a"), dir.join("missing"), dir.join("b")],
        }
        .run();
        assert!(!dir.join("a").exists());
        assert!(!dir.join("b").exists());
        let msg = out.error.expect("the missing path should fail");
        assert!(msg.contains("1 of 3 items"), "{msg}");
        assert_eq!(out.dir, dir);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn valid_name_rejects_separators_and_aliases() {
        assert!(valid_name("notes.txt"));
        assert!(valid_name("My Folder"));
        assert!(!valid_name(""));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("."));
        assert!(!valid_name(".."));
    }
}
