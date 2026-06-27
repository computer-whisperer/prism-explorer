//! The portal file-picker [`App`]: the full explorer browsing page
//! with chooser chrome underneath — a filename field in save mode,
//! Cancel, and the caller's accept label.
//!
//! One picker instance backs one portal request: the FileChooser
//! service builds a [`PickerRequest`] from the D-Bus options, wraps it
//! in a window spec, and blocks on the reply. The picker answers
//! exactly once — accept, cancel, Escape, or simply being dropped when
//! the user closes the window (the reply channel disconnecting reads
//! as cancel on the service side).

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use damascene_core::prelude::*;
use damascene_core::selection::Selection;
use damascene_core::widgets::dialog::{
    dialog, dialog_description, dialog_footer, dialog_header, dialog_title,
};
use damascene_core::widgets::select::{self, select_menu, select_trigger};
use damascene_core::widgets::text_input::{self, TextInputOpts};
use damascene_core::{App, BuildCx, EventCx, KeyChord, UiEvent, UiEventKind, UiKey};
use explorer_io::{EntryKind, Pool};

use crate::app::{scaffold, ExplorerApp, FileActivation};
use crate::model::FileFilter;

/// What the dialog is choosing. Mirrors the portal's OpenFile /
/// SaveFile split (`directory` is OpenFile's folder-choosing option).
pub enum PickerKind {
    /// Pick an existing file (or folder, with `directory`). With
    /// `multiple`, accept returns every marked file.
    Open { directory: bool, multiple: bool },
    /// Pick a destination: a directory plus a (typed) file name.
    Save,
}

/// Everything the chrome needs, distilled from the portal options by
/// the FileChooser service.
pub struct PickerRequest {
    pub kind: PickerKind,
    /// Caller-supplied accept-button label (portal `accept_label`),
    /// already defaulted ("Open"/"Save") and stripped of the `_`
    /// mnemonic markers GTK callers embed.
    pub accept_label: String,
    pub start_dir: PathBuf,
    /// Save mode: pre-filled file name (portal `current_name`).
    pub current_name: String,
    /// File-type filters (portal `filters`), already split into glob /
    /// mimetype alternatives. Empty means list everything.
    pub filters: Vec<FileFilter>,
    /// Index into `filters` to start on (portal `current_filter`,
    /// already resolved by the service; ignored when out of range).
    pub current_filter: usize,
}

/// The result of an accepted picker: the chosen paths plus the filter
/// that was active at accept time (so the FileChooser service can both
/// remember it per-app and, later, report it back to the caller). Empty
/// `filter` when the picker had no filters.
pub struct PickerOutcome {
    pub paths: Vec<PathBuf>,
    pub filter: Option<FileFilter>,
}

/// Called exactly once with the outcome; `None` is cancel.
pub type PickerReply = Box<dyn FnOnce(Option<PickerOutcome>) + Send>;

pub struct PickerApp {
    explorer: ExplorerApp,
    kind: PickerKind,
    accept_label: String,
    /// Save mode: the file-name field's value (app-owned, per the
    /// text_input contract).
    filename: String,
    /// Global selection state — carets/selection for the filename
    /// field live here, surfaced through `App::selection`.
    selection: Selection,
    /// File-type filters the caller supplied; the active one is
    /// installed on the wrapped explorer. Empty hides the selector.
    filters: Vec<FileFilter>,
    filter_idx: usize,
    /// The filter select's popover-open flag.
    filter_open: bool,
    /// Save mode: when accepting would overwrite an existing file, the
    /// would-be result is parked here and a modal confirm dialog shown
    /// instead of answering. `Replace` finishes with it; `Cancel`
    /// clears it.
    pending_overwrite: Option<Vec<PathBuf>>,
    reply: Option<PickerReply>,
    /// Asks the host to close this picker's window (posts
    /// `HostCommand::CloseWindow` with the token the service chose).
    close: Arc<dyn Fn() + Send + Sync>,
    /// The picker's *dedicated* IO pool — generation cancellation is
    /// pool-wide, so a picker must never share a pool with another
    /// window (its navigations would cancel that window's jobs). Shut
    /// down on drop; `None` only in tests.
    pool: Option<Pool>,
}

