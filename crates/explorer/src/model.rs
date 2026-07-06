//! The directory model: entries stream in from the listing pass, get
//! lazily enriched by stat results, and are displayed through a sorted
//! `order` index.
//!
//! `entries` is `Arc<Mutex<…>>` because damascene's `virtual_list` row
//! builder is `Fn + Send + Sync + 'static` — display data has to be
//! shared, not cloned per frame (a Ceph directory can hold 100k
//! entries). The mutex is uncontended in practice: only the UI thread
//! touches it (workers communicate by message), the lock just satisfies
//! the bound soundly.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use explorer_io::ceph::CephInfo;
use explorer_io::listing::ListingUpdate;
use explorer_io::stat::EntryMeta;
use explorer_io::{EntryKind, RawEntry};
use explorer_previews::{preview_kind_for_path, Preview, PreviewKind, RawPreview};

use crate::places::Place;

/// Stable entry identity: index into `Listing::entries`, which only
/// ever appends. Sorting happens in `order`, so ids survive resorts —
/// in-flight stat/preview results stay attached to the right file.
pub type EntryId = u32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortMode {
    #[default]
    NameAsc,
    NameDesc,
    TypeAsc,
    ModifiedDesc,
    ModifiedAsc,
    SizeDesc,
    SizeAsc,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::NameAsc => "Name A-Z",
            SortMode::NameDesc => "Name Z-A",
            SortMode::TypeAsc => "Type",
            SortMode::ModifiedDesc => "Modified newest",
            SortMode::ModifiedAsc => "Modified oldest",
            SortMode::SizeDesc => "Size largest",
            SortMode::SizeAsc => "Size smallest",
        }
    }

    pub fn uses_meta(self) -> bool {
        matches!(
            self,
            SortMode::ModifiedDesc | SortMode::ModifiedAsc | SortMode::SizeDesc | SortMode::SizeAsc
        )
    }
}

impl std::fmt::Display for SortMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SortMode::NameAsc => "name-asc",
            SortMode::NameDesc => "name-desc",
            SortMode::TypeAsc => "type-asc",
            SortMode::ModifiedDesc => "modified-desc",
            SortMode::ModifiedAsc => "modified-asc",
            SortMode::SizeDesc => "size-desc",
            SortMode::SizeAsc => "size-asc",
        })
    }
}

pub fn parse_sort_mode(value: &str) -> Option<SortMode> {
    match value {
        "name-asc" => Some(SortMode::NameAsc),
        "name-desc" => Some(SortMode::NameDesc),
        "type-asc" => Some(SortMode::TypeAsc),
        "modified-desc" => Some(SortMode::ModifiedDesc),
        "modified-asc" => Some(SortMode::ModifiedAsc),
        "size-desc" => Some(SortMode::SizeDesc),
        "size-asc" => Some(SortMode::SizeAsc),
        _ => None,
    }
}

pub struct Entry {
    pub name: OsString,
    /// Lossy display name, cached (rows render every frame).
    pub display: String,
    /// Case-folded sort key, cached.
    sort_key: String,
    /// Best-known kind: `d_type` from the listing, upgraded by the
    /// stat pass (which follows symlinks).
    pub kind: EntryKind,
    pub is_symlink: bool,
    /// Name claims an image extension (cached: the grid asks every
    /// frame, and the answer can't change without a new listing).
    pub is_image: bool,
    pub preview_kind: PreviewKind,
    pub meta: Option<EntryMeta>,
    pub meta_error: Option<String>,
    /// CephFS storage pool and (for dirs) recursive size, fetched lazily
    /// like `meta` but only on CephFS mounts. `None` until fetched, or
    /// where the mount isn't CephFS.
    pub ceph: Option<CephInfo>,
}

