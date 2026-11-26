//! Native Wayland (and X11 fallback) `winit 0.30` GUI shell for `omarex`.
//!
//! Architecture:
//! - Reactive event-driven loop: parks in `ControlFlow::Wait` (**0% idle CPU**) and wakes
//!   immediately via `EventLoopProxy<AppWakeEvent>` whenever `HostManager` queues a
//!   [`RuntimeEvent`], switching to `ControlFlow::WaitUntil(deadline)` only while a snapshot,
//!   coalesced resize, prompt timeout, Kitty animation, or working pulse dot is active.
//! - Native Hyprland Wayland integration: `app_id = "omarex"`, frameless decorations
//!   (`with_decorations(false)`), optional translucent surface matching `WindowMaterial`,
//!   HiDPI `ScaleFactorChanged` & `Resized` swapchain reconfiguring, and Wayland IME + DnD.
//! - Full international keyboard mapping (`winit::keyboard::KeyCode` -> `vt_engine::PhysicalKey`
//!   and `key_without_modifiers` -> canonical `tr3x-bridge::KeyChord`), supporting German QWERTZ,
//!   AltGr (`@`, `€`, `{`, `}`, `[`, `]`, `\`, `~`, `|`), Kitty Keyboard Protocol (`Shift+Enter`),
//!   Omarchy `Ctrl+Alt+*` `tr3x` aliases, `Ctrl+Shift+*` chrome, `Alt+1..9`, and
//!   `Ctrl+Insert` / `Shift+Insert`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gpu_renderer::{GpuConfig, GpuRenderer, PresentModePreference};
use omarchy_theme::{Theme, ThemePalette};
use rex_proto::RexEndpoint;
use tokio::sync::mpsc;
use tr3x_bridge::Modifiers;
use vt_engine::PhysicalKey;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{
    ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey as WinitPhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;
#[cfg(target_os = "linux")]
use winit::platform::x11::WindowAttributesExtX11;
use winit::window::{CursorIcon, Window, WindowAttributes, WindowId};

use crate::bridge::{
    CommandLaunchSpec, CursorHint, KeyInputEvent, OmarexController, PointerButton,
    register_all_ui_icons,
};
use crate::config::UserSettings;
use crate::host_manager::{HostCommand, HostManager, RuntimeEvent};
#[cfg(target_os = "linux")]
use crate::wayland_dnd::{WaylandDndEvent, WaylandDndHandle, WaylandDropPayload};

/// Custom event loop wakeup signal pushed by [`HostManager`]'s `wake_ui` callback or native
/// Wayland drag-and-drop listener.
#[derive(Debug, Clone, PartialEq)]
pub enum AppWakeEvent {
    RuntimeEventReady,
    #[cfg(target_os = "linux")]
    WaylandDnd(WaylandDndEvent),
}

/// Interactive GUI launch options parsed from CLI arguments (including `xdg-terminal-exec`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuiLaunchOptions {
    pub server_override: Option<String>,
    pub preferred_session: Option<String>,
    pub force_light: bool,
    /// Wayland `app_id` / X11 `WM_CLASS` override (`--class` / `--app-id`, default `"omarex"`).
    pub app_id: Option<String>,
    /// Pinned window title override (`--title`, e.g. `"Omarchy"` for TUI/presentation windows).
    pub window_title: Option<String>,
    /// Starting directory (`--working-directory` / `--dir`) for new shells or `-e` commands.
    pub working_directory: Option<String>,
    /// Standalone command to run (`-e <cmd> [args...]`) in an ephemeral Rex session.
    pub command: Option<Vec<String>>,
    /// Keep standalone `-e` window open after command exits (`--wait-after-command` / `--hold`).
    pub wait_after_command: bool,
}

/// Launches the native Wayland/Vulkan `omarex` desktop client.
pub fn run_gui(opts: GuiLaunchOptions) -> Result<()> {
    let local_endpoint = match opts.server_override.as_deref() {
        Some(raw) => RexEndpoint::parse(raw).context("failed to parse --server endpoint")?,
        None => RexEndpoint::discover_default(),
    };

    let settings_path = UserSettings::default_path();
    let settings = UserSettings::load_or_default();
    let mut theme = Theme::load_live();
    if opts.force_light {
        theme = Theme::from_parts(
            ThemePalette::default_light(),
            theme.rounding,
            settings.material.to_theme_material(),
        );
    } else {
        theme.material = settings.material.to_theme_material();
    }

    let mut controller = OmarexController::new(settings, settings_path, theme)
        .context("failed to initialize OmarexController")?;
    controller.set_preferred_session(opts.preferred_session);
    controller.set_default_working_directory(opts.working_directory.clone());
    if let Some(cmd) = opts.command {
        controller.set_command_launch(Some(CommandLaunchSpec {
            command: cmd,
            cwd: opts.working_directory,
            wait_after_command: opts.wait_after_command,
        }));
    }

    let app_id = opts
        .app_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "omarex".to_owned());
    let pinned_title = opts.window_title.filter(|s| !s.trim().is_empty());
    let initial_title = pinned_title.clone().unwrap_or_else(|| "omarex".to_owned());

    let event_loop = EventLoop::<AppWakeEvent>::with_user_event()
        .build()
        .context("failed to create Wayland/X11 event loop")?;
    let proxy: EventLoopProxy<AppWakeEvent> = event_loop.create_proxy();
    let wake_proxy = proxy.clone();

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("omarex-net")
        .build()
        .context("failed to build Tokio runtime")?;

    let wake_ui: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = wake_proxy.send_event(AppWakeEvent::RuntimeEventReady);
    });
    let hm_cfg = controller.build_host_manager_config(Some(local_endpoint), Some(wake_ui));

    let (host_manager, event_rx) = {
        let _guard = tokio_rt.enter();
        HostManager::spawn(hm_cfg)
    };
    controller.attach_host_manager(host_manager.clone());

    let mut app = OmarexApp {
        tokio_rt,
        host_manager,
        event_rx,
        proxy,
        controller,
        window: None,
        renderer: None,
        #[cfg(target_os = "linux")]
        wayland_dnd: None,
        app_id,
        pinned_title,
        modifiers: ModifiersState::empty(),
        pointer_logical: (0.0, 0.0),
        last_cursor: CursorHint::Default,
        last_window_title: initial_title,
        pending_drops: Vec::new(),
        started_at: Instant::now(),
        fatal_error: None,
        shutting_down: false,
    };

    event_loop
        .run_app(&mut app)
        .context("winit event loop terminated with error")?;

    app.shutdown_gracefully();

    if let Some(err) = app.fatal_error.take() {
        return Err(err);
    }
    Ok(())
}

