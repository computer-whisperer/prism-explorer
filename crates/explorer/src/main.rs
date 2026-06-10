//! prism-explorer — a color-managed file explorer.
//!
//! Browse with damascene on Wayland: HDR-aware image previews via the
//! achromat decode stack, text previews, and an IO layer built for big
//! slow filesystems (everything streams; nothing blocks the UI thread).
//!
//! Usage: `prism-explorer [DIRECTORY]` — defaults to `$HOME`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use damascene_core::color::ColorPreferences;
use damascene_winit_wgpu::HostConfig;

use explorer_io::{Notifier, Pool};
use explorer_previews::Registry;
use explorer_thumbs::ThumbCache;
use prism_explorer::app::ExplorerApp;
use prism_explorer::host::{self, HostCommand, WindowSpec};
use prism_explorer::{filechooser, filemanager1};

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

    // The event loop exists before any app so workers get their
    // notifier from frame zero; wakes that land before the first
    // window are covered by its initial redraw.
    let event_loop = host::event_loop().map_err(|e| anyhow::anyhow!("host error: {e}"))?;
    let notifier: Notifier = {
        let proxy = event_loop.create_proxy();
        Arc::new(move || {
            let _ = proxy.send_event(HostCommand::Wake);
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

    let registry = Arc::new(Registry::standard());
    let pool = Pool::spawn(workers, "explorer-io");
    let app = ExplorerApp::new(
        start,
        pool,
        notifier.clone(),
        registry.clone(),
        thumbs.clone(),
    );

    // "Show this in the file manager" service — browsers' Open
    // containing folder, etc. Best-effort: if another file manager
    // owns the name we stay a plain browser.
    filemanager1::spawn(app.msg_sender(), notifier.clone());

    // Portal FileChooser backend: open/save dialogs for every
    // portal-using app, served as picker windows from this process.
    // Owning the name makes us a service, so the process then stays
    // resident after its last window closes.
    let resident = filechooser::spawn(filechooser::PickerDeps {
        notifier,
        registry,
        thumbs,
        proxy: event_loop.create_proxy(),
    });

    let config = HostConfig::default()
        .with_app_id("prism-explorer")
        // Extended-range linear swapchain on HDR outputs; degrades to
        // P3/sRGB per compositor capability.
        .with_color_preferences(ColorPreferences::hdr_extended());

    host::run(
        event_loop,
        config,
        WindowSpec {
            title: "Prism Explorer".into(),
            width: 1500.0,
            height: 950.0,
            app: Box::new(app),
        },
        resident,
    )
    .map_err(|e| anyhow::anyhow!("host error: {e}"))
}
