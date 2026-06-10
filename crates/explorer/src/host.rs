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
//! focus, and frame pacing are all per-window. Glyph/MSDF atlases
//! duplicate per window for now (a `Runner` owns its atlases); shared
//! atlases are a later damascene-side improvement.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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
/// [`EventLoopProxy`]. The IO pool's notifier and the D-Bus services
/// send [`Wake`](HostCommand::Wake); window-opening commands (portal
/// picker requests, `ShowFolders` on a closed explorer) join this enum
/// as those features land.
#[derive(Debug)]
pub enum HostCommand {
    /// Data outside the UI trees changed (listing batch, decoded
    /// preview, D-Bus navigation message): rebuild every window.
    Wake,
}

/// A window to open: title, logical size, and the [`App`] that owns it.
pub struct WindowSpec {
    pub title: String,
    pub width: f32,
    pub height: f32,
    pub app: Box<dyn App>,
}

/// Run the host loop with one initial window. Returns when the last
/// window closes, or with `Err` if GPU bring-up for the first window
/// fails.
///
/// `config` supplies the color-preference ladder, MSAA sample count,
/// present-mode choice, and Wayland `app_id`; its run-loop knobs
/// (`redraw_interval`, `external_wakeup`) are not consulted — pacing
/// is two-lane per window, and wakeups arrive as [`HostCommand`]s on
/// the loop the caller built (see [`event_loop`]).
pub fn run(
    event_loop: EventLoop<HostCommand>,
    config: HostConfig,
    initial: WindowSpec,
) -> Result<(), String> {
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut host = Host {
        config,
        gpu: None,
        windows: HashMap::new(),
        initial: Some(initial),
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

struct Host {
    config: HostConfig,
    gpu: Option<Gpu>,
    windows: HashMap<WindowId, WindowState>,
    /// The first window's spec, consumed by `resumed()`.
    initial: Option<WindowSpec>,
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
    app: Box<dyn App>,
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
    fn create_window(&mut self, event_loop: &ActiveEventLoop, spec: WindowSpec) {
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
            let mut gfx =
                WindowGfx::new(&gpu.instance, &gpu.adapter, &gpu.device, &gpu.queue, window, &self.config)
                    .map_err(|e| format!("could not create a rendering surface: {e}"))?;
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
                self.windows.insert(state.gfx.window.id(), state);
            }
            Err(message) if first => {
                // No GPU, no app: surface the failure as run()'s Err.
                tracing::error!("{message}");
                self.setup_error = Some(message);
                event_loop.exit();
            }
            Err(message) => {
                tracing::error!(error = %message, "dropping window request");
            }
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

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: HostCommand) {
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
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let WindowEvent::CloseRequested = event {
            self.windows.remove(&id);
            // Resident-with-zero-windows is the end state, but nothing
            // re-opens a window yet (the portal service and
            // single-instance activation will). Until then a closed
            // last window means exit, not a zombie.
            if self.windows.is_empty() {
                event_loop.exit();
            }
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
                        for event in win.gfx.renderer.pointer_down(Pointer::mouse(lx, ly, button)) {
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
                    for event in win.gfx.renderer.key_down(key, win.modifiers, key_event.repeat) {
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
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
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
            let palette = gfx.renderer.theme().palette().clone();
            let t_after_build = Instant::now();
            let prepare = gfx
                .renderer
                .repaint(&gfx.device, &gfx.queue, viewport, scale_factor);
            (prepare, palette, t_after_build, Instant::now())
        } else {
            self.frame_index = self.frame_index.wrapping_add(1);
            let t = &self.last_timings;
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
            let theme = self.app.theme();
            let palette = theme.palette().clone();
            let cx = damascene_core::BuildCx::new(&theme)
                .with_ui_state(gfx.renderer.ui_state())
                .with_diagnostics(&diagnostics)
                .with_viewport(viewport.w, viewport.h);
            let mut tree = self.app.build(&cx);
            gfx.renderer.set_theme(theme);
            gfx.renderer.set_hotkeys(self.app.hotkeys());
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