impl Entry {
    fn from_raw(raw: RawEntry) -> Self {
        let display = raw.name.to_string_lossy().into_owned();
        let preview_kind =
            preview_kind_for_path(std::path::Path::new(&raw.name), raw.kind == EntryKind::Dir);
        Entry {
            sort_key: display.to_lowercase(),
            display,
            is_image: preview_kind == PreviewKind::Image,
            preview_kind,
            name: raw.name,
            kind: raw.kind,
            is_symlink: raw.kind == EntryKind::Symlink,
            meta: None,
            meta_error: None,
            ceph: None,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.display.starts_with('.')
    }

    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

pub type SharedEntries = Arc<Mutex<Vec<Entry>>>;

pub struct Listing {
    pub dir: PathBuf,
    /// Pool generation this listing belongs to; results tagged with
    /// any other generation are strays from a directory already left.
    pub generation: u64,
    pub entries: SharedEntries,
    /// Sorted, hidden-filtered view over `entries`.
    pub order: Arc<Vec<EntryId>>,
    pub complete: bool,
    pub error: Option<String>,
    /// Whether this directory lives on CephFS, probed once when the
    /// listing starts. Gates the per-entry [`CephInfo`] fetches so other
    /// mounts pay no extra `getxattr` calls.
    pub is_ceph: bool,
}

impl Listing {
    pub fn new(dir: PathBuf, generation: u64) -> Self {
        Listing {
            dir,
            generation,
            entries: Arc::new(Mutex::new(Vec::new())),
            order: Arc::new(Vec::new()),
            complete: false,
            error: None,
            is_ceph: false,
        }
    }

    /// Fold one streaming update in. Returns whether `order` changed
    /// (the caller remaps its selection position).
    pub fn absorb(
        &mut self,
        update: ListingUpdate,
        show_hidden: bool,
        filter: Option<&FileFilter>,
        search: Option<&str>,
        sort: SortMode,
    ) -> bool {
        if let Some(e) = update.error {
            self.error = Some(e);
        }
        self.complete |= update.done;
        if update.batch.is_empty() {
            return false;
        }
        self.entries
            .lock()
            .unwrap()
            .extend(update.batch.into_iter().map(Entry::from_raw));
        self.rebuild_order(show_hidden, filter, search, sort);
        true
    }

    /// Apply a stat result. Returns whether `order` changed (a symlink
    /// resolved to a directory regroups it among the dirs).
    pub fn apply_stat(
        &mut self,
        id: EntryId,
        result: Result<EntryMeta, String>,
        show_hidden: bool,
        filter: Option<&FileFilter>,
        search: Option<&str>,
        sort: SortMode,
    ) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let Some(entry) = entries.get_mut(id as usize) else {
            return false;
        };
        let mut regrouped = false;
        match result {
            Ok(meta) => {
                regrouped = entry.is_dir() != (meta.kind == EntryKind::Dir);
                entry.kind = meta.kind;
                entry.is_symlink = meta.is_symlink;
                if entry.kind == EntryKind::Dir {
                    entry.preview_kind = PreviewKind::Directory;
                    entry.is_image = false;
                }
                entry.meta = Some(meta);
            }
            Err(e) => entry.meta_error = Some(e),
        }
        drop(entries);
        if regrouped || sort.uses_meta() {
            self.rebuild_order(show_hidden, filter, search, sort);
            true
        } else {
            false
        }
    }

    /// Attach a CephFS info result to an entry. Never reorders — the pool
    /// and recursive size only feed the badge and Properties, not sort.
    pub fn apply_ceph(&mut self, id: EntryId, info: Option<CephInfo>) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(id as usize) {
            entry.ceph = info;
        }
    }