impl PickerApp {
    pub fn new(
        request: PickerRequest,
        mut explorer: ExplorerApp,
        pool: Option<Pool>,
        reply: PickerReply,
        close: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        explorer.set_file_activation(FileActivation::Collect);
        explorer.set_search_visible(false);
        let filter_idx = request
            .current_filter
            .min(request.filters.len().saturating_sub(1));
        if let Some(filter) = request.filters.get(filter_idx) {
            explorer.set_file_filter(Some(filter.clone()));
        }
        PickerApp {
            explorer,
            kind: request.kind,
            accept_label: request.accept_label,
            filename: request.current_name,
            selection: Selection::default(),
            filters: request.filters,
            filter_idx,
            filter_open: false,
            pending_overwrite: None,
            reply: Some(reply),
            close,
            pool,
        }
    }

    /// The paths an accept would currently produce; `None` disables
    /// the accept button.
    fn acceptable(&self) -> Option<Vec<PathBuf>> {
        match self.kind {
            PickerKind::Open {
                directory: false,
                multiple,
            } => {
                // Multi-select returns every marked file; with nothing
                // marked it falls back to the cursor's file.
                if multiple {
                    let marked = self.explorer.marked_file_paths();
                    if !marked.is_empty() {
                        return Some(marked);
                    }
                }
                let (path, is_dir) = self.explorer.selected_entry_path()?;
                (!is_dir).then(|| vec![path])
            }
            // Folder mode: the selected directory if there is one,
            // otherwise the directory being browsed.
            PickerKind::Open {
                directory: true, ..
            } => match self.explorer.selected_entry_path() {
                Some((path, true)) => Some(vec![path]),
                _ => Some(vec![self.explorer.cwd_path().to_path_buf()]),
            },
            PickerKind::Save => {
                let name = self.filename.trim();
                if name.is_empty() || name.contains('/') {
                    return None;
                }
                Some(vec![self.explorer.cwd_path().join(name)])
            }
        }
    }

    /// The filter active at accept, used to tag the outcome. `None` when
    /// the picker has no filters.
    fn active_filter(&self) -> Option<FileFilter> {
        self.filters.get(self.filter_idx).cloned()
    }

    fn finish(&mut self, paths: Option<Vec<PathBuf>>) {
        if let Some(reply) = self.reply.take() {
            let outcome = paths.map(|paths| PickerOutcome {
                paths,
                filter: self.active_filter(),
            });
            reply(outcome);
        }
        (self.close)();
    }

    fn accept(&mut self) {
        let Some(paths) = self.acceptable() else {
            return;
        };
        // Save mode: warn before clobbering an existing file. The check
        // reads the in-memory listing (no extra stat) and so is
        // best-effort — a not-yet-streamed name in a huge directory
        // won't trip it. Directories aren't overwrite targets; let the
        // app surface that error itself.
        if matches!(self.kind, PickerKind::Save) {
            let name = self.filename.trim();
            if matches!(self.explorer.existing_kind(OsStr::new(name)), Some(k) if k != EntryKind::Dir)
            {
                self.pending_overwrite = Some(paths);
                return;
            }
        }
        self.finish(Some(paths));
    }

    /// Modal "this file exists" confirmation, stacked over the picker
    /// while `pending_overwrite` is set.
    fn overwrite_dialog(&self) -> El {
        let name = self.filename.trim();
        dialog(
            "overwrite",
            [
                dialog_header([
                    dialog_title("Replace file?"),
                    dialog_description(format!(
                        "\u{201c}{name}\u{201d} already exists in this folder. \
                         Replacing it overwrites its contents."
                    )),
                ]),
                dialog_footer([
                    button("Cancel").ghost().key("overwrite:cancel"),
                    button("Replace").primary().key("overwrite:replace"),
                ]),
            ],
        )
    }

