//! prism-explorer — a color-managed file explorer.
//!
//! Browse with damascene on Wayland: HDR-aware image previews via the
//! achromat decode stack, text previews, and an IO layer built for big
//! slow filesystems (everything streams; nothing blocks the UI thread).
//!
//! Usage: `prism-explorer [DIRECTORY]` — defaults to `$HOME`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use damascene_core::color::ColorPreferences;
use damascene_winit_wgpu::HostConfig;

use explorer_io::Notifier;
use explorer_previews::Registry;
use explorer_thumbs::ThumbCache;
use prism_explorer::browser::BrowserDeps;
use prism_explorer::filechooser::PickerDeps;
use prism_explorer::host::{self, HostCommand};
use prism_explorer::{filechooser, filemanager1, instance, settings, state};

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

    // `--service` starts headless (no initial window) and resident: a
    // portal / FileManager1 activation that serves requests and spins up
    // windows on demand. Any other lone argument is the start directory.
    let mut service_mode = false;
    let mut dir_arg: Option<OsString> = None;
    for arg in std::env::args_os().skip(1) {
        if arg == "--service" {
            service_mode = true;
        } else if dir_arg.is_none() {
            dir_arg = Some(arg);
        }
    }

    // Persisted last-used location and browser preferences, shared by the
    // browser windows and every portal picker. Each is loaded once here
    // (a single small read) and owned by this process — which is why only
    // one prism may run: see the single-instance handoff below.
    let store = state::Store::load();
    let settings = settings::Store::load();

    let start = match dir_arg {
        Some(arg) => {
            let p = PathBuf::from(arg);
            if p.is_absolute() {
                p
            } else {
                // getcwd is a memory read, not filesystem IO.
                std::env::current_dir()?.join(p)
            }
        }
        // No path given: reopen where we left off, else home.
        None => store.last_dir().unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"))
        }),
    };

    // The event loop exists before any app so workers get their
    // notifier from frame zero; wakes that land before the first
    // window are covered by its initial redraw. It is also the source of
    // the proxies the D-Bus services post commands through.
    let event_loop = host::event_loop().map_err(|e| anyhow::anyhow!("host error: {e}"))?;
    let notifier: Notifier = {
        let proxy = event_loop.create_proxy();
        Arc::new(move || {
            let _ = proxy.send_event(HostCommand::Wake);
        })
    };

    // Single-instance arbitration. Only the primary owns the bus names
    // and the history/settings stores; a second launch hands its
    // directory to the primary (which opens a window for it) and exits,
    // so the two never clobber each other's persisted state.
    if !instance::try_become_primary(event_loop.create_proxy()) {
        if !service_mode {
            // Ask the running primary to open the requested directory.
            instance::hand_off(&start)?;
        }
        // A `--service` activation while a primary already runs has
        // nothing to do — the primary serves the request.
        return Ok(());
    }

    // ---- we are the primary instance ------------------------------------

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

    // IO-bound (stat against a slow MDS) and CPU-bound (image decode)
    // work share each window's pool; a few more threads than the
    // gallery's decode-only pool so a stalled stat doesn't starve
    // previews. Every browser window spawns its own pool (generations
    // bump per navigation), so this is a per-window worker count.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get().clamp(4, 8))
        .unwrap_or(4);

    // Mints browser windows: the initial one below, and every later one
    // the host opens for a handoff ([`HostCommand::OpenBrowser`]) or a
    // "show in folder" with no window to reuse ([`ShowLocation`]).
    let browser_deps = BrowserDeps {
        notifier: notifier.clone(),
        registry: registry.clone(),
        thumbs: thumbs.clone(),
        store: store.clone(),
        settings: settings.clone(),
        workers,
    };

    // "Show this in the file manager" service — browsers' Open
    // containing folder, etc. Posts ShowLocation to the loop.
    filemanager1::spawn(event_loop.create_proxy());

    // Portal FileChooser backend: open/save dialogs for every
    // portal-using app, served as picker windows from this process.
    // Best-effort; the primary stays resident regardless (it owns the
    // instance + FileManager1 names too), serving requests with zero
    // windows open.
    let _portal = filechooser::spawn(PickerDeps {
        notifier,
        registry,
        thumbs,
        store,
        settings,
        proxy: event_loop.create_proxy(),
    });

    // Headless in service mode; otherwise open the start directory.
    let initial = (!service_mode).then(|| browser_deps.window(start, None));
    let factory = browser_deps.into_factory();

    let config = HostConfig::default()
        .with_app_id("prism-explorer")
        // Extended-range linear swapchain on HDR outputs; degrades to
        // P3/sRGB per compositor capability.
        .with_color_preferences(ColorPreferences::hdr_extended());

    host::run(event_loop, config, initial, true, Some(factory))
        .map_err(|e| anyhow::anyhow!("host error: {e}"))
}
