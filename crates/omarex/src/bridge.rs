//! Window-independent `omarex` application controller bridging `HostManager` (Tokio / Rex rev 24),
//! `WorkspaceState`, `PaneStore` (`libghostty-vt`), `tr3x-bridge`, `UiTree` (`ui-chrome`),
//! `omarchy-theme`, and `GpuRenderer` (`wgpu` Vulkan 1.4).
//!
//! Designed so `app.rs` (`winit`) is a thin platform event-loop shell forwarding keyboard, pointer,
//! IME, clipboard, file-drop, and `RuntimeEvent` notifications directly into [`OmarexController`],
//! and so headless offscreen PNG renders execute the exact same frame pipeline without a display.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use gpu_renderer::{
    CellMetrics, ClipRect, Color, CornerRadii, DrawList, FontFamily, FontStyle,
    FontWeight as GpuFontWeight, FrameStats, GlyphAtlas, GpuConfig, GpuRenderer, ImageQuad,
    ImageUpload, Rect, SceneFrame, TextAlign as GpuTextAlign, TextRun, layers,
};
use omarchy_theme::{NotificationRequest, NotificationTarget, NotificationUrgency, Rgba, Theme};
use rex_proto::types::{
    BlockCreateSpec, ClientActionRequest, ClientReplyRequest, InitialWindowSpec, LayoutCreateSpec,
    LayoutNode, OwnMode, ServerEvent, SessionCreateRequest, SessionEvent, SessionFloatBlockRequest,
    SessionNewSplitRequest, SessionNewWindowRequest, SessionResizeLayerRequest,
    SessionResizePaneRequest, SessionSetLayerStateRequest, SessionSummary, SessionView,
    SessionZoomBlockRequest, TerminalResizeArgs, WindowView,
};
use rex_proto::{DownloadResult, RexEndpoint, UploadBatch};
use tr3x_bridge::{
    ActionKind, ActionRegistry, BlockCardContext, ClientCommand, KeyChord, KeyResolution,
    KeymapOptions, Modifiers, PaneDirection, PromptManager, ProtoPromptOutcome, SplitDirection,
    apply_proto_keymap, build_client_key_request, build_proto_keymap_request,
    build_session_tab_cards, dispatch_proto_queued_actions, extract_project_name_from_pwd,
    handle_proto_perform_request, is_placeholder_window_label, is_pr_layer, is_t3_session,
    keymap_changed_needs_refresh, on_proto_prompt_ended, on_proto_prompt_requested,
    parse_client_command, plan_proto_remote_call, program_status_from_proto, proxy_reply_error,
    reply_to_proto, rollup_records,
};
use ui_chrome::{
    ChromeLayoutConfig, DividerGeometry, DropZone, DropZoneHit, FloatingHitZone, FloatingLayerSpec,
    FocusTarget, FontKind, FontWeight as UiFontWeight, HitTarget, HostConnectionState,
    HoverPreviewState, IconId, ModalContent, ModalKey, ModalKind, ModalState, Osc94Progress,
    Osc94State, PaletteItem, PaneDensity, PaneHeaderButton, PaneHeaderMode, PaneMeta, RectF,
    ScrollbarMetrics, SettingChange, SettingsViewSnapshot, SidebarHostView, SidebarSessionView,
    SidebarTabView, SplitAxis, SplitNode, TextAlign as UiTextAlign, ToastItem, ToastSeverity,
    ToolbarBehavior, ToolbarPosition, UiAction, UiFrameInput, UiPrimitive, UiRenderOutput, UiTree,
    WhichKeyState, compute_drag_ratio, detect_at_char_col, detect_url_at_char_col,
};
use vt_engine::{
    KeyAction as VtKeyAction, KeyEventInput as VtKeyEventInput, Modifiers as VtModifiers,
    MouseAction, MouseButton, MouseEventInput, MouseGeometry, Overscan, PhysicalKey, ProgressState,
    ScrollViewport, SelectionGeometry, TerminalEvent,
};

use crate::config::{
    LayoutDensity, SavedHost, SavedPaneHeaderMode, SidebarBehavior, SidebarPosition, UserSettings,
    WindowMaterial,
};
use crate::host_manager::{
    DataStreamEvent, DataStreamEventKind, HostCommand, HostManager, HostManagerConfig,
    LOCAL_HOST_ID, RuntimeEvent,
};
use crate::pane_store::{DEFAULT_BACKGROUND_SNAPSHOT_BUDGET, PaneStore, build_terminal_viewport};
use crate::workspace::{
    BlockKey, HostEntry, HostStatus, OwnershipDecision, OwnershipTrigger, SessionKey,
    WorkspaceState, theme_palette_to_rex_args,
};

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