    fn chrome_el(&self) -> El {
        let mut items: Vec<El> = Vec::new();
        if matches!(self.kind, PickerKind::Save) {
            items.push(text("Name").caption().muted());
            items.push(
                text_input::text_input_with(
                    "picker-name",
                    &self.filename,
                    &self.selection,
                    TextInputOpts {
                        placeholder: Some("file name"),
                        ..TextInputOpts::default()
                    },
                )
                .width(Size::Fixed(360.0)),
            );
        }
        items.push(spacer());
        if let Some(active) = self.filters.get(self.filter_idx) {
            // Fixed width: the trigger otherwise Fills, fighting the
            // spacer for the whole row (names ellipsize).
            items.push(select_trigger("picker-filter", &active.name).width(Size::Fixed(200.0)));
        }
        items.push(button("Cancel").ghost().key("picker-cancel"));
        let accept = button(self.accept_label.clone())
            .primary()
            .key("picker-accept");
        items.push(if self.acceptable().is_some() {
            accept
        } else {
            accept.disabled()
        });
        row(items)
            .gap(tokens::SPACE_3)
            .align(Align::Center)
            .width(Size::Fill(1.0))
    }
}

impl Drop for PickerApp {
    /// The host dropped us without an answer (user closed the window):
    /// that is a cancel. Either way the dialog's IO pool dies with it.
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            reply(None);
        }
        if let Some(pool) = &self.pool {
            pool.shutdown();
        }
    }
}

impl App for PickerApp {
    fn before_build(&mut self) {
        self.explorer.before_build();
    }

    fn build(&self, cx: &BuildCx) -> El {
        let mut layers: Vec<El> = vec![column([self.explorer.page_el(cx), self.chrome_el()])
            .gap(tokens::SPACE_3)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))];
        // Overlays stack over the page in z-order: the filter menu
        // (popover, anchored to its trigger by the shared key), then
        // the overwrite confirmation (modal, on top of everything).
        if self.filter_open {
            let options = self
                .filters
                .iter()
                .enumerate()
                .map(|(i, f)| (i.to_string(), f.name.clone()));
            layers.push(select_menu("picker-filter", options));
        }
        if self.pending_overwrite.is_some() {
            layers.push(self.overwrite_dialog());
        }
        let page = if layers.len() == 1 {
            layers.pop().unwrap()
        } else {
            stack(layers).width(Size::Fill(1.0)).height(Size::Fill(1.0))
        };
        scaffold(page)
    }

    fn hotkeys(&self) -> Vec<(KeyChord, String)> {
        let mut keys = self.explorer.hotkeys();
        keys.push((KeyChord::named(UiKey::Escape), "picker-cancel".into()));
        keys
    }

    fn on_event(&mut self, event: UiEvent, cx: &EventCx) {
        // The overwrite confirmation is modal: while it's up, only its
        // own controls do anything — every other event is swallowed so
        // nothing reaches the name field or the explorer beneath it.
        if self.pending_overwrite.is_some() {
            if event.is_click_or_activate("overwrite:replace") {
                if let Some(paths) = self.pending_overwrite.take() {
                    self.finish(Some(paths));
                }
            } else if event.is_click_or_activate("overwrite:cancel")
                || event.is_click_or_activate("overwrite:dismiss")
                || event.is_hotkey("picker-cancel")
            {
                self.pending_overwrite = None;
            }
            return;
        }
        // The filename field: fold edits, accept on Enter. Key events
        // route here whenever the field is focused, so the explorer's
        // Enter/arrow hotkeys don't fire mid-typing.
        if event.target_key() == Some("picker-name") {
            if event.kind == UiEventKind::KeyDown
                && event
                    .key_press
                    .as_ref()
                    .is_some_and(|kp| kp.key == UiKey::Enter)
            {
                self.accept();
                return;
            }
            text_input::apply_event(
                &mut self.filename,
                &mut self.selection,
                &event,
                "picker-name",
            );
            return;
        }
        // The filter select: toggle / dismiss / pick, folded into
        // (filter_idx, filter_open). A pick re-filters the listing.
        let prev = self.filter_idx;
        let filters = self.filters.len();
        if select::apply_event(
            &mut self.filter_idx,
            &mut self.filter_open,
            &event,
            "picker-filter",
            |s| s.parse().ok().filter(|&i: &usize| i < filters),
        ) {
            if self.filter_idx != prev {
                self.explorer
                    .set_file_filter(self.filters.get(self.filter_idx).cloned());
            }
            return;
        }
        if event.is_click_or_activate("picker-accept") {
            self.accept();
            return;
        }
        if event.is_click_or_activate("picker-cancel") || event.is_hotkey("picker-cancel") {
            // Escape dismisses an open overlay before the dialog: the
            // filter menu first, then the explorer's location bar (the
            // Cancel button, not being Escape, always cancels outright).
            if self.filter_open {
                self.filter_open = false;
                return;
            }
            if event.is_hotkey("picker-cancel") && self.explorer.dismiss_location() {
                return;
            }
            self.finish(None);
            return;
        }

        self.explorer.on_event(event, cx);

        // Activating a file means "this one": accept in open mode,
        // adopt the name in save mode (GTK convention).
        for path in self.explorer.take_activated() {
            match self.kind {
                // Double-click / Enter commits the activated file alone,
                // even in multi-select — the accept button is what
                // returns the marked set.
                PickerKind::Open {
                    directory: false, ..
                } => {
                    if self.reply.is_some() {
                        self.finish(Some(vec![path]));
                    }
                }
                PickerKind::Open {
                    directory: true, ..
                } => {}
                PickerKind::Save => {
                    if let Some(name) = path.file_name() {
                        self.filename = name.to_string_lossy().into_owned();
                    }
                }
            }
        }
    }

    fn selection(&self) -> Selection {
        self.selection.clone()
    }

    fn drain_scroll_requests(&mut self) -> Vec<damascene_core::scroll::ScrollRequest> {
        self.explorer.drain_scroll_requests()
    }
}

