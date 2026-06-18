//! The explorer [`App`]: sidebar of places, breadcrumb navigation, a
//! virtualized directory listing, and a preview pane for the selected
//! file.
//!
//! Built for big, slow filesystems: the UI thread never touches the
//! filesystem. Listings stream in (names first, batched), metadata is
//! stat'ed lazily for rows the list actually realizes, the selected
//! file's preview decodes at the front of the queue, and navigating
//! away drops every queued job for the directory being left.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use damascene_core::image::Image;
use damascene_core::prelude::*;
use damascene_core::scroll::{ScrollAlignment, ScrollRequest};
use damascene_core::selection::{Selection, SelectionPoint, SelectionRange};
use damascene_core::surface::SurfaceAlpha;
use damascene_core::widgets::dialog::{dialog, dialog_footer, dialog_header, dialog_title};
use damascene_core::widgets::popover::{context_menu, menu_item};
use damascene_core::widgets::resize_handle::HANDLE_THICKNESS;
use damascene_core::widgets::select::{self, select_menu, select_trigger};
use damascene_core::widgets::tabs::{self, tabs_list};
use damascene_core::widgets::text_input::{self, TextInputOpts};
use damascene_core::{BuildCx, EventCx, KeyChord, KeyModifiers, Rect, UiEvent, UiEventKind, UiKey};
use lru::LruCache;

use explorer_io::{listing, stat, EntryKind, Notifier, Pool, Tier};
use explorer_previews::{BinaryPreview, CodeSpan, Preview, RawPreview, Registry};
use explorer_thumbs::ThumbCache;

use crate::apps::AppDb;
use crate::binary_surface::{BinarySurface, BinarySurfaceMetrics};
use crate::fmt;
use crate::model::{
    parse_sort_mode, Entry, EntryId, FileFilter, Listing, Msg, PreviewPayload, SortMode,
    ThumbResult,
};
use crate::places::Place;
use crate::preview_policy::{grid_thumbnail_policy, GridThumbPolicy};

const ROW_H: f32 = 34.0;
const SIDEBAR_MIN: f32 = 160.0;
const SIDEBAR_MAX: f32 = 420.0;
const PREVIEW_MIN: f32 = 260.0;
const PREVIEW_MAX: f32 = 900.0;
const PREVIEW_ABSOLUTE_MIN: f32 = 160.0;
const LISTING_MIN: f32 = 280.0;

// Grid view geometry. Cells are media (thumbnail or icon) over a name
// caption; the virtual list row height bakes the gap in, which also
// keeps focus rings clear of the next row.
const TILE_W: f32 = 128.0;
const TILE_MEDIA_H: f32 = 84.0;
const TILE_H: f32 = 116.0;
const TILE_GAP: f32 = tokens::SPACE_2;

/// RAM cap on decoded thumbnails (LRU). At the 256px cache edge a 16:9
/// thumb is ~300 KB of f16, so this bounds thumbnail RAM near 150 MB
/// however large the directory is.
const RAM_THUMBS: usize = 512;

/// Display cap for text previews — the handler already bounds the read
/// at 128 KiB; this bounds what one mono text leaf has to lay out.
const TEXT_PREVIEW_MAX_LINES: usize = 400;
const SORT_STAT_BATCH: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ViewMode {
    List,
    Grid,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BrowserChrome {
    sidebar_w: Option<f32>,
    preview_w: Option<f32>,
}

/// Decoded thumbnails plus request bookkeeping. Shared with the grid's
/// `'static` cell builder (same Arc<Mutex> rationale as the entries —
/// UI thread only, the lock just satisfies the bound).
struct ThumbState {
    ram: LruCache<EntryId, Image>,
    /// Jobs submitted, to dedupe across frames.
    requested: HashSet<EntryId>,
    /// Entries with no usable grid thumbnail. Decode errors are logged;
    /// policy misses, like oversized cache misses, stay quiet.
    failed: HashSet<EntryId>,
}

impl ThumbState {
    fn new() -> Self {
        ThumbState {
            ram: LruCache::new(NonZeroUsize::new(RAM_THUMBS).expect("nonzero")),
            requested: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    /// Entry ids restart at 0 in a new directory — everything here is
    /// meaningless after a navigation.
    fn reset(&mut self) {
        self.ram.clear();
        self.requested.clear();
        self.failed.clear();
    }
}

/// What activating a *file* (Enter / double-click) does. Directories
/// always navigate.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileActivation {
    /// Hand to the system opener — the browser-window behavior.
    SystemOpen,
    /// Record into an outbox the wrapper drains — picker dialogs treat
    /// activation as "choose this file".
    Collect,
}

enum PreviewState {
    /// Nothing selected, or a directory is.
    Empty,
    Loading {
        id: EntryId,
    },
    Ready {
        id: EntryId,
        preview: Result<Preview, String>,
        raw: Option<RawPreview>,
    },
    Failed {
        id: EntryId,
        error: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewMode {
    Normal,
    Text,
    Binary,
}

impl std::fmt::Display for PreviewMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PreviewMode::Normal => "normal",
            PreviewMode::Text => "text",
            PreviewMode::Binary => "binary",
        })
    }
}

fn parse_preview_mode(value: &str) -> Option<PreviewMode> {
    match value {
        "normal" => Some(PreviewMode::Normal),
        "text" => Some(PreviewMode::Text),
        "binary" => Some(PreviewMode::Binary),
        _ => None,
    }
}

fn preview_modes(
    preview: Option<&Preview>,
    raw: Option<&RawPreview>,
) -> Vec<(PreviewMode, &'static str)> {
    let mut modes = vec![(PreviewMode::Normal, "Auto")];
    if raw.and_then(|raw| raw.text.as_ref()).is_some()
        || matches!(preview, Some(Preview::Text { .. } | Preview::Code { .. }))
    {
        modes.push((PreviewMode::Text, "Text"));
    }
    if raw.is_some() || matches!(preview, Some(Preview::Binary(_))) {
        modes.push((PreviewMode::Binary, "Bin"));
    }
    modes
}

fn effective_preview_mode(
    wanted: PreviewMode,
    preview: Option<&Preview>,
    raw: Option<&RawPreview>,
) -> PreviewMode {
    if preview_modes(preview, raw)
        .iter()
        .any(|(mode, _)| *mode == wanted)
    {
        wanted
    } else {
        PreviewMode::Normal
    }
}

fn browser_chrome_widths(
    viewport_w: f32,
    requested_sidebar_w: f32,
    requested_preview_w: f32,
    wants_preview: bool,
) -> BrowserChrome {
    let mut show_sidebar = true;
    let mut show_preview = wants_preview;

    if show_sidebar
        && show_preview
        && !browser_layout_fits(viewport_w, Some(SIDEBAR_MIN), Some(PREVIEW_MIN))
    {
        show_sidebar = false;
    }
    if show_sidebar && !show_preview && !browser_layout_fits(viewport_w, Some(SIDEBAR_MIN), None) {
        show_sidebar = false;
    }
    if show_preview
        && !browser_layout_fits(
            viewport_w,
            show_sidebar.then_some(SIDEBAR_MIN),
            Some(PREVIEW_ABSOLUTE_MIN),
        )
    {
        show_preview = false;
    }

    let mut sidebar_w = show_sidebar.then(|| requested_sidebar_w.clamp(SIDEBAR_MIN, SIDEBAR_MAX));
    let mut preview_w = show_preview.then(|| requested_preview_w.clamp(PREVIEW_MIN, PREVIEW_MAX));

    let chrome = content_chrome_x(show_sidebar, show_preview);
    if let Some(width) = &mut preview_w {
        let sidebar = sidebar_w.unwrap_or(0.0);
        let max =
            (viewport_w - chrome - sidebar - LISTING_MIN).clamp(PREVIEW_ABSOLUTE_MIN, PREVIEW_MAX);
        *width = (*width).min(max);
    }
    if let Some(width) = &mut sidebar_w {
        let preview = preview_w.unwrap_or(0.0);
        let max = (viewport_w - chrome - preview - LISTING_MIN).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        *width = (*width).min(max);
    }

    BrowserChrome {
        sidebar_w,
        preview_w,
    }
}

fn browser_layout_fits(viewport_w: f32, sidebar_w: Option<f32>, preview_w: Option<f32>) -> bool {
    viewport_w
        >= content_chrome_x(sidebar_w.is_some(), preview_w.is_some())
            + sidebar_w.unwrap_or(0.0)
            + preview_w.unwrap_or(0.0)
            + LISTING_MIN
}

fn content_chrome_x(show_sidebar: bool, show_preview: bool) -> f32 {
    let handles = show_sidebar as usize + show_preview as usize;
    let children = 1 + handles * 2;
    let gaps = children.saturating_sub(1);
    2.0 * tokens::SPACE_4 + gaps as f32 * tokens::SPACE_2 + handles as f32 * HANDLE_THICKNESS
}

fn listing_available_w(viewport_w: f32, chrome: BrowserChrome) -> f32 {
    viewport_w
        - content_chrome_x(chrome.sidebar_w.is_some(), chrome.preview_w.is_some())
        - chrome.sidebar_w.unwrap_or(0.0)
        - chrome.preview_w.unwrap_or(0.0)
}

pub struct ExplorerApp {
    pool: Pool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    notifier: Notifier,
    registry: Arc<Registry>,

    cwd: PathBuf,
    listing: Listing,
    places: Vec<Place>,
    show_hidden: bool,
    /// Picker-imposed type filter (portal `filters`); the browser
    /// window never sets one. Applies to files only, in
    /// `Listing::rebuild_order`.
    file_filter: Option<FileFilter>,
    /// Browser name filter for the current directory listing. This is
    /// deliberately local to the streamed entries we already have, not
    /// a recursive filesystem search.
    search: String,
    selection: Selection,
    show_search: bool,
    sort: SortMode,
    sort_open: bool,
    focus_requests: Vec<String>,
    view: ViewMode,
    file_activation: FileActivation,
    /// Files activated under [`FileActivation::Collect`], drained by
    /// the wrapping picker.
    activated: Vec<PathBuf>,

    thumbs: Arc<ThumbCache>,
    thumb_state: Arc<Mutex<ThumbState>>,
    /// Columns the grid laid out last frame (build-time, viewport
    /// dependent); keyboard navigation moves by this much vertically.
    cols: Cell<usize>,

    /// The cursor: stable id plus its current position in
    /// `listing.order` (kept in sync across resorts). Drives the
    /// preview pane and keyboard navigation.
    selected: Option<(EntryId, usize)>,
    /// Multi-selection beyond the cursor (Ctrl/Shift/Space). Empty is
    /// plain single-select — the cursor is the whole selection. A
    /// `multiple` picker returns this set.
    marked: HashSet<EntryId>,
    /// Range-select anchor for Shift+click; set by plain click,
    /// Ctrl+click, and Space.
    anchor: Option<EntryId>,
    /// Select this name once it appears in the stream — set when
    /// navigating to a parent so the directory we came from is focused.
    pending_select: Option<OsString>,

    preview: PreviewState,
    preview_mode: PreviewMode,
    /// At most one preview decode in flight; the latest wanted id wins
    /// when it finishes (holding an arrow key through a directory must
    /// not queue fifty decodes).
    preview_inflight: Option<EntryId>,
    preview_wanted: Option<EntryId>,

    /// Stat jobs submitted, to dedupe across frames (the row builder
    /// re-runs for visible rows every frame). Shared with the builder
    /// closure, hence the mutex (uncontended; UI thread only).
    stat_requested: Arc<Mutex<HashSet<EntryId>>>,
    scroll_requests: RefCell<Vec<ScrollRequest>>,

    sidebar_w: f32,
    preview_w: f32,
    sidebar_drag: ResizeDrag,
    preview_drag: ResizeDrag,
    binary_surface: Option<BinarySurface>,

    /// Open right-click context menu: the entry it targets and the
    /// logical-pixel point to anchor it at. The menu is a non-modal
    /// popover, so dismissal is handled explicitly in `on_event`.
    context_menu: Option<ContextMenu>,
    /// Entry whose Properties modal is open, if any.
    properties: Option<EntryId>,
    /// Entry whose "Open with…" app chooser is open, if any.
    open_with: Option<EntryId>,
    /// The desktop-app database for "Open with…", loaded off-thread at
    /// startup (`None` until the probe answers).
    app_db: Option<Arc<AppDb>>,
    /// Strings the app wants placed on the system clipboard ("copy
    /// path"). The clipboard lives in the host, not the app, so this is
    /// an outbox the host drains (`HostApp::drain_clipboard_writes`).
    clipboard_writes: Vec<String>,

    /// An open New Folder / Rename name-entry prompt (modal). While it's
    /// up `hotkeys()` returns empty so editing keys reach the field.
    prompt: Option<Prompt>,
    /// Entries awaiting a permanent-delete confirmation (modal). Holds
    /// the whole selection so the dialog can name the count.
    confirm_delete: Option<Vec<EntryId>>,
    /// Last file-operation failure, shown modally until dismissed.
    op_error: Option<String>,
}

/// An open name-entry prompt for a mutating op. The text buffer and its
/// caret/selection live here (the field is keyed `prompt-field`).
struct Prompt {
    kind: PromptKind,
    value: String,
    selection: Selection,
}

enum PromptKind {
    /// Create a folder in the current directory.
    NewFolder,
    /// Rename an existing entry; `original` is its current name (a
    /// no-op rename to the same name is dropped).
    Rename { target: EntryId, original: String },
}

impl PromptKind {
    fn title(&self) -> &'static str {
        match self {
            PromptKind::NewFolder => "New folder",
            PromptKind::Rename { .. } => "Rename",
        }
    }

    fn commit_label(&self) -> &'static str {
        match self {
            PromptKind::NewFolder => "Create",
            PromptKind::Rename { .. } => "Rename",
        }
    }
}

/// An open context menu (right-click). The browser window owns at most
/// one.
#[derive(Clone, Copy)]
struct ContextMenu {
    target: ContextTarget,
    point: (f32, f32),
}

/// What a context menu acts on: a specific entry (the row that was
/// right-clicked) or the directory background (empty space).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextTarget {
    Entry(EntryId),
    Background,
}

impl ExplorerApp {
    pub fn new(
        start: PathBuf,
        pool: Pool,
        notifier: Notifier,
        registry: Arc<Registry>,
        thumbs: Arc<ThumbCache>,
    ) -> Self {
        let (tx, rx) = channel();
        let mut app = ExplorerApp {
            listing: Listing::new(start.clone(), pool.generation()),
            cwd: start.clone(),
            pool,
            tx,
            rx,
            notifier,
            registry,
            places: Vec::new(),
            show_hidden: false,
            file_filter: None,
            search: String::new(),
            selection: Selection::default(),
            show_search: true,
            sort: SortMode::default(),
            sort_open: false,
            focus_requests: Vec::new(),
            view: ViewMode::List,
            file_activation: FileActivation::SystemOpen,
            activated: Vec::new(),
            thumbs,
            thumb_state: Arc::new(Mutex::new(ThumbState::new())),
            cols: Cell::new(1),
            selected: None,
            marked: HashSet::new(),
            anchor: None,
            pending_select: None,
            preview: PreviewState::Empty,
            preview_mode: PreviewMode::Normal,
            preview_inflight: None,
            preview_wanted: None,
            stat_requested: Arc::new(Mutex::new(HashSet::new())),
            scroll_requests: RefCell::new(Vec::new()),
            sidebar_w: 220.0,
            preview_w: 420.0,
            sidebar_drag: ResizeDrag::default(),
            preview_drag: ResizeDrag::default(),
            binary_surface: None,
            context_menu: None,
            properties: None,
            open_with: None,
            app_db: None,
            clipboard_writes: Vec::new(),
            prompt: None,
            confirm_delete: None,
            op_error: None,
        };
        app.spawn_places_probe();
        app.spawn_app_db_probe();
        app.navigate(start, None);
        app
    }

    fn browser_chrome(&self, viewport_w: f32) -> BrowserChrome {
        browser_chrome_widths(
            viewport_w,
            self.sidebar_w,
            self.preview_w,
            self.has_selected_file(),
        )
    }

    fn has_selected_file(&self) -> bool {
        let Some((id, _)) = self.selected else {
            return false;
        };
        let entries = self.listing.entries.lock().unwrap();
        entries.get(id as usize).is_some_and(|e| !e.is_dir())
    }

