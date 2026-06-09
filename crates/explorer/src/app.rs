//! The explorer [`App`]: sidebar of places, breadcrumb navigation, a
//! virtualized directory listing, and a preview pane for the selected
//! file.
//!
//! Built for big, slow filesystems: the UI thread never touches the
//! filesystem. Listings stream in (names first, batched), metadata is
//! stat'ed lazily for rows the list actually realizes, the selected
//! file's preview decodes at the front of the queue, and navigating
//! away drops every queued job for the directory being left.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use damascene_core::prelude::*;
use damascene_core::scroll::{ScrollAlignment, ScrollRequest};
use damascene_core::{BuildCx, EventCx, KeyChord, UiEvent, UiEventKind, UiKey};
use damascene_winit_wgpu::Wakeup;

use explorer_io::{listing, stat, EntryKind, Notifier, Pool, Tier};
use explorer_previews::{Preview, Registry};

use crate::fmt;
use crate::model::{EntryId, Listing, Msg};
use crate::places::Place;

/// The host's wakeup handle, shared with every worker that posts
/// results. Delivered once the event loop exists; `None` before that.
pub type SharedWakeup = Arc<Mutex<Option<Wakeup>>>;

const ROW_H: f32 = 34.0;
const SIDEBAR_MIN: f32 = 160.0;
const SIDEBAR_MAX: f32 = 420.0;
const PREVIEW_MIN: f32 = 260.0;
const PREVIEW_MAX: f32 = 900.0;

/// Display cap for text previews — the handler already bounds the read
/// at 128 KiB; this bounds what one mono text leaf has to lay out.
const TEXT_PREVIEW_MAX_LINES: usize = 400;

enum PreviewState {
    /// Nothing selected, or a directory is.
    Empty,
    Loading {
        id: EntryId,
    },
    Ready {
        id: EntryId,
        preview: Preview,
    },
    Failed {
        id: EntryId,
        error: String,
    },
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

    /// Selected entry: stable id plus its current position in
    /// `listing.order` (kept in sync across resorts).
    selected: Option<(EntryId, usize)>,
    /// Select this name once it appears in the stream — set when
    /// navigating to a parent so the directory we came from is focused.
    pending_select: Option<OsString>,

    preview: PreviewState,
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
}

