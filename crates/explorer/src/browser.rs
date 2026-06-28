//! Browser-window factory: mints standalone browser windows on demand
//! for the host — a second `prism-explorer` launch handing off to this
//! resident process, or a "show in folder" call with no window to
//! reuse. Mirrors the portal's per-window setup in [`crate::filechooser`]:
//! each window spawns its own IO pool (generations bump per navigation,
//! so a shared pool would cancel sibling windows' jobs).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use explorer_io::{Notifier, Pool};
use explorer_previews::Registry;
use explorer_thumbs::ThumbCache;

use crate::app::ExplorerApp;
use crate::host::{BrowserFactory, WindowSpec};
use crate::model::Msg;
use crate::{settings, state};

/// Construction deps shared by every browser window. Like
/// [`crate::filechooser::PickerDeps`], the IO pool is intentionally
/// absent — each window spawns its own.
pub struct BrowserDeps {
    pub notifier: Notifier,
    pub registry: Arc<Registry>,
    pub thumbs: Arc<ThumbCache>,
    /// In-process history store, shared with the first window and every
    /// picker (one owner per process, mutex-guarded).
    pub store: Arc<state::Store>,
    /// Persisted browser preferences, shared so a change in any window
    /// sticks for all.
    pub settings: Arc<settings::Store>,
    /// Workers per window's IO pool — precomputed once so every
    /// on-demand window matches the first window's responsiveness.
    pub workers: usize,
}

/// Default window geometry, matching the standalone browser in `main`.
const WIDTH: f32 = 1500.0;
const HEIGHT: f32 = 950.0;

impl BrowserDeps {
    /// Build the [`WindowSpec`] for one browser window at `dir`,
    /// selecting `select` once the listing streams in.
    pub fn window(&self, dir: PathBuf, select: Option<OsString>) -> WindowSpec {
        let pool = Pool::spawn(self.workers, "explorer-io");
        let app = ExplorerApp::new(
            dir.clone(),
            pool,
            self.notifier.clone(),
            self.registry.clone(),
            self.thumbs.clone(),
            self.store.clone(),
            self.settings.clone(),
            // On-demand windows share the one in-process history store;
            // recording their visits keeps the global history complete.
            true,
        );
        // A pending selection (a "show in folder" on a file) is delivered
        // as the first message the new window drains — the same path the
        // FileManager1 service used when there was a single window.
        if select.is_some() {
            let _ = app.msg_sender().send(Msg::OpenLocation { dir, select });
        }
        WindowSpec {
            title: "Prism Explorer".into(),
            width: WIDTH,
            height: HEIGHT,
            app: Box::new(app),
        }
    }

    /// Turn these deps into a [`BrowserFactory`] the host calls to open
    /// browser windows for `OpenBrowser` / `ShowLocation`.
    pub fn into_factory(self) -> BrowserFactory {
        Arc::new(move |dir, select| self.window(dir, select))
    }
}
