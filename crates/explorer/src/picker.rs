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

use std::path::PathBuf;
use std::sync::Arc;

use damascene_core::prelude::*;
use damascene_core::selection::Selection;
use damascene_core::widgets::text_input::{self, TextInputOpts};
use damascene_core::{App, BuildCx, EventCx, KeyChord, UiEvent, UiEventKind, UiKey};
use explorer_io::Pool;

use crate::app::{scaffold, ExplorerApp, FileActivation};

/// What the dialog is choosing. Mirrors the portal's OpenFile /
/// SaveFile split (`directory` is OpenFile's folder-choosing option).
pub enum PickerKind {
    /// Pick an existing file (or folder, with `directory`).
    Open { directory: bool },
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
}

/// Called exactly once with the chosen paths; `None` is cancel.
pub type PickerReply = Box<dyn FnOnce(Option<Vec<PathBuf>>) + Send>;

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
        PickerApp {
            explorer,
            kind: request.kind,
            accept_label: request.accept_label,
            filename: request.current_name,
            selection: Selection::default(),
            reply: Some(reply),
            close,
            pool,
        }
    }

    /// The paths an accept would currently produce; `None` disables
    /// the accept button.
    fn acceptable(&self) -> Option<Vec<PathBuf>> {
        match self.kind {
            PickerKind::Open { directory: false } => {
                let (path, is_dir) = self.explorer.selected_entry_path()?;
                (!is_dir).then(|| vec![path])
            }
            // Folder mode: the selected directory if there is one,
            // otherwise the directory being browsed.
            PickerKind::Open { directory: true } => match self.explorer.selected_entry_path() {
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

    fn finish(&mut self, result: Option<Vec<PathBuf>>) {
        if let Some(reply) = self.reply.take() {
            reply(result);
        }
        (self.close)();
    }

    fn accept(&mut self) {
        if let Some(paths) = self.acceptable() {
            self.finish(Some(paths));
        }
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
        scaffold(
            column([self.explorer.page_el(cx), self.chrome_el()])
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
        )
    }

    fn hotkeys(&self) -> Vec<(KeyChord, String)> {
        let mut keys = self.explorer.hotkeys();
        keys.push((KeyChord::named(UiKey::Escape), "picker-cancel".into()));
        keys
    }

    fn on_event(&mut self, event: UiEvent, cx: &EventCx) {
        // The filename field: fold edits, accept on Enter. Key events
        // route here whenever the field is focused, so the explorer's
        // Enter/arrow hotkeys don't fire mid-typing.
        if event.target_key() == Some("picker-name") {
            if event.kind == UiEventKind::KeyDown
                && event.key_press.as_ref().is_some_and(|kp| kp.key == UiKey::Enter)
            {
                self.accept();
                return;
            }
            text_input::apply_event(&mut self.filename, &mut self.selection, "picker-name", &event);
            return;
        }
        if event.is_click_or_activate("picker-accept") {
            self.accept();
            return;
        }
        if event.is_click_or_activate("picker-cancel") || event.is_hotkey("picker-cancel") {
            self.finish(None);
            return;
        }

        self.explorer.on_event(event, cx);

        // Activating a file means "this one": accept in open mode,
        // adopt the name in save mode (GTK convention).
        for path in self.explorer.take_activated() {
            match self.kind {
                PickerKind::Open { directory: false } => {
                    if self.reply.is_some() {
                        self.finish(Some(vec![path]));
                    }
                }
                PickerKind::Open { directory: true } => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use damascene_core::{render_bundle_themed, Rect, Theme};

    #[test]
    fn picker_tree_lints_clean() {
        let explorer = crate::app::tests::test_app();
        let picker = PickerApp::new(
            PickerRequest {
                kind: PickerKind::Save,
                accept_label: "Save".into(),
                start_dir: PathBuf::from("/test/somewhere"),
                current_name: "untitled.txt".into(),
            },
            explorer,
            None,
            Box::new(|_| {}),
            Arc::new(|| {}),
        );
        let theme = Theme::default();
        let (w, h) = (1500.0, 950.0);
        let diag = damascene_core::HostDiagnostics::default();
        let cx = BuildCx::new(&theme)
            .with_viewport(w, h)
            .with_diagnostics(&diag);
        let mut tree = picker.build(&cx);
        let bundle = render_bundle_themed(&mut tree, Rect::new(0.0, 0.0, w, h), &theme);
        let findings: Vec<String> = bundle
            .lint
            .findings
            .iter()
            .map(|f| format!("{f:?}"))
            .collect();
        assert!(findings.is_empty(), "lint findings: {findings:#?}");
    }

    #[test]
    fn save_accept_paths() {
        let explorer = crate::app::tests::test_app();
        let picked: Arc<std::sync::Mutex<Option<Option<Vec<PathBuf>>>>> = Default::default();
        let picked2 = picked.clone();
        let mut picker = PickerApp::new(
            PickerRequest {
                kind: PickerKind::Save,
                accept_label: "Save".into(),
                start_dir: PathBuf::from("/test/somewhere"),
                current_name: "out.png".into(),
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
            picked.lock().unwrap().clone(),
            Some(Some(vec![PathBuf::from("/test/somewhere/fine.txt")]))
        );
        // Dropping after an answer must not answer again (the reply
        // was consumed).
        drop(picker);
        assert_eq!(
            picked.lock().unwrap().clone(),
            Some(Some(vec![PathBuf::from("/test/somewhere/fine.txt")]))
        );
    }
}