    /// One-shot at startup, on a detached thread rather than the pool:
    /// the constructor's initial `navigate` (and any quick follow-up
    /// navigation) bumps the pool generation, which cancels queued
    /// jobs wholesale — a pooled probe was reliably dropped before it
    /// ran, leaving the sidebar on "probing…" forever.
    /// Sender half of the app's message queue, for external services
    /// (D-Bus) that post commands to the UI thread.
    pub fn msg_sender(&self) -> Sender<Msg> {
        self.tx.clone()
    }

    fn spawn_places_probe(&self) {
        let tx = self.tx.clone();
        let notify = self.notifier.clone();
        let spawned = std::thread::Builder::new()
            .name("places-probe".into())
            .spawn(move || {
                let places = crate::places::probe();
                tracing::debug!(count = places.len(), "places probed");
                let _ = tx.send(Msg::Places(places));
                notify();
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "places probe thread failed to spawn");
        }
    }

    /// Load the desktop-app database for "Open with…" off-thread. Like
    /// the places probe it scans the local disk (XDG dirs), which is
    /// never the slow browsed filesystem, but the UI thread rule holds.
    fn spawn_app_db_probe(&self) {
        let tx = self.tx.clone();
        let notify = self.notifier.clone();
        let spawned = std::thread::Builder::new()
            .name("app-db-probe".into())
            .spawn(move || {
                let db = AppDb::load();
                let _ = tx.send(Msg::AppDb(Arc::new(db)));
                notify();
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "app-db probe thread failed to spawn");
        }
    }

    /// Leave for `dir`: invalidate all queued work, reset per-directory
    /// state, and start a streaming listing.
    fn navigate(&mut self, dir: PathBuf, select: Option<OsString>) {
        let generation = self.pool.bump_generation();
        tracing::info!(dir = %dir.display(), generation, "navigate");
        self.cwd = dir.clone();
        self.listing = Listing::new(dir.clone(), generation);
        self.selected = None;
        self.marked.clear();
        self.anchor = None;
        // Entry ids the menu/dialog targeted belong to the old listing.
        self.context_menu = None;
        self.properties = None;
        self.open_with = None;
        // A rename targets an entry id from this listing; a New Folder
        // prompt is anchored to this directory. Both are stale now. (A
        // pending op already in flight is unaffected — it owns absolute
        // paths and reports back regardless of where we navigate.)
        self.prompt = None;
        self.confirm_delete = None;
        self.pending_select = select;
        self.preview = PreviewState::Empty;
        self.preview_inflight = None;
        self.preview_wanted = None;
        self.stat_requested.lock().unwrap().clear();
        self.thumb_state.lock().unwrap().reset();
        self.scroll_requests.borrow_mut().push(ScrollRequest::new(
            self.scroll_key(),
            0,
            ScrollAlignment::Visible,
        ));

        let tx = self.tx.clone();
        let notify = self.notifier.clone();
        let pool = self.pool.clone();
        self.pool.submit(Tier::Urgent, move || {
            listing::read_dir_streaming(&dir, |update| {
                if pool.generation() != generation {
                    return false; // navigated away mid-listing
                }
                let alive = tx.send(Msg::Listing { generation, update }).is_ok();
                notify();
                alive
            });
        });
    }

