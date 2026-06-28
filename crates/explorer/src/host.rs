//! The explorer's own winit host loop: a resident, multi-window
//! process on one shared wgpu device.
//!
//! damascene-winit-wgpu's `run_with_config` owns the whole event loop
//! and exactly one window — right for most apps, wrong for this one.
//! The explorer stays warm (thumbnail cache, glyph atlases, compiled
//! shaders, D-Bus services) and spins windows off on demand: browser
//! windows today, portal FileChooser dialogs next. So we drive winit
//! ourselves and build each window from damascene's exposed host
//! layers (damascene #79/#80/#81):
//!
//! - [`damascene_winit_wgpu::host::gfx::WindowGfx`] — per-window
//!   surface/swapchain/Runner/color-driver bundle on the shared device.
//! - [`damascene_winit_wgpu::host::color`] — per-window HDR
//!   negotiation and live re-negotiation (output moves, HDR toggles).
//! - [`damascene_winit_wgpu::host::input`] — the pure winit→damascene
//!   event mappers.
//!
//! What this file owns is the part that is genuinely ours: the
//! `WindowId → (WindowGfx, App)` registry, per-window input routing and
//! two-lane redraw pacing, the shared clipboard, and the user-event
//! channel that background threads (IO pool, D-Bus services) poke.
//!
//! Each window hosts its own [`App`] — interaction state, hotkeys,
//! focus, and frame pacing are all per-window. A `Runner` owns its
//! pipelines and glyph atlas, so rather than share them across windows
//! the daemon keeps a small pool of pre-warmed `Runner`s ([`RunnerPool`])
//! and hands one to each new window — a portal picker opens from a
//! ready pipeline instead of paying ~300ms of construction on the open
//! path (damascene #105's `WindowGfx::with_surface_and_renderer`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use damascene_wgpu::{Runner, RunnerCaps};

use damascene_core::widgets::text_input::{self, ClipboardKind};
use damascene_core::{
    clipboard, App, Cursor, FrameTrigger, HostDiagnostics, KeyModifiers, Pointer, PointerButton,
    Rect, UiEvent, UiEventKind,
};
use damascene_winit_wgpu::host::input::{
    key_modifiers, map_key, pointer_button, touch_pressure, winit_cursor,
};
use damascene_winit_wgpu::host::WindowGfx;
use damascene_winit_wgpu::HostConfig;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Commands posted into the loop from other threads via
/// `EventLoopProxy`. The IO pool's notifier and the D-Bus services
/// send [`Wake`](HostCommand::Wake); the portal FileChooser service
/// opens its picker dialogs with [`OpenWindow`](HostCommand::OpenWindow)
/// and tears them down with [`CloseWindow`](HostCommand::CloseWindow);
/// the single-instance and FileManager1 services open browser windows
/// with [`OpenBrowser`](HostCommand::OpenBrowser) /
/// [`ShowLocation`](HostCommand::ShowLocation).
pub enum HostCommand {
    /// Data outside the UI trees changed (listing batch, decoded
    /// preview, D-Bus navigation message): rebuild every window.
    Wake,
    /// Open a window for `spec`. `token` is the sender's name for it —
    /// the host keeps a token → `WindowId` map so the sender can close
    /// it later without ever seeing winit types.
    OpenWindow { token: u64, spec: WindowSpec },
    /// Close the window opened under `token` (drops its app). A stale
    /// or unknown token is a no-op — the user may have closed the
    /// window first.
    CloseWindow { token: u64 },
    /// Open a *new* browser window at `dir` (optionally selecting
    /// `select`). Used when a second `prism-explorer` launch hands off
    /// to this resident process. The host mints the window from its
    /// browser factory, so the sender needs no app-construction deps.
    OpenBrowser {
        dir: PathBuf,
        select: Option<OsString>,
    },
    /// "Show this in the file manager": navigate the focused browser
    /// window to `dir` (selecting `select`), or — if no browser window
    /// is open — mint a new one. Posted by the FileManager1 service.
    ShowLocation {
        dir: PathBuf,
        select: Option<OsString>,
    },
}

/// Mints a fresh browser window at a directory. The host holds one of
/// these so D-Bus service threads can ask for browser windows without
/// owning the explorer's construction dependencies (pool, registry,
/// caches, stores). Each call spawns that window's own IO pool, the
/// same way the portal's pickers do.
pub type BrowserFactory =
    Arc<dyn Fn(PathBuf, Option<OsString>) -> WindowSpec + Send + Sync>;

/// A window to open: title, logical size, and the [`App`] that owns it.
///
/// `Send` because specs travel inside [`HostCommand`]s, which D-Bus
/// threads post through the loop proxy. The app only ever *runs* on
/// the loop thread.
pub struct WindowSpec {
    pub title: String,
    pub width: f32,
    pub height: f32,
    pub app: Box<dyn HostApp + Send>,
}

/// App contract for this custom multi-window host. Most apps only need
/// [`App`]; GPU hooks are for app-owned `surface()` textures.
pub trait HostApp: App {
    fn gpu_setup(&mut self, _device: &wgpu::Device, _queue: &wgpu::Queue) {}

    fn before_paint(
        &mut self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _viewport: Rect,
        _scale_factor: f32,
    ) {
    }