    /// Recompute `order`: hidden + type filters, directories first,
    /// then case-insensitive by name (raw name as the total-order
    /// tiebreak). The type filter never hides directories — they're
    /// how the user navigates.
    pub fn rebuild_order(
        &mut self,
        show_hidden: bool,
        filter: Option<&FileFilter>,
        search: Option<&str>,
        sort: SortMode,
    ) {
        let entries = self.entries.lock().unwrap();
        let search = search
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let mut order: Vec<EntryId> = (0..entries.len() as EntryId)
            .filter(|&i| {
                let e = &entries[i as usize];
                (show_hidden || !e.is_hidden())
                    && search.as_ref().is_none_or(|q| e.sort_key.contains(q))
                    && (e.is_dir() || filter.is_none_or(|f| f.matches(&e.display)))
            })
            .collect();
        order.sort_by(|&a, &b| compare_entries(&entries[a as usize], &entries[b as usize], sort));
        drop(entries);
        self.order = Arc::new(order);
    }

    pub fn pos_of(&self, id: EntryId) -> Option<usize> {
        self.order.iter().position(|&i| i == id)
    }

    /// Best-effort kind of an entry with this exact name, if one has
    /// streamed in. The save picker uses it to confirm before
    /// overwriting an existing file — `None` includes "not yet listed".
    pub fn kind_of_name(&self, name: &OsStr) -> Option<EntryKind> {
        let entries = self.entries.lock().unwrap();
        entries.iter().find(|e| e.name == name).map(|e| e.kind)
    }

    pub fn id_by_name(&self, name: &OsStr) -> Option<EntryId> {
        let entries = self.entries.lock().unwrap();
        entries
            .iter()
            .position(|e| e.name == name)
            .map(|i| i as EntryId)
    }

    pub fn path_of(&self, id: EntryId) -> PathBuf {
        let entries = self.entries.lock().unwrap();
        self.dir.join(&entries[id as usize].name)
    }
}

fn compare_entries(a: &Entry, b: &Entry, sort: SortMode) -> std::cmp::Ordering {
    use std::cmp::Reverse;

    let directory_group = b.is_dir().cmp(&a.is_dir());
    if directory_group != std::cmp::Ordering::Equal {
        return directory_group;
    }

    let primary = match sort {
        SortMode::NameAsc => a.sort_key.cmp(&b.sort_key),
        SortMode::NameDesc => b.sort_key.cmp(&a.sort_key),
        SortMode::TypeAsc => a
            .preview_kind
            .label()
            .cmp(b.preview_kind.label())
            .then_with(|| a.sort_key.cmp(&b.sort_key)),
        SortMode::ModifiedDesc => known_first(
            a.meta.and_then(|m| m.modified).map(Reverse),
            b.meta.and_then(|m| m.modified).map(Reverse),
        ),
        SortMode::ModifiedAsc => known_first(
            a.meta.and_then(|m| m.modified),
            b.meta.and_then(|m| m.modified),
        ),
        SortMode::SizeDesc => known_first(
            a.meta.map(|m| Reverse(m.size)),
            b.meta.map(|m| Reverse(m.size)),
        ),
        SortMode::SizeAsc => known_first(a.meta.map(|m| m.size), b.meta.map(|m| m.size)),
    };

    primary
        .then_with(|| a.sort_key.cmp(&b.sort_key))
        .then_with(|| a.name.cmp(&b.name))
}