impl ExplorerApp {
    pub fn new(start: PathBuf, pool: Pool, notifier: Notifier, registry: Arc<Registry>) -> Self {
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
            selected: None,
            pending_select: None,
            preview: PreviewState::Empty,
            preview_inflight: None,
            preview_wanted: None,
            stat_requested: Arc::new(Mutex::new(HashSet::new())),
            scroll_requests: RefCell::new(Vec::new()),
            sidebar_w: 220.0,
            preview_w: 420.0,
            sidebar_drag: ResizeDrag::default(),
            preview_drag: ResizeDrag::default(),
        };
        app.spawn_places_probe();
        app.navigate(start, None);
        app
    }

    fn spawn_places_probe(&self) {
        let tx = self.tx.clone();
        let notify = self.notifier.clone();
        self.pool.submit(Tier::Visible, move || {
            let _ = tx.send(Msg::Places(crate::places::probe()));
            notify();
        });
    }

    /// Leave for `dir`: invalidate all queued work, reset per-directory
    /// state, and start a streaming listing.
    fn navigate(&mut self, dir: PathBuf, select: Option<OsString>) {
        let generation = self.pool.bump_generation();
        tracing::info!(dir = %dir.display(), generation, "navigate");
        self.cwd = dir.clone();
        self.listing = Listing::new(dir.clone(), generation);
        self.selected = None;
        self.pending_select = select;
        self.preview = PreviewState::Empty;
        self.preview_inflight = None;
        self.preview_wanted = None;
        self.stat_requested.lock().unwrap().clear();
        self.scroll_requests.borrow_mut().push(ScrollRequest::new(
            "list",
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

    fn select_id(&mut self, id: EntryId) {
        let Some(pos) = self.listing.pos_of(id) else {
            return;
        };
        self.selected = Some((id, pos));
        self.scroll_requests.borrow_mut().push(ScrollRequest::new(
            "list",
            pos,
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

    fn select_pos(&mut self, pos: usize) {
        if let Some(&id) = self.listing.order.get(pos) {
            self.select_id(id);
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
            // Detached; xdg-open does its own (possibly slow) IO in its
            // own process.
            if let Err(e) = std::process::Command::new("xdg-open").arg(&path).spawn() {
                tracing::warn!(path = %path.display(), error = %e, "xdg-open failed");
            }
        }
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
            let result = registry.load(&path).map_err(|e| format!("{e:#}"));
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
        self.listing.rebuild_order(self.show_hidden);
        self.remap_selection();
    }

    /// Re-derive the selection's position after `order` changed; drop
    /// the selection if its entry was filtered out.
    fn remap_selection(&mut self) {
        if let Some((id, _)) = self.selected {
            self.selected = self.listing.pos_of(id).map(|pos| (id, pos));
            if self.selected.is_none() {
                self.preview = PreviewState::Empty;
                self.preview_wanted = None;
            }
        }
    }

    fn selected_id(&self) -> Option<EntryId> {
        self.selected.map(|(id, _)| id)
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

    fn sidebar_el(&self) -> El {
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
            .width(Size::Fixed(self.sidebar_w))
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
                    breadcrumb_link(label)
                        .key(format!("crumb:{i}"))
                        .tooltip(path.display().to_string()),
                ));
                items.push(breadcrumb_separator());
            }
        }

        toolbar([
            icon_button("chevron-up")
                .key("up")
                .tooltip("parent directory (Backspace)"),
            breadcrumb([breadcrumb_list(items)]),
            spacer(),
            color_mode_badge(cx),
        ])
    }

    fn list_el(&self) -> El {
        if let Some(err) = &self.listing.error {
            return column([icon("alert-circle"), text(err.clone()).muted()])
                .gap(tokens::SPACE_3)
                .align(Align::Center)
                .justify(Justify::Center)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0));
        }
        if self.listing.order.is_empty() {
            let label: El = if self.listing.complete {
                text("empty directory").muted()
            } else {
                row([spinner(), text("listing…").muted()])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center)
            };
            return column([label])
                .align(Align::Center)
                .justify(Justify::Center)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0));
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

        virtual_list(order.len(), ROW_H, move |i| {
            let id = order[i];
            let entries = entries.lock().unwrap();
            let e = &entries[id as usize];

            // Realized row without metadata: queue a stat, once.
            if e.meta.is_none() && e.meta_error.is_none() {
                let mut requested = stat_requested.lock().unwrap();
                if requested.insert(id) {
                    let path = dir.join(&e.name);
                    let tx = tx.clone();
                    let notify = notify.clone();
                    pool.submit(Tier::Visible, move || {
                        let result = stat::stat_entry(&path);
                        let _ = tx.send(Msg::Stat {
                            generation,
                            id,
                            result,
                        });
                        notify();
                    });
                }
            }

            let icon_name = match e.kind {
                EntryKind::Dir => "folder",
                EntryKind::File => "file",
                EntryKind::Symlink => "file",
                EntryKind::Other => "more-horizontal",
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
            let mut name = text(e.display.clone()).width(Size::Fill(1.0));
            if e.is_symlink {
                name = name.italic();
            }
            if e.meta_error.is_some() {
                name = name.muted();
            }

            let r = row([
                icon(icon_name)
                    .icon_size(tokens::ICON_SM)
                    .color(tokens::MUTED_FOREGROUND),
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
            .focusable();
            if Some(id) == selected_id {
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

    fn preview_pane(&self) -> El {
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
            column([spinner()])
                .align(Align::Center)
                .justify(Justify::Center)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0))
        } else {
            match &self.preview {
                PreviewState::Empty => preview_placeholder_body("file", ""),
                PreviewState::Loading { .. } => column([spinner()])
                    .align(Align::Center)
                    .justify(Justify::Center)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0)),
                PreviewState::Failed { error, .. } => {
                    preview_placeholder_body("alert-circle", error)
                }
                PreviewState::Ready { preview, .. } => match preview {
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
                    Preview::Text {
                        text: body,
                        truncated,
                    } => {
                        let mut children = vec![scroll([code_block(body.clone())])
                            .width(Size::Fill(1.0))
                            .height(Size::Fill(1.0))];
                        if *truncated {
                            children.push(text("truncated preview").caption().muted());
                        }
                        column(children)
                            .gap(tokens::SPACE_2)
                            .width(Size::Fill(1.0))
                            .height(Size::Fill(1.0))
                    }
                    Preview::Unsupported { reason } => preview_placeholder_body("file", reason),
                },
            }
        };

        card([column([header, body])
            .gap(tokens::SPACE_3)
            .padding(tokens::SPACE_4)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))])
        .width(Size::Fixed(self.preview_w))
        .height(Size::Fill(1.0))
    }

    fn status_el(&self) -> El {
        let mut left = format!("{} items", self.listing.order.len());
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

impl App for ExplorerApp {
    fn before_build(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Listing { generation, update } => {
                    if generation != self.listing.generation {
                        continue;
                    }
                    if self.listing.absorb(update, self.show_hidden) {
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
                    if self.listing.apply_stat(id, result, self.show_hidden) {
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
                        self.preview = match result {
                            Ok(preview) => PreviewState::Ready {
                                id,
                                preview: cap_text_preview(preview),
                            },
                            Err(error) => PreviewState::Failed { id, error },
                        };
                    }
                    if let Some(wanted) = self.preview_wanted.take() {
                        if wanted != id && self.selected_id() == Some(wanted) {
                            self.request_preview(wanted);
                        }
                    }
                }
                Msg::Places(places) => self.places = places,
            }
        }
    }

    fn build(&self, cx: &BuildCx) -> El {
        let content = row([
            self.sidebar_el(),
            resize_handle(Axis::Row).key("sidebar-resize"),
            card([self.list_el().padding(tokens::SPACE_2)])
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            resize_handle(Axis::Row).key("preview-resize"),
            self.preview_pane(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));

        let page = column([self.toolbar_el(cx), content, self.status_el()])
            .gap(tokens::SPACE_3)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));

        // Page scaffold (the hero-fixture idiom, as in the gallery):
        // themed background under padded content; overlay root on top.
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

    fn hotkeys(&self) -> Vec<(KeyChord, String)> {
        vec![
            (KeyChord::named(UiKey::ArrowUp), "prev".into()),
            (KeyChord::named(UiKey::ArrowDown), "next".into()),
            (KeyChord::named(UiKey::Home), "first".into()),
            (KeyChord::named(UiKey::End), "last".into()),
            (KeyChord::named(UiKey::Enter), "open".into()),
            (KeyChord::named(UiKey::Backspace), "parent".into()),
            (KeyChord::vim('k'), "prev".into()),
            (KeyChord::vim('j'), "next".into()),
            (KeyChord::vim('.'), "hidden".into()),
        ]
    }

    fn on_event(&mut self, event: UiEvent, _cx: &EventCx) {
        use damascene_core::widgets::resize_handle::Side;
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
                if event.kind == UiEventKind::Click && event.click_count >= 2 {
                    self.select_id(id);
                    self.activate_id(id);
                    return;
                }
                if event.is_click_or_activate(key) {
                    self.select_id(id);
                    return;
                }
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
        }

        if event.is_hotkey("next") {
            self.move_selection(1);
        } else if event.is_hotkey("prev") {
            self.move_selection(-1);
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
        }
    }

    fn drain_scroll_requests(&mut self) -> Vec<ScrollRequest> {
        std::mem::take(&mut *self.scroll_requests.borrow_mut())
    }
}

