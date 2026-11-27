//! Window-independent `omarex` application controller bridging `HostManager` (Tokio / Rex rev 24),
//! `WorkspaceState`, `PaneStore` (`libghostty-vt`), `tr3x-bridge`, `UiTree` (`ui-chrome`),
//! `omarchy-theme`, and `GpuRenderer` (`wgpu` Vulkan 1.4).
//!
//! Designed so `app.rs` (`winit`) is a thin platform event-loop shell forwarding keyboard, pointer,
//! IME, clipboard, file-drop, and `RuntimeEvent` notifications directly into [`OmarexController`],
//! and so headless offscreen PNG renders execute the exact same frame pipeline without a display.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use gpu_renderer::{
    CellMetrics, ClipRect, Color, CornerRadii, CursorShape, DrawList, FrameStats, GlyphAtlas,
    GpuRenderer, GpuShared, Rect, SceneFrame, TerminalCursor, layers,
};
use omarchy_theme::{Rgba, Theme, ThemePalette};
use rex_proto::RexEndpoint;
use rex_proto::types::{
    BlockCreateSpec, InitialWindowSpec, KeymapDiagnostic, LayoutCreateSpec, LayoutNode,
    SessionCreateRequest, SessionNewWindowRequest, SessionView, TerminalResizeArgs, WindowView,
};
use tr3x_bridge::{
    ActionRegistry, BlockCardContext, KeymapOptions, Modifiers, PromptManager,
    build_proto_keymap_request, build_session_tab_cards, extract_project_name_from_pwd,
    is_placeholder_window_label, is_pr_layer, is_t3_session, program_status_from_proto,
    rollup_records,
};
use ui_chrome::{
    AudioViewerState, DiagnosticsHostRow, DiagnosticsSnapshot, DividerGeometry, DropZone,
    DropZoneHit, FilePickerPurpose, FloatingHitZone, FloatingLayerSpec, HitTarget,
    HostConnectionState, IconId, ImageViewerState, MarkdownInlineImage, ModalContent,
    Osc94Progress, PaneDensity, PaneHeaderMode, PaneMeta, PdfViewerState, PortBadge, RectF,
    ResolvedTarget, ResumeBadge, ScrollbarMetrics, SidebarHostView, SidebarSessionView,
    SidebarTabView, SplitAxis, SplitNode, TargetKind, TextViewerState, ToastItem, ToastSeverity,
    ToolbarBehavior, ToolbarPosition, UiFrameInput, UiRenderOutput, UiTree, VideoViewerState,
    ViewerContent,
};
use vt_engine::{MouseButton, Overscan, PhysicalKey, RowCellSpan, ScreenPoint, SelectionMode};

use crate::audio_chime::ChimeDispatcher;

use crate::config::{
    LayoutDensity, SavedPaneHeaderMode, SidebarBehavior, SidebarPosition, UserSettings,
    WindowMaterial,
};
use crate::host_manager::{HostCommand, HostManager, HostManagerConfig, LOCAL_HOST_ID};
use crate::media_viewer::{MediaEvent, MediaKind, MediaViewerBackend, RgbaFrame};
use crate::pane_store::{DEFAULT_BACKGROUND_SNAPSHOT_BUDGET, PaneStore, build_terminal_viewport};
use crate::workspace::{
    BlockKey, HostEntry, HostStatus, OwnershipTrigger, SessionKey, WorkspaceState,
    theme_palette_to_rex_args,
};

mod actions;
mod events;
mod input;
mod render;
#[cfg(test)]
mod tests;
mod viewer;

pub use render::{
    render_headless_live_png, render_headless_live_png_with_options, resolve_startup_theme,
    translate_ui_to_gpu, translate_ui_to_gpu_with_scale,
};

struct AtlasTextMeasurer<'a>(&'a mut GlyphAtlas);

impl ui_chrome::UiTextMeasurer for AtlasTextMeasurer<'_> {
    fn measure_text(
        &mut self,
        text: &str,
        size_px: f32,
        bold: bool,
        italic: bool,
        mono: bool,
    ) -> f32 {
        let lh = (size_px * 1.25).ceil();
        let family = if mono {
            gpu_renderer::FontFamily::Monospace
        } else {
            gpu_renderer::FontFamily::SansSerif
        };
        let weight = if bold {
            gpu_renderer::FontWeight::Bold
        } else {
            gpu_renderer::FontWeight::Regular
        };
        let style = if italic {
            gpu_renderer::FontStyle::Italic
        } else {
            gpu_renderer::FontStyle::Normal
        };
        self.0
            .measure_text_advance(text, size_px, lh, &family, weight, style)
    }
}

/// Registers all 40 embedded vector SVG icons from `ui-chrome` into [`GlyphAtlas`]
/// so every [`IconId`] rasterizes crisply via `resvg` + `tiny-skia`.
pub fn register_all_ui_icons(atlas: &mut GlyphAtlas) -> usize {
    for &id in IconId::ALL {
        atlas.register_svg(id.name(), id.svg_bytes());
    }
    IconId::ALL.len()
}

/// Window-independent keyboard event normalized by `app.rs` (`winit`) for [`OmarexController`].
#[derive(Debug, Clone)]
pub struct KeyInputEvent {
    pub physical_key: PhysicalKey,
    /// Canonical lowercase key name (`"a"`, `"enter"`, `"escape"`, `"["`, `"up"`, `"insert"`, etc.).
    pub canonical_key: String,
    pub text: Option<String>,
    pub unshifted_codepoint: u32,
    pub mods: Modifiers,
    pub pressed: bool,
    pub repeat: bool,
    pub composing: bool,
}

/// Mouse button passed by `app.rs` (`winit`) into [`OmarexController`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
}

impl PointerButton {
    fn to_vt(self) -> MouseButton {
        match self {
            Self::Left => MouseButton::Left,
            Self::Middle => MouseButton::Middle,
            Self::Right => MouseButton::Right,
        }
    }
}

/// Native cursor shape hint produced by [`OmarexController::cursor_hint`] for `app.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorHint {
    #[default]
    Default,
    Pointer,
    Text,
    ColResize,
    RowResize,
    NwseResize,
    NeswResize,
    Grabbing,
}

/// Built-in deterministic visual-QA scenarios for `--headless-render --headless-scenario <NAME>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeadlessScenario {
    SidebarDark,
    SidebarLight,
    SplitPanes,
    FloatingPr,
    CopyMode,
    SyntaxViewer,
    DiagnosticsHud,
    PortsPill,
    MarkdownViewer,
    ThemeSwitcher,
    SettingsModal,
    SettingsIosQr,
    SettingsServer,
    ProgramResume,
    DndSplit,
    MultiWindow,
}

impl HeadlessScenario {
    pub const ALL_NAMES: &'static [&'static str] = &[
        "sidebar-dark",
        "sidebar-light",
        "split-panes",
        "floating-pr",
        "copy-mode",
        "syntax-viewer",
        "diagnostics-hud",
        "ports-pill",
        "markdown-viewer",
        "theme-switcher",
        "settings-modal",
        "settings-ios-qr",
        "settings-server",
        "program-resume",
        "dnd-split",
        "multi-window",
    ];

    pub fn parse(raw: &str) -> std::result::Result<Self, String> {
        match raw.trim() {
            "sidebar-dark" => Ok(Self::SidebarDark),
            "sidebar-light" => Ok(Self::SidebarLight),
            "split-panes" => Ok(Self::SplitPanes),
            "floating-pr" => Ok(Self::FloatingPr),
            "copy-mode" => Ok(Self::CopyMode),
            "syntax-viewer" => Ok(Self::SyntaxViewer),
            "diagnostics-hud" => Ok(Self::DiagnosticsHud),
            "ports-pill" => Ok(Self::PortsPill),
            "markdown-viewer" => Ok(Self::MarkdownViewer),
            "theme-switcher" => Ok(Self::ThemeSwitcher),
            "settings-modal" => Ok(Self::SettingsModal),
            "settings-ios-qr" => Ok(Self::SettingsIosQr),
            "settings-server" => Ok(Self::SettingsServer),
            "program-resume" => Ok(Self::ProgramResume),
            "dnd-split" => Ok(Self::DndSplit),
            "multi-window" => Ok(Self::MultiWindow),
            other => Err(format!(
                "unknown headless scenario '{other}' (expected one of: {})",
                Self::ALL_NAMES.join(", ")
            )),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SidebarDark => "sidebar-dark",
            Self::SidebarLight => "sidebar-light",
            Self::SplitPanes => "split-panes",
            Self::FloatingPr => "floating-pr",
            Self::CopyMode => "copy-mode",
            Self::SyntaxViewer => "syntax-viewer",
            Self::DiagnosticsHud => "diagnostics-hud",
            Self::PortsPill => "ports-pill",
            Self::MarkdownViewer => "markdown-viewer",
            Self::ThemeSwitcher => "theme-switcher",
            Self::SettingsModal => "settings-modal",
            Self::SettingsIosQr => "settings-ios-qr",
            Self::SettingsServer => "settings-server",
            Self::ProgramResume => "program-resume",
            Self::DndSplit => "dnd-split",
            Self::MultiWindow => "multi-window",
        }
    }
}

/// Options for [`render_headless_live_png_with_options`].
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessRenderOptions {
    pub server_override: Option<String>,
    pub session_filter: Option<String>,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub force_light: bool,
    /// Optional local paths (`path[:line]`) to open in the right-hand Viewer Pane before capture.
    pub viewer_paths: Vec<String>,
    /// Optional deterministic scenario preset (`--headless-scenario <NAME>`).
    pub scenario: Option<HeadlessScenario>,
}

impl Default for HeadlessRenderOptions {
    fn default() -> Self {
        Self {
            server_override: None,
            session_filter: None,
            width: 1440,
            height: 900,
            scale_factor: 1.0,
            force_light: false,
            viewer_paths: Vec::new(),
            scenario: None,
        }
    }
}

/// Report returned by [`render_headless_live_png`].
#[derive(Debug, Clone)]
pub struct HeadlessRenderReport {
    pub output_path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub host_connected: bool,
    pub active_session_label: Option<String>,
    pub total_windows: usize,
    pub ready_panes: usize,
    pub frame_stats: FrameStats,
}

#[derive(Debug, Clone)]
enum ActiveDrag {
    SidebarResize {
        on_right: bool,
    },
    SplitDivider {
        host_id: String,
        session_id: String,
        divider: DividerGeometry,
        /// `(direction, before_block_id, after_block_id)` resolved from the split tree.
        split_target: Option<(String, String, String)>,
    },
    FloatingLayer {
        host_id: String,
        session_id: String,
        layer_id: String,
        zone: FloatingHitZone,
        start_px: (f32, f32),
        start_bounds: [f32; 4],
        workspace: RectF,
    },
    TerminalSelection {
        key: BlockKey,
        content_rect: RectF,
        cell_w: f32,
        cell_h: f32,
        rectangle: bool,
    },
    PaneScrollbar {
        key: BlockKey,
        track_rect: RectF,
    },
    ViewerDivider {
        workspace: RectF,
    },
    ViewerPan {
        last_px: (f32, f32),
    },
    ViewerScrollbar {
        track_rect: RectF,
    },
    ViewerVideoSeek {
        tab_id: u64,
        bar_rect: RectF,
    },
    ViewerTextSelection {
        tab_id: u64,
    },
    ViewerAudioSeek {
        tab_id: u64,
        bar_rect: RectF,
    },
    PaneMove {
        host_id: String,
        session_id: String,
        block_id: String,
        start_px: (f32, f32),
        activated: bool,
        hover_target: Option<(String, DropZoneHit)>,
    },
    TabReorder {
        host_id: String,
        session_id: String,
        window_id: String,
        start_px: (f32, f32),
        activated: bool,
        horizontal: bool,
        before_window_id: Option<String>,
        has_valid_drop: bool,
    },
}

/// Request queued by [`OmarexController`] when an action or Rex transfer needs a new OS window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingNewWindow {
    Session {
        session: SessionKey,
        focused_block: Option<String>,
    },
    Offline,
}

/// Per-OS-window UI, viewport, drag, viewer, and focus state swapped onto [`OmarexController`]
/// via [`OmarexController::with_window_context`].
pub struct WindowContext {
    pub id: u64,
    pub active_session: Option<SessionKey>,
    pub ui: UiTree,
    viewport_w: f32,
    viewport_h: f32,
    scale_factor: f32,
    last_ui_output: Option<UiRenderOutput>,
    last_cell_metrics: Option<CellMetrics>,
    pointer_pos: (f32, f32),
    last_pointer_take: Option<(BlockKey, Instant)>,
    active_drag: Option<ActiveDrag>,
    hovered_path_cell: Option<(BlockKey, u16, u32)>,
    hovered_terminal_target: Option<(BlockKey, HoveredTerminalTarget)>,
    hovered_link_spans: Option<(BlockKey, Vec<RowCellSpan>)>,
    pending_plain_link_click: Option<PendingPlainLinkClick>,
    context_menu_terminal_target: Option<(BlockKey, HoveredTerminalTarget)>,
    hover_image_id: Option<u64>,
    pending_hover_image: Option<(PathBuf, u64)>,
    loaded_hover_image: Option<(PathBuf, u64)>,
    hover_image_gen: u64,
    media_backend: MediaViewerBackend,
    pending_viewer_uploads: Vec<PendingViewerUpload>,
    pending_viewer_texture_removals: Vec<u64>,
    viewer_tab_texture_keys: HashMap<u64, Vec<u64>>,
    pending_viewer_downloads: HashMap<String, PendingRemoteViewerDownload>,
    pending_tab_jump_lines: HashMap<u64, (u32, Option<u32>)>,
    last_viewer_click: Option<(Instant, (f32, f32), u8)>,
    viewer_upload_gen: u64,
    active_dnd_hover: Option<(f32, f32)>,
    toast_deadlines: HashMap<u64, Instant>,
    copy_shines: Vec<CopyShineEffect>,
    last_inertia_tick: Option<Instant>,
    metadata_hydrated_count: usize,
    nav_history: Vec<(String, String, Option<String>)>,
    nav_index: usize,
    window_focused: bool,
    needs_redraw: bool,
    visible_pane_keys: Vec<BlockKey>,
    last_pane_grids: HashMap<BlockKey, (u16, u16)>,
    force_immediate_resize: bool,
    pending_rename_target: Option<PendingRenameTarget>,
    ephemeral_session: Option<SessionKey>,
    close_requested: bool,
    copy_mode: Option<CopyModeState>,
    file_picker_session: Option<FilePickerSession>,
    synced_ui_gen: u64,
    synced_resize_gen: u64,
}

impl WindowContext {
    #[must_use]
    pub fn active_session(&self) -> Option<&SessionKey> {
        self.active_session.as_ref()
    }

    #[must_use]
    pub fn visible_pane_keys(&self) -> &[BlockKey] {
        &self.visible_pane_keys
    }

    #[must_use]
    pub fn needs_redraw(&self) -> bool {
        self.needs_redraw
    }

    pub fn mark_needs_redraw(&mut self) {
        self.needs_redraw = true;
    }

    #[must_use]
    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    #[must_use]
    pub fn ephemeral_session(&self) -> Option<&SessionKey> {
        self.ephemeral_session.as_ref()
    }

    #[must_use]
    pub fn is_close_requested(&self) -> bool {
        self.close_requested
    }