fn known_first<T: Ord>(a: Option<T>, b: Option<T>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Worker result for a grid thumbnail request.
pub enum ThumbResult {
    Image(damascene_core::image::Image),
    /// No thumbnail by policy, usually a cache-only miss.
    Miss,
    Error(String),
}

/// Everything workers post back to the UI thread.
pub enum Msg {
    Listing {
        generation: u64,
        update: ListingUpdate,
    },
    Stat {
        generation: u64,
        id: EntryId,
        result: Result<EntryMeta, String>,
    },
    /// The listing's directory was probed for CephFS membership; `true`
    /// turns on the per-entry pool/recursive-size fetches.
    CephProbe {
        generation: u64,
        is_ceph: bool,
    },
    /// A per-entry CephFS lookup landed: pool and (for dirs) recursive
    /// size, or `None` for an entry that carries neither.
    Ceph {
        generation: u64,
        id: EntryId,
        info: Option<CephInfo>,
    },
    Preview {
        generation: u64,
        id: EntryId,
        result: PreviewPayload,
    },
    Thumb {
        generation: u64,
        id: EntryId,
        result: ThumbResult,
    },
    Places(Vec<Place>),
    /// The desktop-application database, loaded once off-thread at
    /// startup for the "Open with…" chooser.
    AppDb(Arc<crate::apps::AppDb>),
    /// A mutating file operation (create / rename / delete) finished on
    /// its worker thread. Carries the directory to refresh and any error.
    OpDone(crate::ops::OpOutcome),
    /// A copy / move scan found name collisions and paused for a policy
    /// choice. Carries the measured plan for the conflict dialog.
    TransferConflict(crate::ops::PendingTransfer),
    /// A copy / move finished on its worker thread. Carries the
    /// directories to refresh and a summary of what landed or failed.
    TransferDone(crate::ops::TransferOutcome),
    /// The system clipboard was re-read on window focus: a file selection
    /// copied in another app (`Some`) or nothing pasteable (`None`).
    SystemClipboard(Option<crate::sysclip::FileList>),
    /// External command (D-Bus): navigate to `dir`, then select
    /// `select` once it streams in. Not generation-tagged — it isn't a
    /// stale result, it's a fresh instruction.
    OpenLocation {
        dir: PathBuf,
        select: Option<OsString>,
    },
}

pub struct PreviewPayload {
    pub preview: Result<Preview, String>,
    pub raw: Result<RawPreview, String>,
}

/// One file-type filter from a portal request: a display name plus
/// glob and mimetype alternatives — a file passes if *any* alternative
/// matches. Directories are never filtered (see
/// [`Listing::rebuild_order`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFilter {
    pub name: String,
    /// Glob patterns (`*.png`; `*` and `?` wildcards), pre-lowercased
    /// by the parser — matching is case-insensitive.
    pub globs: Vec<String>,
    /// Mimetype patterns, possibly wildcarded (`image/*`), matched
    /// against the extension-guessed type of the file name.
    pub mimes: Vec<String>,
}

impl FileFilter {
    pub fn matches(&self, file_name: &str) -> bool {
        let lower = file_name.to_lowercase();
        self.globs.iter().any(|g| glob_match(g, &lower))
            || self.mimes.iter().any(|m| mime_matches(m, file_name))
    }
}

/// Iterative fnmatch-style glob: `*`, `?`, and `[...]` character
/// classes (sets, ranges, `!` negation). Classes matter in practice:
/// Firefox's GTK picker rewrites every extension filter through
/// `MakeCaseInsensitiveShellGlob` — `*.xsn` arrives on the portal wire
/// as `*.[xX][sS][nN]` — so a matcher without them hides every file.
/// The caller lowercases both sides for case-insensitivity.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    // Most recent `*` and the name position its current expansion
    // started at — on mismatch, grow that expansion by one.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if let Some(next) = token_match(&p, pi, n[ni]) {
            pi = next;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            star = Some((sp, sn + 1));
            pi = sp + 1;
            ni = sn + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// If the single pattern token at `pi` consumes `c`, the index just
/// past that token; `None` on mismatch (or on `*`, which the caller's
/// backtracking owns).
fn token_match(p: &[char], pi: usize, c: char) -> Option<usize> {
    match p.get(pi)? {
        '?' => Some(pi + 1),
        '*' => None,
        '[' => match class_match(p, pi, c) {
            Some((true, next)) => Some(next),
            Some((false, _)) => None,
            // Unterminated class: fnmatch treats the `[` as a literal.
            None => (c == '[').then_some(pi + 1),
        },
        &pc => (pc == c).then_some(pi + 1),
    }
}