struct OmarexApp {
    tokio_rt: tokio::runtime::Runtime,
    host_manager: HostManager,
    event_rx: mpsc::UnboundedReceiver<RuntimeEvent>,
    proxy: EventLoopProxy<AppWakeEvent>,
    controller: OmarexController,
    window: Option<Arc<Window>>,
    renderer: Option<GpuRenderer>,
    #[cfg(target_os = "linux")]
    wayland_dnd: Option<WaylandDndHandle>,
    app_id: String,
    pinned_title: Option<String>,
    modifiers: ModifiersState,
    pointer_logical: (f32, f32),
    last_cursor: CursorHint,
    last_window_title: String,
    pending_drops: Vec<PathBuf>,
    started_at: Instant,
    fatal_error: Option<anyhow::Error>,
    shutting_down: bool,
}

impl OmarexApp {
    fn drain_runtime_events(&mut self, now: Instant) -> bool {
        let mut drained = false;
        while let Ok(ev) = self.event_rx.try_recv() {
            self.controller.process_runtime_event(ev, now);
            drained = true;
        }
        drained
    }

    fn flush_pending_drops(&mut self) {
        if !self.pending_drops.is_empty() {
            let paths = std::mem::take(&mut self.pending_drops);
            self.controller
                .handle_file_drop_at(&paths, Some(self.pointer_logical));
        }
    }

    fn sync_cursor_and_title(&mut self) {
        let Some(window) = &self.window else {
            return;
        };

        let hint = self.controller.cursor_hint();
        if hint != self.last_cursor {
            window.set_cursor(cursor_hint_to_winit(hint));
            self.last_cursor = hint;
        }

        let desired_title = if let Some(pinned) = &self.pinned_title {
            pinned.clone()
        } else {
            self.controller
                .active_session_key()
                .and_then(|sk| self.controller.workspace.session(sk))
                .filter(|s| !s.label.is_empty())
                .map(|s| format!("omarex — {}", s.label))
                .unwrap_or_else(|| "omarex".to_owned())
        };
        if desired_title != self.last_window_title {
            window.set_title(&desired_title);
            self.last_window_title = desired_title;
        }
    }

    fn shutdown_gracefully(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        #[cfg(target_os = "linux")]
        {
            // Stop the foreign-display Wayland DnD thread before the `winit` window/display drops.
            self.wayland_dnd = None;
        }
        if let Some(sk) = self.controller.ephemeral_session().cloned() {
            let _ = self.host_manager.send(HostCommand::DestroySession {
                host_id: sk.host_id,
                session_id: sk.session_id,
            });
            self.tokio_rt.block_on(async {
                tokio::time::sleep(Duration::from_millis(60)).await;
            });
        } else if !self.controller.read_only
            && let Some(sk) = self.controller.active_session_key().cloned()
        {
            // Release block ownership cleanly right before closing so any remaining tiled
            // `omarex` window immediately receives `block_owner_changed` and expands to full width.
            let _ = self.host_manager.send(HostCommand::DisownBlock {
                host_id: sk.host_id,
                session_id: sk.session_id,
                block_ids: None,
            });
            self.tokio_rt.block_on(async {
                tokio::time::sleep(Duration::from_millis(25)).await;
            });
        }
        self.host_manager.shutdown();
        // Give WS close frames (status 1000) a short window to flush cleanly.
        self.tokio_rt.block_on(async {
            tokio::time::sleep(Duration::from_millis(45)).await;
        });
    }

    fn sync_window_dimensions(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let phys = window.inner_size();
        if phys.width == 0 || phys.height == 0 {
            return;
        }
        let scale = (window.scale_factor() as f32).max(0.5);
        if let Some(renderer) = self.renderer.as_mut()
            && (renderer.context().surface_size() != Some((phys.width, phys.height))
                || (renderer.context().scale_factor() - scale).abs() > f32::EPSILON)
        {
            renderer.resize(phys.width, phys.height, scale);
        }
        let expected_w = (phys.width as f32 / scale).max(1.0);
        let expected_h = (phys.height as f32 / scale).max(1.0);
        let (cur_w, cur_h) = self.controller.viewport_logical();
        if (cur_w - expected_w).abs() > 0.5
            || (cur_h - expected_h).abs() > 0.5
            || (self.controller.scale_factor() - scale).abs() > f32::EPSILON
        {
            self.controller.set_viewport(phys.width, phys.height, scale);
        }
    }