    fn navigate_parent(&mut self) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return;
        };
        let from = self.cwd.file_name().map(OsString::from);
        self.navigate(parent, from);
    }

    /// Re-list the current directory, keeping the selection by name
    /// (its id will differ in the fresh listing).
    fn refresh(&mut self) {
        let keep = self.selected_id().and_then(|id| {
            let entries = self.listing.entries.lock().unwrap();
            entries.get(id as usize).map(|e| e.name.clone())
        });
        self.navigate(self.cwd.clone(), keep);
    }

    fn search_term(&self) -> Option<String> {
        let term = self.search.trim();
        (!term.is_empty()).then(|| term.to_string())
    }

    fn rebuild_visible_order(&mut self) {
        let search = self.search_term();
        self.listing.rebuild_order(
            self.show_hidden,
            self.file_filter.as_ref(),
            search.as_deref(),
            self.sort,
        );
        self.remap_selection();
    }

    /// Scroll container key + the index of the line holding `pos` in
    /// the active view (the grid packs `cols` entries per line).
    fn scroll_key(&self) -> &'static str {
        match self.view {
            ViewMode::List => "list",
            ViewMode::Grid => "grid",
        }
    }

    fn scroll_line_of(&self, pos: usize) -> usize {
        match self.view {
            ViewMode::List => pos,
            ViewMode::Grid => pos / self.cols.get().max(1),
        }
    }

    fn select_id(&mut self, id: EntryId) {
        let Some(pos) = self.listing.pos_of(id) else {
            return;
        };
        self.selected = Some((id, pos));
        self.scroll_requests.borrow_mut().push(ScrollRequest::new(
            self.scroll_key(),
            self.scroll_line_of(pos),
            ScrollAlignment::Visible,
        ));

        let entries = self.listing.entries.lock().unwrap();
        let kind = entries[id as usize].kind;
        drop(entries);
        if kind == EntryKind::Dir {
            self.preview = PreviewState::Empty;
            self.preview_wanted = None;
        } else {
            self.request_preview(id);
        }
    }

    /// Keyboard navigation (arrows, Home/End) is single-select: move
    /// the cursor and drop any marks. Multi-select is mouse/Space only.
    fn select_pos(&mut self, pos: usize) {
        if let Some(&id) = self.listing.order.get(pos) {
            self.select_only(id);
        }
    }

    /// Plain selection: cursor to `id`, clear marks, reset the anchor.
    fn select_only(&mut self, id: EntryId) {
        self.marked.clear();
        self.anchor = Some(id);
        self.select_id(id);
    }

    /// Ctrl+click / Space: toggle `id`'s mark; the cursor and anchor
    /// follow it.
    fn toggle_mark(&mut self, id: EntryId) {
        if !self.marked.insert(id) {
            self.marked.remove(&id);
        }
        self.anchor = Some(id);
        self.select_id(id);
    }

    /// Shift+click: mark the inclusive range from the anchor to `id`
    /// (replacing the set). No anchor yet falls back to single-select.
    fn range_mark(&mut self, id: EntryId) {
        match (
            self.anchor.and_then(|a| self.listing.pos_of(a)),
            self.listing.pos_of(id),
        ) {
            (Some(a), Some(b)) => {
                let (lo, hi) = (a.min(b), a.max(b));
                self.marked = self.listing.order[lo..=hi].iter().copied().collect();
                self.select_id(id);
            }
            _ => self.select_only(id),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.listing.order.len();
        if len == 0 {
            return;
        }
        let pos = match self.selected {
            Some((_, pos)) => (pos as isize + delta).clamp(0, len as isize - 1) as usize,
            None => {
                if delta >= 0 {
                    0
                } else {
                    len - 1
                }
            }
        };
        self.select_pos(pos);
    }

    /// Enter / double-click: descend into directories, hand files to
    /// the system opener.
    fn activate_id(&mut self, id: EntryId) {
        let entries = self.listing.entries.lock().unwrap();
        let Some(entry) = entries.get(id as usize) else {
            return;
        };
        let is_dir = entry.is_dir();
        let name = entry.name.clone();
        drop(entries);
        let path = self.cwd.join(&name);
        if is_dir {
            self.navigate(path, None);
        } else {
            match self.file_activation {
                // Detached; xdg-open does its own (possibly slow) IO
                // in its own process.
                FileActivation::SystemOpen => {
                    if let Err(e) = std::process::Command::new("xdg-open").arg(&path).spawn() {
                        tracing::warn!(path = %path.display(), error = %e, "xdg-open failed");
                    }
                }
                FileActivation::Collect => self.activated.push(path),
            }
        }
    }

    // ---- context menu ----------------------------------------------------

    /// Absolute path of the entry `id`, or `None` if it has aged out of
    /// the listing. Name-join only — no IO.
    fn entry_path(&self, id: EntryId) -> Option<PathBuf> {
        let entries = self.listing.entries.lock().unwrap();
        entries.get(id as usize).map(|e| self.cwd.join(&e.name))
    }

    /// Right-click opened a context menu for entry `id` at `point`.
    /// Right-clicking a row outside the current multi-selection collapses
    /// to it (file-manager convention) so the menu and selection agree;
    /// right-clicking one already in the selection keeps the whole set so
    /// a bulk action covers all of it.
    fn open_context_menu(&mut self, id: EntryId, point: (f32, f32)) {
        if !self.marked.contains(&id) {
            self.select_only(id);
        }
        self.context_menu = Some(ContextMenu {
            target: ContextTarget::Entry(id),
            point,
        });
    }

    /// Right-click landed on empty listing space — open the directory
    /// (background) menu at `point`.
    fn open_background_menu(&mut self, point: (f32, f32)) {
        self.context_menu = Some(ContextMenu {
            target: ContextTarget::Background,
            point,
        });
    }

    /// The entries a bulk action (trash / delete / copy-path) targets:
    /// the whole multi-selection when `anchor` is part of it, otherwise
    /// just `anchor`. Returned in listing order.
    fn bulk_targets(&self, anchor: EntryId) -> Vec<EntryId> {
        if self.marked.contains(&anchor) {
            self.listing
                .order
                .iter()
                .copied()
                .filter(|id| self.marked.contains(id))
                .collect()
        } else {
            vec![anchor]
        }
    }

    /// Absolute paths of `ids` (dirs included), skipping any that have
    /// aged out of the listing.
    fn entry_paths(&self, ids: &[EntryId]) -> Vec<PathBuf> {
        let entries = self.listing.entries.lock().unwrap();
        ids.iter()
            .filter_map(|&id| entries.get(id as usize).map(|e| self.cwd.join(&e.name)))
            .collect()
    }

    /// Queue the entries' absolute paths for the system clipboard, one
    /// per line. The host drains `clipboard_writes` into its `arboard`
    /// clipboard (taking the last entry, so a multi-selection must be a
    /// single newline-joined string).
    fn copy_paths(&mut self, ids: Vec<EntryId>) {
        let paths = self.entry_paths(&ids);
        if !paths.is_empty() {
            let joined = paths
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>()
                .join("\n");
            self.clipboard_writes.push(joined);
        }
    }

    /// Dispatch a `ctx:*` menu item to its action.
    fn run_context_action(&mut self, action: &str, target: ContextTarget) {
        match target {
            ContextTarget::Entry(id) => self.run_entry_action(action, id),
            ContextTarget::Background => self.run_background_action(action),
        }
    }

    /// Dispatch an entry-menu item. Destructive and copy actions span the
    /// whole multi-selection (`bulk_targets`); open / rename / properties
    /// stay on the right-clicked entry.
    fn run_entry_action(&mut self, action: &str, id: EntryId) {
        match action {
            "open" => self.activate_id(id),
            "open-with" => self.open_with = Some(id),
            "copy-path" => self.copy_paths(self.bulk_targets(id)),
            "terminal" => self.open_terminal(id),
            "rename" => self.begin_rename(id),
            "trash" => {
                let targets = self.bulk_targets(id);
                self.trash_entries(targets);
            }
            "delete" => self.confirm_delete = Some(self.bulk_targets(id)),
            "properties" => self.properties = Some(id),
            _ => {}
        }
    }

    /// Dispatch a background-menu item (acts on the current directory).
    fn run_background_action(&mut self, action: &str) {
        match action {
            "new-folder" => self.begin_new_folder(),
            "terminal" => spawn_terminal(&self.cwd),
            _ => {}
        }
    }

    /// Spawn a terminal emulator rooted at the entry (if it's a
    /// directory) or at the current directory otherwise. Detached; the
    /// terminal does its own IO in its own process.
    fn open_terminal(&mut self, id: EntryId) {
        let dir = {
            let entries = self.listing.entries.lock().unwrap();
            match entries.get(id as usize) {
                Some(e) if e.is_dir() => self.cwd.join(&e.name),
                _ => self.cwd.clone(),
            }
        };
        spawn_terminal(&dir);
    }

    // ---- file operations -------------------------------------------------
    //
    // Mutating ops run on a detached thread (NOT the pool — navigation
    // bumps the pool generation and would cancel them) and report back
    // as `Msg::OpDone`, handled in `before_build`.

    /// Run `op` off the UI thread and post its outcome back.
    fn spawn_op(&self, op: crate::ops::FileOp) {
        let tx = self.tx.clone();
        let notify = self.notifier.clone();
        let spawned = std::thread::Builder::new()
            .name("file-op".into())
            .spawn(move || {
                let outcome = op.run();
                let _ = tx.send(Msg::OpDone(outcome));
                notify();
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "file-op thread failed to spawn");
        }
    }

    /// Open the New Folder prompt for the current directory.
    fn begin_new_folder(&mut self) {
        self.prompt = Some(Prompt {
            kind: PromptKind::NewFolder,
            value: String::new(),
            selection: Selection::default(),
        });
        self.focus_requests.push("prompt-field".into());
    }

    /// Open the Rename prompt for entry `id`, pre-filled with its name
    /// and the name pre-selected so typing replaces it.
    fn begin_rename(&mut self, id: EntryId) {
        let Some(name) = ({
            let entries = self.listing.entries.lock().unwrap();
            entries.get(id as usize).map(|e| e.display.clone())
        }) else {
            return;
        };
        let selection = Selection {
            range: Some(SelectionRange {
                anchor: SelectionPoint::new("prompt-field", 0),
                head: SelectionPoint::new("prompt-field", name.len()),
            }),
        };
        self.prompt = Some(Prompt {
            kind: PromptKind::Rename {
                target: id,
                original: name.clone(),
            },
            value: name,
            selection,
        });
        self.focus_requests.push("prompt-field".into());
    }

    /// Commit the open prompt. An invalid name keeps the prompt open
    /// (the user can fix it); a valid one spawns the op and closes. A
    /// rename to the unchanged name just closes — nothing to do.
    fn commit_prompt(&mut self) {
        let Some(prompt) = self.prompt.as_ref() else {
            return;
        };
        let name = prompt.value.trim().to_string();
        if !crate::ops::valid_name(&name) {
            return;
        }
        // Capture the op while borrowing the prompt, then close and spawn.
        let op = match &prompt.kind {
            PromptKind::NewFolder => Some(crate::ops::FileOp::NewFolder {
                parent: self.cwd.clone(),
                name,
            }),
            PromptKind::Rename { target, original } => {
                if name == *original {
                    None
                } else {
                    self.entry_path(*target)
                        .map(|from| crate::ops::FileOp::Rename {
                            from,
                            to: self.cwd.join(&name),
                        })
                }
            }
        };
        self.prompt = None;
        if let Some(op) = op {
            self.spawn_op(op);
        }
    }

    /// Move `ids` to the trash (recoverable) in one op.
    fn trash_entries(&mut self, ids: Vec<EntryId>) {
        let paths = self.entry_paths(&ids);
        if !paths.is_empty() {
            self.spawn_op(crate::ops::FileOp::Trash { paths });
        }
    }

    /// Permanently delete the entries the confirm dialog targets.
    fn confirm_delete_now(&mut self) {
        if let Some(ids) = self.confirm_delete.take() {
            let paths = self.entry_paths(&ids);
            if !paths.is_empty() {
                self.spawn_op(crate::ops::FileOp::DeletePermanent { paths });
            }
        }
    }

    /// Fold a finished op back into the UI: surface a failure, or refresh
    /// the affected directory (when we're still in it) and focus the new
    /// or renamed entry by name. A re-list is the simple, correct choice;
    /// optimistic in-place row edits are a future refinement.
    fn apply_op_outcome(&mut self, outcome: crate::ops::OpOutcome) {
        if let Some(error) = outcome.error {
            self.op_error = Some(error);
        } else if outcome.dir == self.cwd {
            self.navigate(self.cwd.clone(), outcome.select);
        }
    }

    // ---- picker-wrapper surface ------------------------------------------
    //
    // The portal picker composes this app for everything browsing —
    // these are the hooks its chrome needs.

    /// Route file activation into the outbox instead of `xdg-open`.
    pub(crate) fn set_file_activation(&mut self, mode: FileActivation) {
        self.file_activation = mode;
    }

    /// Files activated since the last drain (under
    /// [`FileActivation::Collect`]).
    pub(crate) fn take_activated(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.activated)
    }

    pub(crate) fn cwd_path(&self) -> &Path {
        &self.cwd
    }

    /// Best-effort kind of an entry named `name` in the current
    /// directory — the save picker uses it to confirm before
    /// overwriting an existing file (no extra stat; reads the listing).
    pub(crate) fn existing_kind(&self, name: &std::ffi::OsStr) -> Option<EntryKind> {
        self.listing.kind_of_name(name)
    }

    /// The selected entry's absolute path and whether it is a
    /// directory (through the symlink, once stat'ed).
    pub(crate) fn selected_entry_path(&self) -> Option<(PathBuf, bool)> {
        let id = self.selected_id()?;
        let entries = self.listing.entries.lock().unwrap();
        let e = entries.get(id as usize)?;
        Some((self.cwd.join(&e.name), e.is_dir()))
    }

    fn request_preview(&mut self, id: EntryId) {
        if self.preview_inflight.is_some() {
            self.preview_wanted = Some(id);
            return;
        }
        self.preview_inflight = Some(id);
        self.preview = PreviewState::Loading { id };
        let path = self.listing.path_of(id);
        let generation = self.listing.generation;
        let tx = self.tx.clone();
        let notify = self.notifier.clone();
        let registry = self.registry.clone();
        self.pool.submit(Tier::Urgent, move || {
            let result = PreviewPayload {
                preview: registry.load(&path).map_err(|e| format!("{e:#}")),
                raw: explorer_previews::raw_preview(&path).map_err(|e| format!("{e:#}")),
            };
            let _ = tx.send(Msg::Preview {
                generation,
                id,
                result,
            });
            notify();
        });
    }

    fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.rebuild_visible_order();
    }

    fn set_sort_mode(&mut self, sort: SortMode) {
        if self.sort == sort {
            return;
        }
        self.sort = sort;
        self.rebuild_visible_order();
        self.keep_selection_visible();
        self.request_missing_stats_for_sort();
    }

    fn keep_selection_visible(&self) {
        if let Some((_, pos)) = self.selected {
            self.scroll_requests.borrow_mut().push(ScrollRequest::new(
                self.scroll_key(),
                self.scroll_line_of(pos),
                ScrollAlignment::Visible,
            ));
        }
    }

    fn request_missing_stats_for_sort(&mut self) {
        if !self.sort.uses_meta() {
            return;
        }
        let generation = self.listing.generation;
        let jobs = {
            let entries = self.listing.entries.lock().unwrap();
            let mut requested = self.stat_requested.lock().unwrap();
            let mut jobs = Vec::new();
            for &id in self.listing.order.iter() {
                if jobs.len() >= SORT_STAT_BATCH {
                    break;
                }
                let Some(e) = entries.get(id as usize) else {
                    continue;
                };
                if e.meta.is_some() || e.meta_error.is_some() || !requested.insert(id) {
                    continue;
                }
                jobs.push((id, self.cwd.join(&e.name)));
            }
            jobs
        };
        for (id, path) in jobs {
            submit_stat_job(
                id,
                path,
                generation,
                Tier::Sweep,
                &self.pool,
                &self.tx,
                &self.notifier,
            );
        }
    }

    /// Visible (ordered) entry names — what a row-by-row reading of
    /// the list view would show.
    #[cfg(test)]
    pub(crate) fn visible_names(&self) -> Vec<String> {
        let entries = self.listing.entries.lock().unwrap();
        self.listing
            .order
            .iter()
            .map(|&id| entries[id as usize].display.clone())
            .collect()
    }

    /// Install (or clear) the picker's file-type filter and refilter
    /// the current listing in place.
    pub(crate) fn set_file_filter(&mut self, filter: Option<FileFilter>) {
        self.file_filter = filter;
        self.rebuild_visible_order();
    }

    pub(crate) fn set_search_visible(&mut self, visible: bool) {
        self.show_search = visible;
        if !visible && !self.search.is_empty() {
            self.search.clear();
            self.selection = Selection::default();
            self.rebuild_visible_order();
        }
    }

    fn set_search(&mut self, search: String) {
        if self.search == search {
            return;
        }
        self.search = search;
        self.rebuild_visible_order();
    }

    fn focus_search(&mut self) {
        if !self.show_search {
            return;
        }
        self.selection.range = Some(SelectionRange {
            anchor: SelectionPoint::new("browser-search", 0),
            head: SelectionPoint::new("browser-search", self.search.len()),
        });
        self.focus_requests.push("browser-search".into());
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            ViewMode::List => ViewMode::Grid,
            ViewMode::Grid => ViewMode::List,
        };
        // Keep the selection on screen in the other view. On the first
        // ever switch to grid, `cols` is still the default — the target
        // self-corrects on the next selection move.
        if let Some((_, pos)) = self.selected {
            self.scroll_requests.borrow_mut().push(ScrollRequest::new(
                self.scroll_key(),
                self.scroll_line_of(pos),
                ScrollAlignment::Visible,
            ));
        }
    }

    /// Re-derive the selection's position after `order` changed; drop
    /// the cursor and any marks whose entries were filtered out.
    fn remap_selection(&mut self) {
        if let Some((id, _)) = self.selected {
            self.selected = self.listing.pos_of(id).map(|pos| (id, pos));
            if self.selected.is_none() {
                self.preview = PreviewState::Empty;
                self.preview_wanted = None;
            }
        }
        if !self.marked.is_empty() {
            self.marked.retain(|&id| self.listing.pos_of(id).is_some());
        }
    }

    fn selected_id(&self) -> Option<EntryId> {
        self.selected.map(|(id, _)| id)
    }

    /// Absolute paths of the marked files (non-dirs), in listing order.
    /// What a `multiple` picker returns; empty when nothing is marked.
    pub(crate) fn marked_file_paths(&self) -> Vec<PathBuf> {
        let entries = self.listing.entries.lock().unwrap();
        self.listing
            .order
            .iter()
            .filter(|id| self.marked.contains(id))
            .filter_map(|&id| {
                let e = entries.get(id as usize)?;
                (!e.is_dir()).then(|| self.cwd.join(&e.name))
            })
            .collect()
    }

    /// Breadcrumb segments for the current directory: (label, path)
    /// per component, root included.
    fn crumbs(&self) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        let mut acc = PathBuf::new();
        for comp in self.cwd.components() {
            acc.push(comp);
            let label = match comp {
                std::path::Component::RootDir => "/".to_string(),
                other => other.as_os_str().to_string_lossy().into_owned(),
            };
            out.push((label, acc.clone()));
        }
        out
    }

    // ---- build helpers -------------------------------------------------

    fn sidebar_el(&self, sidebar_w: f32) -> El {
        let buttons: Vec<El> = self
            .places
            .iter()
            .enumerate()
            .map(|(i, p)| {
                sidebar_menu_item(
                    sidebar_menu_button_with_icon(p.icon, p.label.clone(), p.path == self.cwd)
                        .key(format!("place:{i}"))
                        .tooltip(p.path.display().to_string()),
                )
            })
            .collect();
        let menu: El = if buttons.is_empty() {
            text("probing…").caption().muted()
        } else {
            sidebar_menu(buttons)
        };
        sidebar([sidebar_group([sidebar_group_label("Places"), menu])])
            .width(Size::Fixed(sidebar_w))
            .height(Size::Fill(1.0))
    }

    fn toolbar_el(&self, cx: &BuildCx) -> El {
        let crumbs = self.crumbs();
        let last = crumbs.len().saturating_sub(1);
        let mut items = Vec::new();
        for (i, (label, path)) in crumbs.into_iter().enumerate() {
            if i == last {
                items.push(breadcrumb_item(breadcrumb_page(label)));
            } else {
                items.push(breadcrumb_item(
                    breadcrumb_link(format!("crumb:{i}"), label)
                        .tooltip(path.display().to_string()),
                ));
                items.push(breadcrumb_separator());
            }
        }

        let (view_icon, view_tip) = match self.view {
            ViewMode::List => ("layout-dashboard", "grid view (g)"),
            ViewMode::Grid => ("menu", "list view (g)"),
        };
        // Half-width browsers (tiling WMs) crowd the toolbar; shrink the
        // search box there so the action buttons still fit on one row.
        let narrow = cx.viewport_width().unwrap_or(1280.0) < 900.0;
        let mut tools = vec![
            icon_button("chevron-up")
                .key("up")
                .tooltip("parent directory (Backspace)"),
            breadcrumb([breadcrumb_list(items)]),
            spacer(),
        ];
        if self.show_search {
            tools.push(self.search_el(narrow));
        }
        tools.extend([
            icon_button("plus")
                .key("new-folder")
                .tooltip("new folder (Ctrl+Shift+N)"),
            select_trigger("browser-sort", self.sort.label())
                .width(Size::Fixed(128.0))
                .tooltip("sort order"),
            icon_button(view_icon).key("view-toggle").tooltip(view_tip),
            color_mode_badge(cx),
        ]);
        toolbar(tools)
    }

    fn search_el(&self, narrow: bool) -> El {
        let (input_w, row_w) = if narrow {
            (160.0, 224.0)
        } else {
            (220.0, 284.0)
        };
        let input = text_input::text_input_with(
            "browser-search",
            &self.search,
            &self.selection,
            TextInputOpts {
                placeholder: Some("Search names"),
                ..TextInputOpts::default()
            },
        )
        .width(Size::Fixed(input_w));
        let mut children = vec![
            icon("search")
                .icon_size(tokens::ICON_SM)
                .color(tokens::MUTED_FOREGROUND),
            input,
        ];
        if !self.search.is_empty() {
            children.push(icon_button("x").key("search-clear").tooltip("clear search"));
        }
        row(children)
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fixed(row_w))
    }

    /// Error / still-empty states shared by both views; `None` once
    /// there are entries to show.
    fn listing_placeholder(&self) -> Option<El> {
        if let Some(err) = &self.listing.error {
            return Some(
                column([
                    icon("alert-circle"),
                    text(err.clone()).muted().wrap_text().width(Size::Fill(1.0)),
                ])
                .gap(tokens::SPACE_3)
                .align(Align::Center)
                .justify(Justify::Center)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            );
        }
        if self.listing.order.is_empty() {
            let entry_count = self.listing.entries.lock().unwrap().len();
            let filtered = entry_count > 0
                && (self.file_filter.is_some()
                    || self.search_term().is_some()
                    || !self.show_hidden);
            let label: El = if self.listing.complete {
                if filtered {
                    text("no matches").muted()
                } else {
                    text("empty directory").muted()
                }
            } else {
                row([spinner(), text("listing…").muted()])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center)
            };
            return Some(
                column([label])
                    .align(Align::Center)
                    .justify(Justify::Center)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0)),
            );
        }
        None
    }

    fn list_el(&self) -> El {
        if let Some(placeholder) = self.listing_placeholder() {
            return placeholder;
        }

        // Snapshots/handles for the 'static row builder.
        let entries = self.listing.entries.clone();
        let order = self.listing.order.clone();
        let dir = self.listing.dir.clone();
        let generation = self.listing.generation;
        let pool = self.pool.clone();
        let tx = self.tx.clone();
        let notify = self.notifier.clone();
        let stat_requested = self.stat_requested.clone();
        let selected_id = self.selected_id();
        let marked = self.marked.clone();

        virtual_list(order.len(), ROW_H, move |i| {
            let id = order[i];
            let entries = entries.lock().unwrap();
            let e = &entries[id as usize];

            maybe_request_stat(
                e,
                id,
                &dir,
                generation,
                &pool,
                &tx,
                &notify,
                &stat_requested,
            );

            // Marked rows swap their kind icon for a check accent (no
            // layout shift), and highlight like the cursor.
            let is_marked = marked.contains(&id);
            let lead_icon = if is_marked {
                icon("check")
                    .icon_size(tokens::ICON_SM)
                    .color(tokens::PRIMARY)
            } else {
                icon(entry_icon(e))
                    .icon_size(tokens::ICON_SM)
                    .color(tokens::MUTED_FOREGROUND)
            };
            let size = match &e.meta {
                Some(m) if m.kind == EntryKind::File => fmt::human_bytes(m.size),
                _ => String::new(),
            };
            let date = e
                .meta
                .as_ref()
                .and_then(|m| m.modified)
                .map(fmt::mtime)
                .unwrap_or_default();
            let mut name = text(e.display.clone()).ellipsis().width(Size::Fill(1.0));
            if e.is_symlink {
                name = name.italic();
            }
            if e.meta_error.is_some() {
                name = name.muted();
            }

            let r = row([
                lead_icon,
                name,
                text(size).caption().muted().width(Size::Fixed(76.0)),
                text(date).caption().muted().width(Size::Fixed(118.0)),
            ])
            .gap(tokens::SPACE_3)
            .padding(Sides::xy(tokens::SPACE_3, 0.0))
            .align(Align::Center)
            .height(Size::Fixed(ROW_H))
            .radius(tokens::RADIUS_SM)
            .clip()
            .key(format!("row:{id}"))
            .focusable()
            .tooltip(entry_tooltip(e));
            if Some(id) == selected_id || is_marked {
                r.current()
            } else {
                r.ghost()
            }
        })
        // The gap keeps row focus rings from being occluded by the
        // next row; the call site pads the list inside its card (which
        // also keeps the rings inside the scroll scissor).
        .gap(tokens::RING_WIDTH)
        .key("list")
    }

    /// Grid of thumbnail tiles, virtualized by row (the gallery's
    /// pattern). Image cells pull from the thumbnail cache — RAM LRU
    /// first, then a worker job that hits disk or decodes; everything
    /// else shows its kind icon.
    fn grid_el(&self, cx: &BuildCx, chrome: BrowserChrome) -> El {
        if let Some(placeholder) = self.listing_placeholder() {
            return placeholder;
        }

        // Width available to tiles: center pane width minus slack for
        // card padding, strokes, resize handles, and the scrollbar
        // gutter. Erring low costs at most one column; erring high
        // overflows the row.
        let vw = cx.viewport_width().unwrap_or(1280.0);
        let avail = listing_available_w(vw, chrome) - 24.0;
        let cols = (((avail + TILE_GAP) / (TILE_W + TILE_GAP)) as usize).max(1);
        self.cols.set(cols);
        let count = self.listing.order.len();
        let rows = count.div_ceil(cols);

        // Snapshots/handles for the 'static cell builder.
        let entries = self.listing.entries.clone();
        let order = self.listing.order.clone();
        let dir = self.listing.dir.clone();
        let generation = self.listing.generation;
        let pool = self.pool.clone();
        let tx = self.tx.clone();
        let notify = self.notifier.clone();
        let stat_requested = self.stat_requested.clone();
        let thumb_state = self.thumb_state.clone();
        let thumbs = self.thumbs.clone();
        let selected_id = self.selected_id();
        let marked = self.marked.clone();

        virtual_list(rows, TILE_H + TILE_GAP, move |r| {
            let mut cells = Vec::with_capacity(cols);
            for col in 0..cols {
                let i = r * cols + col;
                if i >= count {
                    cells.push(spacer().width(Size::Fixed(TILE_W)));
                    continue;
                }
                let id = order[i];
                let entries = entries.lock().unwrap();
                let e = &entries[id as usize];

                maybe_request_stat(
                    e,
                    id,
                    &dir,
                    generation,
                    &pool,
                    &tx,
                    &notify,
                    &stat_requested,
                );

                let name = e.display.clone();
                let icon_name = entry_icon(e);
                let preview_label = e.preview_kind.label();
                let thumb_policy =
                    grid_thumbnail_policy(e.preview_kind, e.kind, e.meta, e.meta_error.is_some());
                let path = matches!(
                    thumb_policy,
                    GridThumbPolicy::Decode | GridThumbPolicy::CacheOnly
                )
                .then(|| dir.join(&e.name));
                drop(entries);

                let media: El = if let Some(path) = path {
                    let mut ts = thumb_state.lock().unwrap();
                    if let Some(img) = ts.ram.get(&id) {
                        image(img.clone())
                            .image_fit(ImageFit::Cover)
                            // A wall of 1000-nit tiles would be hostile;
                            // cap grid brights at 2× reference (the
                            // preview pane shows full headroom).
                            .dynamic_range_limit(DynamicRangeLimit::ConstrainedHigh)
                            .radius(tokens::RADIUS_SM)
                            .width(Size::Fill(1.0))
                            .height(Size::Fixed(TILE_MEDIA_H))
                    } else if ts.failed.contains(&id) {
                        tile_icon(icon_name)
                    } else {
                        // Realized cell without a thumb: queue one, once.
                        if ts.requested.insert(id) {
                            let tx = tx.clone();
                            let notify = notify.clone();
                            let thumbs = thumbs.clone();
                            pool.submit(Tier::Visible, move || {
                                let result = match thumb_policy {
                                    GridThumbPolicy::Decode => match thumbs.thumbnail(&path) {
                                        Ok(image) => ThumbResult::Image(image),
                                        Err(error) => ThumbResult::Error(format!("{error:#}")),
                                    },
                                    GridThumbPolicy::CacheOnly => {
                                        match thumbs.cached_thumbnail(&path) {
                                            Ok(Some(image)) => ThumbResult::Image(image),
                                            Ok(None) => ThumbResult::Miss,
                                            Err(error) => ThumbResult::Error(format!("{error:#}")),
                                        }
                                    }
                                    GridThumbPolicy::Never | GridThumbPolicy::WaitForMeta => {
                                        ThumbResult::Miss
                                    }
                                };
                                let _ = tx.send(Msg::Thumb {
                                    generation,
                                    id,
                                    result,
                                });
                                notify();
                            });
                        }
                        skeleton()
                            .radius(tokens::RADIUS_SM)
                            .width(Size::Fill(1.0))
                            .height(Size::Fixed(TILE_MEDIA_H))
                    }
                } else {
                    tile_icon(icon_name)
                };

                // Marked tiles get a check accent on the caption and
                // highlight like the cursor.
                let is_marked = marked.contains(&id);
                let caption: El = if is_marked {
                    row([
                        icon("check")
                            .icon_size(tokens::ICON_SM)
                            .color(tokens::PRIMARY),
                        text(name.clone()).caption(),
                    ])
                    .gap(tokens::SPACE_1)
                    .align(Align::Center)
                } else {
                    text(name.clone()).caption()
                };
                let cell = column([media, caption])
                    .gap(tokens::SPACE_1)
                    .align(Align::Center)
                    .width(Size::Fixed(TILE_W))
                    .height(Size::Fixed(TILE_H))
                    .radius(tokens::RADIUS_SM)
                    .clip()
                    .key(format!("row:{id}"))
                    .focusable()
                    .tooltip(entry_tooltip_by_parts(&name, preview_label));
                cells.push(if Some(id) == selected_id || is_marked {
                    cell.current()
                } else {
                    cell.ghost()
                });
            }
            row(cells).gap(TILE_GAP)
        })
        .key("grid")
    }

    fn preview_pane(&self, preview_w: f32) -> El {
        let Some((id, _)) = self.selected else {
            return preview_placeholder("file", "select a file to preview");
        };
        let entries = self.listing.entries.lock().unwrap();
        let Some(e) = entries.get(id as usize) else {
            return preview_placeholder("file", "select a file to preview");
        };

        let mut header_meta = Vec::new();
        if let Some(m) = &e.meta {
            if m.kind == EntryKind::File {
                header_meta.push(fmt::human_bytes(m.size));
            }
            if let Some(t) = m.modified {
                header_meta.push(fmt::mtime(t));
            }
        }
        if e.is_symlink {
            header_meta.push("symlink".into());
        }
        let header = column([
            text(e.display.clone()).bold(),
            text(header_meta.join(" · ")).caption().muted(),
        ])
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0));

        let is_dir = e.is_dir();
        drop(entries);

        // Belt and braces: never render a preview that belongs to a
        // different entry than the selection (e.g. one frame of skew
        // between a selection change and the next preview request).
        let state_id = match &self.preview {
            PreviewState::Empty => None,
            PreviewState::Loading { id }
            | PreviewState::Ready { id, .. }
            | PreviewState::Failed { id, .. } => Some(*id),
        };
        let stale = state_id.is_some_and(|s| s != id);

        let body: El = if is_dir {
            preview_placeholder_body("folder", "directory")
        } else if stale {
            self.preview_loading_body(id)
        } else {
            match &self.preview {
                PreviewState::Empty => preview_placeholder_body("file", ""),
                PreviewState::Loading { .. } => self.preview_loading_body(id),
                PreviewState::Failed { error, .. } => {
                    preview_placeholder_body("alert-circle", error)
                }
                PreviewState::Ready { preview, raw, .. } => {
                    let mode = effective_preview_mode(
                        self.preview_mode,
                        preview.as_ref().ok(),
                        raw.as_ref(),
                    );
                    self.preview_body(preview, raw.as_ref(), mode, preview_w)
                }
            }
        };

        let mut children = vec![header];
        if !is_dir && !stale {
            if let PreviewState::Ready { preview, raw, .. } = &self.preview {
                let modes = preview_modes(preview.as_ref().ok(), raw.as_ref());
                if modes.len() > 1 {
                    let current = effective_preview_mode(
                        self.preview_mode,
                        preview.as_ref().ok(),
                        raw.as_ref(),
                    );
                    children
                        .push(tabs_list("preview-mode", &current, modes).width(Size::Fill(1.0)));
                }
            }
        }
        children.push(body);

        card([column(children)
            .gap(tokens::SPACE_3)
            .padding(tokens::SPACE_4)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))])
        .width(Size::Fixed(preview_w))
        .height(Size::Fill(1.0))
    }

    fn preview_body(
        &self,
        preview: &Result<Preview, String>,
        raw: Option<&RawPreview>,
        mode: PreviewMode,
        preview_w: f32,
    ) -> El {
        match mode {
            PreviewMode::Normal => match preview {
                Ok(preview) => {
                    let surface = self
                        .binary_surface
                        .as_ref()
                        .map(|s| (s.app_texture(), s.metrics()));
                    normal_preview_body(preview, preview_w, surface)
                }
                Err(error) => preview_placeholder_body("alert-circle", error),
            },
            PreviewMode::Text => match raw.and_then(|raw| raw.text.as_ref()) {
                Some(text) => {
                    text_preview_body(text.clone(), raw.is_some_and(|r| r.binary.truncated))
                }
                None => match preview {
                    Ok(Preview::Text { text, truncated }) => {
                        text_preview_body(text.clone(), *truncated)
                    }
                    // The raw plain text is normally present; this is the
                    // fallback (e.g. fixtures with no RawPreview).
                    Ok(Preview::Code { lines, truncated }) => {
                        text_preview_body(code_plain_text(lines), *truncated)
                    }
                    _ => preview_placeholder_body("file-text", "text view unavailable"),
                },
            },
            PreviewMode::Binary => {
                let binary = raw.map(|raw| &raw.binary).or(match preview {
                    Ok(Preview::Binary(binary)) => Some(binary),
                    _ => None,
                });
                match binary {
                    Some(binary) => {
                        let surface = self
                            .binary_surface
                            .as_ref()
                            .map(|s| (s.app_texture(), s.metrics()));
                        binary_preview_body(binary, preview_w, surface)
                    }
                    None => preview_placeholder_body("file", "binary view unavailable"),
                }
            }
        }
    }

    /// Preview body while the full decode is in flight: the grid's
    /// cached thumbnail when RAM has one (instant feedback — on a slow
    /// mount the real decode can take seconds), a bare spinner
    /// otherwise. NoLimit matches the full image so brightness doesn't
    /// pop when it swaps in.
    fn preview_loading_body(&self, id: EntryId) -> El {
        let thumb = self.thumb_state.lock().unwrap().ram.get(&id).cloned();
        match thumb {
            Some(img) => column([
                image(img)
                    .image_fit(ImageFit::Contain)
                    .dynamic_range_limit(DynamicRangeLimit::NoLimit)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0)),
                row([spinner(), text("decoding…").caption().muted()])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center),
            ])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0)),
            None => column([spinner()])
                .align(Align::Center)
                .justify(Justify::Center)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
        }
    }

    /// Toolbar + sidebar/listing/preview + status bar, the whole
    /// browsing page minus the window scaffold — the picker stacks its
    /// chrome under this.
    pub(crate) fn page_el(&self, cx: &BuildCx) -> El {
        let viewport_w = cx.viewport_width().unwrap_or(1280.0);
        let chrome = self.browser_chrome(viewport_w);
        let center = match self.view {
            ViewMode::List => self.list_el(),
            ViewMode::Grid => self.grid_el(cx, chrome),
        };
        let mut panes = Vec::new();
        if let Some(sidebar_w) = chrome.sidebar_w {
            panes.push(self.sidebar_el(sidebar_w));
            panes.push(resize_handle("sidebar-resize", Axis::Row));
        }
        // Keyed so a right-click on empty listing space (below the rows,
        // or in an empty directory) routes here for the background menu;
        // the rows inside claim their own hits first.
        panes.push(
            card([center.padding(tokens::SPACE_2)])
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0))
                .key("listing-area"),
        );
        if let Some(preview_w) = chrome.preview_w {
            panes.push(resize_handle("preview-resize", Axis::Row));
            panes.push(self.preview_pane(preview_w));
        }
        let content = row(panes)
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));

        let page = column([self.toolbar_el(cx), content, self.status_el()])
            .gap(tokens::SPACE_3)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));
        // Overlays stack newest-on-top: sort menu, then the right-click
        // context menu, then the modal Properties dialog above both.
        let mut layers: Vec<El> = vec![page];
        if self.sort_open {
            layers.push(select_menu("browser-sort", sort_options()));
        }
        if let Some(menu) = self.context_menu {
            layers.push(self.context_menu_el(menu));
        }
        if let Some(id) = self.properties {
            if let Some(dialog) = self.properties_dialog(id) {
                layers.push(dialog);
            }
        }
        if let Some(id) = self.open_with {
            if let Some(dialog) = self.open_with_dialog(id) {
                layers.push(dialog);
            }
        }
        if let Some(prompt) = &self.prompt {
            layers.push(self.prompt_dialog(prompt));
        }
        if let Some(ids) = &self.confirm_delete {
            if let Some(dialog) = self.confirm_delete_dialog(ids) {
                layers.push(dialog);
            }
        }
        if let Some(message) = &self.op_error {
            layers.push(self.op_error_dialog(message));
        }
        if layers.len() == 1 {
            layers.pop().unwrap()
        } else {
            stack(layers).width(Size::Fill(1.0)).height(Size::Fill(1.0))
        }
    }

    /// The right-click menu popover for `menu`, anchored at the click
    /// point. Items are keyed `ctx:*` and routed in `on_event`.
    fn context_menu_el(&self, menu: ContextMenu) -> El {
        let items = match menu.target {
            ContextTarget::Entry(id) => self.entry_menu_items(id),
            ContextTarget::Background => self.background_menu_items(),
        };
        context_menu("context-menu", menu.point, items)
    }

    /// Items for an entry's right-click menu. Destructive / copy actions
    /// name the multi-selection count when one is active so the user can
    /// see the menu acts on all of it.
    fn entry_menu_items(&self, id: EntryId) -> Vec<El> {
        let is_dir = {
            let entries = self.listing.entries.lock().unwrap();
            entries.get(id as usize).is_some_and(|e| e.is_dir())
        };
        let n = self.bulk_targets(id).len();
        let mut items = vec![menu_item("Open").key("ctx:open")];
        // "Open with…" picks a non-default handler — only meaningful
        // for files (directories open in the browser itself).
        if !is_dir {
            items.push(menu_item("Open with…").key("ctx:open-with"));
        }
        let copy_label = if n > 1 {
            format!("Copy {n} paths")
        } else {
            "Copy path".to_string()
        };
        items.push(menu_item(copy_label).key("ctx:copy-path"));
        // "Open terminal here" roots at the entry when it's a directory,
        // otherwise at the current folder (handled in `open_terminal`).
        items.push(menu_item("Open terminal here").key("ctx:terminal"));
        items.push(menu_item("Rename…").key("ctx:rename"));
        let (trash_label, delete_label) = if n > 1 {
            (
                format!("Move {n} items to Trash"),
                format!("Delete {n} items permanently…"),
            )
        } else {
            (
                "Move to Trash".to_string(),
                "Delete permanently…".to_string(),
            )
        };
        items.push(menu_item(trash_label).key("ctx:trash"));
        items.push(menu_item(delete_label).key("ctx:delete"));
        items.push(menu_item("Properties").key("ctx:properties"));
        items
    }

    /// Items for the directory-background right-click menu (empty space).
    /// Paste joins here once copy/cut exists.
    fn background_menu_items(&self) -> Vec<El> {
        vec![
            menu_item("New folder").key("ctx:new-folder"),
            menu_item("Open terminal here").key("ctx:terminal"),
        ]
    }

    /// Modal details dialog for entry `id`. `None` if the entry has
    /// aged out of the listing.
    fn properties_dialog(&self, id: EntryId) -> Option<El> {
        let entries = self.listing.entries.lock().unwrap();
        let entry = entries.get(id as usize)?;
        let path = self.cwd.join(&entry.name);
        let mut rows = vec![
            property_row("Name", &entry.display),
            property_row("Where", &path.to_string_lossy()),
            property_row("Kind", entry.preview_kind.label()),
        ];
        match &entry.meta {
            Some(meta) => {
                if !entry.is_dir() {
                    rows.push(property_row("Size", &fmt::human_bytes(meta.size)));
                }
                if let Some(modified) = meta.modified {
                    rows.push(property_row("Modified", &fmt::mtime(modified)));
                }
            }
            None => rows.push(property_row("Size", "—")),
        }
        if entry.is_symlink {
            rows.push(property_row("Link", "symbolic link"));
        }
        drop(entries);
        Some(dialog(
            "props",
            [
                dialog_header([dialog_title("Properties")]),
                column(rows).gap(tokens::SPACE_2),
                dialog_footer([button("Close").primary().key("props:close")]),
            ],
        ))
    }

    /// Modal "Open with…" app chooser for entry `id`. App rows are
    /// keyed `openwith:{desktop-id}`; chrome is keyed `open-with:*`.
    /// `None` if the entry has aged out of the listing.
    fn open_with_dialog(&self, id: EntryId) -> Option<El> {
        let (display, mime) = {
            let entries = self.listing.entries.lock().unwrap();
            let entry = entries.get(id as usize)?;
            let mime = mime_guess::from_path(Path::new(&entry.name)).first_or_octet_stream();
            (entry.display.clone(), mime)
        };
        let body = match &self.app_db {
            // Probe still running.
            None => column([text("Loading applications…").caption().muted()]),
            Some(db) => {
                let candidates = db.candidates(mime.essence_str());
                if candidates.is_empty() {
                    column([text(format!(
                        "No applications registered for {}.",
                        mime.essence_str()
                    ))
                    .caption()
                    .muted()])
                } else {
                    let rows: Vec<El> = candidates
                        .iter()
                        .map(|c| {
                            let label = if c.is_default {
                                format!("{} — default", c.name)
                            } else {
                                c.name.clone()
                            };
                            button(label)
                                .ghost()
                                .key(format!("openwith:{}", c.id))
                                .width(Size::Fill(1.0))
                        })
                        .collect();
                    column(rows).gap(tokens::SPACE_1)
                }
            }
        };
        Some(dialog(
            "open-with",
            [
                dialog_header([dialog_title(format!("Open \u{201c}{display}\u{201d} with"))]),
                body.width(Size::Fill(1.0)),
                dialog_footer([button("Cancel").ghost().key("open-with:cancel")]),
            ],
        ))
    }

    /// Modal name-entry dialog for a New Folder / Rename prompt. The
    /// field is keyed `prompt-field`; chrome is keyed `prompt:*`.
    fn prompt_dialog(&self, prompt: &Prompt) -> El {
        let field = text_input::text_input_with(
            "prompt-field",
            &prompt.value,
            &prompt.selection,
            TextInputOpts {
                placeholder: Some("name"),
                ..TextInputOpts::default()
            },
        )
        .width(Size::Fill(1.0));
        let commit = button(prompt.kind.commit_label())
            .primary()
            .key("prompt:commit");
        let commit = if crate::ops::valid_name(prompt.value.trim()) {
            commit
        } else {
            commit.disabled()
        };
        dialog(
            "prompt",
            [
                dialog_header([dialog_title(prompt.kind.title())]),
                field,
                dialog_footer([button("Cancel").ghost().key("prompt:cancel"), commit]),
            ],
        )
    }

    /// Modal confirmation before a permanent (non-trash) delete. Names
    /// the single entry, or the count for a multi-selection. `None` if
    /// every target has aged out of the listing.
    fn confirm_delete_dialog(&self, ids: &[EntryId]) -> Option<El> {
        let lead = {
            let entries = self.listing.entries.lock().unwrap();
            match ids {
                [id] => format!(
                    "\u{201c}{}\u{201d} will be deleted permanently.",
                    entries.get(*id as usize)?.display
                ),
                _ => {
                    // Skip any that aged out; bail only if all are gone.
                    let live = ids
                        .iter()
                        .filter(|&&id| entries.get(id as usize).is_some())
                        .count();
                    if live == 0 {
                        return None;
                    }
                    format!("{live} items will be deleted permanently.")
                }
            }
        };
        Some(dialog(
            "confirm-delete",
            [
                dialog_header([dialog_title("Delete permanently?")]),
                column([
                    text(lead),
                    text("This cannot be undone — use Move to Trash to keep a copy.")
                        .caption()
                        .muted(),
                ])
                .gap(tokens::SPACE_2),
                dialog_footer([
                    button("Cancel").ghost().key("confirm-delete:cancel"),
                    button("Delete").destructive().key("confirm-delete:confirm"),
                ]),
            ],
        ))
    }

    /// Modal notice for a failed file operation.
    fn op_error_dialog(&self, message: &str) -> El {
        dialog(
            "op-error",
            [
                dialog_header([dialog_title("Operation failed")]),
                text(message.to_string()),
                dialog_footer([button("Close").primary().key("op-error:close")]),
            ],
        )
    }

    fn status_el(&self) -> El {
        let mut left = if let Some(search) = self.search_term() {
            format!("{} matches for \"{search}\"", self.listing.order.len())
        } else {
            format!("{} items", self.listing.order.len())
        };
        if !self.listing.complete && self.listing.error.is_none() {
            left.push_str(" · listing…");
        }
        if !self.show_hidden {
            left.push_str(" · hidden files off (.)");
        }
        let right = self
            .selected
            .and_then(|(id, _)| {
                let entries = self.listing.entries.lock().unwrap();
                entries.get(id as usize).map(|e| e.display.clone())
            })
            .unwrap_or_default();
        row([
            text(left).caption().muted(),
            spacer(),
            text(right).caption().muted(),
        ])
        .width(Size::Fill(1.0))
    }
}