    pub fn take_close_requested(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    #[must_use]
    pub fn copy_mode(&self) -> Option<&CopyModeState> {
        self.copy_mode.as_ref()
    }

    #[must_use]
    pub fn has_pending_viewer_download(&self, transfer_id: &str) -> bool {
        self.pending_viewer_downloads.contains_key(transfer_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingRenameTarget {
    Host {
        host_id: String,
    },
    Session {
        host_id: String,
        session_id: String,
    },
    Window {
        host_id: String,
        session_id: String,
        window_id: String,
    },
    Block {
        host_id: String,
        session_id: String,
        block_id: String,
    },
}

/// Command specification for standalone TUI / `-e <cmd> [args...]` launches
/// (e.g. `xdg-terminal-exec --app-id=org.omarchy.btop -e btop`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLaunchSpec {
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub wait_after_command: bool,
}

/// Reserved GPU image ID for local image hover preview cards (`u64::MAX - 1` avoids Kitty IDs).
const HOVER_PREVIEW_IMAGE_GPU_ID: u64 = u64::MAX - 1;
/// Maximum local image size loaded into RAM for hover preview thumbnails (`16 MiB`).
const MAX_HOVER_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
/// Threshold above which remote uploads display an immediate progress toast (`1 MiB`).
const LARGE_UPLOAD_TOAST_THRESHOLD_BYTES: u64 = 1024 * 1024;
/// Minimum gap between two pointer-triggered `Take`s of the same block.
const POINTER_TAKE_THROTTLE: Duration = Duration::from_millis(250);
/// Duration of the diagonal GPU Copy Shine sweep animation (220 ms).
const COPY_SHINE_DURATION: Duration = Duration::from_millis(220);

#[derive(Debug, Clone)]
struct CopyShineEffect {
    key: Option<BlockKey>,
    rects: Vec<Rect>,
    clip_rect: ClipRect,
    layer: i32,
    started_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HoveredTerminalTarget {
    Url(String),
    ThreadLink(String),
    LocalFile {
        resolved_path: PathBuf,
        display_path: String,
        target: ResolvedTarget,
        line: Option<u32>,
        column: Option<u32>,
        is_osc8: bool,
    },
    RemoteFile {
        remote_path: String,
        target: ResolvedTarget,
        line: Option<u32>,
        column: Option<u32>,
        is_osc8: bool,
    },
    KittyImage {
        image_id: u32,
        width: u32,
        height: u32,
        source_path: Option<PathBuf>,
    },
}

impl HoveredTerminalTarget {
    fn copyable_string(&self) -> Option<String> {
        match self {
            Self::Url(u) | Self::ThreadLink(u) => Some(u.clone()),
            Self::LocalFile { resolved_path, .. } => {
                Some(resolved_path.to_string_lossy().into_owned())
            }
            Self::RemoteFile { remote_path, .. } => Some(remote_path.clone()),
            Self::KittyImage { source_path, .. } => source_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

#[derive(Debug, Clone)]
struct PendingViewerUpload {
    texture_key: u64,
    generation: u64,
    frame: RgbaFrame,
}

#[derive(Debug, Clone)]
struct PendingRemoteViewerDownload {
    tab_id: u64,
    remote_path: String,
    kind: MediaKind,
}

#[derive(Debug, Clone)]
struct PendingPlainLinkClick {
    key: BlockKey,
    target: HoveredTerminalTarget,
    press_px: (f32, f32),
}

/// Active Vim Copy Mode state for a single terminal pane (`F15`).
#[derive(Debug, Clone)]
pub struct CopyModeState {
    pub block_key: BlockKey,
    pub cursor: ScreenPoint,
    pub visual: Option<(SelectionMode, ScreenPoint)>,
    pub search_active: bool,
    pub search_query: String,
    pub pending_g: bool,
}

#[derive(Debug, Clone)]
struct FilePickerSession {
    purpose: FilePickerPurpose,
    key: BlockKey,
    current_dir: String,
    request_id: u64,
}

/// Tracked local port-forwarder + detached `com.superlogical.tunnel.dial` block (`F19`).
#[derive(Debug, Clone)]
pub struct ActiveTunnel {
    pub tunnel_id: u64,
    pub source_block: BlockKey,
    pub tunnel_session_id: String,
    pub tunnel_block_id: Option<String>,
    pub remote_host: String,
    pub remote_port: u16,
    pub local_port: Option<u16>,
    pub error: Option<String>,
}

/// Complete, window-independent application engine for `omarex`.
pub struct OmarexController {
    pub settings: UserSettings,
    pub settings_path: PathBuf,
    pub theme: Theme,
    pub workspace: WorkspaceState,
    pub pane_store: PaneStore,
    pub ui: UiTree,
    pub actions: ActionRegistry,
    pub prompts: PromptManager,
    pub host_manager: Option<HostManager>,
    pub pending_commands: Vec<HostCommand>,
    pub pending_new_windows: Vec<PendingNewWindow>,
    media_waker: Option<Arc<dyn Fn() + Send + Sync>>,
    /// When `true` (e.g. headless read-only screenshots), suppresses mutating RPCs
    /// (`TerminalResize`, `OwnBlock`, `SetTheme`) against live Rex sessions.
    pub read_only: bool,
    /// Suppresses system clipboard side effects in automated unit tests if desired.
    pub clipboard_enabled: bool,

    viewport_w: f32,
    viewport_h: f32,
    scale_factor: f32,
    started_at: Instant,
    last_ui_output: Option<UiRenderOutput>,
    last_cell_metrics: Option<CellMetrics>,
    pointer_pos: (f32, f32),
    /// Last `Take` sent because of local pointer activity (throttles motion-event storms).
    last_pointer_take: Option<(BlockKey, Instant)>,
    active_drag: Option<ActiveDrag>,
    collapsed_hosts: HashSet<String>,
    collapsed_sessions: HashSet<(String, String)>,
    unread_windows: HashSet<(String, String, String)>,
    opened_streams: HashSet<BlockKey>,
    hydrated_blocks: HashSet<BlockKey>,
    block_activity_at: HashMap<BlockKey, Instant>,
    block_updated_unix: HashMap<BlockKey, i64>,
    block_osc94: HashMap<BlockKey, Osc94Progress>,
    hovered_path_cell: Option<(BlockKey, u16, u32)>,
    hovered_terminal_target: Option<(BlockKey, HoveredTerminalTarget)>,
    hovered_link_spans: Option<(BlockKey, Vec<RowCellSpan>)>,
    pending_plain_link_click: Option<PendingPlainLinkClick>,
    context_menu_terminal_target: Option<(BlockKey, HoveredTerminalTarget)>,
    hover_image_id: Option<u64>,
    pending_hover_image: Option<(PathBuf, u64)>,
    loaded_hover_image: Option<(PathBuf, u64)>,
    hover_image_gen: u64,
    media_backend: MediaViewerBackend,
    pending_viewer_uploads: Vec<PendingViewerUpload>,
    pending_viewer_texture_removals: Vec<u64>,
    viewer_tab_texture_keys: HashMap<u64, Vec<u64>>,
    pending_viewer_downloads: HashMap<String, PendingRemoteViewerDownload>,
    pending_tab_jump_lines: HashMap<u64, (u32, Option<u32>)>,
    last_viewer_click: Option<(Instant, (f32, f32), u8)>,
    viewer_upload_gen: u64,
    active_dnd_hover: Option<(f32, f32)>,
    downloaded_file_cache: HashMap<(String, String), (PathBuf, Option<String>)>,
    active_prompt_host: Option<String>,
    toast_deadlines: HashMap<u64, Instant>,
    next_toast_id: u64,
    next_transfer_id: u64,
    next_kitty_anim_ms: Option<u64>,
    copy_shines: Vec<CopyShineEffect>,
    last_inertia_tick: Option<Instant>,
    metadata_hydrated_count: usize,
    nav_history: Vec<(String, String, Option<String>)>,
    nav_index: usize,
    window_focused: bool,
    needs_redraw: bool,
    visible_pane_keys: Vec<BlockKey>,
    last_pane_grids: HashMap<BlockKey, (u16, u16)>,
    force_immediate_resize: bool,
    other_app_attached_sessions: HashMap<String, HashSet<String>>,
    pending_rename_target: Option<PendingRenameTarget>,
    preferred_session_filter: Option<String>,
    default_working_dir: Option<String>,
    command_launch: Option<CommandLaunchSpec>,
    ephemeral_session: Option<SessionKey>,
    ephemeral_spawn_requested: bool,
    should_exit: bool,
    close_requested: bool,
    current_window_ctx_id: u64,
    next_window_ctx_id: AtomicU64,
    pending_transfer_blocks: HashSet<BlockKey>,
    transfer_closed_source_blocks: HashSet<BlockKey>,
    // M4 workflows, protocol & telemetry state
    pub copy_mode: Option<CopyModeState>,
    file_picker_session: Option<FilePickerSession>,
    next_file_picker_request_id: u64,
    theme_preview_rollback: Option<(Option<String>, ThemePalette)>,
    theme_preview_cache: HashMap<String, ThemePalette>,
    last_clipboard_write: Option<(BlockKey, u64, Instant)>,
    pub active_tunnels: Vec<ActiveTunnel>,
    next_tunnel_id: u64,
    ports_queried_blocks: HashSet<BlockKey>,
    pub frame_times_us: VecDeque<u32>,
    pub last_frame_stats: Option<FrameStats>,
    pub keymap_diagnostics: Vec<KeymapDiagnostic>,
    chime_dispatcher: ChimeDispatcher,
    program_chime_state: HashMap<BlockKey, (vt_engine::ProgramStatusState, Instant)>,
    pub(super) shared_ui_gen: u64,
    pub(super) shared_resize_gen: u64,
    pub(super) current_window_synced_ui_gen: u64,
    pub(super) current_window_synced_resize_gen: u64,
    pub(super) tailscale_action_busy: bool,
    pub(super) tailscale_last_error: Option<String>,
    pub(super) server_action_busy: Option<String>,
    pub(super) server_last_error: Option<String>,
}

impl OmarexController {
    /// Creates a new [`OmarexController`] initialized with `settings` and `theme`.
    pub fn new(settings: UserSettings, settings_path: PathBuf, theme: Theme) -> Result<Self> {
        let warm_limit = settings.warm_session_cache_limit;
        let remap = settings.remap_ctrl_cmd_to_ctrl_alt;
        let pane_store =
            PaneStore::with_theme(&theme.palette).context("failed to initialize PaneStore")?;

        let mut ui = UiTree::new();
        sync_settings_to_ui(&settings, &theme, &mut ui);

        let collapsed_sessions: HashSet<(String, String)> = settings
            .collapsed_sessions
            .iter()
            .filter_map(|raw| {
                let (h, s) = raw.split_once('/')?;
                Some((h.to_owned(), s.to_owned()))
            })
            .collect();

        let actions = ActionRegistry::new(KeymapOptions {
            remap_ctrl_cmd_to_ctrl_alt: remap,
        });

        let mut workspace = WorkspaceState::new(warm_limit);
        if !settings.saved_hosts.is_empty() {
            workspace.add_host(HostEntry::new(
                LOCAL_HOST_ID,
                "This Machine",
                RexEndpoint::discover_default(),
            ));
            for saved in &settings.saved_hosts {
                let label = clean_host_label(&saved.label, &saved.endpoint);
                let ep = RexEndpoint::parse(&saved.endpoint).unwrap_or_else(|_| RexEndpoint::Tcp {
                    host: saved.endpoint.clone(),
                    port: 443,
                    tls: true,
                    path_prefix: String::new(),
                });
                let mut entry = HostEntry::new(saved.id.clone(), label, ep);
                entry.is_local = false;
                entry.status = if saved.auto_connect {
                    HostStatus::Connecting
                } else {
                    HostStatus::Disconnected
                };
                workspace.add_host(entry);
            }
        }

        Ok(Self {
            settings,
            settings_path,
            theme,
            workspace,
            pane_store,
            ui,
            actions,
            prompts: PromptManager::new(),
            host_manager: None,
            pending_commands: Vec::new(),
            pending_new_windows: Vec::new(),
            media_waker: None,
            read_only: false,
            clipboard_enabled: true,
            viewport_w: 1440.0,
            viewport_h: 900.0,
            scale_factor: 1.0,
            started_at: Instant::now(),
            last_ui_output: None,
            last_cell_metrics: None,
            pointer_pos: (0.0, 0.0),
            last_pointer_take: None,
            active_drag: None,
            collapsed_hosts: HashSet::new(),
            collapsed_sessions,
            unread_windows: HashSet::new(),
            opened_streams: HashSet::new(),
            hydrated_blocks: HashSet::new(),
            block_activity_at: HashMap::new(),
            block_updated_unix: HashMap::new(),
            block_osc94: HashMap::new(),
            hovered_path_cell: None,
            hovered_terminal_target: None,
            hovered_link_spans: None,
            pending_plain_link_click: None,
            context_menu_terminal_target: None,
            hover_image_id: None,
            pending_hover_image: None,
            loaded_hover_image: None,
            hover_image_gen: 0,
            media_backend: MediaViewerBackend::new(),
            pending_viewer_uploads: Vec::new(),
            pending_viewer_texture_removals: Vec::new(),
            viewer_tab_texture_keys: HashMap::new(),
            pending_viewer_downloads: HashMap::new(),
            pending_tab_jump_lines: HashMap::new(),
            last_viewer_click: None,
            viewer_upload_gen: 1,
            active_dnd_hover: None,
            downloaded_file_cache: HashMap::new(),
            active_prompt_host: None,
            toast_deadlines: HashMap::new(),
            next_toast_id: 1,
            next_transfer_id: 1,
            next_kitty_anim_ms: None,
            copy_shines: Vec::new(),
            last_inertia_tick: None,
            metadata_hydrated_count: 0,
            nav_history: Vec::new(),
            nav_index: 0,
            window_focused: true,
            needs_redraw: true,
            visible_pane_keys: Vec::new(),
            last_pane_grids: HashMap::new(),
            force_immediate_resize: false,
            other_app_attached_sessions: HashMap::new(),
            pending_rename_target: None,
            preferred_session_filter: None,
            default_working_dir: None,
            command_launch: None,
            ephemeral_session: None,
            ephemeral_spawn_requested: false,
            should_exit: false,
            close_requested: false,
            current_window_ctx_id: 0,
            next_window_ctx_id: AtomicU64::new(1),
            pending_transfer_blocks: HashSet::new(),
            transfer_closed_source_blocks: HashSet::new(),
            copy_mode: None,
            file_picker_session: None,
            next_file_picker_request_id: 1,
            theme_preview_rollback: None,
            theme_preview_cache: HashMap::new(),
            last_clipboard_write: None,
            active_tunnels: Vec::new(),
            next_tunnel_id: 1,
            ports_queried_blocks: HashSet::new(),
            frame_times_us: VecDeque::with_capacity(120),
            last_frame_stats: None,
            keymap_diagnostics: Vec::new(),
            chime_dispatcher: ChimeDispatcher::new(),
            program_chime_state: HashMap::new(),
            shared_ui_gen: 0,
            shared_resize_gen: 0,
            current_window_synced_ui_gen: 0,
            current_window_synced_resize_gen: 0,
            tailscale_action_busy: false,
            tailscale_last_error: None,
            server_action_busy: None,
            server_last_error: None,
        })
    }

    /// Attaches a live [`HostManager`] handle and flushes any queued [`HostCommand`]s.
    pub fn attach_host_manager(&mut self, host_manager: HostManager) {
        self.host_manager = Some(host_manager);
        let queued = std::mem::take(&mut self.pending_commands);
        for cmd in queued {
            self.send_command(cmd);
        }
    }

    /// Registers a thread-safe waker invoked by [`MediaViewerBackend`] whenever async file loads,
    /// image decodes, or video/GIF playback frames become ready for the UI event loop.
    pub fn set_media_waker(&mut self, waker: Arc<dyn Fn() + Send + Sync>) {
        self.media_waker = Some(Arc::clone(&waker));
        self.media_backend.set_waker(waker);
    }

    /// Creates a new per-OS-window [`WindowContext`] initialized from controller settings & theme.
    pub fn create_window_context(
        &self,
        physical_width: u32,
        physical_height: u32,
        scale_factor: f32,
        active_session: Option<SessionKey>,
        focused_block: Option<String>,
    ) -> WindowContext {
        let scale = scale_factor.max(0.5);
        let viewport_w = (physical_width as f32 / scale).max(1.0);
        let viewport_h = (physical_height as f32 / scale).max(1.0);
        let mut ui = UiTree::new();
        sync_settings_to_ui(&self.settings, &self.theme, &mut ui);
        if self.command_launch.is_some() {
            ui.layout_cfg.toolbar_behavior = ToolbarBehavior::Hidden;
            ui.layout_cfg.pane_header_mode = PaneHeaderMode::Hidden;
            ui.layout_cfg.density = PaneDensity::Compact;
        }
        if focused_block.is_some() {
            ui.focus.focus_terminal(focused_block);
        }
        let mut media_backend = MediaViewerBackend::new();
        if let Some(waker) = &self.media_waker {
            media_backend.set_waker(Arc::clone(waker));
        }
        let id = self.next_window_ctx_id.fetch_add(1, Ordering::Relaxed);
        WindowContext {
            id,
            active_session,
            ui,
            viewport_w,
            viewport_h,
            scale_factor: scale,
            last_ui_output: None,
            last_cell_metrics: None,
            pointer_pos: (0.0, 0.0),
            last_pointer_take: None,
            active_drag: None,
            hovered_path_cell: None,
            hovered_terminal_target: None,
            hovered_link_spans: None,
            pending_plain_link_click: None,
            context_menu_terminal_target: None,
            hover_image_id: None,
            pending_hover_image: None,
            loaded_hover_image: None,
            hover_image_gen: 0,
            media_backend,
            pending_viewer_uploads: Vec::new(),
            pending_viewer_texture_removals: Vec::new(),
            viewer_tab_texture_keys: HashMap::new(),
            pending_viewer_downloads: HashMap::new(),
            pending_tab_jump_lines: HashMap::new(),
            last_viewer_click: None,
            viewer_upload_gen: 1,
            active_dnd_hover: None,
            toast_deadlines: HashMap::new(),
            copy_shines: Vec::new(),
            last_inertia_tick: None,
            metadata_hydrated_count: 0,
            nav_history: Vec::new(),
            nav_index: 0,
            window_focused: true,
            needs_redraw: true,
            visible_pane_keys: Vec::new(),
            last_pane_grids: HashMap::new(),
            force_immediate_resize: true,
            pending_rename_target: None,
            ephemeral_session: None,
            close_requested: false,
            copy_mode: None,
            file_picker_session: None,
            synced_ui_gen: self.shared_ui_gen,
            synced_resize_gen: self.shared_resize_gen,
        }
    }

    fn swap_window_context(&mut self, ctx: &mut WindowContext) {
        std::mem::swap(&mut self.current_window_ctx_id, &mut ctx.id);
        std::mem::swap(&mut self.workspace.active_session, &mut ctx.active_session);
        std::mem::swap(&mut self.ui, &mut ctx.ui);
        std::mem::swap(&mut self.viewport_w, &mut ctx.viewport_w);
        std::mem::swap(&mut self.viewport_h, &mut ctx.viewport_h);
        std::mem::swap(&mut self.scale_factor, &mut ctx.scale_factor);
        std::mem::swap(&mut self.last_ui_output, &mut ctx.last_ui_output);
        std::mem::swap(&mut self.last_cell_metrics, &mut ctx.last_cell_metrics);
        std::mem::swap(&mut self.pointer_pos, &mut ctx.pointer_pos);
        std::mem::swap(&mut self.last_pointer_take, &mut ctx.last_pointer_take);
        std::mem::swap(&mut self.active_drag, &mut ctx.active_drag);
        std::mem::swap(&mut self.hovered_path_cell, &mut ctx.hovered_path_cell);
        std::mem::swap(
            &mut self.hovered_terminal_target,
            &mut ctx.hovered_terminal_target,
        );
        std::mem::swap(&mut self.hovered_link_spans, &mut ctx.hovered_link_spans);
        std::mem::swap(
            &mut self.pending_plain_link_click,
            &mut ctx.pending_plain_link_click,
        );
        std::mem::swap(
            &mut self.context_menu_terminal_target,
            &mut ctx.context_menu_terminal_target,
        );
        std::mem::swap(&mut self.hover_image_id, &mut ctx.hover_image_id);
        std::mem::swap(&mut self.pending_hover_image, &mut ctx.pending_hover_image);
        std::mem::swap(&mut self.loaded_hover_image, &mut ctx.loaded_hover_image);
        std::mem::swap(&mut self.hover_image_gen, &mut ctx.hover_image_gen);
        std::mem::swap(&mut self.media_backend, &mut ctx.media_backend);
        std::mem::swap(
            &mut self.pending_viewer_uploads,
            &mut ctx.pending_viewer_uploads,
        );
        std::mem::swap(
            &mut self.pending_viewer_texture_removals,
            &mut ctx.pending_viewer_texture_removals,
        );
        std::mem::swap(
            &mut self.viewer_tab_texture_keys,
            &mut ctx.viewer_tab_texture_keys,
        );
        std::mem::swap(
            &mut self.pending_viewer_downloads,
            &mut ctx.pending_viewer_downloads,
        );
        std::mem::swap(
            &mut self.pending_tab_jump_lines,
            &mut ctx.pending_tab_jump_lines,
        );
        std::mem::swap(&mut self.last_viewer_click, &mut ctx.last_viewer_click);
        std::mem::swap(&mut self.viewer_upload_gen, &mut ctx.viewer_upload_gen);
        std::mem::swap(&mut self.active_dnd_hover, &mut ctx.active_dnd_hover);
        std::mem::swap(&mut self.toast_deadlines, &mut ctx.toast_deadlines);
        std::mem::swap(&mut self.copy_shines, &mut ctx.copy_shines);
        std::mem::swap(&mut self.last_inertia_tick, &mut ctx.last_inertia_tick);
        std::mem::swap(
            &mut self.metadata_hydrated_count,
            &mut ctx.metadata_hydrated_count,
        );
        std::mem::swap(&mut self.nav_history, &mut ctx.nav_history);
        std::mem::swap(&mut self.nav_index, &mut ctx.nav_index);
        std::mem::swap(&mut self.window_focused, &mut ctx.window_focused);
        std::mem::swap(&mut self.needs_redraw, &mut ctx.needs_redraw);
        std::mem::swap(&mut self.visible_pane_keys, &mut ctx.visible_pane_keys);
        std::mem::swap(&mut self.last_pane_grids, &mut ctx.last_pane_grids);
        std::mem::swap(
            &mut self.force_immediate_resize,
            &mut ctx.force_immediate_resize,
        );
        std::mem::swap(
            &mut self.pending_rename_target,
            &mut ctx.pending_rename_target,
        );
        std::mem::swap(&mut self.ephemeral_session, &mut ctx.ephemeral_session);
        std::mem::swap(&mut self.close_requested, &mut ctx.close_requested);
        std::mem::swap(&mut self.copy_mode, &mut ctx.copy_mode);
        std::mem::swap(&mut self.file_picker_session, &mut ctx.file_picker_session);
        std::mem::swap(
            &mut self.current_window_synced_ui_gen,
            &mut ctx.synced_ui_gen,
        );
        std::mem::swap(
            &mut self.current_window_synced_resize_gen,
            &mut ctx.synced_resize_gen,
        );
    }

    fn sync_active_context_if_stale(&mut self) {
        if self.current_window_synced_ui_gen == self.shared_ui_gen
            && self.current_window_synced_resize_gen == self.shared_resize_gen
        {
            return;
        }
        sync_settings_to_ui(&self.settings, &self.theme, &mut self.ui);
        if self.command_launch.is_some() {
            self.ui.layout_cfg.toolbar_behavior = ToolbarBehavior::Hidden;
            self.ui.layout_cfg.pane_header_mode = PaneHeaderMode::Hidden;
            self.ui.layout_cfg.density = PaneDensity::Compact;
        }
        let fresh_snap = self.settings_view_snapshot();
        if let Some(modal) = &mut self.ui.modal
            && let ModalContent::Settings { snapshot, .. } = &mut modal.content
        {
            *snapshot = fresh_snap;
        }
        if self.current_window_synced_resize_gen != self.shared_resize_gen {
            self.force_immediate_resize = true;
            self.current_window_synced_resize_gen = self.shared_resize_gen;
        }
        self.current_window_synced_ui_gen = self.shared_ui_gen;
        self.needs_redraw = true;
    }

    pub(super) fn mark_shared_settings_changed(&mut self, resize: bool) {
        let fresh_snap = self.settings_view_snapshot();
        if let Some(modal) = &mut self.ui.modal
            && let ModalContent::Settings { snapshot, .. } = &mut modal.content
        {
            *snapshot = fresh_snap;
        }
        self.shared_ui_gen = self.shared_ui_gen.saturating_add(1);
        self.current_window_synced_ui_gen = self.shared_ui_gen;
        if resize {
            self.force_immediate_resize = true;
            self.shared_resize_gen = self.shared_resize_gen.saturating_add(1);
            self.current_window_synced_resize_gen = self.shared_resize_gen;
        }
        self.needs_redraw = true;
    }

    pub(super) fn refresh_open_settings_modals(&mut self, contexts: &mut [&mut WindowContext]) {
        let fresh_snap = self.settings_view_snapshot();
        if let Some(modal) = &mut self.ui.modal
            && let ModalContent::Settings { snapshot, .. } = &mut modal.content
        {
            *snapshot = fresh_snap.clone();
            self.needs_redraw = true;
        }
        for ctx in contexts.iter_mut() {
            if let Some(modal) = &mut ctx.ui.modal
                && let ModalContent::Settings { snapshot, .. } = &mut modal.content
            {
                *snapshot = fresh_snap.clone();
                ctx.needs_redraw = true;
            }
        }
        self.shared_ui_gen = self.shared_ui_gen.saturating_add(1);
        self.current_window_synced_ui_gen = self.shared_ui_gen;
    }

    /// Synchronizes shared settings/theme UI state onto `ctx` if another window mutated them.
    pub fn sync_window_context(&mut self, ctx: &mut WindowContext) {
        self.with_window_context(ctx, |_| {});
    }

    /// Temporarily binds `ctx` onto `self` for the duration of `f`, restoring `ctx` via an RAII
    /// `Drop` guard even on early returns or panics.
    pub fn with_window_context<R>(
        &mut self,
        ctx: &mut WindowContext,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        struct Guard<'a> {
            ctrl: *mut OmarexController,
            ctx: *mut WindowContext,
            _marker: std::marker::PhantomData<(&'a mut OmarexController, &'a mut WindowContext)>,
        }
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                // SAFETY: `Guard` holds exclusive mutable borrows for `'a` and restores the exact
                // swapped fields back into `ctx` when exiting `with_window_context`.
                unsafe {
                    (*self.ctrl).swap_window_context(&mut *self.ctx);
                }
            }
        }
        self.swap_window_context(ctx);
        let guard = Guard {
            ctrl: self as *mut Self,
            ctx: ctx as *mut WindowContext,
            _marker: std::marker::PhantomData,
        };
        // SAFETY: `guard.ctrl` is valid for the duration of `f` and `guard` restores on drop.
        let ctrl = unsafe { &mut *guard.ctrl };
        ctrl.sync_active_context_if_stale();
        f(ctrl)
    }

    /// Drains any pending OS window spawn requests queued by actions or Rex transfer events.
    pub fn take_pending_new_windows(&mut self) -> Vec<PendingNewWindow> {
        std::mem::take(&mut self.pending_new_windows)
    }

    /// Updates the set of sessions actively displayed by open OS windows (protecting them from
    /// warm-LRU eviction).
    pub fn set_pinned_sessions(&mut self, pinned: HashSet<SessionKey>) {
        self.workspace.pinned_sessions = pinned;
    }

    /// Cleans up ownership/ephemeral state when a secondary OS window closes without shutting down
    /// the rest of the application.
    pub fn on_secondary_window_closed(
        &mut self,
        closed_ctx: &WindowContext,
        remaining_visible_panes: &HashSet<BlockKey>,
    ) {
        if let Some(sk) = &closed_ctx.ephemeral_session {
            self.send_command(HostCommand::DestroySession {
                host_id: sk.host_id.clone(),
                session_id: sk.session_id.clone(),
            });
            return;
        }
        if self.read_only {
            return;
        }
        let mut by_session: HashMap<SessionKey, Vec<String>> = HashMap::new();
        for key in &closed_ctx.visible_pane_keys {
            if !remaining_visible_panes.contains(key) {
                by_session
                    .entry(SessionKey::new(key.host_id.clone(), key.session_id.clone()))
                    .or_default()
                    .push(key.block_id.clone());
            }
        }
        for (sk, block_ids) in by_session {
            if !block_ids.is_empty() {
                self.send_command(HostCommand::DisownBlock {
                    host_id: sk.host_id,
                    session_id: sk.session_id,
                    block_ids: Some(block_ids),
                });
            }
        }
    }

    /// Sets an optional initial session name or ID preference (e.g. CLI `--session "T3 Code"`).
    pub fn set_preferred_session(&mut self, session: Option<String>) {
        self.preferred_session_filter = session;
    }

    /// Sets the default working directory inherited from `--working-directory` / `--dir`
    /// (used when creating new shells/tabs or launching `-e` commands).
    pub fn set_default_working_directory(&mut self, cwd: Option<String>) {
        self.default_working_dir = cwd.filter(|d| !d.trim().is_empty());
    }

    /// Configures standalone TUI / `-e <cmd> [args...]` execution mode.
    ///
    /// Hides the sidebar and pane header (compact edge-to-edge layout) so Omarchy TUIs
    /// (`btop`, `lazygit`, floating update dialogs) fill the window cleanly and auto-exit
    /// when the command finishes.
    pub fn set_command_launch(&mut self, spec: Option<CommandLaunchSpec>) {
        if spec.is_some() {
            self.ui.layout_cfg.toolbar_behavior = ToolbarBehavior::Hidden;
            self.ui.layout_cfg.pane_header_mode = PaneHeaderMode::Hidden;
            self.ui.layout_cfg.density = PaneDensity::Compact;
        }
        self.command_launch = spec;
    }

    /// True once a standalone `-e` command has exited and the GUI should close cleanly.
    #[must_use]
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Returns the ephemeral Rex session created for `-e <cmd>` launches, if active.
    #[must_use]
    pub fn ephemeral_session(&self) -> Option<&SessionKey> {
        self.ephemeral_session.as_ref()
    }

    fn resolve_shell_cwd_for_host(
        &self,
        host_id: &str,
        anchor_key: Option<&BlockKey>,
        allow_t3_pwd: bool,
    ) -> Option<String> {
        if host_id == LOCAL_HOST_ID
            && let Some(cwd) = self
                .command_launch
                .as_ref()
                .and_then(|c| c.cwd.as_deref())
                .filter(|s| !s.trim().is_empty())
        {
            return Some(cwd.to_owned());
        }
        let lookup_key = anchor_key
            .filter(|k| k.host_id == host_id)
            .cloned()
            .or_else(|| self.focused_block_key().filter(|k| k.host_id == host_id));
        if let Some(key) = lookup_key {
            let sk = SessionKey::new(key.host_id.clone(), key.session_id.clone());
            let is_t3 = self
                .workspace
                .session(&sk)
                .is_some_and(|s| is_t3_session(&s.label));
            if (allow_t3_pwd || !is_t3)
                && let Some(raw_pwd) = self.workspace.block(&key).and_then(|b| b.pwd.as_deref())
                && let Some(path) = normalize_osc7_pwd(raw_pwd)
            {
                return Some(path);
            }
        }
        // Never leak local Linux working directories or `$HOME` onto a remote Rex host (e.g. macOS).
        if host_id != LOCAL_HOST_ID {
            return None;
        }
        if let Some(cwd) = self
            .default_working_dir
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            return Some(cwd.to_owned());
        }
        std::env::var("HOME").ok().filter(|h| !h.trim().is_empty())
    }

    fn build_shell_layout_spec_with_cwd(
        &self,
        cwd: Option<&str>,
        command: Option<&[String]>,
        wait_after: bool,
    ) -> LayoutCreateSpec {
        let mut opts = serde_json::Map::new();
        if let Some(dir) = cwd.map(str::trim).filter(|s| !s.is_empty()) {
            opts.insert("cwd".to_owned(), serde_json::Value::String(dir.to_owned()));
        }
        if let Some(cmd) = command
            && !cmd.is_empty()
        {
            opts.insert(
                "command".to_owned(),
                serde_json::Value::Array(
                    cmd.iter()
                        .map(|arg| serde_json::Value::String(arg.clone()))
                        .collect(),
                ),
            );
            opts.insert(
                "exit".to_owned(),
                serde_json::json!({
                    "on_completion": !wait_after,
                    "quick_exit_threshold_ms": 0
                }),
            );
        }
        if let Ok(theme_val) = serde_json::to_value(theme_palette_to_rex_args(&self.theme.palette))
        {
            opts.insert("theme".to_owned(), theme_val);
        }

        LayoutCreateSpec {
            block: Some(BlockCreateSpec {
                flavor: "com.superlogical.terminal.shell".into(),
                label: None,
                options: if opts.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Object(opts))
                },
            }),
            split: None,
        }
    }

    fn build_shell_layout_spec(
        &self,
        command: Option<&[String]>,
        wait_after: bool,
    ) -> LayoutCreateSpec {
        let cwd = self.resolve_shell_cwd_for_host(LOCAL_HOST_ID, None, false);
        self.build_shell_layout_spec_with_cwd(cwd.as_deref(), command, wait_after)
    }

    /// Label for a new terminal session, or `None` to let the Rex server name it.
    ///
    /// Like the Rex app, Omarex leaves new sessions unlabeled so the server assigns a friendly
    /// name (`wandering-river`, `quiet-marsh`, …). Only when Omarex is launched into a specific
    /// project directory (`prefer_cwd_basename`) is the session named after that folder, if no
    /// other session on the host already uses the name.
    fn new_terminal_session_label(
        &self,
        host_id: &str,
        cwd: Option<&str>,
        prefer_cwd_basename: bool,
    ) -> Option<String> {
        if !prefer_cwd_basename {
            return None;
        }
        let dir = cwd?;
        let home = std::env::var("HOME").ok();
        if home
            .as_deref()
            .is_some_and(|h| Path::new(dir) == Path::new(h))
        {
            return None;
        }
        let base = Path::new(dir)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && !is_t3_session(s))?;
        let taken = self.workspace.host(host_id).is_some_and(|h| {
            h.sessions
                .values()
                .any(|s| s.label.eq_ignore_ascii_case(base))
        });
        (!taken).then(|| base.to_owned())
    }