    fn render_now(&mut self) {
        let now = Instant::now();
        self.drain_runtime_events(now);
        self.flush_pending_drops();
        self.sync_window_dimensions();

        let (Some(window), Some(renderer)) = (self.window.as_ref(), self.renderer.as_mut()) else {
            return;
        };

        let frame = match self.controller.render_frame(renderer, now) {
            Ok(frame) => frame,
            Err(err) => {
                tracing::error!(error = %err, "failed to build composite frame");
                return;
            }
        };

        window.pre_present_notify();
        match renderer.render_to_surface(&frame) {
            Ok(Some(_stats)) => {}
            Ok(None) => {
                // Surface was lost/outdated and auto-reconfigured; schedule one immediate repaint.
                self.controller.mark_needs_redraw();
                window.request_redraw();
            }
            Err(err) => {
                tracing::warn!(error = %err, "surface render failed");
                self.controller.mark_needs_redraw();
            }
        }
        self.sync_cursor_and_title();
    }
}

impl ApplicationHandler<AppWakeEvent> for OmarexApp {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            let now = Instant::now();
            self.drain_runtime_events(now);
            let _ = self.controller.tick(now);
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let transparent = self.controller.has_translucent_material();
        let mut attrs = WindowAttributes::default()
            .with_title(self.last_window_title.clone())
            .with_inner_size(LogicalSize::new(1440.0_f64, 900.0_f64))
            .with_min_inner_size(LogicalSize::new(480.0_f64, 320.0_f64))
            .with_decorations(false)
            .with_transparent(transparent);

        #[cfg(target_os = "linux")]
        {
            attrs = WindowAttributesExtWayland::with_name(attrs, &self.app_id, &self.app_id);
            attrs = WindowAttributesExtX11::with_name(attrs, &self.app_id, &self.app_id);
        }