impl crate::host::HostApp for PickerApp {
    fn gpu_setup(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        crate::host::HostApp::gpu_setup(&mut self.explorer, device, queue);
    }

    fn before_paint(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: damascene_core::Rect,
        scale_factor: f32,
    ) {
        crate::host::HostApp::before_paint(
            &mut self.explorer,
            device,
            queue,
            viewport,
            scale_factor,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The picker's tree-lint coverage lives in `crate::fixtures`
    // (`all_scenes_lint_clean` renders the picker_open / picker_save
    // scenes alongside the browser ones).

    /// Captures the picker's single reply: outer `Option` = answered?,
    /// inner = the outcome (`None` = cancelled).
    type ReplySink = Arc<std::sync::Mutex<Option<Option<PickerOutcome>>>>;

    /// The paths the picker answered with, flattened for assertions:
    /// outer `None` = not answered yet, inner `None` = cancelled.
    fn answered_paths(sink: &ReplySink) -> Option<Option<Vec<PathBuf>>> {
        sink.lock()
            .unwrap()
            .as_ref()
            .map(|answer| answer.as_ref().map(|o| o.paths.clone()))
    }

    /// The name of the filter the picker answered with, if any.
    fn answered_filter(sink: &ReplySink) -> Option<String> {
        sink.lock()
            .unwrap()
            .as_ref()
            .and_then(|answer| answer.as_ref())
            .and_then(|o| o.filter.as_ref())
            .map(|f| f.name.clone())
    }

    /// Trigger click opens the menu; picking an option closes it and
    /// refilters the wrapped explorer's listing (dirs always stay).
    #[test]
    fn filter_switch_refilters_listing() {
        let explorer = crate::app::fixtures::browse();
        let mut picker = PickerApp::new(
            PickerRequest {
                kind: PickerKind::Open {
                    directory: false,
                    multiple: false,
                },
                accept_label: "Open".into(),
                start_dir: PathBuf::from("/test/somewhere"),
                current_name: String::new(),
                filters: vec![
                    FileFilter {
                        name: "Images".into(),
                        globs: vec!["*.jxr".into()],
                        mimes: vec![],
                    },
                    FileFilter {
                        name: "All files".into(),
                        globs: vec!["*".into()],
                        mimes: vec![],
                    },
                ],
                current_filter: 0,
            },
            explorer,
            None,
            Box::new(|_| {}),
            Arc::new(|| {}),
        );
        let cx = EventCx::new();
        assert_eq!(picker.explorer.visible_names(), ["docs", "photo.jxr"]);

        picker.on_event(UiEvent::synthetic_click("picker-filter"), &cx);
        assert!(picker.filter_open);

        picker.on_event(UiEvent::synthetic_click("picker-filter:option:1"), &cx);
        assert!(!picker.filter_open);
        assert_eq!(picker.filter_idx, 1);
        assert_eq!(
            picker.explorer.visible_names(),
            ["docs", "notes.txt", "photo.jxr"]
        );
    }

    /// Helper: a Save picker over the synthetic browse() listing
    /// (which has `notes.txt` (file) and `docs` (dir)), capturing its
    /// one reply.
    fn save_picker(name: &str) -> (PickerApp, ReplySink) {
        let picked: ReplySink = Default::default();
        let sink = picked.clone();
        let picker = PickerApp::new(
            PickerRequest {
                kind: PickerKind::Save,
                accept_label: "Save".into(),
                start_dir: PathBuf::from("/test/somewhere"),
                current_name: name.into(),
                filters: Vec::new(),
                current_filter: 0,
            },
            crate::app::fixtures::browse(),
            None,
            Box::new(move |r| *sink.lock().unwrap() = Some(r)),
            Arc::new(|| {}),
        );
        (picker, picked)
    }

    /// A `multiple` Open picker returns every marked file; with nothing
    /// marked it falls back to the cursor's file.
    #[test]
    fn multiple_open_returns_marked_files() {
        let mut explorer = crate::app::fixtures::browse();
        crate::app::fixtures::mark(&mut explorer, "notes.txt");
        crate::app::fixtures::mark(&mut explorer, "photo.jxr");
        let picker = PickerApp::new(
            PickerRequest {
                kind: PickerKind::Open {
                    directory: false,
                    multiple: true,
                },
                accept_label: "Open".into(),
                start_dir: PathBuf::from("/test/somewhere"),
                current_name: String::new(),
                filters: Vec::new(),
                current_filter: 0,
            },
            explorer,
            None,
            Box::new(|_| {}),
            Arc::new(|| {}),
        );
        assert_eq!(
            picker.acceptable(),
            Some(vec![
                PathBuf::from("/test/somewhere/notes.txt"),
                PathBuf::from("/test/somewhere/photo.jxr"),
            ])
        );
    }

    /// On accept, the outcome carries the filter that was active — the
    /// signal the FileChooser service records per-app.
    #[test]
    fn accept_reports_active_filter() {
        let mut explorer = crate::app::fixtures::browse();
        crate::app::fixtures::mark(&mut explorer, "photo.jxr");
        let picked: ReplySink = Default::default();
        let sink = picked.clone();
        let mut picker = PickerApp::new(
            PickerRequest {
                kind: PickerKind::Open {
                    directory: false,
                    multiple: true,
                },
                accept_label: "Open".into(),
                start_dir: PathBuf::from("/test/somewhere"),
                current_name: String::new(),
                filters: vec![
                    FileFilter {
                        name: "Images".into(),
                        globs: vec!["*.jxr".into()],
                        mimes: vec![],
                    },
                    FileFilter {
                        name: "All files".into(),
                        globs: vec!["*".into()],
                        mimes: vec![],
                    },
                ],
                current_filter: 0,
            },
            explorer,
            None,
            Box::new(move |r| *sink.lock().unwrap() = Some(r)),
            Arc::new(|| {}),
        );
        let cx = EventCx::new();
        // Switch to the second filter, then accept the marked file.
        picker.on_event(UiEvent::synthetic_click("picker-filter"), &cx);
        picker.on_event(UiEvent::synthetic_click("picker-filter:option:1"), &cx);
        picker.on_event(UiEvent::synthetic_click("picker-accept"), &cx);
        assert_eq!(
            answered_paths(&picked),
            Some(Some(vec![PathBuf::from("/test/somewhere/photo.jxr")]))
        );
        assert_eq!(answered_filter(&picked), Some("All files".to_string()));
    }

    #[test]
    fn save_over_existing_file_confirms_then_replaces() {
        let (mut picker, picked) = save_picker("notes.txt");
        let cx = EventCx::new();
        // Accept parks the result behind the modal — no answer yet.
        picker.on_event(UiEvent::synthetic_click("picker-accept"), &cx);
        assert!(picker.pending_overwrite.is_some());
        assert!(picked.lock().unwrap().is_none());
        // Replace confirms with the parked path.
        picker.on_event(UiEvent::synthetic_click("overwrite:replace"), &cx);
        assert_eq!(
            answered_paths(&picked),
            Some(Some(vec![PathBuf::from("/test/somewhere/notes.txt")]))
        );
    }

    #[test]
    fn save_overwrite_cancel_keeps_picker_open() {
        let (mut picker, picked) = save_picker("notes.txt");
        let cx = EventCx::new();
        picker.on_event(UiEvent::synthetic_click("picker-accept"), &cx);
        assert!(picker.pending_overwrite.is_some());
        // Cancel dismisses the dialog without answering; the picker
        // stays open (reply not consumed).
        picker.on_event(UiEvent::synthetic_click("overwrite:cancel"), &cx);
        assert!(picker.pending_overwrite.is_none());
        assert!(picked.lock().unwrap().is_none());
        assert!(picker.reply.is_some());
    }

    #[test]
    fn save_new_name_answers_without_confirm() {
        let (mut picker, picked) = save_picker("brand-new.txt");
        picker.on_event(UiEvent::synthetic_click("picker-accept"), &EventCx::new());
        assert!(picker.pending_overwrite.is_none());
        assert_eq!(
            answered_paths(&picked),
            Some(Some(vec![PathBuf::from("/test/somewhere/brand-new.txt")]))
        );
    }

    #[test]
    fn save_accept_paths() {
        let explorer = crate::app::fixtures::browse();
        let picked: ReplySink = Default::default();
        let picked2 = picked.clone();
        let mut picker = PickerApp::new(
            PickerRequest {
                kind: PickerKind::Save,
                accept_label: "Save".into(),
                start_dir: PathBuf::from("/test/somewhere"),
                current_name: "out.png".into(),
                filters: Vec::new(),
                current_filter: 0,
            },
            explorer,
            None,
            Box::new(move |r| *picked2.lock().unwrap() = Some(r)),
            Arc::new(|| {}),
        );
        assert_eq!(
            picker.acceptable(),
            Some(vec![PathBuf::from("/test/somewhere/out.png")])
        );
        picker.filename = "  ".into();
        assert_eq!(picker.acceptable(), None);
        picker.filename = "a/b".into();
        assert_eq!(picker.acceptable(), None);
        picker.filename = "fine.txt".into();
        picker.accept();
        assert_eq!(
            answered_paths(&picked),
            Some(Some(vec![PathBuf::from("/test/somewhere/fine.txt")]))
        );
        // Dropping after an answer must not answer again (the reply
        // was consumed).
        drop(picker);
        assert_eq!(
            answered_paths(&picked),
            Some(Some(vec![PathBuf::from("/test/somewhere/fine.txt")]))
        );
    }
}