/// Page scaffold (the hero-fixture idiom, as in the gallery): themed
/// background under padded content; overlay root on top.
pub(crate) fn scaffold(page: El) -> El {
    overlays(
        stack([
            column(Vec::<El>::new())
                .fill(tokens::BACKGROUND)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            page.padding(tokens::SPACE_4)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
        ])
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0)),
        [],
    )
}

fn entry_icon(entry: &Entry) -> &'static str {
    match entry.kind {
        EntryKind::Dir => "folder",
        EntryKind::File | EntryKind::Symlink => entry.preview_kind.icon(),
        EntryKind::Other => "more-horizontal",
    }
}

fn entry_tooltip(entry: &Entry) -> String {
    entry_tooltip_by_parts(&entry.display, entry.preview_kind.label())
}

fn entry_tooltip_by_parts(name: &str, kind: &str) -> String {
    format!("{name} · {kind}")
}

/// One label/value row in the Properties dialog. The label is a fixed
/// muted column; the value wraps/fills the rest.
fn property_row(label: &str, value: &str) -> El {
    row([
        text(label).caption().muted().width(Size::Fixed(72.0)),
        text(value).caption(),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
}

/// Launch a terminal emulator with its working directory set to `dir`.
/// Honors `$TERMINAL`, then falls back through common emulators; the
/// first one that launches wins. Detached — like `xdg-open`, the child
/// runs in its own process. A missing binary is skipped silently; any
/// other spawn error stops the search and is logged.
fn spawn_terminal(dir: &Path) {
    const FALLBACKS: [&str; 7] = [
        "foot",
        "alacritty",
        "kitty",
        "wezterm",
        "gnome-terminal",
        "konsole",
        "xterm",
    ];
    let candidates = std::env::var("TERMINAL")
        .ok()
        .into_iter()
        .chain(FALLBACKS.iter().map(|s| s.to_string()));
    for term in candidates {
        match std::process::Command::new(&term).current_dir(dir).spawn() {
            Ok(_) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(terminal = %term, error = %e, "terminal failed to launch");
                return;
            }
        }
    }
    tracing::warn!("no terminal emulator found (set $TERMINAL)");
}

/// Icon centered in a grid cell's media area when no thumbnail should
/// be shown.
fn tile_icon(name: &'static str) -> El {
    column([icon(name).icon_size(28.0).color(tokens::MUTED_FOREGROUND)])
        .align(Align::Center)
        .justify(Justify::Center)
        .width(Size::Fill(1.0))
        .height(Size::Fixed(TILE_MEDIA_H))
}

/// Realized row/cell without metadata: queue a stat, once per entry.
/// Lives outside the app so both views' `'static` builders can call it
/// on their captured handles.
#[allow(clippy::too_many_arguments)]
fn maybe_request_stat(
    e: &Entry,
    id: EntryId,
    dir: &Path,
    generation: u64,
    pool: &Pool,
    tx: &Sender<Msg>,
    notify: &Notifier,
    stat_requested: &Arc<Mutex<HashSet<EntryId>>>,
) {
    if e.meta.is_some() || e.meta_error.is_some() {
        return;
    }
    let mut requested = stat_requested.lock().unwrap();
    if !requested.insert(id) {
        return;
    }
    let path = dir.join(&e.name);
    submit_stat_job(id, path, generation, Tier::Visible, pool, tx, notify);
}

fn submit_stat_job(
    id: EntryId,
    path: PathBuf,
    generation: u64,
    tier: Tier,
    pool: &Pool,
    tx: &Sender<Msg>,
    notify: &Notifier,
) {
    let tx = tx.clone();
    let notify = notify.clone();
    pool.submit(tier, move || {
        let result = stat::stat_entry(&path);
        let _ = tx.send(Msg::Stat {
            generation,
            id,
            result,
        });
        notify();
    });
}

fn preview_placeholder(icon_name: &str, label: &str) -> El {
    card([preview_placeholder_body(icon_name, label).padding(tokens::SPACE_4)])
        .width(Size::Fixed(420.0))
        .height(Size::Fill(1.0))
}

fn preview_placeholder_body(icon_name: &str, label: &str) -> El {
    column([
        icon(icon_name)
            .icon_size(40.0)
            .color(tokens::MUTED_FOREGROUND),
        text(label.to_string()).caption().muted(),
    ])
    .gap(tokens::SPACE_3)
    .align(Align::Center)
    .justify(Justify::Center)
    .width(Size::Fill(1.0))
    .height(Size::Fill(1.0))
}

fn normal_preview_body(
    preview: &Preview,
    preview_w: f32,
    surface_state: Option<(damascene_core::surface::AppTexture, BinarySurfaceMetrics)>,
) -> El {
    match preview {
        Preview::Image { image: img, meta } => {
            let caption = format!("{}×{} · {}", meta.width, meta.height, meta.encoding);
            column([
                image(img.clone())
                    .image_fit(ImageFit::Contain)
                    .dynamic_range_limit(DynamicRangeLimit::NoLimit)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0)),
                text(caption).caption().muted(),
            ])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
        }
        Preview::Text { text, truncated } => text_preview_body(text.clone(), *truncated),
        Preview::Code { lines, truncated } => code_preview_body(lines, *truncated),
        Preview::Details { icon, title, rows } => details_preview_body(icon, title, rows),
        Preview::Binary(binary) => binary_preview_body(binary, preview_w, surface_state),
        Preview::Unsupported { reason } => preview_placeholder_body("file", reason),
    }
}