    fn find_preferred_terminal_session(
        &self,
        host_id: &str,
        exclude_sessions: Option<&HashSet<String>>,
    ) -> Option<String> {
        let host = self.workspace.host(host_id)?;
        let is_eligible = |sid: &str| {
            let Some(sess) = host.sessions.get(sid) else {
                return false;
            };
            if is_t3_session(&sess.label) {
                return false;
            }
            exclude_sessions.is_none_or(|ex| !ex.contains(sid))
        };

        // Prefer a session explicitly named "Terminal" or "main" first.
        if let Some(sid) = host.session_order.iter().find(|sid| {
            is_eligible(sid)
                && host.sessions.get(*sid).is_some_and(|s| {
                    s.label.eq_ignore_ascii_case("terminal") || s.label.eq_ignore_ascii_case("main")
                })
        }) {
            return Some(sid.clone());
        }

        host.session_order
            .iter()
            .find(|sid| is_eligible(sid))
            .cloned()
    }

    /// Opens a real Ghostty-backed interactive shell terminal (`com.superlogical.terminal.shell`),
    /// never hijacking into a `T3 Code` thread.
    fn open_real_shell_terminal(&mut self, host_id: Option<String>, force_new_session: bool) {
        let target_host = host_id
            .or_else(|| {
                self.workspace
                    .active_session
                    .as_ref()
                    .map(|s| s.host_id.clone())
            })
            .unwrap_or_else(|| LOCAL_HOST_ID.to_owned());

        self.collapsed_hosts.remove(&target_host);
        let cwd = self.resolve_shell_cwd_for_host(&target_host, None, false);
        let layout = self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);

        if force_new_session {
            let label = self.new_terminal_session_label(&target_host, cwd.as_deref(), false);
            self.send_command(HostCommand::NewSession {
                host_id: target_host,
                request: SessionCreateRequest {
                    label,
                    initial_windows: Some(vec![InitialWindowSpec {
                        window_label: None,
                        layout,
                    }]),
                },
                open_in_new_window: false,
                origin_window_id: None,
            });
            return;
        }

        // If the active session on `target_host` is a normal (non-T3) session, add a shell tab there.
        if let Some(active_sk) = self.workspace.active_session.clone()
            && active_sk.host_id == target_host
            && self
                .workspace
                .session(&active_sk)
                .is_some_and(|s| !is_t3_session(&s.label))
        {
            self.set_session_collapsed(&active_sk.host_id, &active_sk.session_id, false);
            self.send_command(HostCommand::NewWindow {
                host_id: active_sk.host_id,
                request: SessionNewWindowRequest {
                    session_id: active_sk.session_id,
                    window_label: None,
                    layout: Some(layout),
                    focus: Some(true),
                },
                origin_window_id: None,
            });
            return;
        }