    /// Text the app wants placed on the system clipboard since the last
    /// frame ("copy path"). The clipboard is host-owned (one `arboard`
    /// shared across windows), so the app queues writes and the host
    /// drains them here. Last write wins.
    fn drain_clipboard_writes(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// The window gained (or lost) keyboard focus. The browser uses the
    /// focus-gained edge to re-read the system clipboard, so a file
    /// selection copied in another app becomes pasteable here.
    fn window_focused(&mut self, _focused: bool) {}

    /// Navigate this window to `dir`, selecting `select` once it streams
    /// in. Returns whether the app accepted it: browser windows do
    /// (so [`HostCommand::ShowLocation`] reuses the focused one); picker
    /// windows leave the default and decline, so a "show in folder" call
    /// never hijacks an open dialog.
    fn navigate_to(&mut self, _dir: PathBuf, _select: Option<OsString>) -> bool {
        false
    }
}

/// Run the host loop. Returns when the last window closes
/// (`resident: false`) or only on a fatal error (`resident: true` — a
/// process serving D-Bus requests stays warm with zero windows, ready
/// to spin one up for the next request). `Err` means GPU bring-up for
/// the first window failed.
///
/// `initial` is the window opened at startup, or `None` for a service
/// launch (`--service`) that starts headless and waits for a portal or
/// FileManager1 request; such a launch must be `resident`. `browser`
/// mints browser windows on demand for [`HostCommand::OpenBrowser`] /
/// [`ShowLocation`](HostCommand::ShowLocation) (and is `None` only for
/// hosts that never serve those — currently always supplied).
///
/// `config` supplies the color-preference ladder, MSAA sample count,
/// present-mode choice, and Wayland `app_id`; its run-loop knobs
/// (`redraw_interval`, `external_wakeup`) are not consulted — pacing
/// is two-lane per window, and wakeups arrive as [`HostCommand`]s on
/// the loop the caller built (see [`event_loop`]).
pub fn run(
    event_loop: EventLoop<HostCommand>,
    config: HostConfig,
    initial: Option<WindowSpec>,
    resident: bool,
    browser: Option<BrowserFactory>,
) -> Result<(), String> {
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut host = Host {
        config,
        gpu: None,
        runner_pool: None,
        windows: HashMap::new(),
        tokens: HashMap::new(),
        initial,
        resident,
        browser,
        focused: None,
        clipboard: arboard::Clipboard::new().ok(),
        last_primary: String::new(),
        setup_error: None,
    };
    event_loop.run_app(&mut host).map_err(|e| e.to_string())?;
    match host.setup_error {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Build the user-event loop. Separate from [`run`] so the caller can
/// take proxies first — app constructors need their notifier before
/// the loop starts.
pub fn event_loop() -> Result<EventLoop<HostCommand>, String> {
    EventLoop::with_user_event()
        .build()
        .map_err(|e| e.to_string())
}

/// The GPU context every window shares. Acquired once, at first-window
/// creation (that window's surface anchors `compatible_surface` during
/// adapter selection); each further `Runner` clones the internally
/// ref-counted handles.
struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Human-readable backend tag for `HostDiagnostics`.
    backend: &'static str,
}

/// How many warm spare `Runner`s to keep ready. One covers the common
/// single-picker case; two absorbs a burst (two apps popping a dialog
/// at once) and the ~300ms it takes a background rebuild to refill.
/// Each idle spare holds a full pipeline set + glyph atlas, so this is
/// a memory/latency trade — kept small deliberately.
const WARM_RUNNERS: usize = 2;

/// Pre-warmed [`Runner`]s the host hands to new windows so they skip
/// the ~300ms of pipeline + glyph-atlas construction on the open path
/// — the whole point of a resident daemon. A background thread builds
/// spares for the primary window's `(format, sample_count)` and parks
/// them; each window open pops one and requests a rebuild to refill.
///
/// Both degenerate cases stay correct: a pool miss (none ready yet)
/// falls back to a cold `WindowGfx::with_surface`, and a window that
/// negotiates a *different* format (e.g. landing on a different-HDR
/// output) is repaired in place by `with_surface_and_renderer`'s
/// `set_target_format` — the slower path the pool exists to avoid, not
/// a bug.
struct RunnerPool {
    /// Warm spares, all built for the primary window's
    /// `(format, sample_count)`. LIFO; depth tops out at
    /// [`WARM_RUNNERS`].
    spares: Arc<Mutex<Vec<Runner>>>,
    /// Each `()` asks the warmer thread to build one more spare.
    refill: mpsc::Sender<()>,
}

impl RunnerPool {
    /// Spawn the warmer thread and request `target` spares up front.
    /// `device`/`queue` are `Send + Sync` and wgpu permits resource
    /// creation on another thread concurrently with rendering on the
    /// loop, so the build cost lands entirely off the open path.
    fn spawn(gpu: &Gpu, format: wgpu::TextureFormat, sample_count: u32, target: usize) -> Self {
        let spares: Arc<Mutex<Vec<Runner>>> = Arc::new(Mutex::new(Vec::new()));
        let caps = RunnerCaps::from_adapter(&gpu.adapter);
        let device = gpu.device.clone();
        let queue = gpu.queue.clone();
        let sink = spares.clone();
        let (refill, requests) = mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("runner-warmer".into())
            .spawn(move || {
                // Ends when the last `refill` sender drops (host exit).
                for () in requests {
                    let mut runner = Runner::with_caps(&device, &queue, format, sample_count, caps);
                    runner.warm_default_glyphs();
                    sink.lock().unwrap().push(runner);
                }
            });
        if spawned.is_ok() {
            for _ in 0..target {
                let _ = refill.send(());
            }
        } else {
            tracing::warn!("runner warmer thread failed to spawn; windows open cold");
        }
        RunnerPool { spares, refill }
    }

    /// Pop a warm spare, requesting a 1-for-1 replacement to keep the
    /// depth steady. `None` when none is ready (caller builds cold).
    fn take(&self) -> Option<Runner> {
        let runner = self.spares.lock().unwrap().pop();
        if runner.is_some() {
            let _ = self.refill.send(());
        }
        runner
    }
}

struct Host {
    config: HostConfig,
    gpu: Option<Gpu>,
    /// Warm-Runner pool, spawned once the first window reveals the
    /// negotiated `(format, sample_count)`. `None` until then.
    runner_pool: Option<RunnerPool>,
    windows: HashMap<WindowId, WindowState>,
    /// Sender-chosen names for command-opened windows, so
    /// [`HostCommand::CloseWindow`] can find them. Entries for windows
    /// the user already closed are pruned on that close.
    tokens: HashMap<u64, WindowId>,
    /// The first window's spec, consumed by `resumed()`. `None` for a
    /// `--service` launch that starts headless.
    initial: Option<WindowSpec>,
    /// Stay alive with zero windows (the process is a D-Bus service);
    /// otherwise the last window closing exits the loop.
    resident: bool,
    /// Mints browser windows for [`HostCommand::OpenBrowser`] /
    /// [`ShowLocation`](HostCommand::ShowLocation).
    browser: Option<BrowserFactory>,
    /// The window that last gained keyboard focus, so `ShowLocation`
    /// can route "show in folder" to the front browser. Pruned to
    /// `None` when that window closes.
    focused: Option<WindowId>,
    /// Best-effort native clipboard, shared across windows.
    /// Initialization can fail headless; copy shortcuts no-op then.
    clipboard: Option<arboard::Clipboard>,
    /// Last text mirrored into the Linux primary selection.
    last_primary: String,
    /// Fatal GPU bring-up failure for the *first* window — recorded
    /// here and surfaced as `run()`'s `Err` (winit handlers can't
    /// return errors). Later windows fail soft: log and drop the spec,
    /// the resident process carries on.
    setup_error: Option<String>,
}

/// Everything one window owns: its GPU bundle, its app, and the input
/// and pacing state the upstream host kept globally (correct there —
/// one window — but per-window here).
struct WindowState {
    /// Drop order: `WindowGfx` internally drops its color driver before
    /// its `Window` (they share winit's libwayland connection).
    gfx: WindowGfx,
    app: Box<dyn HostApp + Send>,
    /// Last pointer position in logical pixels.
    last_pointer: Option<(f32, f32)>,
    modifiers: KeyModifiers,
    /// Last cursor pushed to `Window::set_cursor` (syscall — only push
    /// changes).
    last_cursor: Cursor,
    /// Latest `Resized` not yet applied — coalesced so
    /// `surface.configure()` runs once per frame, not once per event.
    pending_resize: Option<PhysicalSize<u32>>,
    /// Two-lane redraw deadlines: layout (rebuild + full prepare) and
    /// paint-only (`Runner::repaint`, cached ops, time-driven shaders).
    next_layout_redraw: Option<Instant>,
    next_paint_redraw: Option<Instant>,
    /// Why the next redraw was requested; consumed by `RedrawRequested`
    /// to pick the full vs paint-only path.
    next_trigger: FrameTrigger,
    last_frame_at: Option<Instant>,
    frame_index: u64,
    /// Previous frame's stage costs, surfaced via `HostDiagnostics`.
    last_timings: damascene_core::runtime::PrepareTimings,
    last_build: Duration,
    last_prepare: Duration,
    last_submit: Duration,
}

impl Host {
    fn create_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        spec: WindowSpec,
    ) -> Option<WindowId> {
        let first = self.gpu.is_none();
        let attrs = Window::default_attributes()
            .with_title(&spec.title)
            .with_inner_size(PhysicalSize::new(spec.width as u32, spec.height as u32));
        #[cfg(target_os = "linux")]
        let attrs = if let Some(app_id) = self.config.app_id.as_deref() {
            use winit::platform::wayland::WindowAttributesExtWayland;
            use winit::platform::x11::WindowAttributesExtX11;
            let a = WindowAttributesExtWayland::with_name(attrs, app_id, "");
            WindowAttributesExtX11::with_name(a, app_id, app_id)
        } else {
            attrs
        };

        let result = (|| -> Result<WindowState, String> {
            let window = std::sync::Arc::new(
                event_loop
                    .create_window(attrs)
                    .map_err(|e| format!("could not create a window: {e}"))?,
            );
            if self.gpu.is_none() {
                self.gpu = Some(acquire_gpu(window.clone())?);
            }
            let gpu = self.gpu.as_ref().unwrap();
            // Create the window's real surface here (not inside
            // WindowGfx::new) so a pooled Runner can be injected
            // alongside it. A warm spare skips ~300ms of pipeline +
            // glyph-atlas construction; a miss falls back to the cold
            // constructor, which builds and warms its own Runner.
            let surface = gpu
                .instance
                .create_surface(window.clone())
                .map_err(|e| format!("could not create a rendering surface: {e}"))?;
            let mut gfx = match self.runner_pool.as_ref().and_then(|p| p.take()) {
                Some(runner) => WindowGfx::with_surface_and_renderer(
                    &gpu.adapter,
                    &gpu.device,
                    &gpu.queue,
                    window,
                    surface,
                    &self.config,
                    runner,
                ),
                None => WindowGfx::with_surface(
                    &gpu.adapter,
                    &gpu.device,
                    &gpu.queue,
                    window,
                    surface,
                    &self.config,
                ),
            };
            let mut app = spec.app;
            gfx.renderer.set_theme(app.theme());
            for s in app.shaders() {
                gfx.renderer.register_shader_with(
                    &gpu.device,
                    s.name,
                    s.wgsl,
                    s.samples_backdrop,
                    s.samples_time,
                );
            }
            app.gpu_setup(&gpu.device, &gpu.queue);
            app.before_build();
            Ok(WindowState {
                gfx,
                app,
                last_pointer: None,
                modifiers: KeyModifiers::default(),
                last_cursor: Cursor::Default,
                pending_resize: None,
                next_layout_redraw: None,
                next_paint_redraw: None,
                next_trigger: FrameTrigger::Initial,
                last_frame_at: None,
                frame_index: 0,
                last_timings: Default::default(),
                last_build: Duration::ZERO,
                last_prepare: Duration::ZERO,
                last_submit: Duration::ZERO,
            })
        })();

        match result {
            Ok(state) => {
                state.gfx.window.request_redraw();
                let id = state.gfx.window.id();
                // First window done: start warming spare Runners against
                // the format it negotiated, so later windows (portal
                // pickers) open from a ready pipeline.
                if self.runner_pool.is_none() {
                    let format = state.gfx.config.format;
                    let sample_count = self.config.sample_count.max(1);
                    self.runner_pool = self
                        .gpu
                        .as_ref()
                        .map(|gpu| RunnerPool::spawn(gpu, format, sample_count, WARM_RUNNERS));
                }
                self.windows.insert(id, state);
                Some(id)
            }
            Err(message) if first => {
                // No GPU, no app: surface the failure as run()'s Err.
                tracing::error!("{message}");
                self.setup_error = Some(message);
                event_loop.exit();
                None
            }
            Err(message) => {
                tracing::error!(error = %message, "dropping window request");
                None
            }
        }
    }

    /// Drop one window's state (its app with it) and exit the loop if
    /// that was the last window of a non-resident host.
    fn close_window(&mut self, event_loop: &ActiveEventLoop, id: WindowId) {
        self.windows.remove(&id);
        self.tokens.retain(|_, mapped| *mapped != id);
        if self.focused == Some(id) {
            self.focused = None;
        }
        if self.windows.is_empty() && !self.resident {
            event_loop.exit();
        }
    }

    /// Mint a new browser window at `dir` via the factory, if one is
    /// configured. Without a factory (a host that never serves browser
    /// requests) the command is a logged no-op rather than a panic.
    fn open_browser(
        &mut self,
        event_loop: &ActiveEventLoop,
        dir: PathBuf,
        select: Option<OsString>,
    ) {
        match &self.browser {
            Some(factory) => {
                let spec = factory(dir, select);
                self.create_window(event_loop, spec);
            }
            None => tracing::warn!("OpenBrowser/ShowLocation with no browser factory; ignoring"),
        }
    }
}

/// Acquire the shared adapter/device against the first window's
/// surface. Failures are environment outcomes (no Vulkan driver, no
/// GPU in a container), not bugs — returned, not panicked.
fn acquire_gpu(window: std::sync::Arc<Window>) -> Result<Gpu, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    // This surface only anchors adapter selection; WindowGfx::new
    // creates the one the window renders with. Cheap, and it keeps
    // one code path for every window.
    let surface = instance
        .create_surface(window)
        .map_err(|e| format!("could not create a rendering surface: {e}"))?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .map_err(|e| {
        format!("no compatible GPU adapter ({e}) — a Vulkan driver (or lavapipe) is required")
    })?;
    let backend = backend_label(adapter.get_info().backend);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("prism-explorer::device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .map_err(|e| format!("GPU device creation failed on the selected adapter: {e}"))?;
    Ok(Gpu {
        instance,
        adapter,
        device,
        queue,
        backend,
    })
}

fn backend_label(backend: wgpu::Backend) -> &'static str {
    match backend {
        wgpu::Backend::Vulkan => "Vulkan",
        wgpu::Backend::Metal => "Metal",
        wgpu::Backend::Dx12 => "DX12",
        wgpu::Backend::Gl => "GL",
        wgpu::Backend::BrowserWebGpu => "WebGPU",
        _ => "?",
    }
}