/// Bound what one mono text leaf has to lay out; the read itself is
/// already bounded by the handler.
fn cap_text_preview(preview: Preview) -> Preview {
    match preview {
        Preview::Text { text, truncated } => {
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
            let mut text = text;
            text.truncate(end);
            Preview::Text {
                text,
                truncated: truncated || capped,
            }
        }
        other => other,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use damascene_core::{render_bundle_themed, Rect, Theme};
    use explorer_io::listing::ListingUpdate;
    use explorer_io::RawEntry;

    /// An app with a synthetic listing — no real IO, pool jobs are
    /// dropped before any worker would run them.
    fn test_app() -> ExplorerApp {
        let pool = Pool::spawn(1, "test");
        let notifier: Notifier = Arc::new(|| {});
        let mut app = ExplorerApp::new(
            PathBuf::from("/test/somewhere"),
            pool,
            notifier,
            Arc::new(Registry::standard()),
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
        );
        app
    }

    fn lint_findings(app: &ExplorerApp) -> Vec<String> {
        let theme = Theme::default();
        let (w, h) = (1500.0, 950.0);
        let diag = damascene_core::HostDiagnostics::default();
        let cx = BuildCx::new(&theme)
            .with_viewport(w, h)
            .with_diagnostics(&diag);
        let mut tree = app.build(&cx);
        let bundle = render_bundle_themed(&mut tree, Rect::new(0.0, 0.0, w, h), &theme);
        bundle
            .lint
            .findings
            .iter()
            .map(|f| format!("{f:?}"))
            .collect()
    }

    #[test]
    fn browse_tree_lints_clean() {
        let app = test_app();
        assert_eq!(lint_findings(&app), Vec::<String>::new());
    }

    #[test]
    fn text_preview_tree_lints_clean() {
        let mut app = test_app();
        let id = app
            .listing
            .id_by_name(std::ffi::OsStr::new("notes.txt"))
            .unwrap();
        let pos = app.listing.pos_of(id).unwrap();
        app.selected = Some((id, pos));
        app.preview = PreviewState::Ready {
            id,
            preview: Preview::Text {
                text: "hello\nworld\n".into(),
                truncated: true,
            },
        };
        assert_eq!(lint_findings(&app), Vec::<String>::new());
    }

    #[test]
    fn listing_error_tree_lints_clean() {
        let mut app = test_app();
        app.listing.error = Some("opening /test/somewhere: permission denied".into());
        assert_eq!(lint_findings(&app), Vec::<String>::new());
    }

    /// Selection follows ids across a mid-stream resort: select an
    /// entry, absorb a batch that sorts ahead of it, and the selected
    /// id must be unchanged with an updated position.
    #[test]
    fn selection_survives_streaming_resort() {
        let mut app = test_app();
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
        );
        app.remap_selection();

        let (sel_id, sel_pos) = app.selected.unwrap();
        assert_eq!(sel_id, id);
        assert_eq!(app.listing.order[sel_pos], id);
        assert!(sel_pos != pos, "position should have shifted");
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
}