        let window = match event_loop.create_window(attrs) {
            Ok(win) => Arc::new(win),
            Err(err) => {
                self.fatal_error = Some(anyhow::anyhow!("failed to create window: {err}"));
                event_loop.exit();
                return;
            }
        };
        window.set_decorations(false);
        window.set_ime_allowed(true);

        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);
        let scale = (window.scale_factor() as f32).max(0.5);

        let gpu_cfg = GpuConfig {
            present_mode: PresentModePreference::Fifo,
            transparent_surface: transparent,
            ..Default::default()
        };
        let mut renderer =
            match GpuRenderer::new_with_surface(Arc::clone(&window), width, height, scale, gpu_cfg)
            {
                Ok(r) => r,
                Err(err) => {
                    self.fatal_error =
                        Some(anyhow::anyhow!("failed to initialize GPU renderer: {err}"));
                    event_loop.exit();
                    return;
                }
            };

        let icon_count = register_all_ui_icons(renderer.atlas_mut());
        tracing::info!(
            adapter = %renderer.adapter_summary().name,
            backend = ?renderer.adapter_summary().backend,
            width,
            height,
            scale,
            transparent,
            icons = icon_count,
            "omarex window surface ready"
        );

        self.controller.set_viewport(width, height, scale);
        #[cfg(target_os = "linux")]
        {
            let dnd_proxy = self.proxy.clone();
            self.wayland_dnd = WaylandDndHandle::attach(window.as_ref(), move |ev| {
                let _ = dnd_proxy.send_event(AppWakeEvent::WaylandDnd(ev));
            });
        }
        self.window = Some(Arc::clone(&window));
        self.renderer = Some(renderer);
        window.request_redraw();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppWakeEvent) {
        let now = Instant::now();
        match event {
            AppWakeEvent::RuntimeEventReady => {
                self.drain_runtime_events(now);
                let _ = self.controller.tick(now);
            }
            #[cfg(target_os = "linux")]
            AppWakeEvent::WaylandDnd(dnd_ev) => match dnd_ev {
                WaylandDndEvent::Hover { x, y, .. } => {
                    self.pointer_logical = (x, y);
                    self.controller.handle_file_hover(x, y);
                }
                WaylandDndEvent::Leave => {
                    self.controller.handle_file_hover_cancelled();
                }
                WaylandDndEvent::Drop { x, y, payload } => {
                    self.pointer_logical = (x, y);
                    match payload {
                        WaylandDropPayload::Files(paths) => {
                            self.controller.handle_file_drop_at(&paths, Some((x, y)));
                        }
                        WaylandDropPayload::Text(text) => {
                            self.controller
                                .handle_text_drop_at(&text, Some((x, y)), now);
                        }
                    }
                }
            },
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let now = Instant::now();
        match event {
            WindowEvent::CloseRequested => {
                self.shutdown_gracefully();
                event_loop.exit();
            }

            WindowEvent::Resized(phys) => {
                if phys.width > 0 && phys.height > 0 {
                    let scale = self
                        .window
                        .as_ref()
                        .map_or(1.0, |w| (w.scale_factor() as f32).max(0.5));
                    if let Some(renderer) = self.renderer.as_mut() {
                        renderer.resize(phys.width, phys.height, scale);
                    }
                    self.controller.set_viewport(phys.width, phys.height, scale);
                    self.controller.on_viewport_resized(now);
                    // Paint immediately so Hyprland composites the resized surface with zero stale frame.
                    self.render_now();
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let scale = (scale_factor as f32).max(0.5);
                if let Some(win) = &self.window {
                    let phys = win.inner_size();
                    let w = phys.width.max(1);
                    let h = phys.height.max(1);
                    if let Some(renderer) = self.renderer.as_mut() {
                        renderer.resize(w, h, scale);
                    }
                    self.controller.set_viewport(w, h, scale);
                    self.controller.on_viewport_resized(now);
                    self.render_now();
                }
            }

            WindowEvent::Focused(gained) => {
                self.controller.handle_window_focus(gained, now);
            }

            WindowEvent::ModifiersChanged(new_mods) => {
                self.modifiers = new_mods.state();
            }

            WindowEvent::KeyboardInput {
                event: key_ev,
                is_synthetic,
                ..
            } => {
                if is_synthetic {
                    return;
                }
                if let Some(input) = translate_winit_key_event(&key_ev, self.modifiers) {
                    self.controller.handle_key(&input, now);
                }
            }

            WindowEvent::Ime(ime) => match ime {
                Ime::Enabled => {}
                Ime::Preedit(text, range) => {
                    let preedit = if text.is_empty() {
                        None
                    } else {
                        Some((text, range.map(|(s, e)| s..e)))
                    };
                    self.controller.handle_ime(preedit, None, now);
                }
                Ime::Commit(text) => {
                    self.controller.handle_ime(None, Some(text), now);
                }
                Ime::Disabled => {
                    self.controller.handle_ime(None, None, now);
                }
            },

            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.controller.scale_factor().max(0.5);
                let px = position.x as f32 / scale;
                let py = position.y as f32 / scale;
                self.pointer_logical = (px, py);
                self.controller.handle_pointer_move(
                    px,
                    py,
                    winit_mods_to_bridge(self.modifiers),
                    now,
                );
            }

            WindowEvent::CursorLeft { .. } => {
                self.controller.handle_pointer_leave();
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let Some(btn) = winit_mouse_button(button) else {
                    return;
                };
                let (px, py) = self.pointer_logical;
                let mods = winit_mods_to_bridge(self.modifiers);
                match state {
                    ElementState::Pressed => {
                        let time_ns = self.started_at.elapsed().as_nanos() as u64;
                        self.controller
                            .handle_pointer_down(px, py, btn, mods, time_ns, now);
                    }
                    ElementState::Released => {
                        self.controller.handle_pointer_up(px, py, btn, mods);
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let scale = self.controller.scale_factor().max(0.5);
                let (dx_px, dy_px) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 54.0, y * 54.0),
                    MouseScrollDelta::PixelDelta(PhysicalPosition { x, y }) => {
                        (x as f32 / scale, y as f32 / scale)
                    }
                };
                let (px, py) = self.pointer_logical;
                self.controller.handle_pointer_wheel(
                    px,
                    py,
                    dx_px,
                    dy_px,
                    winit_mods_to_bridge(self.modifiers),
                );
            }

            WindowEvent::HoveredFile(_) => {
                let (px, py) = self.pointer_logical;
                self.controller.handle_file_hover(px, py);
            }

            WindowEvent::HoveredFileCancelled => {
                self.controller.handle_file_hover_cancelled();
            }

            WindowEvent::DroppedFile(path) => {
                self.pending_drops.push(path);
            }

            WindowEvent::RedrawRequested => {
                self.render_now();
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        self.drain_runtime_events(now);
        self.flush_pending_drops();
        let _ = self.controller.tick(now);
        self.sync_cursor_and_title();

        if self.controller.should_exit() {
            self.shutdown_gracefully();
            event_loop.exit();
            return;
        }

        if self.controller.needs_redraw() {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }

        match self.controller.next_wakeup_deadline(now) {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

// ============================================================================
// Platform Input Translation (Wayland / X11 -> `bridge` + `vt-engine`)
// ============================================================================

/// Converts `winit`'s [`ModifiersState`] into `tr3x-bridge`'s [`Modifiers`].
#[inline]
pub fn winit_mods_to_bridge(state: ModifiersState) -> Modifiers {
    Modifiers {
        ctrl: state.control_key(),
        alt: state.alt_key(),
        shift: state.shift_key(),
        super_key: state.super_key(),
    }
}

/// Converts a `winit` [`KeyEvent`] into a normalized [`KeyInputEvent`] for [`OmarexController`].
///
/// Properly distinguishes:
/// - Layout-independent physical scancode (`KeyCode` -> `vt_engine::PhysicalKey`)
/// - Unshifted base character via `key_without_modifiers()` (German QWERTZ, AltGr, `Shift+7` -> `/`)
/// - Control/shortcut suppression of raw control text (`\r`, `\x1b`, `\x08`, `\x7f`) so
///   `libghostty-vt` emits exact Kitty Keyboard Protocol sequences (`Shift+Enter` -> `\x1b[13;2u`)
pub fn translate_winit_key_event(
    event: &KeyEvent,
    mods_state: ModifiersState,
) -> Option<KeyInputEvent> {
    let physical_key = match event.physical_key {
        WinitPhysicalKey::Code(code) => winit_keycode_to_physical(code),
        WinitPhysicalKey::Unidentified(_) => PhysicalKey::Unidentified,
    };

    let mut mods = winit_mods_to_bridge(mods_state);

    // On some XKB layouts AltGr (`NamedKey::AltGraph`) may report without `ModifiersState::ALT`
    // or with `ALT`; never drop the produced character when `logical_key` is a printable char.
    let unshifted_key = event.key_without_modifiers();
    let raw_text = event
        .text
        .as_deref()
        .or_else(|| event.text_with_all_modifiers());

    let (canonical_key, unshifted_codepoint, text) = normalize_logical_key(
        &event.logical_key,
        &unshifted_key,
        physical_key,
        raw_text,
        &mut mods,
    )?;

    Some(KeyInputEvent {
        physical_key,
        canonical_key,
        text,
        unshifted_codepoint,
        mods,
        pressed: event.state == ElementState::Pressed,
        repeat: event.repeat,
        composing: false,
    })
}

fn normalize_logical_key(
    logical_key: &Key,
    unshifted_key: &Key,
    physical_key: PhysicalKey,
    raw_text: Option<&str>,
    mods: &mut Modifiers,
) -> Option<(String, u32, Option<String>)> {
    // 1. Named non-character keys (`Enter`, `Escape`, `Insert`, arrows, `F1..F24`, etc.)
    if let Key::Named(named) = logical_key {
        let (name, cp) = named_key_to_canonical(*named)?;
        // Only Space produces printable text when no non-shift shortcut modifier is held.
        let text = if *named == NamedKey::Space && !mods.ctrl && !mods.alt && !mods.super_key {
            Some(" ".to_owned())
        } else {
            None
        };
        return Some((name.to_owned(), cp, text));
    }

    // 2. Character keys (ASCII, German `ä`/`ö`/`ü`/`ß`, AltGr `@`/`€`/`{`/`}`, etc.)
    if let Key::Character(ch_str) = logical_key {
        let unshifted_char = match unshifted_key {
            Key::Character(u) => u.chars().next(),
            _ => ch_str.chars().next(),
        };
        let unshifted_cp = unshifted_char.map_or(0, |c| c as u32);

        // Canonical key for shortcut lookup prefers lowercase unshifted character when Ctrl/Alt/Super
        // is held (e.g. `Ctrl+Shift+P` -> `"p"`, `Ctrl+Alt+I` -> `"i"`, `Alt+1` -> `"1"`), and
        // falls back to lowercase produced character otherwise.
        let canonical = if mods.ctrl || mods.alt || mods.super_key {
            unshifted_char
                .map(|c| c.to_lowercase().to_string())
                .unwrap_or_else(|| ch_str.to_lowercase())
        } else {
            ch_str.to_lowercase()
        };

        // Filter out C0/DEL control strings emitted by winit when Ctrl is held (`"\x03"`, `"\r"`, etc.)
        // so `libghostty-vt` encodes control chords cleanly according to Kitty/legacy flags.
        let candidate_text = raw_text.or(Some(ch_str.as_str())).filter(|s| {
            !s.is_empty() && !s.chars().all(char::is_control) && !mods.ctrl && !mods.super_key
        });

        // If AltGr produced a distinct character (e.g. `q` -> `@`, `7` -> `{`, `e` -> `€`),
        // clear `mods.alt` if it would otherwise be misinterpreted as a Meta shortcut key!
        let text = candidate_text.map(str::to_owned);
        if mods.alt
            && !mods.ctrl
            && !mods.super_key
            && let Some(ref produced) = text
            && let Some(base) = unshifted_char
            && produced.chars().count() == 1
            && !produced.starts_with(base)
            && !produced.eq_ignore_ascii_case(&base.to_string())
        {
            mods.alt = false;
        }

        return Some((canonical, unshifted_cp, text));
    }

    // 3. Dead keys / unidentified fallback via physical key
    let fallback = physical_key_fallback_name(physical_key)?;
    Some((fallback.to_owned(), 0, None))
}

fn named_key_to_canonical(named: NamedKey) -> Option<(&'static str, u32)> {
    Some(match named {
        NamedKey::Enter => ("enter", 13),
        NamedKey::Escape => ("escape", 27),
        NamedKey::Tab => ("tab", 9),
        NamedKey::Backspace => ("backspace", 8),
        NamedKey::Space => ("space", 32),
        NamedKey::Delete => ("delete", 0),
        NamedKey::Insert => ("insert", 0),
        NamedKey::Home => ("home", 0),
        NamedKey::End => ("end", 0),
        NamedKey::PageUp => ("page_up", 0),
        NamedKey::PageDown => ("page_down", 0),
        NamedKey::ArrowUp => ("up", 0),
        NamedKey::ArrowDown => ("down", 0),
        NamedKey::ArrowLeft => ("left", 0),
        NamedKey::ArrowRight => ("right", 0),
        NamedKey::F1 => ("f1", 0),
        NamedKey::F2 => ("f2", 0),
        NamedKey::F3 => ("f3", 0),
        NamedKey::F4 => ("f4", 0),
        NamedKey::F5 => ("f5", 0),
        NamedKey::F6 => ("f6", 0),
        NamedKey::F7 => ("f7", 0),
        NamedKey::F8 => ("f8", 0),
        NamedKey::F9 => ("f9", 0),
        NamedKey::F10 => ("f10", 0),
        NamedKey::F11 => ("f11", 0),
        NamedKey::F12 => ("f12", 0),
        NamedKey::F13 => ("f13", 0),
        NamedKey::F14 => ("f14", 0),
        NamedKey::F15 => ("f15", 0),
        NamedKey::F16 => ("f16", 0),
        NamedKey::F17 => ("f17", 0),
        NamedKey::F18 => ("f18", 0),
        NamedKey::F19 => ("f19", 0),
        NamedKey::F20 => ("f20", 0),
        NamedKey::F21 => ("f21", 0),
        NamedKey::F22 => ("f22", 0),
        NamedKey::F23 => ("f23", 0),
        NamedKey::F24 => ("f24", 0),
        NamedKey::Shift => ("shift", 0),
        NamedKey::Control => ("ctrl", 0),
        NamedKey::Alt => ("alt", 0),
        NamedKey::AltGraph => ("alt_graph", 0),
        NamedKey::Super | NamedKey::Meta => ("super", 0),
        NamedKey::CapsLock => ("caps_lock", 0),
        NamedKey::NumLock => ("num_lock", 0),
        NamedKey::ContextMenu => ("context_menu", 0),
        NamedKey::PrintScreen => ("print_screen", 0),
        NamedKey::ScrollLock => ("scroll_lock", 0),
        NamedKey::Pause => ("pause", 0),
        NamedKey::Copy => ("copy", 0),
        NamedKey::Cut => ("cut", 0),
        NamedKey::Paste => ("paste", 0),
        _ => return None,
    })
}

fn physical_key_fallback_name(key: PhysicalKey) -> Option<&'static str> {
    Some(match key {
        PhysicalKey::Enter | PhysicalKey::NumpadEnter => "enter",
        PhysicalKey::Escape => "escape",
        PhysicalKey::Tab => "tab",
        PhysicalKey::Backspace => "backspace",
        PhysicalKey::Space => "space",
        PhysicalKey::Delete => "delete",
        PhysicalKey::Insert => "insert",
        PhysicalKey::Home => "home",
        PhysicalKey::End => "end",
        PhysicalKey::PageUp => "page_up",
        PhysicalKey::PageDown => "page_down",
        PhysicalKey::ArrowUp => "up",
        PhysicalKey::ArrowDown => "down",
        PhysicalKey::ArrowLeft => "left",
        PhysicalKey::ArrowRight => "right",
        _ => return None,
    })
}

/// Complete W3C physical scancode mapping from `winit::keyboard::KeyCode` to
/// `vt_engine::PhysicalKey` (matching `libghostty-vt`'s `GhosttyKey` 1:1).
pub fn winit_keycode_to_physical(code: KeyCode) -> PhysicalKey {
    match code {
        // Writing System Keys (W3C § 3.1.1)
        KeyCode::Backquote => PhysicalKey::Backquote,
        KeyCode::Backslash => PhysicalKey::Backslash,
        KeyCode::BracketLeft => PhysicalKey::BracketLeft,
        KeyCode::BracketRight => PhysicalKey::BracketRight,
        KeyCode::Comma => PhysicalKey::Comma,
        KeyCode::Digit0 => PhysicalKey::Digit0,
        KeyCode::Digit1 => PhysicalKey::Digit1,
        KeyCode::Digit2 => PhysicalKey::Digit2,
        KeyCode::Digit3 => PhysicalKey::Digit3,
        KeyCode::Digit4 => PhysicalKey::Digit4,
        KeyCode::Digit5 => PhysicalKey::Digit5,
        KeyCode::Digit6 => PhysicalKey::Digit6,
        KeyCode::Digit7 => PhysicalKey::Digit7,
        KeyCode::Digit8 => PhysicalKey::Digit8,
        KeyCode::Digit9 => PhysicalKey::Digit9,
        KeyCode::Equal => PhysicalKey::Equal,
        KeyCode::IntlBackslash => PhysicalKey::IntlBackslash,
        KeyCode::IntlRo => PhysicalKey::IntlRo,
        KeyCode::IntlYen => PhysicalKey::IntlYen,
        KeyCode::KeyA => PhysicalKey::KeyA,
        KeyCode::KeyB => PhysicalKey::KeyB,
        KeyCode::KeyC => PhysicalKey::KeyC,
        KeyCode::KeyD => PhysicalKey::KeyD,
        KeyCode::KeyE => PhysicalKey::KeyE,
        KeyCode::KeyF => PhysicalKey::KeyF,
        KeyCode::KeyG => PhysicalKey::KeyG,
        KeyCode::KeyH => PhysicalKey::KeyH,
        KeyCode::KeyI => PhysicalKey::KeyI,
        KeyCode::KeyJ => PhysicalKey::KeyJ,
        KeyCode::KeyK => PhysicalKey::KeyK,
        KeyCode::KeyL => PhysicalKey::KeyL,
        KeyCode::KeyM => PhysicalKey::KeyM,
        KeyCode::KeyN => PhysicalKey::KeyN,
        KeyCode::KeyO => PhysicalKey::KeyO,
        KeyCode::KeyP => PhysicalKey::KeyP,
        KeyCode::KeyQ => PhysicalKey::KeyQ,
        KeyCode::KeyR => PhysicalKey::KeyR,
        KeyCode::KeyS => PhysicalKey::KeyS,
        KeyCode::KeyT => PhysicalKey::KeyT,
        KeyCode::KeyU => PhysicalKey::KeyU,
        KeyCode::KeyV => PhysicalKey::KeyV,
        KeyCode::KeyW => PhysicalKey::KeyW,
        KeyCode::KeyX => PhysicalKey::KeyX,
        KeyCode::KeyY => PhysicalKey::KeyY,
        KeyCode::KeyZ => PhysicalKey::KeyZ,
        KeyCode::Minus => PhysicalKey::Minus,
        KeyCode::Period => PhysicalKey::Period,
        KeyCode::Quote => PhysicalKey::Quote,
        KeyCode::Semicolon => PhysicalKey::Semicolon,
        KeyCode::Slash => PhysicalKey::Slash,

        // Functional Keys (W3C § 3.1.2)
        KeyCode::AltLeft => PhysicalKey::AltLeft,
        KeyCode::AltRight => PhysicalKey::AltRight,
        KeyCode::Backspace => PhysicalKey::Backspace,
        KeyCode::CapsLock => PhysicalKey::CapsLock,
        KeyCode::ContextMenu => PhysicalKey::ContextMenu,
        KeyCode::ControlLeft => PhysicalKey::ControlLeft,
        KeyCode::ControlRight => PhysicalKey::ControlRight,
        KeyCode::Enter => PhysicalKey::Enter,
        KeyCode::SuperLeft => PhysicalKey::MetaLeft,
        KeyCode::SuperRight => PhysicalKey::MetaRight,
        KeyCode::ShiftLeft => PhysicalKey::ShiftLeft,
        KeyCode::ShiftRight => PhysicalKey::ShiftRight,
        KeyCode::Space => PhysicalKey::Space,
        KeyCode::Tab => PhysicalKey::Tab,
        KeyCode::Convert => PhysicalKey::Convert,
        KeyCode::KanaMode => PhysicalKey::KanaMode,
        KeyCode::NonConvert => PhysicalKey::NonConvert,

        // Control Pad Section (W3C § 3.2)
        KeyCode::Delete => PhysicalKey::Delete,
        KeyCode::End => PhysicalKey::End,
        KeyCode::Help => PhysicalKey::Help,
        KeyCode::Home => PhysicalKey::Home,
        KeyCode::Insert => PhysicalKey::Insert,
        KeyCode::PageDown => PhysicalKey::PageDown,
        KeyCode::PageUp => PhysicalKey::PageUp,

        // Arrow Pad Section (W3C § 3.3)
        KeyCode::ArrowDown => PhysicalKey::ArrowDown,
        KeyCode::ArrowLeft => PhysicalKey::ArrowLeft,
        KeyCode::ArrowRight => PhysicalKey::ArrowRight,
        KeyCode::ArrowUp => PhysicalKey::ArrowUp,

        // Numpad Section (W3C § 3.4)
        KeyCode::NumLock => PhysicalKey::NumLock,
        KeyCode::Numpad0 => PhysicalKey::Numpad0,
        KeyCode::Numpad1 => PhysicalKey::Numpad1,
        KeyCode::Numpad2 => PhysicalKey::Numpad2,
        KeyCode::Numpad3 => PhysicalKey::Numpad3,
        KeyCode::Numpad4 => PhysicalKey::Numpad4,
        KeyCode::Numpad5 => PhysicalKey::Numpad5,
        KeyCode::Numpad6 => PhysicalKey::Numpad6,
        KeyCode::Numpad7 => PhysicalKey::Numpad7,
        KeyCode::Numpad8 => PhysicalKey::Numpad8,
        KeyCode::Numpad9 => PhysicalKey::Numpad9,
        KeyCode::NumpadAdd => PhysicalKey::NumpadAdd,
        KeyCode::NumpadBackspace => PhysicalKey::NumpadBackspace,
        KeyCode::NumpadClear => PhysicalKey::NumpadClear,
        KeyCode::NumpadClearEntry => PhysicalKey::NumpadClearEntry,
        KeyCode::NumpadComma => PhysicalKey::NumpadComma,
        KeyCode::NumpadDecimal => PhysicalKey::NumpadDecimal,
        KeyCode::NumpadDivide => PhysicalKey::NumpadDivide,
        KeyCode::NumpadEnter => PhysicalKey::NumpadEnter,
        KeyCode::NumpadEqual => PhysicalKey::NumpadEqual,
        KeyCode::NumpadMemoryAdd => PhysicalKey::NumpadMemoryAdd,
        KeyCode::NumpadMemoryClear => PhysicalKey::NumpadMemoryClear,
        KeyCode::NumpadMemoryRecall => PhysicalKey::NumpadMemoryRecall,
        KeyCode::NumpadMemoryStore => PhysicalKey::NumpadMemoryStore,
        KeyCode::NumpadMemorySubtract => PhysicalKey::NumpadMemorySubtract,
        KeyCode::NumpadMultiply => PhysicalKey::NumpadMultiply,
        KeyCode::NumpadParenLeft => PhysicalKey::NumpadParenLeft,
        KeyCode::NumpadParenRight => PhysicalKey::NumpadParenRight,
        KeyCode::NumpadSubtract => PhysicalKey::NumpadSubtract,

        // Function Section (W3C § 3.5)
        KeyCode::Escape => PhysicalKey::Escape,
        KeyCode::F1 => PhysicalKey::F1,
        KeyCode::F2 => PhysicalKey::F2,
        KeyCode::F3 => PhysicalKey::F3,
        KeyCode::F4 => PhysicalKey::F4,
        KeyCode::F5 => PhysicalKey::F5,
        KeyCode::F6 => PhysicalKey::F6,
        KeyCode::F7 => PhysicalKey::F7,
        KeyCode::F8 => PhysicalKey::F8,
        KeyCode::F9 => PhysicalKey::F9,
        KeyCode::F10 => PhysicalKey::F10,
        KeyCode::F11 => PhysicalKey::F11,
        KeyCode::F12 => PhysicalKey::F12,
        KeyCode::F13 => PhysicalKey::F13,
        KeyCode::F14 => PhysicalKey::F14,
        KeyCode::F15 => PhysicalKey::F15,
        KeyCode::F16 => PhysicalKey::F16,
        KeyCode::F17 => PhysicalKey::F17,
        KeyCode::F18 => PhysicalKey::F18,
        KeyCode::F19 => PhysicalKey::F19,
        KeyCode::F20 => PhysicalKey::F20,
        KeyCode::F21 => PhysicalKey::F21,
        KeyCode::F22 => PhysicalKey::F22,
        KeyCode::F23 => PhysicalKey::F23,
        KeyCode::F24 => PhysicalKey::F24,
        KeyCode::F25 => PhysicalKey::F25,
        KeyCode::Fn => PhysicalKey::Fn,
        KeyCode::FnLock => PhysicalKey::FnLock,
        KeyCode::PrintScreen => PhysicalKey::PrintScreen,
        KeyCode::ScrollLock => PhysicalKey::ScrollLock,
        KeyCode::Pause => PhysicalKey::Pause,

        // Media Keys (W3C § 3.6)
        KeyCode::BrowserBack => PhysicalKey::BrowserBack,
        KeyCode::BrowserFavorites => PhysicalKey::BrowserFavorites,
        KeyCode::BrowserForward => PhysicalKey::BrowserForward,
        KeyCode::BrowserHome => PhysicalKey::BrowserHome,
        KeyCode::BrowserRefresh => PhysicalKey::BrowserRefresh,
        KeyCode::BrowserSearch => PhysicalKey::BrowserSearch,
        KeyCode::BrowserStop => PhysicalKey::BrowserStop,
        KeyCode::Eject => PhysicalKey::Eject,
        KeyCode::LaunchApp1 => PhysicalKey::LaunchApp1,
        KeyCode::LaunchApp2 => PhysicalKey::LaunchApp2,
        KeyCode::LaunchMail => PhysicalKey::LaunchMail,
        KeyCode::MediaPlayPause => PhysicalKey::MediaPlayPause,
        KeyCode::MediaSelect => PhysicalKey::MediaSelect,
        KeyCode::MediaStop => PhysicalKey::MediaStop,
        KeyCode::MediaTrackNext => PhysicalKey::MediaTrackNext,
        KeyCode::MediaTrackPrevious => PhysicalKey::MediaTrackPrevious,
        KeyCode::Power => PhysicalKey::Power,
        KeyCode::Sleep => PhysicalKey::Sleep,
        KeyCode::AudioVolumeDown => PhysicalKey::AudioVolumeDown,
        KeyCode::AudioVolumeMute => PhysicalKey::AudioVolumeMute,
        KeyCode::AudioVolumeUp => PhysicalKey::AudioVolumeUp,
        KeyCode::WakeUp => PhysicalKey::WakeUp,

        // Legacy / Special Keys (W3C § 3.7)
        KeyCode::Copy => PhysicalKey::Copy,
        KeyCode::Cut => PhysicalKey::Cut,
        KeyCode::Paste => PhysicalKey::Paste,
        _ => PhysicalKey::Unidentified,
    }
}

fn winit_mouse_button(button: MouseButton) -> Option<PointerButton> {
    match button {
        MouseButton::Left => Some(PointerButton::Left),
        MouseButton::Middle => Some(PointerButton::Middle),
        MouseButton::Right => Some(PointerButton::Right),
        _ => None,
    }
}

fn cursor_hint_to_winit(hint: CursorHint) -> CursorIcon {
    match hint {
        CursorHint::Default => CursorIcon::Default,
        CursorHint::Pointer => CursorIcon::Pointer,
        CursorHint::Text => CursorIcon::Text,
        CursorHint::ColResize => CursorIcon::ColResize,
        CursorHint::RowResize => CursorIcon::RowResize,
        CursorHint::NwseResize => CursorIcon::NwseResize,
        CursorHint::NeswResize => CursorIcon::NeswResize,
        CursorHint::Grabbing => CursorIcon::Grabbing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_physical_scancodes_and_german_qwertz_keys() {
        assert_eq!(winit_keycode_to_physical(KeyCode::KeyA), PhysicalKey::KeyA);
        assert_eq!(
            winit_keycode_to_physical(KeyCode::Enter),
            PhysicalKey::Enter
        );
        assert_eq!(
            winit_keycode_to_physical(KeyCode::SuperLeft),
            PhysicalKey::MetaLeft
        );
        assert_eq!(
            winit_keycode_to_physical(KeyCode::IntlBackslash),
            PhysicalKey::IntlBackslash
        );
        assert_eq!(
            winit_keycode_to_physical(KeyCode::Insert),
            PhysicalKey::Insert
        );

        // German QWERTZ umlaut `ä` passes UTF-8 directly.
        let mut mods = Modifiers::NONE;
        let (canon, cp, text) = normalize_logical_key(
            &Key::Character("ä".into()),
            &Key::Character("ä".into()),
            PhysicalKey::Quote,
            Some("ä"),
            &mut mods,
        )
        .expect("normalize ä");
        assert_eq!(canon, "ä");
        assert_eq!(cp, 'ä' as u32);
        assert_eq!(text.as_deref(), Some("ä"));

        // German QWERTZ AltGr+Q -> `@` clears Alt modifier so it emits `@` instead of ESC-@!
        let mut alt_mods = Modifiers::ALT;
        let (_canon, cp, text) = normalize_logical_key(
            &Key::Character("@".into()),
            &Key::Character("q".into()),
            PhysicalKey::KeyQ,
            Some("@"),
            &mut alt_mods,
        )
        .expect("normalize AltGr @");
        assert_eq!(cp, 'q' as u32);
        assert_eq!(text.as_deref(), Some("@"));
        assert!(!alt_mods.alt, "AltGr transformed character must clear ALT");

        // Standard Alt+f keeps ALT intact for readline Meta-f!
        let mut meta_mods = Modifiers::ALT;
        let (canon, cp, text) = normalize_logical_key(
            &Key::Character("f".into()),
            &Key::Character("f".into()),
            PhysicalKey::KeyF,
            Some("f"),
            &mut meta_mods,
        )
        .expect("normalize Alt+f");
        assert_eq!(canon, "f");
        assert_eq!(cp, 'f' as u32);
        assert_eq!(text.as_deref(), Some("f"));
        assert!(meta_mods.alt);

        // Shift+Enter suppresses raw "\r" text so Kitty Keyboard Protocol emits CSI 13;2u cleanly.
        let mut shift_mods = Modifiers::SHIFT;
        let (canon, cp, text) = normalize_logical_key(
            &Key::Named(NamedKey::Enter),
            &Key::Named(NamedKey::Enter),
            PhysicalKey::Enter,
            Some("\r"),
            &mut shift_mods,
        )
        .expect("normalize Shift+Enter");
        assert_eq!(canon, "enter");
        assert_eq!(cp, 13);
        assert!(text.is_none());

        // Ctrl+Shift+P resolves canonical `"p"` with suppressed control text.
        let mut cs_mods = Modifiers::CTRL_SHIFT;
        let (canon, cp, text) = normalize_logical_key(
            &Key::Character("P".into()),
            &Key::Character("p".into()),
            PhysicalKey::KeyP,
            Some("\x10"),
            &mut cs_mods,
        )
        .expect("normalize Ctrl+Shift+P");
        assert_eq!(canon, "p");
        assert_eq!(cp, 'p' as u32);
        assert!(text.is_none());
    }
}