impl ApplicationHandler<HostCommand> for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(spec) = self.initial.take() {
            self.create_window(event_loop, spec);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: HostCommand) {
        match event {
            HostCommand::Wake => {
                // External data changed; any window's app may be
                // affected, and Wake carries no addressee. Full
                // rebuilds on every window per poke — fine at the
                // window counts a file manager runs at.
                for win in self.windows.values_mut() {
                    win.next_trigger = FrameTrigger::External;
                    win.gfx.window.request_redraw();
                }
            }
            HostCommand::OpenWindow { token, spec } => {
                if let Some(id) = self.create_window(event_loop, spec) {
                    self.tokens.insert(token, id);
                }
            }
            HostCommand::CloseWindow { token } => {
                if let Some(id) = self.tokens.remove(&token) {
                    self.close_window(event_loop, id);
                }
            }
            HostCommand::OpenBrowser { dir, select } => {
                self.open_browser(event_loop, dir, select);
            }
            HostCommand::ShowLocation { dir, select } => {
                // Reuse the focused browser window if it accepts the
                // navigation; otherwise open a fresh one.
                let routed = self
                    .focused
                    .and_then(|id| self.windows.get_mut(&id))
                    .is_some_and(|win| {
                        let accepted = win.app.navigate_to(dir.clone(), select.clone());
                        if accepted {
                            win.gfx.window.request_redraw();
                        }
                        accepted
                    });
                if !routed {
                    self.open_browser(event_loop, dir, select);
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let WindowEvent::CloseRequested = event {
            // Dropping the state drops the window's app; a portal
            // picker's pending D-Bus reply observes that as its result
            // channel disconnecting and answers "cancelled".
            self.close_window(event_loop, id);
            return;
        }
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        let scale = win.gfx.window.scale_factor() as f32;

        match event {
            WindowEvent::CloseRequested => unreachable!("handled above"),

            WindowEvent::Resized(size) => {
                let w = size.width.max(1);
                let h = size.height.max(1);
                // Drop no-op resizes — surface.configure() for them
                // stalls the GPU pipeline without changing anything.
                let already_pending = win
                    .pending_resize
                    .map(|s| s.width == w && s.height == h)
                    .unwrap_or(false);
                let same_as_current = win.pending_resize.is_none()
                    && w == win.gfx.config.width
                    && h == win.gfx.config.height;
                if already_pending || same_as_current {
                    return;
                }
                win.pending_resize = Some(PhysicalSize::new(w, h));
                win.next_trigger = FrameTrigger::Resize;
                win.gfx.window.request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                let lx = position.x as f32 / scale;
                let ly = position.y as f32 / scale;
                win.last_pointer = Some((lx, ly));
                let moved = win.gfx.renderer.pointer_moved(Pointer::moving(lx, ly));
                for event in moved.events {
                    dispatch_app_event(
                        win.app.as_mut(),
                        event,
                        &win.gfx.renderer,
                        &mut self.clipboard,
                        &mut self.last_primary,
                    );
                }
                // High-frequency on Wayland — only redraw when the
                // move changed something (hover identity, drag).
                if moved.needs_redraw {
                    win.next_trigger = FrameTrigger::Pointer;
                    win.gfx.window.request_redraw();
                }
            }

            WindowEvent::CursorLeft { .. } => {
                win.last_pointer = None;
                for event in win.gfx.renderer.pointer_left() {
                    dispatch_app_event(
                        win.app.as_mut(),
                        event,
                        &win.gfx.renderer,
                        &mut self.clipboard,
                        &mut self.last_primary,
                    );
                }
                win.next_trigger = FrameTrigger::Pointer;
                win.gfx.window.request_redraw();
            }

            WindowEvent::HoveredFile(path) => {
                let (lx, ly) = win.last_pointer.unwrap_or((0.0, 0.0));
                for event in win.gfx.renderer.file_hovered(path, lx, ly) {
                    dispatch_app_event(
                        win.app.as_mut(),
                        event,
                        &win.gfx.renderer,
                        &mut self.clipboard,
                        &mut self.last_primary,
                    );
                }
                win.next_trigger = FrameTrigger::Pointer;
                win.gfx.window.request_redraw();
            }

            WindowEvent::HoveredFileCancelled => {
                for event in win.gfx.renderer.file_hover_cancelled() {
                    dispatch_app_event(
                        win.app.as_mut(),
                        event,
                        &win.gfx.renderer,
                        &mut self.clipboard,
                        &mut self.last_primary,
                    );
                }
                win.next_trigger = FrameTrigger::Pointer;
                win.gfx.window.request_redraw();
            }

            WindowEvent::DroppedFile(path) => {
                let (lx, ly) = win.last_pointer.unwrap_or((0.0, 0.0));
                for event in win.gfx.renderer.file_dropped(path, lx, ly) {
                    dispatch_app_event(
                        win.app.as_mut(),
                        event,
                        &win.gfx.renderer,
                        &mut self.clipboard,
                        &mut self.last_primary,
                    );
                }
                win.next_trigger = FrameTrigger::Pointer;
                win.gfx.window.request_redraw();
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button) = pointer_button(button) else {
                    return;
                };
                let Some((lx, ly)) = win.last_pointer else {
                    return;
                };
                match state {
                    ElementState::Pressed => {
                        for event in win
                            .gfx
                            .renderer
                            .pointer_down(Pointer::mouse(lx, ly, button))
                        {
                            dispatch_app_event(
                                win.app.as_mut(),
                                event,
                                &win.gfx.renderer,
                                &mut self.clipboard,
                                &mut self.last_primary,
                            );
                        }
                    }
                    ElementState::Released => {
                        for event in win.gfx.renderer.pointer_up(Pointer::mouse(lx, ly, button)) {
                            let event = attach_primary_selection_text(event, &mut self.clipboard);
                            dispatch_app_event(
                                win.app.as_mut(),
                                event,
                                &win.gfx.renderer,
                                &mut self.clipboard,
                                &mut self.last_primary,
                            );
                        }
                    }
                }
                win.next_trigger = FrameTrigger::Pointer;
                win.gfx.window.request_redraw();
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let Some((lx, ly)) = win.last_pointer else {
                    return;
                };
                // Wheel ticks → logical pixels; ~50 px/line matches
                // typical OS feel for notched wheels.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * 50.0, -y * 50.0),
                    MouseScrollDelta::PixelDelta(p) => {
                        (-(p.x as f32) / scale, -(p.y as f32) / scale)
                    }
                };
                let mut needs_redraw = false;
                let consumed =
                    if let Some(event) = win.gfx.renderer.pointer_wheel_event(lx, ly, dx, dy) {
                        needs_redraw = true;
                        dispatch_app_wheel_event(
                            win.app.as_mut(),
                            event,
                            &win.gfx.renderer,
                            &mut self.clipboard,
                            &mut self.last_primary,
                        )
                    } else {
                        false
                    };
                if !consumed && win.gfx.renderer.pointer_wheel(lx, ly, dy) {
                    needs_redraw = true;
                }
                if needs_redraw {
                    win.next_trigger = FrameTrigger::Pointer;
                    win.gfx.window.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                win.modifiers = key_modifiers(modifiers.state());
                win.gfx.renderer.set_modifiers(win.modifiers);
            }

            WindowEvent::Focused(focused) => {
                // The browser re-reads the system clipboard on focus gain;
                // the worker's notifier pokes the redraw once it lands, so
                // no frame is forced here.
                win.app.window_focused(focused);
                // Track the front window so ShowLocation ("show in
                // folder") can route to it. Only the focus-gain edge
                // updates it; a focus-loss leaves the last-focused window
                // as the routing target.
                if focused {
                    self.focused = Some(id);
                }
            }

            WindowEvent::KeyboardInput {
                event:
                    key_event @ winit::event::KeyEvent {
                        state: ElementState::Pressed,
                        ..
                    },
                is_synthetic: false,
                ..
            } => {
                if let Some(key) = map_key(&key_event.logical_key) {
                    for event in win
                        .gfx
                        .renderer
                        .key_down(key, win.modifiers, key_event.repeat)
                    {
                        // Clipboard chords resolve host-side: the app
                        // sees a paste with text attached, a cut as a
                        // delete-selection, and copies untouched.
                        match text_input::clipboard_request(&event) {
                            Some(ClipboardKind::Copy) => {
                                copy_current_selection(&win.gfx.renderer, &mut self.clipboard);
                                dispatch_app_event(
                                    win.app.as_mut(),
                                    event,
                                    &win.gfx.renderer,
                                    &mut self.clipboard,
                                    &mut self.last_primary,
                                );
                            }
                            Some(ClipboardKind::Cut) => {
                                copy_current_selection(&win.gfx.renderer, &mut self.clipboard);
                                let delete = clipboard::delete_selection_event(event);
                                dispatch_app_event(
                                    win.app.as_mut(),
                                    delete,
                                    &win.gfx.renderer,
                                    &mut self.clipboard,
                                    &mut self.last_primary,
                                );
                            }
                            Some(ClipboardKind::Paste) => {
                                let event = match get_clipboard_text(&mut self.clipboard) {
                                    Some(text) => clipboard::paste_text_event(event, text),
                                    None => event,
                                };
                                dispatch_app_event(
                                    win.app.as_mut(),
                                    event,
                                    &win.gfx.renderer,
                                    &mut self.clipboard,
                                    &mut self.last_primary,
                                );
                            }
                            None => dispatch_app_event(
                                win.app.as_mut(),
                                event,
                                &win.gfx.renderer,
                                &mut self.clipboard,
                                &mut self.last_primary,
                            ),
                        }
                    }
                }
                // Composed text on the same press (Shift+a → "A",
                // dead keys); IME commits arrive via WindowEvent::Ime.
                if let Some(text) = &key_event.text {
                    if let Some(event) = win.gfx.renderer.text_input(text.to_string()) {
                        dispatch_app_event(
                            win.app.as_mut(),
                            event,
                            &win.gfx.renderer,
                            &mut self.clipboard,
                            &mut self.last_primary,
                        );
                    }
                }
                win.next_trigger = FrameTrigger::Keyboard;
                win.gfx.window.request_redraw();
            }

            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                if let Some(event) = win.gfx.renderer.text_input(text) {
                    dispatch_app_event(
                        win.app.as_mut(),
                        event,
                        &win.gfx.renderer,
                        &mut self.clipboard,
                        &mut self.last_primary,
                    );
                }
                win.next_trigger = FrameTrigger::Keyboard;
                win.gfx.window.request_redraw();
            }

            WindowEvent::Touch(touch) => {
                let lx = touch.location.x as f32 / scale;
                let ly = touch.location.y as f32 / scale;
                win.last_pointer = Some((lx, ly));
                let mut pointer = Pointer::touch(
                    lx,
                    ly,
                    PointerButton::Primary,
                    damascene_core::PointerId(touch.id as u32),
                );
                pointer.pressure = touch_pressure(touch.force);
                match touch.phase {
                    TouchPhase::Started => {
                        for event in win.gfx.renderer.pointer_down(pointer) {
                            dispatch_app_event(
                                win.app.as_mut(),
                                event,
                                &win.gfx.renderer,
                                &mut self.clipboard,
                                &mut self.last_primary,
                            );
                        }
                    }
                    TouchPhase::Moved => {
                        let moved = win.gfx.renderer.pointer_moved(pointer);
                        for event in moved.events {
                            dispatch_app_event(
                                win.app.as_mut(),
                                event,
                                &win.gfx.renderer,
                                &mut self.clipboard,
                                &mut self.last_primary,
                            );
                        }
                        if !moved.needs_redraw {
                            return;
                        }
                    }
                    TouchPhase::Ended => {
                        for event in win.gfx.renderer.pointer_up(pointer) {
                            dispatch_app_event(
                                win.app.as_mut(),
                                event,
                                &win.gfx.renderer,
                                &mut self.clipboard,
                                &mut self.last_primary,
                            );
                        }
                        win.last_pointer = None;
                    }
                    TouchPhase::Cancelled => {
                        for event in win.gfx.renderer.pointer_left() {
                            dispatch_app_event(
                                win.app.as_mut(),
                                event,
                                &win.gfx.renderer,
                                &mut self.clipboard,
                                &mut self.last_primary,
                            );
                        }
                        win.last_pointer = None;
                    }
                }
                win.next_trigger = FrameTrigger::Pointer;
                win.gfx.window.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                let backend = self.gpu.as_ref().map(|g| g.backend).unwrap_or("?");
                win.redraw(backend);
                // Drain app-initiated clipboard writes ("copy path")
                // into the host-owned clipboard. Last write wins.
                if let Some(text) = win.app.drain_clipboard_writes().pop() {
                    if let Some(clipboard) = self.clipboard.as_mut() {
                        if let Err(e) = clipboard.set_text(text) {
                            tracing::warn!(error = %e, "clipboard write failed");
                        }
                    }
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drain each window's color-management queue once per loop
        // wake (non-blocking in the steady state). A compositor-side
        // change (output move, HDR toggle) re-negotiates the window's
        // swapchain and redraws it.
        for win in self.windows.values_mut() {
            if let Some(plan) = win.gfx.color.poll() {
                win.gfx.apply_renegotiation(&plan);
                win.next_trigger = FrameTrigger::External;
                win.gfx.window.request_redraw();
            }
        }

        // Fire expired redraw deadlines; park the loop until the
        // earliest pending one across all windows.
        let now = Instant::now();
        let mut wake_up: Option<Instant> = None;
        for win in self.windows.values_mut() {
            if let Some(t) = win.next_layout_redraw {
                if now >= t {
                    win.next_trigger = FrameTrigger::Animation;
                    win.gfx.window.request_redraw();
                    win.next_layout_redraw = None;
                } else {
                    wake_up = Some(wake_up.map_or(t, |p| p.min(t)));
                }
            }
            if let Some(t) = win.next_paint_redraw {
                if now >= t {
                    // Layout wins when both fire this turn — it
                    // re-derives the paint deadline from a fresh
                    // prepare.
                    if !matches!(
                        win.next_trigger,
                        FrameTrigger::Animation | FrameTrigger::External
                    ) {
                        win.next_trigger = FrameTrigger::ShaderPaint;
                    }
                    win.gfx.window.request_redraw();
                    win.next_paint_redraw = None;
                } else {
                    wake_up = Some(wake_up.map_or(t, |p| p.min(t)));
                }
            }
        }
        match wake_up {
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

impl WindowState {
    /// Render one frame: drain time-driven input, apply the coalesced
    /// resize, then either the full build → prepare → render path or
    /// the paint-only `repaint` path (cached ops, time-driven shaders)
    /// depending on what triggered the frame. Reschedules the two
    /// redraw lanes from the prepare result.
    fn redraw(&mut self, backend: &'static str) {
        let gfx = &mut self.gfx;

        // Time-driven input (touch long-press) whose deadline elapsed
        // — dispatch before build so the app sees it this frame.
        for event in gfx.renderer.poll_input(Instant::now()) {
            let cx = damascene_core::EventCx::new().with_ui_state(gfx.renderer.ui_state());
            self.app.on_event(event, &cx);
        }

        if let Some(size) = self.pending_resize.take() {
            gfx.resize(size.width, size.height);
        }

        let frame = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                // Reconfigure and re-request — without the request the
                // compositor keeps the stale frame until some other
                // event wakes us.
                gfx.surface.configure(&gfx.device, &gfx.config);
                gfx.window.request_redraw();
                return;
            }
            other => {
                tracing::warn!("surface unavailable: {other:?}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let frame_start = Instant::now();
        let last_frame_dt = self
            .last_frame_at
            .map(|t| frame_start.duration_since(t))
            .unwrap_or(Duration::ZERO);
        self.last_frame_at = Some(frame_start);
        let trigger = std::mem::take(&mut self.next_trigger);
        let scale_factor = gfx.window.scale_factor() as f32;
        let viewport = Rect::new(
            0.0,
            0.0,
            gfx.config.width as f32 / scale_factor,
            gfx.config.height as f32 / scale_factor,
        );
        // Paint-only: a time-driven shader's deadline fired with no
        // input/layout signal queued — skip rebuild + layout, reuse
        // cached ops, advance frame.time.
        let paint_only = trigger == FrameTrigger::ShaderPaint && self.pending_resize.is_none();

        let (prepare, palette, t_after_build, t_after_prepare) = if paint_only {
            self.app
                .before_paint(&gfx.device, &gfx.queue, viewport, scale_factor);
            let palette = gfx.renderer.theme().palette().clone();
            let t_after_build = Instant::now();
            let prepare = gfx
                .renderer
                .repaint(&gfx.device, &gfx.queue, viewport, scale_factor);
            (prepare, palette, t_after_build, Instant::now())
        } else {
            self.frame_index = self.frame_index.wrapping_add(1);
            let t = &self.last_timings;
            // The spread is currently a no-op (every field is listed)
            // but keeps this compiling when upstream adds fields.
            #[allow(clippy::needless_update)]
            let diagnostics = HostDiagnostics {
                backend,
                surface_size: (gfx.config.width, gfx.config.height),
                scale_factor,
                msaa_samples: gfx.msaa.as_ref().map(|m| m.sample_count).unwrap_or(1),
                frame_index: self.frame_index,
                last_frame_dt,
                last_build: self.last_build,
                last_prepare: self.last_prepare,
                last_submit: self.last_submit,
                last_layout: t.layout,
                last_layout_intrinsic_cache_hits: t.layout_intrinsic_cache.hits,
                last_layout_intrinsic_cache_misses: t.layout_intrinsic_cache.misses,
                last_layout_pruned_subtrees: t.layout_prune.subtrees,
                last_layout_pruned_nodes: t.layout_prune.nodes,
                last_draw_ops: t.draw_ops,
                last_draw_ops_culled_text_ops: t.draw_ops_culled_text_ops,
                last_paint: t.paint,
                last_paint_culled_ops: t.paint_culled_ops,
                last_gpu_upload: t.gpu_upload,
                last_snapshot: t.snapshot,
                last_text_layout_cache_hits: t.text_layout_cache.hits,
                last_text_layout_cache_misses: t.text_layout_cache.misses,
                last_text_layout_cache_evictions: t.text_layout_cache.evictions,
                last_text_layout_shaped_bytes: t.text_layout_cache.shaped_bytes,
                trigger,
                working_color_space: gfx.renderer.working_color_space(),
                color_management: gfx.color.status().clone(),
                surface_color: Some(gfx.surface_color.clone()),
                ..HostDiagnostics::default()
            };
            self.app.before_build();
            self.app
                .before_paint(&gfx.device, &gfx.queue, viewport, scale_factor);
            let theme = self.app.theme();
            let palette = theme.palette().clone();
            let cx = damascene_core::BuildCx::new(&theme)
                .with_ui_state(gfx.renderer.ui_state())
                .with_diagnostics(&diagnostics)
                .with_viewport(viewport.w, viewport.h);
            let mut tree = self.app.build(&cx);
            gfx.renderer.set_theme(theme);
            // Yield hotkeys to a focused text input. Registered chords
            // otherwise beat a `capture_keys` widget's own key handling
            // (the runtime checks hotkeys first), so a bare 'j' or a
            // global Ctrl+C would hijack typing / clipboard in the search
            // field. The renderer reflects the previous frame's focus
            // here — a one-frame lag that's harmless, since focusing a
            // field is itself a redraw before any key is typed.
            let hotkeys = if gfx.renderer.focused_captures_keys() {
                Vec::new()
            } else {
                self.app.hotkeys()
            };
            gfx.renderer.set_hotkeys(hotkeys);
            gfx.renderer.set_selection(self.app.selection());
            gfx.renderer.push_toasts(self.app.drain_toasts());
            gfx.renderer
                .push_focus_requests(self.app.drain_focus_requests());
            gfx.renderer
                .push_scroll_requests(self.app.drain_scroll_requests());
            for url in self.app.drain_link_opens() {
                open_link(&url);
            }
            let t_after_build = Instant::now();
            let prepare =
                gfx.renderer
                    .prepare(&gfx.device, &gfx.queue, &mut tree, viewport, scale_factor);
            let t_after_prepare = Instant::now();
            // Cursor resolution needs the laid-out tree, so it only
            // updates on the full path; paint-only frames inherit it.
            let cursor = gfx.renderer.ui_state().cursor(&tree);
            if cursor != self.last_cursor {
                gfx.window.set_cursor(winit_cursor(cursor));
                self.last_cursor = cursor;
            }
            (prepare, palette, t_after_build, t_after_prepare)
        };

        let mut encoder = gfx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("prism-explorer::encoder"),
            });
        gfx.renderer.render(
            &gfx.device,
            &mut encoder,
            &frame.texture,
            &view,
            gfx.msaa.as_ref().map(|msaa| &msaa.view),
            wgpu::LoadOp::Clear(bg_color(&palette, gfx.renderer.working_color_space())),
        );
        gfx.queue.submit(Some(encoder.finish()));
        frame.present();

        self.last_build = t_after_build - frame_start;
        self.last_prepare = t_after_prepare - t_after_build;
        self.last_submit = Instant::now() - t_after_prepare;
        self.last_timings = prepare.timings;

        // Reschedule the two redraw lanes. On a paint-only frame only
        // the paint lane updates — repaint reports
        // next_layout_redraw_in = None because it didn't re-evaluate
        // that signal, so the parked layout deadline stands.
        let now = Instant::now();
        if !paint_only {
            match prepare.next_layout_redraw_in {
                None => self.next_layout_redraw = None,
                Some(d) if d.is_zero() => {
                    self.next_layout_redraw = None;
                    self.next_trigger = FrameTrigger::Animation;
                    gfx.window.request_redraw();
                }
                Some(d) => self.next_layout_redraw = Some(now + d),
            }
        }
        match prepare.next_paint_redraw_in {
            None => self.next_paint_redraw = None,
            Some(d) if d.is_zero() => {
                self.next_paint_redraw = None;
                // Don't downgrade an Animation trigger set just above.
                if !matches!(self.next_trigger, FrameTrigger::Animation) {
                    self.next_trigger = FrameTrigger::ShaderPaint;
                }
                gfx.window.request_redraw();
            }
            Some(d) => self.next_paint_redraw = Some(now + d),
        }
    }
}

/// Clear color: the background token converted into the negotiated
/// working space, exactly like every painted fill.
fn bg_color(
    palette: &damascene_core::Palette,
    working: damascene_core::color::ColorSpace,
) -> wgpu::Color {
    let [r, g, b, a] = damascene_core::paint::rgba_f32_in(palette.background, working);
    wgpu::Color {
        r: r as f64,
        g: g as f64,
        b: b as f64,
        a: a as f64,
    }
}

fn open_link(url: &str) {
    let spawned = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(err) = spawned {
        tracing::warn!(error = %err, url, "failed to open link");
    }
}

type Clipboard = Option<arboard::Clipboard>;

/// Dispatch one UI event to the app; if the app's text selection
/// changed in response, mirror it into the Linux primary selection.
fn dispatch_app_event(
    app: &mut dyn App,
    event: UiEvent,
    renderer: &damascene_wgpu::Runner,
    clipboard: &mut Clipboard,
    last_primary: &mut String,
) {
    let before = app.selection();
    let cx = damascene_core::EventCx::new().with_ui_state(renderer.ui_state());
    app.on_event(event, &cx);
    if app.selection() != before {
        sync_primary_selection(app, renderer, clipboard, last_primary);
    }
}

fn dispatch_app_wheel_event(
    app: &mut dyn App,
    event: UiEvent,
    renderer: &damascene_wgpu::Runner,
    clipboard: &mut Clipboard,
    last_primary: &mut String,
) -> bool {
    let before = app.selection();
    let cx = damascene_core::EventCx::new().with_ui_state(renderer.ui_state());
    let consumed = app.on_wheel_event(event, &cx);
    if app.selection() != before {
        sync_primary_selection(app, renderer, clipboard, last_primary);
    }
    consumed
}

fn sync_primary_selection(
    app: &dyn App,
    renderer: &damascene_wgpu::Runner,
    clipboard: &mut Clipboard,
    last_primary: &mut String,
) {
    let text = renderer
        .selected_text_for(&app.selection())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    if text == *last_primary {
        return;
    }
    if !text.is_empty() {
        primary_set(clipboard, &text);
    }
    *last_primary = text;
}

/// Copy the current damascene text selection to the clipboard
/// (Ctrl+C / Ctrl+X path).
fn copy_current_selection(renderer: &damascene_wgpu::Runner, clipboard: &mut Clipboard) {
    let Some(text) = renderer.selected_text() else {
        return;
    };
    if let Some(cb) = clipboard {
        let _ = cb.set_text(text);
    }
}

/// Middle-click pastes the primary selection on Linux: attach its text
/// to the event before the app sees it.
fn attach_primary_selection_text(mut event: UiEvent, clipboard: &mut Clipboard) -> UiEvent {
    if event.kind == UiEventKind::MiddleClick {
        event.text = primary_get(clipboard);
    }
    event
}

fn get_clipboard_text(clipboard: &mut Clipboard) -> Option<String> {
    clipboard.as_mut()?.get_text().ok()
}

fn primary_set(clipboard: &mut Clipboard, text: &str) {
    use arboard::{LinuxClipboardKind, SetExtLinux};
    if let Some(cb) = clipboard {
        let _ = cb.set().clipboard(LinuxClipboardKind::Primary).text(text);
    }
}

fn primary_get(clipboard: &mut Clipboard) -> Option<String> {
    use arboard::{GetExtLinux, LinuxClipboardKind};
    let cb = clipboard.as_mut()?;
    cb.get().clipboard(LinuxClipboardKind::Primary).text().ok()
}