        // Otherwise (inside `T3 Code` or no active session), target an existing non-T3 session
        // on `target_host` if one exists, or create a brand-new Rex session (named by the server)!
        if let Some(term_sid) = self.find_preferred_terminal_session(&target_host, None) {
            let sk = SessionKey::new(target_host.clone(), term_sid.clone());
            self.set_session_collapsed(&sk.host_id, &sk.session_id, false);
            self.activate_session(sk.clone(), &HashMap::new());
            self.send_command(HostCommand::NewWindow {
                host_id: sk.host_id,
                request: SessionNewWindowRequest {
                    session_id: sk.session_id,
                    window_label: None,
                    layout: Some(layout),
                    focus: Some(true),
                },
                origin_window_id: None,
            });
        } else {
            let label = self.new_terminal_session_label(&target_host, cwd.as_deref(), false);
            self.send_command(HostCommand::NewSession {
                host_id: target_host,
                request: SessionCreateRequest {
                    label,
                    initial_windows: Some(vec![InitialWindowSpec {
                        window_label: None,
                        layout,
                    }]),
                },
                open_in_new_window: false,
                origin_window_id: None,
            });
        }
    }

    pub(super) fn open_new_os_window_session(&mut self, host_id: Option<String>) {
        let target_host = host_id
            .or_else(|| {
                self.workspace
                    .active_session
                    .as_ref()
                    .map(|s| s.host_id.clone())
            })
            .unwrap_or_else(|| LOCAL_HOST_ID.to_owned());

        let is_connected = self
            .workspace
            .host(&target_host)
            .is_some_and(|h| matches!(h.status, HostStatus::Connected { .. }));
        if !is_connected || self.read_only {
            self.pending_new_windows.push(PendingNewWindow::Offline);
            return;
        }

        self.collapsed_hosts.remove(&target_host);
        let cwd = self.resolve_shell_cwd_for_host(&target_host, None, false);
        let layout = self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
        let label = self.new_terminal_session_label(&target_host, cwd.as_deref(), false);
        self.send_command(HostCommand::NewSession {
            host_id: target_host,
            request: SessionCreateRequest {
                label,
                initial_windows: Some(vec![InitialWindowSpec {
                    window_label: None,
                    layout,
                }]),
            },
            open_in_new_window: true,
            origin_window_id: None,
        });
    }

    fn set_session_collapsed(&mut self, host_id: &str, session_id: &str, collapsed: bool) {
        let key = (host_id.to_owned(), session_id.to_owned());
        let changed = if collapsed {
            self.collapsed_sessions.insert(key)
        } else {
            self.collapsed_sessions.remove(&key)
        };
        if changed {
            self.settings
                .set_session_collapsed(host_id, session_id, collapsed);
            self.save_settings_best_effort();
            self.needs_redraw = true;
        }
    }

    /// Builds a [`HostManagerConfig`] wired with our initial `client.keymap` registration,
    /// saved remote hosts, and live Omarchy material style.
    pub fn build_host_manager_config(
        &self,
        local_endpoint: Option<RexEndpoint>,
        wake_ui: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    ) -> HostManagerConfig {
        HostManagerConfig {
            local_endpoint,
            saved_hosts: self.settings.saved_hosts.clone(),
            preferred_local_session_id: self.settings.last_session_id.clone(),
            client_label: Some("omarex".to_owned()),
            initial_keymap_request: build_proto_keymap_request(&self.actions),
            start_theme_watchers: !self.read_only,
            start_notifier: !self.read_only,
            material: self.settings.material.to_theme_material(),
            wake_ui,
        }
    }

    /// Updates logical viewport dimensions and HiDPI scale factor.
    pub fn set_viewport(&mut self, physical_width: u32, physical_height: u32, scale_factor: f32) {
        let scale = scale_factor.max(0.5);
        self.scale_factor = scale;
        self.viewport_w = (physical_width as f32 / scale).max(1.0);
        self.viewport_h = (physical_height as f32 / scale).max(1.0);
        self.needs_redraw = true;
    }

    /// Called by `app.rs` immediately after a Wayland/Hyprland `Resized` or `ScaleFactorChanged`
    /// event so visible terminal grids claim ownership and reconcile with zero stale frames.
    pub fn on_viewport_resized(&mut self, _now: Instant) {
        self.force_immediate_resize = true;
        self.needs_redraw = true;
        self.hovered_path_cell = None;
        self.hovered_terminal_target = None;
        self.pending_hover_image = None;
        self.ui.hover_preview = None;

        let keys = self.visible_pane_keys.clone();
        for key in keys {
            self.workspace.invalidate_sent_resize(&key);
            if !self.read_only {
                let trigger = if self.window_focused {
                    OwnershipTrigger::ExplicitInteractOrFit
                } else {
                    OwnershipTrigger::AttachOrView
                };
                self.maybe_claim_ownership(&key, trigger);
            }
        }
    }

    pub fn viewport_logical(&self) -> (f32, f32) {
        (self.viewport_w, self.viewport_h)
    }

    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    pub fn needs_redraw(&self) -> bool {
        if self.needs_redraw
            || self.pane_store.has_pending_snapshots()
            || !self.copy_shines.is_empty()
            || self.ui.has_active_animations()
            || self.pane_store.has_active_scroll_inertia()
        {
            return true;
        }
        if self.visible_pane_keys.is_empty() {
            self.pane_store.any_dirty()
        } else {
            self.pane_store.any_dirty_in(self.visible_pane_keys.iter())
        }
    }

    pub fn mark_needs_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// Records the latest GPU [`FrameStats`] into the bounded 120-sample ring buffer (`F20`).
    pub fn record_frame_stats(&mut self, stats: &FrameStats) {
        if self.frame_times_us.len() >= 120 {
            self.frame_times_us.pop_front();
        }
        self.frame_times_us
            .push_back(stats.total_us.min(u64::from(u32::MAX)) as u32);
        self.last_frame_stats = Some(stats.clone());
    }

    pub(super) fn copy_mode_badge_for(&self, key: &BlockKey) -> Option<&'static str> {
        let cm = self.copy_mode.as_ref()?;
        if &cm.block_key != key {
            return None;
        }
        if cm.search_active {
            return Some("COPY /");
        }
        Some(match cm.visual.map(|(m, _)| m) {
            None => "COPY",
            Some(SelectionMode::Char) => "VISUAL",
            Some(SelectionMode::Line) => "V-LINE",
            Some(SelectionMode::Block) => "V-BLOCK",
        })
    }

    /// Clamps active Copy Mode `cursor` and `visual` anchor against current terminal bounds
    /// and re-applies visual selection if clamping adjusted either coordinate (`F15`).
    pub(super) fn clamp_copy_mode_to_bounds(&mut self) -> Option<(u16, u32, vt_engine::Scrollbar)> {
        let cm = self.copy_mode.as_mut()?;
        let key = cm.block_key.clone();
        let (cols, total_rows, sb) = self.pane_store.copy_mode_screen_bounds(&key)?;
        if cols == 0 || total_rows == 0 {
            return None;
        }
        let max_col = cols.saturating_sub(1);
        let max_row = total_rows.saturating_sub(1);
        let prev_cursor = cm.cursor;
        cm.cursor.col = cm.cursor.col.min(max_col);
        cm.cursor.row = cm.cursor.row.min(max_row);
        let mut changed = cm.cursor != prev_cursor;
        if let Some((_, anchor)) = &mut cm.visual {
            let prev_anchor = *anchor;
            anchor.col = anchor.col.min(max_col);
            anchor.row = anchor.row.min(max_row);
            if *anchor != prev_anchor {
                changed = true;
            }
        }
        let visual = cm.visual;
        let cursor = cm.cursor;
        if changed && let Some((mode, anchor)) = visual {
            let _ = self
                .pane_store
                .copy_mode_select_range(&key, anchor, cursor, mode);
        }
        Some((cols, total_rows, sb))
    }

    pub(super) fn copy_mode_cursor_override(
        &mut self,
        key: &BlockKey,
        accent: Color,
        bg: Color,
    ) -> Option<TerminalCursor> {
        if self
            .copy_mode
            .as_ref()
            .is_none_or(|cm| &cm.block_key != key)
        {
            return None;
        }
        let (_, _, sb) = self.clamp_copy_mode_to_bounds()?;
        let cm = self.copy_mode.as_ref()?;
        let vrow = i64::from(cm.cursor.row) - sb.offset as i64;
        if vrow >= 0 && vrow < sb.len as i64 {
            Some(TerminalCursor {
                col: cm.cursor.col,
                row: vrow as i32,
                shape: CursorShape::Block,
                color: accent,
                text_color: bg,
                visible: true,
            })
        } else {
            Some(TerminalCursor {
                col: 0,
                row: 0,
                shape: CursorShape::Block,
                color: accent,
                text_color: bg,
                visible: false,
            })
        }
    }

    /// Formats a redacted diagnostics bundle suitable for clipboard bug reports (`F20`).
    #[must_use]
    pub fn format_debug_bundle(&self) -> String {
        let snap = self.build_diagnostics_snapshot();
        let mut out = String::new();
        out.push_str(&format!(
            "omarex {} (Rex API rev {}, VT {})\n",
            snap.app_version,
            rex_proto::TARGET_API_REVISION,
            snap.vt_version
        ));
        out.push_str(&format!(
            "GPU: {} ({}) | avg={:.2}ms p95={:.2}ms max={:.2}ms | draws={} quads={} glyphs={}\n",
            snap.gpu_adapter,
            snap.gpu_backend,
            snap.avg_frame_ms,
            snap.p95_frame_ms,
            snap.max_frame_ms,
            snap.last_draw_calls,
            snap.last_quads,
            snap.last_glyphs
        ));
        out.push_str("Hosts:\n");
        for h in &snap.hosts {
            out.push_str(&format!(
                "  - {} [{}] tailscale={} streams={} dropped={}\n",
                h.label, h.status, h.tailscale_summary, h.active_streams, h.dropped_events
            ));
        }
        out.push_str(&format!(
            "Keymap warnings: {}\n",
            snap.keymap_warnings.len()
        ));
        for w in &snap.keymap_warnings {
            out.push_str(&format!("  ! {w}\n"));
        }
        out.push_str(&format!("Active ports: {:?}\n", snap.active_ports));
        out.push_str(&format!("Active tunnels: {:?}\n", snap.active_tunnels));
        out.push_str(&format!(
            "Program Resume (OSC 7577): {}\n",
            snap.resume_records.len()
        ));
        for rec in &snap.resume_records {
            out.push_str(&format!("  - {rec}\n"));
        }
        out
    }

    pub(super) fn build_diagnostics_snapshot(&self) -> DiagnosticsSnapshot {
        let samples: Vec<u32> = self.frame_times_us.iter().copied().collect();
        let (avg_ms, p95_ms, max_ms) = if samples.is_empty() {
            (0.0, 0.0, 0.0)
        } else {
            let sum: u64 = samples.iter().map(|&v| u64::from(v)).sum();
            let avg = (sum as f32 / samples.len() as f32) / 1000.0;
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            let p95_idx = ((sorted.len() * 95) / 100).min(sorted.len() - 1);
            let p95 = sorted[p95_idx] as f32 / 1000.0;
            let max = sorted[sorted.len() - 1] as f32 / 1000.0;
            (avg, p95, max)
        };
        let (gpu_adapter, gpu_backend, last_draw_calls, last_quads, last_glyphs) =
            match &self.last_frame_stats {
                Some(s) => (
                    s.adapter_name.clone(),
                    format!("{:?}", s.backend),
                    s.draw_calls,
                    s.quad_count,
                    s.glyph_count,
                ),
                None => (
                    "None (headless/no frame)".to_owned(),
                    "Vulkan".to_owned(),
                    0,
                    0,
                    0,
                ),
            };

        let mut hosts = Vec::new();
        let mut active_ports = Vec::new();
        let mut resume_records = Vec::new();
        for host_id in &self.workspace.host_order {
            let Some(host) = self.workspace.host(host_id) else {
                continue;
            };
            let status = match &host.status {
                HostStatus::Connected {
                    api_revision,
                    resumed,
                    ..
                } => {
                    let ver = host
                        .server_version
                        .as_ref()
                        .map(|v| format!(" {}", v.rex_version))
                        .unwrap_or_default();
                    format!(
                        "Connected{ver} rev {}{}",
                        api_revision,
                        if *resumed { " (resumed)" } else { "" }
                    )
                }
                HostStatus::Connecting => "Connecting".to_owned(),
                HostStatus::Reconnecting { reason } => {
                    format!("Reconnecting ({})", redact_diagnostics_text(reason))
                }
                HostStatus::Disconnected => "Disconnected".to_owned(),
                HostStatus::Error(err) => format!("Error: {}", redact_diagnostics_text(err)),
            };
            let tailscale_summary = match &host.tailscale_status {
                Some(ts) => {
                    let dns = redact_diagnostics_text(ts.dns_name.as_deref().unwrap_or(""));
                    if let Some(err) = &ts.error {
                        format!("{} ({})", ts.state, redact_diagnostics_text(err))
                    } else {
                        format!("{} {dns}", ts.state).trim().to_owned()
                    }
                }
                None => "unavailable".to_owned(),
            };
            hosts.push(DiagnosticsHostRow {
                label: host.label.clone(),
                status,
                tailscale_summary,
                dropped_events: host.dropped_events_count,
                active_streams: self.pane_store.active_stream_count_for_host(host_id),
            });

            for sess in host.sessions.values() {
                for (bid, blk) in &sess.blocks {
                    for p in &blk.listening_ports {
                        let proc_lbl = blk
                            .port_process_label(p)
                            .unwrap_or_else(|| "tcp".to_owned());
                        active_ports.push(format!(
                            "{} {}:{} ({proc_lbl})",
                            host.label, p.address, p.port
                        ));
                    }
                    if let Some(rec) = blk.chosen_resume_record() {
                        let prog = if rec.program.trim().is_empty() {
                            "program"
                        } else {
                            rec.program.trim()
                        };
                        let trust = if rec.trusted {
                            match rec.rule.as_deref() {
                                Some(rule) if !rule.is_empty() => {
                                    format!("trusted: rule={rule}")
                                }
                                _ => "trusted".to_owned(),
                            }
                        } else {
                            format!(
                                "untrusted: {}",
                                rec.untrusted_reason_label().unwrap_or("not trusted")
                            )
                        };
                        // Never include `rec.command` argv in diagnostics (may contain secrets).
                        resume_records.push(format!(
                            "{}/{} · {} [{}]",
                            host.label, sess.label, prog, trust
                        ));
                    }
                    let _ = bid;
                }
            }
        }

        let active_tunnels = self
            .active_tunnels
            .iter()
            .map(|t| {
                format!(
                    "127.0.0.1:{} -> {}:{}:{} ({})",
                    t.local_port
                        .map_or_else(|| "?".to_owned(), |p| p.to_string()),
                    t.source_block.host_id,
                    t.remote_host,
                    t.remote_port,
                    t.tunnel_block_id.as_deref().unwrap_or("pending")
                )
            })
            .collect();

        let keymap_warnings = self
            .keymap_diagnostics
            .iter()
            .map(|d| format!("[{}] {}", d.severity, redact_diagnostics_text(&d.message)))
            .collect();

        let vt_version = vt_engine::build_info()
            .map(|b| b.version)
            .unwrap_or_else(|_| "unknown".to_owned());

        DiagnosticsSnapshot {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            vt_version,
            gpu_adapter,
            gpu_backend,
            frame_times_us: samples,
            avg_frame_ms: avg_ms,
            p95_frame_ms: p95_ms,
            max_frame_ms: max_ms,
            last_draw_calls,
            last_quads,
            last_glyphs,
            hosts,
            keymap_warnings,
            active_ports,
            active_tunnels,
            resume_records,
        }
    }

    /// Triggers a 220 ms diagonal GPU Copy Shine sweep over `rects` (clipped to `clip_rect`).
    /// Reusable by both M2 selection copy and M4 Vim Copy Mode `y` yank.
    pub fn trigger_copy_shine(&mut self, rects: Vec<Rect>, clip_rect: ClipRect, now: Instant) {
        self.trigger_copy_shine_at_layer(rects, clip_rect, layers::KITTY_ABOVE_TEXT + 5, now);
    }

    /// Triggers a 220 ms diagonal GPU Copy Shine sweep over `rects` at an explicit `gpu_renderer` layer
    /// (e.g. `layers::MODAL - 1` for docked viewer chrome vs `layers::KITTY_ABOVE_TEXT + 5` for panes).
    pub fn trigger_copy_shine_at_layer(
        &mut self,
        rects: Vec<Rect>,
        clip_rect: ClipRect,
        layer: i32,
        now: Instant,
    ) {
        if rects.is_empty() {
            return;
        }
        self.copy_shines.push(CopyShineEffect {
            key: None,
            rects,
            clip_rect,
            layer,
            started_at: now,
        });
        self.needs_redraw = true;
    }

    /// Triggers a 220 ms diagonal GPU Copy Shine over the active terminal selection of `key`.
    pub fn trigger_copy_shine_for_block(&mut self, key: &BlockKey, now: Instant) -> bool {
        let Some(ui_out) = self.last_ui_output.as_ref() else {
            return false;
        };
        let Some(metrics) = self.last_cell_metrics else {
            return false;
        };
        let terminal_rect = ui_out
            .tiled_panes
            .iter()
            .find(|p| p.id == key.block_id)
            .map(|p| p.terminal_rect)
            .or_else(|| {
                ui_out
                    .floating_panes
                    .iter()
                    .find(|p| p.id == key.block_id)
                    .map(|p| p.terminal_rect)
            });
        let Some(term_rect) = terminal_rect else {
            return false;
        };
        let (pad_x, pad_y) = self.pane_content_padding();
        let content_rect = rectf_to_gpu(term_rect.inset(pad_x, pad_y, pad_x, pad_y));
        let clip_rect = rectf_to_gpu(term_rect).to_clip_rect();
        let scroll_y = self.pane_store.scroll_offset_y(key);
        let Ok(rects) =
            self.pane_store
                .selection_viewport_rects(key, content_rect, metrics, scroll_y)
        else {
            return false;
        };
        if rects.is_empty() {
            return false;
        }
        self.copy_shines.push(CopyShineEffect {
            key: Some(key.clone()),
            rects,
            clip_rect,
            layer: layers::KITTY_ABOVE_TEXT + 5,
            started_at: now,
        });
        self.needs_redraw = true;
        true
    }

    /// Returns `true` when translucent surface material is active (Wayland transparency hint).
    pub fn has_translucent_material(&self) -> bool {
        matches!(self.settings.material, WindowMaterial::Translucent { .. })
    }

    /// Computes the earliest future [`Instant`] at which `app.rs` must wake up for scheduled work
    /// (background snapshot stepping, coalesced resize flush, prompt expiration, Kitty animation,
    /// or working pulse indicator).
    ///
    /// Returns `None` when completely idle so the Wayland event loop parks in `ControlFlow::Wait`
    /// with **0% idle CPU**.
    pub fn next_wakeup_deadline(&self, now: Instant) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        let mut consider = |t: Instant| {
            earliest = Some(earliest.map_or(t, |prev| prev.min(t)));
        };

        if self.pane_store.has_pending_snapshots() {
            consider(now + Duration::from_millis(16));
        }
        if !self.read_only
            && let Some(resize_due) = self.workspace.next_resize_deadline()
        {
            consider(resize_due);
        }
        if let Some(active) = self.prompts.active()
            && active.timeout_ms > 0
        {
            consider(active.deadline);
        }
        if let Some(anim_ms) = self.next_kitty_anim_ms {
            consider(self.started_at + Duration::from_millis(anim_ms));
        }
        if let Some(media_due) = self.media_backend.next_wakeup_instant() {
            consider(media_due);
        }
        if self.window_focused && self.has_visible_working_indicator() {
            consider(now + Duration::from_millis(120));
        }
        for &deadline in self.toast_deadlines.values() {
            consider(deadline);
        }
        for shine in &self.copy_shines {
            let end = shine.started_at + COPY_SHINE_DURATION;
            if end > now {
                consider(end.min(now + Duration::from_millis(8)));
            }
        }
        if self.ui.has_active_animations() || self.pane_store.has_active_scroll_inertia() {
            consider(now + Duration::from_millis(8));
        }
        earliest
    }

    /// Returns the appropriate native Wayland cursor icon for the current drag or hovered hit target.
    pub fn cursor_hint(&self) -> CursorHint {
        if let Some(drag) = &self.active_drag {
            return match drag {
                ActiveDrag::SidebarResize { .. } | ActiveDrag::ViewerDivider { .. } => {
                    CursorHint::ColResize
                }
                ActiveDrag::SplitDivider { divider, .. } => match divider.axis {
                    SplitAxis::Horizontal => CursorHint::ColResize,
                    SplitAxis::Vertical => CursorHint::RowResize,
                },
                ActiveDrag::FloatingLayer { zone, .. } => floating_zone_cursor(*zone, true),
                ActiveDrag::TerminalSelection { .. } | ActiveDrag::ViewerTextSelection { .. } => {
                    CursorHint::Text
                }
                ActiveDrag::PaneScrollbar { .. }
                | ActiveDrag::ViewerScrollbar { .. }
                | ActiveDrag::ViewerVideoSeek { .. }
                | ActiveDrag::ViewerAudioSeek { .. } => CursorHint::Pointer,
                ActiveDrag::ViewerPan { .. } => CursorHint::Grabbing,
                ActiveDrag::PaneMove { activated, .. }
                | ActiveDrag::TabReorder { activated, .. } => {
                    if *activated {
                        CursorHint::Grabbing
                    } else {
                        CursorHint::Pointer
                    }
                }
            };
        }
        let Some(out) = &self.last_ui_output else {
            return CursorHint::Default;
        };
        let (px, py) = self.pointer_pos;
        match out.draw_list.hit_test(px, py) {
            Some(HitTarget::SidebarResizeHandle | HitTarget::ViewerDivider) => {
                CursorHint::ColResize
            }
            Some(HitTarget::SplitDivider { split_path }) => out
                .dividers
                .iter()
                .find(|d| &d.split_path == split_path)
                .map_or(CursorHint::ColResize, |d| match d.axis {
                    SplitAxis::Horizontal => CursorHint::ColResize,
                    SplitAxis::Vertical => CursorHint::RowResize,
                }),
            Some(HitTarget::FloatingHandle { zone, .. }) => floating_zone_cursor(*zone, false),
            Some(HitTarget::PaneSurface { .. }) => {
                if self.hovered_terminal_target.is_some() {
                    CursorHint::Pointer
                } else {
                    CursorHint::Text
                }
            }
            Some(HitTarget::ViewerCanvas) => CursorHint::Pointer,
            Some(
                HitTarget::ModalInput
                | HitTarget::FindBarInput
                | HitTarget::SidebarFilterInput
                | HitTarget::ViewerSearchInput
                | HitTarget::ViewerTextBody,
            ) => CursorHint::Text,
            Some(
                HitTarget::PaneScrollbarTrack { .. }
                | HitTarget::PaneScrollbarThumb { .. }
                | HitTarget::ViewerScrollbarTrack
                | HitTarget::ViewerScrollbarThumb
                | HitTarget::ViewerVideoSeekBar
                | HitTarget::ViewerAudioSeekBar
                | HitTarget::ViewerAudioWaveform,
            ) => CursorHint::Pointer,
            Some(target) if target.is_pointer_clickable() => CursorHint::Pointer,
            _ => CursorHint::Default,
        }
    }

    fn has_visible_working_indicator(&self) -> bool {
        if self.ui.layout_cfg.sidebar_collapsed
            && !self.ui.layout_cfg.toolbar_revealed
            && self.ui.layout_cfg.toolbar_behavior != ToolbarBehavior::Always
        {
            return false;
        }
        if self
            .last_ui_output
            .as_ref()
            .is_some_and(|out| out.chrome.sidebar.is_none() && out.chrome.tab_bar.is_none())
        {
            return false;
        }
        self.workspace.hosts.values().any(|host| {
            host.sessions.values().any(|sess| {
                sess.blocks.values().any(|blk| {
                    blk.program_statuses
                        .iter()
                        .any(|r| r.state.eq_ignore_ascii_case("working"))
                })
            })
        })
    }

    /// Drains queued [`HostCommand`]s (used primarily in unit tests without a live `HostManager`).
    pub fn drain_pending_commands(&mut self) -> Vec<HostCommand> {
        std::mem::take(&mut self.pending_commands)
    }

    /// Returns the last rendered [`UiRenderOutput`], if a frame has been built.
    pub fn last_ui_output(&self) -> Option<&UiRenderOutput> {
        self.last_ui_output.as_ref()
    }

    /// Returns the currently active `(host_id, session_id)` key, if any.
    pub fn active_session_key(&self) -> Option<&SessionKey> {
        self.workspace.active_session.as_ref()
    }

    /// Returns the currently focused terminal [`BlockKey`], if any.
    pub fn focused_block_key(&self) -> Option<BlockKey> {
        let sk = self.workspace.active_session.as_ref()?;
        if let Some(block_id) = self.ui.focus.last_terminal_block() {
            return Some(BlockKey::new(
                sk.host_id.clone(),
                sk.session_id.clone(),
                block_id.to_owned(),
            ));
        }
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        let bid = primary_block_id_of_window(win)?;
        Some(BlockKey::new(
            sk.host_id.clone(),
            sk.session_id.clone(),
            bid.to_owned(),
        ))
    }

    fn send_command(&mut self, mut cmd: HostCommand) {
        let origin = (self.current_window_ctx_id != 0).then_some(self.current_window_ctx_id);
        match &mut cmd {
            HostCommand::NewWindow {
                origin_window_id, ..
            }
            | HostCommand::NewSession {
                origin_window_id, ..
            }
            | HostCommand::CreateCommandSession {
                origin_window_id, ..
            } if origin_window_id.is_none() => {
                *origin_window_id = origin;
            }
            _ => {}
        }
        if let Some(hm) = &self.host_manager {
            if let Err(err) = hm.send(cmd) {
                tracing::warn!(?err, "HostManager command channel closed");
            }
        } else {
            self.pending_commands.push(cmd);
        }
    }

    fn font_size_px(&self) -> f32 {
        (self.settings.font_size_pt * (96.0 / 72.0))
            .round()
            .clamp(9.0, 48.0)
    }

    fn pane_content_padding(&self) -> (f32, f32) {
        match self.ui.layout_cfg.density {
            PaneDensity::Comfortable => (12.0, 10.0),
            PaneDensity::Compact => (6.0, 4.0),
        }
    }

    fn toast_ttl(severity: ToastSeverity) -> Duration {
        match severity {
            ToastSeverity::Info | ToastSeverity::Success => Duration::from_secs(4),
            ToastSeverity::Warning => Duration::from_secs(6),
            ToastSeverity::Error => Duration::from_secs(8),
        }
    }

    fn push_toast(
        &mut self,
        severity: ToastSeverity,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        self.push_toast_at(severity, title, body, Instant::now());
    }

    fn push_toast_at(
        &mut self,
        severity: ToastSeverity,
        title: impl Into<String>,
        body: impl Into<String>,
        now: Instant,
    ) {
        let title = title.into();
        let body = body.into();
        let deadline = now + Self::toast_ttl(severity);

        // Deduplicate identical active toasts by refreshing their expiry instead of stacking clones.
        if let Some(existing) = self
            .ui
            .toasts
            .iter()
            .find(|t| t.severity == severity && t.title == title && t.body == body)
        {
            self.toast_deadlines.insert(existing.id, deadline);
            self.needs_redraw = true;
            return;
        }

        let id = self.next_toast_id;
        self.next_toast_id = self.next_toast_id.saturating_add(1);
        self.ui.toasts.push(ToastItem {
            id,
            severity,
            title,
            body,
        });
        self.toast_deadlines.insert(id, deadline);
        if self.ui.toasts.len() > 5 {
            let evicted = self.ui.toasts.remove(0);
            self.toast_deadlines.remove(&evicted.id);
        }
        self.needs_redraw = true;
    }

    fn flush_ready_resizes(&mut self, now: Instant) {
        if self.read_only {
            return;
        }
        let ready_resizes = self.workspace.drain_ready_resizes(now);
        for (key, args) in ready_resizes {
            // Do NOT optimistically resize local VT before Rex's inline `RexApc::Resize` arrives
            // in `/api/data` (prevents pre-resize 80-col output from corrupting a wide local grid).
            // Only claim an *unowned* block here (`OwnMode::Request`). Never `Take` from this
            // path: a queued resize is not a user action, and taking ownership from another
            // device (e.g. the Mac mini showing the same tab) makes both clients steal it back
            // and forth, resizing the PTY up and down in a loop. If another device owns the
            // block, Rex records this resize as *desired* and applies it once we own it again
            // (keypress, paste, window resize, focus gain or explicit `t3_fit`).
            self.maybe_claim_ownership(&key, OwnershipTrigger::AttachOrView);
            self.send_command(HostCommand::TerminalResize {
                host_id: key.host_id,
                session_id: key.session_id,
                block_id: key.block_id,
                args,
            });
            self.needs_redraw = true;
        }
    }

    /// Periodic maintenance tick: advances background snapshots, expires timed-out prompts,
    /// drains coalesced resizes, and returns `true` when a GPU repaint is warranted.
    pub fn tick(&mut self, now: Instant) -> bool {
        // 1. Bounded background GHOSTSNP scrollback stepping (<= 2ms per frame)
        if self.pane_store.has_pending_snapshots()
            && let Ok(summary) = self
                .pane_store
                .step_background_snapshots(DEFAULT_BACKGROUND_SNAPSHOT_BUDGET, &mut self.workspace)
        {
            if !summary.dirty_panes.is_empty() || summary.panes_finished > 0 {
                self.needs_redraw = true;
            }
            for (key, events) in summary.terminal_events {
                self.handle_terminal_events(&key, &events, now);
            }
        }

        // 2. Drain coalesced resizes whose READY gate + debounce elapsed
        self.flush_ready_resizes(now);

        // 3. Check active `rex.ui` prompt expiration
        if let Some(active) = self.prompts.active()
            && active.is_expired(now)
        {
            let call_id = active.call_id.clone();
            self.prompts
                .on_prompt_ended(&tr3x_bridge::PromptEndedEvent {
                    call_id: call_id.clone(),
                    reason: "timeout".to_owned(),
                });
            self.ui.dismiss_rex_prompt_if_matches(&call_id);
            self.active_prompt_host = None;
            self.needs_redraw = true;
        }

        // 4. Advance Kitty animations
        let now_ms = now.saturating_duration_since(self.started_at).as_millis() as u64;
        self.next_kitty_anim_ms = self.pane_store.tick_kitty_animations(now_ms);
        if self.next_kitty_anim_ms.is_some() {
            self.needs_redraw = true;
        }

        // 5. Poll async MediaViewerBackend events (text loads, decoded images/GIFs, video frames)
        if self.poll_media_events() {
            self.needs_redraw = true;
        }

        // 6. Auto-dismiss expired toasts so transient notices never accumulate permanently.
        if !self.toast_deadlines.is_empty() {
            let before = self.ui.toasts.len();
            self.toast_deadlines.retain(|_, deadline| *deadline > now);
            self.ui
                .toasts
                .retain(|t| self.toast_deadlines.contains_key(&t.id));
            if self.ui.toasts.len() != before {
                self.needs_redraw = true;
            }
        }

        // 7. Advance UI chrome spring & slide/fade transitions, gating resize flush until settled.
        let was_layout_animating = self.ui.is_layout_animating();
        if self.ui.tick_animations(now) {
            self.needs_redraw = true;
        }
        if was_layout_animating && !self.ui.is_layout_animating() {
            self.force_immediate_resize = true;
            self.needs_redraw = true;
        }

        // 8. Advance smooth-scroll inertia momentum.
        if self.pane_store.has_active_scroll_inertia() {
            let dt = match self.last_inertia_tick {
                Some(prev) => now
                    .saturating_duration_since(prev)
                    .as_secs_f32()
                    .clamp(0.0, 0.05),
                None => 1.0 / 120.0,
            };
            self.last_inertia_tick = Some(now);
            if self.pane_store.tick_scroll_inertia(dt) {
                self.needs_redraw = true;
            }
        } else {
            self.last_inertia_tick = None;
        }

        // 9. Advance & prune 220 ms diagonal GPU Copy Shine effects.
        if !self.copy_shines.is_empty() {
            self.copy_shines
                .retain(|s| now.saturating_duration_since(s.started_at) < COPY_SHINE_DURATION);
            // Redraw while active AND on the frame an effect expires so the shine quad clears.
            self.needs_redraw = true;
        }

        self.needs_redraw()
    }

    pub(super) fn release_viewer_tab_textures(&mut self, tab_id: u64) {
        if let Some(keys) = self.viewer_tab_texture_keys.remove(&tab_id) {
            self.pending_viewer_texture_removals.extend(keys);
        }
    }

    fn record_viewer_tab_texture_key(&mut self, tab_id: u64, key: u64) {
        let keys = self.viewer_tab_texture_keys.entry(tab_id).or_default();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }

    /// Drains ready [`MediaEvent`]s from [`MediaViewerBackend`], updating `ui.viewer` tab states
    /// and queueing decoded RGBA8 frames for zero-copy GPU upload in [`Self::render_frame`].
    pub fn poll_media_events(&mut self) -> bool {
        let events = self.media_backend.poll_events();
        if events.is_empty() {
            return false;
        }
        for ev in events {
            match ev {
                MediaEvent::TextLoaded {
                    tab_id,
                    content,
                    is_reload,
                } => {
                    let is_md = matches!(
                        ui_chrome::classify_extension(&content.display_path),
                        TargetKind::Markdown
                    );
                    let lang = Path::new(&content.display_path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|s| s.to_ascii_lowercase());
                    let joined = content.lines.join("\n");

                    self.release_viewer_tab_textures(tab_id);
                    let mut inline_map = HashMap::new();
                    let inline_gen = self.viewer_upload_gen;
                    self.viewer_upload_gen = self.viewer_upload_gen.wrapping_add(1).max(1);
                    for (idx, img) in content.inline_images.into_iter().enumerate() {
                        let texture_key = 0x9000_0000_0000_0000
                            | ((tab_id & 0xFFFF_FFFF) << 16)
                            | (idx as u64 & 0xFFFF);
                        self.record_viewer_tab_texture_key(tab_id, texture_key);
                        inline_map.insert(
                            img.src,
                            MarkdownInlineImage::Loaded {
                                texture_key,
                                width: img.width,
                                height: img.height,
                            },
                        );
                        self.pending_viewer_uploads.push(PendingViewerUpload {
                            texture_key,
                            generation: inline_gen,
                            frame: img.frame,
                        });
                    }

                    if is_reload
                        && let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Text(existing) = &mut tab.content
                    {
                        existing.set_text(joined);
                        existing.file_size_bytes = Some(content.file_size_bytes);
                        existing.inline_images = inline_map;
                        existing.set_git_diff(content.git_diff);
                        if self.ui.viewer.active_tab().map(|t| t.id) == Some(tab_id)
                            && self.ui.viewer.search.open
                        {
                            self.ui.viewer.recompute_search();
                        }
                        continue;
                    }

                    let mut state = TextViewerState::new(joined, lang, is_md);
                    state.mono_cell_w = self.ui.viewer.mono_cell_w;
                    state.file_size_bytes = Some(content.file_size_bytes);
                    state.inline_images = inline_map;
                    state.set_git_diff(content.git_diff);
                    if let Some((line, col)) = self.pending_tab_jump_lines.remove(&tab_id) {
                        let viewport_h = self
                            .last_ui_output
                            .as_ref()
                            .and_then(|o| o.viewer_split)
                            .map(|vs| (vs.viewer_rect.h - 60.0).max(200.0))
                            .or_else(|| {
                                (self.viewport_h > 120.0)
                                    .then(|| (self.viewport_h - 100.0).max(200.0))
                            });
                        state.jump_to_line(line as usize, col.map(|c| c as usize), viewport_h);
                    }
                    self.ui
                        .viewer
                        .set_tab_content(tab_id, ViewerContent::Text(state));
                    if self.ui.viewer.active_tab().map(|t| t.id) == Some(tab_id)
                        && self.ui.viewer.search.open
                    {
                        self.ui.viewer.recompute_search();
                    }
                }
                MediaEvent::ImageLoaded {
                    tab_id,
                    texture_key,
                    generation,
                    metadata,
                    frame,
                    ..
                } => {
                    self.pending_tab_jump_lines.remove(&tab_id);
                    self.record_viewer_tab_texture_key(tab_id, texture_key);
                    let has_alpha = matches!(
                        metadata.format_label.as_str(),
                        "PNG" | "WebP" | "WebP (Animated)" | "GIF" | "GIF (Animated)" | "SVG"
                    );
                    let state = ImageViewerState::new(
                        texture_key,
                        metadata.width,
                        metadata.height,
                        metadata.format_label,
                        Some(metadata.file_size_bytes),
                        has_alpha,
                    );
                    self.ui
                        .viewer
                        .set_tab_content(tab_id, ViewerContent::Image(state));
                    self.pending_viewer_uploads.push(PendingViewerUpload {
                        texture_key,
                        generation,
                        frame,
                    });
                }
                MediaEvent::AnimationFrame {
                    texture_key,
                    generation,
                    frame,
                    ..
                } => {
                    self.pending_viewer_uploads.push(PendingViewerUpload {
                        texture_key,
                        generation,
                        frame,
                    });
                }
                MediaEvent::VideoLoaded {
                    tab_id,
                    texture_key,
                    generation,
                    metadata,
                    poster_frame,
                    state: pb_state,
                    ..
                } => {
                    self.pending_tab_jump_lines.remove(&tab_id);
                    self.record_viewer_tab_texture_key(tab_id, texture_key);
                    let fps = (metadata.fps.is_finite() && metadata.fps > 0.0)
                        .then_some(metadata.fps as f32);
                    let codec = (!metadata.video_codec.is_empty()).then_some(metadata.video_codec);
                    let mut vstate = VideoViewerState::new(
                        texture_key,
                        metadata.render_width.max(metadata.width).max(1),
                        metadata.render_height.max(metadata.height).max(1),
                        metadata.duration_secs,
                        fps,
                        codec,
                        Some(metadata.file_size_bytes),
                    );
                    vstate.playing = pb_state.playing;
                    vstate.position_secs = pb_state.position_secs;
                    vstate.muted = pb_state.muted;
                    vstate.looping = pb_state.looping;
                    self.ui
                        .viewer
                        .set_tab_content(tab_id, ViewerContent::Video(vstate));
                    if let Some(frame) = poster_frame {
                        self.pending_viewer_uploads.push(PendingViewerUpload {
                            texture_key,
                            generation,
                            frame,
                        });
                    }
                }
                MediaEvent::VideoFrame {
                    tab_id,
                    texture_key,
                    generation,
                    position_secs,
                    frame,
                    state: pb_state,
                    ..
                } => {
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Video(vid) = &mut tab.content
                    {
                        vid.position_secs = position_secs;
                        vid.playing = pb_state.playing;
                        vid.muted = pb_state.muted;
                        vid.looping = pb_state.looping;
                    }
                    self.pending_viewer_uploads.push(PendingViewerUpload {
                        texture_key,
                        generation,
                        frame,
                    });
                }
                MediaEvent::VideoStateChanged { tab_id, state } => {
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Video(vid) = &mut tab.content
                    {
                        vid.playing = state.playing;
                        vid.position_secs = state.position_secs;
                        vid.muted = state.muted;
                        vid.looping = state.looping;
                    }
                }
                MediaEvent::AudioLoaded {
                    tab_id,
                    metadata,
                    state: pb_state,
                } => {
                    self.pending_tab_jump_lines.remove(&tab_id);
                    let mut astate = AudioViewerState::new(
                        metadata.duration_secs,
                        metadata.waveform_peaks,
                        (!metadata.codec.is_empty()).then_some(metadata.codec),
                        metadata.sample_rate_hz,
                        metadata.channels,
                        metadata.bitrate_kbps,
                        Some(metadata.file_size_bytes),
                    );
                    astate.warning_banner = metadata.warning_banner;
                    astate.playing = pb_state.playing;
                    astate.position_secs = pb_state.position_secs;
                    astate.muted = pb_state.muted;
                    astate.looping = pb_state.looping;
                    self.ui
                        .viewer
                        .set_tab_content(tab_id, ViewerContent::Audio(astate));
                }
                MediaEvent::AudioStateChanged { tab_id, state } => {
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Audio(audio) = &mut tab.content
                    {
                        audio.playing = state.playing;
                        audio.position_secs = state.position_secs;
                        audio.muted = state.muted;
                        audio.looping = state.looping;
                    }
                }
                MediaEvent::PdfLoaded {
                    tab_id,
                    texture_key,
                    generation,
                    metadata,
                    page_frame,
                } => {
                    let pending_jump = self.pending_tab_jump_lines.remove(&tab_id);
                    self.record_viewer_tab_texture_key(tab_id, texture_key);
                    let mut pstate = PdfViewerState::new(
                        texture_key,
                        metadata.current_page,
                        metadata.page_count,
                        metadata.page_width,
                        metadata.page_height,
                        Some(metadata.file_size_bytes),
                        metadata.page_text,
                    );
                    pstate.fallback_notice = metadata.fallback_notice;
                    if let Some((target_page, _)) = pending_jump
                        && target_page >= 1
                    {
                        let clamped = (target_page as usize).clamp(1, metadata.page_count.max(1));
                        if clamped != metadata.current_page
                            && self.media_backend.pdf_goto_page(tab_id, clamped)
                        {
                            pstate.loading_page = true;
                        }
                    }
                    self.ui
                        .viewer
                        .set_tab_content(tab_id, ViewerContent::Pdf(pstate));
                    if let Some(frame) = page_frame {
                        self.pending_viewer_uploads.push(PendingViewerUpload {
                            texture_key,
                            generation,
                            frame,
                        });
                    }
                }
                MediaEvent::PdfPageLoaded {
                    tab_id,
                    texture_key,
                    generation,
                    metadata,
                    page_frame,
                } => {
                    self.record_viewer_tab_texture_key(tab_id, texture_key);
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Pdf(pdf) = &mut tab.content
                    {
                        pdf.current_page = metadata.current_page;
                        pdf.page_count = metadata.page_count;
                        pdf.page_width = metadata.page_width;
                        pdf.page_height = metadata.page_height;
                        pdf.page_text = metadata.page_text;
                        pdf.fallback_notice = metadata.fallback_notice;
                        pdf.loading_page = false;
                        pdf.viewport.pan_offset = (0.0, 0.0);
                    }
                    if let Some(frame) = page_frame {
                        self.pending_viewer_uploads.push(PendingViewerUpload {
                            texture_key,
                            generation,
                            frame,
                        });
                    }
                }
                MediaEvent::LoadError {
                    tab_id,
                    error,
                    is_reload,
                } => {
                    self.pending_tab_jump_lines.remove(&tab_id);
                    let existing_label = self
                        .ui
                        .viewer
                        .tab_by_id(tab_id)
                        .filter(|t| {
                            !matches!(
                                t.content,
                                ViewerContent::Loading { .. } | ViewerContent::Error { .. }
                            )
                        })
                        .map(|t| t.title.clone());
                    if is_reload && let Some(label) = existing_label {
                        self.push_toast_at(
                            ToastSeverity::Warning,
                            format!("Reload paused for {label}"),
                            error,
                            Instant::now(),
                        );
                    } else {
                        self.ui.viewer.set_tab_error(tab_id, error);
                    }
                }
            }
        }
        true
    }

    // =========================================================================
    // 1. Full Composite Frame Pipeline (`UiTree` + `PaneStore` -> `SceneFrame`)
    // =========================================================================

    /// Builds a complete composite GPU [`SceneFrame`] combining `ui-chrome` (sidebar/tab bar,
    /// inset pane cards, headers, scrollbars, floating `/pr` drawer, modals, toasts) with
    /// `PaneStore`'s live `RowId`-cached terminals and Kitty Graphics Protocol image layers.
    pub fn render_frame(&mut self, renderer: &mut GpuRenderer, now: Instant) -> Result<SceneFrame> {
        self.render_frame_shared(renderer.shared(), now)
    }

    /// Shared-GPU multi-window entrypoint for building a composite [`SceneFrame`] from [`GpuShared`].
    ///
    /// Preserves strict `atlas -> images` mutex lock ordering and drops all GPU locks before
    /// returning so `SurfaceRenderer::render_to_surface` can acquire them cleanly.
    pub fn render_frame_shared(&mut self, shared: &GpuShared, now: Instant) -> Result<SceneFrame> {
        let _ = self.tick(now);

        // Ensure font family in atlas matches settings and measure cell metrics.
        let font_px = self.font_size_px();
        let metrics = {
            let mut atlas = shared.atlas();
            if !self.settings.font_family.is_empty()
                && atlas.mono_family_name() != self.settings.font_family
            {
                atlas.set_mono_family(self.settings.font_family.clone());
            }
            atlas.measure_monospace_cell(font_px, self.scale_factor)
        };
        self.last_cell_metrics = Some(metrics);

        // Refresh sampled `program_bg` for visible panes prior to `build_ui_frame_input` so
        // frame 1 (including single-pass `--headless-render`) captures fullscreen TUI colors.
        if let Some(sk) = self.workspace.active_session.clone()
            && let Some(view) = self.workspace.session(&sk).and_then(|s| s.view.as_ref())
            && let Some(win) = active_window_of_view(view)
        {
            let block_ids: Vec<String> = win
                .layers
                .iter()
                .flat_map(|l| l.blocks.iter())
                .filter_map(|b| b.block_id.clone())
                .collect();
            for bid in block_ids {
                let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), bid);
                self.pane_store.refresh_program_bg(&key);
            }
        }

        // Build `UiFrameInput` from live multi-host workspace + `tr3x` tab cards.
        let frame_input = self.build_ui_frame_input(now);

        // Update T3 session hint on pane options & keep active sidebar tab cleanly in view.
        self.ui.pane_options.is_t3_session = is_t3_session(&frame_input.active_session_label);
        self.ui.sync_sidebar_scroll(&frame_input);

        let anim_phase = now.saturating_duration_since(self.started_at).as_secs_f32();
        let viewer_lh = (12.5_f32 * 1.25).ceil();
        let ui_output = {
            let mut atlas = shared.atlas();
            let exact_mono_w = atlas.measure_text_advance(
                "MMMMMMMMMM",
                12.5,
                viewer_lh,
                &gpu_renderer::FontFamily::Monospace,
                gpu_renderer::FontWeight::Regular,
                gpu_renderer::FontStyle::Normal,
            ) / 10.0;
            self.ui.viewer.mono_cell_w = exact_mono_w.max(1.0);
            let mut text_measurer = AtlasTextMeasurer(&mut atlas);
            self.ui.render_with_measurer(
                &frame_input,
                &self.theme,
                now,
                anim_phase,
                Some(&mut text_measurer),
            )
        };
        self.ui.sync_viewer_metrics(&ui_output.viewer_metrics);

        // Release GPU textures for any closed or reloaded viewer tabs and flush newly decoded frames.
        if !self.pending_viewer_texture_removals.is_empty()
            || !self.pending_viewer_uploads.is_empty()
        {
            let mut images = shared.images();
            if !self.pending_viewer_texture_removals.is_empty() {
                let removals = std::mem::take(&mut self.pending_viewer_texture_removals);
                for key in removals {
                    images.remove(key);
                }
            }
            if !self.pending_viewer_uploads.is_empty() {
                let uploads = std::mem::take(&mut self.pending_viewer_uploads);
                let device = shared.device();
                let queue = shared.queue();
                for up in uploads {
                    if let Err(err) = images.upload_rgba8_frame(
                        device,
                        queue,
                        up.texture_key,
                        up.generation,
                        up.frame.width,
                        up.frame.height,
                        &up.frame.rgba,
                    ) {
                        tracing::warn!(
                            ?err,
                            texture_key = up.texture_key,
                            "failed to upload viewer RGBA8 frame"
                        );
                    }
                }
            }
        }

        // Upload any pending local image hover thumbnail before locking `atlas` + `images` together.
        if let Some((img_path, generation)) = self.pending_hover_image.take() {
            let already_loaded = self
                .loaded_hover_image
                .as_ref()
                .is_some_and(|(p, _)| p == &img_path);
            if already_loaded {
                self.hover_image_id = Some(HOVER_PREVIEW_IMAGE_GPU_ID);
            } else if let Ok(meta) = std::fs::metadata(&img_path)
                && meta.is_file()
                && meta.len() <= MAX_HOVER_IMAGE_BYTES
                && let Ok(bytes) = std::fs::read(&img_path)
                && self
                    .set_hover_preview_image_shared(
                        shared,
                        HOVER_PREVIEW_IMAGE_GPU_ID,
                        generation,
                        &bytes,
                    )
                    .is_ok()
            {
                self.loaded_hover_image = Some((img_path, generation));
            }
        }

        // Track currently visible pane keys so `needs_redraw()` ignores background tabs (0% idle CPU).
        self.visible_pane_keys.clear();
        if let Some(sk) = &self.workspace.active_session {
            for geo in &ui_output.tiled_panes {
                self.visible_pane_keys.push(BlockKey::new(
                    sk.host_id.clone(),
                    sk.session_id.clone(),
                    geo.id.clone(),
                ));
            }
            for fgeo in &ui_output.floating_panes {
                self.visible_pane_keys.push(BlockKey::new(
                    sk.host_id.clone(),
                    sk.session_id.clone(),
                    fgeo.id.clone(),
                ));
            }
        }

        // Ensure `/api/data` streams are open for all visible panes in the active window.
        self.ensure_visible_pane_streams(&ui_output);

        let phys_w = (self.viewport_w * self.scale_factor).round().max(1.0) as u32;
        let phys_h = (self.viewport_h * self.scale_factor).round().max(1.0) as u32;

        let mut draw_list = DrawList::new(phys_w, phys_h, self.scale_factor);
        let tokens = self.theme.effective_ui();
        draw_list.set_clear_color(Color::from(tokens.surface_frame));
        draw_list.set_light_mode(!self.theme.is_dark);

        let active_sk = self.workspace.active_session.clone();
        let (pad_x, pad_y) = self.pane_content_padding();
        let dim_unfocused =
            self.ui.pane_options.fade_inactive_panes && ui_output.tiled_panes.len() >= 2;

        // Queue desired grid sizes for all visible panes first and flush any immediate resizes
        // synchronously within this same frame so Hyprland resize events have zero latency!
        // Skip while a docked sidebar or viewer slide transition is animating to avoid PTY
        // `SIGWINCH` storms (`force_immediate_resize` flushes the final size on settle).
        if !self.ui.is_layout_animating() {
            if let Some(sk) = &active_sk {
                for geo in &ui_output.tiled_panes {
                    let key =
                        BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), geo.id.clone());
                    let content_rect = geo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                    let (cols, rows) = compute_grid_cols_rows(content_rect, metrics);
                    self.last_pane_grids.insert(key.clone(), (cols, rows));
                    self.maybe_queue_pane_resize(&key, cols, rows, metrics, now);
                }
                for fgeo in &ui_output.floating_panes {
                    let key =
                        BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), fgeo.id.clone());
                    let content_rect = fgeo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                    let (cols, rows) = compute_grid_cols_rows(content_rect, metrics);
                    self.last_pane_grids.insert(key.clone(), (cols, rows));
                    self.maybe_queue_pane_resize(&key, cols, rows, metrics, now);
                }
            }
            self.force_immediate_resize = false;
            self.flush_ready_resizes(now);
        }

        let mut program_bg_changed_any = false;
        let accent = Color::from(tokens.accent_primary);
        let shine_color = Color::rgba(
            accent.r * 0.45 + 0.55,
            accent.g * 0.45 + 0.55,
            accent.b * 0.45 + 0.55,
            0.48,
        );

        let mut floating_frames: HashMap<String, SceneFrame> = HashMap::new();
        {
            // Strict lock order: `atlas` first, then `images`.
            let mut atlas = shared.atlas();
            {
                let mut images = shared.images();

                // Render tiled panes into layers 10..50 (above CANVAS=0 pane card fill, below CHROME=60).
                if let Some(sk) = &active_sk {
                    for geo in &ui_output.tiled_panes {
                        let key = BlockKey::new(
                            sk.host_id.clone(),
                            sk.session_id.clone(),
                            geo.id.clone(),
                        );
                        let content_rect = geo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                        let (cols, rows) = compute_grid_cols_rows(content_rect, metrics);

                        let focused = self.ui.focus.last_terminal_block() == Some(geo.id.as_str())
                            || ui_output.tiled_panes.len() == 1;
                        let scroll_y = self.pane_store.scroll_offset_y(&key);
                        let overscan = if scroll_y.abs() > 0.01 {
                            Overscan { above: 1, below: 1 }
                        } else {
                            Overscan { above: 0, below: 0 }
                        };
                        let clip_rect = if scroll_y.abs() > 0.01 {
                            rectf_to_gpu(geo.terminal_rect).to_clip_rect()
                        } else {
                            rectf_to_gpu(content_rect).to_clip_rect()
                        };
                        let viewport = build_terminal_viewport(
                            rectf_to_gpu(content_rect),
                            clip_rect,
                            cols,
                            rows,
                            metrics,
                            scroll_y,
                            &self.theme.palette,
                            focused,
                            dim_unfocused,
                        );

                        let cursor_override = self.copy_mode_cursor_override(
                            &key,
                            accent,
                            Color::from(self.theme.palette.terminal.background),
                        );
                        match self.pane_store.render_pane_parts_with_cursor_override(
                            &key,
                            &viewport,
                            overscan,
                            shared.device(),
                            shared.queue(),
                            &mut atlas,
                            &mut images,
                            &mut draw_list,
                            cursor_override,
                        ) {
                            Ok(Some(res)) => {
                                if res.program_bg_changed {
                                    program_bg_changed_any = true;
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::warn!(?err, block = %geo.id, "failed to render tiled pane");
                            }
                        }

                        // Draw crisp accent underline spans across all wrapped rows of a hovered link/path.
                        if let Some((hover_key, spans)) = &self.hovered_link_spans
                            && hover_key == &key
                        {
                            let underline_clip = rectf_to_gpu(if scroll_y.abs() > 0.01 {
                                geo.terminal_rect
                            } else {
                                content_rect
                            });
                            render::emit_hovered_link_underlines(
                                spans,
                                content_rect,
                                underline_clip,
                                metrics,
                                scroll_y,
                                Color::from(tokens.accent_primary),
                                &mut draw_list,
                            );
                        }
                    }
                }

                // Emit tiled Copy Shine quads on `layers::KITTY_ABOVE_TEXT + 5` (above terminal text, below CHROME).
                let floating_ids: HashSet<&str> = ui_output
                    .floating_panes
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect();
                for shine in &self.copy_shines {
                    let is_floating = shine
                        .key
                        .as_ref()
                        .is_some_and(|k| floating_ids.contains(k.block_id.as_str()));
                    if !is_floating {
                        emit_copy_shine_effect(shine, shine_color, now, &mut draw_list);
                    }
                }

                // Pre-render floating pane terminals into scratch DrawLists so we can splice them at the
                // exact Z-layer above their floating card backgrounds and below floating headers & modals!
                if let Some(sk) = &active_sk {
                    for fgeo in &ui_output.floating_panes {
                        let key = BlockKey::new(
                            sk.host_id.clone(),
                            sk.session_id.clone(),
                            fgeo.id.clone(),
                        );
                        let content_rect = fgeo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                        let (cols, rows) = compute_grid_cols_rows(content_rect, metrics);

                        let scroll_y = self.pane_store.scroll_offset_y(&key);
                        let overscan = if scroll_y.abs() > 0.01 {
                            Overscan { above: 1, below: 1 }
                        } else {
                            Overscan { above: 0, below: 0 }
                        };
                        let clip_rect = if scroll_y.abs() > 0.01 {
                            rectf_to_gpu(fgeo.terminal_rect).to_clip_rect()
                        } else {
                            rectf_to_gpu(content_rect).to_clip_rect()
                        };
                        let viewport = build_terminal_viewport(
                            rectf_to_gpu(content_rect),
                            clip_rect,
                            cols,
                            rows,
                            metrics,
                            scroll_y,
                            &self.theme.palette,
                            true,
                            false,
                        );
                        let mut scratch = DrawList::new(phys_w, phys_h, self.scale_factor);
                        let cursor_override = self.copy_mode_cursor_override(
                            &key,
                            accent,
                            Color::from(self.theme.palette.terminal.background),
                        );
                        if let Ok(Some(res)) =
                            self.pane_store.render_pane_parts_with_cursor_override(
                                &key,
                                &viewport,
                                overscan,
                                shared.device(),
                                shared.queue(),
                                &mut atlas,
                                &mut images,
                                &mut scratch,
                                cursor_override,
                            )
                        {
                            if res.program_bg_changed {
                                program_bg_changed_any = true;
                            }
                            for shine in &self.copy_shines {
                                if shine.key.as_ref() == Some(&key) {
                                    emit_copy_shine_effect(shine, shine_color, now, &mut scratch);
                                }
                            }
                            floating_frames.insert(fgeo.id.clone(), scratch.finish());
                        }
                    }
                }
            }

            // Translate `UiDrawList` into exact Z-ordered `gpu_renderer::DrawList` layers while
            // holding only `atlas` (releasing `images` first).
            let hover_img = ui_output.hover_image_slot.zip(self.hover_image_id);
            translate_ui_to_gpu_with_scale(
                &ui_output,
                &floating_frames,
                hover_img,
                self.scale_factor,
                &mut atlas,
                &mut draw_list,
            );
        }

        self.last_ui_output = Some(ui_output);
        self.needs_redraw = program_bg_changed_any;

        Ok(draw_list.finish())
    }

    fn ensure_visible_pane_streams(&mut self, ui_output: &UiRenderOutput) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let mut visible_ids: Vec<String> =
            ui_output.tiled_panes.iter().map(|p| p.id.clone()).collect();
        visible_ids.extend(ui_output.floating_panes.iter().map(|p| p.id.clone()));

        for block_id in visible_ids {
            let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id.clone());
            if !self.opened_streams.contains(&key) && !self.pane_store.contains(&key) {
                self.opened_streams.insert(key.clone());
                self.send_command(HostCommand::OpenBlockStream {
                    host_id: key.host_id.clone(),
                    session_id: key.session_id.clone(),
                    block_id: key.block_id.clone(),
                    ticket: None,
                });
            }
            if self.ports_queried_blocks.insert(key.clone()) {
                self.send_command(HostCommand::ListPorts { key });
            }
        }
    }

    /// Dispatches read-only `HostCommand::ListPorts` for every block in the active window of the
    /// active session (used both live and during `--headless-render` before `hm.shutdown()`),
    /// returning all active-window [`BlockKey`]s.
    pub(super) fn query_active_window_ports(&mut self) -> Vec<BlockKey> {
        let Some(sk) = self.workspace.active_session.clone() else {
            return Vec::new();
        };
        let block_ids: Vec<String> = self
            .workspace
            .session(&sk)
            .and_then(|s| s.view.as_ref())
            .and_then(active_window_of_view)
            .map(|win| {
                win.layers
                    .iter()
                    .flat_map(|l| l.blocks.iter())
                    .filter_map(|b| b.block_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut keys = Vec::with_capacity(block_ids.len());
        for block_id in block_ids {
            let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id);
            if self.ports_queried_blocks.insert(key.clone()) {
                self.send_command(HostCommand::ListPorts { key: key.clone() });
            }
            keys.push(key);
        }
        keys
    }

    fn maybe_queue_pane_resize(
        &mut self,
        key: &BlockKey,
        cols: u16,
        rows: u16,
        metrics: CellMetrics,
        now: Instant,
    ) {
        if self.read_only || cols < 2 || rows < 1 {
            return;
        }
        let local_matches = self
            .pane_store
            .size(key)
            .is_some_and(|(cur_cols, cur_rows)| cur_cols == cols && cur_rows == rows);
        let server_size = self.workspace.block(key).and_then(|b| b.last_server_size);
        let server_diverges = server_size.is_some_and(|(sc, sr)| sc != cols || sr != rows);
        let server_confirmed_matches = server_size.is_some_and(|(sc, sr)| sc == cols && sr == rows);
        if local_matches
            && !server_diverges
            && (!self.force_immediate_resize || server_confirmed_matches)
        {
            return;
        }
        let cell_w_px = (metrics.cell_width * metrics.scale_factor).round().max(1.0) as u32;
        let cell_h_px = (metrics.cell_height * metrics.scale_factor)
            .round()
            .max(1.0) as u32;
        let args = TerminalResizeArgs {
            columns: cols,
            rows,
            cell_width_px: Some(cell_w_px),
            cell_height_px: Some(cell_h_px),
        };
        if self.force_immediate_resize {
            self.workspace.queue_resize_immediate(key, args, now);
        } else {
            self.workspace.queue_resize(key, args, now);
        }
    }

    fn build_ui_frame_input(&self, now: Instant) -> UiFrameInput {
        let mut hosts = Vec::with_capacity(self.workspace.host_order.len());
        let active_sk = self.workspace.active_session.as_ref();

        for host_id in &self.workspace.host_order {
            let Some(host) = self.workspace.hosts.get(host_id) else {
                continue;
            };
            let connection = match &host.status {
                HostStatus::Connected { .. } => HostConnectionState::Online,
                HostStatus::Connecting | HostStatus::Reconnecting { .. } => {
                    HostConnectionState::Reconnecting
                }
                HostStatus::Disconnected | HostStatus::Error(_) => HostConnectionState::Offline,
            };

            let mut sessions = Vec::with_capacity(host.session_order.len());
            for session_id in &host.session_order {
                let Some(sess) = host.sessions.get(session_id) else {
                    continue;
                };
                let is_active_sess = active_sk
                    .is_some_and(|sk| sk.host_id == *host_id && sk.session_id == *session_id);
                let session_muted = self
                    .settings
                    .muted_sessions
                    .contains(&format!("{host_id}/{session_id}"));

                let tabs = if let Some(view) = &sess.view {
                    let block_contexts =
                        self.build_block_card_contexts(host_id, session_id, sess, now);
                    let cards = build_session_tab_cards(view, &block_contexts);
                    view.windows
                        .iter()
                        .zip(cards)
                        .map(|(win, wcard)| {
                            let wid = wcard.window_id.clone().unwrap_or_default();
                            let pane_count = win
                                .layers
                                .iter()
                                .map(|l| l.blocks.len())
                                .sum::<usize>()
                                .max(1);
                            let muted = session_muted
                                || wcard.focused_block_id.as_deref().is_some_and(|bid| {
                                    self.settings.is_muted(host_id, session_id, bid)
                                });
                            let unread = self.unread_windows.contains(&(
                                host_id.clone(),
                                session_id.clone(),
                                wid.clone(),
                            ));
                            SidebarTabView {
                                window_id: wid,
                                card: wcard.card,
                                active: is_active_sess && wcard.is_active,
                                muted,
                                unread,
                                pane_count,
                                icon_override: None,
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                };

                sessions.push(SidebarSessionView {
                    session_id: session_id.clone(),
                    label: sess.label.clone(),
                    active: is_active_sess,
                    collapsed: self
                        .collapsed_sessions
                        .contains(&(host_id.clone(), session_id.clone())),
                    muted: session_muted,
                    tabs,
                });
            }

            hosts.push(SidebarHostView {
                host_id: host.id.clone(),
                label: host.label.clone(),
                is_local: host.is_local,
                connection,
                collapsed: self.collapsed_hosts.contains(&host.id),
                sessions,
            });
        }

        let (active_host_id, active_session_id, active_session_label) = match active_sk {
            Some(sk) => {
                let label = self
                    .workspace
                    .session(sk)
                    .map(|s| s.label.clone())
                    .unwrap_or_default();
                (sk.host_id.clone(), sk.session_id.clone(), label)
            }
            None => (String::new(), String::new(), String::new()),
        };

        let mut split_root: Option<SplitNode<String>> = None;
        let mut zoomed_pane: Option<String> = None;
        let mut floating_layers: Vec<FloatingLayerSpec<String>> = Vec::new();
        let mut pane_metas: Vec<PaneMeta> = Vec::new();

        let hovered_pane_id = self.hovered_pane_card_block();
        let hovered_scrollbar_block = self.hovered_scrollbar_block_id();

        if let Some(sk) = active_sk
            && let Some(sess) = self.workspace.session(sk)
            && let Some(view) = &sess.view
            && let Some(win) = active_window_of_view(view)
        {
            zoomed_pane = win
                .zoomed_block_id
                .clone()
                .or_else(|| win.tiled_zoomed_block_id.clone());

            for layer in &win.layers {
                if layer.kind.eq_ignore_ascii_case("tiled") {
                    if split_root.is_none() {
                        split_root = layer
                            .layout
                            .as_ref()
                            .and_then(layout_node_to_split_node)
                            .or_else(|| {
                                layer
                                    .blocks
                                    .first()
                                    .and_then(|b| b.block_id.clone())
                                    .map(SplitNode::leaf)
                            });
                    }
                } else if layer.kind.eq_ignore_ascii_case("floating")
                    && let Some(primary) = layer.blocks.first().and_then(|b| b.block_id.clone())
                {
                    let minimized = layer
                        .state
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case("minimized"));
                    floating_layers.push(FloatingLayerSpec {
                        id: primary,
                        bounds: [
                            layer.bounds[0] as f32,
                            layer.bounds[1] as f32,
                            layer.bounds[2] as f32,
                            layer.bounds[3] as f32,
                        ],
                        minimized,
                    });
                }

                for blk in &layer.blocks {
                    let Some(block_id) = blk.block_id.as_ref() else {
                        continue;
                    };
                    let key =
                        BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id.clone());
                    let rt = sess.blocks.get(block_id);
                    let status_records: Vec<tr3x_bridge::ProgramStatusRecord> = rt
                        .map(|r| {
                            r.program_statuses
                                .iter()
                                .map(|rec| program_status_from_proto(rec, false))
                                .collect()
                        })
                        .unwrap_or_default();
                    let rolled = rollup_records(&status_records);

                    let provider_hint = rolled
                        .as_ref()
                        .and_then(|r| r.app.as_deref())
                        .or_else(|| rt.and_then(|r| r.foreground_process.as_deref()));
                    let is_pr =
                        layer.kind.eq_ignore_ascii_case("floating") && is_pr_layer(&blk.label);
                    let icon = if is_pr {
                        IconId::Branch
                    } else {
                        IconId::for_provider_or_terminal(provider_hint)
                    };

                    let is_t3_raw_tag =
                        blk.label.trim().starts_with("t3:") || blk.label.trim().starts_with("t3#");
                    let label = rt
                        .and_then(|r| r.title.as_deref())
                        .filter(|t| !t.trim().is_empty())
                        .or_else(|| {
                            rolled
                                .as_ref()
                                .and_then(|r| r.title.as_deref())
                                .filter(|t| !t.trim().is_empty())
                        })
                        .or_else(|| {
                            (!win.label.trim().is_empty()
                                && !is_placeholder_window_label(
                                    &win.label,
                                    win.window_id.as_deref(),
                                )
                                && (is_t3_raw_tag || layer.blocks.len() == 1))
                                .then_some(win.label.as_str())
                        })
                        .or_else(|| {
                            (!blk.label.trim().is_empty() && !is_t3_raw_tag)
                                .then_some(blk.label.as_str())
                        })
                        .or_else(|| rt.and_then(|r| r.foreground_process.as_deref()))
                        .unwrap_or("Terminal")
                        .to_owned();

                    let cwd = rt
                        .and_then(|r| r.pwd.as_deref())
                        .and_then(extract_project_name_from_pwd);

                    let sb = self.pane_store.scrollbar(&key).unwrap_or_default();
                    let scrollbar = ScrollbarMetrics {
                        total_rows: sb.total.min(u64::from(u32::MAX)) as u32,
                        viewport_rows: sb.len.min(u64::from(u32::MAX)) as u32,
                        offset_rows: sb.offset.min(u64::from(u32::MAX)) as u32,
                        visible: sb.total > sb.len && sb.len > 0,
                        hovered: hovered_scrollbar_block.as_deref() == Some(block_id.as_str()),
                    };

                    let focused = self.ui.focus.last_terminal_block() == Some(block_id.as_str())
                        || win.focused_block_id.as_deref() == Some(block_id.as_str())
                        || layer.blocks.len() == 1;

                    let copy_mode_badge = self.copy_mode_badge_for(&key);
                    let listening_ports: Vec<PortBadge> = rt
                        .map(|r| {
                            r.listening_ports
                                .iter()
                                .map(|p| PortBadge {
                                    port: p.port,
                                    label: r.port_process_label(p),
                                    tunneled: self
                                        .active_tunnels
                                        .iter()
                                        .any(|t| t.source_block == key && t.remote_port == p.port),
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    let resume_badge =
                        rt.and_then(|r| r.chosen_resume_record())
                            .map(|rec| ResumeBadge {
                                program: rec.program.clone(),
                                trusted: rec.trusted,
                                rule: rec.rule.clone(),
                                untrusted_reason: rec
                                    .untrusted_reason_label()
                                    .map(ToOwned::to_owned),
                            });

                    pane_metas.push(PaneMeta {
                        block_id: block_id.clone(),
                        label,
                        cwd,
                        icon,
                        focused,
                        hovered: hovered_pane_id.as_deref() == Some(block_id.as_str()),
                        bell_unread: rt.is_some_and(|r| r.has_bell) && !focused,
                        program_status: rolled,
                        resume_badge,
                        osc94_progress: self.block_osc94.get(&key).copied(),
                        scrollbar,
                        program_bg: self
                            .pane_store
                            .program_bg(&key)
                            .map(|rgb| Rgba::new(rgb.r, rgb.g, rgb.b, 255)),
                        has_unread_output: self.pane_store.has_unread_output(&key),
                        copy_mode_badge,
                        listening_ports,
                        is_pr_drawer: is_pr,
                        pr_subtitle: is_pr.then(|| "sup / GitHub PR".to_owned()),
                    });
                }
            }
        }

        let hovered_header_pane = self.hovered_pane_header_block();
        let dnd_overlay = match &self.active_drag {
            Some(ActiveDrag::PaneMove {
                activated: true,
                hover_target: Some((_, hit)),
                ..
            }) => Some(*hit),
            _ => self.active_dnd_hover.and_then(|(px, py)| {
                self.pane_card_rect_at_pointer(px, py)
                    .map(|preview_rect| DropZoneHit {
                        zone: DropZone::CenterSwap,
                        preview_rect,
                    })
            }),
        };

        UiFrameInput {
            viewport_w: self.viewport_w,
            viewport_h: self.viewport_h,
            hosts,
            active_host_id,
            active_session_id,
            active_session_label,
            split_root,
            zoomed_pane,
            hovered_header_pane,
            floating_layers,
            pane_metas,
            dnd_overlay,
            fill_horizontal_tabs: true,
        }
    }

    fn build_block_card_contexts(
        &self,
        host_id: &str,
        session_id: &str,
        sess: &crate::workspace::SessionEntry,
        now: Instant,
    ) -> HashMap<String, BlockCardContext> {
        let now_unix = current_unix_secs();
        let mut map = HashMap::with_capacity(sess.blocks.len());
        for (block_id, rt) in &sess.blocks {
            let key = BlockKey::new(host_id, session_id, block_id);
            let age_secs = self
                .block_activity_at
                .get(&key)
                .map(|t| now.saturating_duration_since(*t).as_secs())
                .or_else(|| {
                    self.block_updated_unix
                        .get(&key)
                        .map(|&ts| now_unix.saturating_sub(ts).max(0) as u64)
                });
            let statuses = rt
                .program_statuses
                .iter()
                .map(|rec| program_status_from_proto(rec, false))
                .collect();
            map.insert(
                block_id.clone(),
                BlockCardContext {
                    program_statuses: statuses,
                    process_name: rt.foreground_process.clone(),
                    project_or_cwd: rt.pwd.as_deref().and_then(extract_project_name_from_pwd),
                    age_secs,
                    title: rt.title.clone(),
                },
            );
        }
        map
    }

    fn hovered_pane_header_block(&self) -> Option<String> {
        let out = self.last_ui_output.as_ref()?;
        let (px, py) = self.pointer_pos;
        for pane in &out.tiled_panes {
            let top_strip = RectF::new(pane.card_rect.x, pane.card_rect.y, pane.card_rect.w, 30.0);
            if top_strip.contains(px, py) {
                return Some(pane.id.clone());
            }
        }
        None
    }

    fn hovered_pane_card_block(&self) -> Option<String> {
        let out = self.last_ui_output.as_ref()?;
        let (px, py) = self.pointer_pos;
        for fpane in out.floating_panes.iter().rev() {
            if fpane.card_rect.contains(px, py) {
                return Some(fpane.id.clone());
            }
        }
        for pane in &out.tiled_panes {
            if pane.card_rect.contains(px, py) {
                return Some(pane.id.clone());
            }
        }
        None
    }

    fn hovered_scrollbar_block_id(&self) -> Option<String> {
        if let Some(ActiveDrag::PaneScrollbar { key, .. }) = &self.active_drag {
            return Some(key.block_id.clone());
        }
        let out = self.last_ui_output.as_ref()?;
        let (px, py) = self.pointer_pos;
        match out.draw_list.hit_test(px, py) {
            Some(
                HitTarget::PaneScrollbarTrack { block_id }
                | HitTarget::PaneScrollbarThumb { block_id },
            ) => Some(block_id.clone()),
            _ => None,
        }
    }

    fn host_display_label(&self, host_id: &str) -> String {
        if host_id == LOCAL_HOST_ID {
            return "This Machine".to_owned();
        }
        if let Some(host) = self.workspace.host(host_id)
            && !host.label.trim().is_empty()
        {
            return host.label.clone();
        }
        if let Some(saved) = self.settings.saved_hosts.iter().find(|h| h.id == host_id) {
            return clean_host_label(&saved.label, &saved.endpoint);
        }
        derive_host_label(host_id)
    }

    fn ensure_host_entry(&mut self, host_id: &str) -> &mut HostEntry {
        if !self.workspace.hosts.contains_key(host_id) {
            if host_id == LOCAL_HOST_ID {
                let entry = HostEntry::new(
                    LOCAL_HOST_ID,
                    "This Machine",
                    RexEndpoint::discover_default(),
                );
                self.workspace.hosts.insert(LOCAL_HOST_ID.to_owned(), entry);
                if !self.workspace.host_order.iter().any(|h| h == LOCAL_HOST_ID) {
                    self.workspace
                        .host_order
                        .insert(0, LOCAL_HOST_ID.to_owned());
                }
            } else {
                let saved = self.settings.saved_hosts.iter().find(|h| h.id == host_id);
                let label = saved
                    .map(|h| clean_host_label(&h.label, &h.endpoint))
                    .unwrap_or_else(|| derive_host_label(host_id));
                let ep = saved
                    .and_then(|h| RexEndpoint::parse(&h.endpoint).ok())
                    .unwrap_or_else(|| RexEndpoint::Tcp {
                        host: host_id.to_owned(),
                        port: 443,
                        tls: true,
                        path_prefix: String::new(),
                    });
                let mut entry = HostEntry::new(host_id.to_owned(), label, ep);
                entry.is_local = false;
                self.workspace.add_host(entry);
            }
        }
        self.workspace
            .hosts
            .get_mut(host_id)
            .expect("host entry just ensured")
    }
}

// ============================================================================
// Pure utility converters
// ============================================================================

fn sync_settings_to_ui(settings: &UserSettings, theme: &Theme, ui: &mut UiTree) {
    ui.layout_cfg.toolbar_position = match settings.sidebar_position {
        SidebarPosition::Left => ToolbarPosition::Left,
        SidebarPosition::Right => ToolbarPosition::Right,
        SidebarPosition::Top => ToolbarPosition::Top,
        SidebarPosition::Bottom => ToolbarPosition::Bottom,
    };
    ui.layout_cfg.toolbar_behavior = match settings.sidebar_behavior {
        SidebarBehavior::Always => ToolbarBehavior::Always,
        SidebarBehavior::OnHover => ToolbarBehavior::OnHover,
        SidebarBehavior::Hidden => ToolbarBehavior::Hidden,
    };
    ui.layout_cfg.sidebar_width = settings.sidebar_width_px;
    ui.layout_cfg.density = match settings.density {
        LayoutDensity::Comfortable => PaneDensity::Comfortable,
        LayoutDensity::Compact => PaneDensity::Compact,
    };
    ui.layout_cfg.pane_header_mode = match settings.pane_header_mode {
        SavedPaneHeaderMode::Always => PaneHeaderMode::Always,
        SavedPaneHeaderMode::OnHover => PaneHeaderMode::OnHover,
        SavedPaneHeaderMode::Hidden => PaneHeaderMode::Hidden,
    };
    ui.layout_cfg.hyprland_rounding = if settings.adaptive_rounding {
        Some(theme.rounding)
    } else {
        None
    };
    ui.pane_options.highlight_active_border = settings.highlight_active_border;
    ui.pane_options.fade_inactive_panes = settings.fade_inactive_panes;
    ui.pane_options.program_color_header = settings.program_color_header;
}

/// Converts an OSC 7 `pwd` (either a raw POSIX path or `file://<host>/path`) into an absolute path.
fn normalize_osc7_pwd(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path_str = if let Some(rest) = trimmed.strip_prefix("file://") {
        let idx = rest.find('/')?;
        &rest[idx..]
    } else {
        trimmed
    };
    let decoded = percent_decode_path(path_str);
    decoded.starts_with('/').then_some(decoded)
}

fn percent_decode_path(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[inline]
fn rectf_to_gpu(r: RectF) -> Rect {
    Rect::new(r.x, r.y, r.w, r.h)
}

fn emit_copy_shine_effect(
    shine: &CopyShineEffect,
    color: Color,
    now: Instant,
    draw_list: &mut DrawList,
) {
    let elapsed = now.saturating_duration_since(shine.started_at);
    if elapsed >= COPY_SHINE_DURATION {
        return;
    }
    let progress = (elapsed.as_secs_f32() / COPY_SHINE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
    let radii = CornerRadii::uniform(3.0);
    let has_clip = shine.clip_rect.max_x > shine.clip_rect.min_x
        && shine.clip_rect.max_y > shine.clip_rect.min_y
        && shine.clip_rect.max_x.is_finite()
        && shine.clip_rect.max_y.is_finite();
    draw_list.with_layer(shine.layer, |dl| {
        if has_clip {
            let clip = Rect::new(
                shine.clip_rect.min_x,
                shine.clip_rect.min_y,
                (shine.clip_rect.max_x - shine.clip_rect.min_x).max(0.0),
                (shine.clip_rect.max_y - shine.clip_rect.min_y).max(0.0),
            );
            dl.with_clip(clip, |dl| {
                for &r in &shine.rects {
                    dl.push_copy_shine(r, radii, color, progress);
                }
            });
        } else {
            for &r in &shine.rects {
                dl.push_copy_shine(r, radii, color, progress);
            }
        }
    });
}

#[inline]
fn compute_grid_cols_rows(content: RectF, metrics: CellMetrics) -> (u16, u16) {
    let cols = (content.w / metrics.cell_width.max(1.0))
        .floor()
        .clamp(2.0, 500.0) as u16;
    let rows = (content.h / metrics.cell_height.max(1.0))
        .floor()
        .clamp(1.0, 300.0) as u16;
    (cols, rows)
}

fn active_window_of_view(view: &SessionView) -> Option<&WindowView> {
    view.active_window_id
        .as_deref()
        .and_then(|aid| {
            view.windows
                .iter()
                .find(|w| w.window_id.as_deref() == Some(aid))
        })
        .or_else(|| view.windows.first())
}

fn primary_block_id_of_window(win: &WindowView) -> Option<&str> {
    win.focused_block_id.as_deref().or_else(|| {
        win.layers
            .iter()
            .find(|l| l.kind.eq_ignore_ascii_case("tiled"))
            .or_else(|| win.layers.first())
            .and_then(|l| l.blocks.first())
            .and_then(|b| b.block_id.as_deref())
    })
}

fn layout_node_to_split_node(node: &LayoutNode) -> Option<SplitNode<String>> {
    if let Some(split) = &node.split {
        let axis = if split.direction.eq_ignore_ascii_case("vertical") {
            SplitAxis::Vertical
        } else {
            SplitAxis::Horizontal
        };
        let first = layout_node_to_split_node(&split.before)?;
        let second = layout_node_to_split_node(&split.after)?;
        Some(SplitNode::split(axis, split.ratio as f32, first, second))
    } else {
        node.block_id.clone().map(SplitNode::leaf)
    }
}

fn first_leaf_block_id(node: &LayoutNode) -> Option<String> {
    if let Some(bid) = &node.block_id {
        return Some(bid.clone());
    }
    let sp = node.split.as_ref()?;
    first_leaf_block_id(&sp.before).or_else(|| first_leaf_block_id(&sp.after))
}

fn sanitize_host_id(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn derive_host_label(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "Remote Host".to_owned();
    }
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("ssh://"))
        .or_else(|| trimmed.strip_prefix("unix://"))
        .or_else(|| trimmed.strip_prefix("remote:"))
        .unwrap_or(trimmed);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or(host_port).trim();
    let cleaned = if let Some(prefix) = host.strip_suffix(".ts.net") {
        // Strip Tailscale tailnet segment (`box.tail000000.ts.net` -> `box`).
        prefix.split('.').next().unwrap_or(prefix)
    } else {
        host
    };
    let final_label = cleaned.trim_matches('-').trim();
    if final_label.is_empty() {
        trimmed.to_owned()
    } else {
        final_label.to_owned()
    }
}

fn clean_host_label(label: &str, endpoint: &str) -> String {
    let trimmed = label.trim();
    if !trimmed.is_empty()
        && !trimmed.starts_with("remote:")
        && !trimmed.starts_with("https://")
        && !trimmed.starts_with("http://")
        && !trimmed.starts_with("ssh://")
        && !trimmed.starts_with("unix://")
    {
        return trimmed.to_owned();
    }
    derive_host_label(endpoint)
}

fn current_process_uid() -> Option<u32> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("Uid:")?;
            rest.split_whitespace().next()?.parse::<u32>().ok()
        })
}

fn floating_zone_cursor(zone: FloatingHitZone, dragging: bool) -> CursorHint {
    match zone {
        FloatingHitZone::MoveHeader => {
            if dragging {
                CursorHint::Grabbing
            } else {
                CursorHint::Pointer
            }
        }
        FloatingHitZone::ResizeE | FloatingHitZone::ResizeW => CursorHint::ColResize,
        FloatingHitZone::ResizeN | FloatingHitZone::ResizeS => CursorHint::RowResize,
        FloatingHitZone::ResizeNW | FloatingHitZone::ResizeSE => CursorHint::NwseResize,
        FloatingHitZone::ResizeNE | FloatingHitZone::ResizeSW => CursorHint::NeswResize,
        FloatingHitZone::Content => CursorHint::Text,
    }
}

fn current_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn set_system_clipboard_text(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_owned());
    }
}

/// Scrubs URL embedded credentials (`scheme://user:pass@host`), sensitive query parameters
/// (`token=`, `api_key=`, `secret=`, `password=`, `auth=`, `key=`, etc.), and `Bearer` /
/// `Authorization` credential strings from diagnostics output (`F20`).
pub(crate) fn redact_diagnostics_text(input: &str) -> String {
    let step1 = scrub_url_userinfo(input);
    let step2 = scrub_sensitive_query_params(&step1);
    scrub_auth_headers_and_bearer(&step2)
}

fn is_url_authority_end(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '/' | '?' | '#' | ')' | ']' | '"' | '\'' | ',' | ';' | '<' | '>'
        )
}

fn is_query_value_end(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '&' | '#' | ')' | ']' | '"' | '\'' | ',' | ';' | '<' | '>'
        )
}

