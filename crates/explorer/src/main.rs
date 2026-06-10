//! prism-explorer — a color-managed file explorer.
//!
//! Browse with damascene on Wayland: HDR-aware image previews via the
//! achromat decode stack, text previews, and an IO layer built for big
//! slow filesystems (everything streams; nothing blocks the UI thread).
//!
//! Usage: `prism-explorer [DIRECTORY]` — defaults to `$HOME`.

mod app;
mod fmt;
mod model;
mod places;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use damascene_core::color::ColorPreferences;
use damascene_core::Rect;
use damascene_winit_wgpu::{run_with_config, HostConfig};

use app::{ExplorerApp, SharedWakeup};
use explorer_io::{Notifier, Pool};
use explorer_previews::Registry;
use explorer_thumbs::ThumbCache;

/// Long edge of cached thumbnails: 2× the grid tile width, so tiles
/// stay sharp on 2× displays.
const THUMB_EDGE: u32 = 256;

/// On-disk cache budget. ~300 KB per f16 thumbnail → room for roughly
/// seven thousand of them.
const THUMB_BUDGET_BYTES: u64 = 2 << 30;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "prism_explorer=info".into()),
        )
        .init();

    let start = match std::env::args_os().nth(1) {
        Some(arg) => {
            let p = PathBuf::from(arg);
            if p.is_absolute() {
                p
            } else {
                // getcwd is a memory read, not filesystem IO.
                std::env::current_dir()?.join(p)
            }
        }
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/")),
    };

    // IO-bound (stat against a slow MDS) and CPU-bound (image decode)
    // work share the pool; a few more threads than the gallery's
    // decode-only pool so a stalled stat doesn't starve previews.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get().clamp(4, 8))
        .unwrap_or(4);

    let wakeup = SharedWakeup::default();
    let notifier: Notifier = {
        let wakeup = wakeup.clone();
        Arc::new(move || {
            if let Some(w) = wakeup.lock().unwrap().as_ref() {
                w.wake();
            }
        })
    };

    // Thumbnails cache to local disk, never the (possibly network)
    // filesystem being browsed. tmp fallback still caches within a
    // session if no home directory resolves.
    let thumbs = Arc::new(ThumbCache::standard(THUMB_EDGE).unwrap_or_else(|| {
        ThumbCache::new(
            std::env::temp_dir().join("prism-explorer-thumbs"),
            THUMB_EDGE,
        )
    }));

    // Evict LRU thumbnails beyond the byte budget (and orphaned temp
    // files) once per launch. A detached thread, not the IO pool: this
    // touches only local disk (no Ceph contention to manage), and pool
    // jobs are cancelled wholesale on every navigation.
    {
        let thumbs = thumbs.clone();
        std::thread::Builder::new()
            .name("thumb-sweep".into())
            .spawn(move || {
                thumbs.sweep(THUMB_BUDGET_BYTES);
            })?;
    }

    let pool = Pool::spawn(workers, "explorer-io");
    let app = ExplorerApp::new(
        start,
        pool,
        notifier,
        Arc::new(Registry::standard()),
        thumbs,
    );

    let config = HostConfig::default()
        .with_app_id("prism-explorer")
        // Extended-range linear swapchain on HDR outputs; degrades to
        // P3/sRGB per compositor capability.
        .with_color_preferences(ColorPreferences::hdr_extended())
        .with_external_wakeup(move |w| *wakeup.lock().unwrap() = Some(w));

    let viewport = Rect::new(0.0, 0.0, 1500.0, 950.0);
    run_with_config("Prism Explorer", viewport, app, config)
        .map_err(|e| anyhow::anyhow!("host error: {e}"))
}