fn text_preview_body(body: String, truncated: bool) -> El {
    let (body, capped) = cap_text_body(body);
    let mut children = vec![scroll([code_block(body)])
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))];
    if truncated || capped {
        children.push(text("truncated preview").caption().muted());
    }
    column(children)
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

/// Flatten highlighted lines back to plain text (the "Text" tab view
/// when no `RawPreview` text is available).
fn code_plain_text(lines: &[Vec<CodeSpan>]) -> String {
    lines
        .iter()
        .map(|line| line.iter().map(|s| s.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Syntax-highlighted source: one `text_runs` flow with a mono,
/// per-token-colored run for each span and a hard break between lines,
/// in the same sunken code-block chrome as the plain text preview.
fn code_preview_body(lines: &[Vec<CodeSpan>], truncated: bool) -> El {
    let mut runs: Vec<El> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            runs.push(hard_break());
        }
        for span in line {
            // Syntax colors are intentionally raw per-token colors, not
            // theme tokens — the whole point of highlighting.
            runs.push(
                text(span.text.clone())
                    .mono()
                    .font_size(tokens::TEXT_SM.size)
                    .text_color(Color::srgb_u8(span.color[0], span.color[1], span.color[2]))
                    .allow_lint(damascene_core::bundle::lint::FindingKind::RawColor),
            );
        }
    }
    let body = code_block_chrome(text_runs(runs).width(Size::Fill(1.0)));
    let mut children = vec![scroll([body])
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))];
    if truncated {
        children.push(text("truncated preview").caption().muted());
    }
    column(children)
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

fn details_preview_body(icon_name: &str, title: &str, rows: &[explorer_previews::DetailRow]) -> El {
    let mut children = vec![
        icon(icon_name)
            .icon_size(40.0)
            .color(tokens::MUTED_FOREGROUND),
        text(title.to_string()).bold(),
    ];
    children.extend(rows.iter().map(|row| {
        column([
            text(row.label.clone()).caption().muted(),
            text(row.value.clone()),
        ])
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0))
    }));
    column(children)
        .gap(tokens::SPACE_3)
        .align(Align::Center)
        .justify(Justify::Center)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