/// Options for [`render_headless_live_png_with_options`].
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessRenderOptions {
    pub server_override: Option<String>,
    pub session_filter: Option<String>,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub force_light: bool,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum HoveredTerminalTarget {
    Url(String),
    LocalFile {
        resolved_path: PathBuf,
        display_path: String,
        line: Option<u32>,
        column: Option<u32>,
    },
    RemoteFile {
        remote_path: String,
        line: Option<u32>,
        column: Option<u32>,
    },
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
    hover_image_id: Option<u64>,
    pending_hover_image: Option<(PathBuf, u64)>,
    loaded_hover_image: Option<(PathBuf, u64)>,
    hover_image_gen: u64,
    active_dnd_hover: Option<(f32, f32)>,
    downloaded_file_cache: HashMap<(String, String), (PathBuf, Option<String>)>,
    active_prompt_host: Option<String>,
    toast_deadlines: HashMap<u64, Instant>,
    next_toast_id: u64,
    next_transfer_id: u64,
    next_kitty_anim_ms: Option<u64>,
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
            read_only: false,
            clipboard_enabled: true,
            viewport_w: 1440.0,
            viewport_h: 900.0,
            scale_factor: 1.0,
            started_at: Instant::now(),
            last_ui_output: None,
            last_cell_metrics: None,
            pointer_pos: (0.0, 0.0),
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
            hover_image_id: None,
            pending_hover_image: None,
            loaded_hover_image: None,
            hover_image_gen: 0,
            active_dnd_hover: None,
            downloaded_file_cache: HashMap::new(),
            active_prompt_host: None,
            toast_deadlines: HashMap::new(),
            next_toast_id: 1,
            next_transfer_id: 1,
            next_kitty_anim_ms: None,
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

    fn next_terminal_session_label(
        &self,
        host_id: &str,
        cwd: Option<&str>,
        prefer_cwd_basename: bool,
    ) -> String {
        let used: HashSet<String> = self
            .workspace
            .host(host_id)
            .map(|h| {
                h.sessions
                    .values()
                    .map(|s| s.label.to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default();

        if prefer_cwd_basename && let Some(dir) = cwd {
            let home = std::env::var("HOME").ok();
            let is_home = home
                .as_deref()
                .is_some_and(|h| Path::new(dir) == Path::new(h));
            if !is_home
                && let Some(base) = Path::new(dir)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !is_t3_session(s))
                && !used.contains(&base.to_ascii_lowercase())
            {
                return base.to_owned();
            }
        }

        if !used.contains("terminal") {
            return "Terminal".to_owned();
        }
        for n in 2..=999 {
            let cand = format!("Terminal {n}");
            if !used.contains(&cand.to_ascii_lowercase()) {
                return cand;
            }
        }
        "Terminal".to_owned()
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
            let label = self.next_terminal_session_label(&target_host, cwd.as_deref(), false);
            self.send_command(HostCommand::NewSession {
                host_id: target_host,
                request: SessionCreateRequest {
                    label: Some(label),
                    initial_windows: Some(vec![InitialWindowSpec {
                        window_label: None,
                        layout,
                    }]),
                },
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
            });
            return;
        }

        // Otherwise (inside `T3 Code` or no active session), target an existing non-T3 session
        // on `target_host` if one exists, or create a brand-new `"Terminal"` Rex session!
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
            });
        } else {
            let label = self.next_terminal_session_label(&target_host, cwd.as_deref(), false);
            self.send_command(HostCommand::NewSession {
                host_id: target_host,
                request: SessionCreateRequest {
                    label: Some(label),
                    initial_windows: Some(vec![InitialWindowSpec {
                        window_label: None,
                        layout,
                    }]),
                },
            });
        }
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
        if self.needs_redraw || self.pane_store.has_pending_snapshots() {
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
        if self.window_focused && self.has_visible_working_indicator() {
            consider(now + Duration::from_millis(120));
        }
        for &deadline in self.toast_deadlines.values() {
            consider(deadline);
        }
        earliest
    }

    /// Returns the appropriate native Wayland cursor icon for the current drag or hovered hit target.
    pub fn cursor_hint(&self) -> CursorHint {
        if let Some(drag) = &self.active_drag {
            return match drag {
                ActiveDrag::SidebarResize { .. } => CursorHint::ColResize,
                ActiveDrag::SplitDivider { divider, .. } => match divider.axis {
                    SplitAxis::Horizontal => CursorHint::ColResize,
                    SplitAxis::Vertical => CursorHint::RowResize,
                },
                ActiveDrag::FloatingLayer { zone, .. } => floating_zone_cursor(*zone, true),
                ActiveDrag::TerminalSelection { .. } => CursorHint::Text,
                ActiveDrag::PaneScrollbar { .. } => CursorHint::Pointer,
            };
        }
        let Some(out) = &self.last_ui_output else {
            return CursorHint::Default;
        };
        let (px, py) = self.pointer_pos;
        match out.draw_list.hit_test(px, py) {
            Some(HitTarget::SidebarResizeHandle) => CursorHint::ColResize,
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
            Some(
                HitTarget::ModalInput | HitTarget::FindBarInput | HitTarget::SidebarFilterInput,
            ) => CursorHint::Text,
            Some(HitTarget::PaneScrollbarTrack { .. } | HitTarget::PaneScrollbarThumb { .. }) => {
                CursorHint::Pointer
            }
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

    fn send_command(&mut self, cmd: HostCommand) {
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
            // Instead, ensure our focused window owns the block (`OwnMode::Take`) so Rex applies
            // `com.superlogical.terminal.resize` immediately and streams back `RexApc::Resize` at
            // the exact PTY byte boundary.
            if self.window_focused {
                self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
            }
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

        // 5. Auto-dismiss expired toasts so transient notices never accumulate permanently.
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

        self.needs_redraw()
    }

    // =========================================================================
    // 1. Full Composite Frame Pipeline (`UiTree` + `PaneStore` -> `SceneFrame`)
    // =========================================================================

    /// Builds a complete composite GPU [`SceneFrame`] combining `ui-chrome` (sidebar/tab bar,
    /// inset pane cards, headers, scrollbars, floating `/pr` drawer, modals, toasts) with
    /// `PaneStore`'s live `RowId`-cached terminals and Kitty Graphics Protocol image layers.
    pub fn render_frame(&mut self, renderer: &mut GpuRenderer, now: Instant) -> Result<SceneFrame> {
        let _ = self.tick(now);

        // Ensure font family in atlas matches settings.
        if !self.settings.font_family.is_empty()
            && renderer.atlas().mono_family_name() != self.settings.font_family
        {
            renderer
                .atlas_mut()
                .set_mono_family(self.settings.font_family.clone());
        }

        let font_px = self.font_size_px();
        let metrics = renderer
            .atlas_mut()
            .measure_monospace_cell(font_px, self.scale_factor);
        self.last_cell_metrics = Some(metrics);

        // Build `UiFrameInput` from live multi-host workspace + `tr3x` tab cards.
        let frame_input = self.build_ui_frame_input(now);

        // Update T3 session hint on pane options & keep active sidebar tab cleanly in view.
        self.ui.pane_options.is_t3_session = is_t3_session(&frame_input.active_session_label);
        self.ui.sync_sidebar_scroll(&frame_input);

        let anim_phase = now.saturating_duration_since(self.started_at).as_secs_f32();
        let ui_output = self.ui.render(&frame_input, &self.theme, now, anim_phase);

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
        if let Some(sk) = &active_sk {
            for geo in &ui_output.tiled_panes {
                let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), geo.id.clone());
                let content_rect = geo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                let (cols, rows) = compute_grid_cols_rows(content_rect, metrics);
                self.last_pane_grids.insert(key.clone(), (cols, rows));
                self.maybe_queue_pane_resize(&key, cols, rows, metrics, now);
            }
            for fgeo in &ui_output.floating_panes {
                let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), fgeo.id.clone());
                let content_rect = fgeo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                let (cols, rows) = compute_grid_cols_rows(content_rect, metrics);
                self.last_pane_grids.insert(key.clone(), (cols, rows));
                self.maybe_queue_pane_resize(&key, cols, rows, metrics, now);
            }
        }
        self.force_immediate_resize = false;
        self.flush_ready_resizes(now);

        // Render tiled panes into layers 10..50 (above CANVAS=0 pane card fill, below CHROME=60).
        if let Some(sk) = &active_sk {
            for geo in &ui_output.tiled_panes {
                let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), geo.id.clone());
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

                if let Err(err) =
                    self.pane_store
                        .render_pane(&key, &viewport, overscan, renderer, &mut draw_list)
                {
                    tracing::warn!(?err, block = %geo.id, "failed to render tiled pane");
                }
            }
        }

        // Pre-render floating pane terminals into scratch DrawLists so we can splice them at the
        // exact Z-layer above their floating card backgrounds and below floating headers & modals!
        let mut floating_frames: HashMap<String, SceneFrame> = HashMap::new();
        if let Some(sk) = &active_sk {
            for fgeo in &ui_output.floating_panes {
                let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), fgeo.id.clone());
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
                if let Ok(Some(_)) =
                    self.pane_store
                        .render_pane(&key, &viewport, overscan, renderer, &mut scratch)
                {
                    floating_frames.insert(fgeo.id.clone(), scratch.finish());
                }
            }
        }

        // Upload any pending local image hover thumbnail directly into `GpuRenderer` before
        // translating `UiDrawList` so the preview card renders immediately in this same frame.
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
                    .set_hover_preview_image(
                        renderer,
                        HOVER_PREVIEW_IMAGE_GPU_ID,
                        generation,
                        &bytes,
                    )
                    .is_ok()
            {
                self.loaded_hover_image = Some((img_path, generation));
            }
        }

        // Translate `UiDrawList` into exact Z-ordered `gpu_renderer::DrawList` layers,
        // placing tiled pane card fills on CANVAS (0), chrome/headers/sidebar/overlays in exact
        // painter's order on CHROME+ (60..), and splicing floating pane terminals cleanly!
        let hover_img = ui_output.hover_image_slot.zip(self.hover_image_id);
        translate_ui_to_gpu(
            &ui_output,
            &floating_frames,
            hover_img,
            renderer.atlas_mut(),
            &mut draw_list,
        );

        self.last_ui_output = Some(ui_output);
        self.needs_redraw = false;

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
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                    ticket: None,
                });
            }
        }
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

                    pane_metas.push(PaneMeta {
                        block_id: block_id.clone(),
                        label,
                        cwd,
                        icon,
                        focused,
                        hovered: hovered_pane_id.as_deref() == Some(block_id.as_str()),
                        bell_unread: rt.is_some_and(|r| r.has_bell) && !focused,
                        program_status: rolled,
                        osc94_progress: self.block_osc94.get(&key).copied(),
                        scrollbar,
                        program_bg: None,
                        is_pr_drawer: is_pr,
                        pr_subtitle: is_pr.then(|| "sup / GitHub PR".to_owned()),
                    });
                }
            }
        }

        let hovered_header_pane = self.hovered_pane_header_block();
        let dnd_overlay = self.active_dnd_hover.and_then(|(px, py)| {
            self.pane_card_rect_at_pointer(px, py)
                .map(|preview_rect| DropZoneHit {
                    zone: DropZone::CenterSwap,
                    preview_rect,
                })
        });

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

    // =========================================================================
    // 2. RuntimeEvent Reducer (`HostManager` -> Controller)
    // =========================================================================

    /// Processes an asynchronous [`RuntimeEvent`] emitted by [`HostManager`].
    pub fn process_runtime_event(&mut self, event: RuntimeEvent, now: Instant) {
        if !matches!(
            event,
            RuntimeEvent::DataStream(_)
                | RuntimeEvent::ClientsListed { .. }
                | RuntimeEvent::HostsListUpdated { .. }
                | RuntimeEvent::TransferProgress { .. }
        ) {
            self.needs_redraw = true;
        }
        match event {
            RuntimeEvent::HostConnected {
                host_id,
                client_id,
                resumed,
                api_revision,
            } => {
                let host = self.ensure_host_entry(&host_id);
                let prev_server_host = host.server_host_id().map(str::to_owned);
                host.status = HostStatus::Connected {
                    client_id,
                    server_host_id: prev_server_host,
                    api_revision,
                    resumed,
                };
            }

            RuntimeEvent::HostDisconnected {
                host_id,
                will_reconnect,
                reason,
            } => {
                let host_label = self.host_display_label(&host_id);
                if let Some(host) = self.workspace.host_mut(&host_id) {
                    host.status = if will_reconnect {
                        HostStatus::Reconnecting {
                            reason: reason.clone(),
                        }
                    } else {
                        HostStatus::Disconnected
                    };
                }
                if !will_reconnect {
                    self.push_toast(
                        ToastSeverity::Warning,
                        format!("Disconnected from {host_label}"),
                        reason,
                    );
                }
            }

            RuntimeEvent::HostError {
                host_id,
                context,
                message,
            } => {
                tracing::warn!(host = %host_id, %context, %message, "host runtime error");
                let host_label = self.host_display_label(&host_id);
                if context == "com.superlogical.terminal.resize" {
                    self.workspace.invalidate_host_sent_resizes(&host_id);
                }
                if let Some(host) = self.workspace.host_mut(&host_id)
                    && matches!(
                        host.status,
                        HostStatus::Disconnected | HostStatus::Connecting
                    )
                {
                    host.status = HostStatus::Error(message.clone());
                }
                self.push_toast(
                    ToastSeverity::Error,
                    format!("{host_label}: {context}"),
                    message,
                );
            }

            RuntimeEvent::ClientsListed { host_id, clients } => {
                let my_client_id = self
                    .workspace
                    .host(&host_id)
                    .and_then(HostEntry::client_id)
                    .map(str::to_owned);
                let mut attached = HashSet::new();
                for client in clients.clients {
                    if my_client_id.as_deref() == Some(client.client_id.as_str()) {
                        continue;
                    }
                    // Track other GUI app windows (ignore headless CLI/daemon/sync helpers).
                    let is_app = client
                        .info
                        .as_ref()
                        .and_then(|i| i.kind.as_deref())
                        .is_none_or(|k| k.eq_ignore_ascii_case("app"));
                    if is_app {
                        for sid in client.session_ids {
                            attached.insert(sid);
                        }
                    }
                }
                self.other_app_attached_sessions.insert(host_id, attached);
            }

            RuntimeEvent::BootstrapReady {
                host_id, bootstrap, ..
            } => {
                let host = self.ensure_host_entry(&host_id);

                let api_rev = match &host.status {
                    HostStatus::Connected { api_revision, .. } => *api_revision,
                    _ => rex_proto::TARGET_API_REVISION,
                };

                // Preserve pre-issued data tickets before consuming `bootstrap`.
                let mut tickets: HashMap<(String, String), String> = HashMap::new();
                for grant in &bootstrap.data_tickets {
                    tickets.insert(
                        (grant.session_id.clone(), grant.block_id.clone()),
                        grant.ticket.clone(),
                    );
                }

                let selected_id = bootstrap
                    .selected_session
                    .as_ref()
                    .map(|s| s.session.session_id.clone());
                let updated_session_ids: Vec<String> = bootstrap
                    .selected_session
                    .iter()
                    .chain(bootstrap.attached_sessions.iter())
                    .map(|s| s.session.session_id.clone())
                    .collect();
                host.apply_bootstrap(*bootstrap, api_rev);
                for sid in updated_session_ids {
                    self.prune_stale_session_blocks(&SessionKey::new(host_id.clone(), sid));
                }

                // Standalone `-e <cmd>` mode: spawn an ephemeral Rex session for the command
                // instead of attaching to the user's workspace sessions.
                if let Some(spec) = self.command_launch.clone()
                    && host_id == LOCAL_HOST_ID
                {
                    if !self.ephemeral_spawn_requested {
                        self.ephemeral_spawn_requested = true;
                        let label = spec
                            .command
                            .first()
                            .and_then(|s| Path::new(s).file_name())
                            .and_then(|s| s.to_str())
                            .map(|prog| format!("omarex-run-{prog}"))
                            .unwrap_or_else(|| "omarex-run".to_owned());
                        let layout = self
                            .build_shell_layout_spec(Some(&spec.command), spec.wait_after_command);
                        self.send_command(HostCommand::CreateCommandSession {
                            host_id: host_id.clone(),
                            request: SessionCreateRequest {
                                label: Some(label),
                                initial_windows: Some(vec![InitialWindowSpec {
                                    window_label: None,
                                    layout,
                                }]),
                            },
                        });
                    }
                } else if self.workspace.active_session.is_none() {
                    // Explicit `--session <name>` filter takes top priority (preserves CLI/headless).
                    if self.preferred_session_filter.is_some() {
                        if let Some(chosen_sess) =
                            self.pick_initial_session_id(&host_id, selected_id.as_deref())
                        {
                            let sk = SessionKey::new(host_id.clone(), chosen_sess);
                            self.activate_session(sk.clone(), &tickets);
                            self.maybe_hydrate_session_metadata(&sk);
                        }
                    } else if let Some(launch_cwd) = self.default_working_dir.clone()
                        && host_id == LOCAL_HOST_ID
                        && !self.read_only
                    {
                        // `Super+Return` / `xdg-terminal-exec` (`--working-directory=<dir>`):
                        // NEVER hijack focus into `T3 Code` or fight over a session held by another
                        // live tiled `omarex` window! Reuse an unattached normal session if launched
                        // at `$HOME`, otherwise spawn a fresh interactive Ghostty shell in `<dir>`.
                        let home_dir = std::env::var("HOME").ok();
                        let is_home_cwd = home_dir
                            .as_deref()
                            .is_some_and(|h| Path::new(&launch_cwd) == Path::new(h));
                        let other_attached = self.other_app_attached_sessions.get(&host_id);
                        let reusable = if is_home_cwd {
                            self.find_preferred_terminal_session(&host_id, other_attached)
                        } else {
                            None
                        };

                        if let Some(chosen_sess) = reusable {
                            let sk = SessionKey::new(host_id.clone(), chosen_sess);
                            self.activate_session(sk.clone(), &tickets);
                            self.maybe_hydrate_session_metadata(&sk);
                        } else {
                            let label =
                                self.next_terminal_session_label(&host_id, Some(&launch_cwd), true);
                            let layout = self.build_shell_layout_spec_with_cwd(
                                Some(&launch_cwd),
                                None,
                                false,
                            );
                            self.send_command(HostCommand::NewSession {
                                host_id: host_id.clone(),
                                request: SessionCreateRequest {
                                    label: Some(label),
                                    initial_windows: Some(vec![InitialWindowSpec {
                                        window_label: None,
                                        layout,
                                    }]),
                                },
                            });
                        }
                    } else if let Some(chosen_sess) =
                        self.pick_initial_session_id(&host_id, selected_id.as_deref())
                    {
                        let sk = SessionKey::new(host_id.clone(), chosen_sess);
                        self.activate_session(sk.clone(), &tickets);
                        self.maybe_hydrate_session_metadata(&sk);
                    } else if host_id == LOCAL_HOST_ID && !self.read_only {
                        // Empty local server: spin up an initial `"Terminal"` shell session.
                        let label = self.next_terminal_session_label(&host_id, None, false);
                        let layout = self.build_shell_layout_spec(None, false);
                        self.send_command(HostCommand::NewSession {
                            host_id: host_id.clone(),
                            request: SessionCreateRequest {
                                label: Some(label),
                                initial_windows: Some(vec![InitialWindowSpec {
                                    window_label: None,
                                    layout,
                                }]),
                            },
                        });
                    }
                }
            }

            RuntimeEvent::SessionCreatedAndAttached {
                host_id,
                session_id,
                view,
                ..
            } => {
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                let focused_block = active_window_of_view(&view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);
                let label = view.label.clone();
                if let Some(host) = self.workspace.host_mut(&host_id) {
                    host.on_session_created(SessionSummary {
                        session_id: session_id.clone(),
                        label,
                    });
                }
                if let Some(sess) = self.workspace.session_mut(&sk) {
                    sess.apply_view(*view);
                }
                self.prune_stale_session_blocks(&sk);
                self.collapsed_hosts.remove(&host_id);
                self.set_session_collapsed(&host_id, &session_id, false);
                self.activate_session(sk.clone(), &HashMap::new());
                self.ui.focus.focus_terminal(focused_block.clone());
                self.force_immediate_resize = true;
                self.open_active_window_streams(&sk, &HashMap::new());
                self.maybe_hydrate_session_metadata(&sk);
                if let Some(bid) = focused_block {
                    let key = BlockKey::new(host_id, session_id, bid);
                    self.workspace.invalidate_sent_resize(&key);
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
            }

            RuntimeEvent::WindowCreated {
                host_id,
                session_id,
                window_id,
                block_ids,
                view,
                ..
            } => {
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                self.collapsed_hosts.remove(&host_id);
                self.set_session_collapsed(&host_id, &session_id, false);

                let focused_from_view = if let Some(v) = view {
                    let primary = active_window_of_view(&v)
                        .and_then(primary_block_id_of_window)
                        .map(str::to_owned);
                    if let Some(sess) = self.workspace.session_mut(&sk) {
                        sess.apply_view(*v);
                    }
                    self.prune_stale_session_blocks(&sk);
                    primary
                } else {
                    if let Some(sess) = self.workspace.session_mut(&sk)
                        && let Some(v) = &mut sess.view
                    {
                        v.active_window_id = Some(window_id);
                    }
                    self.send_command(HostCommand::FetchView {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                    });
                    None
                };

                if self.workspace.active_session.as_ref() != Some(&sk) {
                    self.activate_session(sk.clone(), &HashMap::new());
                }
                let target_block = block_ids.first().cloned().or(focused_from_view);
                if target_block.is_some() {
                    self.ui.focus.focus_terminal(target_block.clone());
                }
                self.force_immediate_resize = true;
                self.open_active_window_streams(&sk, &HashMap::new());
                for bid in &block_ids {
                    let key = BlockKey::new(host_id.clone(), session_id.clone(), bid.clone());
                    if self.opened_streams.insert(key.clone()) {
                        self.send_command(HostCommand::OpenBlockStream {
                            host_id: key.host_id.clone(),
                            session_id: key.session_id.clone(),
                            block_id: key.block_id.clone(),
                            ticket: None,
                        });
                    }
                    self.workspace.invalidate_sent_resize(&key);
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
                if block_ids.is_empty()
                    && let Some(bid) = target_block
                {
                    let key = BlockKey::new(host_id, session_id, bid);
                    self.workspace.invalidate_sent_resize(&key);
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
                self.maybe_hydrate_session_metadata(&sk);
            }

            RuntimeEvent::CommandSessionReady {
                host_id,
                session_id,
                view,
            } => {
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                self.ephemeral_session = Some(sk.clone());
                let focused_block = active_window_of_view(&view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);
                let label = view.label.clone();
                if let Some(host) = self.workspace.host_mut(&host_id) {
                    host.on_session_created(SessionSummary {
                        session_id: session_id.clone(),
                        label,
                    });
                }
                if let Some(sess) = self.workspace.session_mut(&sk) {
                    sess.apply_view(*view);
                }
                self.prune_stale_session_blocks(&sk);
                self.activate_session(sk.clone(), &HashMap::new());
                self.ui.on_session_view_changed(focused_block.clone());
                self.open_active_window_streams(&sk, &HashMap::new());
                if let Some(bid) = focused_block {
                    self.maybe_claim_ownership(
                        &BlockKey::new(host_id, session_id, bid),
                        OwnershipTrigger::ExplicitInteractOrFit,
                    );
                }
            }

            RuntimeEvent::SessionViewUpdated {
                host_id,
                session_id,
                view,
                ..
            } => {
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                let focused_block = active_window_of_view(&view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);

                let needs_catchup = self
                    .workspace
                    .session_mut(&sk)
                    .is_some_and(|s| s.apply_view(*view));
                self.prune_stale_session_blocks(&sk);
                if needs_catchup {
                    self.send_command(HostCommand::FetchView {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                    });
                }

                // Batch-hydrate initial OSC 7501 `program_status` for any newly discovered blocks.
                self.maybe_hydrate_session_metadata(&sk);

                // Update FocusGuard strictly WITHOUT stealing focus from any active modal/overlay!
                if self.workspace.active_session.as_ref() == Some(&sk) {
                    self.ui.on_session_view_changed(focused_block.clone());
                    self.open_active_window_streams(&sk, &HashMap::new());
                    if let Some(bid) = focused_block {
                        self.maybe_claim_ownership(
                            &BlockKey::new(host_id, session_id, bid),
                            OwnershipTrigger::AttachOrView,
                        );
                    }
                }
            }

            RuntimeEvent::SessionViewFetchFailed {
                host_id,
                session_id,
                session_not_found,
            } => {
                self.workspace
                    .clear_view_fetch_in_flight(&host_id, &session_id);
                if session_not_found {
                    self.cleanup_removed_session(&host_id, &session_id);
                }
            }

            RuntimeEvent::SessionMetadataHydrated {
                host_id,
                session_id,
                blocks,
            } => {
                for meta in &blocks {
                    if let Some(latest_unix) = meta
                        .program_statuses
                        .iter()
                        .filter_map(|r| parse_rfc3339_unix_secs(&r.updated_at))
                        .max()
                    {
                        self.block_updated_unix.insert(
                            BlockKey::new(&host_id, &session_id, &meta.block_id),
                            latest_unix,
                        );
                    }
                }
                let updated = self
                    .workspace
                    .apply_hydrated_metadata(&host_id, &session_id, blocks);
                self.metadata_hydrated_count = self.metadata_hydrated_count.saturating_add(updated);
            }

            RuntimeEvent::KeymapUpdated { keymap, .. } => {
                apply_proto_keymap(&mut self.actions, &keymap);
                self.refresh_which_key_overlay();
            }

            RuntimeEvent::HostsListUpdated { .. } => {}

            RuntimeEvent::ControlEvent { host_id, event, .. } => {
                self.handle_control_event(&host_id, *event, now);
            }

            RuntimeEvent::PromptRequested {
                host_id, request, ..
            } => match on_proto_prompt_requested(&mut self.prompts, &request, now) {
                ProtoPromptOutcome::Opened => {
                    if let Some(session) = self.prompts.active().cloned() {
                        self.active_prompt_host = Some(host_id);
                        self.ui.open_rex_prompt(session);
                    }
                }
                ProtoPromptOutcome::ImmediateReply(reply) => {
                    self.send_command(HostCommand::ClientReply {
                        host_id,
                        request: reply,
                    });
                }
            },

            RuntimeEvent::PromptEnded { event, .. } => {
                on_proto_prompt_ended(&mut self.prompts, &event);
                if self.ui.dismiss_rex_prompt_if_matches(&event.call_id) {
                    self.active_prompt_host = None;
                }
            }

            RuntimeEvent::PerformRequested {
                host_id, request, ..
            } => {
                let local_uid = current_process_uid();
                let (cmd_opt, reply) = handle_proto_perform_request(&request, local_uid);
                if let Some(cmd) = cmd_opt {
                    self.execute_client_command(&host_id, cmd, now);
                }
                self.send_command(HostCommand::ClientReply {
                    host_id,
                    request: reply,
                });
            }

            RuntimeEvent::CallRequested {
                host_id, request, ..
            } => {
                let connected: Vec<String> = self
                    .workspace
                    .hosts
                    .values()
                    .filter(|h| matches!(h.status, HostStatus::Connected { .. }))
                    .map(|h| h.id.clone())
                    .collect();
                let refs: Vec<&str> = connected.iter().map(String::as_str).collect();
                let reply = match plan_proto_remote_call(&request, &refs) {
                    Ok(plan) => proxy_reply_error(
                        &plan,
                        "not_implemented",
                        "remote multi-server proxy dispatch is not available in this session",
                    ),
                    Err(immediate_err) => immediate_err,
                };
                self.send_command(HostCommand::ClientReply {
                    host_id,
                    request: reply,
                });
            }

            RuntimeEvent::ClientActionCompleted {
                host_id, response, ..
            } => {
                let queued = dispatch_proto_queued_actions(&response);
                for cmd in queued {
                    self.execute_client_command(&host_id, cmd, now);
                }
            }

            RuntimeEvent::DataStream(stream_ev) => {
                self.handle_data_stream_event(stream_ev, now);
            }

            RuntimeEvent::TransferProgress { .. } => {}

            RuntimeEvent::UploadFinished {
                block_id, result, ..
            } => match result {
                Ok(res) => self.push_toast(
                    ToastSeverity::Success,
                    "File uploaded",
                    format!(
                        "Uploaded {} ({}) to {block_id}",
                        res.sanitized_name,
                        format_byte_size(res.bytes_uploaded),
                    ),
                ),
                Err(err) => self.push_toast(ToastSeverity::Error, "Upload failed", err),
            },

            RuntimeEvent::DownloadFinished {
                host_id, result, ..
            } => match result {
                Ok(DownloadResult::Downloaded {
                    remote_path,
                    local_path,
                    version,
                    ..
                }) => {
                    self.downloaded_file_cache
                        .insert((host_id, remote_path), (local_path.clone(), version));
                    if let Err(err) = open::that_detached(&local_path) {
                        self.push_toast(
                            ToastSeverity::Warning,
                            "Downloaded file",
                            format!("Saved to {} (open failed: {err})", local_path.display()),
                        );
                    }
                }
                Ok(DownloadResult::NotModified { remote_path, .. }) => {
                    if let Some((cached_path, _)) = self
                        .downloaded_file_cache
                        .get(&(host_id, remote_path.clone()))
                        .cloned()
                        && cached_path.exists()
                    {
                        if let Err(err) = open::that_detached(&cached_path) {
                            self.push_toast(
                                ToastSeverity::Warning,
                                "Open cached file failed",
                                format!("{} ({err})", cached_path.display()),
                            );
                        }
                    } else {
                        self.push_toast(
                            ToastSeverity::Info,
                            "File unchanged",
                            format!("Cached copy of {remote_path} is up to date"),
                        );
                    }
                }
                Err(err) => self.push_toast(ToastSeverity::Error, "Download failed", err),
            },

            RuntimeEvent::ThemeChanged(ev) => {
                if self.theme.apply_event(ev) {
                    self.ui.layout_cfg.hyprland_rounding = Some(self.theme.rounding);
                    let mode_2031 = self.pane_store.apply_theme_all(&self.theme.palette);
                    if !self.read_only {
                        for (key, report_bytes) in mode_2031 {
                            // Never push Mode 2031 reports to closed/stale blocks that are no
                            // longer tracked in the live workspace.
                            if self.workspace.block(&key).is_none() {
                                continue;
                            }
                            if let Ok(data) = String::from_utf8(report_bytes) {
                                self.send_command(HostCommand::TerminalWrite {
                                    host_id: key.host_id,
                                    session_id: key.session_id,
                                    block_id: key.block_id,
                                    data,
                                });
                            }
                        }
                        self.push_theme_to_owned_blocks();
                    }
                }
            }

            RuntimeEvent::NotificationClicked(target) => {
                self.focus_target_block(&target);
            }
        }
    }

    fn handle_control_event(&mut self, host_id: &str, event: ServerEvent, now: Instant) {
        match event {
            ServerEvent::SessionCreated { session } => {
                let new_sid = session.session_id.clone();
                if let Some(host) = self.workspace.host_mut(host_id) {
                    host.on_session_created(session);
                }
                if self.workspace.active_session.is_none() && self.command_launch.is_none() {
                    self.activate_session(
                        SessionKey::new(host_id.to_owned(), new_sid),
                        &HashMap::new(),
                    );
                }
            }
            ServerEvent::SessionDestroyed { session_id } => {
                self.cleanup_removed_session(host_id, &session_id);
            }
            ServerEvent::SessionMoved {
                session_id,
                before_session_id,
            } => {
                if let Some(host) = self.workspace.host_mut(host_id) {
                    host.on_session_moved(&session_id, before_session_id.as_deref());
                }
            }
            ServerEvent::KeymapChanged { generation, hash } => {
                if keymap_changed_needs_refresh(&self.actions, generation, &hash) {
                    self.send_command(HostCommand::RefreshKeymap {
                        host_id: host_id.to_owned(),
                        request: build_proto_keymap_request(&self.actions),
                    });
                }
            }
            ServerEvent::SessionEvent {
                session_id,
                event: sess_ev,
            } => {
                if let SessionEvent::BlockClosed { ref block_id } = sess_ev {
                    let key = BlockKey::new(host_id, &session_id, block_id);
                    self.pane_store.remove_block(&key, None);
                    let had_stream = self.opened_streams.remove(&key);
                    self.hydrated_blocks.remove(&key);
                    self.block_activity_at.remove(&key);
                    self.block_updated_unix.remove(&key);
                    self.block_osc94.remove(&key);
                    if had_stream {
                        self.send_command(HostCommand::CloseBlockStream {
                            host_id: key.host_id,
                            session_id: key.session_id,
                            block_id: key.block_id,
                        });
                    }
                    if self
                        .ephemeral_session
                        .as_ref()
                        .is_some_and(|sk| sk.host_id == host_id && sk.session_id == session_id)
                        && self
                            .command_launch
                            .as_ref()
                            .is_none_or(|c| !c.wait_after_command)
                    {
                        self.should_exit = true;
                    }
                }
                if let SessionEvent::BlockEvent(ref blk_ev) = sess_ev {
                    let key = BlockKey::new(host_id, &session_id, &blk_ev.block_id);
                    self.block_activity_at.insert(key, now);
                    if blk_ev.name == "child_exited"
                        && self
                            .ephemeral_session
                            .as_ref()
                            .is_some_and(|sk| sk.host_id == host_id && sk.session_id == session_id)
                        && self
                            .command_launch
                            .as_ref()
                            .is_none_or(|c| !c.wait_after_command)
                    {
                        self.should_exit = true;
                    }
                }
                let owner_change = match &sess_ev {
                    SessionEvent::BlockOwnerChanged {
                        block_id, owner, ..
                    } => Some((block_id.clone(), owner.clone())),
                    _ => None,
                };
                let needs_view = self
                    .workspace
                    .apply_session_event(host_id, &session_id, &sess_ev);
                if let Some((block_id, new_owner)) = owner_change {
                    let key = BlockKey::new(host_id, &session_id, &block_id);
                    let my_client_id = self
                        .workspace
                        .host(host_id)
                        .and_then(HostEntry::client_id)
                        .map(str::to_owned);
                    let is_visible = self.visible_pane_keys.contains(&key)
                        || self.focused_block_key().as_ref() == Some(&key);
                    if is_visible {
                        if my_client_id.is_some() && new_owner == my_client_id {
                            // Ownership just transferred to us: immediately reconcile grid size & theme!
                            self.workspace.invalidate_sent_resize(&key);
                            self.force_immediate_resize = true;
                            if !self.read_only {
                                self.send_command(HostCommand::SetTheme {
                                    host_id: key.host_id.clone(),
                                    session_id: key.session_id.clone(),
                                    block_id: key.block_id.clone(),
                                    args: theme_palette_to_rex_args(&self.theme.palette),
                                });
                            }
                        } else if new_owner.is_none() && !self.read_only {
                            // Another window closed/disowned this visible block: immediately reclaim!
                            self.workspace.invalidate_sent_resize(&key);
                            self.force_immediate_resize = true;
                            let trigger = if self.window_focused {
                                OwnershipTrigger::ExplicitInteractOrFit
                            } else {
                                OwnershipTrigger::AttachOrView
                            };
                            self.maybe_claim_ownership(&key, trigger);
                        }
                    }
                }
                if needs_view {
                    self.send_command(HostCommand::FetchView {
                        host_id: host_id.to_owned(),
                        session_id,
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_data_stream_event(&mut self, event: DataStreamEvent, now: Instant) {
        let key = BlockKey::new(
            event.host_id.clone(),
            event.session_id.clone(),
            event.block_id.clone(),
        );
        if matches!(
            event.kind,
            DataStreamEventKind::LiveVtChunk { .. } | DataStreamEventKind::SnapshotChunk { .. }
        ) {
            self.block_activity_at.insert(key.clone(), now);
        }

        match self
            .pane_store
            .handle_data_stream_event(event, &mut self.workspace)
        {
            Ok(outcome) => {
                let is_visible =
                    self.visible_pane_keys.is_empty() || self.visible_pane_keys.contains(&key);
                if is_visible
                    || !outcome.terminal_events.is_empty()
                    || outcome.became_ready
                    || outcome.stream_closed.is_some()
                    || outcome.rex_apc.is_some()
                {
                    self.needs_redraw = true;
                }
                if let Some(resize_args) = outcome.pending_resize
                    && !self.read_only
                {
                    if self.window_focused {
                        self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                    }
                    self.send_command(HostCommand::TerminalResize {
                        host_id: key.host_id.clone(),
                        session_id: key.session_id.clone(),
                        block_id: key.block_id.clone(),
                        args: resize_args,
                    });
                }
                if let Some(vt_engine::RexControlApc::Theme {
                    background, scheme, ..
                }) = outcome.rex_apc.as_ref()
                {
                    let apc_is_dark = match scheme.as_deref() {
                        Some(s) if s.eq_ignore_ascii_case("light") => Some(false),
                        Some(s) if s.eq_ignore_ascii_case("dark") => Some(true),
                        _ => background
                            .as_deref()
                            .and_then(|hex| omarchy_theme::Rgb::from_hex(hex).ok())
                            .map(|rgb| rgb.relative_luminance() < 0.25),
                    };
                    if self.read_only || apc_is_dark.is_some_and(|dark| dark != self.theme.is_dark)
                    {
                        let _ = self.pane_store.apply_theme_all(&self.theme.palette);
                    }
                }
                if let Some(vt_engine::RexControlApc::ChildExited { .. }) = outcome.rex_apc.as_ref()
                    && self.ephemeral_session.as_ref().is_some_and(|sk| {
                        sk.host_id == key.host_id && sk.session_id == key.session_id
                    })
                    && self
                        .command_launch
                        .as_ref()
                        .is_none_or(|c| !c.wait_after_command)
                {
                    self.should_exit = true;
                }
                if outcome.became_ready && self.read_only {
                    let _ = self.pane_store.apply_theme_all(&self.theme.palette);
                }
                if !outcome.terminal_events.is_empty() {
                    self.handle_terminal_events(&key, &outcome.terminal_events, now);
                }
                if outcome.stream_closed.is_some() {
                    self.opened_streams.remove(&key);
                }
                if let Some(err) = outcome.stream_error {
                    tracing::warn!(block = %key.block_id, error = %err, "data stream error");
                    // Always release `opened_streams` on stream error (and drop any non-ready
                    // placeholder pane) so a transient failure never permanently wedges the pane
                    // into a black screen!
                    self.opened_streams.remove(&key);
                    if !self.pane_store.is_ready(&key) {
                        self.pane_store.remove_block(&key, None);
                    }
                    self.needs_redraw = true;
                }
            }
            Err(err) => {
                tracing::warn!(?err, block = %key.block_id, "PaneStore failed to process DataStreamEvent");
            }
        }
    }

    fn handle_terminal_events(&mut self, key: &BlockKey, events: &[TerminalEvent], now: Instant) {
        let is_focused_pane = self.window_focused && self.focused_block_key().as_ref() == Some(key);
        let muted = self
            .settings
            .is_muted(&key.host_id, &key.session_id, &key.block_id);

        for ev in events {
            match ev {
                TerminalEvent::ProgressReport { state, progress } => match state {
                    ProgressState::Remove => {
                        self.block_osc94.remove(key);
                    }
                    ProgressState::Set => {
                        self.block_osc94.insert(
                            key.clone(),
                            Osc94Progress {
                                state: Osc94State::Normal,
                                percent: progress.unwrap_or(0),
                            },
                        );
                    }
                    ProgressState::Error => {
                        self.block_osc94.insert(
                            key.clone(),
                            Osc94Progress {
                                state: Osc94State::Error,
                                percent: progress.unwrap_or(100),
                            },
                        );
                    }
                    ProgressState::Indeterminate => {
                        self.block_osc94.insert(
                            key.clone(),
                            Osc94Progress {
                                state: Osc94State::Indeterminate,
                                percent: 0,
                            },
                        );
                    }
                    ProgressState::Pause => {
                        self.block_osc94.insert(
                            key.clone(),
                            Osc94Progress {
                                state: Osc94State::Paused,
                                percent: progress.unwrap_or(0),
                            },
                        );
                    }
                },
                TerminalEvent::DesktopNotification { title, body } => {
                    if !muted && !is_focused_pane && !self.read_only {
                        let session_label = self
                            .workspace
                            .session(&SessionKey::new(&key.host_id, &key.session_id))
                            .map(|s| s.label.clone())
                            .unwrap_or_else(|| "omarex".to_owned());
                        let formatted_title = if title.is_empty() {
                            session_label
                        } else {
                            format!("{session_label} · {title}")
                        };
                        self.send_command(HostCommand::SendNotification(NotificationRequest::new(
                            NotificationTarget {
                                host: key.host_id.clone(),
                                session_id: key.session_id.clone(),
                                block_id: key.block_id.clone(),
                            },
                            formatted_title,
                            body.clone(),
                            NotificationUrgency::Normal,
                        )));
                    }
                }
                TerminalEvent::ProgramStatus(report) => {
                    self.block_activity_at.insert(key.clone(), now);
                    if matches!(
                        report.state,
                        vt_engine::ProgramStatusState::Blocked
                            | vt_engine::ProgramStatusState::Error
                    ) && !muted
                        && !is_focused_pane
                        && !self.read_only
                    {
                        let urgency = if report.state == vt_engine::ProgramStatusState::Error {
                            NotificationUrgency::Critical
                        } else {
                            NotificationUrgency::Normal
                        };
                        let session_label = self
                            .workspace
                            .session(&SessionKey::new(&key.host_id, &key.session_id))
                            .map(|s| s.label.clone())
                            .unwrap_or_else(|| "omarex".to_owned());
                        let headline = if report.title.is_empty() {
                            "Needs attention"
                        } else {
                            report.title.as_str()
                        };
                        self.send_command(HostCommand::SendNotification(NotificationRequest::new(
                            NotificationTarget {
                                host: key.host_id.clone(),
                                session_id: key.session_id.clone(),
                                block_id: key.block_id.clone(),
                            },
                            format!("{session_label} · {headline}"),
                            report.message.clone(),
                            urgency,
                        )));
                    }
                }
                TerminalEvent::ClipboardWrite { entries, .. } => {
                    if self.clipboard_enabled
                        && let Some(text_entry) =
                            entries.iter().find(|e| e.mime.starts_with("text/"))
                        && let Ok(text) = std::str::from_utf8(&text_entry.data)
                    {
                        set_system_clipboard_text(text);
                    }
                }
                _ => {}
            }
        }
    }

    fn pick_initial_session_id(
        &self,
        host_id: &str,
        server_selected: Option<&str>,
    ) -> Option<String> {
        let host = self.workspace.host(host_id)?;
        // 1. Explicit CLI `--session <name|id>` filter.
        if let Some(pref) = &self.preferred_session_filter
            && let Some(found) = host
                .sessions
                .values()
                .find(|s| s.session_id == *pref || s.label.eq_ignore_ascii_case(pref))
        {
            return Some(found.session_id.clone());
        }
        // 2. Last session the user actively worked in (session continuity).
        if let Some(last_sid) = &self.settings.last_session_id
            && host.sessions.contains_key(last_sid)
        {
            return Some(last_sid.clone());
        }
        // 3. Normal (non-T3) Rex terminal session first — Rex is a terminal multiplexer first.
        if let Some(normal_sid) = self.find_preferred_terminal_session(host_id, None) {
            return Some(normal_sid);
        }
        // 4. Server-selected session, or fallback to first session (including `T3 Code`).
        if let Some(sel) = server_selected
            && host.sessions.contains_key(sel)
        {
            return Some(sel.to_owned());
        }
        host.session_order.first().cloned()
    }

    fn activate_session(
        &mut self,
        target: SessionKey,
        tickets: &HashMap<(String, String), String>,
    ) {
        let plan = self.workspace.select_session(target.clone());
        for evicted in plan.evicted_sessions {
            self.pane_store
                .evict_session(&evicted.host_id, &evicted.session_id, None);
            self.opened_streams
                .retain(|k| !(k.host_id == evicted.host_id && k.session_id == evicted.session_id));
            self.send_command(HostCommand::EvictSessionStreams {
                host_id: evicted.host_id,
                session_id: evicted.session_id,
                disown: !self.read_only,
            });
        }

        let has_view = self
            .workspace
            .session(&target)
            .is_some_and(|s| s.view.is_some());
        if plan.needs_attach || !has_view {
            self.send_command(HostCommand::SelectSession {
                host_id: target.host_id.clone(),
                session_id: target.session_id.clone(),
            });
        }

        self.settings.last_host_id = Some(target.host_id.clone());
        self.settings.last_session_id = Some(target.session_id.clone());

        let focused = self
            .workspace
            .session(&target)
            .and_then(|s| s.view.as_ref())
            .and_then(active_window_of_view)
            .and_then(primary_block_id_of_window)
            .map(str::to_owned);
        self.ui.on_session_view_changed(focused.clone());
        self.record_nav(target.host_id.clone(), target.session_id.clone(), focused);
        let has_ticket_for_target = tickets.keys().any(|(sid, _)| sid == &target.session_id);
        if !plan.needs_attach || has_ticket_for_target {
            self.open_active_window_streams(&target, tickets);
        }
    }

    /// Evicts any zombie panes/streams in `sk` that were pruned from `SessionView` (preventing
    /// `TerminalWrite` 404 errors on theme changes and stale data-stream retries).
    fn prune_stale_session_blocks(&mut self, sk: &SessionKey) {
        let Some(live_ids): Option<HashSet<String>> = self
            .workspace
            .session(sk)
            .map(|s| s.blocks.keys().cloned().collect())
        else {
            return;
        };

        let mut stale: Vec<BlockKey> = self
            .pane_store
            .pane_keys()
            .into_iter()
            .filter(|k| {
                k.host_id == sk.host_id
                    && k.session_id == sk.session_id
                    && !live_ids.contains(&k.block_id)
            })
            .collect();
        for k in &self.opened_streams {
            if k.host_id == sk.host_id
                && k.session_id == sk.session_id
                && !live_ids.contains(&k.block_id)
                && !stale.contains(k)
            {
                stale.push(k.clone());
            }
        }

        for key in stale {
            self.pane_store.remove_block(&key, None);
            let had_stream = self.opened_streams.remove(&key);
            self.hydrated_blocks.remove(&key);
            self.block_activity_at.remove(&key);
            self.block_updated_unix.remove(&key);
            self.block_osc94.remove(&key);
            if had_stream {
                self.send_command(HostCommand::CloseBlockStream {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                });
            }
        }
    }

    /// Cleans up all local panes, stream tasks, and metadata for a destroyed or missing session.
    fn cleanup_removed_session(&mut self, host_id: &str, session_id: &str) {
        self.collapsed_sessions
            .remove(&(host_id.to_owned(), session_id.to_owned()));
        if self
            .ephemeral_session
            .as_ref()
            .is_some_and(|sk| sk.host_id == host_id && sk.session_id == session_id)
        {
            self.ephemeral_session = None;
            if self
                .command_launch
                .as_ref()
                .is_none_or(|c| !c.wait_after_command)
            {
                self.should_exit = true;
            }
        }
        let had_streams = self
            .opened_streams
            .iter()
            .any(|k| k.host_id == host_id && k.session_id == session_id);
        self.pane_store.evict_session(host_id, session_id, None);
        self.opened_streams
            .retain(|k| !(k.host_id == host_id && k.session_id == session_id));
        self.hydrated_blocks
            .retain(|k| !(k.host_id == host_id && k.session_id == session_id));
        self.block_activity_at
            .retain(|k, _| !(k.host_id == host_id && k.session_id == session_id));
        self.block_updated_unix
            .retain(|k, _| !(k.host_id == host_id && k.session_id == session_id));
        self.block_osc94
            .retain(|k, _| !(k.host_id == host_id && k.session_id == session_id));
        if had_streams {
            self.send_command(HostCommand::EvictSessionStreams {
                host_id: host_id.to_owned(),
                session_id: session_id.to_owned(),
                disown: false,
            });
        }
        self.workspace.on_session_destroyed(host_id, session_id);
        self.needs_redraw = true;
    }

    fn open_active_window_streams(
        &mut self,
        sk: &SessionKey,
        tickets: &HashMap<(String, String), String>,
    ) {
        let block_ids: Vec<String> = self
            .workspace
            .session(sk)
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

        for block_id in block_ids {
            let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id.clone());
            if self.opened_streams.insert(key.clone()) {
                let ticket = tickets
                    .get(&(sk.session_id.clone(), block_id.clone()))
                    .cloned();
                self.send_command(HostCommand::OpenBlockStream {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                    ticket,
                });
            }
        }
    }

    fn maybe_hydrate_session_metadata(&mut self, sk: &SessionKey) {
        let Some(sess) = self.workspace.session(sk) else {
            return;
        };
        let mut to_hydrate = Vec::new();
        for block_id in sess.all_block_ids() {
            let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id.clone());
            if self.hydrated_blocks.insert(key) {
                to_hydrate.push(block_id);
            }
        }
        if !to_hydrate.is_empty() {
            self.send_command(HostCommand::HydrateSessionMetadata {
                host_id: sk.host_id.clone(),
                session_id: sk.session_id.clone(),
                block_ids: to_hydrate,
            });
        }
    }

    fn maybe_claim_ownership(&mut self, key: &BlockKey, trigger: OwnershipTrigger) {
        if self.read_only {
            return;
        }
        if let OwnershipDecision::Own { mode } = self.workspace.decide_ownership(key, trigger) {
            self.send_command(HostCommand::OwnBlock {
                host_id: key.host_id.clone(),
                session_id: key.session_id.clone(),
                block_ids: Some(vec![key.block_id.clone()]),
                mode,
            });
        }
    }

    fn push_theme_to_owned_blocks(&mut self) {
        let args = theme_palette_to_rex_args(&self.theme.palette);
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let client_id = self
            .workspace
            .host(&sk.host_id)
            .and_then(HostEntry::client_id)
            .map(str::to_owned);
        let targets: Vec<String> = self
            .workspace
            .session(&sk)
            .map(|s| {
                s.blocks
                    .iter()
                    .filter(|&(_, b)| {
                        b.owner.is_none() || (client_id.is_some() && b.owner == client_id)
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();

        for block_id in targets {
            self.send_command(HostCommand::SetTheme {
                host_id: sk.host_id.clone(),
                session_id: sk.session_id.clone(),
                block_id,
                args: args.clone(),
            });
        }
    }

    fn focus_target_block(&mut self, target: &NotificationTarget) {
        self.set_session_collapsed(&target.host, &target.session_id, false);
        let sk = SessionKey::new(target.host.clone(), target.session_id.clone());
        self.activate_session(sk, &HashMap::new());
        self.send_command(HostCommand::FocusBlock {
            host_id: target.host.clone(),
            session_id: target.session_id.clone(),
            block_id: target.block_id.clone(),
        });
        self.ui.focus.focus_terminal(Some(target.block_id.clone()));
        self.force_immediate_resize = true;
        let key = BlockKey::new(&target.host, &target.session_id, &target.block_id);
        self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
    }

    fn record_nav(&mut self, host_id: String, session_id: String, block_id: Option<String>) {
        let entry = (host_id, session_id, block_id);
        if self.nav_history.get(self.nav_index) == Some(&entry) {
            return;
        }
        if self.nav_index + 1 < self.nav_history.len() {
            self.nav_history.truncate(self.nav_index + 1);
        }
        self.nav_history.push(entry);
        self.nav_index = self.nav_history.len().saturating_sub(1);
    }

    // =========================================================================
    // 3. UiAction & ClientCommand Dispatcher
    // =========================================================================

    /// Executes any high-level [`UiAction`] emitted by `UiTree` (clicks, modals, menus, shortcuts).
    pub fn dispatch_ui_action(&mut self, action: UiAction, now: Instant) {
        self.needs_redraw = true;
        match action {
            UiAction::None | UiAction::CloseModal => {}

            UiAction::ToggleSidebar => {
                // UiTree::handle_click already toggled `sidebar_collapsed` for button click;
                // snap visible terminal grids immediately to the new viewport width.
                self.force_immediate_resize = true;
            }

            UiAction::ToggleActivityMode | UiAction::ToggleActivitySort => {}

            UiAction::ToggleHostCollapse { host_id } => {
                if !self.collapsed_hosts.remove(&host_id) {
                    self.collapsed_hosts.insert(host_id);
                }
            }

            UiAction::ToggleSessionCollapse {
                host_id,
                session_id,
            } => {
                let currently_collapsed = self
                    .collapsed_sessions
                    .contains(&(host_id.clone(), session_id.clone()));
                self.set_session_collapsed(&host_id, &session_id, !currently_collapsed);
            }

            UiAction::SelectSession {
                host_id,
                session_id,
            } => {
                self.activate_session(SessionKey::new(host_id, session_id), &HashMap::new());
                self.force_immediate_resize = true;
            }

            UiAction::ToggleSessionMute {
                host_id,
                session_id,
            } => {
                self.settings.toggle_mute_session(&host_id, &session_id);
                self.save_settings_best_effort();
            }

            UiAction::NewSession { host_id } => {
                self.open_real_shell_terminal(host_id, true);
            }

            UiAction::NewTerminal => {
                self.open_real_shell_terminal(None, false);
            }

            UiAction::SelectTab {
                host_id,
                session_id,
                window_id,
            } => {
                self.unread_windows.remove(&(
                    host_id.clone(),
                    session_id.clone(),
                    window_id.clone(),
                ));
                self.set_session_collapsed(&host_id, &session_id, false);
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                if self.workspace.active_session.as_ref() != Some(&sk) {
                    self.activate_session(sk.clone(), &HashMap::new());
                }
                let (primary_block, window_known) = self
                    .workspace
                    .session_mut(&sk)
                    .and_then(|sess| sess.view.as_mut())
                    .map(|view| {
                        view.active_window_id = Some(window_id.clone());
                        let found = view
                            .windows
                            .iter()
                            .find(|w| w.window_id.as_deref() == Some(window_id.as_str()));
                        (
                            found
                                .and_then(primary_block_id_of_window)
                                .map(str::to_owned),
                            found.is_some(),
                        )
                    })
                    .unwrap_or((None, false));
                self.send_command(HostCommand::FocusWindow {
                    host_id: host_id.clone(),
                    session_id: session_id.clone(),
                    window_id,
                });
                if !window_known {
                    self.send_command(HostCommand::FetchView {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                    });
                }
                self.open_active_window_streams(&sk, &HashMap::new());
                self.force_immediate_resize = true;
                if let Some(bid) = primary_block {
                    self.ui.focus.focus_terminal(Some(bid.clone()));
                    let key = BlockKey::new(host_id, session_id, bid);
                    self.workspace.invalidate_sent_resize(&key);
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
            }

            UiAction::CloseTab {
                host_id,
                session_id,
                window_id,
            } => {
                self.send_command(HostCommand::CloseWindow {
                    host_id,
                    session_id,
                    window_id,
                    undoable: true,
                });
            }

            UiAction::NewTab {
                host_id,
                session_id,
            } => {
                let resolved_sk = match (host_id.as_deref(), session_id.as_deref()) {
                    (Some(hid), Some(sid)) => Some(SessionKey::new(hid, sid)),
                    (None, Some(sid)) => self.resolve_session_key(Some(sid)),
                    _ => None,
                };
                if let Some(sk) = resolved_sk {
                    let is_t3 = self
                        .workspace
                        .session(&sk)
                        .is_some_and(|s| is_t3_session(&s.label));
                    if is_t3 {
                        self.dispatch_ui_action(
                            UiAction::NewT3Thread {
                                host_id: sk.host_id,
                                session_id: sk.session_id,
                            },
                            now,
                        );
                    } else {
                        self.collapsed_hosts.remove(&sk.host_id);
                        self.set_session_collapsed(&sk.host_id, &sk.session_id, false);
                        if self.workspace.active_session.as_ref() != Some(&sk) {
                            self.activate_session(sk.clone(), &HashMap::new());
                        }
                        let cwd = self.resolve_shell_cwd_for_host(&sk.host_id, None, false);
                        let layout =
                            self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
                        self.send_command(HostCommand::NewWindow {
                            host_id: sk.host_id,
                            request: SessionNewWindowRequest {
                                session_id: sk.session_id,
                                window_label: None,
                                layout: Some(layout),
                                focus: Some(true),
                            },
                        });
                    }
                } else {
                    self.open_real_shell_terminal(host_id, false);
                }
            }

            UiAction::NewT3Thread {
                host_id,
                session_id,
            } => {
                self.collapsed_hosts.remove(&host_id);
                self.set_session_collapsed(&host_id, &session_id, false);
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                if self.workspace.active_session.as_ref() != Some(&sk) {
                    self.activate_session(sk, &HashMap::new());
                    self.force_immediate_resize = true;
                }
                self.send_server_action_for_session(&host_id, Some(&session_id), "t3_new", None);
            }

            UiAction::StartRenameHost { host_id } => {
                if host_id != LOCAL_HOST_ID {
                    self.pending_rename_target = Some(PendingRenameTarget::Host { host_id });
                    self.open_modal_kind(ModalKind::Rename);
                }
            }

            UiAction::DisconnectHost { host_id } => {
                if host_id != LOCAL_HOST_ID {
                    let sids: Vec<String> = self
                        .workspace
                        .host(&host_id)
                        .map(|h| h.session_order.clone())
                        .unwrap_or_default();
                    for sid in &sids {
                        self.pane_store.evict_session(&host_id, sid, None);
                    }
                    self.opened_streams.retain(|k| k.host_id != host_id);
                    self.hydrated_blocks.retain(|k| k.host_id != host_id);
                    self.block_activity_at.retain(|k, _| k.host_id != host_id);
                    self.block_updated_unix.retain(|k, _| k.host_id != host_id);
                    self.block_osc94.retain(|k, _| k.host_id != host_id);
                    self.other_app_attached_sessions.remove(&host_id);
                    if let Some(host) = self.workspace.host_mut(&host_id) {
                        host.status = HostStatus::Disconnected;
                        host.sessions.clear();
                        host.session_order.clear();
                    }
                    if self
                        .workspace
                        .active_session
                        .as_ref()
                        .is_some_and(|k| k.host_id == host_id)
                    {
                        self.workspace.active_session = None;
                        if let Some(fallback_sk) =
                            self.workspace.host_order.iter().find_map(|hid| {
                                let host = self.workspace.host(hid)?;
                                let sid = host.session_order.first()?;
                                Some(SessionKey::new(hid.clone(), sid.clone()))
                            })
                        {
                            self.activate_session(fallback_sk, &HashMap::new());
                            self.force_immediate_resize = true;
                        }
                    }
                    self.send_command(HostCommand::RemoveHost { host_id });
                }
            }

            UiAction::ReconnectHost { host_id } => {
                if host_id != LOCAL_HOST_ID {
                    let saved = self
                        .settings
                        .saved_hosts
                        .iter()
                        .find(|h| h.id == host_id)
                        .cloned();
                    let label = self.host_display_label(&host_id);
                    let endpoint = saved
                        .as_ref()
                        .and_then(|s| RexEndpoint::parse(&s.endpoint).ok())
                        .or_else(|| self.workspace.host(&host_id).map(|h| h.endpoint.clone()));
                    if let Some(ep) = endpoint {
                        if let Some(host) = self.workspace.host_mut(&host_id) {
                            host.status = HostStatus::Reconnecting {
                                reason: "reconnecting…".into(),
                            };
                        }
                        self.send_command(HostCommand::RemoveHost {
                            host_id: host_id.clone(),
                        });
                        self.send_command(HostCommand::AddHost {
                            host_id,
                            label,
                            endpoint: ep,
                            preferred_session_id: None,
                        });
                    }
                }
            }

            UiAction::RemoveHost { host_id } => {
                if host_id != LOCAL_HOST_ID {
                    let removed_sessions = self.workspace.remove_host(&host_id);
                    for sk in &removed_sessions {
                        self.pane_store
                            .evict_session(&sk.host_id, &sk.session_id, None);
                    }
                    self.collapsed_hosts.remove(&host_id);
                    self.collapsed_sessions.retain(|(h, _)| h != &host_id);
                    self.unread_windows.retain(|(h, _, _)| h != &host_id);
                    self.opened_streams.retain(|k| k.host_id != host_id);
                    self.hydrated_blocks.retain(|k| k.host_id != host_id);
                    self.block_activity_at.retain(|k, _| k.host_id != host_id);
                    self.block_updated_unix.retain(|k, _| k.host_id != host_id);
                    self.block_osc94.retain(|k, _| k.host_id != host_id);
                    self.other_app_attached_sessions.remove(&host_id);

                    self.settings.saved_hosts.retain(|h| h.id != host_id);
                    self.save_settings_best_effort();
                    self.send_command(HostCommand::RemoveHost { host_id });

                    if self.workspace.active_session.is_none()
                        && let Some(fallback_sk) =
                            self.workspace.host_order.iter().find_map(|hid| {
                                let host = self.workspace.host(hid)?;
                                let sid = host.session_order.first()?;
                                Some(SessionKey::new(hid.clone(), sid.clone()))
                            })
                    {
                        self.activate_session(fallback_sk, &HashMap::new());
                        self.force_immediate_resize = true;
                    }
                }
            }

            UiAction::FocusPane { block_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let key =
                        BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id.clone());
                    self.ui.focus.focus_terminal(Some(block_id.clone()));
                    self.send_command(HostCommand::FocusBlock {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                        block_id,
                    });
                    self.maybe_claim_ownership(&key, OwnershipTrigger::FocusPane);
                }
            }

            UiAction::PaneButton { block_id, button } => {
                self.handle_pane_header_button(&block_id, button);
            }

            UiAction::UnzoomPane { block_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    self.send_command(HostCommand::ZoomBlock {
                        host_id: sk.host_id,
                        request: SessionZoomBlockRequest {
                            session_id: sk.session_id,
                            block_id: Some(block_id),
                            window_id: None,
                            zoom: Some(false),
                        },
                    });
                }
            }

            UiAction::RestoreMinimizedFloating { block_id } => {
                if let Some((sk, layer_id)) = self.find_layer_for_block(&block_id) {
                    self.send_command(HostCommand::SetLayerState {
                        host_id: sk.host_id,
                        request: SessionSetLayerStateRequest {
                            session_id: sk.session_id,
                            layer_id,
                            state: "normal".to_owned(),
                            focus: Some(true),
                        },
                    });
                }
            }

            UiAction::CloseMinimizedFloating { block_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    self.send_command(HostCommand::CloseBlock {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                        block_id,
                        undoable: true,
                    });
                }
            }

            UiAction::BeginSidebarResize => {
                self.active_drag = Some(ActiveDrag::SidebarResize {
                    on_right: self.ui.layout_cfg.toolbar_position == ToolbarPosition::Right,
                });
            }

            UiAction::BeginSplitDividerDrag { split_path } => {
                if let Some(sk) = self.workspace.active_session.clone()
                    && let Some(out) = &self.last_ui_output
                    && let Some(div) = out
                        .dividers
                        .iter()
                        .find(|d| d.split_path == split_path)
                        .cloned()
                {
                    let split_target = self.resolve_split_target_at_path(&sk, &split_path);
                    self.active_drag = Some(ActiveDrag::SplitDivider {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                        divider: div,
                        split_target,
                    });
                }
            }

            UiAction::BeginFloatingDrag { block_id, zone } => {
                if let Some((sk, layer_id)) = self.find_layer_for_block(&block_id)
                    && let Some(bounds) = self.find_layer_bounds(&sk, &layer_id)
                    && let Some(out) = &self.last_ui_output
                {
                    self.active_drag = Some(ActiveDrag::FloatingLayer {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                        layer_id,
                        zone,
                        start_px: self.pointer_pos,
                        start_bounds: bounds,
                        workspace: out.chrome.workspace,
                    });
                }
            }

            UiAction::OpenModal(kind) => {
                self.open_modal_kind(kind);
            }

            UiAction::RunCommand { action_id } => {
                self.dispatch_command_id(&action_id, None, now);
            }

            UiAction::ReplyRexPrompt(reply) => {
                let call_id = reply.call_id.clone();
                self.prompts
                    .on_prompt_ended(&tr3x_bridge::PromptEndedEvent {
                        call_id,
                        reason: "replied".to_owned(),
                    });
                let host_id = self
                    .active_prompt_host
                    .take()
                    .or_else(|| {
                        self.workspace
                            .active_session
                            .as_ref()
                            .map(|s| s.host_id.clone())
                    })
                    .unwrap_or_else(|| LOCAL_HOST_ID.to_owned());
                let proto_reply: ClientReplyRequest = reply_to_proto(reply);
                self.send_command(HostCommand::ClientReply {
                    host_id,
                    request: proto_reply,
                });
            }

            UiAction::SubmitRename {
                target_id,
                new_label,
            } => {
                let trimmed = new_label.trim().to_owned();
                if !trimmed.is_empty() {
                    if let Some(hid) = target_id.strip_prefix("__rename_host:") {
                        let (new_host_label, new_endpoint_opt) = match trimmed.split_once('=') {
                            Some((lhs, rhs))
                                if !lhs.trim().is_empty()
                                    && !lhs.contains("://")
                                    && !rhs.trim().is_empty() =>
                            {
                                (lhs.trim().to_owned(), Some(rhs.trim().to_owned()))
                            }
                            _ => (trimmed.clone(), None),
                        };
                        let mut reconnect_ep: Option<RexEndpoint> = None;
                        if let Some(ref raw_ep) = new_endpoint_opt {
                            match RexEndpoint::parse(raw_ep) {
                                Ok(parsed) => {
                                    reconnect_ep = Some(parsed);
                                }
                                Err(err) => {
                                    self.push_toast(
                                        ToastSeverity::Error,
                                        "Invalid host endpoint",
                                        err.to_string(),
                                    );
                                    return;
                                }
                            }
                        }
                        if let Some(host) = self.workspace.host_mut(hid) {
                            host.label = new_host_label.clone();
                            if let Some(ref ep) = reconnect_ep {
                                host.endpoint = ep.clone();
                                host.status = HostStatus::Reconnecting {
                                    reason: "endpoint updated".into(),
                                };
                            }
                        }
                        if let Some(saved) =
                            self.settings.saved_hosts.iter_mut().find(|h| h.id == hid)
                        {
                            saved.label = new_host_label.clone();
                            if let Some(raw_ep) = new_endpoint_opt {
                                saved.endpoint = raw_ep;
                            }
                            self.save_settings_best_effort();
                        }
                        if let Some(ep) = reconnect_ep {
                            self.send_command(HostCommand::RemoveHost {
                                host_id: hid.to_owned(),
                            });
                            self.send_command(HostCommand::AddHost {
                                host_id: hid.to_owned(),
                                label: new_host_label,
                                endpoint: ep,
                                preferred_session_id: None,
                            });
                        }
                    } else if let Some(rest) = target_id.strip_prefix("__rename_window:") {
                        let mut parts = rest.splitn(3, '|');
                        if let (Some(h), Some(s), Some(w)) =
                            (parts.next(), parts.next(), parts.next())
                        {
                            self.send_command(HostCommand::RenameWindow {
                                host_id: h.to_owned(),
                                session_id: s.to_owned(),
                                window_id: w.to_owned(),
                                label: trimmed,
                            });
                        }
                    } else if let Some(rest) = target_id.strip_prefix("__rename_block:") {
                        let mut parts = rest.splitn(3, '|');
                        if let (Some(h), Some(s), Some(b)) =
                            (parts.next(), parts.next(), parts.next())
                        {
                            self.send_command(HostCommand::RenameBlock {
                                host_id: h.to_owned(),
                                session_id: s.to_owned(),
                                block_id: b.to_owned(),
                                label: trimmed,
                            });
                        }
                    } else if let Some(rest) = target_id.strip_prefix("__rename_session:")
                        && let Some((h, s)) = rest.split_once('|')
                    {
                        self.send_command(HostCommand::RenameSession {
                            host_id: h.to_owned(),
                            session_id: s.to_owned(),
                            label: trimmed,
                        });
                    } else if let Some(sk) = self.workspace.active_session.clone() {
                        if let Some(win_id) = target_id.strip_prefix("window:") {
                            self.send_command(HostCommand::RenameWindow {
                                host_id: sk.host_id,
                                session_id: sk.session_id,
                                window_id: format!("window:{win_id}"),
                                label: trimmed,
                            });
                        } else if target_id.starts_with("block:") {
                            self.send_command(HostCommand::RenameBlock {
                                host_id: sk.host_id,
                                session_id: sk.session_id,
                                block_id: target_id,
                                label: trimmed,
                            });
                        } else if target_id.starts_with("session:") {
                            self.send_command(HostCommand::RenameSession {
                                host_id: sk.host_id,
                                session_id: target_id,
                                label: trimmed,
                            });
                        } else {
                            self.send_command(HostCommand::RenameSession {
                                host_id: sk.host_id,
                                session_id: sk.session_id,
                                label: trimmed,
                            });
                        }
                    }
                }
            }

            UiAction::SubmitAddHost { endpoint } => {
                let raw = endpoint.trim();
                if !raw.is_empty() {
                    let (explicit_label, raw_ep) = match raw.split_once('=') {
                        Some((lhs, rhs))
                            if !lhs.trim().is_empty()
                                && !lhs.contains("://")
                                && !rhs.trim().is_empty() =>
                        {
                            (Some(lhs.trim().to_owned()), rhs.trim())
                        }
                        _ => (None, raw),
                    };
                    match RexEndpoint::parse(raw_ep) {
                        Ok(parsed) => {
                            let host_id = format!("remote:{}", sanitize_host_id(raw_ep));
                            let label = explicit_label.unwrap_or_else(|| derive_host_label(raw_ep));
                            if let Some(existing) = self
                                .settings
                                .saved_hosts
                                .iter_mut()
                                .find(|h| h.id == host_id || h.endpoint == raw_ep)
                            {
                                existing.label = label.clone();
                                existing.endpoint = raw_ep.to_owned();
                                existing.auto_connect = true;
                            } else {
                                self.settings.saved_hosts.push(SavedHost {
                                    id: host_id.clone(),
                                    label: label.clone(),
                                    endpoint: raw_ep.to_owned(),
                                    auto_connect: true,
                                });
                            }
                            self.save_settings_best_effort();

                            let mut entry =
                                HostEntry::new(host_id.clone(), label.clone(), parsed.clone());
                            entry.is_local = false;
                            entry.status = HostStatus::Connecting;
                            if let Some(existing) = self.workspace.host_mut(&host_id) {
                                existing.label = label.clone();
                                existing.endpoint = parsed.clone();
                                existing.is_local = false;
                                existing.status = HostStatus::Connecting;
                            } else {
                                self.workspace.add_host(entry);
                            }

                            self.send_command(HostCommand::AddHost {
                                host_id,
                                label,
                                endpoint: parsed,
                                preferred_session_id: None,
                            });
                        }
                        Err(err) => {
                            self.push_toast(
                                ToastSeverity::Error,
                                "Invalid host endpoint",
                                err.to_string(),
                            );
                        }
                    }
                }
            }

            UiAction::UpdateSetting(change) => {
                self.apply_setting_change(change);
            }

            UiAction::FindQueryChanged(query) => {
                if let Some(key) = self.focused_block_key()
                    && let Ok(summary) = self.pane_store.search_query(&key, &query)
                {
                    self.ui.find_bar.total_matches = summary.total_matches;
                    self.ui.find_bar.current_match = summary.selected_index.map_or(0, |i| i + 1);
                }
            }

            UiAction::FindNext => {
                if let Some(key) = self.focused_block_key() {
                    let _ = self.pane_store.search_next(&key);
                }
            }

            UiAction::FindPrev => {
                if let Some(key) = self.focused_block_key() {
                    let _ = self.pane_store.search_prev(&key);
                }
            }

            UiAction::FindToggleCase | UiAction::FindToggleRegex => {
                let query = self.ui.find_bar.input.text().to_owned();
                if !query.is_empty()
                    && let Some(key) = self.focused_block_key()
                    && let Ok(summary) = self.pane_store.search_query(&key, &query)
                {
                    self.ui.find_bar.total_matches = summary.total_matches;
                    self.ui.find_bar.current_match = summary.selected_index.map_or(0, |i| i + 1);
                }
            }

            UiAction::CloseFindBar => {
                if let Some(key) = self.focused_block_key() {
                    self.pane_store.search_close(&key);
                }
            }

            UiAction::ActivateToast { id } | UiAction::DismissToast { id } => {
                self.ui.toasts.retain(|t| t.id != id);
                self.toast_deadlines.remove(&id);
            }

            UiAction::CopySelection { block_id } => {
                let target = block_id
                    .and_then(|bid| {
                        let sk = self.workspace.active_session.as_ref()?;
                        Some(BlockKey::new(&sk.host_id, &sk.session_id, bid))
                    })
                    .or_else(|| self.focused_block_key());
                if let Some(key) = target
                    && let Ok(Some(text)) = self.pane_store.copy_selection_text(&key, true, true)
                    && self.clipboard_enabled
                {
                    set_system_clipboard_text(&text);
                }
            }

            UiAction::PasteClipboard { block_id } => {
                self.handle_clipboard_paste_or_image(block_id, now);
            }

            UiAction::CloseOtherTabs {
                session_id,
                keep_window_id,
            } => {
                if let Some(sk) = self.resolve_session_key(Some(&session_id)) {
                    let to_close: Vec<String> = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .map(|v| {
                            v.windows
                                .iter()
                                .filter_map(|w| w.window_id.clone())
                                .filter(|id| *id != keep_window_id)
                                .collect()
                        })
                        .unwrap_or_default();
                    for window_id in to_close {
                        self.send_command(HostCommand::CloseWindow {
                            host_id: sk.host_id.clone(),
                            session_id: sk.session_id.clone(),
                            window_id,
                            undoable: true,
                        });
                    }
                }
            }

            UiAction::MarkTabUnread { window_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let tuple = (sk.host_id, sk.session_id, window_id);
                    if !self.unread_windows.remove(&tuple) {
                        self.unread_windows.insert(tuple);
                    }
                }
            }

            UiAction::MoveTabToNewWindow { .. } => {}

            UiAction::CloseSession { session_id } => {
                if let Some(sk) = self.resolve_session_key(Some(&session_id)) {
                    self.set_session_collapsed(&sk.host_id, &sk.session_id, false);
                    self.send_command(HostCommand::DestroySession {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                    });
                }
            }

            UiAction::CollapseOtherSessions { keep_session_id } => {
                let target_host = self
                    .resolve_session_key(Some(&keep_session_id))
                    .map(|sk| sk.host_id)
                    .or_else(|| {
                        self.workspace
                            .active_session
                            .as_ref()
                            .map(|sk| sk.host_id.clone())
                    });
                if let Some(host_id) = target_host
                    && let Some(session_ids) = self
                        .workspace
                        .host(&host_id)
                        .map(|h| h.session_order.clone())
                {
                    for sid in session_ids {
                        let should_collapse = sid != keep_session_id;
                        self.set_session_collapsed(&host_id, &sid, should_collapse);
                    }
                }
            }

            UiAction::ScrollPaneToFraction { block_id, fraction } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let key = BlockKey::new(sk.host_id, sk.session_id, block_id);
                    if let Some(sb) = self.pane_store.scrollbar(&key) {
                        let max_off = sb.total.saturating_sub(sb.len);
                        let row = ((max_off as f32) * fraction.clamp(0.0, 1.0)).round() as usize;
                        let _ = self
                            .pane_store
                            .scroll_viewport(&key, ScrollViewport::Row(row));
                    }
                }
            }
        }
    }

    fn handle_pane_header_button(&mut self, block_id: &str, button: PaneHeaderButton) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let anchor_key = BlockKey::new(
            sk.host_id.clone(),
            sk.session_id.clone(),
            block_id.to_owned(),
        );
        match button {
            PaneHeaderButton::SplitRight => {
                let cwd = self.resolve_shell_cwd_for_host(&sk.host_id, Some(&anchor_key), true);
                let layout = self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
                self.send_command(HostCommand::SplitPane {
                    host_id: sk.host_id,
                    request: SessionNewSplitRequest {
                        session_id: sk.session_id,
                        anchor_id: Some(block_id.to_owned()),
                        window_id: None,
                        direction: "horizontal".to_owned(),
                        side: "after".to_owned(),
                        ratio: None,
                        layout,
                        focus: Some(true),
                    },
                });
            }
            PaneHeaderButton::SplitDown => {
                let cwd = self.resolve_shell_cwd_for_host(&sk.host_id, Some(&anchor_key), true);
                let layout = self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
                self.send_command(HostCommand::SplitPane {
                    host_id: sk.host_id,
                    request: SessionNewSplitRequest {
                        session_id: sk.session_id,
                        anchor_id: Some(block_id.to_owned()),
                        window_id: None,
                        direction: "vertical".to_owned(),
                        side: "after".to_owned(),
                        ratio: None,
                        layout,
                        focus: Some(true),
                    },
                });
            }
            PaneHeaderButton::FloatOrDock => {
                let is_floating = self.is_block_in_floating_layer(&sk, block_id);
                if is_floating {
                    if let Some(tiled_anchor) = self.find_tiled_anchor_block(&sk, block_id) {
                        self.send_command(HostCommand::MoveBlock {
                            host_id: sk.host_id,
                            session_id: sk.session_id,
                            block_id: block_id.to_owned(),
                            anchor_block_id: tiled_anchor,
                            direction: "horizontal".to_owned(),
                            side: "after".to_owned(),
                            ratio: Some(0.5),
                        });
                    } else if let Some((_, layer_id)) = self.find_layer_for_block(block_id) {
                        self.send_command(HostCommand::SetLayerState {
                            host_id: sk.host_id,
                            request: SessionSetLayerStateRequest {
                                session_id: sk.session_id,
                                layer_id,
                                state: "minimized".to_owned(),
                                focus: Some(false),
                            },
                        });
                    }
                } else {
                    self.send_command(HostCommand::FloatBlock {
                        host_id: sk.host_id,
                        request: SessionFloatBlockRequest {
                            session_id: sk.session_id,
                            block_id: block_id.to_owned(),
                            bounds: [0.15, 0.15, 0.70, 0.70],
                        },
                    });
                }
            }
            PaneHeaderButton::Zoom => {
                self.send_command(HostCommand::ZoomBlock {
                    host_id: sk.host_id,
                    request: SessionZoomBlockRequest {
                        session_id: sk.session_id,
                        block_id: Some(block_id.to_owned()),
                        window_id: None,
                        zoom: None,
                    },
                });
            }
            PaneHeaderButton::Close => {
                self.send_command(HostCommand::CloseBlock {
                    host_id: sk.host_id,
                    session_id: sk.session_id,
                    block_id: block_id.to_owned(),
                    undoable: true,
                });
            }
        }
    }

    fn apply_setting_change(&mut self, change: SettingChange) {
        match change {
            SettingChange::ToolbarPosition(pos) => {
                self.ui.layout_cfg.toolbar_position = pos;
                self.settings.sidebar_position = match pos {
                    ToolbarPosition::Left => SidebarPosition::Left,
                    ToolbarPosition::Right => SidebarPosition::Right,
                    ToolbarPosition::Top => SidebarPosition::Top,
                    ToolbarPosition::Bottom => SidebarPosition::Bottom,
                };
                self.force_immediate_resize = true;
            }
            SettingChange::ToolbarBehavior(beh) => {
                self.ui.layout_cfg.toolbar_behavior = beh;
                self.settings.sidebar_behavior = match beh {
                    ToolbarBehavior::Always => SidebarBehavior::Always,
                    ToolbarBehavior::OnHover => SidebarBehavior::OnHover,
                    ToolbarBehavior::Hidden => SidebarBehavior::Hidden,
                };
                self.force_immediate_resize = true;
            }
            SettingChange::PaneDensity(den) => {
                self.ui.layout_cfg.density = den;
                self.settings.density = match den {
                    PaneDensity::Comfortable => LayoutDensity::Comfortable,
                    PaneDensity::Compact => LayoutDensity::Compact,
                };
                self.force_immediate_resize = true;
            }
            SettingChange::PaneHeaderMode(mode) => {
                self.ui.layout_cfg.pane_header_mode = mode;
                self.settings.pane_header_mode = match mode {
                    PaneHeaderMode::Always => SavedPaneHeaderMode::Always,
                    PaneHeaderMode::OnHover => SavedPaneHeaderMode::OnHover,
                    PaneHeaderMode::Hidden => SavedPaneHeaderMode::Hidden,
                };
                self.force_immediate_resize = true;
            }
            SettingChange::ToggleTranslucentMaterial => {
                self.settings.material = match self.settings.material {
                    WindowMaterial::Classic => WindowMaterial::Translucent { alpha: 0.90 },
                    WindowMaterial::Translucent { .. } => WindowMaterial::Classic,
                };
                self.theme.material = self.settings.material.to_theme_material();
            }
            SettingChange::ToggleActivePaneBorder => {
                self.ui.pane_options.highlight_active_border =
                    !self.ui.pane_options.highlight_active_border;
                self.settings.highlight_active_border =
                    self.ui.pane_options.highlight_active_border;
            }
            SettingChange::ToggleFadeInactivePanes => {
                self.ui.pane_options.fade_inactive_panes =
                    !self.ui.pane_options.fade_inactive_panes;
                self.settings.fade_inactive_panes = self.ui.pane_options.fade_inactive_panes;
            }
            SettingChange::ToggleAdaptiveRounding => {
                self.settings.adaptive_rounding = !self.settings.adaptive_rounding;
                self.ui.layout_cfg.hyprland_rounding = if self.settings.adaptive_rounding {
                    Some(self.theme.rounding)
                } else {
                    None
                };
            }
        }

        // Refresh live Settings modal snapshot if open.
        if let Some(modal) = &mut self.ui.modal
            && let ModalContent::Settings { snapshot, .. } = &mut modal.content
        {
            *snapshot = SettingsViewSnapshot::from_config(
                &self.ui.layout_cfg,
                self.theme.material,
                self.ui.pane_options.highlight_active_border,
                self.ui.pane_options.fade_inactive_panes,
            );
        }

        self.save_settings_best_effort();
    }

    fn save_settings_best_effort(&self) {
        if let Err(err) = self.settings.save_to(&self.settings_path) {
            tracing::warn!(path = %self.settings_path.display(), error = %err, "failed to save settings");
        }
    }

    fn open_modal_kind(&mut self, kind: ModalKind) {
        match kind {
            ModalKind::CommandPalette => {
                let items: Vec<PaletteItem> = self
                    .actions
                    .actions()
                    .iter()
                    .map(|a| {
                        let mut item = PaletteItem::new(a.name.clone(), a.title.clone());
                        item.category = (!a.category.is_empty()).then(|| a.category.clone());
                        item.keywords = (!a.keywords.is_empty()).then(|| a.keywords.clone());
                        item.shortcut = a.shortcut_hint.clone();
                        item.icon = Some(if a.is_tr3x {
                            IconId::T3
                        } else {
                            IconId::Terminal
                        });
                        item
                    })
                    .collect();
                self.ui.open_modal(ModalState::open_list(
                    ModalKind::CommandPalette,
                    "Command Palette",
                    "Type a command…",
                    Some("Ctrl+Shift+P".to_owned()),
                    items,
                ));
            }
            ModalKind::SwitchSession => {
                let mut items = Vec::new();
                for host_id in &self.workspace.host_order {
                    let Some(host) = self.workspace.host(host_id) else {
                        continue;
                    };
                    for sid in &host.session_order {
                        if let Some(sess) = host.sessions.get(sid) {
                            let win_count = sess.view.as_ref().map_or(0, |v| v.windows.len());
                            let mut item = PaletteItem::new(
                                format!("__select_session:{host_id}|{sid}"),
                                sess.label.clone(),
                            );
                            item.category = Some(host.label.clone());
                            item.detail = Some(format!("{win_count} tabs"));
                            item.icon = Some(if is_t3_session(&sess.label) {
                                IconId::T3
                            } else {
                                IconId::Server
                            });
                            items.push(item);
                        }
                    }
                }
                self.ui.open_modal(ModalState::open_list(
                    ModalKind::SwitchSession,
                    "Switch Session",
                    "Filter sessions…",
                    Some("Ctrl+Shift+K".to_owned()),
                    items,
                ));
            }
            ModalKind::GoToAnything | ModalKind::ActivityPicker => {
                let mut items = Vec::new();
                for host_id in &self.workspace.host_order {
                    let Some(host) = self.workspace.host(host_id) else {
                        continue;
                    };
                    for sid in &host.session_order {
                        let Some(sess) = host.sessions.get(sid) else {
                            continue;
                        };
                        if let Some(view) = &sess.view {
                            for win in &view.windows {
                                if let Some(wid) = &win.window_id {
                                    let mut item = PaletteItem::new(
                                        format!("__select_tab:{host_id}|{sid}|{wid}"),
                                        if win.label.is_empty() {
                                            "Untitled Tab".to_owned()
                                        } else {
                                            win.label.clone()
                                        },
                                    );
                                    item.category = Some(sess.label.clone());
                                    item.subtitle = Some(host.label.clone());
                                    item.icon = Some(IconId::Terminal);
                                    items.push(item);
                                }
                            }
                        }
                    }
                }
                let title = if kind == ModalKind::ActivityPicker {
                    "Activity"
                } else {
                    "Go to Anything"
                };
                self.ui.open_modal(ModalState::open_list(
                    kind,
                    title,
                    "Jump to tab, session, or pane…",
                    Some("Ctrl+Alt+P".to_owned()),
                    items,
                ));
            }
            ModalKind::Settings => {
                let snap = SettingsViewSnapshot::from_config(
                    &self.ui.layout_cfg,
                    self.theme.material,
                    self.ui.pane_options.highlight_active_border,
                    self.ui.pane_options.fade_inactive_panes,
                );
                self.ui.open_modal(ModalState::open_settings(snap));
            }
            ModalKind::Rename => {
                let (target_id, title, current) = match self.pending_rename_target.take() {
                    Some(PendingRenameTarget::Host { host_id }) => {
                        let cur = self.host_display_label(&host_id);
                        (format!("__rename_host:{host_id}"), "Edit Host", cur)
                    }
                    Some(PendingRenameTarget::Window {
                        host_id,
                        session_id,
                        window_id,
                    }) => {
                        let cur = self
                            .workspace
                            .session(&SessionKey::new(&host_id, &session_id))
                            .and_then(|s| s.view.as_ref())
                            .and_then(|v| {
                                v.windows
                                    .iter()
                                    .find(|w| w.window_id.as_deref() == Some(window_id.as_str()))
                            })
                            .map(|w| w.label.clone())
                            .unwrap_or_default();
                        (
                            format!("__rename_window:{host_id}|{session_id}|{window_id}"),
                            "Rename Tab",
                            cur,
                        )
                    }
                    Some(PendingRenameTarget::Block {
                        host_id,
                        session_id,
                        block_id,
                    }) => {
                        let cur = self
                            .workspace
                            .session(&SessionKey::new(&host_id, &session_id))
                            .and_then(|s| s.view.as_ref())
                            .and_then(|v| {
                                v.windows
                                    .iter()
                                    .flat_map(|w| w.layers.iter())
                                    .flat_map(|l| l.blocks.iter())
                                    .find(|b| b.block_id.as_deref() == Some(block_id.as_str()))
                            })
                            .map(|b| b.label.clone())
                            .unwrap_or_default();
                        (
                            format!("__rename_block:{host_id}|{session_id}|{block_id}"),
                            "Rename Pane",
                            cur,
                        )
                    }
                    Some(PendingRenameTarget::Session {
                        host_id,
                        session_id,
                    }) => {
                        let cur = self
                            .workspace
                            .session(&SessionKey::new(&host_id, &session_id))
                            .map(|s| s.label.clone())
                            .unwrap_or_default();
                        (
                            format!("__rename_session:{host_id}|{session_id}"),
                            "Rename Session",
                            cur,
                        )
                    }
                    None => self
                        .workspace
                        .active_session
                        .as_ref()
                        .and_then(|sk| self.workspace.session(sk).map(|s| (sk, s)))
                        .map(|(sk, s)| {
                            (
                                format!("__rename_session:{}|{}", sk.host_id, sk.session_id),
                                "Rename Session",
                                s.label.clone(),
                            )
                        })
                        .unwrap_or_else(|| (String::new(), "Rename Session", String::new())),
                };
                self.ui
                    .open_modal(ModalState::open_rename(target_id, title, current));
            }
            ModalKind::AddHost => {
                self.ui.open_modal(ModalState::open_add_host());
            }
            ModalKind::ShortcutsHelp => {
                self.ui.open_modal(ModalState::open_shortcuts_help());
            }
            ModalKind::RexPrompt => {}
        }
    }

    fn dispatch_command_id(
        &mut self,
        action_id: &str,
        args: Option<serde_json::Value>,
        now: Instant,
    ) {
        // Internal modal jump tokens
        if let Some(rest) = action_id.strip_prefix("__select_session:")
            && let Some((host_id, session_id)) = rest.split_once('|')
        {
            self.dispatch_ui_action(
                UiAction::SelectSession {
                    host_id: host_id.to_owned(),
                    session_id: session_id.to_owned(),
                },
                now,
            );
            return;
        }
        if let Some(rest) = action_id.strip_prefix("__select_tab:") {
            let mut parts = rest.splitn(3, '|');
            if let (Some(h), Some(s), Some(w)) = (parts.next(), parts.next(), parts.next()) {
                self.dispatch_ui_action(
                    UiAction::SelectTab {
                        host_id: h.to_owned(),
                        session_id: s.to_owned(),
                        window_id: w.to_owned(),
                    },
                    now,
                );
                return;
            }
        }

        let host_id = self
            .workspace
            .active_session
            .as_ref()
            .map(|s| s.host_id.clone())
            .unwrap_or_else(|| LOCAL_HOST_ID.to_owned());

        // Check if registered as a server Lua/block action first.
        if let Some(entry) = self.actions.get(action_id)
            && entry.kind != ActionKind::Client
        {
            if action_id == "t3_fit" {
                self.fit_active_session_to_device();
            }
            self.send_server_action(&host_id, action_id, args);
            return;
        }

        let fallback_sid = self
            .workspace
            .active_session
            .as_ref()
            .map(|s| s.session_id.as_str());
        let fallback_bid = self.ui.focus.last_terminal_block();
        match parse_client_command(action_id, args.as_ref(), fallback_sid, fallback_bid) {
            Ok(cmd) => self.execute_client_command(&host_id, cmd, now),
            Err(_) => self.send_server_action(&host_id, action_id, args),
        }
    }

    fn send_server_action(&mut self, host_id: &str, action: &str, args: Option<serde_json::Value>) {
        self.send_server_action_for_session(host_id, None, action, args);
    }

    fn send_server_action_for_session(
        &mut self,
        host_id: &str,
        target_session_id: Option<&str>,
        action: &str,
        args: Option<serde_json::Value>,
    ) {
        let session_id = target_session_id.map(str::to_owned).or_else(|| {
            self.workspace
                .active_session
                .as_ref()
                .filter(|s| s.host_id == host_id)
                .map(|s| s.session_id.clone())
        });
        let block_id = self
            .focused_block_key()
            .filter(|k| {
                k.host_id == host_id && session_id.as_deref().is_none_or(|sid| k.session_id == sid)
            })
            .map(|k| k.block_id);
        self.send_command(HostCommand::ClientAction {
            host_id: host_id.to_owned(),
            request: ClientActionRequest {
                action: action.to_owned(),
                session_id,
                block_id,
                args,
                origin: None,
                servers: None,
                server: None,
            },
        });
    }

    fn fit_active_session_to_device(&mut self) {
        if self.read_only {
            return;
        }
        if let Some(sk) = self.workspace.active_session.clone() {
            self.send_command(HostCommand::OwnBlock {
                host_id: sk.host_id,
                session_id: sk.session_id,
                block_ids: None,
                mode: OwnMode::Take,
            });
            self.push_theme_to_owned_blocks();
        }
    }

    fn execute_client_command(&mut self, host_id: &str, cmd: ClientCommand, now: Instant) {
        match cmd {
            ClientCommand::SelectSession {
                session_id,
                window_id,
                block_id,
            } => {
                if let Some(sid) = session_id {
                    let sk = SessionKey::new(host_id.to_owned(), sid.clone());
                    self.activate_session(sk, &HashMap::new());
                    if let Some(wid) = window_id {
                        self.dispatch_ui_action(
                            UiAction::SelectTab {
                                host_id: host_id.to_owned(),
                                session_id: sid.clone(),
                                window_id: wid,
                            },
                            now,
                        );
                    }
                    if let Some(bid) = block_id {
                        self.dispatch_ui_action(UiAction::FocusPane { block_id: bid }, now);
                    }
                } else if let Some(bid) = block_id {
                    self.dispatch_ui_action(UiAction::FocusPane { block_id: bid }, now);
                }
            }
            ClientCommand::Paste { block_id, .. } => {
                self.dispatch_ui_action(UiAction::PasteClipboard { block_id }, now);
            }
            ClientCommand::CopySelection => {
                self.dispatch_ui_action(UiAction::CopySelection { block_id: None }, now);
            }
            ClientCommand::TogglePalette => {
                if matches!(
                    self.ui.focus.current(),
                    FocusTarget::Modal(ModalKind::CommandPalette)
                ) {
                    self.ui.close_modal();
                } else {
                    self.open_modal_kind(ModalKind::CommandPalette);
                }
            }
            ClientCommand::OpenSessionSwitcher => self.open_modal_kind(ModalKind::SwitchSession),
            ClientCommand::OpenGoTo => self.open_modal_kind(ModalKind::GoToAnything),
            ClientCommand::OpenActivity => self.open_modal_kind(ModalKind::ActivityPicker),
            ClientCommand::OpenFind => self.ui.open_find_bar(),
            ClientCommand::FindNext => self.dispatch_ui_action(UiAction::FindNext, now),
            ClientCommand::FindPrevious => self.dispatch_ui_action(UiAction::FindPrev, now),
            ClientCommand::OpenSettings => self.open_modal_kind(ModalKind::Settings),
            ClientCommand::OpenDiagnostics => {
                let sessions = self
                    .workspace
                    .hosts
                    .values()
                    .map(|h| h.sessions.len())
                    .sum::<usize>();
                self.push_toast(
                    ToastSeverity::Info,
                    "omarex diagnostics",
                    format!(
                        "Hosts: {} · Sessions: {} · Panes: {} · Theme: {}",
                        self.workspace.hosts.len(),
                        sessions,
                        self.pane_store.len(),
                        if self.theme.is_dark { "dark" } else { "light" }
                    ),
                );
            }
            ClientCommand::ToggleSidebar => {
                self.ui.layout_cfg.sidebar_collapsed = !self.ui.layout_cfg.sidebar_collapsed;
                self.force_immediate_resize = true;
            }
            ClientCommand::ToggleTabStyle => {
                let next = match self.ui.layout_cfg.toolbar_position {
                    ToolbarPosition::Left => ToolbarPosition::Top,
                    ToolbarPosition::Top => ToolbarPosition::Right,
                    ToolbarPosition::Right => ToolbarPosition::Bottom,
                    ToolbarPosition::Bottom => ToolbarPosition::Left,
                };
                self.apply_setting_change(SettingChange::ToolbarPosition(next));
            }
            ClientCommand::FocusSidebarFilter => {
                self.ui.sidebar.open_filter();
                self.ui.focus.focus_sidebar_filter();
            }
            ClientCommand::RevealToolbar => {
                self.ui.layout_cfg.toolbar_revealed = !self.ui.layout_cfg.toolbar_revealed;
            }
            ClientCommand::RevealPaneHeader => {
                let next = match self.ui.layout_cfg.pane_header_mode {
                    PaneHeaderMode::Always => PaneHeaderMode::OnHover,
                    PaneHeaderMode::OnHover | PaneHeaderMode::Hidden => PaneHeaderMode::Always,
                };
                self.apply_setting_change(SettingChange::PaneHeaderMode(next));
            }
            ClientCommand::DismissNotice => {
                if let Some(dismissed) = self.ui.toasts.pop() {
                    self.toast_deadlines.remove(&dismissed.id);
                }
            }
            ClientCommand::RefreshSessions => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    self.send_command(HostCommand::FetchView {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                    });
                }
            }
            ClientCommand::NavigateBack => {
                if self.nav_index > 0 {
                    self.nav_index -= 1;
                    if let Some((h, s, b)) = self.nav_history.get(self.nav_index).cloned() {
                        self.activate_session(SessionKey::new(h, s), &HashMap::new());
                        if let Some(bid) = b {
                            self.ui.focus.focus_terminal(Some(bid));
                        }
                    }
                }
            }
            ClientCommand::NavigateForward => {
                if self.nav_index + 1 < self.nav_history.len() {
                    self.nav_index += 1;
                    if let Some((h, s, b)) = self.nav_history.get(self.nav_index).cloned() {
                        self.activate_session(SessionKey::new(h, s), &HashMap::new());
                        if let Some(bid) = b {
                            self.ui.focus.focus_terminal(Some(bid));
                        }
                    }
                }
            }
            ClientCommand::NewSession { .. } | ClientCommand::NewWindow => {
                self.dispatch_ui_action(
                    UiAction::NewSession {
                        host_id: Some(host_id.to_owned()),
                    },
                    now,
                );
            }
            ClientCommand::NewTab => {
                self.dispatch_ui_action(UiAction::NewTerminal, now);
            }
            ClientCommand::CloseCurrent | ClientCommand::ClosePane => {
                if let Some(key) = self.focused_block_key() {
                    self.send_command(HostCommand::CloseBlock {
                        host_id: key.host_id,
                        session_id: key.session_id,
                        block_id: key.block_id,
                        undoable: true,
                    });
                }
            }
            ClientCommand::CloseTab => {
                if let Some(sk) = self.workspace.active_session.clone()
                    && let Some(wid) = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .and_then(|v| v.active_window_id.clone())
                {
                    self.dispatch_ui_action(
                        UiAction::CloseTab {
                            host_id: sk.host_id,
                            session_id: sk.session_id,
                            window_id: wid,
                        },
                        now,
                    );
                }
            }
            ClientCommand::UndoClose => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    self.send_command(HostCommand::UndoClose {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                    });
                }
            }
            ClientCommand::NextTab => self.cycle_active_tab(1, now),
            ClientCommand::PrevTab => self.cycle_active_tab(-1, now),
            ClientCommand::GotoTab { index } => self.goto_tab_index(index, now),
            ClientCommand::MoveTab { forward } => self.move_active_tab(forward),
            ClientCommand::MarkTabUnread => {
                if let Some(sk) = self.workspace.active_session.clone()
                    && let Some(wid) = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .and_then(|v| v.active_window_id.clone())
                {
                    self.dispatch_ui_action(UiAction::MarkTabUnread { window_id: wid }, now);
                }
            }
            ClientCommand::ToggleTabMute => {
                if let Some(key) = self.focused_block_key() {
                    self.settings.toggle_mute_block(&key.host_id, &key.block_id);
                    self.save_settings_best_effort();
                }
            }
            ClientCommand::SplitPane(dir) => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let (direction, side) = match dir {
                        SplitDirection::Left => ("horizontal", "before"),
                        SplitDirection::Right | SplitDirection::Auto => ("horizontal", "after"),
                        SplitDirection::Up => ("vertical", "before"),
                        SplitDirection::Down => ("vertical", "after"),
                    };
                    let anchor_id = self.ui.focus.last_terminal_block().map(str::to_owned);
                    let anchor_key = anchor_id
                        .as_deref()
                        .map(|bid| BlockKey::new(&sk.host_id, &sk.session_id, bid));
                    let cwd =
                        self.resolve_shell_cwd_for_host(&sk.host_id, anchor_key.as_ref(), true);
                    let layout = self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
                    self.send_command(HostCommand::SplitPane {
                        host_id: sk.host_id,
                        request: SessionNewSplitRequest {
                            session_id: sk.session_id,
                            anchor_id,
                            window_id: None,
                            direction: direction.to_owned(),
                            side: side.to_owned(),
                            ratio: None,
                            layout,
                            focus: Some(true),
                        },
                    });
                }
            }
            ClientCommand::ZoomPane => {
                if let Some(key) = self.focused_block_key() {
                    self.handle_pane_header_button(&key.block_id, PaneHeaderButton::Zoom);
                }
            }
            ClientCommand::FloatPane => {
                if let Some(key) = self.focused_block_key() {
                    self.handle_pane_header_button(&key.block_id, PaneHeaderButton::FloatOrDock);
                }
            }
            ClientCommand::MinimizeFloating => {
                if let Some(key) = self.focused_block_key()
                    && let Some((sk, layer_id)) = self.find_layer_for_block(&key.block_id)
                {
                    self.send_command(HostCommand::SetLayerState {
                        host_id: sk.host_id,
                        request: SessionSetLayerStateRequest {
                            session_id: sk.session_id,
                            layer_id,
                            state: "minimized".to_owned(),
                            focus: Some(false),
                        },
                    });
                }
            }
            ClientCommand::MinimizeAllFloating => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let layer_ids: Vec<String> =
                        self.workspace
                            .session(&sk)
                            .and_then(|s| s.view.as_ref())
                            .and_then(active_window_of_view)
                            .map(|win| {
                                win.layers
                                    .iter()
                                    .filter(|l| {
                                        l.kind.eq_ignore_ascii_case("floating")
                                            && !l.state.as_deref().is_some_and(|s| {
                                                s.eq_ignore_ascii_case("minimized")
                                            })
                                    })
                                    .filter_map(|l| l.layer_id.clone())
                                    .collect()
                            })
                            .unwrap_or_default();
                    for layer_id in layer_ids {
                        self.send_command(HostCommand::SetLayerState {
                            host_id: sk.host_id.clone(),
                            request: SessionSetLayerStateRequest {
                                session_id: sk.session_id.clone(),
                                layer_id,
                                state: "minimized".to_owned(),
                                focus: Some(false),
                            },
                        });
                    }
                }
            }
            ClientCommand::MovePane(dir) => {
                self.move_focused_pane(dir);
            }
            ClientCommand::BalancePanes => {
                self.send_server_action(host_id, "pane.balance", None);
            }
            ClientCommand::FontIncrease => {
                self.settings.increase_font();
                self.save_settings_best_effort();
            }
            ClientCommand::FontDecrease => {
                self.settings.decrease_font();
                self.save_settings_best_effort();
            }
            ClientCommand::FontReset => {
                self.settings.reset_font();
                self.save_settings_best_effort();
            }
            ClientCommand::ReloadConfig => {
                self.send_command(HostCommand::ReloadConfig {
                    host_id: host_id.to_owned(),
                });
            }
            ClientCommand::FitToDevice => {
                self.fit_active_session_to_device();
            }
            ClientCommand::PromptJump { next } => {
                if let Some(key) = self.focused_block_key() {
                    let delta = if next { 1 } else { -1 };
                    let _ = self
                        .pane_store
                        .scroll_viewport(&key, ScrollViewport::DeltaPrompt(delta));
                }
            }
            ClientCommand::FocusPane(dir) => {
                self.focus_adjacent_pane(dir, now);
            }
            ClientCommand::FocusNextPane => self.cycle_pane_focus(1, now),
            ClientCommand::FocusPrevPane => self.cycle_pane_focus(-1, now),
            ClientCommand::EnterKeyMode { mode } => {
                let hints = self.actions.keymap_mut().enter_mode(&mode);
                self.ui.which_key = Some(WhichKeyState {
                    header: format!("MODE: {}", mode.to_uppercase()),
                    exclusive: false,
                    hints,
                });
            }
            ClientCommand::ExitKeyMode => {
                self.actions.keymap_mut().exit_mode();
                self.refresh_which_key_overlay();
            }
            ClientCommand::ExitAllKeyModes => {
                self.actions.keymap_mut().exit_all_modes();
                self.ui.which_key = None;
            }
            ClientCommand::ServerAction { action, args } => {
                self.send_server_action(host_id, &action, args);
            }
            _ => {}
        }
    }

    fn cycle_active_tab(&mut self, delta: isize, now: Instant) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let target_wid = self
            .workspace
            .session(&sk)
            .and_then(|s| s.view.as_ref())
            .and_then(|view| {
                let ids: Vec<&str> = view
                    .windows
                    .iter()
                    .filter_map(|w| w.window_id.as_deref())
                    .collect();
                if ids.len() <= 1 {
                    return None;
                }
                let cur = view
                    .active_window_id
                    .as_deref()
                    .and_then(|id| ids.iter().position(|&w| w == id))
                    .unwrap_or(0);
                let next = (cur as isize + delta).rem_euclid(ids.len() as isize) as usize;
                Some(ids[next].to_owned())
            });
        if let Some(window_id) = target_wid {
            self.dispatch_ui_action(
                UiAction::SelectTab {
                    host_id: sk.host_id,
                    session_id: sk.session_id,
                    window_id,
                },
                now,
            );
        }
    }

    fn goto_tab_index(&mut self, index: i32, now: Instant) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let target_wid = self
            .workspace
            .session(&sk)
            .and_then(|s| s.view.as_ref())
            .and_then(|view| {
                if view.windows.is_empty() {
                    return None;
                }
                let idx = if index < 0 {
                    view.windows.len() - 1
                } else {
                    (index as usize)
                        .saturating_sub(1)
                        .min(view.windows.len() - 1)
                };
                view.windows[idx].window_id.clone()
            });
        if let Some(window_id) = target_wid {
            self.dispatch_ui_action(
                UiAction::SelectTab {
                    host_id: sk.host_id,
                    session_id: sk.session_id,
                    window_id,
                },
                now,
            );
        }
    }

    fn focus_adjacent_pane(&mut self, dir: PaneDirection, now: Instant) {
        let Some(out) = &self.last_ui_output else {
            return;
        };
        if out.tiled_panes.len() <= 1 {
            return;
        }
        let cur_id = self.ui.focus.last_terminal_block();
        let Some(cur) = out
            .tiled_panes
            .iter()
            .find(|p| Some(p.id.as_str()) == cur_id)
        else {
            return;
        };
        let (cx, cy) = cur.card_rect.center();
        let mut best: Option<(f32, String)> = None;
        for cand in &out.tiled_panes {
            if cand.id == cur.id {
                continue;
            }
            let (tx, ty) = cand.card_rect.center();
            let valid = match dir {
                PaneDirection::Left => tx < cx - 4.0,
                PaneDirection::Right => tx > cx + 4.0,
                PaneDirection::Up => ty < cy - 4.0,
                PaneDirection::Down => ty > cy + 4.0,
            };
            if !valid {
                continue;
            }
            let dist = (tx - cx).hypot(ty - cy);
            if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                best = Some((dist, cand.id.clone()));
            }
        }
        if let Some((_, next_id)) = best {
            self.dispatch_ui_action(UiAction::FocusPane { block_id: next_id }, now);
        }
    }

    fn cycle_pane_focus(&mut self, delta: isize, now: Instant) {
        let next_id = self.last_ui_output.as_ref().and_then(|out| {
            if out.tiled_panes.len() <= 1 {
                return None;
            }
            let cur = self
                .ui
                .focus
                .last_terminal_block()
                .and_then(|id| out.tiled_panes.iter().position(|p| p.id == id))
                .unwrap_or(0);
            let next = (cur as isize + delta).rem_euclid(out.tiled_panes.len() as isize) as usize;
            Some(out.tiled_panes[next].id.clone())
        });
        if let Some(block_id) = next_id {
            self.dispatch_ui_action(UiAction::FocusPane { block_id }, now);
        }
    }

    fn refresh_which_key_overlay(&mut self) {
        let mode = self.actions.keymap().active_mode().map(str::to_owned);
        let pending = self.actions.keymap().pending_sequence().to_vec();
        if mode.is_none() && pending.is_empty() {
            self.ui.which_key = None;
            return;
        }
        let hints = self.actions.keymap().hints_for(mode.as_deref(), &pending);
        let header = if let Some(m) = mode {
            format!("MODE: {}", m.to_uppercase())
        } else {
            let trail: Vec<String> = pending.iter().map(KeyChord::to_display_string).collect();
            format!("{} ›", trail.join(" › "))
        };
        self.ui.which_key = Some(WhichKeyState {
            header,
            exclusive: false,
            hints,
        });
    }

    // =========================================================================
    // 4. Input Entrypoints for `app.rs` (`winit`)
    // =========================================================================

    /// Routes a normalized keyboard event honoring [`FocusTarget`] (modals/find/filter first,
    /// then `KeymapEngine`, then `libghostty-vt` key encoding to PTY).
    pub fn handle_key(&mut self, input: &KeyInputEvent, now: Instant) {
        self.needs_redraw = true;

        // Overlays handle press events only; releases never leak to underlying PTY.
        if self.ui.focus.is_overlay_focused() {
            if !input.pressed {
                return;
            }
            match self.ui.focus.current().clone() {
                FocusTarget::Modal(_) => {
                    if let Some(mkey) = key_input_to_modal_key(input) {
                        let action = self.ui.handle_modal_key(mkey);
                        self.dispatch_ui_action(action, now);
                    }
                }
                FocusTarget::FindBar => self.handle_find_bar_key(input, now),
                FocusTarget::SidebarFilter => self.handle_sidebar_filter_key(input),
                FocusTarget::ContextMenu => match input.canonical_key.as_str() {
                    "escape" => {
                        self.ui.close_context_menu();
                    }
                    "up" | "down" => {
                        if let Some(menu) = self.ui.context_menu.as_mut()
                            && !menu.items.is_empty()
                        {
                            let len = menu.items.len();
                            let next = match (menu.hovered_index, input.canonical_key.as_str()) {
                                (None, "down") => 0,
                                (None, _) => len - 1,
                                (Some(i), "down") => (i + 1) % len,
                                (Some(i), _) => (i + len - 1) % len,
                            };
                            menu.hovered_index = Some(next);
                        }
                    }
                    "enter" | "space" => {
                        let action = self
                            .ui
                            .context_menu
                            .as_ref()
                            .and_then(|m| m.hovered_index.and_then(|idx| m.activate_index(idx)))
                            .unwrap_or(UiAction::None);
                        self.ui.close_context_menu();
                        self.dispatch_ui_action(action, now);
                    }
                    _ => {}
                },
                FocusTarget::Terminal(_) => {}
            }
            return;
        }

        // Keymap resolution applies on key press (non-composing).
        if input.pressed && !input.composing {
            let chord = KeyChord::new(input.mods, &input.canonical_key);
            match self.actions.keymap_mut().resolve_key(chord) {
                KeyResolution::Action { action, args, .. } => {
                    self.ui.which_key = None;
                    self.dispatch_command_id(&action, args, now);
                    return;
                }
                KeyResolution::ForwardToServer { prefix, trigger } => {
                    self.ui.which_key = None;
                    if let Some(sk) = self.workspace.active_session.clone() {
                        let req = build_client_key_request(
                            sk.session_id,
                            self.ui.focus.last_terminal_block().map(str::to_owned),
                            &prefix,
                            &trigger,
                        );
                        self.send_command(HostCommand::ClientKey {
                            host_id: sk.host_id,
                            request: req,
                        });
                    }
                    return;
                }
                KeyResolution::PendingSequence { prefix, hints } => {
                    let trail: Vec<String> =
                        prefix.iter().map(KeyChord::to_display_string).collect();
                    self.ui.which_key = Some(WhichKeyState {
                        header: format!("{} ›", trail.join(" › ")),
                        exclusive: false,
                        hints,
                    });
                    return;
                }
                KeyResolution::ModeEntered { mode, hints } => {
                    self.ui.which_key = Some(WhichKeyState {
                        header: format!("MODE: {}", mode.to_uppercase()),
                        exclusive: false,
                        hints,
                    });
                    return;
                }
                KeyResolution::ModeExited => {
                    self.refresh_which_key_overlay();
                    return;
                }
                KeyResolution::Blocked => {
                    return;
                }
                KeyResolution::PassThrough => {
                    self.refresh_which_key_overlay();
                }
            }
        }

        // Pass directly through to focused terminal via `PaneStore::encode_key`!
        let Some(key) = self.focused_block_key() else {
            return;
        };
        if input.pressed {
            self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
            let _ = self.pane_store.scroll_to_bottom(&key);
        }

        let vt_action = if !input.pressed {
            VtKeyAction::Release
        } else if input.repeat {
            VtKeyAction::Repeat
        } else {
            VtKeyAction::Press
        };
        let unshifted_char = char::from_u32(input.unshifted_codepoint).filter(|&c| c != '\0');
        let consumed_mods = infer_consumed_mods(input.mods, input.text.as_deref(), unshifted_char);
        let vt_input = VtKeyEventInput {
            action: vt_action,
            key: input.physical_key,
            mods: bridge_mods_to_vt(input.mods),
            consumed_mods,
            composing: input.composing,
            utf8: input.text.as_deref(),
            unshifted_codepoint: unshifted_char,
        };
        if let Ok(bytes) = self.pane_store.encode_key(&key, &vt_input)
            && !bytes.is_empty()
            && let Ok(data) = String::from_utf8(bytes)
        {
            self.send_command(HostCommand::TerminalWrite {
                host_id: key.host_id,
                session_id: key.session_id,
                block_id: key.block_id,
                data,
            });
        }
    }

    fn handle_find_bar_key(&mut self, input: &KeyInputEvent, now: Instant) {
        match input.canonical_key.as_str() {
            "escape" => {
                self.ui.close_find_bar();
                self.dispatch_ui_action(UiAction::CloseFindBar, now);
                return;
            }
            "enter" => {
                let act = if input.mods.shift {
                    UiAction::FindPrev
                } else {
                    UiAction::FindNext
                };
                self.dispatch_ui_action(act, now);
                return;
            }
            _ => {}
        }
        if apply_key_to_text_input(&mut self.ui.find_bar.input, input) {
            let query = self.ui.find_bar.input.text().to_owned();
            self.dispatch_ui_action(UiAction::FindQueryChanged(query), now);
        }
    }

    fn handle_sidebar_filter_key(&mut self, input: &KeyInputEvent) {
        if input.canonical_key == "escape" {
            self.ui.sidebar.close_filter();
            self.ui.focus.restore_terminal();
            return;
        }
        let _ = apply_key_to_text_input(&mut self.ui.sidebar.filter_input, input);
    }

    /// Handles pointer motion (hover tracking, active drag updates, terminal mouse motion / selection,
    /// and cell-debounced file-path hover detection).
    pub fn handle_pointer_move(&mut self, px: f32, py: f32, mods: Modifiers, _now: Instant) {
        self.pointer_pos = (px, py);

        // 1. Active drag takes priority
        if let Some(drag) = self.active_drag.clone() {
            self.needs_redraw = true;
            match drag {
                ActiveDrag::SidebarResize { on_right } => {
                    let raw = if on_right { self.viewport_w - px } else { px };
                    self.ui.layout_cfg.sidebar_width = raw.clamp(
                        ChromeLayoutConfig::MIN_SIDEBAR_WIDTH,
                        ChromeLayoutConfig::MAX_SIDEBAR_WIDTH,
                    );
                    self.settings.sidebar_width_px = self.ui.layout_cfg.sidebar_width;
                }
                ActiveDrag::SplitDivider {
                    host_id,
                    session_id,
                    divider,
                    split_target,
                } => {
                    let ratio = compute_drag_ratio(&divider, px, py);
                    if let Some((direction, before_block_id, after_block_id)) = split_target {
                        self.send_command(HostCommand::ResizePane {
                            host_id,
                            request: SessionResizePaneRequest {
                                session_id,
                                direction,
                                before_block_id,
                                after_block_id,
                                ratio: f64::from(ratio),
                            },
                        });
                    }
                }
                ActiveDrag::FloatingLayer {
                    host_id,
                    session_id,
                    layer_id,
                    zone,
                    start_px,
                    start_bounds,
                    workspace,
                    ..
                } => {
                    let next = compute_floating_drag_bounds(
                        start_bounds,
                        zone,
                        (px - start_px.0) / workspace.w.max(1.0),
                        (py - start_px.1) / workspace.h.max(1.0),
                    );
                    self.send_command(HostCommand::ResizeLayer {
                        host_id,
                        request: SessionResizeLayerRequest {
                            session_id,
                            layer_id,
                            bounds: [
                                f64::from(next[0]),
                                f64::from(next[1]),
                                f64::from(next[2]),
                                f64::from(next[3]),
                            ],
                        },
                    });
                }
                ActiveDrag::TerminalSelection {
                    key,
                    content_rect,
                    cell_w,
                    cell_h,
                    rectangle,
                } => {
                    let rel_x = (px - content_rect.x).max(0.0);
                    let rel_y = (py - content_rect.y).max(0.0);
                    let col = (rel_x / cell_w).floor() as u16;
                    let row = (rel_y / cell_h).floor() as u32;
                    let geom = SelectionGeometry {
                        columns: (content_rect.w / cell_w).floor().max(1.0) as u32,
                        cell_width: cell_w.round().max(1.0) as u32,
                        padding_left: 0,
                        screen_height: content_rect.h.round().max(1.0) as u32,
                    };
                    let _ = self.pane_store.selection_drag(
                        &key,
                        col,
                        row,
                        f64::from(rel_x),
                        f64::from(rel_y),
                        geom,
                        rectangle || mods.alt,
                    );
                }
                ActiveDrag::PaneScrollbar { key, track_rect } => {
                    let frac = ((py - track_rect.y) / track_rect.h.max(1.0)).clamp(0.0, 1.0);
                    if let Some(sb) = self.pane_store.scrollbar(&key) {
                        let max_off = sb.total.saturating_sub(sb.len);
                        let row = ((max_off as f32) * frac).round() as usize;
                        let _ = self
                            .pane_store
                            .scroll_viewport(&key, ScrollViewport::Row(row));
                    }
                }
            }
            return;
        }

        // 2. Update unified chrome hover state + on-hover toolbar reveal
        if let Some(out) = &self.last_ui_output {
            if self.ui.update_pointer_hover(&out.draw_list, px, py) {
                self.needs_redraw = true;
            }

            if self.ui.layout_cfg.toolbar_behavior == ToolbarBehavior::OnHover {
                let on_edge = out.chrome.hot_edge.is_some_and(|r| r.contains(px, py));
                let on_toolbar = out
                    .chrome
                    .sidebar
                    .or(out.chrome.tab_bar)
                    .is_some_and(|r| r.contains(px, py));
                let should_reveal = on_edge || (self.ui.layout_cfg.toolbar_revealed && on_toolbar);
                if should_reveal != self.ui.layout_cfg.toolbar_revealed {
                    self.ui.layout_cfg.toolbar_revealed = should_reveal;
                    self.needs_redraw = true;
                }
            }

            // 3. Forward mouse motion to mouse-tracking terminal if pointer is inside a pane
            if let Some((key, content_rect, col, row, rel_x, rel_y)) =
                self.pane_cell_at_pointer(px, py)
            {
                let metrics = self.last_cell_metrics.unwrap_or(CellMetrics {
                    font_size: 14.0,
                    scale_factor: 1.0,
                    cell_width: 8.0,
                    cell_height: 19.0,
                    baseline_y: 14.0,
                    underline_y: 16.0,
                    underline_thickness: 1.0,
                    strikethrough_y: 10.0,
                });
                let geom = MouseGeometry {
                    screen_width: content_rect.w.round().max(1.0) as u32,
                    screen_height: content_rect.h.round().max(1.0) as u32,
                    cell_width: metrics.cell_width.round().max(1.0) as u32,
                    cell_height: metrics.cell_height.round().max(1.0) as u32,
                    padding_top: 0,
                    padding_bottom: 0,
                    padding_left: 0,
                    padding_right: 0,
                };
                let mouse_in = MouseEventInput {
                    action: MouseAction::Motion,
                    button: None,
                    mods: bridge_mods_to_vt(mods),
                    x: rel_x,
                    y: rel_y,
                };
                if let Ok(Some(bytes)) = self.pane_store.encode_mouse(&key, &mouse_in, geom)
                    && let Ok(data) = String::from_utf8(bytes)
                {
                    self.send_command(HostCommand::TerminalWrite {
                        host_id: key.host_id,
                        session_id: key.session_id,
                        block_id: key.block_id,
                        data,
                    });
                    return;
                }

                // Cell-debounced hover file-path detection (`orex` P3 fix: only runs on cell change!).
                let cell_id = (key.clone(), col, row);
                if self.hovered_path_cell.as_ref() != Some(&cell_id) {
                    self.hovered_path_cell = Some(cell_id);
                    self.update_hovered_file_path(&key, col, row, px, py);
                }
            } else {
                self.hovered_path_cell = None;
                self.hovered_terminal_target = None;
                self.pending_hover_image = None;
                if self.ui.hover_preview.take().is_some() {
                    self.needs_redraw = true;
                }
            }
        }
    }

    fn resolve_terminal_target(
        &self,
        key: &BlockKey,
        col: u16,
        row: u32,
    ) -> Option<HoveredTerminalTarget> {
        let is_local = self.is_block_local(key);
        let block_pwd = self
            .workspace
            .block(key)
            .and_then(|b| b.pwd.as_deref())
            .and_then(normalize_osc7_pwd)
            .or_else(|| {
                self.pane_store
                    .pwd(key)
                    .as_deref()
                    .and_then(normalize_osc7_pwd)
            });

        // 1. Explicit OSC 8 terminal hyperlink always takes priority.
        if let Ok(Some(uri)) = self.pane_store.hyperlink_at(key, col, row) {
            let trimmed = uri.trim();
            if trimmed.starts_with("http://")
                || trimmed.starts_with("https://")
                || trimmed.starts_with("mailto:")
            {
                return Some(HoveredTerminalTarget::Url(trimmed.to_owned()));
            }
            if let Some(local_from_uri) = normalize_osc7_pwd(trimmed) {
                if is_local {
                    let p = PathBuf::from(&local_from_uri);
                    if p.exists() {
                        return Some(HoveredTerminalTarget::LocalFile {
                            resolved_path: p,
                            display_path: local_from_uri,
                            line: None,
                            column: None,
                        });
                    }
                } else {
                    return Some(HoveredTerminalTarget::RemoteFile {
                        remote_path: local_from_uri,
                        line: None,
                        column: None,
                    });
                }
            }
            if let Some(detected) = detect_at_char_col(trimmed, 0) {
                if is_local {
                    if let Some(resolved) =
                        self.resolve_local_file_path(detected.path, block_pwd.as_deref())
                        && resolved.exists()
                    {
                        return Some(HoveredTerminalTarget::LocalFile {
                            resolved_path: resolved,
                            display_path: detected.path.to_owned(),
                            line: detected.line,
                            column: detected.column,
                        });
                    }
                } else {
                    return Some(HoveredTerminalTarget::RemoteFile {
                        remote_path: detected.path.to_owned(),
                        line: detected.line,
                        column: detected.column,
                    });
                }
            }
            return Some(HoveredTerminalTarget::Url(trimmed.to_owned()));
        }

        // 2. Plain terminal text across soft-wrapped logical lines: check file paths then URLs.
        let Ok(Some((line_text, char_col))) = self.pane_store.logical_line_at(key, col, row) else {
            return None;
        };

        if let Some(detected) = detect_at_char_col(&line_text, char_col) {
            if is_local {
                if let Some(resolved) =
                    self.resolve_local_file_path(detected.path, block_pwd.as_deref())
                    && resolved.exists()
                {
                    return Some(HoveredTerminalTarget::LocalFile {
                        resolved_path: resolved,
                        display_path: detected.path.to_owned(),
                        line: detected.line,
                        column: detected.column,
                    });
                }
            } else if looks_like_remote_file_path(detected.path) {
                return Some(HoveredTerminalTarget::RemoteFile {
                    remote_path: detected.path.to_owned(),
                    line: detected.line,
                    column: detected.column,
                });
            }
        }

        if let Some(url) = detect_url_at_char_col(&line_text, char_col) {
            return Some(HoveredTerminalTarget::Url(url.to_owned()));
        }

        None
    }

    fn resolve_local_file_path(&self, raw: &str, block_pwd: Option<&str>) -> Option<PathBuf> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.starts_with("file://") {
            return normalize_osc7_pwd(trimmed).map(PathBuf::from);
        }
        if trimmed == "~" {
            return dirs::home_dir();
        }
        if let Some(rest) = trimmed.strip_prefix("~/") {
            return dirs::home_dir().map(|home| home.join(rest));
        }
        let path = Path::new(trimmed);
        if path.is_absolute() {
            return Some(path.to_path_buf());
        }
        if let Some(pwd) = block_pwd {
            return Some(Path::new(pwd).join(path));
        }
        if let Some(cwd) = &self.default_working_dir {
            return Some(Path::new(cwd).join(path));
        }
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }

    fn update_hovered_file_path(&mut self, key: &BlockKey, col: u16, row: u32, px: f32, py: f32) {
        if let Some(target) = self.resolve_terminal_target(key, col, row) {
            match &target {
                HoveredTerminalTarget::Url(url) => {
                    self.pending_hover_image = None;
                    self.ui.hover_preview = Some(HoverPreviewState::FilePath {
                        path: url.clone(),
                        line: None,
                        column: None,
                        anchor_x: px,
                        anchor_y: py,
                    });
                }
                HoveredTerminalTarget::LocalFile {
                    resolved_path,
                    display_path,
                    line,
                    column,
                } => {
                    let preview_dims = if is_previewable_image_path(resolved_path)
                        && std::fs::metadata(resolved_path)
                            .ok()
                            .is_some_and(|m| m.is_file() && m.len() <= MAX_HOVER_IMAGE_BYTES)
                    {
                        image::image_dimensions(resolved_path)
                            .ok()
                            .filter(|&(w, h)| w > 0 && h > 0)
                    } else {
                        None
                    };

                    if let Some((image_width, image_height)) = preview_dims {
                        let caption = resolved_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(display_path.as_str())
                            .to_owned();
                        self.ui.hover_preview = Some(HoverPreviewState::ImageCard {
                            caption,
                            image_width,
                            image_height,
                            anchor_x: px,
                            anchor_y: py,
                        });
                        if self
                            .loaded_hover_image
                            .as_ref()
                            .is_some_and(|(p, _)| p == resolved_path)
                        {
                            self.hover_image_id = Some(HOVER_PREVIEW_IMAGE_GPU_ID);
                            self.pending_hover_image = None;
                        } else {
                            self.hover_image_gen = self.hover_image_gen.saturating_add(1);
                            self.pending_hover_image =
                                Some((resolved_path.clone(), self.hover_image_gen));
                        }
                    } else {
                        self.pending_hover_image = None;
                        self.ui.hover_preview = Some(HoverPreviewState::FilePath {
                            path: display_path.clone(),
                            line: *line,
                            column: *column,
                            anchor_x: px,
                            anchor_y: py,
                        });
                    }
                }
                HoveredTerminalTarget::RemoteFile {
                    remote_path,
                    line,
                    column,
                } => {
                    self.pending_hover_image = None;
                    self.ui.hover_preview = Some(HoverPreviewState::FilePath {
                        path: remote_path.clone(),
                        line: *line,
                        column: *column,
                        anchor_x: px,
                        anchor_y: py,
                    });
                }
            }
            self.hovered_terminal_target = Some((key.clone(), target));
            self.needs_redraw = true;
        } else {
            self.hovered_terminal_target = None;
            self.pending_hover_image = None;
            if self.ui.hover_preview.take().is_some() {
                self.needs_redraw = true;
            }
        }
    }

    fn open_terminal_target(&mut self, key: &BlockKey, target: &HoveredTerminalTarget) {
        match target {
            HoveredTerminalTarget::Url(uri) => {
                if let Err(err) = open::that_detached(uri) {
                    self.push_toast(
                        ToastSeverity::Warning,
                        "Open link failed",
                        format!("{uri} ({err})"),
                    );
                }
            }
            HoveredTerminalTarget::LocalFile { resolved_path, .. } => {
                if let Err(err) = open::that_detached(resolved_path) {
                    self.push_toast(
                        ToastSeverity::Warning,
                        "Open file failed",
                        format!("{} ({err})", resolved_path.display()),
                    );
                }
            }
            HoveredTerminalTarget::RemoteFile { remote_path, .. } => {
                let if_none_match = self
                    .downloaded_file_cache
                    .get(&(key.host_id.clone(), remote_path.clone()))
                    .and_then(|(path, ver)| path.exists().then(|| ver.clone()).flatten());
                let transfer_id = format!("dl-{}", self.next_transfer_id);
                self.next_transfer_id = self.next_transfer_id.saturating_add(1);
                self.push_toast(
                    ToastSeverity::Info,
                    "Downloading remote file",
                    remote_path.clone(),
                );
                self.send_command(HostCommand::DownloadFile {
                    host_id: key.host_id.clone(),
                    session_id: key.session_id.clone(),
                    block_id: key.block_id.clone(),
                    transfer_id,
                    remote_path: remote_path.clone(),
                    cache_dir: remote_download_cache_dir(),
                    if_none_match,
                    max_bytes: None,
                });
            }
        }
    }

    /// Handles pointer button press (UI chrome hit-test first, then terminal mouse tracking /
    /// OSC 8 hyperlink Ctrl+click / selection gesture).
    pub fn handle_pointer_down(
        &mut self,
        px: f32,
        py: f32,
        button: PointerButton,
        mods: Modifiers,
        time_ns: u64,
        now: Instant,
    ) {
        self.pointer_pos = (px, py);
        self.needs_redraw = true;

        // Middle-click on a tab closes it immediately (`gui-spec.md §2.2`).
        if button == PointerButton::Middle {
            if let Some(out) = &self.last_ui_output
                && let Some(HitTarget::TabCard {
                    host_id,
                    session_id,
                    window_id,
                }) = out.draw_list.hit_test(px, py).cloned()
            {
                self.dispatch_ui_action(
                    UiAction::CloseTab {
                        host_id,
                        session_id,
                        window_id,
                    },
                    now,
                );
                return;
            }
            // Middle-click paste inside terminal (Linux primary selection with clipboard fallback).
            self.handle_primary_paste(now);
            return;
        }

        // Check if click lands inside a terminal pane's content rect (and NOT covered by modal/menu/header).
        let is_pane_surface_hit = self.last_ui_output.as_ref().is_some_and(|out| {
            matches!(
                out.draw_list.hit_test(px, py),
                Some(HitTarget::PaneSurface { .. })
            )
        });

        if is_pane_surface_hit
            && let Some((key, content_rect, col, row, rel_x, rel_y)) =
                self.pane_cell_at_pointer(px, py)
        {
            self.ui.focus.focus_terminal(Some(key.block_id.clone()));
            self.maybe_claim_ownership(&key, OwnershipTrigger::FocusPane);

            // 1. Terminal SGR mouse tracking (honors Shift-bypass automatically via `encode_mouse`)
            let metrics = self.last_cell_metrics.unwrap_or(CellMetrics {
                font_size: 14.0,
                scale_factor: 1.0,
                cell_width: 8.0,
                cell_height: 19.0,
                baseline_y: 14.0,
                underline_y: 16.0,
                underline_thickness: 1.0,
                strikethrough_y: 10.0,
            });
            let geom = MouseGeometry {
                screen_width: content_rect.w.round().max(1.0) as u32,
                screen_height: content_rect.h.round().max(1.0) as u32,
                cell_width: metrics.cell_width.round().max(1.0) as u32,
                cell_height: metrics.cell_height.round().max(1.0) as u32,
                padding_top: 0,
                padding_bottom: 0,
                padding_left: 0,
                padding_right: 0,
            };
            let mouse_in = MouseEventInput {
                action: MouseAction::Press,
                button: Some(button.to_vt()),
                mods: bridge_mods_to_vt(mods),
                x: rel_x,
                y: rel_y,
            };
            if let Ok(Some(bytes)) = self.pane_store.encode_mouse(&key, &mouse_in, geom)
                && let Ok(data) = String::from_utf8(bytes)
            {
                self.send_command(HostCommand::TerminalWrite {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                    data,
                });
                return;
            }

            // 2. Ctrl+Click opens OSC 8 links, plain file paths (local or remote), and plain URLs
            if button == PointerButton::Left && mods.ctrl {
                let target = self
                    .hovered_terminal_target
                    .as_ref()
                    .filter(|(k, _)| k == &key)
                    .map(|(_, t)| t.clone())
                    .or_else(|| self.resolve_terminal_target(&key, col, row));
                if let Some(target) = target {
                    self.open_terminal_target(&key, &target);
                    return;
                }
            }

            // 3. Left-click starts native text selection gesture (Alt+drag = rectangle!)
            if button == PointerButton::Left {
                let _ = self.pane_store.selection_press(
                    &key,
                    col,
                    row,
                    f64::from(rel_x),
                    f64::from(rel_y),
                    time_ns,
                );
                self.active_drag = Some(ActiveDrag::TerminalSelection {
                    key,
                    content_rect,
                    cell_w: metrics.cell_width.max(1.0),
                    cell_h: metrics.cell_height.max(1.0),
                    rectangle: mods.alt,
                });
                return;
            }
        }

        // Track pressed chrome target for tactile button feedback & remember right-clicked target for Rename.
        if let Some(out) = self.last_ui_output.clone() {
            if button == PointerButton::Left && self.ui.set_pointer_pressed(&out.draw_list, px, py)
            {
                self.needs_redraw = true;
            }
            if button == PointerButton::Right {
                self.pending_rename_target = match out.draw_list.hit_test(px, py) {
                    Some(
                        HitTarget::HostHeader { host_id } | HitTarget::HostNewSession { host_id },
                    ) if host_id != LOCAL_HOST_ID => Some(PendingRenameTarget::Host {
                        host_id: host_id.clone(),
                    }),
                    Some(HitTarget::SessionRow {
                        host_id,
                        session_id,
                    }) => Some(PendingRenameTarget::Session {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                    }),
                    Some(HitTarget::TabCard {
                        host_id,
                        session_id,
                        window_id,
                    }) => Some(PendingRenameTarget::Window {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                        window_id: window_id.clone(),
                    }),
                    Some(
                        HitTarget::PaneSurface { block_id } | HitTarget::PaneHeader { block_id },
                    ) => self.workspace.active_session.as_ref().map(|sk| {
                        PendingRenameTarget::Block {
                            host_id: sk.host_id.clone(),
                            session_id: sk.session_id.clone(),
                            block_id: block_id.clone(),
                        }
                    }),
                    _ => None,
                };
            }

            // Left-click on terminal scrollbar track or thumb: scroll to exact click fraction + drag!
            if button == PointerButton::Left
                && let Some(entry) = out.draw_list.hit_test_entry(px, py)
                && let HitTarget::PaneScrollbarTrack { block_id }
                | HitTarget::PaneScrollbarThumb { block_id } = &entry.target
                && let Some(sk) = self.workspace.active_session.clone()
            {
                let track_rect = out
                    .draw_list
                    .hits
                    .iter()
                    .find(|h| {
                        matches!(
                            &h.target,
                            HitTarget::PaneScrollbarTrack { block_id: bid } if bid == block_id
                        )
                    })
                    .map(|h| h.rect)
                    .unwrap_or(entry.rect);
                let frac = ((py - track_rect.y) / track_rect.h.max(1.0)).clamp(0.0, 1.0);
                let key = BlockKey::new(sk.host_id, sk.session_id, block_id.clone());
                if let Some(sb) = self.pane_store.scrollbar(&key) {
                    let max_off = sb.total.saturating_sub(sb.len);
                    let row = ((max_off as f32) * frac).round() as usize;
                    let _ = self
                        .pane_store
                        .scroll_viewport(&key, ScrollViewport::Row(row));
                }
                self.active_drag = Some(ActiveDrag::PaneScrollbar { key, track_rect });
                return;
            }
        }

        // Route all other clicks (sidebar, tabs, headers, modals, context menu) through `UiTree`.
        let action = if let Some(out) = self.last_ui_output.clone() {
            self.ui
                .handle_click(&out.draw_list, px, py, button == PointerButton::Right)
        } else {
            UiAction::None
        };
        self.dispatch_ui_action(action, now);
    }

    /// Handles pointer leaving the window surface (clears transient hover/pressed tooltips).
    pub fn handle_pointer_leave(&mut self) {
        let mut changed = self.ui.clear_pointer_pressed();
        if self.ui.hover_preview.take().is_some() {
            changed = true;
        }
        self.hovered_path_cell = None;
        self.hovered_terminal_target = None;
        self.pending_hover_image = None;
        if self
            .ui
            .update_pointer_hover(&ui_chrome::UiDrawList::new(), -1.0, -1.0)
        {
            changed = true;
        }
        if changed {
            self.needs_redraw = true;
        }
    }

    /// Handles pointer button release (finalizes active drags, selection gestures, or mouse release).
    pub fn handle_pointer_up(&mut self, px: f32, py: f32, button: PointerButton, mods: Modifiers) {
        self.pointer_pos = (px, py);
        if self.ui.clear_pointer_pressed() {
            self.needs_redraw = true;
        }
        if let Some(drag) = self.active_drag.take() {
            self.needs_redraw = true;
            match drag {
                ActiveDrag::SidebarResize { .. } => {
                    self.force_immediate_resize = true;
                    self.save_settings_best_effort();
                }
                ActiveDrag::TerminalSelection {
                    key,
                    content_rect,
                    cell_w,
                    cell_h,
                    ..
                } => {
                    let col = ((px - content_rect.x).max(0.0) / cell_w).floor() as u16;
                    let row = ((py - content_rect.y).max(0.0) / cell_h).floor() as u32;
                    let _ = self.pane_store.selection_release(&key, Some((col, row)));
                }
                ActiveDrag::SplitDivider { .. }
                | ActiveDrag::FloatingLayer { .. }
                | ActiveDrag::PaneScrollbar { .. } => {}
            }
            return;
        }

        // Forward release to mouse-tracking terminal if active.
        if let Some((key, content_rect, _, _, rel_x, rel_y)) = self.pane_cell_at_pointer(px, py)
            && let Some(metrics) = self.last_cell_metrics
        {
            let geom = MouseGeometry {
                screen_width: content_rect.w.round().max(1.0) as u32,
                screen_height: content_rect.h.round().max(1.0) as u32,
                cell_width: metrics.cell_width.round().max(1.0) as u32,
                cell_height: metrics.cell_height.round().max(1.0) as u32,
                padding_top: 0,
                padding_bottom: 0,
                padding_left: 0,
                padding_right: 0,
            };
            let mouse_in = MouseEventInput {
                action: MouseAction::Release,
                button: Some(button.to_vt()),
                mods: bridge_mods_to_vt(mods),
                x: rel_x,
                y: rel_y,
            };
            if let Ok(Some(bytes)) = self.pane_store.encode_mouse(&key, &mouse_in, geom)
                && let Ok(data) = String::from_utf8(bytes)
            {
                self.send_command(HostCommand::TerminalWrite {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                    data,
                });
            }
        }
    }

    /// Handles smooth mouse wheel / Wayland touchpad scrolling (`delta_y_px > 0` scrolls UP).
    pub fn handle_pointer_wheel(
        &mut self,
        px: f32,
        py: f32,
        delta_x_px: f32,
        delta_y_px: f32,
        mods: Modifiers,
    ) {
        self.pointer_pos = (px, py);
        self.needs_redraw = true;

        // 0. If a modal is open, scroll the modal list instead of leaking wheel input to the terminal.
        if let Some(modal) = self.ui.modal.as_mut() {
            let steps = ((delta_y_px.abs() / 36.0).round() as isize).clamp(1, 5);
            let delta_rows = if delta_y_px >= 0.0 { -steps } else { steps };
            let _ = modal.scroll_by(delta_rows);
            return;
        }
        if self.ui.context_menu.is_some() {
            return;
        }

        // 1. Scroll sidebar or overflowing horizontal tab bar if pointer is over chrome toolbar
        if let Some(out) = &self.last_ui_output {
            if out.chrome.sidebar.is_some_and(|r| r.contains(px, py)) {
                self.ui
                    .sidebar
                    .scroll_by(delta_y_px, out.sidebar_scroll.max_scroll);
                return;
            }
            if out.chrome.tab_bar.is_some_and(|r| r.contains(px, py)) {
                let horiz = if delta_x_px.abs() > 0.1 {
                    delta_x_px
                } else {
                    delta_y_px
                };
                self.ui.scroll_tabbar_by(horiz);
                return;
            }
        }

        // 2. Scroll or report wheel to hovered terminal pane
        if let Some((key, content_rect, _, _, rel_x, rel_y)) = self.pane_cell_at_pointer(px, py) {
            let cell_h = self
                .last_cell_metrics
                .map_or(19.0, |m| m.cell_height.max(1.0));
            let cell_w = self
                .last_cell_metrics
                .map_or(8.0, |m| m.cell_width.max(1.0));

            // If TUI has mouse tracking on (and Shift isn't held), encode SGR wheel events.
            let geom = MouseGeometry {
                screen_width: content_rect.w.round().max(1.0) as u32,
                screen_height: content_rect.h.round().max(1.0) as u32,
                cell_width: cell_w.round().max(1.0) as u32,
                cell_height: cell_h.round().max(1.0) as u32,
                padding_top: 0,
                padding_bottom: 0,
                padding_left: 0,
                padding_right: 0,
            };
            let wheel_btn = if delta_y_px >= 0.0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            let mouse_in = MouseEventInput {
                action: MouseAction::Press,
                button: Some(wheel_btn),
                mods: bridge_mods_to_vt(mods),
                x: rel_x,
                y: rel_y,
            };
            if let Ok(Some(bytes)) = self.pane_store.encode_mouse(&key, &mouse_in, geom)
                && let Ok(data) = String::from_utf8(bytes)
            {
                self.send_command(HostCommand::TerminalWrite {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                    data,
                });
                return;
            }

            // Otherwise perform smooth fractional Y overscan scroll!
            let _ = self.pane_store.scroll_wheel_px(&key, delta_y_px, cell_h);
        }
    }

    /// Pastes text from the Linux primary selection (middle-click paste), falling back to the
    /// standard Wayland/X11 clipboard if primary selection is empty.
    pub fn handle_primary_paste(&mut self, now: Instant) {
        if !self.clipboard_enabled {
            return;
        }
        if let Some(text) = get_primary_selection_text().or_else(get_system_clipboard_text) {
            self.handle_paste(&text, now);
        }
    }

    /// Pastes clipboard text, copied files (`text/uri-list` from file managers), or clipboard
    /// images (Wayland screenshots for Mode 5522 / T3 Code / Claude Code) into the target block
    /// or focused UI input.
    pub fn handle_clipboard_paste_or_image(&mut self, block_id: Option<String>, now: Instant) {
        if !self.clipboard_enabled {
            return;
        }

        // 1. When terminal is focused, check Wayland clipboard file URIs first (e.g. files,
        //    images, or videos copied in Dolphin/Nautilus/Thunar).
        if !self.ui.focus.is_overlay_focused() {
            let copied_files = get_wayland_clipboard_uri_paths();
            if !copied_files.is_empty() {
                if let Some(bid) = block_id.as_ref()
                    && let Some(sk) = self.workspace.active_session.clone()
                {
                    let key = BlockKey::new(sk.host_id, sk.session_id, bid.clone());
                    self.handle_file_drop_into_block(&key, &copied_files);
                } else {
                    self.handle_file_drop_at(&copied_files, None);
                }
                return;
            }
        }

        // 2. Standard UTF-8 clipboard text.
        if let Some(text) = get_system_clipboard_text()
            && !text.is_empty()
        {
            if let Some(bid) = block_id
                && let Some(sk) = self.workspace.active_session.clone()
            {
                self.paste_into_block(&BlockKey::new(sk.host_id, sk.session_id, bid), &text);
            } else {
                self.handle_paste(&text, now);
            }
            return;
        }

        // 3. Fallback: image clipboard -> temporary image file -> local path paste or remote upload.
        if !self.ui.focus.is_overlay_focused()
            && let Some(tmp_img) = save_clipboard_image_to_temp_file(self.next_transfer_id)
        {
            self.next_transfer_id = self.next_transfer_id.saturating_add(1);
            if let Some(bid) = block_id
                && let Some(sk) = self.workspace.active_session.clone()
            {
                let key = BlockKey::new(sk.host_id, sk.session_id, bid);
                self.handle_file_drop_into_block(&key, &[tmp_img]);
            } else {
                self.handle_file_drop_at(&[tmp_img], None);
            }
        }
    }

    /// Pastes UTF-8 text into whichever surface holds focus (modal, find bar, sidebar filter, or PTY).
    pub fn handle_paste(&mut self, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        self.needs_redraw = true;
        match self.ui.focus.current() {
            FocusTarget::Modal(_) => {
                let action = self.ui.handle_modal_key(ModalKey::Paste(text.to_owned()));
                self.dispatch_ui_action(action, now);
            }
            FocusTarget::FindBar => {
                self.ui.find_bar.input.paste(text);
                let query = self.ui.find_bar.input.text().to_owned();
                self.dispatch_ui_action(UiAction::FindQueryChanged(query), now);
            }
            FocusTarget::SidebarFilter => {
                self.ui.sidebar.filter_input.paste(text);
            }
            FocusTarget::ContextMenu => {}
            FocusTarget::Terminal(_) => {
                if let Some(key) = self.focused_block_key() {
                    self.paste_into_block(&key, text);
                }
            }
        }
    }

    fn paste_into_block(&mut self, key: &BlockKey, text: &str) {
        self.maybe_claim_ownership(key, OwnershipTrigger::ExplicitInteractOrFit);
        let bytes = self
            .pane_store
            .encode_paste(key, text.as_bytes())
            .unwrap_or_else(|_| text.as_bytes().to_vec());
        if let Ok(data) = String::from_utf8(bytes) {
            self.send_command(HostCommand::TerminalWrite {
                host_id: key.host_id.clone(),
                session_id: key.session_id.clone(),
                block_id: key.block_id.clone(),
                data,
            });
        }
    }

    /// Updates visual drop-zone highlight while a file/media drag hovers at logical `(px, py)`.
    pub fn handle_file_hover(&mut self, px: f32, py: f32) {
        let changed = self
            .active_dnd_hover
            .is_none_or(|(ox, oy)| (ox - px).abs() > 0.5 || (oy - py).abs() > 0.5);
        self.active_dnd_hover = Some((px, py));
        if changed {
            self.needs_redraw = true;
        }
    }

    /// Clears the active file drag-and-drop highlight when the drag leaves the window or aborts.
    pub fn handle_file_hover_cancelled(&mut self) {
        if self.active_dnd_hover.take().is_some() {
            self.needs_redraw = true;
        }
    }

    /// Handles dropped file paths on the currently focused pane.
    pub fn handle_file_drop(&mut self, paths: &[PathBuf]) {
        self.handle_file_drop_at(paths, None);
    }

    /// Handles dropped file paths (images, videos, documents, folders, etc.) from Wayland/X11 DnD
    /// at an optional window-local logical drop coordinate `(px, py)`:
    /// - Targets the exact pane beneath `drop_pos` (falling back to the focused pane), focusing it.
    /// - Local blocks: shell-escaped paths pasted via bracketed paste (`tr3x` / shell / agents).
    /// - Remote blocks: streams all regular files via `upload_begin` (`64 KiB` chunks) using a
    ///   single `UploadBatch` for multi-file drops so Rex pastes every path together in order.
    pub fn handle_file_drop_at(&mut self, paths: &[PathBuf], drop_pos: Option<(f32, f32)>) {
        if self.active_dnd_hover.take().is_some() {
            self.needs_redraw = true;
        }
        if paths.is_empty() {
            return;
        }
        let target_key = drop_pos
            .and_then(|(px, py)| self.pane_key_at_pointer(px, py))
            .or_else(|| self.focused_block_key());
        let Some(key) = target_key else {
            return;
        };
        self.ui.focus.focus_terminal(Some(key.block_id.clone()));
        self.handle_file_drop_into_block(&key, paths);
    }

    /// Handles dropped plain UTF-8 text from Wayland DnD at an optional logical `(px, py)`.
    pub fn handle_text_drop_at(&mut self, text: &str, drop_pos: Option<(f32, f32)>, now: Instant) {
        if self.active_dnd_hover.take().is_some() {
            self.needs_redraw = true;
        }
        if text.is_empty() {
            return;
        }
        if let Some((px, py)) = drop_pos
            && let Some(key) = self.pane_key_at_pointer(px, py)
        {
            self.ui.focus.focus_terminal(Some(key.block_id.clone()));
            self.paste_into_block(&key, text);
            self.needs_redraw = true;
            return;
        }
        self.handle_paste(text, now);
    }

    fn handle_file_drop_into_block(&mut self, key: &BlockKey, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        self.needs_redraw = true;
        let is_local = self.is_block_local(key);
        if is_local {
            let escaped: Vec<String> = paths.iter().map(|p| shell_escape_path(p)).collect();
            let joined = escaped.join(" ");
            self.paste_into_block(key, &joined);
            return;
        }

        let mut regular_files: Vec<(PathBuf, u64)> = Vec::new();
        let mut skipped_dirs: usize = 0;
        let mut missing_files: usize = 0;

        for path in paths {
            match std::fs::metadata(path) {
                Ok(meta) if meta.is_file() => {
                    regular_files.push((path.clone(), meta.len()));
                }
                Ok(meta) if meta.is_dir() => {
                    skipped_dirs += 1;
                }
                Ok(_) => {
                    missing_files += 1;
                }
                Err(_) => {
                    // Preserve unit tests that pass synthetic non-existent paths into remote mocks
                    // only when `host_manager` is not attached; on live runs report missing files.
                    if self.host_manager.is_none() {
                        regular_files.push((path.clone(), 0));
                    } else {
                        missing_files += 1;
                    }
                }
            }
        }

        if skipped_dirs > 0 {
            self.push_toast(
                ToastSeverity::Warning,
                "Folders skipped for remote upload",
                format!(
                    "{skipped_dirs} folder(s) cannot be dropped into a remote Rex terminal (use `rex cp` for directories)"
                ),
            );
        }

        if regular_files.is_empty() {
            if missing_files > 0 {
                self.push_toast(
                    ToastSeverity::Warning,
                    "Drop skipped",
                    format!("{missing_files} dropped path(s) could not be read"),
                );
            }
            return;
        }

        let count = regular_files.len() as u32;
        let total_bytes: u64 = regular_files.iter().map(|(_, sz)| *sz).sum();
        let batch_id = (count > 1).then(|| uuid::Uuid::new_v4().to_string());

        if count > 1 || total_bytes >= LARGE_UPLOAD_TOAST_THRESHOLD_BYTES {
            let detail = if count == 1 {
                let name = regular_files[0]
                    .0
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");
                format!("Uploading {name} ({})…", format_byte_size(total_bytes))
            } else {
                format!(
                    "Uploading {count} files ({})…",
                    format_byte_size(total_bytes)
                )
            };
            self.push_toast(ToastSeverity::Info, "Uploading to remote pane", detail);
        }

        self.maybe_claim_ownership(key, OwnershipTrigger::ExplicitInteractOrFit);
        for (idx, (path, _size)) in regular_files.into_iter().enumerate() {
            let transfer_id = format!("up-{}", self.next_transfer_id);
            self.next_transfer_id = self.next_transfer_id.saturating_add(1);
            let batch = batch_id.as_ref().map(|id| UploadBatch {
                batch_id: id.clone(),
                index: idx as u32,
                count,
            });
            self.send_command(HostCommand::UploadFile {
                host_id: key.host_id.clone(),
                session_id: key.session_id.clone(),
                block_id: key.block_id.clone(),
                transfer_id,
                local_path: path,
                batch,
            });
        }
    }

    /// Handles Wayland IME preedit and committed text.
    pub fn handle_ime(
        &mut self,
        preedit: Option<(String, Option<Range<usize>>)>,
        commit: Option<String>,
        now: Instant,
    ) {
        self.needs_redraw = true;
        if let Some(committed) = commit {
            self.handle_paste(&committed, now);
            return;
        }
        let apply_preedit = |inp: &mut ui_chrome::TextInput| {
            if let Some((text, cursor_range)) = preedit {
                inp.set_preedit(text, cursor_range);
            } else {
                inp.clear_preedit();
            }
        };
        match self.ui.focus.current() {
            FocusTarget::FindBar => {
                apply_preedit(&mut self.ui.find_bar.input);
            }
            FocusTarget::SidebarFilter => {
                apply_preedit(&mut self.ui.sidebar.filter_input);
            }
            FocusTarget::Modal(_) => {
                if let Some(modal) = &mut self.ui.modal {
                    match &mut modal.content {
                        ModalContent::List { input, .. }
                        | ModalContent::RexPrompt { input, .. }
                        | ModalContent::Rename { input, .. }
                        | ModalContent::AddHost { input } => apply_preedit(input),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// Updates Wayland top-level keyboard focus state (forwarding DEC Mode 1004 reports if enabled
    /// and reclaiming block ownership / snapping dimensions when regaining focus after a split).
    pub fn handle_window_focus(&mut self, gained: bool, _now: Instant) {
        self.window_focused = gained;
        self.needs_redraw = true;
        if let Some(key) = self.focused_block_key()
            && let Ok(Some(bytes)) = self.pane_store.encode_focus(&key, gained)
            && let Ok(data) = String::from_utf8(bytes)
        {
            self.send_command(HostCommand::TerminalWrite {
                host_id: key.host_id,
                session_id: key.session_id,
                block_id: key.block_id,
                data,
            });
        }
        if gained && !self.read_only {
            let keys = if self.visible_pane_keys.is_empty() {
                self.focused_block_key().into_iter().collect()
            } else {
                self.visible_pane_keys.clone()
            };
            for key in keys {
                let my_client_id = self
                    .workspace
                    .host(&key.host_id)
                    .and_then(HostEntry::client_id)
                    .map(str::to_owned);
                let blk = self.workspace.block(&key);
                let not_owned_by_me = blk.is_none_or(|b| b.owner != my_client_id);
                let expected = self.last_pane_grids.get(&key).copied();
                let local_mismatch =
                    expected.is_some_and(|exp| self.pane_store.size(&key) != Some(exp));
                let server_mismatch = expected.is_some_and(|exp| {
                    blk.and_then(|b| b.last_server_size)
                        .is_some_and(|srv| srv != exp)
                });
                if not_owned_by_me || local_mismatch || server_mismatch {
                    self.workspace.invalidate_sent_resize(&key);
                    self.force_immediate_resize = true;
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
            }
        }
    }

    /// Uploads a hover preview image into `GpuRenderer` and records its ID for `hover_image_slot`.
    pub fn set_hover_preview_image(
        &mut self,
        renderer: &mut GpuRenderer,
        image_id: u64,
        generation: u64,
        encoded_bytes: &[u8],
    ) -> Result<()> {
        renderer.upload_image(ImageUpload {
            id: image_id,
            generation,
            width: 0,
            height: 0,
            format: gpu_renderer::ImagePixelFormat::Encoded,
            data: encoded_bytes,
        })?;
        self.hover_image_id = Some(image_id);
        self.needs_redraw = true;
        Ok(())
    }

    // =========================================================================
    // Helpers
    // =========================================================================

    fn pane_key_at_pointer(&self, px: f32, py: f32) -> Option<BlockKey> {
        let sk = self.workspace.active_session.as_ref()?;
        let out = self.last_ui_output.as_ref()?;
        for fgeo in out.floating_panes.iter().rev() {
            if fgeo.card_rect.contains(px, py) {
                return Some(BlockKey::new(&sk.host_id, &sk.session_id, &fgeo.id));
            }
        }
        for geo in &out.tiled_panes {
            if geo.card_rect.contains(px, py) {
                return Some(BlockKey::new(&sk.host_id, &sk.session_id, &geo.id));
            }
        }
        None
    }

    fn pane_card_rect_at_pointer(&self, px: f32, py: f32) -> Option<RectF> {
        let out = self.last_ui_output.as_ref()?;
        for fgeo in out.floating_panes.iter().rev() {
            if fgeo.card_rect.contains(px, py) {
                return Some(fgeo.card_rect);
            }
        }
        for geo in &out.tiled_panes {
            if geo.card_rect.contains(px, py) {
                return Some(geo.card_rect);
            }
        }
        // Fallback to focused pane card when dragging over window border or sidebar.
        if let Some(focused_id) = self.ui.focus.last_terminal_block() {
            if let Some(fgeo) = out.floating_panes.iter().find(|p| p.id == focused_id) {
                return Some(fgeo.card_rect);
            }
            if let Some(geo) = out.tiled_panes.iter().find(|p| p.id == focused_id) {
                return Some(geo.card_rect);
            }
        }
        out.tiled_panes.first().map(|p| p.card_rect)
    }

    fn pane_cell_at_pointer(
        &self,
        px: f32,
        py: f32,
    ) -> Option<(BlockKey, RectF, u16, u32, f32, f32)> {
        let sk = self.workspace.active_session.as_ref()?;
        let out = self.last_ui_output.as_ref()?;
        let (pad_x, pad_y) = self.pane_content_padding();
        let metrics = self.last_cell_metrics?;

        // Floating panes take priority over tiled panes beneath them.
        for fgeo in out.floating_panes.iter().rev() {
            if fgeo.terminal_rect.contains(px, py) {
                let content = fgeo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                let rel_x = (px - content.x).max(0.0);
                let rel_y = (py - content.y).max(0.0);
                let col = (rel_x / metrics.cell_width.max(1.0)).floor() as u16;
                let row = (rel_y / metrics.cell_height.max(1.0)).floor() as u32;
                return Some((
                    BlockKey::new(&sk.host_id, &sk.session_id, &fgeo.id),
                    content,
                    col,
                    row,
                    rel_x,
                    rel_y,
                ));
            }
        }
        for geo in &out.tiled_panes {
            if geo.terminal_rect.contains(px, py) {
                let content = geo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                let rel_x = (px - content.x).max(0.0);
                let rel_y = (py - content.y).max(0.0);
                let col = (rel_x / metrics.cell_width.max(1.0)).floor() as u16;
                let row = (rel_y / metrics.cell_height.max(1.0)).floor() as u32;
                return Some((
                    BlockKey::new(&sk.host_id, &sk.session_id, &geo.id),
                    content,
                    col,
                    row,
                    rel_x,
                    rel_y,
                ));
            }
        }
        None
    }

    fn resolve_session_key(&self, session_id: Option<&str>) -> Option<SessionKey> {
        if let Some(sid) = session_id {
            for host in self.workspace.hosts.values() {
                if host.sessions.contains_key(sid) {
                    return Some(SessionKey::new(host.id.clone(), sid.to_owned()));
                }
            }
        }
        self.workspace.active_session.clone()
    }

    fn find_layer_for_block(&self, block_id: &str) -> Option<(SessionKey, String)> {
        let sk = self.workspace.active_session.as_ref()?;
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        for layer in &win.layers {
            if layer
                .blocks
                .iter()
                .any(|b| b.block_id.as_deref() == Some(block_id))
                && let Some(lid) = &layer.layer_id
            {
                return Some((sk.clone(), lid.clone()));
            }
        }
        None
    }

    fn is_block_in_floating_layer(&self, sk: &SessionKey, block_id: &str) -> bool {
        let Some(view) = self.workspace.session(sk).and_then(|s| s.view.as_ref()) else {
            return false;
        };
        let Some(win) = active_window_of_view(view) else {
            return false;
        };
        win.layers.iter().any(|l| {
            !l.kind.eq_ignore_ascii_case("tiled")
                && l.blocks
                    .iter()
                    .any(|b| b.block_id.as_deref() == Some(block_id))
        })
    }

    fn find_tiled_anchor_block(&self, sk: &SessionKey, exclude_block_id: &str) -> Option<String> {
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        win.layers
            .iter()
            .filter(|l| l.kind.eq_ignore_ascii_case("tiled"))
            .flat_map(|l| l.blocks.iter())
            .filter_map(|b| b.block_id.as_deref())
            .find(|&bid| bid != exclude_block_id)
            .map(str::to_owned)
    }

    fn move_active_tab(&mut self, forward: bool) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let move_args = self
            .workspace
            .session(&sk)
            .and_then(|s| s.view.as_ref())
            .and_then(|view| {
                let ids: Vec<&str> = view
                    .windows
                    .iter()
                    .filter_map(|w| w.window_id.as_deref())
                    .collect();
                if ids.len() <= 1 {
                    return None;
                }
                let cur = view
                    .active_window_id
                    .as_deref()
                    .and_then(|id| ids.iter().position(|&w| w == id))?;
                let wid = ids[cur].to_owned();
                if forward {
                    if cur + 1 >= ids.len() {
                        return None;
                    }
                    let before = ids.get(cur + 2).map(|&s| s.to_owned());
                    Some((wid, before))
                } else {
                    if cur == 0 {
                        return None;
                    }
                    let before = Some(ids[cur - 1].to_owned());
                    Some((wid, before))
                }
            });
        if let Some((window_id, before_window_id)) = move_args {
            self.send_command(HostCommand::MoveWindow {
                host_id: sk.host_id,
                session_id: sk.session_id,
                window_id,
                before_window_id,
            });
        }
    }

    fn move_focused_pane(&mut self, dir: PaneDirection) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let Some(cur_key) = self.focused_block_key() else {
            return;
        };
        let Some(out) = &self.last_ui_output else {
            return;
        };
        let Some(cur) = out.tiled_panes.iter().find(|p| p.id == cur_key.block_id) else {
            return;
        };
        let (cx, cy) = cur.card_rect.center();
        let mut best: Option<(f32, String)> = None;
        for cand in &out.tiled_panes {
            if cand.id == cur.id {
                continue;
            }
            let (tx, ty) = cand.card_rect.center();
            let valid = match dir {
                PaneDirection::Left => tx < cx - 4.0,
                PaneDirection::Right => tx > cx + 4.0,
                PaneDirection::Up => ty < cy - 4.0,
                PaneDirection::Down => ty > cy + 4.0,
            };
            if !valid {
                continue;
            }
            let dist = (tx - cx).hypot(ty - cy);
            if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                best = Some((dist, cand.id.clone()));
            }
        }
        if let Some((_, anchor_block_id)) = best {
            let (direction, side) = match dir {
                PaneDirection::Left => ("horizontal", "before"),
                PaneDirection::Right => ("horizontal", "after"),
                PaneDirection::Up => ("vertical", "before"),
                PaneDirection::Down => ("vertical", "after"),
            };
            self.send_command(HostCommand::MoveBlock {
                host_id: sk.host_id,
                session_id: sk.session_id,
                block_id: cur_key.block_id,
                anchor_block_id,
                direction: direction.to_owned(),
                side: side.to_owned(),
                ratio: Some(0.5),
            });
        }
    }

    fn find_layer_bounds(&self, sk: &SessionKey, layer_id: &str) -> Option<[f32; 4]> {
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        let layer = win
            .layers
            .iter()
            .find(|l| l.layer_id.as_deref() == Some(layer_id))?;
        Some([
            layer.bounds[0] as f32,
            layer.bounds[1] as f32,
            layer.bounds[2] as f32,
            layer.bounds[3] as f32,
        ])
    }

    fn resolve_split_target_at_path(
        &self,
        sk: &SessionKey,
        path: &[bool],
    ) -> Option<(String, String, String)> {
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        let tiled = win
            .layers
            .iter()
            .find(|l| l.kind.eq_ignore_ascii_case("tiled"))?;
        let mut cur = tiled.layout.as_ref()?;
        for &go_second in path {
            let sp = cur.split.as_ref()?;
            cur = if go_second { &sp.after } else { &sp.before };
        }
        let sp = cur.split.as_ref()?;
        let before_id = first_leaf_block_id(&sp.before)?;
        let after_id = first_leaf_block_id(&sp.after)?;
        Some((sp.direction.clone(), before_id, after_id))
    }

    fn is_block_local(&self, key: &BlockKey) -> bool {
        let Some(host) = self.workspace.host(&key.host_id) else {
            return false;
        };
        let block_host_id = host
            .sessions
            .get(&key.session_id)
            .and_then(|s| s.view.as_ref())
            .and_then(|v| {
                v.windows
                    .iter()
                    .flat_map(|w| w.layers.iter())
                    .flat_map(|l| l.blocks.iter())
                    .find(|b| b.block_id.as_deref() == Some(key.block_id.as_str()))
                    .and_then(|b| b.host_id.as_deref())
            });
        host.is_block_local_to_this_machine(block_host_id)
    }
}

// ============================================================================
// 5. `UiDrawList` -> `gpu_renderer::DrawList` Z-Ordered Translator
// ============================================================================

/// Translates `ui_chrome`'s backend-agnostic [`UiRenderOutput`] into `gpu_renderer`'s layered
/// [`DrawList`], preserving exact painter's order across tiled pane cards, live terminal grids,
/// floating layers (such as `tr3x`'s `/pr` drawer), modals, hover previews, and context menus.
pub fn translate_ui_to_gpu(
    ui_out: &UiRenderOutput,
    floating_terminal_frames: &HashMap<String, SceneFrame>,
    hover_image: Option<(RectF, u64)>,
    atlas: &mut GlyphAtlas,
    draw_list: &mut DrawList,
) {
    let tiled_cards: Vec<RectF> = ui_out.tiled_panes.iter().map(|p| p.card_rect).collect();
    let floating_cards: Vec<(RectF, &str)> = ui_out
        .floating_panes
        .iter()
        .map(|p| (p.card_rect, p.id.as_str()))
        .collect();

    let mut current_layer: i32 = layers::CHROME;
    let mut layer_has_foreground = false;
    let mut clip_depth: usize = 0;

    for (idx, prim) in ui_out.draw_list.primitives.iter().enumerate() {
        match prim {
            UiPrimitive::PushClip { rect } => {
                draw_list.push_clip(rectf_to_gpu(*rect));
                clip_depth += 1;
            }
            UiPrimitive::PopClip => {
                if clip_depth > 0 {
                    draw_list.pop_clip();
                    clip_depth -= 1;
                }
            }
            UiPrimitive::RoundedRect {
                rect,
                fill,
                border_color,
                border_width,
                corner_radius,
                shadow,
            } => {
                // 1. Root viewport frame background (index 0) -> CANVAS (layer 0)
                if idx == 0 && rects_match(*rect, ui_out.chrome.viewport) {
                    draw_list.with_layer(layers::CANVAS, |dl| {
                        push_ui_rounded_rect(
                            dl,
                            *rect,
                            *fill,
                            *border_color,
                            *border_width,
                            *corner_radius,
                            *shadow,
                        );
                    });
                    continue;
                }

                // 2. Tiled pane card fill -> CANVAS (layer 0, strictly behind terminal grid 10..50);
                //    its border (active accent outline) draws on CHROME (layer 60, above terminal edges).
                if clip_depth == 0 && tiled_cards.iter().any(|&c| rects_match(*rect, c)) {
                    draw_list.with_layer(layers::CANVAS, |dl| {
                        push_ui_rounded_rect(dl, *rect, *fill, None, 0.0, *corner_radius, *shadow);
                    });
                    if let Some(bc) = *border_color
                        && *border_width > 0.0
                    {
                        draw_list.with_layer(layers::CHROME, |dl| {
                            dl.push_rounded_rect(
                                rectf_to_gpu(*rect),
                                CornerRadii::uniform(*corner_radius),
                                Color::TRANSPARENT,
                                Color::from(bc),
                                *border_width,
                            );
                        });
                    }
                    continue;
                }

                // 3. Floating pane card (e.g. `tr3x` `/pr` drawer): bump layer, draw shadow + card
                //    fill, splice its terminal frame cleanly ABOVE the fill, then bump for its header!
                if clip_depth == 0
                    && let Some(&(_, floating_id)) =
                        floating_cards.iter().find(|&&(c, _)| rects_match(*rect, c))
                {
                    current_layer = current_layer.max(layers::FLOATING) + 1;
                    layer_has_foreground = false;

                    draw_list.with_layer(current_layer, |dl| {
                        push_ui_rounded_rect(dl, *rect, *fill, None, 0.0, *corner_radius, *shadow);
                    });

                    if let Some(f_frame) = floating_terminal_frames.get(floating_id) {
                        let base_shift = current_layer + 1;
                        let max_used = splice_scene_frame(f_frame, base_shift, draw_list);
                        current_layer = max_used + 1;
                    } else {
                        current_layer += 1;
                    }

                    if let Some(bc) = *border_color
                        && *border_width > 0.0
                    {
                        draw_list.with_layer(current_layer, |dl| {
                            dl.push_rounded_rect(
                                rectf_to_gpu(*rect),
                                CornerRadii::uniform(*corner_radius),
                                Color::TRANSPARENT,
                                Color::from(bc),
                                *border_width,
                            );
                        });
                    }
                    continue;
                }

                // 4. General UI rect: if prior glyphs/icons exist on `current_layer`, advance to
                //    the next Z-layer so overlapping cards/modals/menus always respect painter's order!
                if layer_has_foreground {
                    current_layer += 1;
                    layer_has_foreground = false;
                }
                draw_list.with_layer(current_layer, |dl| {
                    push_ui_rounded_rect(
                        dl,
                        *rect,
                        *fill,
                        *border_color,
                        *border_width,
                        *corner_radius,
                        *shadow,
                    );
                });
            }
            UiPrimitive::ProgressBar {
                rect,
                fraction,
                fill,
                track,
                corner_radius,
            } => {
                if layer_has_foreground {
                    current_layer += 1;
                    layer_has_foreground = false;
                }
                draw_list.with_layer(current_layer, |dl| {
                    let r = rectf_to_gpu(*rect);
                    let radii = CornerRadii::uniform(*corner_radius);
                    if track.a > 0 {
                        dl.push_rounded_rect(
                            r,
                            radii,
                            Color::from(*track),
                            Color::TRANSPARENT,
                            0.0,
                        );
                    }
                    let fill_w = (r.width * fraction.clamp(0.0, 1.0)).round();
                    if fill_w > 0.0 && fill.a > 0 {
                        dl.push_rounded_rect(
                            Rect::new(r.x, r.y, fill_w, r.height),
                            radii,
                            Color::from(*fill),
                            Color::TRANSPARENT,
                            0.0,
                        );
                    }
                });
            }
            UiPrimitive::Icon { rect, icon, tint } => {
                layer_has_foreground = true;
                draw_list.with_layer(current_layer, |dl| {
                    dl.push_icon(icon.name(), rectf_to_gpu(*rect), Some(Color::from(*tint)));
                });
            }
            UiPrimitive::Text {
                rect,
                text,
                color,
                font,
                size_px,
                weight,
                align,
                ellipsis,
                highlight_ranges,
                highlight_color,
            } => {
                layer_has_foreground = true;
                let family = match font {
                    FontKind::Sans => FontFamily::SansSerif,
                    FontKind::Mono => FontFamily::Monospace,
                };
                let gpu_weight = match weight {
                    UiFontWeight::Regular => GpuFontWeight::Regular,
                    UiFontWeight::Medium => GpuFontWeight::Medium,
                    UiFontWeight::SemiBold => GpuFontWeight::SemiBold,
                    UiFontWeight::Bold => GpuFontWeight::Bold,
                };
                let gpu_align = match align {
                    UiTextAlign::Left => GpuTextAlign::Left,
                    UiTextAlign::Center => GpuTextAlign::Center,
                    UiTextAlign::Right => GpuTextAlign::Right,
                };
                let line_height = (*size_px * 1.25).ceil();
                let y = rect.y + ((rect.h - line_height) * 0.5).max(0.0);
                let display_text = if *ellipsis {
                    ellipsize_to_width(
                        atlas,
                        text,
                        *size_px,
                        line_height,
                        &family,
                        gpu_weight,
                        rect.w,
                    )
                } else {
                    text.clone()
                };

                let text_clip = rectf_to_gpu(*rect).to_clip_rect();
                draw_list.with_layer(current_layer, |dl| {
                    if !highlight_ranges.is_empty()
                        && let Some(hl_col) = *highlight_color
                        && !*ellipsis
                        && *align == UiTextAlign::Left
                    {
                        emit_highlighted_text_runs(
                            dl,
                            atlas,
                            &display_text,
                            rect.x,
                            y,
                            *size_px,
                            line_height,
                            &family,
                            gpu_weight,
                            Color::from(*color),
                            Color::from(hl_col),
                            highlight_ranges,
                            text_clip,
                        );
                    } else {
                        let mut run =
                            TextRun::new(display_text, rect.x, y, *size_px, Color::from(*color))
                                .line_height(line_height)
                                .weight(gpu_weight)
                                .align(gpu_align);
                        run.family = family;
                        if *align != UiTextAlign::Left {
                            run.max_width = Some(rect.w);
                        }
                        run.clip_rect = text_clip;
                        dl.push_text(run);
                    }
                });
            }
        }
    }

    // Balance any stray open clips defensively.
    for _ in 0..clip_depth {
        draw_list.pop_clip();
    }

    // Render hover preview image GPU quad inside its reserved card slot if active.
    if let Some((slot, image_id)) = hover_image {
        let target_layer = current_layer + 1;
        draw_list.with_layer(target_layer, |dl| {
            let mut quad = ImageQuad::new(image_id, rectf_to_gpu(slot));
            quad.corner_radius = 6.0;
            dl.push_image(quad);
        });
    }
}

fn push_ui_rounded_rect(
    dl: &mut DrawList,
    rect: RectF,
    fill: Rgba,
    border_color: Option<Rgba>,
    border_width: f32,
    corner_radius: f32,
    shadow: Option<ui_chrome::ShadowSpec>,
) {
    let r = rectf_to_gpu(rect);
    let radii = CornerRadii::uniform(corner_radius);
    if let Some(sh) = shadow
        && sh.color.a > 0
    {
        let spread = sh.spread;
        let sh_rect = Rect::new(
            r.x - spread,
            r.y - spread,
            (r.width + spread * 2.0).max(0.0),
            (r.height + spread * 2.0).max(0.0),
        );
        dl.push_shadow(
            sh_rect,
            radii,
            Color::from(sh.color),
            sh.blur_radius,
            sh.offset_x,
            sh.offset_y,
        );
    }
    let bc = border_color.map(Color::from).unwrap_or(Color::TRANSPARENT);
    let bw = if border_color.is_some() {
        border_width
    } else {
        0.0
    };
    if fill.a > 0 || (bc.a > 0.0 && bw > 0.0) {
        dl.push_rounded_rect(r, radii, Color::from(fill), bc, bw);
    }
}

fn splice_scene_frame(src: &SceneFrame, base_layer: i32, dst: &mut DrawList) -> i32 {
    let mut max_layer = base_layer;
    for (idx, (_orig_z, content)) in src.layers.iter().enumerate() {
        let target_z = base_layer + (idx as i32);
        max_layer = max_layer.max(target_z);
        if !content.quads.is_empty() {
            dst.extend_raw_quads(target_z, content.quads.iter().copied());
        }
        if !content.raw_glyphs.is_empty() {
            dst.extend_raw_glyphs(target_z, content.raw_glyphs.iter().copied());
        }
        if !content.images.is_empty() {
            dst.with_layer(target_z, |dl| {
                for img in &content.images {
                    dl.push_image(img.clone());
                }
            });
        }
    }
    max_layer
}

#[allow(clippy::too_many_arguments)]
fn emit_highlighted_text_runs(
    dl: &mut DrawList,
    atlas: &mut GlyphAtlas,
    text: &str,
    start_x: f32,
    y: f32,
    size_px: f32,
    line_height: f32,
    family: &FontFamily,
    weight: GpuFontWeight,
    base_color: Color,
    hl_color: Color,
    ranges: &[Range<usize>],
    clip_rect: ClipRect,
) {
    let mut cursor_byte = 0usize;
    let mut cur_x = start_x;
    for r in ranges {
        let s = r.start.min(text.len());
        let e = r.end.min(text.len());
        if s > cursor_byte && text.is_char_boundary(cursor_byte) && text.is_char_boundary(s) {
            let seg = &text[cursor_byte..s];
            let (w, _) = atlas.measure_text(
                seg,
                size_px,
                line_height,
                family,
                weight,
                FontStyle::Normal,
                None,
            );
            let mut run = TextRun::new(seg, cur_x, y, size_px, base_color)
                .line_height(line_height)
                .weight(weight);
            run.family = family.clone();
            run.clip_rect = clip_rect;
            dl.push_text(run);
            cur_x += w;
        }
        if e > s && text.is_char_boundary(s) && text.is_char_boundary(e) {
            let seg = &text[s..e];
            let (w, _) = atlas.measure_text(
                seg,
                size_px,
                line_height,
                family,
                GpuFontWeight::Bold,
                FontStyle::Normal,
                None,
            );
            let mut run = TextRun::new(seg, cur_x, y, size_px, hl_color)
                .line_height(line_height)
                .weight(GpuFontWeight::Bold);
            run.family = family.clone();
            run.clip_rect = clip_rect;
            dl.push_text(run);
            cur_x += w;
        }
        cursor_byte = cursor_byte.max(e);
    }
    if cursor_byte < text.len() && text.is_char_boundary(cursor_byte) {
        let seg = &text[cursor_byte..];
        let mut run = TextRun::new(seg, cur_x, y, size_px, base_color)
            .line_height(line_height)
            .weight(weight);
        run.family = family.clone();
        run.clip_rect = clip_rect;
        dl.push_text(run);
    }
}

fn ellipsize_to_width(
    atlas: &mut GlyphAtlas,
    text: &str,
    size_px: f32,
    line_height: f32,
    family: &FontFamily,
    weight: GpuFontWeight,
    max_width: f32,
) -> String {
    if max_width <= 8.0 || text.is_empty() {
        return text.to_owned();
    }
    // Fast conservative length check avoids measuring short labels that obviously fit.
    let approx_max_px = (text.chars().count() as f32) * size_px * 0.68;
    if approx_max_px <= max_width {
        return text.to_owned();
    }
    let (full_w, _) = atlas.measure_text(
        text,
        size_px,
        line_height,
        family,
        weight,
        FontStyle::Normal,
        None,
    );
    if full_w <= max_width {
        return text.to_owned();
    }

    let (ell_w, _) = atlas.measure_text(
        "…",
        size_px,
        line_height,
        family,
        weight,
        FontStyle::Normal,
        None,
    );
    let target = (max_width - ell_w).max(0.0);
    let char_indices: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let mut lo = 0usize;
    let mut hi = char_indices.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let end = if mid == char_indices.len() {
            text.len()
        } else {
            char_indices[mid]
        };
        let (w, _) = atlas.measure_text(
            &text[..end],
            size_px,
            line_height,
            family,
            weight,
            FontStyle::Normal,
            None,
        );
        if w <= target {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let cut = if lo == char_indices.len() {
        text.len()
    } else {
        char_indices[lo]
    };
    let trimmed = text[..cut].trim_end();
    format!("{trimmed}…")
}

// ============================================================================
// 6. Headless Live Render Entrypoint (`--headless-render`)
// ============================================================================

/// Connects read-only to live `rex-server`, waits up to 3 seconds for `session.bootstrap`,
/// active `SessionView`, and `READY` snapshot of the active pane(s), then renders the full
/// composite `omarex` window (sidebar + live tabs + decoded terminal + live Omarchy theme)
/// offscreen on Vulkan to `out_path`.
pub fn render_headless_live_png(
    server_override: Option<&str>,
    session_filter: Option<&str>,
    width: u32,
    height: u32,
    scale_factor: f32,
    out_path: impl AsRef<Path>,
) -> Result<HeadlessRenderReport> {
    let opts = HeadlessRenderOptions {
        server_override: server_override.map(str::to_owned),
        session_filter: session_filter.map(str::to_owned),
        width,
        height,
        scale_factor,
        force_light: false,
    };
    render_headless_live_png_with_options(&opts, out_path)
}

/// Configurable headless live render supporting `--light` theme override in addition to
/// server, session, dimensions, and HiDPI scale factor.
pub fn render_headless_live_png_with_options(
    opts: &HeadlessRenderOptions,
    out_path: impl AsRef<Path>,
) -> Result<HeadlessRenderReport> {
    let endpoint = match opts.server_override.as_deref() {
        Some(raw) => RexEndpoint::parse(raw).context("failed to parse --server endpoint")?,
        None => RexEndpoint::discover_default(),
    };

    let mut settings = UserSettings::load_or_default();
    if opts.server_override.is_some() {
        settings.saved_hosts.clear(); // Explicit `--server` targets that single endpoint only.
    }
    let mut theme = Theme::load_live();
    if opts.force_light {
        theme = Theme::from_parts(
            omarchy_theme::ThemePalette::default_light(),
            theme.rounding,
            theme.material,
        );
    }

    let mut controller = OmarexController::new(settings, UserSettings::default_path(), theme)?;
    controller.read_only = true;
    controller.clipboard_enabled = false;
    controller.set_preferred_session(opts.session_filter.clone());
    controller.set_viewport(opts.width, opts.height, opts.scale_factor);

    let mut renderer = GpuRenderer::new_headless(GpuConfig::default())
        .context("failed to initialize headless wgpu renderer")?;
    register_all_ui_icons(renderer.atlas_mut());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("failed to build Tokio runtime for headless render")?;

    let mut host_connected = false;
    rt.block_on(async {
        let mut hm_cfg = controller.build_host_manager_config(Some(endpoint), None);
        hm_cfg.start_theme_watchers = false;
        hm_cfg.start_notifier = false;

        let (hm, mut rx) = HostManager::spawn(hm_cfg);
        controller.attach_host_manager(hm.clone());

        let expected_remote_hosts: HashSet<String> = controller
            .settings
            .saved_hosts
            .iter()
            .filter(|h| h.auto_connect)
            .map(|h| h.id.clone())
            .collect();
        let mut bootstrapped_hosts: HashSet<String> = HashSet::new();
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(4);
        let remote_grace = Duration::from_millis(1600);
        let mut ready_since: Option<tokio::time::Instant> = None;

        while tokio::time::Instant::now() < deadline {
            // Once bootstrap + active view + active pane snapshot READY + metadata hydrated
            // (and any auto-connect remote hosts bootstrapped or grace window elapsed),
            // settle briefly (~180ms) for trailing history/status metadata, then exit for capture.
            let remotes_settled = expected_remote_hosts
                .iter()
                .all(|id| bootstrapped_hosts.contains(id))
                || started.elapsed() >= remote_grace;
            if let Some(since) = ready_since
                && since.elapsed() >= Duration::from_millis(180)
                && remotes_settled
            {
                break;
            }
            let wait_for = if let Some(since) = ready_since
                && remotes_settled
            {
                Duration::from_millis(180).saturating_sub(since.elapsed())
            } else {
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(Duration::from_millis(25))
            };

            match tokio::time::timeout(wait_for, rx.recv()).await {
                Ok(Some(ev)) => {
                    if matches!(ev, RuntimeEvent::HostConnected { .. }) {
                        host_connected = true;
                    }
                    if let RuntimeEvent::BootstrapReady { ref host_id, .. } = ev {
                        bootstrapped_hosts.insert(host_id.clone());
                    }
                    controller.process_runtime_event(ev, Instant::now());
                    // Drain any immediately buffered events in one batch.
                    while let Ok(more) = rx.try_recv() {
                        if matches!(more, RuntimeEvent::HostConnected { .. }) {
                            host_connected = true;
                        }
                        if let RuntimeEvent::BootstrapReady { ref host_id, .. } = more {
                            bootstrapped_hosts.insert(host_id.clone());
                        }
                        controller.process_runtime_event(more, Instant::now());
                    }
                    let _ = controller.tick(Instant::now());

                    if ready_since.is_none()
                        && controller.has_ready_active_pane()
                        && (controller.metadata_hydrated_count > 0
                            || controller.hydrated_blocks.is_empty())
                    {
                        ready_since = Some(tokio::time::Instant::now());
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    let _ = controller.tick(Instant::now());
                    if ready_since.is_none() && controller.has_ready_active_pane() {
                        ready_since = Some(tokio::time::Instant::now());
                    }
                }
            }
        }

        // Perform one final background snapshot step before rendering.
        let _ = controller
            .pane_store
            .step_background_snapshots(Duration::from_millis(10), &mut controller.workspace);
        // Ensure local dark/light palette (e.g. `--light`) governs default fg/bg/ANSI colors
        // even if the read-only live server stream emitted a dark `rex1;` theme APC.
        let _ = controller
            .pane_store
            .apply_theme_all(&controller.theme.palette);

        hm.shutdown();
        tokio::time::sleep(Duration::from_millis(30)).await;
    });

    // Warm up glyphs/icons on first frame and render final PNG.
    let frame = controller.render_frame(&mut renderer, Instant::now())?;
    let frame_stats = renderer.render_to_png(&frame, out_path.as_ref())?;

    let (active_session_label, total_windows) = controller
        .workspace
        .active_session
        .as_ref()
        .and_then(|sk| controller.workspace.session(sk))
        .map(|s| {
            (
                Some(s.label.clone()),
                s.view.as_ref().map_or(0, |v| v.windows.len()),
            )
        })
        .unwrap_or((None, 0));

    let ready_panes = controller
        .pane_store
        .pane_keys()
        .iter()
        .filter(|k| controller.pane_store.is_ready(k))
        .count();

    Ok(HeadlessRenderReport {
        output_path: out_path.as_ref().to_path_buf(),
        width: opts.width,
        height: opts.height,
        scale_factor: opts.scale_factor,
        host_connected,
        active_session_label,
        total_windows,
        ready_panes,
        frame_stats,
    })
}

impl OmarexController {
    fn has_ready_active_pane(&self) -> bool {
        let Some(sk) = self.workspace.active_session.as_ref() else {
            return false;
        };
        let Some(view) = self.workspace.session(sk).and_then(|s| s.view.as_ref()) else {
            return false;
        };
        let Some(win) = active_window_of_view(view) else {
            return false;
        };
        win.layers
            .iter()
            .flat_map(|l| l.blocks.iter())
            .filter_map(|b| b.block_id.as_ref())
            .any(|bid| {
                self.pane_store
                    .is_ready(&BlockKey::new(&sk.host_id, &sk.session_id, bid))
            })
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

#[inline]
fn rects_match(a: RectF, b: RectF) -> bool {
    (a.x - b.x).abs() < 0.5
        && (a.y - b.y).abs() < 0.5
        && (a.w - b.w).abs() < 0.5
        && (a.h - b.h).abs() < 0.5
}

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

fn bridge_mods_to_vt(m: Modifiers) -> VtModifiers {
    let mut out = VtModifiers::NONE;
    if m.shift {
        out |= VtModifiers::SHIFT;
    }
    if m.ctrl {
        out |= VtModifiers::CTRL;
    }
    if m.alt {
        out |= VtModifiers::ALT;
    }
    if m.super_key {
        out |= VtModifiers::SUPER;
    }
    out
}

fn key_input_to_modal_key(input: &KeyInputEvent) -> Option<ModalKey> {
    match input.canonical_key.as_str() {
        "up" => Some(ModalKey::Up),
        "down" => Some(ModalKey::Down),
        "page_up" => Some(ModalKey::PageUp),
        "page_down" => Some(ModalKey::PageDown),
        "enter" if input.mods.ctrl => Some(ModalKey::CtrlEnter),
        "enter" => Some(ModalKey::Enter),
        "escape" => Some(ModalKey::Escape),
        "backspace" if input.mods.ctrl => Some(ModalKey::WordBackspace),
        "backspace" => Some(ModalKey::Backspace),
        "delete" => Some(ModalKey::Delete),
        "left" => Some(ModalKey::Left {
            extend: input.mods.shift,
        }),
        "right" => Some(ModalKey::Right {
            extend: input.mods.shift,
        }),
        "home" => Some(ModalKey::Home {
            extend: input.mods.shift,
        }),
        "end" => Some(ModalKey::End {
            extend: input.mods.shift,
        }),
        "a" if input.mods.ctrl && !input.mods.alt => Some(ModalKey::SelectAll),
        "space" if !input.mods.ctrl && !input.mods.alt => Some(ModalKey::Space),
        _ => {
            if !input.mods.ctrl
                && !input.mods.alt
                && !input.mods.super_key
                && let Some(text) = &input.text
            {
                let mut chars = text.chars();
                if let (Some(ch), None) = (chars.next(), chars.next())
                    && !ch.is_control()
                {
                    return Some(ModalKey::Char(ch));
                }
                if !text.is_empty() && !text.chars().all(char::is_control) {
                    return Some(ModalKey::Paste(text.clone()));
                }
            }
            None
        }
    }
}

fn apply_key_to_text_input(input: &mut ui_chrome::TextInput, ev: &KeyInputEvent) -> bool {
    match ev.canonical_key.as_str() {
        "backspace" if ev.mods.ctrl => input.delete_word_backward(),
        "backspace" => input.backspace(),
        "delete" => input.delete_forward(),
        "left" => {
            input.move_left(ev.mods.shift);
            false
        }
        "right" => {
            input.move_right(ev.mods.shift);
            false
        }
        "home" => {
            input.move_home(ev.mods.shift);
            false
        }
        "end" => {
            input.move_end(ev.mods.shift);
            false
        }
        "a" if ev.mods.ctrl => {
            input.select_all();
            false
        }
        _ => {
            if !ev.mods.ctrl
                && !ev.mods.alt
                && !ev.mods.super_key
                && let Some(text) = &ev.text
                && !text.is_empty()
                && !text.chars().all(char::is_control)
            {
                input.insert_str(text);
                return true;
            }
            false
        }
    }
}

fn compute_floating_drag_bounds(
    start: [f32; 4],
    zone: FloatingHitZone,
    dx: f32,
    dy: f32,
) -> [f32; 4] {
    let [mut x, mut y, mut w, mut h] = start;
    let min_w = 0.15f32;
    let min_h = 0.15f32;
    match zone {
        FloatingHitZone::MoveHeader => {
            x = (x + dx).clamp(0.0, (1.0 - w).max(0.0));
            y = (y + dy).clamp(0.0, (1.0 - h).max(0.0));
        }
        FloatingHitZone::ResizeE => {
            w = (w + dx).clamp(min_w, (1.0 - x).max(min_w));
        }
        FloatingHitZone::ResizeS => {
            h = (h + dy).clamp(min_h, (1.0 - y).max(min_h));
        }
        FloatingHitZone::ResizeSE => {
            w = (w + dx).clamp(min_w, (1.0 - x).max(min_w));
            h = (h + dy).clamp(min_h, (1.0 - y).max(min_h));
        }
        FloatingHitZone::ResizeW => {
            let nx = (x + dx).clamp(0.0, (x + w - min_w).max(0.0));
            w += x - nx;
            x = nx;
        }
        FloatingHitZone::ResizeN => {
            let ny = (y + dy).clamp(0.0, (y + h - min_h).max(0.0));
            h += y - ny;
            y = ny;
        }
        FloatingHitZone::ResizeNW => {
            let nx = (x + dx).clamp(0.0, (x + w - min_w).max(0.0));
            let ny = (y + dy).clamp(0.0, (y + h - min_h).max(0.0));
            w += x - nx;
            h += y - ny;
            x = nx;
            y = ny;
        }
        FloatingHitZone::ResizeNE => {
            let ny = (y + dy).clamp(0.0, (y + h - min_h).max(0.0));
            h += y - ny;
            y = ny;
            w = (w + dx).clamp(min_w, (1.0 - x).max(min_w));
        }
        FloatingHitZone::ResizeSW => {
            let nx = (x + dx).clamp(0.0, (x + w - min_w).max(0.0));
            w += x - nx;
            x = nx;
            h = (h + dy).clamp(min_h, (1.0 - y).max(min_h));
        }
        FloatingHitZone::Content => {}
    }
    [x, y, w, h]
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

fn shell_escape_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':'))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
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

fn infer_consumed_mods(
    mods: Modifiers,
    text: Option<&str>,
    unshifted: Option<char>,
) -> VtModifiers {
    let mut consumed = VtModifiers::NONE;
    let Some(t) = text.filter(|s| !s.is_empty() && !s.chars().all(char::is_control)) else {
        return consumed;
    };
    if mods.ctrl || mods.super_key {
        return consumed;
    }
    let mut chars = t.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    let transformed = match (single, unshifted) {
        (Some(produced), Some(base)) => produced != base,
        _ => false,
    };
    if mods.shift && transformed {
        consumed |= VtModifiers::SHIFT;
    }
    // AltGr on German/international layouts transforms keys (e.g. `q` -> `@`, `7` -> `{`, `e` -> `€`).
    // Plain Meta-key shortcuts like `Alt+f` keep `produced == 'f' == unshifted` so ALT stays unconsumed.
    if mods.alt && transformed {
        consumed |= VtModifiers::ALT;
    }
    consumed
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

/// Zero-dependency RFC 3339 timestamp parser (`YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`),
/// used to convert Rex OSC 7501 `updated_at` strings into exact Unix epoch seconds.
fn parse_rfc3339_unix_secs(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.len() < 20 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't' && b[10] != b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let minute: i64 = s[14..16].parse().ok()?;
    let second: i64 = s[17..19].parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Find timezone suffix (`Z`, `z`, `+HH:MM`, or `-HH:MM`) after optional fractional seconds.
    let mut tz_idx = 19usize;
    if tz_idx < b.len() && b[tz_idx] == b'.' {
        tz_idx += 1;
        while tz_idx < b.len() && b[tz_idx].is_ascii_digit() {
            tz_idx += 1;
        }
    }
    let tz_offset_secs: i64 = if tz_idx >= b.len() || b[tz_idx] == b'Z' || b[tz_idx] == b'z' {
        0
    } else if (b[tz_idx] == b'+' || b[tz_idx] == b'-') && tz_idx + 6 <= b.len() {
        let sign = if b[tz_idx] == b'+' { 1 } else { -1 };
        let tz_h: i64 = s[tz_idx + 1..tz_idx + 3].parse().ok()?;
        let tz_m: i64 = s[tz_idx + 4..tz_idx + 6].parse().ok()?;
        sign * (tz_h * 3600 + tz_m * 60)
    } else {
        return None;
    };

    // Howard Hinnant's civil-to-days algorithm (exact Gregorian days since 1970-01-01).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + (doe as i64) - 719_468;

    Some(days * 86_400 + hour * 3600 + minute * 60 + second - tz_offset_secs)
}

fn get_system_clipboard_text() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get_text().ok()
}

fn get_primary_selection_text() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        let mut cb = arboard::Clipboard::new().ok()?;
        cb.get()
            .clipboard(LinuxClipboardKind::Primary)
            .text()
            .ok()
            .filter(|s| !s.is_empty())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn looks_like_remote_file_path(path: &str) -> bool {
    if path.starts_with('/')
        || path.starts_with("~/")
        || path.starts_with("./")
        || path.starts_with("../")
    {
        return true;
    }
    // For bare relative paths on a remote host, require a realistic filename extension in the
    // final component so natural prose like `iPhone/Mac` never triggers a remote file hover.
    let last = path.rsplit('/').next().unwrap_or(path);
    if let Some((stem, ext)) = last.rsplit_once('.') {
        !stem.is_empty()
            && (1..=10).contains(&ext.len())
            && ext.bytes().all(|b| b.is_ascii_alphanumeric())
    } else {
        false
    }
}

fn is_previewable_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
            )
        })
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

fn remote_download_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("omarex")
        .join("downloads")
}

fn get_wayland_clipboard_uri_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Vec::new();
        }
        let Ok(types_out) = std::process::Command::new("wl-paste")
            .arg("--list-types")
            .output()
        else {
            return Vec::new();
        };
        if !types_out.status.success() {
            return Vec::new();
        }
        let offered = String::from_utf8_lossy(&types_out.stdout);
        let has_uri_list = offered.lines().any(|l| l.trim() == "text/uri-list");
        let has_gnome = offered
            .lines()
            .any(|l| l.trim() == "x-special/gnome-copied-files");

        if has_uri_list
            && let Ok(out) = std::process::Command::new("wl-paste")
                .args(["--no-newline", "--type", "text/uri-list"])
                .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let paths = crate::wayland_dnd::parse_uri_list(&text);
            if !paths.is_empty() {
                return paths;
            }
        }
        if has_gnome
            && let Ok(out) = std::process::Command::new("wl-paste")
                .args(["--no-newline", "--type", "x-special/gnome-copied-files"])
                .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let paths = crate::wayland_dnd::parse_gnome_copied_files(&text);
            if !paths.is_empty() {
                return paths;
            }
        }
        Vec::new()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

