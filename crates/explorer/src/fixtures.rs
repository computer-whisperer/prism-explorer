//! Canned scenes for layout review without a window.
//!
//! Each scene is an app over synthetic state (no real IO) that the
//! damascene bundle pipeline can lay out, dump, and lint — the
//! cheapest feedback loop during UI work: CPU-only, but the same
//! layout + draw-op stack the GPU consumes. The in-crate test asserts
//! every scene lints clean; `cargo run --bin dump_bundles` writes the
//! full artifact set (svg, tree dump, draw ops, shader manifest, lint)
//! per scene to `crates/explorer/out/`.

use std::path::PathBuf;
use std::sync::Arc;

use damascene_core::{
    render_bundle_themed, App, BuildCx, Bundle, EventCx, HostDiagnostics, Rect, UiEvent,
};

use crate::app;
use crate::model::FileFilter;
use crate::picker::{PickerApp, PickerKind, PickerRequest};

/// Browser windows open at 1500×950 (`main.rs`), pickers at 1100×760
/// (`filechooser.rs`) — scenes dump at the size they really open at.
const BROWSER: (f32, f32) = (1500.0, 950.0);
const PICKER: (f32, f32) = (1100.0, 760.0);

pub struct Scene {
    pub name: &'static str,
    /// Logical-pixel viewport the scene renders at.
    pub viewport: (f32, f32),
    pub app: Box<dyn App>,
}

pub fn scenes() -> Vec<Scene> {
    fn scene(name: &'static str, viewport: (f32, f32), app: impl App + 'static) -> Scene {
        Scene {
            name,
            viewport,
            app: Box::new(app),
        }
    }
    vec![
        scene("browse", BROWSER, app::fixtures::browse()),
        scene("text_preview", BROWSER, app::fixtures::text_preview()),
        scene("grid", BROWSER, app::fixtures::grid()),
        scene("listing_error", BROWSER, app::fixtures::listing_error()),
        scene("picker_open", PICKER, picker_open()),
        scene("picker_filter_menu", PICKER, picker_filter_menu()),
        scene("picker_save", PICKER, picker_save()),
    ]
}

/// The filters `picker_open` carries: photo.jxr passes "Images",
/// notes.txt only "All files" — switching visibly refilters.
fn image_filters() -> Vec<FileFilter> {
    vec![
        FileFilter {
            name: "Images".into(),
            globs: vec!["*.jxr".into()],
            mimes: vec!["image/*".into()],
        },
        FileFilter {
            name: "All files".into(),
            globs: vec!["*".into()],
            mimes: vec![],
        },
    ]
}

/// Open-file portal picker with an image selected (accept renders
/// enabled) and the "Images" filter active — notes.txt is filtered
/// out of the listing.
fn picker_open() -> PickerApp {
    let mut explorer = app::fixtures::browse();
    app::fixtures::select(&mut explorer, "photo.jxr");
    picker(
        PickerKind::Open { directory: false },
        "Open",
        String::new(),
        explorer,
        image_filters(),
    )
}

/// `picker_open` with the filter select's menu popped open — driven
/// through the real event path (trigger click), not a fixture setter.
fn picker_filter_menu() -> PickerApp {
    let mut picker = picker_open();
    picker.on_event(UiEvent::synthetic_click("picker-filter"), &EventCx::new());
    picker
}

/// Save portal picker with a pre-filled file name and no filters (the
/// selector hides).
fn picker_save() -> PickerApp {
    picker(
        PickerKind::Save,
        "Save",
        "untitled.txt".into(),
        app::fixtures::browse(),
        Vec::new(),
    )
}

fn picker(
    kind: PickerKind,
    accept_label: &str,
    current_name: String,
    explorer: crate::app::ExplorerApp,
    filters: Vec<FileFilter>,
) -> PickerApp {
    PickerApp::new(
        PickerRequest {
            kind,
            accept_label: accept_label.into(),
            start_dir: PathBuf::from("/test/somewhere"),
            current_name,
            filters,
            current_filter: 0,
        },
        explorer,
        None,
        Box::new(|_| {}),
        Arc::new(|| {}),
    )
}

/// Build `app`'s tree at `viewport` and run the bundle pipeline.
///
/// Deliberately skips `before_build`: scenes are hand-assembled state,
/// and draining the message queue would let the places-probe thread's
/// real `/proc/mounts` answer overwrite the synthetic places.
pub fn render(app: &dyn App, viewport: (f32, f32)) -> Bundle {
    let theme = app.theme();
    let diag = HostDiagnostics::default();
    let cx = BuildCx::new(&theme)
        .with_viewport(viewport.0, viewport.1)
        .with_diagnostics(&diag);
    let mut tree = app.build(&cx);
    render_bundle_themed(
        &mut tree,
        Rect::new(0.0, 0.0, viewport.0, viewport.1),
        &theme,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every dumped scene lints clean — the same gate `dump_bundles`
    /// exits nonzero on.
    #[test]
    fn all_scenes_lint_clean() {
        for scene in scenes() {
            let bundle = render(scene.app.as_ref(), scene.viewport);
            let findings: Vec<String> = bundle
                .lint
                .findings
                .iter()
                .map(|f| format!("{f:?}"))
                .collect();
            assert!(
                findings.is_empty(),
                "[{}] lint findings: {findings:#?}",
                scene.name
            );
        }
    }
}