fn binary_preview_body(
    preview: &BinaryPreview,
    preview_w: f32,
    surface_state: Option<(damascene_core::surface::AppTexture, BinarySurfaceMetrics)>,
) -> El {
    const CELL: f32 = 4.0;
    const GAP: f32 = 1.0;
    const MAX_CELLS: usize = 4096;

    let available_w = (preview_w - tokens::SPACE_4 * 2.0 - tokens::SPACE_2 * 2.0).max(80.0);
    let cols = ((available_w + GAP) / (CELL + GAP))
        .floor()
        .clamp(16.0, 160.0) as usize;
    let fallback_shown = preview.bytes.len().min(MAX_CELLS);
    let visible_bytes = surface_state
        .as_ref()
        .map(|(_, metrics)| metrics.shown)
        .unwrap_or(fallback_shown);
    let map = if let Some((texture, metrics)) = surface_state {
        stack([
            spacer()
                .fill(tokens::MUTED)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            surface(texture)
                .surface_alpha(SurfaceAlpha::Opaque)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
        ])
        .radius(tokens::RADIUS_SM)
        .clip()
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .tooltip(format!(
            "{} columns × {} rows",
            metrics.cols.max(1),
            metrics.rows.max(1)
        ))
    } else {
        let mut rows = Vec::new();
        for chunk in preview.bytes[..fallback_shown].chunks(cols) {
            let cells = chunk.iter().map(|&byte| {
                spacer()
                    .fill(byte_color(byte))
                    .width(Size::Fixed(CELL))
                    .height(Size::Fixed(CELL))
            });
            rows.push(row(cells).gap(GAP).width(Size::Fill(1.0)));
        }
        scroll([column(rows)
            .gap(GAP)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))])
        .fill(tokens::MUTED)
        .radius(tokens::RADIUS_SM)
        .padding(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
    };

    let sample = if preview.truncated {
        format!("first {}", fmt::human_bytes(preview.bytes.len() as u64))
    } else {
        fmt::human_bytes(preview.bytes.len() as u64)
    };
    let visible = if visible_bytes < preview.bytes.len() {
        format!("{} shown", fmt::human_bytes(visible_bytes as u64))
    } else {
        "all shown".into()
    };

    column([
        row([
            binary_stat("Sample", sample),
            binary_stat("Entropy", format!("{:.2} bits/B", preview.entropy_bits)),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0)),
        row([
            binary_stat("ASCII", percent(preview.printable_fraction)),
            binary_stat(
                "NUL / FF",
                format!(
                    "{} / {}",
                    percent(preview.nul_fraction),
                    percent(preview.ff_fraction)
                ),
            ),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0)),
        map,
        text(format!(
            "{visible} · {} distinct byte values",
            preview.distinct_values
        ))
        .caption()
        .muted(),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
    .height(Size::Fill(1.0))
}

fn binary_stat(label: &str, value: String) -> El {
    column([text(label.to_string()).caption().muted(), text(value)])
        .gap(tokens::SPACE_1)
        .width(Size::Fill(1.0))
}

fn percent(value: f32) -> String {
    format!("{:.0}%", value * 100.0)
}

fn sort_options() -> Vec<(String, String)> {
    [
        SortMode::NameAsc,
        SortMode::NameDesc,
        SortMode::TypeAsc,
        SortMode::ModifiedDesc,
        SortMode::ModifiedAsc,
        SortMode::SizeDesc,
        SortMode::SizeAsc,
    ]
    .into_iter()
    .map(|mode| (mode.to_string(), mode.label().to_string()))
    .collect()
}

fn byte_color(byte: u8) -> Color {
    let v = byte as f32 / 255.0;
    match byte {
        0x00 => tokens::BACKGROUND.with_alpha(0.90),
        0xff => tokens::FOREGROUND.with_alpha(0.95),
        b'\t' | b'\n' | b'\r' | b' ' => tokens::INFO.with_alpha(0.35 + v * 0.35),
        0x01..=0x1f | 0x7f => tokens::DESTRUCTIVE.with_alpha(0.45 + v * 0.45),
        0x21..=0x7e => tokens::SUCCESS.with_alpha(0.45 + v * 0.40),
        _ => tokens::WARNING.with_alpha(0.35 + v * 0.55),
    }
}

impl App for ExplorerApp {
    fn before_build(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Listing { generation, update } => {
                    if generation != self.listing.generation {
                        continue;
                    }
                    let search = self.search_term();
                    if self.listing.absorb(
                        update,
                        self.show_hidden,
                        self.file_filter.as_ref(),
                        search.as_deref(),
                        self.sort,
                    ) {
                        self.remap_selection();
                    }
                    if let Some(name) = self.pending_select.clone() {
                        if let Some(id) = self.listing.id_by_name(&name) {
                            self.pending_select = None;
                            self.select_id(id);
                        } else if self.listing.complete {
                            self.pending_select = None;
                        }
                    }
                }
                Msg::Stat {
                    generation,
                    id,
                    result,
                } => {
                    if generation != self.listing.generation {
                        continue;
                    }
                    let search = self.search_term();
                    if self.listing.apply_stat(
                        id,
                        result,
                        self.show_hidden,
                        self.file_filter.as_ref(),
                        search.as_deref(),
                        self.sort,
                    ) {
                        self.remap_selection();
                    }
                }
                Msg::Preview {
                    generation,
                    id,
                    result,
                } => {
                    if self.preview_inflight == Some(id) {
                        self.preview_inflight = None;
                    }
                    if generation == self.listing.generation && self.selected_id() == Some(id) {
                        let preview = result.preview.map(cap_text_preview);
                        let raw = result.raw.ok();
                        self.preview = match (preview, raw) {
                            (Err(error), None) => PreviewState::Failed { id, error },
                            (preview, raw) => PreviewState::Ready { id, preview, raw },
                        };
                    }
                    if let Some(wanted) = self.preview_wanted.take() {
                        if wanted != id && self.selected_id() == Some(wanted) {
                            self.request_preview(wanted);
                        }
                    }
                }
                Msg::Thumb {
                    generation,
                    id,
                    result,
                } => {
                    let mut ts = self.thumb_state.lock().unwrap();
                    ts.requested.remove(&id);
                    if generation != self.listing.generation {
                        continue;
                    }
                    match result {
                        ThumbResult::Image(image) => {
                            ts.ram.put(id, image);
                        }
                        ThumbResult::Miss => {
                            ts.failed.insert(id);
                        }
                        ThumbResult::Error(error) => {
                            tracing::warn!(id, error, "thumbnail failed");
                            ts.failed.insert(id);
                        }
                    }
                }
                Msg::Places(places) => self.places = places,
                Msg::AppDb(db) => self.app_db = Some(db),
                Msg::OpDone(outcome) => self.apply_op_outcome(outcome),
                Msg::OpenLocation { dir, select } => self.navigate(dir, select),
            }
        }
        self.request_missing_stats_for_sort();
    }

    fn build(&self, cx: &BuildCx) -> El {
        scaffold(self.page_el(cx))
    }

    fn hotkeys(&self) -> Vec<(KeyChord, String)> {
        // While the New Folder / Rename prompt is open, register nothing
        // so every key (Enter, Backspace, Delete, arrows, letters) is
        // delivered to the focused name field instead of firing a
        // browser action — hotkeys otherwise win over text-field capture.
        if self.prompt.is_some() {
            return Vec::new();
        }
        let shift_delete = KeyChord::named(UiKey::Delete).with_modifiers(KeyModifiers {
            shift: true,
            ..KeyModifiers::default()
        });
        vec![
            (KeyChord::named(UiKey::ArrowUp), "prev".into()),
            (KeyChord::named(UiKey::ArrowDown), "next".into()),
            (KeyChord::named(UiKey::ArrowLeft), "left".into()),
            (KeyChord::named(UiKey::ArrowRight), "right".into()),
            (KeyChord::named(UiKey::Home), "first".into()),
            (KeyChord::named(UiKey::End), "last".into()),
            (KeyChord::named(UiKey::Enter), "open".into()),
            (KeyChord::named(UiKey::Backspace), "parent".into()),
            (KeyChord::vim('k'), "prev".into()),
            (KeyChord::vim('j'), "next".into()),
            (KeyChord::vim('h'), "left".into()),
            (KeyChord::vim('l'), "right".into()),
            (KeyChord::vim('g'), "view".into()),
            (KeyChord::vim('r'), "refresh".into()),
            (KeyChord::ctrl('f'), "search".into()),
            (KeyChord::named(UiKey::Other("F5".into())), "refresh".into()),
            (KeyChord::named(UiKey::Other("F2".into())), "rename".into()),
            (KeyChord::vim('.'), "hidden".into()),
            (KeyChord::named(UiKey::Space), "mark".into()),
            (KeyChord::ctrl_shift('n'), "new-folder".into()),
            // Shift+Delete (permanent) must be registered before bare
            // Delete (trash): `matches` is exact on modifiers, but listing
            // the more-specific chord first keeps intent obvious.
            (shift_delete, "delete-permanent".into()),
            (KeyChord::named(UiKey::Delete), "trash".into()),
        ]
    }

    fn on_event(&mut self, event: UiEvent, _cx: &EventCx) {
        use damascene_core::widgets::resize_handle::Side;
        // A file-op error notice is modal: dismiss and swallow the rest.
        if self.op_error.is_some() {
            if event.is_click_or_activate("op-error:close")
                || event.is_click_or_activate("op-error:dismiss")
                || event.kind == UiEventKind::Escape
            {
                self.op_error = None;
            }
            return;
        }
        // The New Folder / Rename prompt is modal. `hotkeys()` returns
        // empty while it's open, so Enter / Escape / editing keys reach
        // the field here as raw KeyDowns rather than firing browser
        // actions underneath.
        if self.prompt.is_some() {
            if event.is_click_or_activate("prompt:commit") {
                self.commit_prompt();
                return;
            }
            if event.is_click_or_activate("prompt:cancel")
                || event.is_click_or_activate("prompt:dismiss")
            {
                self.prompt = None;
                return;
            }
            let key = event.key_press.as_ref().map(|kp| kp.key.clone());
            if event.kind == UiEventKind::KeyDown && key.as_ref() == Some(&UiKey::Enter) {
                self.commit_prompt();
                return;
            }
            if event.kind == UiEventKind::Escape
                || (event.kind == UiEventKind::KeyDown && key.as_ref() == Some(&UiKey::Escape))
            {
                self.prompt = None;
                return;
            }
            if let Some(prompt) = self.prompt.as_mut() {
                text_input::apply_event(
                    &mut prompt.value,
                    &mut prompt.selection,
                    "prompt-field",
                    &event,
                );
            }
            return;
        }
        // The permanent-delete confirmation is modal.
        if self.confirm_delete.is_some() {
            if event.is_click_or_activate("confirm-delete:confirm") {
                self.confirm_delete_now();
                return;
            }
            if event.is_click_or_activate("confirm-delete:cancel")
                || event.is_click_or_activate("confirm-delete:dismiss")
                || event.kind == UiEventKind::Escape
            {
                self.confirm_delete = None;
            }
            return;
        }
        // The Properties dialog is modal: while it's up, only its own
        // controls are live (Close button, scrim dismiss, Escape).
        if self.properties.is_some() {
            if event.is_click_or_activate("props:close")
                || event.is_click_or_activate("props:dismiss")
                || event.kind == UiEventKind::Escape
            {
                self.properties = None;
            }
            return;
        }
        // The "Open with…" chooser is likewise modal: an app row
        // launches and closes; cancel/scrim/Escape just close.
        if let Some(target) = self.open_with {
            if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
                if let Some(app_id) = event.route().and_then(|r| r.strip_prefix("openwith:")) {
                    let app_id = app_id.to_string();
                    if let Some(path) = self.entry_path(target) {
                        if let Some(db) = &self.app_db {
                            db.launch(&app_id, &path);
                        }
                    }
                    self.open_with = None;
                    return;
                }
            }
            if event.is_click_or_activate("open-with:cancel")
                || event.is_click_or_activate("open-with:dismiss")
                || event.kind == UiEventKind::Escape
            {
                self.open_with = None;
            }
            return;
        }
        // The context menu is a non-modal popover. Item activations
        // route here; Escape or a click elsewhere dismisses it (a left
        // click still acts underneath, so clicking another row both
        // closes the menu and selects that row).
        if self.context_menu.is_some() {
            if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
                if let Some(action) = event.route_suffix("ctx") {
                    let target = self.context_menu.take().unwrap().target;
                    self.run_context_action(action, target);
                    return;
                }
            }
            match event.kind {
                UiEventKind::Escape => {
                    self.context_menu = None;
                    return;
                }
                UiEventKind::Click | UiEventKind::SecondaryClick => {
                    self.context_menu = None;
                }
                _ => {}
            }
        }
        if event.target_key() == Some("browser-search") {
            let mut search = self.search.clone();
            if text_input::apply_event(&mut search, &mut self.selection, "browser-search", &event) {
                self.set_search(search);
            }
            return;
        }
        if event.is_click_or_activate("search-clear") {
            self.selection = Selection::default();
            self.set_search(String::new());
            return;
        }
        if let Some(selection) = event.selection.clone() {
            self.selection = selection;
            return;
        }
        if tabs::apply_event(
            &mut self.preview_mode,
            &event,
            "preview-mode",
            parse_preview_mode,
        ) {
            return;
        }
        let mut sort = self.sort;
        let mut sort_open = self.sort_open;
        if select::apply_event(&mut sort, &mut sort_open, &event, "browser-sort", |s| {
            parse_sort_mode(&s)
        }) {
            self.sort_open = sort_open;
            self.set_sort_mode(sort);
            return;
        }

        if event.route() == Some("sidebar-resize") {
            resize_handle::apply_event_fixed(
                &mut self.sidebar_w,
                &mut self.sidebar_drag,
                &event,
                "sidebar-resize",
                Axis::Row,
                Side::Start,
                SIDEBAR_MIN,
                SIDEBAR_MAX,
            );
            return;
        }
        if event.route() == Some("preview-resize") {
            resize_handle::apply_event_fixed(
                &mut self.preview_w,
                &mut self.preview_drag,
                &event,
                "preview-resize",
                Axis::Row,
                Side::End,
                PREVIEW_MIN,
                PREVIEW_MAX,
            );
            return;
        }

        if let Some(key) = event.target_key() {
            if let Some(id) = key.strip_prefix("row:").and_then(|s| s.parse().ok()) {
                // Rows select on *mouse* click only — keyboard selection
                // is the arrow hotkeys, and keyboard activation (Enter,
                // Space) is handled by the "open"/"mark" hotkeys below,
                // so we don't treat a focused row's Activate as a click.
                if event.kind == UiEventKind::Click {
                    if event.click_count >= 2 {
                        self.select_only(id);
                        self.activate_id(id);
                        return;
                    }
                    let m = event.modifiers;
                    if m.ctrl {
                        self.toggle_mark(id);
                    } else if m.shift {
                        self.range_mark(id);
                    } else {
                        self.select_only(id);
                    }
                    return;
                }
                if event.kind == UiEventKind::SecondaryClick {
                    let point = event.pointer.unwrap_or_default();
                    self.open_context_menu(id, point);
                    return;
                }
            }
            if key == "listing-area" && event.kind == UiEventKind::SecondaryClick {
                let point = event.pointer.unwrap_or_default();
                self.open_background_menu(point);
                return;
            }
            if let Some(i) = key
                .strip_prefix("place:")
                .and_then(|s| s.parse::<usize>().ok())
            {
                if event.is_click_or_activate(key) {
                    if let Some(place) = self.places.get(i) {
                        self.navigate(place.path.clone(), None);
                    }
                    return;
                }
            }
            if let Some(i) = key
                .strip_prefix("crumb:")
                .and_then(|s| s.parse::<usize>().ok())
            {
                if event.is_click_or_activate(key) {
                    if let Some((_, path)) = self.crumbs().get(i).cloned() {
                        self.navigate(path, None);
                    }
                    return;
                }
            }
            if key == "up" && event.is_click_or_activate(key) {
                self.navigate_parent();
                return;
            }
            if key == "view-toggle" && event.is_click_or_activate(key) {
                self.toggle_view();
                return;
            }
            if key == "new-folder" && event.is_click_or_activate(key) {
                self.begin_new_folder();
                return;
            }
        }

        // Vertical motion is one entry in the list, one grid row in the
        // grid; horizontal motion only exists in the grid (in the list,
        // left/right walk the hierarchy, file-manager style).
        let vstep = match self.view {
            ViewMode::List => 1,
            ViewMode::Grid => self.cols.get().max(1) as isize,
        };
        if event.is_hotkey("next") {
            self.move_selection(vstep);
        } else if event.is_hotkey("prev") {
            self.move_selection(-vstep);
        } else if event.is_hotkey("left") {
            match self.view {
                ViewMode::List => self.navigate_parent(),
                ViewMode::Grid => self.move_selection(-1),
            }
        } else if event.is_hotkey("right") {
            match self.view {
                ViewMode::List => {
                    if let Some(id) = self.selected_id() {
                        let entries = self.listing.entries.lock().unwrap();
                        let is_dir = entries.get(id as usize).is_some_and(Entry::is_dir);
                        drop(entries);
                        if is_dir {
                            self.activate_id(id);
                        }
                    }
                }
                ViewMode::Grid => self.move_selection(1),
            }
        } else if event.is_hotkey("view") {
            self.toggle_view();
        } else if event.is_hotkey("refresh") {
            self.refresh();
        } else if event.is_hotkey("search") {
            self.focus_search();
        } else if event.is_hotkey("first") {
            self.select_pos(0);
        } else if event.is_hotkey("last") {
            self.select_pos(self.listing.order.len().saturating_sub(1));
        } else if event.is_hotkey("open") {
            if let Some(id) = self.selected_id() {
                self.activate_id(id);
            }
        } else if event.is_hotkey("parent") {
            self.navigate_parent();
        } else if event.is_hotkey("hidden") {
            self.toggle_hidden();
        } else if event.is_hotkey("mark") {
            if let Some(id) = self.selected_id() {
                self.toggle_mark(id);
            }
        } else if event.is_hotkey("new-folder") {
            self.begin_new_folder();
        } else if event.is_hotkey("rename") {
            if let Some(id) = self.selected_id() {
                self.begin_rename(id);
            }
        } else if event.is_hotkey("trash") {
            if let Some(id) = self.selected_id() {
                let targets = self.bulk_targets(id);
                self.trash_entries(targets);
            }
        } else if event.is_hotkey("delete-permanent") {
            if let Some(id) = self.selected_id() {
                self.confirm_delete = Some(self.bulk_targets(id));
            }
        }
    }

    fn drain_scroll_requests(&mut self) -> Vec<ScrollRequest> {
        std::mem::take(&mut *self.scroll_requests.borrow_mut())
    }

    fn selection(&self) -> Selection {
        self.selection.clone()
    }

    fn drain_focus_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.focus_requests)
    }
}

impl crate::host::HostApp for ExplorerApp {
    fn gpu_setup(&mut self, device: &wgpu::Device, _queue: &wgpu::Queue) {
        self.binary_surface = Some(BinarySurface::new(device));
    }

    fn before_paint(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: Rect,
        scale_factor: f32,
    ) {
        let selected = self.selected.map(|(id, _)| id);
        let binary = match &self.preview {
            PreviewState::Ready { id, preview, raw } if Some(*id) == selected => {
                let preview_ref = preview.as_ref().ok();
                match effective_preview_mode(self.preview_mode, preview_ref, raw.as_ref()) {
                    PreviewMode::Binary => raw.as_ref().map(|raw| &raw.binary).or_else(|| {
                        preview_ref.and_then(|preview| match preview {
                            Preview::Binary(binary) => Some(binary),
                            _ => None,
                        })
                    }),
                    PreviewMode::Normal => preview_ref.and_then(|preview| match preview {
                        Preview::Binary(binary) => Some(binary),
                        _ => None,
                    }),
                    PreviewMode::Text => None,
                }
            }
            _ => None,
        };
        let Some(binary) = binary else {
            return;
        };
        let Some(preview_w) = self.browser_chrome(viewport.w).preview_w else {
            return;
        };
        let Some(surface) = &mut self.binary_surface else {
            return;
        };
        let (logical_w, logical_h) = binary_surface_size(preview_w, viewport.h);
        surface.write(device, queue, binary, logical_w, logical_h, scale_factor);
    }

    fn drain_clipboard_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clipboard_writes)
    }
}

fn binary_surface_size(preview_w: f32, viewport_h: f32) -> (f32, f32) {
    let width = (preview_w - tokens::SPACE_4 * 2.0).max(96.0);
    let height = (viewport_h - 250.0).clamp(160.0, 1200.0);
    (width, height)
}

/// Bound what one mono text leaf has to lay out; the read itself is
/// already bounded by the handler.
fn cap_text_preview(preview: Preview) -> Preview {
    match preview {
        Preview::Text { text, truncated } => {
            let (text, capped) = cap_text_body(text);
            Preview::Text {
                text,
                truncated: truncated || capped,
            }
        }
        other => other,
    }
}

fn cap_text_body(mut text: String) -> (String, bool) {
    let mut end = text.len();
    let mut lines = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            lines += 1;
            if lines >= TEXT_PREVIEW_MAX_LINES {
                end = i;
                break;
            }
        }
    }
    let capped = end < text.len();
    text.truncate(end);
    (text, capped)
}

/// What the host negotiated with the display server, as a toolbar badge
/// (same logic as the gallery's).
fn color_mode_badge(cx: &BuildCx) -> El {
    use damascene_core::color::ColorManagementStatus;

    let Some(diag) = cx.diagnostics() else {
        return badge("SDR").key("color-mode");
    };
    let fp16 = diag
        .surface_color
        .as_ref()
        .is_some_and(|s| s.chosen_format == "Rgba16Float");
    let b = match &diag.color_management {
        ColorManagementStatus::Available { targets, .. } => {
            if fp16 && targets.indicates_hdr() {
                let peak = targets
                    .target_max_luminance_nits
                    .map(|n| format!(" · {n:.0} nits"))
                    .unwrap_or_default();
                badge(format!("HDR · scRGB{peak}"))
                    .success()
                    .tooltip("extended-range Rgba16Float swapchain; compositor reports HDR output")
            } else {
                badge("SDR").tooltip("color management available; output reports no HDR headroom")
            }
        }
        _ => badge("SDR").tooltip("no wp_color_management_v1 on this host"),
    };
    b.key("color-mode")
}

/// Canned-scene constructors — a synthetic listing, no real IO (pool
/// jobs queue but the scenes never drain results). Shared by the
/// in-crate lint tests and the `dump_bundles` artifact bin, which is
/// why this isn't under `cfg(test)`. Composed into named, viewport-
/// sized scenes by [`crate::fixtures`].
pub(crate) mod fixtures {
    use super::*;
    use explorer_io::listing::ListingUpdate;
    use explorer_io::stat::EntryMeta;
    use explorer_io::RawEntry;

    /// Browsing `/test/somewhere`: a directory, a text file, an image
    /// file, two synthetic places. The base every scene grows from.
    pub(crate) fn browse() -> ExplorerApp {
        let pool = Pool::spawn(1, "test");
        let notifier: Notifier = Arc::new(|| {});
        let thumbs = ThumbCache::new(
            std::env::temp_dir().join(format!("explorer-app-test-{}", std::process::id())),
            256,
        );
        let mut app = ExplorerApp::new(
            PathBuf::from("/test/somewhere"),
            pool,
            notifier,
            Arc::new(Registry::standard()),
            Arc::new(thumbs),
        );
        app.places = vec![
            Place {
                label: "Home".into(),
                path: "/home/test".into(),
                icon: "folder",
            },
            Place {
                label: "/ceph".into(),
                path: "/ceph".into(),
                icon: "activity",
            },
        ];
        let batch = vec![
            RawEntry {
                name: "docs".into(),
                kind: EntryKind::Dir,
            },
            RawEntry {
                name: "notes.txt".into(),
                kind: EntryKind::File,
            },
            RawEntry {
                name: "photo.jxr".into(),
                kind: EntryKind::File,
            },
        ];
        app.listing.absorb(
            ListingUpdate {
                batch,
                done: true,
                error: None,
            },
            false,
            None,
            None,
            app.sort,
        );
        stat_file(&mut app, "notes.txt", 12);
        stat_file(&mut app, "photo.jxr", 1024);
        app
    }