fn is_auth_token_end(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | ';' | ')' | ']' | '<' | '>')
}

fn scrub_url_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(scheme_end) = rest.find("://") {
        let prefix_len = scheme_end + 3;
        out.push_str(&rest[..prefix_len]);
        let after = &rest[prefix_len..];
        let auth_end = after
            .char_indices()
            .find(|&(_, ch)| is_url_authority_end(ch))
            .map_or(after.len(), |(idx, _)| idx);
        let auth = &after[..auth_end];
        if let Some(at_idx) = auth.rfind('@') {
            out.push_str("[REDACTED]@");
            rest = &after[at_idx + 1..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn scrub_sensitive_query_params(s: &str) -> String {
    const SENSITIVE_PARAMS: &[&str] = &[
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "client_secret",
        "password",
        "passwd",
        "secret",
        "token",
        "auth",
        "key",
    ];
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let at_param_boundary = i == 0
            || matches!(
                s.as_bytes()[i - 1],
                b'?' | b'&'
                    | b'#'
                    | b';'
                    | b' '
                    | b'\t'
                    | b'\n'
                    | b'\r'
                    | b'('
                    | b'['
                    | b'"'
                    | b'\''
            )
            || (i >= 2 && s.as_bytes()[i - 2] == b'#' && s.as_bytes()[i - 1] == b'!');
        if at_param_boundary
            && let Some(param) = SENSITIVE_PARAMS.iter().find(|&&p| {
                s.get(i..i + p.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(p))
                    && s.as_bytes().get(i + p.len()) == Some(&b'=')
            })
        {
            let eq_end = i + param.len() + 1;
            out.push_str(&s[i..eq_end]);
            let tail = &s[eq_end..];
            let val_end = tail
                .char_indices()
                .find(|&(_, ch)| is_query_value_end(ch))
                .map_or(tail.len(), |(idx, _)| idx);
            out.push_str("[REDACTED]");
            i = eq_end + val_end;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn prev_char_is_word_char(s: &str, idx: usize) -> bool {
    s[..idx]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

fn starts_with_ascii_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .get(..needle.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(needle))
}

fn is_known_auth_scheme(tok: &str) -> bool {
    const KNOWN_SCHEMES: &[&str] = &[
        "bearer",
        "basic",
        "digest",
        "token",
        "apikey",
        "oauth",
        "negotiate",
        "hoba",
        "mutual",
        "aws4-hmac-sha256",
        "scram-sha-256",
        "dpop",
    ];
    KNOWN_SCHEMES
        .iter()
        .any(|&scheme| tok.eq_ignore_ascii_case(scheme))
}

/// Scans a single auth scheme or credential token starting at `start`.
/// Returns `(end_idx, quote_char, closed_quote)` when a non-empty token or quoted literal is found.
fn scan_auth_credential(s: &str, start: usize) -> Option<(usize, Option<char>, bool)> {
    let first = s.get(start..)?.chars().next()?;
    if first == '"' || first == '\'' {
        let after_open = start + first.len_utf8();
        let tail = &s[after_open..];
        if let Some((off, ch)) = tail
            .char_indices()
            .find(|&(_, c)| c == first || c == '\n' || c == '\r')
        {
            if ch == first {
                return Some((after_open + off + ch.len_utf8(), Some(first), true));
            }
            return Some((after_open + off, Some(first), false));
        }
        return Some((s.len(), Some(first), false));
    }
    let tok_end = s[start..]
        .char_indices()
        .find(|&(_, c)| is_auth_token_end(c))
        .map_or(s.len(), |(off, _)| start + off);
    (tok_end > start).then_some((tok_end, None, false))
}

fn push_redacted_credential(out: &mut String, quote: Option<char>, closed_quote: bool) {
    if let Some(q) = quote {
        out.push(q);
        out.push_str("[REDACTED]");
        if closed_quote {
            out.push(q);
        }
    } else {
        out.push_str("[REDACTED]");
    }
}

fn scrub_auth_headers_and_bearer(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let at_word_start = i == 0 || !prev_char_is_word_char(s, i);
        if at_word_start {
            let hdr_len = if starts_with_ascii_ci(&s[i..], "proxy-authorization:") {
                Some("proxy-authorization:".len())
            } else if starts_with_ascii_ci(&s[i..], "authorization:") {
                Some("authorization:".len())
            } else {
                None
            };
            if let Some(hlen) = hdr_len {
                let mut j = i + hlen;
                out.push_str(&s[i..j]);
                let ws1_end = s[j..]
                    .char_indices()
                    .find(|&(_, c)| c != ' ' && c != '\t')
                    .map_or(s.len(), |(off, _)| j + off);
                out.push_str(&s[j..ws1_end]);
                j = ws1_end;

                if let Some((tok1_end, q1, closed1)) = scan_auth_credential(s, j) {
                    if q1.is_none() {
                        let tok1 = &s[j..tok1_end];
                        let ws2_end = s[tok1_end..]
                            .char_indices()
                            .find(|&(_, c)| c != ' ' && c != '\t')
                            .map_or(s.len(), |(off, _)| tok1_end + off);
                        if is_known_auth_scheme(tok1)
                            && ws2_end > tok1_end
                            && let Some((tok2_end, q2, closed2)) = scan_auth_credential(s, ws2_end)
                        {
                            out.push_str(tok1);
                            out.push_str(&s[tok1_end..ws2_end]);
                            push_redacted_credential(&mut out, q2, closed2);
                            i = tok2_end;
                            continue;
                        }
                    }
                    push_redacted_credential(&mut out, q1, closed1);
                    i = tok1_end;
                    continue;
                }
                i = j;
                continue;
            }

            if starts_with_ascii_ci(&s[i..], "bearer")
                && matches!(s.as_bytes().get(i + 6), Some(b' ' | b'\t'))
            {
                let mut j = i + 6;
                out.push_str(&s[i..j]);
                let ws_end = s[j..]
                    .char_indices()
                    .find(|&(_, c)| c != ' ' && c != '\t')
                    .map_or(s.len(), |(off, _)| j + off);
                out.push_str(&s[j..ws_end]);
                j = ws_end;
                if let Some((tok_end, q, closed)) = scan_auth_credential(s, j) {
                    push_redacted_credential(&mut out, q, closed);
                    i = tok_end;
                    continue;
                }
                i = j;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}