fn save_clipboard_image_to_temp_file(seq: u64) -> Option<PathBuf> {
    if let Some(path) = save_clipboard_image_to_temp_png(seq) {
        return Some(path);
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("WAYLAND_DISPLAY")?;
        let types_out = std::process::Command::new("wl-paste")
            .arg("--list-types")
            .output()
            .ok()?;
        if !types_out.status.success() {
            return None;
        }
        let offered: Vec<&str> = std::str::from_utf8(&types_out.stdout)
            .ok()?
            .lines()
            .map(str::trim)
            .collect();
        for (mime, ext) in [
            ("image/png", "png"),
            ("image/webp", "webp"),
            ("image/jpeg", "jpg"),
            ("image/gif", "gif"),
        ] {
            if offered.contains(&mime)
                && let Ok(out) = std::process::Command::new("wl-paste")
                    .args(["--type", mime])
                    .output()
                && out.status.success()
                && !out.stdout.is_empty()
            {
                let path = std::env::temp_dir()
                    .join(format!("omarex-clip-{}-{seq}.{ext}", std::process::id()));
                if std::fs::write(&path, &out.stdout).is_ok() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn save_clipboard_image_to_temp_png(seq: u64) -> Option<PathBuf> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let img = cb.get_image().ok()?;
    let w = u32::try_from(img.width).ok()?;
    let h = u32::try_from(img.height).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let path = std::env::temp_dir().join(format!("omarex-clip-{}-{seq}.png", std::process::id()));
    gpu_renderer::save_rgba_png(&path, w, h, &img.bytes).ok()?;
    Some(path)
}

fn set_system_clipboard_text(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_owned());
    }
}

// ============================================================================
// Tests (pure unit + live read-only full omarex window Vulkan PNG capture!)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use omarchy_theme::ThemeEvent;
    use rex_proto::types::{
        BlockView, ClientActionResponse, ClientPerformRequestedEvent, ClientPromptEndedEvent,
        ClientPromptRequestedEvent, LayerView, Principal, PromptIcon, PromptItem, PromptKind,
        PromptSpec, QueuedClientAction, RequesterInfo, SessionSummary,
    };

    fn make_test_controller() -> OmarexController {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let cfg_path = std::env::temp_dir()
            .join(format!("omarex-bridge-test-{}-{nanos}", std::process::id()))
            .join("settings.json");
        let mut ctrl = OmarexController::new(UserSettings::default(), cfg_path, Theme::default())
            .expect("create controller");
        ctrl.clipboard_enabled = false;
        ctrl
    }

    fn seed_single_pane_workspace(ctrl: &mut OmarexController) {
        let mut host = HostEntry::new(
            LOCAL_HOST_ID,
            "This Machine",
            RexEndpoint::parse("unix:///tmp/test.sock").unwrap(),
        );
        host.status = HostStatus::Connected {
            client_id: "client:1".into(),
            server_host_id: Some("host:local".into()),
            api_revision: 24,
            resumed: false,
        };
        host.on_session_created(SessionSummary {
            session_id: "session:t3".into(),
            label: "T3 Code".into(),
        });
        ctrl.workspace.add_host(host);
        ctrl.workspace
            .select_session(SessionKey::new(LOCAL_HOST_ID, "session:t3"));

        let view = SessionView {
            revision: 1,
            session_id: Some("session:t3".into()),
            label: "T3 Code".into(),
            active_window_id: Some("window:1".into()),
            windows: vec![WindowView {
                window_id: Some("window:1".into()),
                label: "Build bridge.rs".into(),
                focused_block_id: Some("block:1".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![LayerView {
                    layer_id: Some("layer:tiled".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: Some(LayoutNode {
                        rect: [0.0, 0.0, 1.0, 1.0],
                        block_id: Some("block:1".into()),
                        split: None,
                    }),
                    blocks: vec![BlockView {
                        block_id: Some("block:1".into()),
                        label: "t3:1baa1939/thread-1".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:local".into()),
                        owner: Some("client:1".into()),
                    }],
                }],
            }],
        };

        ctrl.process_runtime_event(
            RuntimeEvent::SessionViewUpdated {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                view: Box::new(view),
                seq: 1,
            },
            Instant::now(),
        );

        let term = vt_engine::Terminal::new(100, 32, vt_engine::TerminalConfig::default())
            .expect("create test terminal");
        term.write(b"\x1b[1;32m[omarex]\x1b[0m bridge test terminal ready\r\n");
        ctrl.pane_store
            .insert_terminal(BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1"), term)
            .expect("insert test terminal");
        ctrl.drain_pending_commands();
    }

    #[test]
    fn registers_all_40_embedded_icons_and_translates_ui_to_vulkan_frame() {
        let mut renderer =
            GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
        let count = register_all_ui_icons(renderer.atlas_mut());
        assert_eq!(count, 40);

        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        ctrl.set_viewport(1280, 720, 1.0);
        // Open Command Palette over live pane to verify modal Z-ordering & clip balance.
        ctrl.dispatch_ui_action(
            UiAction::OpenModal(ModalKind::CommandPalette),
            Instant::now(),
        );

        let frame = ctrl
            .render_frame(&mut renderer, Instant::now())
            .expect("build composite SceneFrame");
        assert!(frame.layers.contains_key(&layers::CANVAS));
        assert!(frame.layers.contains_key(&layers::TERMINAL_FG));
        assert!(frame.layers.keys().any(|&z| z >= layers::CHROME));

        let (rgba, stats) = renderer
            .render_to_rgba(&frame)
            .expect("render composite frame on Vulkan");
        assert_eq!(rgba.len(), 1280 * 720 * 4);
        assert!(stats.quad_count > 10);
        assert!(stats.glyph_count > 40);
    }

    #[test]
    fn rex_ui_prompt_lifecycle_focus_guard_and_fast_reply() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let now = Instant::now();

        // 1. Incoming `rex.ui.pick` opens native modal cleanly.
        ctrl.process_runtime_event(
            RuntimeEvent::PromptRequested {
                host_id: LOCAL_HOST_ID.into(),
                seq: 10,
                request: Box::new(ClientPromptRequestedEvent {
                    call_id: "call-42".into(),
                    action: Some("t3_answer".into()),
                    prompt: PromptSpec {
                        kind: PromptKind::Pick,
                        title: "Approve command?".into(),
                        placeholder: Some("Choose action".into()),
                        value: None,
                        confirm: Some("Confirm".into()),
                        multiple: false,
                        allow_other: true,
                        items: vec![
                            PromptItem {
                                id: "approve".into(),
                                title: "Approve".into(),
                                subtitle: Some("cargo test".into()),
                                icon: Some(PromptIcon::Terminal),
                                detail: Some("Allow".into()),
                                selected: false,
                            },
                            PromptItem {
                                id: "decline".into(),
                                title: "Decline".into(),
                                subtitle: None,
                                icon: None,
                                detail: None,
                                selected: false,
                            },
                        ],
                    },
                    timeout_ms: 30_000,
                }),
            },
            now,
        );
        assert_eq!(
            ctrl.ui.focus.current(),
            &FocusTarget::Modal(ModalKind::RexPrompt)
        );

        // 2. Background `SessionViewUpdated` MUST NOT steal focus from the prompt modal!
        let mut updated_view = ctrl
            .workspace
            .session(&SessionKey::new(LOCAL_HOST_ID, "session:t3"))
            .unwrap()
            .view
            .clone()
            .unwrap();
        updated_view.revision = 2;
        ctrl.process_runtime_event(
            RuntimeEvent::SessionViewUpdated {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                view: Box::new(updated_view),
                seq: 11,
            },
            now,
        );
        assert_eq!(
            ctrl.ui.focus.current(),
            &FocusTarget::Modal(ModalKind::RexPrompt)
        );

        // 3. Pressing Enter submits selected item via unordered `HostCommand::ClientReply`.
        ctrl.handle_key(
            &KeyInputEvent {
                physical_key: PhysicalKey::Enter,
                canonical_key: "enter".into(),
                text: None,
                unshifted_codepoint: 13,
                mods: Modifiers::NONE,
                pressed: true,
                repeat: false,
                composing: false,
            },
            now,
        );
        assert!(matches!(ctrl.ui.focus.current(), FocusTarget::Terminal(_)));
        let cmds = ctrl.drain_pending_commands();
        assert!(cmds.iter().any(|c| matches!(
            c,
            HostCommand::ClientReply { request, .. }
                if request.call_id == "call-42" && request.error.is_none()
        )));

        // 4. Multi-client `PromptEnded` dismisses a second prompt cleanly with zero reply.
        ctrl.process_runtime_event(
            RuntimeEvent::PromptRequested {
                host_id: LOCAL_HOST_ID.into(),
                seq: 12,
                request: Box::new(ClientPromptRequestedEvent {
                    call_id: "call-43".into(),
                    action: Some("t3_rename".into()),
                    prompt: PromptSpec {
                        kind: PromptKind::Input,
                        title: "Rename Thread".into(),
                        placeholder: None,
                        value: Some("Old".into()),
                        confirm: None,
                        multiple: false,
                        allow_other: false,
                        items: vec![],
                    },
                    timeout_ms: 0,
                }),
            },
            now,
        );
        assert!(ctrl.ui.modal.is_some());
        ctrl.process_runtime_event(
            RuntimeEvent::PromptEnded {
                host_id: LOCAL_HOST_ID.into(),
                seq: 13,
                event: ClientPromptEndedEvent {
                    call_id: "call-43".into(),
                    reason: "answered_elsewhere".into(),
                },
            },
            now,
        );
        assert!(ctrl.ui.modal.is_none());
        assert!(ctrl.drain_pending_commands().is_empty());
    }

    #[test]
    fn keymap_perform_and_settings_atomic_save_work_end_to_end() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let now = Instant::now();

        // Ctrl+Shift+P opens Command Palette via KeymapEngine.
        ctrl.handle_key(
            &KeyInputEvent {
                physical_key: PhysicalKey::KeyP,
                canonical_key: "p".into(),
                text: None,
                unshifted_codepoint: b'p' as u32,
                mods: Modifiers::CTRL_SHIFT,
                pressed: true,
                repeat: false,
                composing: false,
            },
            now,
        );
        assert_eq!(
            ctrl.ui.focus.current(),
            &FocusTarget::Modal(ModalKind::CommandPalette)
        );
        ctrl.ui.close_modal();

        // Unbound key passes straight to libghostty-vt -> HostCommand::TerminalWrite.
        ctrl.handle_key(
            &KeyInputEvent {
                physical_key: PhysicalKey::KeyA,
                canonical_key: "a".into(),
                text: Some("a".into()),
                unshifted_codepoint: b'a' as u32,
                mods: Modifiers::NONE,
                pressed: true,
                repeat: false,
                composing: false,
            },
            now,
        );
        let cmds = ctrl.drain_pending_commands();
        assert!(cmds.iter().any(|c| matches!(
            c,
            HostCommand::TerminalWrite { data, .. } if data == "a"
        )));

        // PerformRequested (`session.select`) authorizes local UID and replies immediately.
        ctrl.process_runtime_event(
            RuntimeEvent::PerformRequested {
                host_id: LOCAL_HOST_ID.into(),
                seq: 20,
                request: Box::new(ClientPerformRequestedEvent {
                    call_id: "perf-1".into(),
                    action: "session.select".into(),
                    args: Some(serde_json::json!({ "session_id": "session:t3" })),
                    origin: None,
                    session_id: Some("session:t3".into()),
                    block_id: None,
                    requester: RequesterInfo {
                        client_id: "cli:1".into(),
                        info: None,
                        principal: Principal {
                            kind: "peer".into(),
                            transport: "unix".into(),
                            display: "local".into(),
                            remote_addr: None,
                            uid: current_process_uid(),
                            gid: None,
                            pid: None,
                            sid: None,
                            user_id: None,
                            login_name: None,
                            node: None,
                            tags: Vec::new(),
                            path: Vec::new(),
                        },
                    },
                }),
            },
            now,
        );
        let perf_cmds = ctrl.drain_pending_commands();
        assert!(perf_cmds.iter().any(|c| matches!(
            c,
            HostCommand::ClientReply { request, .. }
                if request.call_id == "perf-1" && request.error.is_none()
        )));

        // ClientActionCompleted inline `client_actions` executes `session.select`.
        ctrl.process_runtime_event(
            RuntimeEvent::ClientActionCompleted {
                host_id: LOCAL_HOST_ID.into(),
                action: "t3_inbox".into(),
                response: ClientActionResponse {
                    result: None,
                    client_actions: vec![QueuedClientAction {
                        action: "session.select".into(),
                        args: Some(serde_json::json!({
                            "session_id": "session:t3",
                            "block_id": "block:1"
                        })),
                    }],
                    ordered_after: None,
                },
                seq: 21,
            },
            now,
        );
        assert_eq!(ctrl.ui.focus.last_terminal_block(), Some("block:1"));

        // SettingChange updates UI & atomically persists settings.json.
        ctrl.dispatch_ui_action(
            UiAction::UpdateSetting(SettingChange::ToolbarPosition(ToolbarPosition::Right)),
            now,
        );
        let reloaded = UserSettings::load_from(&ctrl.settings_path).expect("reload saved settings");
        assert_eq!(reloaded.sidebar_position, SidebarPosition::Right);

        // ThemeChanged propagates to live Theme + PaneStore.
        let mut light_pal = omarchy_theme::ThemePalette::default_dark();
        light_pal.mode = omarchy_theme::AppearanceMode::Light;
        ctrl.process_runtime_event(
            RuntimeEvent::ThemeChanged(ThemeEvent::PaletteChanged(light_pal)),
            now,
        );
        assert!(!ctrl.theme.is_dark);

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn viewport_shrink_and_expand_claims_ownership_and_resizes_without_stale_lockout() {
        let mut renderer =
            GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
        register_all_ui_icons(renderer.atlas_mut());

        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
        let _ = ctrl.workspace.mark_snapshot_ready(&key);
        // Simulate remote Mac mini currently owning the block (as observed on live server).
        ctrl.workspace
            .note_server_resize(&key, 80, 32, Some("client:mac-mini".into()));
        let t0 = Instant::now();

        // 1. Wide viewport (1920x1080): must claim OwnMode::Take and send wide TerminalResize.
        ctrl.set_viewport(1920, 1080, 1.0);
        ctrl.on_viewport_resized(t0);
        let _ = ctrl.render_frame(&mut renderer, t0).expect("wide frame");
        let wide_cmds = ctrl.drain_pending_commands();
        assert!(
            wide_cmds.iter().any(|c| matches!(
                c,
                HostCommand::OwnBlock {
                    mode: OwnMode::Take,
                    ..
                }
            )),
            "expected OwnMode::Take when resizing focused window, got {wide_cmds:?}"
        );
        let wide_cols = wide_cmds
            .iter()
            .find_map(|c| match c {
                HostCommand::TerminalResize { args, .. } => Some((args.columns, args.rows)),
                _ => None,
            })
            .expect("expected initial wide TerminalResize");
        assert!(
            wide_cols.0 > 140,
            "expected wide viewport columns > 140, got {wide_cols:?}"
        );

        // Simulate Rex streaming back authoritative RexApc::Resize for wide grid.
        ctrl.process_runtime_event(
            RuntimeEvent::DataStream(DataStreamEvent {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                block_id: "block:1".into(),
                kind: DataStreamEventKind::RexApc(rex_proto::RexControlMessage::Resize {
                    cols: wide_cols.0,
                    rows: wide_cols.1,
                    cell_width_px: 9,
                    cell_height_px: 18,
                    owner: Some("client:1".into()),
                }),
            }),
            t0 + Duration::from_millis(5),
        );
        assert_eq!(ctrl.pane_store.size(&key), Some(wide_cols));

        // 2. Side terminal opens in Hyprland -> viewport shrinks to 960x1080.
        let t1 = t0 + Duration::from_millis(50);
        ctrl.set_viewport(960, 1080, 1.0);
        ctrl.on_viewport_resized(t1);
        let _ = ctrl.render_frame(&mut renderer, t1).expect("narrow frame");
        let narrow_cmds = ctrl.drain_pending_commands();
        let narrow_cols = narrow_cmds
            .iter()
            .find_map(|c| match c {
                HostCommand::TerminalResize { args, .. } => Some((args.columns, args.rows)),
                _ => None,
            })
            .expect("expected narrow TerminalResize after Hyprland split");
        assert!(
            narrow_cols.0 < wide_cols.0,
            "expected narrow cols ({}) < wide cols ({})",
            narrow_cols.0,
            wide_cols.0
        );

        // Simulate server resizing block down to narrow width (and side window taking ownership).
        ctrl.process_runtime_event(
            RuntimeEvent::DataStream(DataStreamEvent {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                block_id: "block:1".into(),
                kind: DataStreamEventKind::RexApc(rex_proto::RexControlMessage::Resize {
                    cols: narrow_cols.0,
                    rows: narrow_cols.1,
                    cell_width_px: 9,
                    cell_height_px: 18,
                    owner: Some("client:side-window".into()),
                }),
            }),
            t1 + Duration::from_millis(5),
        );
        assert_eq!(ctrl.pane_store.size(&key), Some(narrow_cols));

        // 3. Side terminal closes -> viewport expands back to 1920x1080.
        // MUST reclaim ownership and resend exact original wide dimensions (never locked out!).
        let t2 = t1 + Duration::from_millis(50);
        ctrl.set_viewport(1920, 1080, 1.0);
        ctrl.on_viewport_resized(t2);
        let _ = ctrl
            .render_frame(&mut renderer, t2)
            .expect("restored wide frame");
        let restored_cmds = ctrl.drain_pending_commands();
        assert!(
            restored_cmds.iter().any(|c| matches!(
                c,
                HostCommand::OwnBlock {
                    mode: OwnMode::Take,
                    ..
                }
            )),
            "expected OwnMode::Take when expanding back after side window close"
        );
        let restored_cols = restored_cmds
            .iter()
            .find_map(|c| match c {
                HostCommand::TerminalResize { args, .. } => Some((args.columns, args.rows)),
                _ => None,
            })
            .expect("expected restored wide TerminalResize (must not be dropped by stale cache!)");
        assert_eq!(restored_cols, wide_cols);

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn background_dirty_pane_does_not_spin_needs_redraw() {
        let mut renderer =
            GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
        register_all_ui_icons(renderer.atlas_mut());

        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let bg_key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:bg");
        let bg_term = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
            .expect("bg terminal");
        ctrl.pane_store
            .insert_terminal(bg_key.clone(), bg_term)
            .expect("insert bg terminal");

        // Render initial frame so visible pane (`block:1`) clears dirty and `visible_pane_keys` is populated.
        let now = Instant::now();
        ctrl.read_only = true;
        let _ = ctrl
            .render_frame(&mut renderer, now)
            .expect("initial frame");
        assert!(
            !ctrl.needs_redraw(),
            "controller must be idle immediately after rendering visible panes"
        );

        // Background tab receives live VT output -> MUST NOT cause an infinite redraw loop!
        ctrl.process_runtime_event(
            RuntimeEvent::DataStream(DataStreamEvent {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                block_id: "block:bg".into(),
                kind: DataStreamEventKind::LiveVtChunk {
                    bytes: bytes::Bytes::from_static(b"background log output\r\n"),
                    bytes_received: 23,
                },
            }),
            now + Duration::from_millis(10),
        );
        assert!(
            !ctrl.needs_redraw(),
            "dirty output on off-screen background tab must not spin needs_redraw()"
        );

        // Visible pane receives live VT output -> MUST trigger redraw immediately!
        ctrl.process_runtime_event(
            RuntimeEvent::DataStream(DataStreamEvent {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                block_id: "block:1".into(),
                kind: DataStreamEventKind::LiveVtChunk {
                    bytes: bytes::Bytes::from_static(b"visible output\r\n"),
                    bytes_received: 16,
                },
            }),
            now + Duration::from_millis(20),
        );
        assert!(
            ctrl.needs_redraw(),
            "visible pane output must mark controller dirty for repaint"
        );

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn t3_code_session_collapse_toggles_and_persists() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let now = Instant::now();

        // Collapse `T3 Code` via `ToggleSessionCollapse`.
        ctrl.dispatch_ui_action(
            UiAction::ToggleSessionCollapse {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
            },
            now,
        );
        let frame_in = ctrl.build_ui_frame_input(now);
        assert!(
            frame_in.hosts[0].sessions[0].collapsed,
            "T3 Code session should be collapsed after ToggleSessionCollapse"
        );
        let saved = UserSettings::load_from(&ctrl.settings_path).expect("reload settings");
        assert!(saved.collapsed_sessions.contains("local/session:t3"));

        // Expand `T3 Code` back cleanly via second `ToggleSessionCollapse`.
        ctrl.dispatch_ui_action(
            UiAction::ToggleSessionCollapse {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
            },
            now,
        );
        let frame_in2 = ctrl.build_ui_frame_input(now);
        assert!(
            !frame_in2.hosts[0].sessions[0].collapsed,
            "T3 Code session should expand back cleanly"
        );
        let saved2 = UserSettings::load_from(&ctrl.settings_path).expect("reload settings");
        assert!(!saved2.collapsed_sessions.contains("local/session:t3"));

        // Add a second session and verify `CollapseOtherSessions` collapses T3 Code and expands Terminal.
        ctrl.workspace
            .host_mut(LOCAL_HOST_ID)
            .unwrap()
            .on_session_created(SessionSummary {
                session_id: "session:term".into(),
                label: "Terminal".into(),
            });
        ctrl.dispatch_ui_action(
            UiAction::CollapseOtherSessions {
                keep_session_id: "session:term".into(),
            },
            now,
        );
        let frame_in3 = ctrl.build_ui_frame_input(now);
        let t3_sess = frame_in3.hosts[0]
            .sessions
            .iter()
            .find(|s| s.session_id == "session:t3")
            .unwrap();
        let term_sess = frame_in3.hosts[0]
            .sessions
            .iter()
            .find(|s| s.session_id == "session:term")
            .unwrap();
        assert!(t3_sess.collapsed);
        assert!(!term_sess.collapsed);

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn plus_opens_real_shell_terminal_instead_of_t3_thread_and_attaches() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let now = Instant::now();

        // 1. Inside `T3 Code` (with no normal session yet), pressing `+` (`UiAction::NewTerminal`)
        //    MUST create a real `"Terminal"` shell session — NEVER a window in `T3 Code` or `t3_new`!
        ctrl.dispatch_ui_action(UiAction::NewTerminal, now);
        let cmds = ctrl.drain_pending_commands();
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, HostCommand::ClientAction { request, .. } if request.action == "t3_new")),
            "global + must never invoke t3_new"
        );
        let new_sess_req = cmds
            .iter()
            .find_map(|c| match c {
                HostCommand::NewSession { request, .. } => Some(request),
                _ => None,
            })
            .expect("expected HostCommand::NewSession creating a real shell session");
        assert_eq!(new_sess_req.label.as_deref(), Some("Terminal"));
        let init_wins = new_sess_req
            .initial_windows
            .as_ref()
            .expect("NewSession must include initial shell window");
        assert_eq!(init_wins.len(), 1);
        let blk = init_wins[0]
            .layout
            .block
            .as_ref()
            .expect("initial window must contain shell block");
        assert_eq!(blk.flavor, "com.superlogical.terminal.shell");
        assert!(
            blk.options.as_ref().and_then(|o| o.get("theme")).is_some(),
            "new shell must embed live Omarchy theme"
        );

        // 2. When `SessionCreatedAndAttached` returns from `HostManager`, controller MUST switch
        //    active session, focus the new shell block, open its stream, and take ownership!
        let term_view = SessionView {
            revision: 1,
            session_id: Some("session:term1".into()),
            label: "Terminal".into(),
            active_window_id: Some("window:term1".into()),
            windows: vec![WindowView {
                window_id: Some("window:term1".into()),
                label: "zsh".into(),
                focused_block_id: Some("block:term1".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![LayerView {
                    layer_id: Some("layer:term1".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: Some(LayoutNode {
                        rect: [0.0, 0.0, 1.0, 1.0],
                        block_id: Some("block:term1".into()),
                        split: None,
                    }),
                    blocks: vec![BlockView {
                        block_id: Some("block:term1".into()),
                        label: "zsh".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:local".into()),
                        owner: None,
                    }],
                }],
            }],
        };
        ctrl.process_runtime_event(
            RuntimeEvent::SessionCreatedAndAttached {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:term1".into(),
                view: Box::new(term_view),
                seq: 5,
            },
            now,
        );
        assert_eq!(
            ctrl.active_session_key(),
            Some(&SessionKey::new(LOCAL_HOST_ID, "session:term1"))
        );
        assert_eq!(
            ctrl.ui.focus.current(),
            &FocusTarget::Terminal(Some("block:term1".into()))
        );
        let attach_cmds = ctrl.drain_pending_commands();
        assert!(attach_cmds.iter().any(|c| matches!(
            c,
            HostCommand::OpenBlockStream {
                session_id,
                block_id,
                ..
            } if session_id == "session:term1" && block_id == "block:term1"
        )));
        assert!(attach_cmds.iter().any(|c| matches!(
            c,
            HostCommand::OwnBlock {
                session_id,
                mode: OwnMode::Take,
                ..
            } if session_id == "session:term1"
        )));

        // 3. Switch back to `T3 Code` and press global `+` again -> switches to existing `"Terminal"`
        //    session and creates a real shell tab (`NewWindow`) there instead of inside `T3 Code`!
        ctrl.dispatch_ui_action(
            UiAction::SelectSession {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
            },
            now,
        );
        ctrl.drain_pending_commands();
        ctrl.dispatch_ui_action(UiAction::NewTerminal, now);
        let tab_cmds = ctrl.drain_pending_commands();
        assert!(tab_cmds.iter().any(|c| matches!(
            c,
            HostCommand::NewWindow { request, .. }
                if request.session_id == "session:term1"
                    && request
                        .layout
                        .as_ref()
                        .and_then(|l| l.block.as_ref())
                        .is_some_and(|b| b.flavor == "com.superlogical.terminal.shell")
        )));

        // 4. Explicit per-session `+` on `T3 Code` still dispatches explicit `t3_new`.
        ctrl.dispatch_ui_action(
            UiAction::NewTab {
                host_id: Some(LOCAL_HOST_ID.into()),
                session_id: Some("session:t3".into()),
            },
            now,
        );
        let t3_cmds = ctrl.drain_pending_commands();
        assert!(t3_cmds.iter().any(|c| matches!(
            c,
            HostCommand::ClientAction { host_id, request }
                if host_id == LOCAL_HOST_ID
                    && request.action == "t3_new"
                    && request.session_id.as_deref() == Some("session:t3")
        )));

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn multi_host_labels_remote_new_session_and_targeted_t3_new_thread_work() {
        let dir =
            std::env::temp_dir().join(format!("omarex-multihost-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let settings_path = dir.join("settings.json");

        let mut settings = UserSettings::default();
        settings.saved_hosts.push(SavedHost {
            id: "remote:https---rex-tail000000-ts-net".into(),
            label: "Studio Mac".into(),
            endpoint: "https://rex.tail000000.ts.net".into(),
            auto_connect: true,
        });
        let mut ctrl = OmarexController::new(settings, settings_path.clone(), Theme::default())
            .expect("multi-host controller");
        ctrl.clipboard_enabled = false;
        ctrl.set_default_working_directory(Some("/home/user/projects/omarex".into()));
        let now = Instant::now();

        // 1. Saved remote host immediately uses human label (NEVER raw `remote:https---...`) and `is_local == false`!
        let mac_id = "remote:https---rex-tail000000-ts-net";
        assert_eq!(ctrl.workspace.host_order, vec![LOCAL_HOST_ID, mac_id]);
        let mac_host = ctrl
            .workspace
            .host(mac_id)
            .expect("pre-registered remote host");
        assert_eq!(mac_host.label, "Studio Mac");
        assert!(!mac_host.is_local);

        // Connect + bootstrap both local (`T3 Code · omarchy`) and remote Mac (`T3 Code`, `a6dd4a`).
        ctrl.process_runtime_event(
            RuntimeEvent::HostConnected {
                host_id: LOCAL_HOST_ID.into(),
                client_id: "c:local".into(),
                resumed: false,
                api_revision: 24,
            },
            now,
        );
        ctrl.process_runtime_event(
            RuntimeEvent::HostConnected {
                host_id: mac_id.into(),
                client_id: "c:mac".into(),
                resumed: false,
                api_revision: 24,
            },
            now,
        );
        let local_boot: rex_proto::types::SessionBootstrapResponse =
            serde_json::from_value(serde_json::json!({
                "client_id": "c:local",
                "server_status": {
                    "pid": 100, "instance_id": "i1", "host_id": "h:local",
                    "started_at": "2026-11-26T10:00:00Z", "exit_empty": false,
                    "session_count": 1, "api_revision": 24, "api_min_revision": 24
                },
                "attach_all": { "sessions": [{ "session_id": "4e6bb7", "label": "T3 Code · omarchy" }] },
                "selected_session": null,
                "attached_sessions": [],
                "data_tickets": []
            }))
            .expect("local boot");
        let mac_boot: rex_proto::types::SessionBootstrapResponse =
            serde_json::from_value(serde_json::json!({
                "client_id": "c:mac",
                "server_status": {
                    "pid": 200, "instance_id": "i2", "host_id": "h:mac",
                    "started_at": "2026-11-26T10:00:00Z", "exit_empty": false,
                    "session_count": 1, "api_revision": 24, "api_min_revision": 24
                },
                "attach_all": { "sessions": [{ "session_id": "a6dd4a", "label": "T3 Code" }] },
                "selected_session": null,
                "attached_sessions": [],
                "data_tickets": []
            }))
            .expect("mac boot");
        ctrl.process_runtime_event(
            RuntimeEvent::BootstrapReady {
                host_id: LOCAL_HOST_ID.into(),
                seq: 1,
                bootstrap: Box::new(local_boot),
            },
            now,
        );
        ctrl.process_runtime_event(
            RuntimeEvent::BootstrapReady {
                host_id: mac_id.into(),
                seq: 1,
                bootstrap: Box::new(mac_boot),
            },
            now,
        );
        // Active session starts on local; focus a local block to verify zero cross-host block leak.
        ctrl.activate_session(SessionKey::new(LOCAL_HOST_ID, "4e6bb7"), &HashMap::new());
        ctrl.ui.focus.focus_terminal(Some("block:local-1".into()));
        ctrl.drain_pending_commands();

        // After bootstrap, remote host STILL keeps its human label and `is_local == false`!
        let mac_host = ctrl
            .workspace
            .host(mac_id)
            .expect("mac host after bootstrap");
        assert_eq!(mac_host.label, "Studio Mac");
        assert!(!mac_host.is_local);

        // 2. Clicking `+` on remote host's `T3 Code` routes `t3_new` strictly to remote host (`a6dd4a`),
        //    switches active session to remote host, and does NOT leak local's `block:local-1`!
        ctrl.dispatch_ui_action(
            UiAction::NewT3Thread {
                host_id: mac_id.into(),
                session_id: "a6dd4a".into(),
            },
            now,
        );
        assert_eq!(
            ctrl.active_session_key(),
            Some(&SessionKey::new(mac_id, "a6dd4a"))
        );
        let t3_cmds = ctrl.drain_pending_commands();
        assert!(t3_cmds.iter().any(|c| matches!(
            c,
            HostCommand::ClientAction { host_id, request }
                if host_id == mac_id
                    && request.action == "t3_new"
                    && request.session_id.as_deref() == Some("a6dd4a")
                    && request.block_id.is_none()
        )));

        // 3. Clicking `+` on remote host header spawns a new real shell session on that host
        //    and NEVER leaks Linux `/home/user/projects/omarex` or `$HOME` in `cwd`!
        ctrl.dispatch_ui_action(
            UiAction::NewSession {
                host_id: Some(mac_id.into()),
            },
            now,
        );
        let sess_cmds = ctrl.drain_pending_commands();
        let mac_new_sess = sess_cmds
            .iter()
            .find_map(|c| match c {
                HostCommand::NewSession { host_id, request } if host_id == mac_id => Some(request),
                _ => None,
            })
            .expect("NewSession must target remote host");
        assert_eq!(mac_new_sess.label.as_deref(), Some("Terminal"));
        let mac_opts = mac_new_sess.initial_windows.as_ref().unwrap()[0]
            .layout
            .block
            .as_ref()
            .unwrap()
            .options
            .as_ref();
        assert!(
            mac_opts.and_then(|o| o.get("cwd")).is_none(),
            "remote macOS session must NOT inherit Linux cwd: {mac_opts:?}"
        );

        // 4. Rename Host updates workspace label + persists into settings.json!
        ctrl.dispatch_ui_action(
            UiAction::StartRenameHost {
                host_id: mac_id.into(),
            },
            now,
        );
        ctrl.dispatch_ui_action(
            UiAction::SubmitRename {
                target_id: format!("__rename_host:{mac_id}"),
                new_label: "Mac mini Studio".into(),
            },
            now,
        );
        assert_eq!(
            ctrl.workspace.host(mac_id).unwrap().label,
            "Mac mini Studio"
        );
        assert_eq!(ctrl.settings.saved_hosts[0].label, "Mac mini Studio");

        // 5. SubmitAddHost supports `Name = url` syntax AND cleans bare Tailscale URLs.
        ctrl.dispatch_ui_action(
            UiAction::SubmitAddHost {
                endpoint: "Work VPS = https://vps-eu.tail000000.ts.net".into(),
            },
            now,
        );
        let vps_id = format!(
            "remote:{}",
            sanitize_host_id("https://vps-eu.tail000000.ts.net")
        );
        assert_eq!(ctrl.workspace.host(&vps_id).unwrap().label, "Work VPS");
        assert!(!ctrl.workspace.host(&vps_id).unwrap().is_local);
        assert_eq!(derive_host_label("https://rex.tail000000.ts.net"), "rex");

        // 6. DisconnectHost disconnects without removing saved host; RemoveHost removes completely.
        ctrl.drain_pending_commands();
        ctrl.dispatch_ui_action(
            UiAction::DisconnectHost {
                host_id: mac_id.into(),
            },
            now,
        );
        assert_eq!(
            ctrl.workspace.host(mac_id).unwrap().status,
            HostStatus::Disconnected
        );
        assert!(ctrl.settings.saved_hosts.iter().any(|h| h.id == mac_id));
        assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
            c,
            HostCommand::RemoveHost { host_id } if host_id == mac_id
        )));

        ctrl.dispatch_ui_action(
            UiAction::RemoveHost {
                host_id: mac_id.into(),
            },
            now,
        );
        assert!(ctrl.workspace.host(mac_id).is_none());
        assert!(!ctrl.settings.saved_hosts.iter().any(|h| h.id == mac_id));
        assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
            c,
            HostCommand::RemoveHost { host_id } if host_id == mac_id
        )));
        assert_eq!(
            ctrl.active_session_key(),
            Some(&SessionKey::new(LOCAL_HOST_ID, "4e6bb7"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn super_return_working_directory_spawns_independent_shell_session() {
        let mut ctrl = make_test_controller();
        ctrl.set_default_working_directory(Some("/tmp/omarex-proj".into()));
        let now = Instant::now();

        ctrl.process_runtime_event(
            RuntimeEvent::HostConnected {
                host_id: LOCAL_HOST_ID.into(),
                client_id: "client:window-2".into(),
                resumed: false,
                api_revision: 24,
            },
            now,
        );
        // Simulate another live `omarex` window already attached to both `T3 Code` and `Terminal`.
        ctrl.process_runtime_event(
            RuntimeEvent::ClientsListed {
                host_id: LOCAL_HOST_ID.into(),
                clients: rex_proto::types::ClientListResponse {
                    clients: vec![rex_proto::types::ConnectedClient {
                        client_id: "client:window-1".into(),
                        info: Some(rex_proto::types::ClientInfo {
                            kind: Some("app".into()),
                            platform: Some("linux".into()),
                            name: Some("omarex".into()),
                            version: None,
                            label: None,
                        }),
                        principal: Principal {
                            kind: "peer".into(),
                            transport: "unix".into(),
                            display: "local".into(),
                            remote_addr: None,
                            uid: None,
                            gid: None,
                            pid: None,
                            sid: None,
                            user_id: None,
                            login_name: None,
                            node: None,
                            tags: Vec::new(),
                            path: Vec::new(),
                        },
                        connected_at: String::new(),
                        connection_state: None,
                        connection_lost_at: None,
                        session_ids: vec!["session:t3".into(), "session:term".into()],
                    }],
                },
            },
            now,
        );
        let bootstrap: rex_proto::types::SessionBootstrapResponse =
            serde_json::from_value(serde_json::json!({
                "client_id": "client:window-2",
                "server_status": {
                    "pid": 1234,
                    "instance_id": "inst-1",
                    "host_id": "host:local",
                    "started_at": "2026-11-26T10:00:00Z",
                    "exit_empty": false,
                    "session_count": 2,
                    "api_revision": 24,
                    "api_min_revision": 24
                },
                "attach_all": {
                    "sessions": [
                        { "session_id": "session:t3", "label": "T3 Code" },
                        { "session_id": "session:term", "label": "Terminal" }
                    ]
                },
                "selected_session": null,
                "attached_sessions": [],
                "data_tickets": []
            }))
            .expect("parse test bootstrap");
        ctrl.process_runtime_event(
            RuntimeEvent::BootstrapReady {
                host_id: LOCAL_HOST_ID.into(),
                seq: 1,
                bootstrap: Box::new(bootstrap),
            },
            now,
        );

        let cmds = ctrl.drain_pending_commands();
        let new_req = cmds
            .iter()
            .find_map(|c| match c {
                HostCommand::NewSession { request, .. } => Some(request),
                _ => None,
            })
            .expect("Super+Return with --working-directory must spawn a fresh shell session");
        assert_eq!(new_req.label.as_deref(), Some("omarex-proj"));
        let cwd = new_req.initial_windows.as_ref().unwrap()[0]
            .layout
            .block
            .as_ref()
            .unwrap()
            .options
            .as_ref()
            .and_then(|o| o.get("cwd"))
            .and_then(|v| v.as_str());
        assert_eq!(cwd, Some("/tmp/omarex-proj"));

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn close_session_float_dock_and_contextual_rename_wire_correctly() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let now = Instant::now();

        // 1. `CloseSession` sends `HostCommand::DestroySession` (not `ClientAction`).
        ctrl.dispatch_ui_action(
            UiAction::CloseSession {
                session_id: "session:t3".into(),
            },
            now,
        );
        let close_cmds = ctrl.drain_pending_commands();
        assert!(close_cmds.iter().any(|c| matches!(
            c,
            HostCommand::DestroySession { session_id, .. } if session_id == "session:t3"
        )));

        // 2. `FloatOrDock` floats tiled pane and docks floating pane via `MoveBlock`.
        ctrl.dispatch_ui_action(
            UiAction::PaneButton {
                block_id: "block:1".into(),
                button: PaneHeaderButton::FloatOrDock,
            },
            now,
        );
        assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
            c,
            HostCommand::FloatBlock { request, .. } if request.block_id == "block:1"
        )));

        // Add a floating block alongside tiled `block:1` and verify `FloatOrDock` docks it!
        let sk = SessionKey::new(LOCAL_HOST_ID, "session:t3");
        if let Some(view) = ctrl
            .workspace
            .session_mut(&sk)
            .and_then(|s| s.view.as_mut())
        {
            view.windows[0].layers.push(LayerView {
                layer_id: Some("layer:float1".into()),
                kind: "floating".into(),
                bounds: [0.2, 0.2, 0.6, 0.6],
                state: Some("normal".into()),
                layout: None,
                blocks: vec![BlockView {
                    block_id: Some("block:float1".into()),
                    label: "Scratchpad".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            });
        }
        ctrl.dispatch_ui_action(
            UiAction::PaneButton {
                block_id: "block:float1".into(),
                button: PaneHeaderButton::FloatOrDock,
            },
            now,
        );
        assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
            c,
            HostCommand::MoveBlock {
                block_id,
                anchor_block_id,
                ..
            } if block_id == "block:float1" && anchor_block_id == "block:1"
        )));

        // 3. Contextual `SubmitRename` handles window, block, and session targets accurately.
        ctrl.dispatch_ui_action(
            UiAction::SubmitRename {
                target_id: "__rename_window:local|session:t3|window:1".into(),
                new_label: "Renamed Tab".into(),
            },
            now,
        );
        ctrl.dispatch_ui_action(
            UiAction::SubmitRename {
                target_id: "__rename_block:local|session:t3|block:1".into(),
                new_label: "Renamed Pane".into(),
            },
            now,
        );
        let ren_cmds = ctrl.drain_pending_commands();
        assert!(ren_cmds.iter().any(|c| matches!(
            c,
            HostCommand::RenameWindow { window_id, label, .. }
                if window_id == "window:1" && label == "Renamed Tab"
        )));
        assert!(ren_cmds.iter().any(|c| matches!(
            c,
            HostCommand::RenameBlock { block_id, label, .. }
                if block_id == "block:1" && label == "Renamed Pane"
        )));

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn plain_filepath_hover_detects_local_image_card_and_ignores_prose() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

        // Create a real tiny valid 2x2 PNG file on disk so local existence + image_dimensions pass.
        let tmp_dir =
            std::env::temp_dir().join(format!("omarex-hover-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp_dir);
        let png_path = tmp_dir.join("preview shot.png");
        let rgba = [
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
        ];
        gpu_renderer::save_rgba_png(&png_path, 2, 2, &rgba).expect("write test PNG");

        let note_path = tmp_dir.join("notes.txt");
        std::fs::write(&note_path, "hello").expect("write test text file");

        ctrl.pane_store
            .create_local_test_pane(&key, 120, 24)
            .unwrap();
        let line0 = format!("Check '{}'\r\n", png_path.display());
        let line1 = format!("See {} and iPhone/Mac prose\r\n", note_path.display());
        ctrl.pane_store
            .feed_local_test_bytes(&key, line0.as_bytes())
            .unwrap();
        ctrl.pane_store
            .feed_local_test_bytes(&key, line1.as_bytes())
            .unwrap();

        // 1. Hovering quoted PNG path with space -> `ImageCard` + pending GPU upload!
        let png_col = line0.find("preview").unwrap() as u16;
        ctrl.update_hovered_file_path(&key, png_col, 0, 120.0, 80.0);
        match ctrl.ui.hover_preview.as_ref() {
            Some(HoverPreviewState::ImageCard {
                caption,
                image_width,
                image_height,
                ..
            }) => {
                assert_eq!(caption, "preview shot.png");
                assert_eq!((*image_width, *image_height), (2, 2));
            }
            other => panic!("expected ImageCard hover preview, got {other:?}"),
        }
        assert!(
            ctrl.pending_hover_image
                .as_ref()
                .is_some_and(|(p, _)| p == &png_path)
        );

        // 2. Hovering regular text file -> `FilePath` tooltip.
        let txt_col = line1.find("notes.txt").unwrap() as u16;
        ctrl.update_hovered_file_path(&key, txt_col, 1, 120.0, 100.0);
        assert!(matches!(
            ctrl.ui.hover_preview.as_ref(),
            Some(HoverPreviewState::FilePath { path, .. }) if path.ends_with("notes.txt")
        ));

        // 3. Hovering German/English prose slash `iPhone/Mac` -> ZERO false positives!
        let prose_col = line1.find("iPhone/Mac").unwrap() as u16 + 2;
        ctrl.update_hovered_file_path(&key, prose_col, 1, 200.0, 100.0);
        assert!(
            ctrl.ui.hover_preview.is_none(),
            "prose token `iPhone/Mac` must not trigger local file hover"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn remote_multi_file_media_drop_batches_uploads_and_targets_hovered_split_pane() {
        let mut ctrl = make_test_controller();
        let now = Instant::now();

        // Set up a remote host with a 2-pane horizontal split (left: block:left, right: block:right).
        let mut host = HostEntry::new(
            "remote-mac",
            "Mac",
            RexEndpoint::parse("https://rex.example.ts.net").unwrap(),
        );
        host.is_local = false;
        host.status = HostStatus::Connected {
            client_id: "client:1".into(),
            server_host_id: Some("host:mac".into()),
            api_revision: 24,
            resumed: false,
        };
        host.on_session_created(SessionSummary {
            session_id: "session:mac".into(),
            label: "T3 Code".into(),
        });
        ctrl.workspace.add_host(host);
        ctrl.workspace
            .select_session(SessionKey::new("remote-mac", "session:mac"));
        let view = SessionView {
            revision: 1,
            session_id: Some("session:mac".into()),
            label: "T3 Code".into(),
            active_window_id: Some("window:1".into()),
            windows: vec![WindowView {
                window_id: Some("window:1".into()),
                label: "Split".into(),
                focused_block_id: Some("block:left".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![LayerView {
                    layer_id: Some("layer:tiled".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: Some(LayoutNode {
                        rect: [0.0, 0.0, 1.0, 1.0],
                        block_id: None,
                        split: Some(Box::new(rex_proto::types::SplitNode {
                            split_id: Some("split:1".into()),
                            direction: "horizontal".into(),
                            ratio: 0.5,
                            before: LayoutNode {
                                rect: [0.0, 0.0, 0.5, 1.0],
                                block_id: Some("block:left".into()),
                                split: None,
                            },
                            after: LayoutNode {
                                rect: [0.5, 0.0, 0.5, 1.0],
                                block_id: Some("block:right".into()),
                                split: None,
                            },
                        })),
                    }),
                    blocks: vec![
                        BlockView {
                            block_id: Some("block:left".into()),
                            label: "Left".into(),
                            creator_name: Some("com.superlogical.terminal".into()),
                            flavor: Some("com.superlogical.terminal.shell".into()),
                            host_id: Some("host:mac".into()),
                            owner: Some("client:1".into()),
                        },
                        BlockView {
                            block_id: Some("block:right".into()),
                            label: "Right".into(),
                            creator_name: Some("com.superlogical.terminal".into()),
                            flavor: Some("com.superlogical.terminal.shell".into()),
                            host_id: Some("host:mac".into()),
                            owner: Some("client:1".into()),
                        },
                    ],
                }],
            }],
        };
        ctrl.workspace
            .session_mut(&SessionKey::new("remote-mac", "session:mac"))
            .unwrap()
            .apply_view(view);
        ctrl.ui.focus.focus_terminal(Some("block:left".into()));
        ctrl.set_viewport(1440, 900, 1.0);

        // Compute one UI layout pass so `last_ui_output` knows both split pane rects.
        let frame_input = ctrl.build_ui_frame_input(now);
        let ui_out = ctrl.ui.render(&frame_input, &ctrl.theme, now, 0.0);
        let right_card = ui_out
            .tiled_panes
            .iter()
            .find(|p| p.id == "block:right")
            .expect("right pane geometry")
            .card_rect;
        ctrl.last_ui_output = Some(ui_out);

        // 1. Hovering a drag over the right pane produces a crisp `dnd_overlay` on `right_card`.
        let (drop_x, drop_y) = right_card.center();
        ctrl.handle_file_hover(drop_x, drop_y);
        let hover_input = ctrl.build_ui_frame_input(now);
        assert_eq!(
            hover_input.dnd_overlay.map(|d| d.preview_rect),
            Some(right_card),
            "drag hover must highlight the exact pane card under the cursor"
        );

        // 2. Dropping an image + video + folder onto the right pane skips the folder (with toast)
        //    and uploads both media files to `block:right` (NOT `block:left`) in ONE `UploadBatch`!
        let tmp_dir = std::env::temp_dir().join(format!("omarex-dnd-batch-{}", std::process::id()));
        let sub_dir = tmp_dir.join("folder");
        let _ = std::fs::create_dir_all(&sub_dir);
        let img = tmp_dir.join("photo.webp");
        let vid = tmp_dir.join("clip.mp4");
        std::fs::write(&img, b"RIFF....WEBP").unwrap();
        std::fs::write(&vid, b"\x00\x00\x00\x18ftypmp42").unwrap();

        let _ = ctrl.drain_pending_commands();
        ctrl.handle_file_drop_at(
            &[img.clone(), sub_dir.clone(), vid.clone()],
            Some((drop_x, drop_y)),
        );

        // Overlay cleared & focus moved to the target pane.
        assert!(ctrl.active_dnd_hover.is_none());
        assert_eq!(ctrl.ui.focus.last_terminal_block(), Some("block:right"));
        assert!(
            ctrl.ui
                .toasts
                .iter()
                .any(|t| t.title.contains("Folders skipped")),
            "expected warning toast for skipped directory"
        );

        let uploads: Vec<(String, PathBuf, Option<UploadBatch>)> = ctrl
            .drain_pending_commands()
            .into_iter()
            .filter_map(|c| match c {
                HostCommand::UploadFile {
                    block_id,
                    local_path,
                    batch,
                    ..
                } => Some((block_id, local_path, batch)),
                _ => None,
            })
            .collect();
        assert_eq!(uploads.len(), 2);
        assert_eq!(uploads[0].0, "block:right");
        assert_eq!(uploads[1].0, "block:right");
        assert_eq!(uploads[0].1, img);
        assert_eq!(uploads[1].1, vid);
        let b0 = uploads[0].2.as_ref().expect("batch on file 0");
        let b1 = uploads[1].2.as_ref().expect("batch on file 1");
        assert_eq!(b0.batch_id, b1.batch_id);
        assert_eq!((b0.index, b0.count), (0, 2));
        assert_eq!((b1.index, b1.count), (1, 2));

        let _ = std::fs::remove_dir_all(&tmp_dir);
        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn local_media_drop_shell_escapes_images_videos_and_bracketed_paste() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
        ctrl.pane_store
            .create_local_test_pane(&key, 80, 24)
            .unwrap();
        // Enable DEC Mode 2004 bracketed paste like `tr3x` / `zsh` / `claude`.
        ctrl.pane_store
            .feed_local_test_bytes(&key, b"\x1b[?2004h")
            .unwrap();
        let _ = ctrl.drain_pending_commands();

        ctrl.handle_file_drop(&[
            PathBuf::from("/home/user/Pictures/shot.webp"),
            PathBuf::from("/home/user/My Videos/demo clip.mp4"),
        ]);

        let written = ctrl
            .drain_pending_commands()
            .into_iter()
            .find_map(|c| match c {
                HostCommand::TerminalWrite { data, .. } => Some(data),
                _ => None,
            })
            .expect("local drop must paste paths into terminal");
        assert_eq!(
            written,
            "\x1b[200~/home/user/Pictures/shot.webp '/home/user/My Videos/demo clip.mp4'\x1b[201~"
        );

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn session_view_fetch_failed_resets_inflight_and_evicts_not_found_session_without_toast() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let now = Instant::now();
        let sk = SessionKey::new(LOCAL_HOST_ID, "session:t3");

        // Trigger a pending view fetch.
        ctrl.process_runtime_event(
            RuntimeEvent::ControlEvent {
                host_id: LOCAL_HOST_ID.into(),
                seq: 2,
                event: Box::new(ServerEvent::SessionEvent {
                    session_id: "session:t3".into(),
                    event: SessionEvent::SessionViewChanged { revision: 2 },
                }),
            },
            now,
        );
        assert!(ctrl.workspace.session(&sk).unwrap().view_fetch_in_flight);
        ctrl.drain_pending_commands();

        // 1. Transient failure clears `view_fetch_in_flight` without destroying session or toasting.
        ctrl.process_runtime_event(
            RuntimeEvent::SessionViewFetchFailed {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                session_not_found: false,
            },
            now,
        );
        assert!(!ctrl.workspace.session(&sk).unwrap().view_fetch_in_flight);
        assert!(ctrl.ui.toasts.is_empty());

        // 2. `session_not_found` evicts panes, cancels streams, removes session, and shows zero toasts.
        let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
        ctrl.opened_streams.insert(key.clone());
        ctrl.process_runtime_event(
            RuntimeEvent::SessionViewFetchFailed {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                session_not_found: true,
            },
            now,
        );
        assert!(ctrl.workspace.session(&sk).is_none());
        assert!(!ctrl.pane_store.contains(&key));
        assert!(!ctrl.opened_streams.contains(&key));
        assert!(ctrl.ui.toasts.is_empty());
        let cmds = ctrl.drain_pending_commands();
        assert!(cmds.iter().any(|c| matches!(
            c,
            HostCommand::EvictSessionStreams {
                host_id,
                session_id,
                disown: false,
            } if host_id == LOCAL_HOST_ID && session_id == "session:t3"
        )));

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn apply_view_prunes_zombie_panes_and_theme_change_never_writes_to_dead_blocks() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let now = Instant::now();

        // Insert a second pane (`block:zombie`) with Mode 2031 enabled, simulating a block that
        // was closed/replaced by `tr3x` and removed from the next `SessionView`.
        let zombie_key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:zombie");
        let zombie_term = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
            .expect("create zombie terminal");
        zombie_term.write(b"\x1b[?2031h");
        ctrl.pane_store
            .insert_terminal(zombie_key.clone(), zombie_term)
            .expect("insert zombie terminal");
        ctrl.opened_streams.insert(zombie_key.clone());

        // Deliver an updated `SessionView` containing ONLY `block:1`.
        let updated_view = SessionView {
            revision: 2,
            session_id: Some("session:t3".into()),
            label: "T3 Code".into(),
            active_window_id: Some("window:1".into()),
            windows: vec![WindowView {
                window_id: Some("window:1".into()),
                label: "Build bridge.rs".into(),
                focused_block_id: Some("block:1".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![LayerView {
                    layer_id: Some("layer:tiled".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: Some(LayoutNode {
                        rect: [0.0, 0.0, 1.0, 1.0],
                        block_id: Some("block:1".into()),
                        split: None,
                    }),
                    blocks: vec![BlockView {
                        block_id: Some("block:1".into()),
                        label: "t3:1baa1939/thread-1".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:local".into()),
                        owner: Some("client:1".into()),
                    }],
                }],
            }],
        };
        ctrl.process_runtime_event(
            RuntimeEvent::SessionViewUpdated {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                view: Box::new(updated_view),
                seq: 2,
            },
            now,
        );

        assert!(
            !ctrl.pane_store.contains(&zombie_key),
            "zombie pane must be evicted from PaneStore when pruned from SessionView"
        );
        assert!(!ctrl.opened_streams.contains(&zombie_key));
        let prune_cmds = ctrl.drain_pending_commands();
        assert!(prune_cmds.iter().any(|c| matches!(
            c,
            HostCommand::CloseBlockStream { block_id, .. } if block_id == "block:zombie"
        )));

        // Even if an orphaned pane somehow remains in `pane_store` without workspace tracking,
        // `ThemeChanged` must NEVER send `TerminalWrite` to it.
        let orphan_key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:orphan");
        let orphan_term = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
            .expect("create orphan terminal");
        orphan_term.write(b"\x1b[?2031h");
        ctrl.pane_store
            .insert_terminal(orphan_key, orphan_term)
            .expect("insert orphan terminal");
        ctrl.process_runtime_event(
            RuntimeEvent::ThemeChanged(ThemeEvent::PaletteChanged(
                omarchy_theme::ThemePalette::default_light(),
            )),
            now,
        );
        let theme_cmds = ctrl.drain_pending_commands();
        assert!(
            !theme_cmds.iter().any(|c| matches!(
                c,
                HostCommand::TerminalWrite { block_id, .. }
                    if block_id == "block:zombie" || block_id == "block:orphan"
            )),
            "ThemeChanged must never write Mode 2031 reports to dead/untracked blocks"
        );

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn data_stream_error_releases_opened_streams_and_lifecycle_events_cancel_host_streams() {
        let mut ctrl = make_test_controller();
        seed_single_pane_workspace(&mut ctrl);
        let now = Instant::now();
        let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

        // 1. A stream error must release `opened_streams` so the pane is never permanently wedged.
        ctrl.opened_streams.insert(key.clone());
        ctrl.process_runtime_event(
            RuntimeEvent::DataStream(DataStreamEvent {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:t3".into(),
                block_id: "block:1".into(),
                kind: DataStreamEventKind::Error {
                    message: "RPC error: conflict (409): client is not attached".into(),
                },
            }),
            now,
        );
        assert!(
            !ctrl.opened_streams.contains(&key),
            "DataStreamEventKind::Error must clear opened_streams so pane can reconnect"
        );

        // 2. `BlockClosed` cancels the active block stream in `HostManager`.
        ctrl.opened_streams.insert(key.clone());
        ctrl.process_runtime_event(
            RuntimeEvent::ControlEvent {
                host_id: LOCAL_HOST_ID.into(),
                seq: 3,
                event: Box::new(ServerEvent::SessionEvent {
                    session_id: "session:t3".into(),
                    event: SessionEvent::BlockClosed {
                        block_id: "block:1".into(),
                    },
                }),
            },
            now,
        );
        let close_cmds = ctrl.drain_pending_commands();
        assert!(close_cmds.iter().any(|c| matches!(
            c,
            HostCommand::CloseBlockStream { block_id, .. } if block_id == "block:1"
        )));

        // 3. `SessionDestroyed` cancels all active streams for that session in `HostManager`.
        ctrl.opened_streams
            .insert(BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:2"));
        ctrl.process_runtime_event(
            RuntimeEvent::ControlEvent {
                host_id: LOCAL_HOST_ID.into(),
                seq: 4,
                event: Box::new(ServerEvent::SessionDestroyed {
                    session_id: "session:t3".into(),
                }),
            },
            now,
        );
        let destroy_cmds = ctrl.drain_pending_commands();
        assert!(destroy_cmds.iter().any(|c| matches!(
            c,
            HostCommand::EvictSessionStreams {
                session_id,
                disown: false,
                ..
            } if session_id == "session:t3"
        )));

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn toasts_deduplicate_schedule_wakeup_and_auto_dismiss_on_tick() {
        let mut ctrl = make_test_controller();
        let t0 = Instant::now();

        ctrl.push_toast_at(ToastSeverity::Error, "local: session.create", "boom", t0);
        assert_eq!(ctrl.ui.toasts.len(), 1);
        let first_id = ctrl.ui.toasts[0].id;

        // Identical duplicate refreshes expiry instead of stacking a second toast.
        let t1 = t0 + Duration::from_secs(3);
        ctrl.push_toast_at(ToastSeverity::Error, "local: session.create", "boom", t1);
        assert_eq!(ctrl.ui.toasts.len(), 1);
        assert_eq!(ctrl.ui.toasts[0].id, first_id);

        let expected_deadline = t1 + Duration::from_secs(8);
        assert_eq!(ctrl.next_wakeup_deadline(t1), Some(expected_deadline));

        // Before refreshed deadline -> toast stays visible.
        let _ = ctrl.tick(t0 + Duration::from_secs(9));
        assert_eq!(ctrl.ui.toasts.len(), 1);

        // At/after refreshed deadline -> toast auto-dismisses and wakeup clears to None.
        let _ = ctrl.tick(expected_deadline);
        assert!(ctrl.ui.toasts.is_empty());
        assert!(ctrl.toast_deadlines.is_empty());
        assert_eq!(ctrl.next_wakeup_deadline(expected_deadline), None);

        if let Some(parent) = ctrl.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn live_full_omarex_window_renders_to_png() {
        let ep = RexEndpoint::discover_default();
        if let RexEndpoint::Unix(ref sock) = ep
            && !sock.exists()
        {
            eprintln!(
                "skipping live_full_omarex_window_renders_to_png: socket {} absent",
                sock.display()
            );
            return;
        }

        let png_path = std::env::var("OMAREX_LIVE_FULL_PNG").unwrap_or_else(|_| {
            std::env::temp_dir()
                .join("omarex_live_full.png")
                .to_string_lossy()
                .into_owned()
        });

        let report = render_headless_live_png(None, Some("T3 Code"), 1440, 900, 1.0, &png_path)
            .expect("live headless full omarex window render should succeed");

        assert!(report.host_connected, "expected live Rex connection");
        assert!(
            report.total_windows > 0,
            "expected live session windows in sidebar"
        );
        assert!(
            report.ready_panes > 0,
            "expected at least one decoded live terminal pane"
        );
        assert!(Path::new(&png_path).is_file(), "expected output PNG file");

        eprintln!(
            "OMAREX_LIVE_FULL session={:?} windows={} ready_panes={} adapter={:?} total_us={} draws={} quads={} glyphs={} png={}",
            report.active_session_label,
            report.total_windows,
            report.ready_panes,
            report.frame_stats.adapter_name,
            report.frame_stats.total_us,
            report.frame_stats.draw_calls,
            report.frame_stats.quad_count,
            report.frame_stats.glyph_count,
            png_path
        );
    }
}