    /// `notes.txt` selected with a (truncated) text preview ready.
    pub(crate) fn text_preview() -> ExplorerApp {
        let mut app = browse();
        let id = select(&mut app, "notes.txt");
        let raw_bytes = b"hello\nworld\n".to_vec();
        app.preview = PreviewState::Ready {
            id,
            preview: Ok(Preview::Text {
                text: "hello\nworld\n".into(),
                truncated: true,
            }),
            raw: Some(raw_preview_fixture(raw_bytes, Some("hello\nworld\n"), true)),
        };
        app
    }

    /// `main.rs` selected with a syntax-highlighted code preview
    /// (synthetic spans — no syntect call in the fixture).
    pub(crate) fn code_preview() -> ExplorerApp {
        let mut app = browse();
        app.listing.absorb(
            ListingUpdate {
                batch: vec![RawEntry {
                    name: "main.rs".into(),
                    kind: EntryKind::File,
                }],
                done: true,
                error: None,
            },
            false,
            None,
            None,
            app.sort,
        );
        let id = select(&mut app, "main.rs");
        let span = |text: &str, color: [u8; 3]| CodeSpan {
            text: text.into(),
            color,
        };
        const KW: [u8; 3] = [180, 142, 173];
        const FUNC: [u8; 3] = [136, 192, 208];
        const NUM: [u8; 3] = [208, 135, 112];
        const PLAIN: [u8; 3] = [192, 197, 206];
        let lines = vec![
            vec![span("fn ", KW), span("main", FUNC), span("() {", PLAIN)],
            vec![
                span("    let x = ", PLAIN),
                span("42", NUM),
                span(";", PLAIN),
            ],
            vec![span("}", PLAIN)],
        ];
        app.preview = PreviewState::Ready {
            id,
            preview: Ok(Preview::Code {
                lines,
                truncated: false,
            }),
            raw: None,
        };
        app
    }

    /// `report.pdf` selected with a structured metadata preview.
    pub(crate) fn details_preview() -> ExplorerApp {
        let mut app = browse();
        app.listing.absorb(
            ListingUpdate {
                batch: vec![RawEntry {
                    name: "report.pdf".into(),
                    kind: EntryKind::File,
                }],
                done: true,
                error: None,
            },
            false,
            None,
            None,
            app.sort,
        );
        let id = select(&mut app, "report.pdf");
        let raw_bytes = b"%PDF-1.7\n1 0 obj\n".to_vec();
        app.preview = PreviewState::Ready {
            id,
            preview: Ok(Preview::Details {
                icon: "file-text",
                title: "PDF document".into(),
                rows: vec![
                    explorer_previews::DetailRow {
                        label: "Format".into(),
                        value: "PDF 1.7".into(),
                    },
                    explorer_previews::DetailRow {
                        label: "Pages".into(),
                        value: "12".into(),
                    },
                    explorer_previews::DetailRow {
                        label: "Preview".into(),
                        value: "metadata only".into(),
                    },
                ],
            }),
            raw: Some(raw_preview_fixture(
                raw_bytes,
                Some("%PDF-1.7\n1 0 obj\n"),
                false,
            )),
        };
        app
    }

    /// `firmware.bin` selected with a byte-map preview ready.
    pub(crate) fn binary_preview() -> ExplorerApp {
        let mut app = browse();
        app.listing.absorb(
            ListingUpdate {
                batch: vec![RawEntry {
                    name: "firmware.bin".into(),
                    kind: EntryKind::File,
                }],
                done: true,
                error: None,
            },
            false,
            None,
            None,
            app.sort,
        );
        let id = select(&mut app, "firmware.bin");
        let mut bytes = Vec::new();
        for i in 0..4096u32 {
            let b = match i % 64 {
                0..=15 => 0,
                16..=31 => b'A' + (i % 26) as u8,
                32..=47 => 0xff,
                _ => (i * 37) as u8,
            };
            bytes.push(b);
        }
        let raw = raw_preview_fixture(bytes.clone(), None, true);
        app.preview = PreviewState::Ready {
            id,
            preview: Ok(Preview::Binary(explorer_previews::BinaryPreview {
                bytes,
                truncated: true,
                entropy_bits: 4.75,
                printable_fraction: 0.25,
                nul_fraction: 0.25,
                ff_fraction: 0.25,
                distinct_values: 80,
            })),
            raw: Some(raw),
        };
        app
    }

    fn raw_preview_fixture(
        bytes: Vec<u8>,
        text: Option<&str>,
        truncated: bool,
    ) -> explorer_previews::RawPreview {
        explorer_previews::RawPreview {
            text: text.map(str::to_string),
            binary: explorer_previews::BinaryPreview::from_bytes(bytes, truncated),
        }
    }

    /// Grid view with every cell flavor at once: a decoded thumbnail
    /// (synthetic 2×2 image in the RAM LRU, selected), a pending image
    /// (skeleton + queued job), a failed image, plus plain dir/file
    /// icon cells.
    pub(crate) fn grid() -> ExplorerApp {
        let mut app = browse();
        app.view = ViewMode::Grid;
        app.listing.absorb(
            ListingUpdate {
                batch: vec![
                    RawEntry {
                        name: "pending.png".into(),
                        kind: EntryKind::File,
                    },
                    RawEntry {
                        name: "broken.avif".into(),
                        kind: EntryKind::File,
                    },
                ],
                done: true,
                error: None,
            },
            false,
            None,
            None,
            app.sort,
        );
        stat_file(&mut app, "pending.png", 1024);
        stat_file(&mut app, "broken.avif", 1024);
        let thumbed = app
            .listing
            .id_by_name(std::ffi::OsStr::new("photo.jxr"))
            .unwrap();
        let broken = app
            .listing
            .id_by_name(std::ffi::OsStr::new("broken.avif"))
            .unwrap();
        {
            let mut ts = app.thumb_state.lock().unwrap();
            ts.ram.put(thumbed, Image::from_rgba8(2, 2, vec![128; 16]));
            ts.failed.insert(broken);
        }
        select(&mut app, "photo.jxr");
        app
    }

    fn stat_file(app: &mut ExplorerApp, name: &str, size: u64) {
        let id = app
            .listing
            .id_by_name(std::ffi::OsStr::new(name))
            .expect("fixture entry exists");
        app.listing.apply_stat(
            id,
            Ok(EntryMeta {
                size,
                modified: None,
                kind: EntryKind::File,
                is_symlink: false,
            }),
            false,
            None,
            None,
            app.sort,
        );
    }

    /// The listing failed outright.
    pub(crate) fn listing_error() -> ExplorerApp {
        let mut app = browse();
        app.listing.error = Some("opening /test/somewhere: permission denied".into());
        app
    }

    /// Select the entry named `name`, returning its id.
    pub(crate) fn select(app: &mut ExplorerApp, name: &str) -> EntryId {
        let id = app
            .listing
            .id_by_name(std::ffi::OsStr::new(name))
            .expect("fixture entry exists");
        let pos = app.listing.pos_of(id).unwrap();
        app.selected = Some((id, pos));
        id
    }

    /// Add `name` to the marked (multi-selection) set.
    pub(crate) fn mark(app: &mut ExplorerApp, name: &str) {
        let id = app
            .listing
            .id_by_name(std::ffi::OsStr::new(name))
            .expect("fixture entry exists");
        app.marked.insert(id);
    }

    /// Browser with the right-click context menu open on a file —
    /// exercises the popover anchored mid-list.
    pub(crate) fn context_menu() -> ExplorerApp {
        let mut app = browse();
        let id = select(&mut app, "photo.jxr");
        app.open_context_menu(id, (520.0, 320.0));
        app
    }

    /// Browser with the directory-background (empty-space) menu open —
    /// the New Folder / terminal popover, no entry targeted.
    pub(crate) fn background_menu() -> ExplorerApp {
        let mut app = browse();
        app.open_background_menu((360.0, 300.0));
        app
    }

    /// Browser with the modal Properties dialog open on a stat'ed file.
    pub(crate) fn properties() -> ExplorerApp {
        let mut app = browse();
        let id = select(&mut app, "notes.txt");
        app.properties = Some(id);
        app
    }

    /// Browser with the "Open with…" chooser open on a text file,
    /// backed by a synthetic app database (no real desktop DB).
    pub(crate) fn open_with() -> ExplorerApp {
        let mut app = browse();
        let id = select(&mut app, "notes.txt");
        app.app_db = Some(Arc::new(AppDb::from_fixture(
            &[
                ("org.gnome.gedit.desktop", "gedit"),
                ("dev.zed.Zed.desktop", "Zed"),
                ("org.libreoffice.writer.desktop", "LibreOffice Writer"),
            ],
            &[(
                "text/plain",
                &[
                    "org.gnome.gedit.desktop",
                    "dev.zed.Zed.desktop",
                    "org.libreoffice.writer.desktop",
                ],
            )],
        )));
        app.open_with = Some(id);
        app
    }

    /// Browser with the New Folder name-entry prompt open.
    pub(crate) fn new_folder_prompt() -> ExplorerApp {
        let mut app = browse();
        app.begin_new_folder();
        app
    }

    /// Browser with the Rename prompt open on a file, its name
    /// pre-selected for replacement.
    pub(crate) fn rename_prompt() -> ExplorerApp {
        let mut app = browse();
        let id = select(&mut app, "notes.txt");
        app.begin_rename(id);
        app
    }

    /// Browser with the modal permanent-delete confirmation open.
    pub(crate) fn confirm_delete() -> ExplorerApp {
        let mut app = browse();
        let id = select(&mut app, "notes.txt");
        app.confirm_delete = Some(vec![id]);
        app
    }