/// Match `c` against the `[...]` class starting at `p[pi]`. Returns
/// `(matched, index past the closing bracket)`, or `None` when the
/// class never closes. fnmatch semantics: leading `!` negates, `]` as
/// the first member is a literal, `a-z` is an inclusive range (a `-`
/// adjacent to the closing bracket is a literal member).
fn class_match(p: &[char], pi: usize, c: char) -> Option<(bool, usize)> {
    let mut i = pi + 1;
    let negated = p.get(i) == Some(&'!');
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while let Some(&pc) = p.get(i) {
        if pc == ']' && !first {
            return Some((matched != negated, i + 1));
        }
        if let (Some('-'), Some(&hi)) = (p.get(i + 1).copied(), p.get(i + 2)) {
            if hi != ']' {
                if pc <= c && c <= hi {
                    matched = true;
                }
                i += 3;
                first = false;
                continue;
            }
        }
        if pc == c {
            matched = true;
        }
        i += 1;
        first = false;
    }
    None
}

/// Does the extension-guessed mimetype of `name` satisfy `pattern`
/// (`image/png`, `image/*`, `*/*`)?
fn mime_matches(pattern: &str, name: &str) -> bool {
    let Some((pt, ps)) = pattern.split_once('/') else {
        return false;
    };
    mime_guess::from_path(name)
        .iter()
        .any(|m| (pt == "*" || m.type_() == pt) && (ps == "*" || m.subtype() == ps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(names: &[(&str, EntryKind)], done: bool) -> ListingUpdate {
        ListingUpdate {
            batch: names
                .iter()
                .map(|(n, k)| RawEntry {
                    name: n.into(),
                    kind: *k,
                })
                .collect(),
            done,
            error: None,
        }
    }

    fn ordered_names(l: &Listing) -> Vec<String> {
        let entries = l.entries.lock().unwrap();
        l.order
            .iter()
            .map(|&i| entries[i as usize].display.clone())
            .collect()
    }

    #[test]
    fn streams_sort_dirs_first_case_insensitive() {
        let mut l = Listing::new("/test".into(), 0);
        l.absorb(
            update(
                &[
                    ("zebra.txt", EntryKind::File),
                    ("Apps", EntryKind::Dir),
                    (".hidden", EntryKind::File),
                ],
                false,
            ),
            false,
            None,
            None,
            SortMode::NameAsc,
        );
        assert_eq!(ordered_names(&l), ["Apps", "zebra.txt"]);

        // Second batch interleaves; ids of existing entries survive.
        let zebra = l.id_by_name(OsStr::new("zebra.txt")).unwrap();
        l.absorb(
            update(
                &[("banana", EntryKind::File), ("Zoo", EntryKind::Dir)],
                true,
            ),
            false,
            None,
            None,
            SortMode::NameAsc,
        );
        assert_eq!(ordered_names(&l), ["Apps", "Zoo", "banana", "zebra.txt"]);
        assert!(l.complete);
        assert_eq!(l.id_by_name(OsStr::new("zebra.txt")), Some(zebra));

        // Hidden toggle re-admits dotfiles.
        l.rebuild_order(true, None, None, SortMode::NameAsc);
        assert_eq!(
            ordered_names(&l),
            ["Apps", "Zoo", ".hidden", "banana", "zebra.txt"]
        );
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*.png", "shot.png"));
        assert!(!glob_match("*.png", "shot.png.bak"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("img_????.jpg", "img_0042.jpg"));
        assert!(!glob_match("img_????.jpg", "img_42.jpg"));
        assert!(glob_match("a*b*c", "axxbxxc"));
        assert!(!glob_match("a*b*c", "axxcxxb"));
        assert!(glob_match("*.tar.*", "x.tar.gz"));
        assert!(!glob_match("?", ""));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_character_classes() {
        // The Firefox shape: MakeCaseInsensitiveShellGlob rewrites every
        // extension filter into bracket pairs before it reaches the
        // portal (the parser lowercases it on the way in).
        assert!(glob_match("*.[xx][ss][nn]", "form.xsn"));
        assert!(!glob_match("*.[xx][ss][nn]", "form.xsl"));

        // Sets, ranges, negation.
        assert!(glob_match("img[0-9].png", "img4.png"));
        assert!(!glob_match("img[0-9].png", "imgx.png"));
        assert!(glob_match("[abc]it", "bit"));
        assert!(!glob_match("[abc]it", "fit"));
        assert!(glob_match("[!a]bc", "xbc"));
        assert!(!glob_match("[!a]bc", "abc"));

        // `]` as the first member is literal; a class after `*`
        // exercises the backtracking path.
        assert!(glob_match("[]]x", "]x"));
        assert!(glob_match("*[0-9].log", "run-42.log"));
        assert!(!glob_match("*[0-9].log", "run-x.log"));

        // Unterminated class degrades to a literal `[` (fnmatch).
        assert!(glob_match("a[bc", "a[bc"));
        assert!(!glob_match("a[bc", "abc"));
    }

    /// End-to-end through FileFilter: the exact wire filter Firefox
    /// sends for an `accept=".xsn"` picker — display name "*.xsn",
    /// pattern `*.[xX][sS][nN]` — must match .xsn files of any case.
    #[test]
    fn firefox_case_insensitive_bracket_filter_matches() {
        let filter = FileFilter {
            name: "*.xsn".into(),
            // parse_filter lowercases globs on the way in.
            globs: vec!["*.[xX][sS][nN]".to_lowercase()],
            mimes: vec![],
        };
        assert!(filter.matches("form.xsn"));
        assert!(filter.matches("FORM.XSN"));
        assert!(!filter.matches("form.xml"));
    }

    #[test]
    fn filter_matching() {
        let images = FileFilter {
            name: "Images".into(),
            globs: vec!["*.jxr".into()],
            mimes: vec!["image/*".into()],
        };
        // Glob alternative; case-insensitive via pre-lowered pattern.
        assert!(images.matches("photo.JXR"));
        // Mimetype wildcard via extension guess.
        assert!(images.matches("shot.png"));
        assert!(images.matches("pic.jpeg"));
        assert!(!images.matches("notes.txt"));

        let exact = FileFilter {
            name: "PNG".into(),
            globs: vec![],
            mimes: vec!["image/png".into()],
        };
        assert!(exact.matches("a.png"));
        assert!(!exact.matches("a.jpg"));
    }

    #[test]
    fn rebuild_order_filter_spares_dirs() {
        let mut l = Listing::new("/test".into(), 0);
        l.absorb(
            update(
                &[
                    ("docs", EntryKind::Dir),
                    ("notes.txt", EntryKind::File),
                    ("shot.png", EntryKind::File),
                ],
                true,
            ),
            false,
            None,
            None,
            SortMode::NameAsc,
        );
        let images = FileFilter {
            name: "Images".into(),
            globs: vec!["*.png".into()],
            mimes: vec![],
        };
        l.rebuild_order(false, Some(&images), None, SortMode::NameAsc);
        assert_eq!(ordered_names(&l), ["docs", "shot.png"]);
        // Dropping the filter restores everything.
        l.rebuild_order(false, None, None, SortMode::NameAsc);
        assert_eq!(ordered_names(&l), ["docs", "notes.txt", "shot.png"]);
    }

    #[test]
    fn rebuild_order_search_filters_names_case_insensitive() {
        let mut l = Listing::new("/test".into(), 0);
        l.absorb(
            update(
                &[
                    ("Docs", EntryKind::Dir),
                    ("notes.txt", EntryKind::File),
                    ("shot.png", EntryKind::File),
                    ("archive.zip", EntryKind::File),
                ],
                true,
            ),
            false,
            None,
            None,
            SortMode::NameAsc,
        );

        l.rebuild_order(false, None, Some("O"), SortMode::NameAsc);
        assert_eq!(ordered_names(&l), ["Docs", "notes.txt", "shot.png"]);

        let images = FileFilter {
            name: "Images".into(),
            globs: vec!["*.png".into()],
            mimes: vec![],
        };
        l.rebuild_order(false, Some(&images), Some("shot"), SortMode::NameAsc);
        assert_eq!(ordered_names(&l), ["shot.png"]);
    }

    #[test]
    fn stat_resolving_symlink_to_dir_regroups() {
        let mut l = Listing::new("/test".into(), 0);
        l.absorb(
            update(
                &[("alpha", EntryKind::Dir), ("link", EntryKind::Symlink)],
                true,
            ),
            false,
            None,
            None,
            SortMode::NameAsc,
        );
        assert_eq!(ordered_names(&l), ["alpha", "link"]);

        let id = l.id_by_name(OsStr::new("link")).unwrap();
        let regrouped = l.apply_stat(
            id,
            Ok(EntryMeta {
                size: 0,
                modified: None,
                kind: EntryKind::Dir,
                is_symlink: true,
            }),
            false,
            None,
            None,
            SortMode::NameAsc,
        );
        assert!(regrouped);
        // Still dirs-first, now sorted among them.
        assert_eq!(ordered_names(&l), ["alpha", "link"]);
        let entries = l.entries.lock().unwrap();
        assert!(entries[id as usize].is_dir());
        assert!(entries[id as usize].is_symlink);
    }

    #[test]
    fn rebuild_order_sorts_by_type_and_metadata() {
        let mut l = Listing::new("/test".into(), 0);
        l.absorb(
            update(
                &[
                    ("docs", EntryKind::Dir),
                    ("zeta.bin", EntryKind::File),
                    ("alpha.txt", EntryKind::File),
                    ("photo.png", EntryKind::File),
                ],
                true,
            ),
            false,
            None,
            None,
            SortMode::NameAsc,
        );

        l.rebuild_order(false, None, None, SortMode::TypeAsc);
        assert_eq!(
            ordered_names(&l),
            ["docs", "zeta.bin", "photo.png", "alpha.txt"]
        );

        let zeta = l.id_by_name(OsStr::new("zeta.bin")).unwrap();
        let alpha = l.id_by_name(OsStr::new("alpha.txt")).unwrap();
        l.apply_stat(
            zeta,
            Ok(EntryMeta {
                size: 10,
                modified: None,
                kind: EntryKind::File,
                is_symlink: false,
            }),
            false,
            None,
            None,
            SortMode::SizeDesc,
        );
        l.apply_stat(
            alpha,
            Ok(EntryMeta {
                size: 20,
                modified: None,
                kind: EntryKind::File,
                is_symlink: false,
            }),
            false,
            None,
            None,
            SortMode::SizeDesc,
        );

        assert_eq!(
            ordered_names(&l),
            ["docs", "alpha.txt", "zeta.bin", "photo.png"]
        );
    }

    #[test]
    fn ceph_info_attaches_without_reordering() {
        use explorer_io::ceph::{CephInfo, RecursiveStats};

        let mut l = Listing::new("/test".into(), 0);
        l.absorb(
            update(
                &[("media", EntryKind::Dir), ("clip.mp4", EntryKind::File)],
                true,
            ),
            false,
            None,
            None,
            SortMode::NameAsc,
        );
        let before = ordered_names(&l);
        let dir = l.id_by_name(OsStr::new("media")).unwrap();

        l.apply_ceph(
            dir,
            Some(CephInfo {
                pool: Some("data_hdd".into()),
                recursive: Some(RecursiveStats {
                    bytes: 12_000_000_000_000,
                    files: 1_234_567,
                    subdirs: 8_900,
                }),
            }),
        );

        let entries = l.entries.lock().unwrap();
        let info = entries[dir as usize].ceph.as_ref().unwrap();
        assert_eq!(info.pool.as_deref(), Some("data_hdd"));
        assert_eq!(info.recursive.unwrap().files, 1_234_567);
        drop(entries);
        // Attaching pool/size never disturbs the sort order.
        assert_eq!(ordered_names(&l), before);
    }
}