    /// Browser showing a failed-operation error notice.
    pub(crate) fn op_error() -> ExplorerApp {
        let mut app = browse();
        select(&mut app, "notes.txt");
        app.op_error = Some("a file with that name already exists".into());
        app
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{browse, grid, mark};
    use super::*;
    use crate::host::HostApp;
    use explorer_io::listing::ListingUpdate;
    use explorer_io::RawEntry;

    /// `cols` is a build-time output (viewport-dependent); rendering
    /// the grid scene at the browser viewport must produce a real
    /// multi-column layout for keyboard navigation to move by.
    #[test]
    fn grid_lays_out_multiple_columns() {
        let app = grid();
        crate::fixtures::render(&app, (1500.0, 950.0));
        assert!(app.cols.get() > 1, "grid should lay out multiple columns");
    }

    #[test]
    fn half_width_without_file_drops_preview_pane() {
        let chrome = browser_chrome_widths(750.0, 220.0, 420.0, false);
        assert_eq!(chrome.sidebar_w, Some(220.0));
        assert_eq!(chrome.preview_w, None);
        let listing_w = listing_available_w(750.0, chrome);
        assert!(
            listing_w >= LISTING_MIN,
            "listing should stay first-class at half width, got {listing_w}"
        );
    }

    #[test]
    fn half_width_with_file_drops_places_sidebar() {
        let chrome = browser_chrome_widths(750.0, 220.0, 420.0, true);
        assert_eq!(chrome.sidebar_w, None);
        assert!(chrome.preview_w.is_some());
        let listing_w = listing_available_w(750.0, chrome);
        assert!(
            listing_w >= LISTING_MIN,
            "listing should stay first-class beside a preview, got {listing_w}"
        );
    }

    #[test]
    fn full_width_browser_keeps_requested_visible_chrome() {
        assert_eq!(
            browser_chrome_widths(1500.0, 220.0, 420.0, true),
            BrowserChrome {
                sidebar_w: Some(220.0),
                preview_w: Some(420.0),
            }
        );
        assert_eq!(
            browser_chrome_widths(1500.0, 220.0, 420.0, false),
            BrowserChrome {
                sidebar_w: Some(220.0),
                preview_w: None,
            }
        );
    }

    #[test]
    fn preview_tabs_switch_modes() {
        let mut app = fixtures::text_preview();
        let cx = EventCx::new();
        assert_eq!(app.preview_mode, PreviewMode::Normal);

        app.on_event(UiEvent::synthetic_click("preview-mode:tab:text"), &cx);
        assert_eq!(app.preview_mode, PreviewMode::Text);

        app.on_event(UiEvent::synthetic_click("preview-mode:tab:binary"), &cx);
        assert_eq!(app.preview_mode, PreviewMode::Binary);

        if let PreviewState::Ready { preview, raw, .. } = &app.preview {
            assert_eq!(
                effective_preview_mode(app.preview_mode, preview.as_ref().ok(), raw.as_ref()),
                PreviewMode::Binary
            );
        } else {
            panic!("fixture should have a loaded preview");
        }
    }

    #[test]
    fn sort_selector_changes_order() {
        let mut app = browse();
        let cx = EventCx::new();

        app.on_event(UiEvent::synthetic_click("browser-sort"), &cx);
        assert!(app.sort_open);

        app.on_event(
            UiEvent::synthetic_click("browser-sort:option:size-desc"),
            &cx,
        );

        assert_eq!(app.sort, SortMode::SizeDesc);
        assert!(!app.sort_open);
        assert_eq!(app.visible_names(), ["docs", "photo.jxr", "notes.txt"]);
    }

    fn id_of(app: &ExplorerApp, name: &str) -> EntryId {
        app.listing.id_by_name(std::ffi::OsStr::new(name)).unwrap()
    }

    /// Ctrl toggles marks; Shift ranges from the anchor; plain
    /// selection clears the set; marked_file_paths returns non-dir
    /// marks in listing order.
    #[test]
    fn multi_select_chords() {
        let mut app = browse(); // order: docs(dir), notes.txt, photo.jxr
        let notes = id_of(&app, "notes.txt");
        let photo = id_of(&app, "photo.jxr");

        // Ctrl-style toggle marks both files.
        app.toggle_mark(notes);
        app.toggle_mark(photo);
        assert_eq!(app.marked, HashSet::from([notes, photo]));
        // Cursor follows the last toggle.
        assert_eq!(app.selected_id(), Some(photo));

        // Toggling photo again removes it.
        app.toggle_mark(photo);
        assert_eq!(app.marked, HashSet::from([notes]));

        // Paths: non-dir marks, in listing order, absolute.
        assert_eq!(
            app.marked_file_paths(),
            vec![PathBuf::from("/test/somewhere/notes.txt")]
        );

        // A plain selection clears the marks.
        app.select_only(notes);
        assert!(app.marked.is_empty());

        // Shift-range from anchor(notes) to photo marks the whole span
        // (notes, photo — docs is a dir but ranges include it; it's
        // dropped only from marked_file_paths).
        app.range_mark(photo);
        assert!(app.marked.contains(&notes) && app.marked.contains(&photo));
        assert_eq!(
            app.marked_file_paths(),
            vec![
                PathBuf::from("/test/somewhere/notes.txt"),
                PathBuf::from("/test/somewhere/photo.jxr"),
            ]
        );
    }

    /// Keyboard nav and navigation both drop the multi-selection.
    #[test]
    fn marks_clear_on_keyboard_nav_and_navigate() {
        let mut app = browse();
        app.toggle_mark(id_of(&app, "notes.txt"));
        app.toggle_mark(id_of(&app, "photo.jxr"));
        assert_eq!(app.marked.len(), 2);

        // A plain arrow move is single-select.
        app.move_selection(1);
        assert!(app.marked.is_empty());

        // And a navigation resets everything.
        app.toggle_mark(id_of(&app, "notes.txt"));
        app.navigate(PathBuf::from("/elsewhere"), None);
        assert!(app.marked.is_empty() && app.anchor.is_none());
    }

    /// Selection follows ids across a mid-stream resort: select an
    /// entry, absorb a batch that sorts ahead of it, and the selected
    /// id must be unchanged with an updated position.
    #[test]
    fn selection_survives_streaming_resort() {
        let mut app = browse();
        let id = app
            .listing
            .id_by_name(std::ffi::OsStr::new("notes.txt"))
            .unwrap();
        let pos = app.listing.pos_of(id).unwrap();
        app.selected = Some((id, pos));

        app.listing.absorb(
            ListingUpdate {
                batch: vec![
                    RawEntry {
                        name: "aaa-first.txt".into(),
                        kind: EntryKind::File,
                    },
                    RawEntry {
                        name: "also-a-dir".into(),
                        kind: EntryKind::Dir,
                    },
                ],
                done: true,
                error: None,
            },
            false,
            None,
            None,
            app.sort,
        );
        app.remap_selection();

        let (sel_id, sel_pos) = app.selected.unwrap();
        assert_eq!(sel_id, id);
        assert_eq!(app.listing.order[sel_pos], id);
        assert!(sel_pos != pos, "position should have shifted");
    }

    #[test]
    fn search_filters_visible_names_and_remaps_selection() {
        let mut app = browse();
        let photo = id_of(&app, "photo.jxr");
        app.select_only(photo);

        app.set_search("PHO".into());
        assert_eq!(app.visible_names(), ["photo.jxr"]);
        assert_eq!(app.selected_id(), Some(photo));

        app.set_search("docs".into());
        assert_eq!(app.visible_names(), ["docs"]);
        assert_eq!(app.selected_id(), None);

        app.set_search(String::new());
        assert_eq!(app.visible_names(), ["docs", "notes.txt", "photo.jxr"]);
    }

    #[test]
    fn search_focus_request_selects_existing_query() {
        let mut app = browse();
        app.set_search("photo".into());

        app.focus_search();

        assert_eq!(app.drain_focus_requests(), ["browser-search"]);
        assert_eq!(
            app.selection.range,
            Some(SelectionRange {
                anchor: SelectionPoint::new("browser-search", 0),
                head: SelectionPoint::new("browser-search", 5),
            })
        );
    }

    #[test]
    fn text_preview_caps_lines() {
        let many = "line\n".repeat(TEXT_PREVIEW_MAX_LINES * 2);
        match cap_text_preview(Preview::Text {
            text: many,
            truncated: false,
        }) {
            Preview::Text { text, truncated } => {
                assert!(truncated);
                assert_eq!(text.lines().count(), TEXT_PREVIEW_MAX_LINES);
            }
            _ => panic!("expected text"),
        }
    }

    /// Right-click selects the row and opens the menu targeting it.
    #[test]
    fn opening_context_menu_selects_its_target() {
        let mut app = browse();
        let photo = id_of(&app, "photo.jxr");
        app.open_context_menu(photo, (12.0, 34.0));
        assert_eq!(
            app.context_menu.map(|m| m.target),
            Some(ContextTarget::Entry(photo))
        );
        assert_eq!(app.selected.map(|(id, _)| id), Some(photo));
    }

    /// "Copy path" queues the entry's absolute path for the host to
    /// drain, and closes the menu.
    #[test]
    fn context_copy_path_queues_clipboard_write() {
        let mut app = browse();
        let cx = EventCx::new();
        app.open_context_menu(id_of(&app, "photo.jxr"), (0.0, 0.0));

        app.on_event(UiEvent::synthetic_click("ctx:copy-path"), &cx);
        assert!(app.context_menu.is_none());
        assert_eq!(app.clipboard_writes, ["/test/somewhere/photo.jxr"]);
        assert_eq!(app.drain_clipboard_writes(), ["/test/somewhere/photo.jxr"]);
        assert!(app.clipboard_writes.is_empty(), "drained once");
    }

    /// "Open" on a directory navigates into it.
    #[test]
    fn context_open_enters_directory() {
        let mut app = browse();
        let cx = EventCx::new();
        app.open_context_menu(id_of(&app, "docs"), (0.0, 0.0));

        app.on_event(UiEvent::synthetic_click("ctx:open"), &cx);
        assert_eq!(app.cwd, PathBuf::from("/test/somewhere/docs"));
    }

    /// A click that isn't a menu item dismisses the menu and still
    /// performs its own action underneath.
    #[test]
    fn click_outside_dismisses_menu_and_acts() {
        let mut app = browse();
        let cx = EventCx::new();
        app.set_search("no".into());
        app.open_context_menu(id_of(&app, "photo.jxr"), (0.0, 0.0));

        app.on_event(UiEvent::synthetic_click("search-clear"), &cx);
        assert!(app.context_menu.is_none(), "outside click dismisses");
        assert!(app.search_term().is_none(), "and still clears the search");
    }

    /// "Properties" opens a modal that swallows stray clicks until
    /// dismissed.
    #[test]
    fn context_properties_modal_blocks_then_closes() {
        let mut app = browse();
        let cx = EventCx::new();
        let notes = id_of(&app, "notes.txt");
        app.open_context_menu(notes, (0.0, 0.0));

        app.on_event(UiEvent::synthetic_click("ctx:properties"), &cx);
        assert_eq!(app.properties, Some(notes));
        assert!(app.context_menu.is_none());

        // Modal: an unrelated click is swallowed, the dialog stays up.
        app.set_search("x".into());
        app.on_event(UiEvent::synthetic_click("search-clear"), &cx);
        assert_eq!(app.properties, Some(notes));
        assert!(app.search_term().is_some(), "modal swallowed the click");

        app.on_event(UiEvent::synthetic_click("props:close"), &cx);
        assert!(app.properties.is_none());
    }

    /// Navigating away drops menu/dialog state — their ids belonged to
    /// the old listing.
    #[test]
    fn navigation_clears_menu_and_properties() {
        let mut app = browse();
        let docs = id_of(&app, "docs");
        app.open_context_menu(docs, (0.0, 0.0));
        app.properties = Some(docs);
        app.open_with = Some(docs);

        app.navigate(PathBuf::from("/test/elsewhere"), None);
        assert!(app.context_menu.is_none());
        assert!(app.properties.is_none());
        assert!(app.open_with.is_none());
    }

    /// "Open with…" opens the modal chooser, which swallows unrelated
    /// clicks until cancelled.
    #[test]
    fn context_open_with_opens_chooser_then_cancels() {
        let mut app = browse();
        let cx = EventCx::new();
        let notes = id_of(&app, "notes.txt");
        app.open_context_menu(notes, (0.0, 0.0));

        app.on_event(UiEvent::synthetic_click("ctx:open-with"), &cx);
        assert_eq!(app.open_with, Some(notes));
        assert!(app.context_menu.is_none());

        app.set_search("x".into());
        app.on_event(UiEvent::synthetic_click("search-clear"), &cx);
        assert_eq!(app.open_with, Some(notes), "chooser is modal");
        assert!(app.search_term().is_some(), "modal swallowed the click");

        app.on_event(UiEvent::synthetic_click("open-with:cancel"), &cx);
        assert!(app.open_with.is_none());
    }

    /// Picking an app closes the chooser. (app_db left None so this
    /// exercises the close path without spawning a real process.)
    #[test]
    fn open_with_pick_closes_chooser() {
        let mut app = browse();
        let cx = EventCx::new();
        app.open_with = Some(id_of(&app, "notes.txt"));

        app.on_event(
            UiEvent::synthetic_click("openwith:org.gnome.gedit.desktop"),
            &cx,
        );
        assert!(app.open_with.is_none());
    }

    // ---- file operations -------------------------------------------------

    /// Rename pre-fills the field with the entry's name and selects it
    /// so typing replaces it, and focuses the field.
    #[test]
    fn begin_rename_prefills_and_selects_name() {
        let mut app = browse();
        let notes = id_of(&app, "notes.txt");
        app.begin_rename(notes);
        let prompt = app.prompt.as_ref().expect("prompt open");
        assert_eq!(prompt.value, "notes.txt");
        assert!(matches!(prompt.kind, PromptKind::Rename { target, .. } if target == notes));
        let range = prompt.selection.range.as_ref().expect("name selected");
        assert_eq!(range.head.byte, "notes.txt".len());
        assert!(app.focus_requests.contains(&"prompt-field".to_string()));
    }

    /// New Folder opens an empty prompt and focuses the field.
    #[test]
    fn begin_new_folder_opens_empty_prompt() {
        let mut app = browse();
        app.begin_new_folder();
        let prompt = app.prompt.as_ref().expect("prompt open");
        assert!(prompt.value.is_empty());
        assert!(matches!(prompt.kind, PromptKind::NewFolder));
        assert!(app.focus_requests.contains(&"prompt-field".to_string()));
    }

    /// While the prompt is open, no hotkeys are registered — editing
    /// keys reach the field instead of firing browser actions.
    #[test]
    fn prompt_open_suppresses_hotkeys() {
        let mut app = browse();
        assert!(!app.hotkeys().is_empty());
        app.begin_new_folder();
        assert!(app.hotkeys().is_empty());
    }

    /// Committing an invalid (here empty) name keeps the prompt open so
    /// the user can fix it rather than silently doing nothing.
    #[test]
    fn commit_invalid_name_keeps_prompt_open() {
        let mut app = browse();
        app.begin_new_folder(); // empty value
        app.commit_prompt();
        assert!(app.prompt.is_some(), "empty name keeps the prompt open");
    }

    /// Committing a rename to the unchanged name just closes the prompt
    /// — no operation to run.
    #[test]
    fn commit_unchanged_rename_closes_quietly() {
        let mut app = browse();
        let notes = id_of(&app, "notes.txt");
        app.begin_rename(notes); // value == "notes.txt" == original
        app.commit_prompt();
        assert!(app.prompt.is_none());
        assert!(app.op_error.is_none());
    }

    /// The rename prompt is modal: unrelated clicks are swallowed and
    /// the field stays up; Cancel closes it.
    #[test]
    fn prompt_modal_swallows_then_cancels() {
        let mut app = browse();
        let cx = EventCx::new();
        app.set_search("x".into());
        app.begin_rename(id_of(&app, "notes.txt"));

        app.on_event(UiEvent::synthetic_click("search-clear"), &cx);
        assert!(app.prompt.is_some(), "modal swallowed the click");
        assert!(app.search_term().is_some(), "and didn't clear the search");

        app.on_event(UiEvent::synthetic_click("prompt:cancel"), &cx);
        assert!(app.prompt.is_none());
    }

    /// "Rename…" in the context menu opens the prompt for that entry.
    #[test]
    fn context_rename_opens_prompt() {
        let mut app = browse();
        let cx = EventCx::new();
        let notes = id_of(&app, "notes.txt");
        app.open_context_menu(notes, (0.0, 0.0));

        app.on_event(UiEvent::synthetic_click("ctx:rename"), &cx);
        assert!(app.context_menu.is_none());
        assert!(matches!(
            app.prompt.as_ref().map(|p| &p.kind),
            Some(PromptKind::Rename { target, .. }) if *target == notes
        ));
    }

    /// "Delete permanently…" opens a confirmation modal that swallows
    /// stray clicks until cancelled — it never deletes without confirm.
    #[test]
    fn context_delete_confirms_then_cancels() {
        let mut app = browse();
        let cx = EventCx::new();
        let notes = id_of(&app, "notes.txt");
        app.open_context_menu(notes, (0.0, 0.0));

        app.on_event(UiEvent::synthetic_click("ctx:delete"), &cx);
        assert_eq!(app.confirm_delete, Some(vec![notes]));
        assert!(app.context_menu.is_none());

        app.set_search("y".into());
        app.on_event(UiEvent::synthetic_click("search-clear"), &cx);
        assert_eq!(
            app.confirm_delete,
            Some(vec![notes]),
            "modal swallowed the click"
        );
        assert!(app.search_term().is_some());

        app.on_event(UiEvent::synthetic_click("confirm-delete:cancel"), &cx);
        assert!(app.confirm_delete.is_none());
    }

    /// A bulk action spans the whole multi-selection (listing order) when
    /// its anchor is part of it, and just the anchor otherwise.
    #[test]
    fn bulk_targets_span_the_marked_selection() {
        let mut app = browse();
        mark(&mut app, "notes.txt");
        mark(&mut app, "photo.jxr");
        let notes = id_of(&app, "notes.txt");
        let photo = id_of(&app, "photo.jxr");

        let mut spanned = app.bulk_targets(notes);
        spanned.sort_unstable();
        let mut both = vec![notes, photo];
        both.sort_unstable();
        assert_eq!(spanned, both, "anchor in the set → whole set");

        let docs = id_of(&app, "docs");
        assert_eq!(
            app.bulk_targets(docs),
            vec![docs],
            "anchor outside the set → just the anchor"
        );
    }

    /// Right-clicking a row already in the multi-selection keeps the set
    /// (so the menu acts on all of it); right-clicking outside collapses.
    #[test]
    fn context_menu_keeps_or_collapses_selection() {
        let mut app = browse();
        mark(&mut app, "notes.txt");
        mark(&mut app, "photo.jxr");
        let notes = id_of(&app, "notes.txt");

        app.open_context_menu(notes, (0.0, 0.0));
        assert_eq!(app.marked.len(), 2, "right-click inside keeps the set");

        let docs = id_of(&app, "docs");
        app.open_context_menu(docs, (0.0, 0.0));
        assert!(app.marked.is_empty(), "right-click outside collapses");
        assert_eq!(app.selected.map(|(id, _)| id), Some(docs));
    }

    /// "Delete permanently…" on a multi-selection confirms the whole set.
    #[test]
    fn context_delete_targets_whole_selection() {
        let mut app = browse();
        let cx = EventCx::new();
        mark(&mut app, "notes.txt");
        mark(&mut app, "photo.jxr");
        let notes = id_of(&app, "notes.txt");

        app.open_context_menu(notes, (0.0, 0.0));
        app.on_event(UiEvent::synthetic_click("ctx:delete"), &cx);
        let confirmed = app.confirm_delete.expect("confirmation opened");
        assert_eq!(confirmed.len(), 2, "both marked entries are confirmed");
    }

    /// The directory-background menu's "New folder" opens the prompt.
    #[test]
    fn background_menu_new_folder_opens_prompt() {
        let mut app = browse();
        let cx = EventCx::new();
        app.open_background_menu((10.0, 10.0));
        assert_eq!(
            app.context_menu.map(|m| m.target),
            Some(ContextTarget::Background)
        );

        app.on_event(UiEvent::synthetic_click("ctx:new-folder"), &cx);
        assert!(app.context_menu.is_none());
        assert!(matches!(
            app.prompt.as_ref().map(|p| &p.kind),
            Some(PromptKind::NewFolder)
        ));
    }

    /// A failed op surfaces a modal error notice that swallows clicks
    /// until closed.
    #[test]
    fn op_error_modal_blocks_then_closes() {
        let mut app = browse();
        let cx = EventCx::new();
        app.set_search("z".into());
        app.apply_op_outcome(crate::ops::OpOutcome {
            dir: app.cwd.clone(),
            select: None,
            error: Some("a file with that name already exists".into()),
        });
        assert!(app.op_error.is_some());

        app.on_event(UiEvent::synthetic_click("search-clear"), &cx);
        assert!(app.op_error.is_some(), "modal swallowed the click");
        assert!(app.search_term().is_some());

        app.on_event(UiEvent::synthetic_click("op-error:close"), &cx);
        assert!(app.op_error.is_none());
    }

    /// A successful op in the current directory triggers a refresh that
    /// focuses the new/renamed entry by name; one targeting another
    /// directory leaves the view alone.
    #[test]
    fn op_outcome_refreshes_only_current_dir() {
        let mut app = browse();
        app.apply_op_outcome(crate::ops::OpOutcome {
            dir: app.cwd.clone(),
            select: Some(OsString::from("fresh")),
            error: None,
        });
        assert_eq!(
            app.pending_select.as_deref(),
            Some(std::ffi::OsStr::new("fresh"))
        );

        let mut other = browse();
        other.apply_op_outcome(crate::ops::OpOutcome {
            dir: PathBuf::from("/test/elsewhere"),
            select: Some(OsString::from("ignored")),
            error: None,
        });
        assert!(
            other.pending_select.is_none(),
            "other-dir op doesn't refresh"
        );
    }

    /// Navigating away drops a pending prompt / confirmation — their
    /// ids belong to the old listing.
    #[test]
    fn navigation_clears_prompt_and_confirm() {
        let mut app = browse();
        let notes = id_of(&app, "notes.txt");
        app.begin_rename(notes);
        app.confirm_delete = Some(vec![notes]);

        app.navigate(PathBuf::from("/test/elsewhere"), None);
        assert!(app.prompt.is_none());
        assert!(app.confirm_delete.is_none());
    }
}
