//! Per-pane VT emulation, progressive `GHOSTSNP` snapshot decoding, GPU `RowId`-cached rendering,
//! Kitty Graphics texture management, and terminal interaction runtime (`PaneStore`).
//!
//! Connects [`crate::host_manager::DataStreamEvent`] streams directly to `vt-engine` (`libghostty-vt`)
//! and `gpu-renderer` (`TerminalPaneCache` + `ImageTextureCache`) with:
//! - Microsecond-locked `RenderState::begin_update` + lock-free `end_update`
//! - Bounded background snapshot stepping (`<= 2 ms` per UI tick) with `COMPRESS_HISTORY = true`
//! - Collision-free per-pane Kitty Graphics texture keys (`(pane_uid << 32) | image_id`)
//! - DEC 2026 synchronized output hold handling (no mid-frame tearing)
//! - Smooth overscan fractional pixel scrolling (`scroll_accum_px`)
//! - Live Kitty keyboard flags, Shift-bypass mouse encoding, selection/hyperlinks, and scrollback search.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use bytes::Bytes;
use gpu_renderer::{
    CellMetrics, ClipRect, Color, DrawList, GlyphAtlas, GpuRenderer, ImageLayer, ImagePixelFormat,
    ImageTextureCache, ImageUpload, KittyPlacement as GpuKittyPlacement, Rect, RenderError,
    TerminalBuildStats, TerminalPaneCache, TerminalViewport,
};
use omarchy_theme::{AppearanceMode, ThemePalette};
use rex_proto::types::{TerminalResizeArgs, ThemeScheme};
use rex_proto::{DataCloseAction, RexControlMessage};
use vt_engine::{
    BackgroundStep, ColorScheme, KeyEncoder, KeyEventInput, KittyImageLayer, KittyKeyboardFlags,
    KittyPlacement as VtKittyPlacement, MouseAction, MouseEncoder, MouseEventInput, MouseGeometry,
    MouseTrackingMode, Overscan, RenderState, RexControlApc, Rgb as VtRgb, ScrollViewport,
    Scrollbar, SearchScroll, SearchStatus, SelectionAutoscroll, SelectionGeometry,
    SelectionGesture, SnapshotDecoder, SnapshotFeedStatus, Terminal, TerminalConfig, TerminalEvent,
    TerminalSearch, TerminalTheme, UpdateSnapshot, ViewportMatch, VtError,
};

use crate::host_manager::{DataStreamEvent, DataStreamEventKind, LOCAL_HOST_ID};
use crate::workspace::{BlockKey, WorkspaceState};

/// Default per-tick CPU budget for background `GHOSTSNP` history & image decoding (`<= 2 ms`).
pub const DEFAULT_BACKGROUND_SNAPSHOT_BUDGET: Duration = Duration::from_millis(2);

/// Errors returned by [`PaneStore`] operations.
#[derive(Debug)]
pub enum PaneStoreError {
    Vt(VtError),
    Render(RenderError),
    PaneNotFound(BlockKey),
    PaneNotReady(BlockKey),
}

impl fmt::Display for PaneStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vt(err) => write!(f, "vt-engine error: {err}"),
            Self::Render(err) => write!(f, "gpu-renderer error: {err}"),
            Self::PaneNotFound(key) => {
                write!(
                    f,
                    "pane not found: {}:{}:{}",
                    key.host_id, key.session_id, key.block_id
                )
            }
            Self::PaneNotReady(key) => {
                write!(
                    f,
                    "pane snapshot not READY yet: {}:{}:{}",
                    key.host_id, key.session_id, key.block_id
                )
            }
        }
    }
}

impl std::error::Error for PaneStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Vt(err) => Some(err),
            Self::Render(err) => Some(err),
            Self::PaneNotFound(_) | Self::PaneNotReady(_) => None,
        }
    }
}

impl From<VtError> for PaneStoreError {
    fn from(err: VtError) -> Self {
        Self::Vt(err)
    }
}

impl From<RenderError> for PaneStoreError {
    fn from(err: RenderError) -> Self {
        Self::Render(err)
    }
}

/// Composes a collision-free 64-bit GPU texture cache key from a per-pane UID and terminal-local
/// Kitty `image_id`.
#[inline]
pub const fn compose_kitty_image_id(pane_uid: u32, vt_image_id: u32) -> u64 {
    ((pane_uid as u64) << 32) | (vt_image_id as u64)
}

/// Converts an Omarchy [`ThemePalette`] into a `vt-engine` [`TerminalTheme`] including all 16 ANSI
/// colors and explicit Mode 2031 light/dark color scheme.
pub fn theme_palette_to_vt_theme(palette: &ThemePalette) -> TerminalTheme {
    let fg = palette.terminal.foreground;
    let bg = palette.terminal.background;
    let cursor = palette.terminal.cursor;
    let palette_overrides = palette
        .terminal
        .ansi
        .iter()
        .enumerate()
        .map(|(idx, c)| (idx as u8, VtRgb::new(c.r, c.g, c.b)))
        .collect();
    let scheme = match palette.mode {
        AppearanceMode::Light => ColorScheme::Light,
        AppearanceMode::Dark => ColorScheme::Dark,
    };

    TerminalTheme {
        foreground: Some(VtRgb::new(fg.r, fg.g, fg.b)),
        background: Some(VtRgb::new(bg.r, bg.g, bg.b)),
        cursor: Some(VtRgb::new(cursor.r, cursor.g, cursor.b)),
        palette_overrides,
        scheme: Some(scheme),
    }
}

/// Converts a verified wire [`RexControlMessage`] (`rex-proto`) into a `vt-engine`
/// [`RexControlApc`] ready for [`Terminal::apply_rex_apc`].
pub fn rex_control_msg_to_vt_apc(msg: &RexControlMessage) -> RexControlApc {
    match msg {
        RexControlMessage::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            owner,
        } => RexControlApc::Resize {
            cols: *cols,
            rows: *rows,
            cell_width_px: *cell_width_px,
            cell_height_px: *cell_height_px,
            owner: owner.clone(),
        },
        RexControlMessage::Theme {
            foreground,
            background,
            cursor,
            palette,
            scheme,
        } => RexControlApc::Theme {
            foreground: foreground.clone(),
            background: background.clone(),
            cursor: cursor.clone(),
            palette: palette.clone(),
            scheme: scheme.map(|s| match s {
                ThemeScheme::Light => "light".to_owned(),
                ThemeScheme::Dark => "dark".to_owned(),
            }),
        },
        RexControlMessage::ChildExited { exit_code } => RexControlApc::ChildExited {
            exit_code: *exit_code,
        },
        RexControlMessage::Unknown { msg_type, raw } => RexControlApc::Unknown {
            kind: msg_type.clone(),
            raw_json: raw.to_string(),
        },
    }
}

/// Helper constructing a [`TerminalViewport`] styled from an Omarchy [`ThemePalette`].
#[allow(clippy::too_many_arguments)]
pub fn build_terminal_viewport(
    rect: Rect,
    clip_rect: ClipRect,
    cols: u16,
    rows: u16,
    metrics: CellMetrics,
    scroll_offset_y: f32,
    palette: &ThemePalette,
    is_focused: bool,
    dim_unfocused: bool,
) -> TerminalViewport {
    let term = &palette.terminal;
    let dim_alpha = if is_focused || !dim_unfocused {
        1.0
    } else {
        0.78
    };
    TerminalViewport {
        rect,
        clip_rect,
        cols,
        rows,
        metrics,
        scroll_offset_y,
        default_bg: rgb_to_gpu_color(term.background.r, term.background.g, term.background.b),
        default_fg: rgb_to_gpu_color(term.foreground.r, term.foreground.g, term.foreground.b),
        selection_bg: rgb_to_gpu_color(
            term.selection_bg.r,
            term.selection_bg.g,
            term.selection_bg.b,
        ),
        selection_fg: Some(rgb_to_gpu_color(
            term.selection_fg.r,
            term.selection_fg.g,
            term.selection_fg.b,
        )),
        hyperlink_color: rgb_to_gpu_color(
            palette.ui.accent_primary.r,
            palette.ui.accent_primary.g,
            palette.ui.accent_primary.b,
        ),
        dim_alpha,
    }
}

#[inline]
fn rgb_to_gpu_color(r: u8, g: u8, b: u8) -> Color {
    Color::from(VtRgb::new(r, g, b))
}

/// Outcome returned by [`PaneStore::handle_data_stream_event`].
#[derive(Debug, Default)]
pub struct PaneStreamOutcome {
    /// `true` if the pane visual state changed and should be repainted.
    pub dirty: bool,
    /// `true` if the pane's `GHOSTSNP` stream just hit the `READY` marker (`<5ms` first paint).
    pub became_ready: bool,
    /// If a resize was queued before `READY`, [`WorkspaceState::mark_snapshot_ready`] releases it here.
    pub pending_resize: Option<TerminalResizeArgs>,
    /// `true` if `SnapshotComplete` arrived on this event.
    pub snapshot_complete: bool,
    /// Terminal callback events drained during VT writes (OSC 7501, notifications, OSC 52, title, etc.).
    pub terminal_events: Vec<TerminalEvent>,
    /// Verified in-band Rex APC applied on this event, if any.
    pub rex_apc: Option<RexControlApc>,
    /// Set if the stream closed permanently.
    pub stream_closed: Option<DataCloseAction>,
    /// Set if the stream reported a fatal error.
    pub stream_error: Option<String>,
}

/// Summary returned by [`PaneStore::step_background_snapshots`].
#[derive(Debug, Default)]
pub struct BackgroundSnapshotSummary {
    pub panes_stepped: usize,
    pub panes_finished: usize,
    pub pages_decoded: usize,
    pub rows_prepended: usize,
    pub images_decoded: usize,
    pub image_bytes: usize,
    /// `true` if at least one active decoder still has buffered records waiting for the next tick.
    pub more_pending: bool,
    /// Panes whose visual output or scrollbar state changed and should be repainted.
    pub dirty_panes: Vec<BlockKey>,
    /// Terminal events emitted when trailing post-`FINISH` bytes were flushed into a finished pane.
    pub terminal_events: Vec<(BlockKey, Vec<TerminalEvent>)>,
}

/// Telemetry returned by [`PaneStore::render_pane`] and [`PaneStore::render_pane_parts`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneRenderResult {
    pub stats: TerminalBuildStats,
    pub kitty_placements: usize,
    pub kitty_uploads: usize,
    /// `true` when DEC 2026 synchronized output held the terminal update and reused the previous frame.
    pub held_by_sync_output: bool,
    pub scrollbar: Scrollbar,
    pub viewport_active: bool,
    pub scroll_offset_y: f32,
}

/// Summary returned by [`PaneStore::search_query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSummary {
    pub query: String,
    pub status: SearchStatus,
    pub total_matches: usize,
    pub selected_index: Option<usize>,
    pub viewport_matches: Vec<ViewportMatch>,
}

/// Per-block terminal emulation, render cache, selection, search, and scroll runtime.
pub struct PaneRuntime {
    pane_uid: u32,
    key: BlockKey,
    channel_nonce: Option<u64>,
    decoder: Option<SnapshotDecoder>,
    terminal: Option<Terminal>,
    render_state: RenderState,
    pane_cache: TerminalPaneCache,
    mouse_encoder: MouseEncoder,
    selection_gesture: Option<SelectionGesture>,
    search: Option<TerminalSearch>,
    search_query: String,
    scroll_accum_px: f32,
    dirty: bool,
    snapshot_complete: bool,
    has_rendered_once: bool,
    last_update_snapshot: UpdateSnapshot,
    last_exited_code: Option<i32>,
    bytes_received: u64,
    deferred_live_chunks: Vec<Bytes>,
    deferred_apcs: Vec<RexControlApc>,
    kitty_placements_scratch: Vec<VtKittyPlacement>,
    gpu_placements_scratch: Vec<GpuKittyPlacement>,
}

impl fmt::Debug for PaneRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaneRuntime")
            .field("pane_uid", &self.pane_uid)
            .field("key", &self.key)
            .field("ready", &self.terminal.is_some())
            .field("decoding_bg", &self.decoder.is_some())
            .field("dirty", &self.dirty)
            .field("snapshot_complete", &self.snapshot_complete)
            .field("scroll_accum_px", &self.scroll_accum_px)
            .field("bytes_received", &self.bytes_received)
            .finish_non_exhaustive()
    }
}

impl PaneRuntime {
    fn new(pane_uid: u32, key: BlockKey, config: TerminalConfig) -> Result<Self, PaneStoreError> {
        let channel_nonce = config.channel_nonce;
        let decoder = SnapshotDecoder::new(config)?;
        let render_state = RenderState::new()?;
        let mouse_encoder = MouseEncoder::new()?;

        Ok(Self {
            pane_uid,
            key,
            channel_nonce,
            decoder: Some(decoder),
            terminal: None,
            render_state,
            pane_cache: TerminalPaneCache::new(),
            mouse_encoder,
            selection_gesture: None,
            search: None,
            search_query: String::new(),
            scroll_accum_px: 0.0,
            dirty: false,
            snapshot_complete: false,
            has_rendered_once: false,
            last_update_snapshot: UpdateSnapshot::default(),
            last_exited_code: None,
            bytes_received: 0,
            deferred_live_chunks: Vec::new(),
            deferred_apcs: Vec::new(),
            kitty_placements_scratch: Vec::new(),
            gpu_placements_scratch: Vec::new(),
        })
    }

    fn from_terminal(
        pane_uid: u32,
        key: BlockKey,
        terminal: Terminal,
    ) -> Result<Self, PaneStoreError> {
        let render_state = RenderState::new()?;
        let mouse_encoder = MouseEncoder::new()?;
        let selection_gesture = SelectionGesture::new(&terminal)?;

        Ok(Self {
            pane_uid,
            key,
            channel_nonce: None,
            decoder: None,
            terminal: Some(terminal),
            render_state,
            pane_cache: TerminalPaneCache::new(),
            mouse_encoder,
            selection_gesture: Some(selection_gesture),
            search: None,
            search_query: String::new(),
            scroll_accum_px: 0.0,
            dirty: true,
            snapshot_complete: true,
            has_rendered_once: false,
            last_update_snapshot: UpdateSnapshot::default(),
            last_exited_code: None,
            bytes_received: 0,
            deferred_live_chunks: Vec::new(),
            deferred_apcs: Vec::new(),
            kitty_placements_scratch: Vec::new(),
            gpu_placements_scratch: Vec::new(),
        })
    }

    fn reset_for_fresh_snapshot(&mut self, config: TerminalConfig) -> Result<(), PaneStoreError> {
        self.channel_nonce = config.channel_nonce;
        self.decoder = Some(SnapshotDecoder::new(config)?);
        self.terminal = None;
        self.pane_cache.clear();
        self.mouse_encoder.reset();
        self.selection_gesture = None;
        self.search = None;
        self.search_query.clear();
        self.scroll_accum_px = 0.0;
        self.dirty = false;
        self.snapshot_complete = false;
        self.has_rendered_once = false;
        self.last_update_snapshot = UpdateSnapshot::default();
        self.bytes_received = 0;
        self.deferred_live_chunks.clear();
        self.deferred_apcs.clear();
        self.kitty_placements_scratch.clear();
        self.gpu_placements_scratch.clear();
        Ok(())
    }

    #[inline]
    pub fn pane_uid(&self) -> u32 {
        self.pane_uid
    }

    #[inline]
    pub fn key(&self) -> &BlockKey {
        &self.key
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        self.terminal.is_some()
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[inline]
    pub fn is_snapshot_complete(&self) -> bool {
        self.snapshot_complete && self.decoder.is_none()
    }

    #[inline]
    pub fn scroll_offset_y(&self) -> f32 {
        self.scroll_accum_px
    }

    #[inline]
    pub fn terminal(&self) -> Option<&Terminal> {
        self.terminal.as_ref()
    }

    #[inline]
    pub fn last_exited_code(&self) -> Option<i32> {
        self.last_exited_code
    }

    #[inline]
    pub fn last_update_snapshot(&self) -> UpdateSnapshot {
        self.last_update_snapshot
    }

    #[inline]
    pub fn cached_row_count(&self) -> usize {
        self.pane_cache.cached_row_count()
    }

    #[inline]
    pub fn active_search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|_| self.search_query.as_str())
    }
}

/// Manages all live terminal [`PaneRuntime`] instances keyed by [`BlockKey`].
pub struct PaneStore {
    panes: HashMap<BlockKey, PaneRuntime>,
    next_pane_uid: u32,
    active_theme: TerminalTheme,
    key_encoder: KeyEncoder,
    encode_scratch: Vec<u8>,
}

impl fmt::Debug for PaneStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaneStore")
            .field("pane_count", &self.panes.len())
            .field("next_pane_uid", &self.next_pane_uid)
            .finish_non_exhaustive()
    }
}

impl PaneStore {
    /// Creates an empty [`PaneStore`] with default dark theme configuration.
    pub fn new() -> Result<Self, PaneStoreError> {
        let key_encoder = KeyEncoder::new()?;
        Ok(Self {
            panes: HashMap::new(),
            next_pane_uid: 1,
            active_theme: TerminalTheme {
                scheme: Some(ColorScheme::Dark),
                ..TerminalTheme::default()
            },
            key_encoder,
            encode_scratch: Vec::with_capacity(64),
        })
    }

    /// Creates an empty [`PaneStore`] initialized with an Omarchy [`ThemePalette`].
    pub fn with_theme(palette: &ThemePalette) -> Result<Self, PaneStoreError> {
        let mut store = Self::new()?;
        store.active_theme = theme_palette_to_vt_theme(palette);
        Ok(store)
    }

    /// Directly inserts a pre-constructed [`Terminal`] (handy for local previews or unit tests).
    pub fn insert_terminal(
        &mut self,
        key: BlockKey,
        terminal: Terminal,
    ) -> Result<(), PaneStoreError> {
        let uid = self.alloc_pane_uid();
        let _ = terminal.set_theme(&self.active_theme);
        let runtime = PaneRuntime::from_terminal(uid, key.clone(), terminal)?;
        self.panes.insert(key, runtime);
        Ok(())
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.panes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    #[inline]
    pub fn contains(&self, key: &BlockKey) -> bool {
        self.panes.contains_key(key)
    }

    #[inline]
    pub fn get(&self, key: &BlockKey) -> Option<&PaneRuntime> {
        self.panes.get(key)
    }

    #[inline]
    pub fn is_ready(&self, key: &BlockKey) -> bool {
        self.panes.get(key).is_some_and(PaneRuntime::is_ready)
    }

    #[inline]
    pub fn is_dirty(&self, key: &BlockKey) -> bool {
        self.panes.get(key).is_some_and(PaneRuntime::is_dirty)
    }

    /// Returns `true` if any tracked pane needs a GPU repaint.
    pub fn any_dirty(&self) -> bool {
        self.panes.values().any(|p| p.dirty)
    }

    /// Returns `true` if any pane in `keys` (e.g. currently visible panes in the active window)
    /// needs a GPU repaint. Prevents background tabs from causing an idle redraw spin loop.
    pub fn any_dirty_in<'a>(&self, keys: impl IntoIterator<Item = &'a BlockKey>) -> bool {
        keys.into_iter().any(|k| self.is_dirty(k))
    }

    /// Immediately resizes a ready pane's local VT grid, clears its cached GPU rows and fractional
    /// scroll offset, and marks it dirty for instant tear-free repaint.
    pub fn resize_local(
        &mut self,
        key: &BlockKey,
        cols: u16,
        rows: u16,
        cell_w_px: u32,
        cell_h_px: u32,
    ) -> bool {
        let Some(pane) = self.panes.get_mut(key) else {
            return false;
        };
        let Some(term) = pane.terminal.as_ref() else {
            return false;
        };
        if term.resize(cols, rows, cell_w_px, cell_h_px).is_ok() {
            pane.scroll_accum_px = 0.0;
            pane.pane_cache.clear();
            pane.dirty = true;
            true
        } else {
            false
        }
    }

    /// Returns `true` if any pane still has an active progressive `GHOSTSNP` background decoder.
    pub fn has_pending_snapshots(&self) -> bool {
        self.panes.values().any(|p| p.decoder.is_some())
    }

    /// Returns the terminal grid `(cols, rows)` of a ready pane.
    pub fn size(&self, key: &BlockKey) -> Option<(u16, u16)> {
        self.panes.get(key)?.terminal.as_ref()?.size().ok()
    }

    /// Returns the scrollbar state of a ready pane.
    pub fn scrollbar(&self, key: &BlockKey) -> Option<Scrollbar> {
        self.panes.get(key)?.terminal.as_ref()?.scrollbar().ok()
    }

    /// Returns the current smooth-scroll fractional Y offset in logical pixels for `key`.
    pub fn scroll_offset_y(&self, key: &BlockKey) -> f32 {
        self.panes.get(key).map_or(0.0, |p| p.scroll_accum_px)
    }

    /// Returns the live Kitty keyboard flags of a ready pane.
    pub fn kitty_keyboard_flags(&self, key: &BlockKey) -> Option<KittyKeyboardFlags> {
        self.panes
            .get(key)?
            .terminal
            .as_ref()?
            .kitty_keyboard_flags()
            .ok()
    }

    /// Returns the active mouse tracking mode of a ready pane.
    pub fn mouse_tracking_mode(&self, key: &BlockKey) -> Option<MouseTrackingMode> {
        self.panes
            .get(key)?
            .terminal
            .as_ref()?
            .mouse_tracking_mode()
            .ok()
    }

    /// Returns an iterator of keys currently marked dirty.
    pub fn dirty_keys(&self) -> Vec<BlockKey> {
        self.panes
            .iter()
            .filter(|&(_, p)| p.dirty)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Returns all currently tracked pane keys.
    pub fn pane_keys(&self) -> Vec<BlockKey> {
        self.panes.keys().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // DataStreamEvent ingestion & progressive GHOSTSNP decoding
    // -----------------------------------------------------------------------

    /// Processes a single [`DataStreamEvent`] emitted by [`crate::host_manager::HostManager`].
    pub fn handle_data_stream_event(
        &mut self,
        event: DataStreamEvent,
        workspace: &mut WorkspaceState,
    ) -> Result<PaneStreamOutcome, PaneStoreError> {
        let key = BlockKey::new(event.host_id, event.session_id, event.block_id);
        let mut outcome = PaneStreamOutcome::default();

        match event.kind {
            DataStreamEventKind::Opened { welcome }
            | DataStreamEventKind::ReopenedFreshSnapshot { welcome } => {
                let cfg = self.terminal_config(&key, Some(welcome.channel_nonce));
                if let Some(existing) = self.panes.get_mut(&key) {
                    existing.reset_for_fresh_snapshot(cfg)?;
                } else {
                    let uid = self.alloc_pane_uid();
                    let pane = PaneRuntime::new(uid, key.clone(), cfg)?;
                    self.panes.insert(key.clone(), pane);
                }
                if let Some(session) = workspace.session_mut(&crate::workspace::SessionKey::new(
                    key.host_id.clone(),
                    key.session_id.clone(),
                )) && let Some(blk) = session.blocks.get_mut(&key.block_id)
                {
                    blk.snapshot_ready = false;
                    blk.last_sent_resize = None;
                    blk.last_server_size = None;
                }
            }

            DataStreamEventKind::SnapshotChunk {
                bytes,
                bytes_received,
            } => {
                let active_theme = self.active_theme.clone();
                let pane = self.ensure_pane_mut(&key, None)?;
                pane.bytes_received = bytes_received;

                let feed_status = if let Some(decoder) = pane.decoder.as_mut() {
                    decoder.feed(&bytes)?
                } else {
                    // Fallback: snapshot already finished; treat late bytes as live VT.
                    if let Some(term) = pane.terminal.as_ref() {
                        term.write(&bytes);
                        let events = term.drain_events();
                        sync_terminal_events_to_workspace(&key, &events, workspace, pane);
                        outcome.terminal_events.extend(events);
                        pane.dirty = true;
                        outcome.dirty = true;
                    }
                    return Ok(outcome);
                };

                if let SnapshotFeedStatus::Ready(terminal) = feed_status {
                    terminal.set_channel_nonce(pane.channel_nonce);
                    if let Err(err) = terminal.set_theme(&active_theme) {
                        tracing::warn!(?err, block = %key.block_id, "failed to apply initial theme on READY");
                    }
                    if let Some(scheme) = active_theme.scheme {
                        terminal.set_color_scheme(scheme);
                    }

                    // Apply any Rex APCs or live chunks that raced ahead of READY.
                    for apc in pane.deferred_apcs.drain(..) {
                        if let Err(err) = terminal.apply_rex_apc(&apc) {
                            tracing::warn!(?err, block = %key.block_id, "failed to apply deferred rex1 APC");
                        }
                    }
                    for chunk in pane.deferred_live_chunks.drain(..) {
                        terminal.write(&chunk);
                    }

                    let events = terminal.drain_events();
                    sync_terminal_events_to_workspace(&key, &events, workspace, pane);
                    outcome.terminal_events.extend(events);

                    // Sync initial title/pwd from decoded snapshot into workspace metadata.
                    sync_initial_metadata_to_workspace(&key, &terminal, workspace);

                    pane.selection_gesture = Some(SelectionGesture::new(&terminal)?);
                    pane.terminal = Some(terminal);
                    pane.dirty = true;
                    outcome.dirty = true;
                    outcome.became_ready = true;
                    outcome.pending_resize = workspace.mark_snapshot_ready(&key);
                }
            }

            DataStreamEventKind::SnapshotComplete { .. } => {
                outcome.snapshot_complete = true;
                if let Some(pane) = self.panes.get_mut(&key) {
                    pane.snapshot_complete = true;
                    // Run one bounded background slice immediately so compact panes complete on
                    // the spot while huge scrollback panes yield cleanly to `step_background_snapshots`.
                    if pane.decoder.is_some() {
                        let mut finished = false;
                        let mut trailing = Vec::new();
                        let mut changed = false;

                        if let Some(decoder) = pane.decoder.as_mut() {
                            match decoder.step_background(DEFAULT_BACKGROUND_SNAPSHOT_BUDGET)? {
                                BackgroundStep::Finished {
                                    trailing_bytes,
                                    pages_decoded,
                                    rows_prepended,
                                    images_decoded,
                                    ..
                                } => {
                                    finished = true;
                                    trailing = trailing_bytes;
                                    changed = pages_decoded > 0
                                        || rows_prepended > 0
                                        || images_decoded > 0
                                        || !trailing.is_empty();
                                }
                                BackgroundStep::Progress {
                                    pages_decoded,
                                    rows_prepended,
                                    images_decoded,
                                    ..
                                } => {
                                    changed = pages_decoded > 0
                                        || rows_prepended > 0
                                        || images_decoded > 0;
                                }
                                BackgroundStep::NeedMoreData => {}
                            }
                        }

                        if finished {
                            pane.decoder = None;
                            flush_post_finish_bytes(
                                &key,
                                pane,
                                &trailing,
                                workspace,
                                &mut outcome.terminal_events,
                            );
                        }
                        if changed || finished {
                            pane.dirty = true;
                            outcome.dirty = true;
                        }
                    }
                }
            }

            DataStreamEventKind::LiveVtChunk {
                bytes,
                bytes_received,
            } => {
                let pane = self.ensure_pane_mut(&key, None)?;
                pane.bytes_received = bytes_received;

                if let Some(term) = pane.terminal.as_ref() {
                    if !pane.deferred_live_chunks.is_empty() {
                        for queued in pane.deferred_live_chunks.drain(..) {
                            term.write(&queued);
                        }
                    }
                    term.write(&bytes);
                    let events = term.drain_events();
                    sync_terminal_events_to_workspace(&key, &events, workspace, pane);
                    outcome.terminal_events.extend(events);
                    pane.dirty = true;
                    outcome.dirty = true;
                } else {
                    pane.deferred_live_chunks.push(bytes);
                }
            }

            DataStreamEventKind::RexApc(msg) => {
                let vt_apc = rex_control_msg_to_vt_apc(&msg);
                let pane = self.ensure_pane_mut(&key, None)?;

                match &vt_apc {
                    RexControlApc::Resize {
                        cols, rows, owner, ..
                    } => {
                        pane.scroll_accum_px = 0.0;
                        pane.pane_cache.clear();
                        workspace.note_server_resize(&key, *cols, *rows, owner.clone());
                    }
                    RexControlApc::ChildExited { exit_code } => {
                        pane.last_exited_code = Some(*exit_code);
                        if let Some(session) =
                            workspace.session_mut(&crate::workspace::SessionKey::new(
                                key.host_id.clone(),
                                key.session_id.clone(),
                            ))
                        {
                            let blk = session.blocks.entry(key.block_id.clone()).or_default();
                            blk.exited_code = Some(*exit_code);
                        }
                    }
                    RexControlApc::Theme { .. } | RexControlApc::Unknown { .. } => {}
                }

                if let Some(term) = pane.terminal.as_ref() {
                    term.apply_rex_apc(&vt_apc)?;
                    pane.dirty = true;
                    outcome.dirty = true;
                } else {
                    pane.deferred_apcs.push(vt_apc.clone());
                }
                outcome.rex_apc = Some(vt_apc);
            }

            DataStreamEventKind::Resumed {
                bytes_received,
                welcome,
            } => {
                if let Some(pane) = self.panes.get_mut(&key) {
                    pane.channel_nonce = Some(welcome.channel_nonce);
                    pane.bytes_received = bytes_received;
                    if let Some(term) = pane.terminal.as_ref() {
                        term.set_channel_nonce(Some(welcome.channel_nonce));
                    }
                }
            }

            DataStreamEventKind::Closed { action } => {
                if let Some(pane) = self.panes.get_mut(&key) {
                    pane.decoder = None;
                }
                outcome.stream_closed = Some(action);
            }

            DataStreamEventKind::Error { message } => {
                outcome.stream_error = Some(message);
            }
        }

        Ok(outcome)
    }

    /// Advances active background `GHOSTSNP` scrollback & Kitty image decoders across all panes
    /// within a strict `budget` (e.g. `Duration::from_millis(2)` per frame tick) so the UI thread
    /// never drops frames while large scrollback histories decompress.
    pub fn step_background_snapshots(
        &mut self,
        budget: Duration,
        workspace: &mut WorkspaceState,
    ) -> Result<BackgroundSnapshotSummary, PaneStoreError> {
        let mut summary = BackgroundSnapshotSummary::default();
        let deadline = Instant::now() + budget;

        let active_keys: Vec<BlockKey> = self
            .panes
            .iter()
            .filter(|&(_, p)| p.decoder.is_some())
            .map(|(k, _)| k.clone())
            .collect();

        for key in active_keys {
            let now = Instant::now();
            if now >= deadline {
                summary.more_pending = true;
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let Some(pane) = self.panes.get_mut(&key) else {
                continue;
            };
            let Some(decoder) = pane.decoder.as_mut() else {
                continue;
            };

            summary.panes_stepped += 1;
            match decoder.step_background(remaining)? {
                BackgroundStep::NeedMoreData => {}
                BackgroundStep::Progress {
                    pages_decoded,
                    rows_prepended,
                    images_decoded,
                    image_bytes,
                    more_buffered,
                } => {
                    summary.pages_decoded += pages_decoded;
                    summary.rows_prepended += rows_prepended;
                    summary.images_decoded += images_decoded;
                    summary.image_bytes += image_bytes;
                    summary.more_pending |= more_buffered;
                    if pages_decoded > 0 || rows_prepended > 0 || images_decoded > 0 {
                        pane.dirty = true;
                        summary.dirty_panes.push(key);
                    }
                }
                BackgroundStep::Finished {
                    trailing_bytes,
                    pages_decoded,
                    rows_prepended,
                    images_decoded,
                    image_bytes,
                    ..
                } => {
                    summary.panes_finished += 1;
                    summary.pages_decoded += pages_decoded;
                    summary.rows_prepended += rows_prepended;
                    summary.images_decoded += images_decoded;
                    summary.image_bytes += image_bytes;
                    pane.decoder = None;
                    pane.snapshot_complete = true;

                    let mut events = Vec::new();
                    flush_post_finish_bytes(&key, pane, &trailing_bytes, workspace, &mut events);
                    if !events.is_empty() {
                        summary.terminal_events.push((key.clone(), events));
                    }
                    pane.dirty = true;
                    summary.dirty_panes.push(key);
                }
            }
        }

        if !summary.more_pending {
            summary.more_pending = self.panes.values().any(|p| p.decoder.is_some());
        }
        Ok(summary)
    }

    // -----------------------------------------------------------------------
    // Two-phase GPU render bridge + Kitty Graphics upload/compositing
    // -----------------------------------------------------------------------

    /// Renders a ready pane into `draw_list` using the two-phase lock-minimal `RenderState` pipeline,
    /// `RowId`-cached [`TerminalPaneCache`], and z-ordered Kitty Graphics [`ImageTextureCache`].
    ///
    /// Returns `Ok(None)` if the pane does not exist or has not reached `GHOSTSNP` `READY` yet.
    pub fn render_pane(
        &mut self,
        key: &BlockKey,
        viewport: &TerminalViewport,
        overscan: Overscan,
        renderer: &mut GpuRenderer,
        draw_list: &mut DrawList,
    ) -> Result<Option<PaneRenderResult>, PaneStoreError> {
        let Some(pane) = self.panes.get_mut(key) else {
            return Ok(None);
        };
        let Some(term) = pane.terminal.clone() else {
            return Ok(None);
        };

        let held = term.is_render_held();
        if !held || !pane.has_rendered_once {
            // Phase 1: microsecond terminal lock.
            let snap = pane.render_state.begin_update(&term, overscan)?;
            // Phase 2: completely lock-free row/color/cursor/placement resolution.
            pane.render_state.end_update()?;
            pane.last_update_snapshot = snap;
            pane.has_rendered_once = true;
        }

        // `wgpu::Queue` is an inexpensive `Arc`-backed handle; clone it to pass alongside `atlas_mut`.
        let queue = renderer.queue().clone();
        let stats = pane.pane_cache.build_from_vt_render_state(
            &mut pane.render_state,
            viewport,
            renderer.atlas_mut(),
            &queue,
            draw_list,
        )?;

        let (kitty_placements, kitty_uploads) =
            emit_kitty_placements_with_renderer(pane, &term, viewport, renderer, draw_list)?;

        if !held {
            pane.render_state.clean()?;
            pane.dirty = false;
        }

        Ok(Some(PaneRenderResult {
            stats,
            kitty_placements,
            kitty_uploads,
            held_by_sync_output: held,
            scrollbar: pane.last_update_snapshot.scrollbar,
            viewport_active: pane.last_update_snapshot.viewport_active,
            scroll_offset_y: viewport.scroll_offset_y,
        }))
    }

    /// Lower-level variant of [`Self::render_pane`] taking individual GPU components directly.
    #[allow(clippy::too_many_arguments)]
    pub fn render_pane_parts(
        &mut self,
        key: &BlockKey,
        viewport: &TerminalViewport,
        overscan: Overscan,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut GlyphAtlas,
        image_cache: &mut ImageTextureCache,
        draw_list: &mut DrawList,
    ) -> Result<Option<PaneRenderResult>, PaneStoreError> {
        let Some(pane) = self.panes.get_mut(key) else {
            return Ok(None);
        };
        let Some(term) = pane.terminal.clone() else {
            return Ok(None);
        };

        let held = term.is_render_held();
        if !held || !pane.has_rendered_once {
            let snap = pane.render_state.begin_update(&term, overscan)?;
            pane.render_state.end_update()?;
            pane.last_update_snapshot = snap;
            pane.has_rendered_once = true;
        }

        let stats = pane.pane_cache.build_from_vt_render_state(
            &mut pane.render_state,
            viewport,
            atlas,
            queue,
            draw_list,
        )?;

        let (kitty_placements, kitty_uploads) = emit_kitty_placements_with_parts(
            pane,
            &term,
            viewport,
            device,
            queue,
            image_cache,
            draw_list,
        )?;

        if !held {
            pane.render_state.clean()?;
            pane.dirty = false;
        }

        Ok(Some(PaneRenderResult {
            stats,
            kitty_placements,
            kitty_uploads,
            held_by_sync_output: held,
            scrollbar: pane.last_update_snapshot.scrollbar,
            viewport_active: pane.last_update_snapshot.viewport_active,
            scroll_offset_y: viewport.scroll_offset_y,
        }))
    }

    /// Advances Kitty Graphics animations across all live panes and returns the earliest monotonic
    /// millisecond timestamp for the next animation frame, if any animation is running.
    pub fn tick_kitty_animations(&mut self, now_ms: u64) -> Option<u64> {
        let mut earliest: Option<u64> = None;
        for pane in self.panes.values_mut() {
            let Some(term) = pane.terminal.as_ref() else {
                continue;
            };
            if let Ok(Some(next_ms)) = vt_engine::tick_kitty_animations(term, now_ms) {
                pane.dirty = true;
                earliest = Some(earliest.map_or(next_ms, |prev| prev.min(next_ms)));
            }
        }
        earliest
    }

    // -----------------------------------------------------------------------
    // Theme propagation & session/block eviction
    // -----------------------------------------------------------------------

    /// Applies an updated Omarchy [`ThemePalette`] to all live terminals and invalidates cached rows.
    ///
    /// Returns `(BlockKey, report_bytes)` pairs for any live terminal that has DEC Mode 2031
    /// color-scheme reporting enabled so the caller can forward `CSI ? 997 ; 1/2 n` to Rex.
    pub fn apply_theme_all(&mut self, palette: &ThemePalette) -> Vec<(BlockKey, Vec<u8>)> {
        let vt_theme = theme_palette_to_vt_theme(palette);
        let scheme = vt_theme.scheme.unwrap_or(ColorScheme::Dark);
        self.active_theme = vt_theme;

        let mut mode_2031_reports = Vec::new();
        for (key, pane) in &mut self.panes {
            pane.pane_cache.clear();
            let Some(term) = pane.terminal.as_ref() else {
                continue;
            };
            if let Err(err) = term.set_theme(&self.active_theme) {
                tracing::warn!(?err, block = %key.block_id, "failed to apply live theme");
            }
            term.set_color_scheme(scheme);
            pane.dirty = true;

            if term.is_mode_2031_enabled()
                && let Ok(report) = vt_engine::encode_color_scheme_report(scheme)
            {
                mode_2031_reports.push((key.clone(), report));
            }
        }
        mode_2031_reports
    }

    /// Removes a single block runtime and optionally releases its Kitty Graphics GPU textures.
    pub fn remove_block(
        &mut self,
        key: &BlockKey,
        image_cache: Option<&mut ImageTextureCache>,
    ) -> bool {
        let Some(removed) = self.panes.remove(key) else {
            return false;
        };
        if let Some(cache) = image_cache {
            let uid = u64::from(removed.pane_uid);
            cache.retain(|id| (id >> 32) != uid);
        }
        true
    }

    /// Evicts all panes belonging to `(host_id, session_id)` and releases their Kitty GPU textures.
    pub fn evict_session(
        &mut self,
        host_id: &str,
        session_id: &str,
        image_cache: Option<&mut ImageTextureCache>,
    ) -> usize {
        let keys: Vec<BlockKey> = self
            .panes
            .keys()
            .filter(|k| k.host_id == host_id && k.session_id == session_id)
            .cloned()
            .collect();
        let mut evicted_uids = Vec::with_capacity(keys.len());
        for key in &keys {
            if let Some(pane) = self.panes.remove(key) {
                evicted_uids.push(u64::from(pane.pane_uid));
            }
        }
        if let Some(cache) = image_cache
            && !evicted_uids.is_empty()
        {
            cache.retain(|id| !evicted_uids.contains(&(id >> 32)));
        }
        evicted_uids.len()
    }

    /// Evicts all panes belonging to `host_id` and releases their Kitty GPU textures.
    pub fn evict_host(
        &mut self,
        host_id: &str,
        image_cache: Option<&mut ImageTextureCache>,
    ) -> usize {
        let keys: Vec<BlockKey> = self
            .panes
            .keys()
            .filter(|k| k.host_id == host_id)
            .cloned()
            .collect();
        let mut evicted_uids = Vec::with_capacity(keys.len());
        for key in &keys {
            if let Some(pane) = self.panes.remove(key) {
                evicted_uids.push(u64::from(pane.pane_uid));
            }
        }
        if let Some(cache) = image_cache
            && !evicted_uids.is_empty()
        {
            cache.retain(|id| !evicted_uids.contains(&(id >> 32)));
        }
        evicted_uids.len()
    }

    // -----------------------------------------------------------------------
    // Keyboard, Mouse, Paste & Focus encoders
    // -----------------------------------------------------------------------

    /// Encodes a platform-normalized key event synced against the pane's live terminal flags
    /// (including Kitty Keyboard Protocol flags, application cursor/keypad modes, and AltGr text).
    pub fn encode_key(
        &mut self,
        key: &BlockKey,
        input: &KeyEventInput<'_>,
    ) -> Result<Vec<u8>, PaneStoreError> {
        let term = self.ready_terminal(key)?.clone();
        self.key_encoder.sync_from_terminal(&term);
        self.encode_scratch.clear();
        self.key_encoder.encode(input, &mut self.encode_scratch)?;
        Ok(self.encode_scratch.clone())
    }

    /// Encodes paste bytes, automatically checking the pane's DEC Mode 2004 bracketed paste flag.
    pub fn encode_paste(&self, key: &BlockKey, data: &[u8]) -> Result<Vec<u8>, PaneStoreError> {
        let term = self.ready_terminal(key)?;
        let bracketed = term.mode(2004, false).unwrap_or(false);
        Ok(vt_engine::encode_paste(data, bracketed)?)
    }

    /// Encodes a DEC Mode 1004 focus report if the pane has focus reporting enabled.
    pub fn encode_focus(
        &self,
        key: &BlockKey,
        gained: bool,
    ) -> Result<Option<Vec<u8>>, PaneStoreError> {
        let term = self.ready_terminal(key)?;
        if !term.mode(1004, false).unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(vt_engine::encode_focus(gained)?))
    }

    /// Encodes a pointer/wheel event for a mouse-tracking pane, honoring Shift-bypass.
    ///
    /// Returns `Ok(None)` if mouse tracking is disabled, Shift-bypass is active (so the caller
    /// should fall through to native text selection / scrollback), or the motion was deduplicated.
    pub fn encode_mouse(
        &mut self,
        key: &BlockKey,
        input: &MouseEventInput,
        geometry: MouseGeometry,
    ) -> Result<Option<Vec<u8>>, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(term) = pane.terminal.as_ref() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };

        let mode = term.mouse_tracking_mode()?;
        if matches!(mode, MouseTrackingMode::None) {
            return Ok(None);
        }

        pane.mouse_encoder.sync_from_terminal(term);
        pane.mouse_encoder.set_geometry(geometry);
        match input.action {
            MouseAction::Press => pane.mouse_encoder.set_any_button_pressed(true),
            MouseAction::Release => pane.mouse_encoder.set_any_button_pressed(false),
            MouseAction::Motion => {}
        }

        let mut out = Vec::with_capacity(32);
        let encoded = pane
            .mouse_encoder
            .encode_with_shift_bypass(input, mode, &mut out)?;
        if encoded && !out.is_empty() {
            Ok(Some(out))
        } else {
            Ok(None)
        }
    }

    // -----------------------------------------------------------------------
    // Selection gestures, clipboard extraction & OSC 8 hyperlinks
    // -----------------------------------------------------------------------

    /// Starts or advances a single/double/triple-click selection gesture.
    pub fn selection_press(
        &mut self,
        key: &BlockKey,
        col: u16,
        row: u32,
        px_x: f64,
        px_y: f64,
        time_ns: u64,
    ) -> Result<bool, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        if let Some(term) = pane.terminal.as_ref() {
            term.clear_render_hold();
        }
        let Some(gesture) = pane.selection_gesture.as_mut() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        let installed = gesture.press(col, row, px_x, px_y, time_ns)?;
        pane.dirty = true;
        Ok(installed)
    }

    /// Updates an active selection gesture on pointer drag (`rectangle = true` for Alt+drag).
    #[allow(clippy::too_many_arguments)]
    pub fn selection_drag(
        &mut self,
        key: &BlockKey,
        col: u16,
        row: u32,
        px_x: f64,
        px_y: f64,
        geometry: SelectionGeometry,
        rectangle: bool,
    ) -> Result<bool, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(gesture) = pane.selection_gesture.as_mut() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        let changed = gesture.drag(col, row, px_x, px_y, geometry, rectangle)?;
        if changed {
            pane.dirty = true;
        }
        Ok(changed)
    }

    /// Finalizes a selection gesture on pointer release.
    pub fn selection_release(
        &mut self,
        key: &BlockKey,
        cell: Option<(u16, u32)>,
    ) -> Result<(), PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(gesture) = pane.selection_gesture.as_mut() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        gesture.release(cell)?;
        pane.dirty = true;
        Ok(())
    }

    /// Advances selection while dragging past pane edges.
    #[allow(clippy::too_many_arguments)]
    pub fn selection_autoscroll_tick(
        &mut self,
        key: &BlockKey,
        col: u16,
        row: u32,
        px_x: f64,
        px_y: f64,
        geometry: SelectionGeometry,
        rectangle: bool,
    ) -> Result<(bool, SelectionAutoscroll), PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(gesture) = pane.selection_gesture.as_mut() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        let changed = gesture.autoscroll_tick(col, row, px_x, px_y, geometry, rectangle)?;
        let dir = gesture.autoscroll_direction()?;
        if changed {
            pane.dirty = true;
        }
        Ok((changed, dir))
    }

    /// Selects all text in the active screen and scrollback.
    pub fn select_all(&mut self, key: &BlockKey) -> Result<bool, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(term) = pane.terminal.as_ref() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        let selected = vt_engine::select_all(term)?;
        if selected {
            pane.dirty = true;
        }
        Ok(selected)
    }

    /// Clears any active selection on the pane.
    pub fn clear_selection(&mut self, key: &BlockKey) -> Result<(), PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(term) = pane.terminal.as_ref() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        vt_engine::clear_selection(term)?;
        pane.dirty = true;
        Ok(())
    }

    /// Copies the active terminal selection formatted as plain UTF-8 text.
    pub fn copy_selection_text(
        &self,
        key: &BlockKey,
        unwrap: bool,
        trim: bool,
    ) -> Result<Option<String>, PaneStoreError> {
        let term = self.ready_terminal(key)?;
        Ok(vt_engine::selection_text(term, unwrap, trim)?)
    }

    /// Extracts the OSC 8 hyperlink URI at viewport coordinate `(col, row)`, if present.
    pub fn hyperlink_at(
        &self,
        key: &BlockKey,
        col: u16,
        row: u32,
    ) -> Result<Option<String>, PaneStoreError> {
        let term = self.ready_terminal(key)?;
        Ok(vt_engine::hyperlink_at(term, col, row)?)
    }

    /// Extracts the unwrapped logical line containing viewport coordinate `(col, row)` along with
    /// the 0-based Unicode character index corresponding to `(col, row)`.
    pub fn logical_line_at(
        &self,
        key: &BlockKey,
        col: u16,
        row: u32,
    ) -> Result<Option<(String, usize)>, PaneStoreError> {
        let term = self.ready_terminal(key)?;
        Ok(vt_engine::logical_line_at(term, col, row)?)
    }

    /// Extracts the unwrapped logical line across both VT soft-wraps and TUI box gutters (`│ `)
    /// along with exact per-character viewport cell coordinates for hover underline rendering.
    pub fn logical_line_with_tui_unwrap_at(
        &self,
        key: &BlockKey,
        col: u16,
        row: u32,
    ) -> Result<Option<vt_engine::UnwrappedLine>, PaneStoreError> {
        let term = self.ready_terminal(key)?;
        Ok(vt_engine::logical_line_with_tui_unwrap_at(term, col, row)?)
    }

    /// Returns all visible viewport lines stitched across VT soft-wraps and TUI gutters.
    pub fn viewport_text_lines(&self, key: &BlockKey) -> Result<Vec<String>, PaneStoreError> {
        let term = self.ready_terminal(key)?;
        Ok(vt_engine::viewport_text_lines(term)?)
    }

    /// Returns the topmost Kitty image placement covering viewport cell `(col, row)`, if any.
    pub fn kitty_placement_at(
        &self,
        key: &BlockKey,
        col: u16,
        row: u32,
    ) -> Option<VtKittyPlacement> {
        let pane = self.panes.get(key)?;
        vt_engine::kitty_placement_at(&pane.kitty_placements_scratch, col, row)
    }

    /// Reads decoded RGBA8 pixel data for a Kitty graphics `image_id` from a ready pane.
    pub fn read_kitty_image(
        &self,
        key: &BlockKey,
        image_id: u32,
    ) -> Result<Option<vt_engine::KittyImagePixels>, PaneStoreError> {
        let term = self.ready_terminal(key)?;
        Ok(vt_engine::read_kitty_image(term, image_id)?)
    }

    /// Returns the OSC 7 working directory reported by the pane's terminal, if any.
    pub fn pwd(&self, key: &BlockKey) -> Option<String> {
        self.panes
            .get(key)?
            .terminal
            .as_ref()?
            .pwd()
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    // -----------------------------------------------------------------------
    // Smooth overscan scrolling
    // -----------------------------------------------------------------------

    /// Applies a smooth vertical scroll delta in logical pixels (`delta_y_px > 0` scrolls up into
    /// older scrollback; `delta_y_px < 0` scrolls down toward newer output).
    ///
    /// Whole-line crossings step `Terminal::scroll_viewport(ScrollViewport::Delta(..))`, while the
    /// fractional remainder in `(-cell_height_px, +cell_height_px)` is stored in `scroll_accum_px`
    /// for overscan GPU translation with clean clamping at top/bottom scrollback bounds.
    pub fn scroll_wheel_px(
        &mut self,
        key: &BlockKey,
        delta_y_px: f32,
        cell_height_px: f32,
    ) -> Result<f32, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(term) = pane.terminal.as_ref() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };

        let cell_h = cell_height_px.max(1.0);
        let initial_sb = term.scrollbar()?;
        if initial_sb.total <= initial_sb.len {
            pane.scroll_accum_px = 0.0;
            return Ok(0.0);
        }

        pane.scroll_accum_px += delta_y_px;

        // Whole-line steps: positive accum => scroll UP (negative delta in Ghostty viewport coords).
        let whole_lines = (pane.scroll_accum_px / cell_h).trunc() as isize;
        if whole_lines != 0 {
            term.scroll_viewport(ScrollViewport::Delta(-whole_lines));
            pane.scroll_accum_px -= (whole_lines as f32) * cell_h;
        }

        // Clamp fractional overscan strictly inside scrollback bounds so we never expose blank edges.
        let sb = term.scrollbar()?;
        let at_top = sb.offset == 0;
        let at_bottom = sb.offset.saturating_add(sb.len) >= sb.total;
        if (at_top && pane.scroll_accum_px > 0.0) || (at_bottom && pane.scroll_accum_px < 0.0) {
            pane.scroll_accum_px = 0.0;
        } else {
            pane.scroll_accum_px = pane.scroll_accum_px.clamp(-(cell_h - 0.5), cell_h - 0.5);
        }

        pane.dirty = true;
        Ok(pane.scroll_accum_px)
    }

    /// Jumps viewport scroll directly (e.g. `ScrollViewport::Top`, `Bottom`, `Row(r)`), clearing
    /// fractional smooth-scroll offset.
    pub fn scroll_viewport(
        &mut self,
        key: &BlockKey,
        scroll: ScrollViewport,
    ) -> Result<(), PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(term) = pane.terminal.as_ref() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        term.scroll_viewport(scroll);
        pane.scroll_accum_px = 0.0;
        pane.dirty = true;
        Ok(())
    }

    /// Snaps viewport to the live bottom row and clears fractional smooth scroll.
    pub fn scroll_to_bottom(&mut self, key: &BlockKey) -> Result<(), PaneStoreError> {
        self.scroll_viewport(key, ScrollViewport::Bottom)
    }

    /// Snaps viewport to the top of scrollback and clears fractional smooth scroll.
    pub fn scroll_to_top(&mut self, key: &BlockKey) -> Result<(), PaneStoreError> {
        self.scroll_viewport(key, ScrollViewport::Top)
    }

    // -----------------------------------------------------------------------
    // Per-pane scrollback search (`TerminalSearch`)
    // -----------------------------------------------------------------------

    /// Opens (or reuses) a [`TerminalSearch`] session attached to `key`.
    pub fn search_open(&mut self, key: &BlockKey) -> Result<(), PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        if pane.search.is_some() {
            return Ok(());
        }
        let Some(term) = pane.terminal.as_ref() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        let mut search = TerminalSearch::new(term)?;
        search.set_scroll_policy(SearchScroll::IfNeeded)?;
        pane.search = Some(search);
        pane.search_query.clear();
        Ok(())
    }

    /// Sets the search needle, runs incremental search to completion, and returns visible matches.
    pub fn search_query(
        &mut self,
        key: &BlockKey,
        needle: &str,
    ) -> Result<SearchSummary, PaneStoreError> {
        self.search_open(key)?;
        let pane = self.ready_pane_mut(key)?;
        pane.search_query.clear();
        pane.search_query.push_str(needle);

        let Some(search) = pane.search.as_mut() else {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        };
        search.set_needle(needle)?;
        if !needle.is_empty() {
            search.run()?;
        }
        let status = search.status()?;
        let total_matches = search.total_matches()?;
        let selected_index = search.selected_index()?;
        let viewport_matches = search.viewport_matches()?;
        pane.dirty = true;

        Ok(SearchSummary {
            query: pane.search_query.clone(),
            status,
            total_matches,
            selected_index,
            viewport_matches,
        })
    }

    /// Advances an active search incrementally without blocking on large scrollback buffers.
    pub fn search_step(
        &mut self,
        key: &BlockKey,
        max_ticks: usize,
    ) -> Result<SearchStatus, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(search) = pane.search.as_mut() else {
            return Ok(SearchStatus::Complete);
        };
        let mut status = search.status()?;
        for _ in 0..max_ticks.max(1) {
            match status {
                SearchStatus::Complete => break,
                SearchStatus::FeedRequired => {
                    search.feed()?;
                    status = search.tick()?;
                }
                SearchStatus::Running => {
                    status = search.tick()?;
                }
            }
        }
        pane.dirty = true;
        Ok(status)
    }

    /// Jumps selection/viewport to the next search match (older history).
    pub fn search_next(&mut self, key: &BlockKey) -> Result<bool, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(search) = pane.search.as_mut() else {
            return Ok(false);
        };
        let moved = search.select_next()?;
        if moved {
            pane.scroll_accum_px = 0.0;
            pane.dirty = true;
        }
        Ok(moved)
    }

    /// Jumps selection/viewport to the previous search match (newer output).
    pub fn search_prev(&mut self, key: &BlockKey) -> Result<bool, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(search) = pane.search.as_mut() else {
            return Ok(false);
        };
        let moved = search.select_prev()?;
        if moved {
            pane.scroll_accum_px = 0.0;
            pane.dirty = true;
        }
        Ok(moved)
    }

    /// Projects all visible search matches into viewport `(col, row)` coordinates for highlight quads.
    pub fn search_viewport_matches(
        &mut self,
        key: &BlockKey,
    ) -> Result<Vec<ViewportMatch>, PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        let Some(search) = pane.search.as_mut() else {
            return Ok(Vec::new());
        };
        Ok(search.viewport_matches()?)
    }

    /// Closes the pane's active search session and frees its native searcher handle.
    pub fn search_close(&mut self, key: &BlockKey) {
        if let Some(pane) = self.panes.get_mut(key) {
            pane.search = None;
            pane.search_query.clear();
            pane.dirty = true;
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn alloc_pane_uid(&mut self) -> u32 {
        let uid = self.next_pane_uid.max(1);
        self.next_pane_uid = uid.wrapping_add(1).max(1);
        uid
    }

    fn terminal_config(&self, key: &BlockKey, channel_nonce: Option<u64>) -> TerminalConfig {
        TerminalConfig {
            compress_history: true,
            kitty_image_medium_file: key.host_id == LOCAL_HOST_ID,
            initial_scheme: self.active_theme.scheme.unwrap_or(ColorScheme::Dark),
            channel_nonce,
            ..TerminalConfig::default()
        }
    }

    fn ensure_pane_mut(
        &mut self,
        key: &BlockKey,
        channel_nonce: Option<u64>,
    ) -> Result<&mut PaneRuntime, PaneStoreError> {
        if !self.panes.contains_key(key) {
            let uid = self.alloc_pane_uid();
            let cfg = self.terminal_config(key, channel_nonce);
            let pane = PaneRuntime::new(uid, key.clone(), cfg)?;
            self.panes.insert(key.clone(), pane);
        }
        self.panes
            .get_mut(key)
            .ok_or_else(|| PaneStoreError::PaneNotFound(key.clone()))
    }

    fn ready_terminal(&self, key: &BlockKey) -> Result<&Terminal, PaneStoreError> {
        let pane = self
            .panes
            .get(key)
            .ok_or_else(|| PaneStoreError::PaneNotFound(key.clone()))?;
        pane.terminal
            .as_ref()
            .ok_or_else(|| PaneStoreError::PaneNotReady(key.clone()))
    }

    fn ready_pane_mut(&mut self, key: &BlockKey) -> Result<&mut PaneRuntime, PaneStoreError> {
        let pane = self
            .panes
            .get_mut(key)
            .ok_or_else(|| PaneStoreError::PaneNotFound(key.clone()))?;
        if pane.terminal.is_none() {
            return Err(PaneStoreError::PaneNotReady(key.clone()));
        }
        Ok(pane)
    }

    #[cfg(test)]
    pub(crate) fn create_local_test_pane(
        &mut self,
        key: &BlockKey,
        cols: u16,
        rows: u16,
    ) -> Result<(), PaneStoreError> {
        let uid = self.alloc_pane_uid();
        let cfg = self.terminal_config(key, None);
        let mut pane = PaneRuntime::new(uid, key.clone(), cfg.clone())?;
        let term = Terminal::new(cols, rows, cfg)?;
        term.resize(cols, rows, 10, 20)?;
        pane.terminal = Some(term);
        pane.dirty = true;
        self.panes.insert(key.clone(), pane);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn feed_local_test_bytes(
        &mut self,
        key: &BlockKey,
        bytes: &[u8],
    ) -> Result<(), PaneStoreError> {
        let pane = self.ready_pane_mut(key)?;
        if let Some(term) = pane.terminal.as_ref() {
            term.write(bytes);
        }
        pane.dirty = true;
        Ok(())
    }
}

fn flush_post_finish_bytes(
    key: &BlockKey,
    pane: &mut PaneRuntime,
    trailing_bytes: &[u8],
    workspace: &mut WorkspaceState,
    out_events: &mut Vec<TerminalEvent>,
) {
    let Some(term) = pane.terminal.as_ref() else {
        return;
    };
    if !trailing_bytes.is_empty() {
        term.write(trailing_bytes);
    }
    for queued in pane.deferred_live_chunks.drain(..) {
        term.write(&queued);
    }
    let events = term.drain_events();
    if !events.is_empty() {
        sync_terminal_events_to_workspace(key, &events, workspace, pane);
        out_events.extend(events);
    }
}

fn sync_initial_metadata_to_workspace(
    key: &BlockKey,
    terminal: &Terminal,
    workspace: &mut WorkspaceState,
) {
    let Some(session) = workspace.session_mut(&crate::workspace::SessionKey::new(
        key.host_id.clone(),
        key.session_id.clone(),
    )) else {
        return;
    };
    let blk = session.blocks.entry(key.block_id.clone()).or_default();
    if let Ok(title) = terminal.title()
        && !title.is_empty()
    {
        blk.title = Some(title);
    }
    if let Ok(pwd) = terminal.pwd()
        && !pwd.is_empty()
    {
        blk.pwd = Some(pwd);
    }
}

fn sync_terminal_events_to_workspace(
    key: &BlockKey,
    events: &[TerminalEvent],
    workspace: &mut WorkspaceState,
    pane: &mut PaneRuntime,
) {
    if events.is_empty() {
        return;
    }
    let session_key =
        crate::workspace::SessionKey::new(key.host_id.clone(), key.session_id.clone());

    for ev in events {
        match ev {
            TerminalEvent::TitleChanged(title) => {
                if let Some(session) = workspace.session_mut(&session_key) {
                    let blk = session.blocks.entry(key.block_id.clone()).or_default();
                    blk.title = if title.is_empty() {
                        None
                    } else {
                        Some(title.clone())
                    };
                }
            }
            TerminalEvent::PwdChanged(pwd) => {
                if let Some(session) = workspace.session_mut(&session_key) {
                    let blk = session.blocks.entry(key.block_id.clone()).or_default();
                    blk.pwd = if pwd.is_empty() {
                        None
                    } else {
                        Some(pwd.clone())
                    };
                }
            }
            TerminalEvent::Bell => {
                if let Some(session) = workspace.session_mut(&session_key) {
                    let blk = session.blocks.entry(key.block_id.clone()).or_default();
                    blk.has_bell = true;
                }
            }
            TerminalEvent::RexApc(RexControlApc::ChildExited { exit_code }) => {
                pane.last_exited_code = Some(*exit_code);
                if let Some(session) = workspace.session_mut(&session_key) {
                    let blk = session.blocks.entry(key.block_id.clone()).or_default();
                    blk.exited_code = Some(*exit_code);
                }
            }
            _ => {}
        }
    }
}

/// Snapshot of terminal cell occupancy for one captured viewport row, used to prevent
/// fixed `AboveText` (`z=0`) inline Kitty images (e.g. `tr3x` QR codes / chat images) from
/// painting over pinned TUI composers or subsequent text lines when scrolled/overrun.
struct KittyRowSummary {
    viewport_y: i32,
    /// Per-column flag: `true` if cell has a visible non-whitespace glyph.
    has_text: Vec<bool>,
    /// Per-column flag: `true` if cell has an explicit non-default background fill.
    has_explicit_bg: Vec<bool>,
}

impl KittyRowSummary {
    fn span_has_text(&self, col_start: usize, col_end: usize) -> bool {
        let lo = col_start.min(self.has_text.len());
        let hi = col_end.min(self.has_text.len());
        self.has_text[lo..hi].iter().any(|&v| v)
    }

    fn span_has_explicit_bg(&self, col_start: usize, col_end: usize) -> bool {
        let lo = col_start.min(self.has_explicit_bg.len());
        let hi = col_end.min(self.has_explicit_bg.len());
        self.has_explicit_bg[lo..hi].iter().any(|&v| v)
    }
}

/// Context needed to align, scale, and clip Kitty placements cleanly against the terminal grid,
/// DECSTBM scroll region, and subsequent non-empty rows.
struct KittyClipContext {
    origin_x: f32,
    origin_y: f32,
    cell_w: f32,
    cell_h: f32,
    /// Pixel ratio `(render_cell_w_px / term_cell_w_px, render_cell_h_px / term_cell_h_px)`
    /// so images placed under a different client's DPI/font size (e.g. 2x Retina Mac) render at
    /// exact cell-aligned dimensions here.
    px_scale_x: f32,
    px_scale_y: f32,
    grid_clip: ClipRect,
    scroll_region: Option<(u16, u16)>,
    viewport_active: bool,
    rows: Vec<KittyRowSummary>,
}

impl KittyClipContext {
    fn build(
        pane: &mut PaneRuntime,
        term: &Terminal,
        viewport: &TerminalViewport,
        scale: f32,
    ) -> Self {
        // Match `terminal_pass.rs` exact physical-pixel grid snapping so Kitty images lock 1:1
        // onto cell boundaries with zero cumulative row drift on fractional font metrics.
        let origin_x_px = (viewport.rect.x * scale).round();
        let origin_y_px = (viewport.rect.y * scale).round();
        let render_cell_w_px = (viewport.metrics.cell_width * scale).round().max(1.0);
        let render_cell_h_px = (viewport.metrics.cell_height * scale).round().max(1.0);

        let origin_x = origin_x_px / scale;
        let origin_y = origin_y_px / scale;
        let cell_w = render_cell_w_px / scale;
        let cell_h = render_cell_h_px / scale;

        let (term_cols, term_rows) = term.size().unwrap_or((viewport.cols, viewport.rows));
        let (term_w_px, term_h_px) = term.pixel_size().unwrap_or((0, 0));
        let (px_scale_x, px_scale_y) =
            if term_cols > 0 && term_rows > 0 && term_w_px > 0 && term_h_px > 0 {
                let term_cell_w = (term_w_px as f32 / f32::from(term_cols)).max(1.0);
                let term_cell_h = (term_h_px as f32 / f32::from(term_rows)).max(1.0);
                (
                    render_cell_w_px / term_cell_w,
                    render_cell_h_px / term_cell_h,
                )
            } else {
                (1.0, 1.0)
            };

        // Clamp placements inside the active terminal cell grid as well as `viewport.clip_rect`
        // so images never bleed into empty pane padding below/right of a smaller server-owned grid.
        let grid_rect = ClipRect {
            min_x: origin_x,
            min_y: origin_y,
            max_x: origin_x + f32::from(term_cols.max(1)) * cell_w,
            max_y: origin_y + f32::from(term_rows.max(1)) * cell_h,
        };
        let grid_clip = viewport.clip_rect.intersect(grid_rect);

        let viewport_active = pane.last_update_snapshot.viewport_active;
        let scroll_region = if viewport_active {
            term.scroll_region()
        } else {
            None
        };

        let needs_row_scan = pane.kitty_placements_scratch.iter().any(|p| {
            p.kind == vt_engine::KittyPlacementKind::Fixed
                && p.z_layer == KittyImageLayer::AboveText
                && p.z_index == 0
                && p.grid_rows >= 2
        });

        let mut rows = Vec::new();
        if needs_row_scan {
            let default_bg = pane.render_state.colors().background;
            let _ = pane.render_state.for_each_row(|row| {
                let mut has_text = Vec::with_capacity(row.cells.len());
                let mut has_explicit_bg = Vec::with_capacity(row.cells.len());
                for cell in row.cells {
                    let text_present = match cell.content {
                        vt_engine::CellContent::Empty | vt_engine::CellContent::BgOnly => false,
                        vt_engine::CellContent::Codepoint(ch) => !ch.is_whitespace() && ch != '\0',
                        vt_engine::CellContent::Grapheme { .. } => row
                            .cell_grapheme(cell)
                            .is_some_and(|s| s.chars().any(|c| !c.is_whitespace() && c != '\0')),
                    };
                    let bg_present = cell.bg.is_some_and(|bg| bg != default_bg);
                    has_text.push(text_present);
                    has_explicit_bg.push(bg_present);
                }
                rows.push(KittyRowSummary {
                    viewport_y: row.viewport_y,
                    has_text,
                    has_explicit_bg,
                });
            });
        }

        Self {
            origin_x,
            origin_y,
            cell_w,
            cell_h,
            px_scale_x,
            px_scale_y,
            grid_clip,
            scroll_region,
            viewport_active,
            rows,
        }
    }

    fn row_summary(&self, viewport_y: i32) -> Option<&KittyRowSummary> {
        self.rows.iter().find(|r| r.viewport_y == viewport_y)
    }

    /// Computes a refined clip rect for `p`, returning `None` if the placement is completely
    /// occluded by a pinned TUI region (DECSTBM) or subsequent foreground text.
    fn clip_for_placement(
        &self,
        p: &VtKittyPlacement,
        term: &Terminal,
        scroll_offset_y: f32,
    ) -> Option<ClipRect> {
        let mut clip = self.grid_clip;

        // 1. DECSTBM scroll-region clipping: if the terminal is on the live bottom screen with a
        //    sub-screen scroll region `[top..=bottom]` (e.g. `tr3x` chat rows `0..=bottom` above
        //    its pinned Reply composer), any fixed placement anchored inside `[top..=bottom]`
        //    MUST be clipped to `[top, bottom + 1)` so it can never bleed over the pinned header
        //    or composer box! Placements anchored outside `[top..=bottom]` (e.g. icons inside the
        //    composer itself) remain untouched.
        if self.viewport_active
            && p.kind == vt_engine::KittyPlacementKind::Fixed
            && let Some((scroll_top, scroll_bottom)) = self.scroll_region
        {
            let top = i32::from(scroll_top);
            let bottom = i32::from(scroll_bottom);
            if p.viewport_row >= top && p.viewport_row <= bottom {
                let min_y = self.origin_y + (top as f32) * self.cell_h + scroll_offset_y;
                let max_y = self.origin_y + ((bottom + 1) as f32) * self.cell_h + scroll_offset_y;
                clip.min_y = clip.min_y.max(min_y);
                clip.max_y = clip.max_y.min(max_y);
            }
        }

        // 2. Non-empty subsequent row protection for fixed `AboveText` (`z == 0`) inline images:
        //    Inline images (QR codes, chat media, `icat`) are placed on blank placeholder lines.
        //    If the anchor row `p.viewport_row` is blank within `[col..col+gcols)` and a later row
        //    `r > p.viewport_row` inside the placement's span contains real foreground text (or an
        //    explicit background bar when the anchor row had default background), clip the bottom
        //    of the image at row `r` so it never obscures a pinned composer or the next message
        //    (even when scrolled back into scrollback history!).
        if p.kind == vt_engine::KittyPlacementKind::Fixed
            && p.z_layer == KittyImageLayer::AboveText
            && p.z_index == 0
            && p.grid_rows >= 2
            && !self.rows.is_empty()
        {
            let col_start = p.viewport_col.max(0) as usize;
            let col_end = p
                .viewport_col
                .saturating_add(p.grid_cols.max(1) as i32)
                .max(0) as usize;
            if col_end > col_start {
                let (anchor_is_blank, anchor_has_bg) =
                    if let Some(anchor_row) = self.row_summary(p.viewport_row) {
                        (
                            !anchor_row.span_has_text(col_start, col_end),
                            anchor_row.span_has_explicit_bg(col_start, col_end),
                        )
                    } else if p.viewport_row < 0 {
                        let blank = vt_engine::viewport_row_span_is_blank(
                            term,
                            p.viewport_row,
                            col_start.min(usize::from(u16::MAX)) as u16,
                            col_end.min(usize::from(u16::MAX)) as u16,
                        )
                        .ok()
                        .flatten()
                        .unwrap_or(false);
                        (blank, false)
                    } else {
                        (false, false)
                    };

                if anchor_is_blank {
                    let start_check = p.viewport_row.saturating_add(1);
                    let end_row = p.viewport_row.saturating_add(p.grid_rows as i32);
                    for row in &self.rows {
                        if row.viewport_y < start_check || row.viewport_y >= end_row {
                            continue;
                        }
                        let collides_text = row.span_has_text(col_start, col_end);
                        let collides_bar_bg =
                            !anchor_has_bg && row.span_has_explicit_bg(col_start, col_end);
                        if collides_text || collides_bar_bg {
                            let row_top_y = self.origin_y
                                + (row.viewport_y as f32) * self.cell_h
                                + scroll_offset_y;
                            clip.max_y = clip.max_y.min(row_top_y);
                            break;
                        }
                    }
                }
            }
        }

        if clip.max_x <= clip.min_x || clip.max_y <= clip.min_y {
            None
        } else {
            Some(clip)
        }
    }
}

fn emit_kitty_placements_with_renderer(
    pane: &mut PaneRuntime,
    term: &Terminal,
    viewport: &TerminalViewport,
    renderer: &mut GpuRenderer,
    draw_list: &mut DrawList,
) -> Result<(usize, usize), PaneStoreError> {
    pane.render_state
        .kitty_placements(&mut pane.kitty_placements_scratch)?;
    if pane.kitty_placements_scratch.is_empty() {
        return Ok((0, 0));
    }

    let scale = viewport.metrics.scale_factor.max(0.5);
    let clip_ctx = KittyClipContext::build(pane, term, viewport, scale);
    let mut uploads = 0usize;
    pane.gpu_placements_scratch.clear();

    for p in &pane.kitty_placements_scratch {
        let Some(clip_rect) = clip_ctx.clip_for_placement(p, term, viewport.scroll_offset_y) else {
            continue;
        };

        let gpu_id = compose_kitty_image_id(pane.pane_uid, p.image_id);
        if !renderer.images().has_generation(gpu_id, p.generation)
            && let Some(pixels) = vt_engine::read_kitty_image(term, p.image_id)?
        {
            let uploaded = renderer.upload_image(ImageUpload {
                id: gpu_id,
                generation: pixels.generation,
                width: pixels.width,
                height: pixels.height,
                format: ImagePixelFormat::Rgba8,
                data: &pixels.rgba8,
            })?;
            if uploaded {
                uploads += 1;
            }
        }

        if let Some(cached) = renderer.images().get(gpu_id) {
            pane.gpu_placements_scratch.push(convert_kitty_placement(
                p,
                gpu_id,
                cached.width,
                cached.height,
                viewport,
                scale,
                &clip_ctx,
                clip_rect,
            ));
        }
    }

    let count = pane.gpu_placements_scratch.len();
    if count > 0 {
        ImageTextureCache::emit_placements(&mut pane.gpu_placements_scratch, draw_list);
    }
    Ok((count, uploads))
}

#[allow(clippy::too_many_arguments)]
fn emit_kitty_placements_with_parts(
    pane: &mut PaneRuntime,
    term: &Terminal,
    viewport: &TerminalViewport,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    image_cache: &mut ImageTextureCache,
    draw_list: &mut DrawList,
) -> Result<(usize, usize), PaneStoreError> {
    pane.render_state
        .kitty_placements(&mut pane.kitty_placements_scratch)?;
    if pane.kitty_placements_scratch.is_empty() {
        return Ok((0, 0));
    }

    let scale = viewport.metrics.scale_factor.max(0.5);
    let clip_ctx = KittyClipContext::build(pane, term, viewport, scale);
    let mut uploads = 0usize;
    pane.gpu_placements_scratch.clear();

    for p in &pane.kitty_placements_scratch {
        let Some(clip_rect) = clip_ctx.clip_for_placement(p, term, viewport.scroll_offset_y) else {
            continue;
        };

        let gpu_id = compose_kitty_image_id(pane.pane_uid, p.image_id);
        if !image_cache.has_generation(gpu_id, p.generation)
            && let Some(pixels) = vt_engine::read_kitty_image(term, p.image_id)?
        {
            let uploaded = image_cache.upload_or_update(
                device,
                queue,
                ImageUpload {
                    id: gpu_id,
                    generation: pixels.generation,
                    width: pixels.width,
                    height: pixels.height,
                    format: ImagePixelFormat::Rgba8,
                    data: &pixels.rgba8,
                },
            )?;
            if uploaded {
                uploads += 1;
            }
        }

        if let Some(cached) = image_cache.get(gpu_id) {
            pane.gpu_placements_scratch.push(convert_kitty_placement(
                p,
                gpu_id,
                cached.width,
                cached.height,
                viewport,
                scale,
                &clip_ctx,
                clip_rect,
            ));
        }
    }

    let count = pane.gpu_placements_scratch.len();
    if count > 0 {
        ImageTextureCache::emit_placements(&mut pane.gpu_placements_scratch, draw_list);
    }
    Ok((count, uploads))
}

#[allow(clippy::too_many_arguments)]
fn convert_kitty_placement(
    p: &VtKittyPlacement,
    gpu_image_id: u64,
    tex_width: u32,
    tex_height: u32,
    viewport: &TerminalViewport,
    scale: f32,
    clip_ctx: &KittyClipContext,
    clip_rect: ClipRect,
) -> GpuKittyPlacement {
    let x = clip_ctx.origin_x
        + (p.viewport_col as f32) * clip_ctx.cell_w
        + (p.cell_offset_x as f32) * clip_ctx.px_scale_x / scale;
    let y = clip_ctx.origin_y
        + (p.viewport_row as f32) * clip_ctx.cell_h
        + viewport.scroll_offset_y
        + (p.cell_offset_y as f32) * clip_ctx.px_scale_y / scale;

    let width = if p.pixel_width > 0 {
        (p.pixel_width as f32) * clip_ctx.px_scale_x / scale
    } else {
        (p.grid_cols.max(1) as f32) * clip_ctx.cell_w
    };
    let height = if p.pixel_height > 0 {
        (p.pixel_height as f32) * clip_ctx.px_scale_y / scale
    } else {
        (p.grid_rows.max(1) as f32) * clip_ctx.cell_h
    };

    let uv_rect =
        if p.src_rect.width > 0 && p.src_rect.height > 0 && tex_width > 0 && tex_height > 0 {
            let tw = tex_width as f32;
            let th = tex_height as f32;
            let u0 = (p.src_rect.x as f32 / tw).clamp(0.0, 1.0);
            let v0 = (p.src_rect.y as f32 / th).clamp(0.0, 1.0);
            let u1 = ((p.src_rect.x.saturating_add(p.src_rect.width)) as f32 / tw).clamp(u0, 1.0);
            let v1 = ((p.src_rect.y.saturating_add(p.src_rect.height)) as f32 / th).clamp(v0, 1.0);
            [u0, v0, u1, v1]
        } else {
            [0.0, 0.0, 1.0, 1.0]
        };

    let layer = match p.z_layer {
        KittyImageLayer::BelowBackground => ImageLayer::BelowBackground,
        KittyImageLayer::BelowText => ImageLayer::BelowText,
        KittyImageLayer::AboveText => ImageLayer::AboveText,
    };

    GpuKittyPlacement {
        image_id: gpu_image_id,
        generation: p.generation,
        layer,
        z_index: p.z_index,
        dest_rect: Rect::new(x, y, width, height),
        uv_rect,
        opacity: viewport.dim_alpha.clamp(0.0, 1.0),
        corner_radius: 0.0,
        clip_rect,
    }
}

// ============================================================================
// Tests (synthetic full-pipeline + live read-only T3 Code Vulkan PNG screenshot)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use gpu_renderer::GpuConfig;
    use omarchy_theme::Theme;
    use rex_proto::types::SessionSummary;
    use rex_proto::{DataReconnectToken, DataWelcome, RexEndpoint};
    use vt_engine::{KeyAction, Modifiers, MouseButton, PhysicalKey};

    use crate::host_manager::{
        HostCommand, HostManager, HostManagerConfig, LOCAL_HOST_ID, RuntimeEvent,
    };
    use crate::workspace::{HostEntry, SessionEntry, SessionKey};

    fn live_screenshot_path() -> PathBuf {
        std::env::var("OMAREX_LIVE_PANE_PNG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("omarex_live_t3_pane.png"))
    }

    /// Minimal valid 1x1 opaque red PNG (68 bytes) for exercising Kitty `f=100` PNG decode.
    const TEST_PNG_1X1_RED: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR length + type
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, // 8-bit RGB + CRC
        0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT length + type
        0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, // zlib data
        0x18, 0xDD, 0x8D, 0xB0, // IDAT CRC
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82, // IEND
    ];

    fn make_base64(input: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = if chunk.len() > 1 {
                u32::from(chunk[1])
            } else {
                0
            };
            let b2 = if chunk.len() > 2 {
                u32::from(chunk[2])
            } else {
                0
            };
            let triple = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(triple & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn synthetic_stream_lifecycle_render_interaction_and_eviction() {
        let theme = Theme::load_live();
        let mut workspace = WorkspaceState::new(4);
        let mut host = HostEntry::new(LOCAL_HOST_ID, "Local", RexEndpoint::discover_default());
        host.sessions.insert(
            "sess-1".into(),
            SessionEntry::new(SessionSummary {
                session_id: "sess-1".into(),
                label: "Test".into(),
            }),
        );
        workspace.add_host(host);

        let key = BlockKey::new(LOCAL_HOST_ID, "sess-1", "blk-1");
        let queued_resize = TerminalResizeArgs {
            columns: 80,
            rows: 24,
            cell_width_px: Some(10),
            cell_height_px: Some(20),
        };
        workspace.queue_resize(&key, queued_resize.clone(), Instant::now());

        // Build a realistic GHOSTSNP snapshot containing scrollback, ANSI styles, and an OSC 8 link.
        let src_term = Terminal::new(80, 24, TerminalConfig::default()).expect("src terminal");
        for i in 0..60 {
            src_term.write(format!("scrollback-line-{i:03} needle-token\r\n").as_bytes());
        }
        src_term.write(
            b"\x1b[1;32mHello Omarex!\x1b[0m \x1b]8;;https://superlogical.dev\x1b\\Docs\x1b]8;;\x1b\\\r\n",
        );
        src_term.write(b"\x1b[>1u"); // Enable Kitty keyboard protocol DISAMBIGUATE
        src_term.write(b"\x1b[?1000h\x1b[?1006h"); // Enable SGR mouse tracking
        let snapshot = src_term.encode_snapshot().expect("encode snapshot");

        let mut store = PaneStore::with_theme(&theme.palette).expect("create PaneStore");

        // 1. Opened establishes nonce and decoder.
        let opened = store
            .handle_data_stream_event(
                DataStreamEvent {
                    host_id: LOCAL_HOST_ID.into(),
                    session_id: "sess-1".into(),
                    block_id: "blk-1".into(),
                    kind: DataStreamEventKind::Opened {
                        welcome: DataWelcome {
                            reconnect_token: DataReconnectToken::from_bytes([0u8; 16]),
                            resume_ttl_ms: 30_000,
                            channel_nonce: 0xDEAD_BEEF,
                        },
                    },
                },
                &mut workspace,
            )
            .expect("Opened");
        assert!(!opened.became_ready);
        assert!(!store.is_ready(&key));

        // 2. Feed snapshot in 512-byte streaming chunks until READY fires and releases deferred resize.
        let mut saw_ready = false;
        let mut released_resize = None;
        let mut offset = 0usize;
        for chunk in snapshot.chunks(512) {
            offset += chunk.len();
            let out = store
                .handle_data_stream_event(
                    DataStreamEvent {
                        host_id: LOCAL_HOST_ID.into(),
                        session_id: "sess-1".into(),
                        block_id: "blk-1".into(),
                        kind: DataStreamEventKind::SnapshotChunk {
                            bytes: Bytes::copy_from_slice(chunk),
                            bytes_received: offset as u64,
                        },
                    },
                    &mut workspace,
                )
                .expect("SnapshotChunk");
            if out.became_ready {
                saw_ready = true;
                released_resize = out.pending_resize;
            }
        }
        assert!(saw_ready, "expected GHOSTSNP READY");
        assert_eq!(released_resize, Some(queued_resize));
        assert!(store.is_ready(&key));

        // 3. SnapshotComplete + background stepping finishes history cleanly.
        store
            .handle_data_stream_event(
                DataStreamEvent {
                    host_id: LOCAL_HOST_ID.into(),
                    session_id: "sess-1".into(),
                    block_id: "blk-1".into(),
                    kind: DataStreamEventKind::SnapshotComplete {
                        snapshot_bytes: snapshot.len() as u64,
                    },
                },
                &mut workspace,
            )
            .expect("SnapshotComplete");

        while store.has_pending_snapshots() {
            store
                .step_background_snapshots(Duration::from_millis(2), &mut workspace)
                .expect("step background");
        }

        // 3b. Apply authenticated rex1 Resize APC so pixel cell geometry is active for Kitty graphics.
        let apc_out = store
            .handle_data_stream_event(
                DataStreamEvent {
                    host_id: LOCAL_HOST_ID.into(),
                    session_id: "sess-1".into(),
                    block_id: "blk-1".into(),
                    kind: DataStreamEventKind::RexApc(RexControlMessage::Resize {
                        cols: 80,
                        rows: 24,
                        cell_width_px: 10,
                        cell_height_px: 20,
                        owner: Some("client-1".into()),
                    }),
                },
                &mut workspace,
            )
            .expect("RexApc Resize");
        assert!(apc_out.dirty);
        assert!(matches!(
            apc_out.rex_apc,
            Some(RexControlApc::Resize { .. })
        ));

        // 4. Feed LiveVtChunk containing OSC title + Kitty PNG graphic placement (`f=100,t=d,a=T`).
        let png_b64 = make_base64(TEST_PNG_1X1_RED);
        let live_seq =
            format!("\x1b]2;Omarex Unit Pane\x07\x1b_Ga=T,f=100,t=d,i=42,c=2,r=2;{png_b64}\x1b\\");
        let live_out = store
            .handle_data_stream_event(
                DataStreamEvent {
                    host_id: LOCAL_HOST_ID.into(),
                    session_id: "sess-1".into(),
                    block_id: "blk-1".into(),
                    kind: DataStreamEventKind::LiveVtChunk {
                        bytes: Bytes::from(live_seq),
                        bytes_received: (snapshot.len() + 64) as u64,
                    },
                },
                &mut workspace,
            )
            .expect("LiveVtChunk");
        assert!(live_out.dirty);
        assert!(
            live_out
                .terminal_events
                .iter()
                .any(|e| matches!(e, TerminalEvent::TitleChanged(t) if t == "Omarex Unit Pane"))
        );
        let blk_title = workspace
            .session(&SessionKey::new(LOCAL_HOST_ID, "sess-1"))
            .and_then(|s| s.blocks.get("blk-1"))
            .and_then(|b| b.title.clone());
        assert_eq!(blk_title.as_deref(), Some("Omarex Unit Pane"));

        // 5. Render via headless GpuRenderer and confirm glyphs + Kitty GPU texture upload!
        let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless GPU");
        let metrics = renderer.atlas_mut().measure_monospace_cell(14.0, 1.0);
        let vp = build_terminal_viewport(
            Rect::new(
                0.0,
                0.0,
                80.0 * metrics.cell_width,
                24.0 * metrics.cell_height,
            ),
            ClipRect::new(
                0.0,
                0.0,
                80.0 * metrics.cell_width,
                24.0 * metrics.cell_height,
            ),
            80,
            24,
            metrics,
            0.0,
            &theme.palette,
            true,
            true,
        );
        let mut draw_list = DrawList::new(960, 540, 1.0);
        let render_res = store
            .render_pane(
                &key,
                &vp,
                Overscan { above: 1, below: 1 },
                &mut renderer,
                &mut draw_list,
            )
            .expect("render_pane")
            .expect("pane was ready");
        assert!(render_res.stats.total_rows >= 24);
        assert!(render_res.stats.glyphs > 0);
        assert_eq!(render_res.kitty_placements, 1);
        assert_eq!(render_res.kitty_uploads, 1);
        assert!(!store.is_dirty(&key));

        // 6. Key encoding (Shift+Enter under Kitty flags + German AltGr '@').
        let shift_enter = store
            .encode_key(
                &key,
                &KeyEventInput {
                    action: KeyAction::Press,
                    key: PhysicalKey::Enter,
                    mods: Modifiers::SHIFT,
                    consumed_mods: Modifiers::NONE,
                    utf8: None,
                    unshifted_codepoint: Some('\r'),
                    composing: false,
                },
            )
            .expect("encode Shift+Enter");
        assert_eq!(shift_enter, b"\x1b[13;2u");

        let altgr_at = store
            .encode_key(
                &key,
                &KeyEventInput {
                    action: KeyAction::Press,
                    key: PhysicalKey::KeyQ,
                    mods: Modifiers::ALT_RIGHT,
                    consumed_mods: Modifiers::ALT_RIGHT,
                    utf8: Some("@"),
                    unshifted_codepoint: Some('q'),
                    composing: false,
                },
            )
            .expect("encode German AltGr @");
        assert_eq!(altgr_at, b"@");

        // 7. Mouse encoding (SGR normal vs Shift-bypass returning None).
        let geom = MouseGeometry {
            screen_width: 800,
            screen_height: 480,
            cell_width: 10,
            cell_height: 20,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let sgr = store
            .encode_mouse(
                &key,
                &MouseEventInput {
                    action: MouseAction::Press,
                    button: Some(MouseButton::Left),
                    mods: Modifiers::NONE,
                    x: 15.0,
                    y: 25.0,
                },
                geom,
            )
            .expect("encode SGR mouse");
        assert!(sgr.is_some());
        let bypassed = store
            .encode_mouse(
                &key,
                &MouseEventInput {
                    action: MouseAction::Press,
                    button: Some(MouseButton::Left),
                    mods: Modifiers::SHIFT,
                    x: 15.0,
                    y: 25.0,
                },
                geom,
            )
            .expect("Shift bypass");
        assert!(bypassed.is_none());

        // 8. Smooth overscan scroll fractional accumulator + line stepping.
        let frac = store
            .scroll_wheel_px(&key, 8.0, 20.0)
            .expect("fractional scroll up");
        assert!((frac - 8.0).abs() < 0.01);
        let after_step = store
            .scroll_wheel_px(&key, 35.0, 20.0)
            .expect("step scroll up");
        assert!((after_step - 3.0).abs() < 0.01);
        let sb = store.scrollbar(&key).expect("scrollbar");
        assert!(
            sb.offset + sb.len < sb.total,
            "expected scrolled up in history"
        );
        store.scroll_to_bottom(&key).expect("scroll to bottom");
        assert_eq!(store.scroll_offset_y(&key), 0.0);

        // 9. Hyperlink & selection copy & scrollback search.
        let mut found_link = None;
        for row in 0..24u32 {
            if let Ok(Some(uri)) = store.hyperlink_at(&key, 15, row) {
                found_link = Some(uri);
                break;
            }
        }
        assert_eq!(found_link.as_deref(), Some("https://superlogical.dev"));

        assert!(store.select_all(&key).expect("select_all"));
        let copied = store
            .copy_selection_text(&key, true, true)
            .expect("copy")
            .expect("some text");
        assert!(copied.contains("Hello Omarex!"));
        store.clear_selection(&key).expect("clear_selection");

        let summary = store
            .search_query(&key, "needle-token")
            .expect("search_query");
        assert_eq!(summary.total_matches, 60);
        assert!(store.search_next(&key).expect("search_next"));
        store.search_close(&key);

        // 10. Evicting the session cleans up its Kitty Graphics textures from ImageTextureCache!
        assert_eq!(renderer.images().len(), 1);
        let evicted = store.evict_session(LOCAL_HOST_ID, "sess-1", Some(renderer.images_mut()));
        assert_eq!(evicted, 1);
        assert_eq!(renderer.images().len(), 0);
    }

    #[tokio::test]
    async fn live_t3_code_pane_renders_via_pane_store_and_vulkan() {
        let endpoint = RexEndpoint::discover_default();
        if let RexEndpoint::Unix(ref path) = endpoint
            && !path.exists()
        {
            eprintln!(
                "skipping live T3 Code Vulkan render test: {} not present",
                path.display()
            );
            return;
        }

        let theme = Theme::load_live();
        let mut workspace = WorkspaceState::new(4);
        let mut pane_store = PaneStore::with_theme(&theme.palette).expect("create PaneStore");

        let cfg = HostManagerConfig {
            local_endpoint: Some(endpoint.clone()),
            client_label: Some("omarex-pane-store-test".into()),
            start_theme_watchers: false,
            start_notifier: false,
            ..HostManagerConfig::default()
        };
        let (mgr, mut rx) = HostManager::spawn(cfg);

        workspace.add_host(HostEntry::new(LOCAL_HOST_ID, "Local", endpoint));

        let mut t3_session_id: Option<String> = None;
        let mut target_key: Option<BlockKey> = None;
        let mut target_label = String::new();
        let mut opened_stream = false;
        let mut saw_snapshot_complete = false;
        let mut saw_ready = false;
        let mut post_complete_deadline: Option<tokio::time::Instant> = None;

        let pick_t3_block = |view: &rex_proto::types::SessionView| -> Option<(String, String)> {
            // Prefer the active window's focused terminal block, then any terminal block in the
            // active window, then any terminal block across the session.
            let active_win = view.active_window_id.as_deref().and_then(|wid| {
                view.windows
                    .iter()
                    .find(|w| w.window_id.as_deref() == Some(wid))
            });

            let pick_in_window = |w: &rex_proto::types::WindowView| -> Option<(String, String)> {
                if let Some(focused) = w.focused_block_id.as_deref() {
                    for l in &w.layers {
                        for b in &l.blocks {
                            if b.block_id.as_deref() == Some(focused)
                                && b.creator_name.as_deref() != Some("com.superlogical.browser")
                            {
                                return Some((focused.to_owned(), w.label.clone()));
                            }
                        }
                    }
                }
                for l in &w.layers {
                    for b in &l.blocks {
                        if let Some(bid) = b.block_id.as_deref()
                            && b.creator_name.as_deref() != Some("com.superlogical.browser")
                        {
                            return Some((bid.to_owned(), w.label.clone()));
                        }
                    }
                }
                None
            };

            if let Some(w) = active_win
                && let Some(found) = pick_in_window(w)
            {
                return Some(found);
            }
            for w in &view.windows {
                if let Some(found) = pick_in_window(w) {
                    return Some(found);
                }
            }
            None
        };

        let overall_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let wait_until = post_complete_deadline.unwrap_or(overall_deadline);
            let remaining = wait_until.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            let Ok(Some(ev)) = tokio::time::timeout(remaining, rx.recv()).await else {
                break;
            };

            match ev {
                RuntimeEvent::BootstrapReady {
                    host_id, bootstrap, ..
                } => {
                    if let Some(host) = workspace.host_mut(&host_id) {
                        for s in &bootstrap.attach_all.sessions {
                            host.sessions
                                .entry(s.session_id.clone())
                                .or_insert_with(|| SessionEntry::new(s.clone()));
                        }
                    }

                    // Locate the live "T3 Code" session first; otherwise fall back to selected_session.
                    let chosen_session = bootstrap
                        .attach_all
                        .sessions
                        .iter()
                        .find(|s| s.label == "T3 Code")
                        .or_else(|| bootstrap.selected_session.as_ref().map(|sel| &sel.session))
                        .or_else(|| bootstrap.attach_all.sessions.first());

                    if let Some(sess) = chosen_session {
                        t3_session_id = Some(sess.session_id.clone());

                        // Check if bootstrap already includes its SessionView.
                        let existing_view = bootstrap
                            .selected_session
                            .as_ref()
                            .filter(|s| s.session.session_id == sess.session_id)
                            .map(|s| &s.view)
                            .or_else(|| {
                                bootstrap
                                    .attached_sessions
                                    .iter()
                                    .find(|s| s.session.session_id == sess.session_id)
                                    .map(|s| &s.view)
                            });

                        if let Some(view) = existing_view {
                            if let Some((bid, wlabel)) = pick_t3_block(view) {
                                let ticket = bootstrap
                                    .data_tickets
                                    .iter()
                                    .find(|t| t.session_id == sess.session_id && t.block_id == bid)
                                    .map(|t| t.ticket.clone());
                                target_key = Some(BlockKey::new(
                                    host_id.clone(),
                                    sess.session_id.clone(),
                                    bid.clone(),
                                ));
                                target_label = wlabel;
                                opened_stream = true;
                                mgr.send(HostCommand::OpenBlockStream {
                                    host_id: host_id.clone(),
                                    session_id: sess.session_id.clone(),
                                    block_id: bid,
                                    ticket,
                                })
                                .expect("enqueue OpenBlockStream");
                            }
                        } else {
                            mgr.send(HostCommand::FetchView {
                                host_id: host_id.clone(),
                                session_id: sess.session_id.clone(),
                            })
                            .expect("enqueue FetchView for T3 Code");
                        }
                    }
                }

                RuntimeEvent::SessionViewUpdated {
                    host_id,
                    session_id,
                    view,
                    ..
                } => {
                    if !opened_stream
                        && t3_session_id.as_deref() == Some(session_id.as_str())
                        && let Some((bid, wlabel)) = pick_t3_block(&view)
                    {
                        target_key = Some(BlockKey::new(
                            host_id.clone(),
                            session_id.clone(),
                            bid.clone(),
                        ));
                        target_label = wlabel;
                        opened_stream = true;
                        mgr.send(HostCommand::OpenBlockStream {
                            host_id,
                            session_id,
                            block_id: bid,
                            ticket: None,
                        })
                        .expect("enqueue OpenBlockStream");
                    }
                }

                RuntimeEvent::DataStream(ds) => {
                    let is_target = target_key.as_ref().is_some_and(|k| {
                        k.host_id == ds.host_id
                            && k.session_id == ds.session_id
                            && k.block_id == ds.block_id
                    });
                    let out = pane_store
                        .handle_data_stream_event(ds, &mut workspace)
                        .expect("handle_data_stream_event");
                    if is_target {
                        if out.became_ready {
                            saw_ready = true;
                        }
                        if out.snapshot_complete {
                            saw_snapshot_complete = true;
                            // Allow 200ms for immediate post-FINISH rex1 Resize/Theme APCs.
                            post_complete_deadline =
                                Some(tokio::time::Instant::now() + Duration::from_millis(200));
                        }
                        if matches!(out.rex_apc, Some(RexControlApc::Resize { .. }))
                            && saw_ready
                            && saw_snapshot_complete
                        {
                            break;
                        }
                    }
                }

                RuntimeEvent::HostError {
                    context, message, ..
                } => {
                    panic!("unexpected host error during {context}: {message}");
                }

                _ => {}
            }
        }

        let Some(key) = target_key else {
            eprintln!("skipping live Vulkan render: no live terminal block discovered");
            mgr.shutdown();
            return;
        };

        assert!(saw_ready, "expected live T3 Code pane to reach READY");
        assert!(
            saw_snapshot_complete,
            "expected live T3 Code pane SnapshotComplete"
        );

        // Drain any remaining background scrollback/image decoding slices.
        while pane_store.has_pending_snapshots() {
            pane_store
                .step_background_snapshots(Duration::from_millis(2), &mut workspace)
                .expect("step background snapshots");
        }

        // Apply live Omarchy theme (dark/light + 16 ANSI colors) across the restored pane.
        let _ = pane_store.apply_theme_all(&theme.palette);
        let (cols, rows) = pane_store.size(&key).expect("pane terminal size");
        assert!(cols > 0 && rows > 0);

        let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("Vulkan GPU");
        let scale = 2.0f32;
        let font_size = 13.5f32;
        let pad = 16.0f32;
        let metrics = renderer
            .atlas_mut()
            .measure_monospace_cell(font_size, scale);

        let logical_w = (f32::from(cols) * metrics.cell_width + pad * 2.0).clamp(640.0, 1600.0);
        let logical_h = (f32::from(rows) * metrics.cell_height + pad * 2.0).clamp(400.0, 1080.0);
        let phys_w = (logical_w * scale).round() as u32;
        let phys_h = (logical_h * scale).round() as u32;

        let inner_rect = Rect::new(pad, pad, logical_w - pad * 2.0, logical_h - pad * 2.0);
        let viewport = build_terminal_viewport(
            inner_rect,
            inner_rect.to_clip_rect(),
            cols,
            rows,
            metrics,
            pane_store.scroll_offset_y(&key),
            &theme.palette,
            true,
            true,
        );

        let mut draw_list = DrawList::new(phys_w, phys_h, scale);
        draw_list.set_clear_color(viewport.default_bg);
        draw_list.set_light_mode(!theme.is_dark);

        let render_res = pane_store
            .render_pane(
                &key,
                &viewport,
                Overscan { above: 1, below: 1 },
                &mut renderer,
                &mut draw_list,
            )
            .expect("render live T3 Code pane")
            .expect("live pane ready");

        let out_path = live_screenshot_path();
        let frame_stats = renderer
            .render_to_png(&draw_list.finish(), &out_path)
            .expect("render_to_png");

        assert!(render_res.stats.total_rows >= usize::from(rows));
        assert!(out_path.exists(), "expected screenshot PNG on disk");
        let png_len = std::fs::metadata(&out_path)
            .expect("stat screenshot PNG")
            .len();
        assert!(png_len > 1024, "expected non-empty PNG ({png_len} bytes)");

        eprintln!(
            "live T3 Code pane rendered: window={target_label:?} block={} size={cols}x{rows} \
             rows={} glyphs={} kitty_placements={} png={} bytes adapter={:?} backend={:?} total_us={}",
            key.block_id,
            render_res.stats.total_rows,
            render_res.stats.glyphs,
            render_res.kitty_placements,
            png_len,
            frame_stats.adapter_name,
            frame_stats.backend,
            frame_stats.total_us
        );

        let _ = mgr.send(HostCommand::CloseBlockStream {
            host_id: key.host_id,
            session_id: key.session_id,
            block_id: key.block_id,
        });
        mgr.shutdown();
    }

    #[test]
    fn kitty_fixed_placement_clipped_to_decstbm_scroll_region_above_composer() {
        let theme = Theme::default();
        let mut store = PaneStore::with_theme(&theme.palette).expect("create PaneStore");
        let key = BlockKey::new(LOCAL_HOST_ID, "sess-qr", "blk-qr");
        store.create_local_test_pane(&key, 80, 24).unwrap();

        let b64 = make_base64(TEST_PNG_1X1_RED);
        // Mirror `tr3x`: DECSTBM scroll region rows 1..20, pinned Reply composer on rows 21..24,
        // and a 10-row fixed Kitty QR code anchored at row 18 (1-based row 19) inside the scroll region.
        let seq = format!(
            "\x1b[1;20r\
             \x1b[21;1H\x1b[48;2;36;36;36m  › Reply to thread...                                            \x1b[0m\
             \x1b[22;1H  AY Gemini 4 Neon · Extra · Full access\
             \x1b[19;3H\x1b_Ga=T,f=100,i=8,c=24,r=10,C=1;{b64}\x1b\\"
        );
        store.feed_local_test_bytes(&key, seq.as_bytes()).unwrap();

        let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless GPU");
        let metrics = CellMetrics {
            font_size: 14.0,
            scale_factor: 1.0,
            cell_width: 10.0,
            cell_height: 20.0,
            baseline_y: 15.0,
            underline_y: 17.0,
            underline_thickness: 1.0,
            strikethrough_y: 10.0,
        };
        let rect = Rect::new(12.0, 10.0, 800.0, 480.0);
        let viewport = build_terminal_viewport(
            rect,
            rect.to_clip_rect(),
            80,
            24,
            metrics,
            0.0,
            &theme.palette,
            true,
            false,
        );
        let mut draw_list = DrawList::new(900, 600, 1.0);
        let res = store
            .render_pane(
                &key,
                &viewport,
                Overscan::default(),
                &mut renderer,
                &mut draw_list,
            )
            .unwrap()
            .expect("ready pane");
        assert_eq!(res.kitty_placements, 1);

        let frame = draw_list.finish();
        let images: Vec<_> = frame
            .layers
            .values()
            .flat_map(|content| content.images.iter())
            .collect();
        assert_eq!(images.len(), 1);
        let max_allowed_y = rect.y + 20.0 * metrics.cell_height;
        assert!(
            images[0].clip_rect.max_y <= max_allowed_y + 0.01,
            "QR placement must be clipped at scroll_bottom (y={max_allowed_y}), got max_y={}",
            images[0].clip_rect.max_y
        );
    }

    #[test]
    fn kitty_inline_placement_clips_before_subsequent_text_but_preserves_blank_backed_images() {
        let theme = Theme::default();
        let mut store = PaneStore::with_theme(&theme.palette).expect("create PaneStore");
        let key = BlockKey::new(LOCAL_HOST_ID, "sess-scroll", "blk-scroll");
        store.create_local_test_pane(&key, 80, 24).unwrap();

        let b64 = make_base64(TEST_PNG_1X1_RED);
        // Image 1 at row 1 (1-based row 2): 6 rows tall with 7 blank rows beneath -> UNCLIPPED.
        // Image 2 at row 9 (1-based row 10): 6 rows tall, but text arrives on row 11 -> CLIPPED at row 11!
        let seq = format!(
            "\x1b[2;3H\x1b_Ga=T,f=100,i=1,c=12,r=6,C=1;{b64}\x1b\\\
             \x1b[10;3H\x1b_Ga=T,f=100,i=2,c=12,r=6,C=1;{b64}\x1b\\\
             \x1b[12;3H› Omarchy next message"
        );
        store.feed_local_test_bytes(&key, seq.as_bytes()).unwrap();

        let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless GPU");
        let metrics = CellMetrics {
            font_size: 14.0,
            scale_factor: 1.0,
            cell_width: 10.0,
            cell_height: 20.0,
            baseline_y: 15.0,
            underline_y: 17.0,
            underline_thickness: 1.0,
            strikethrough_y: 10.0,
        };
        let rect = Rect::new(0.0, 0.0, 800.0, 480.0);
        let viewport = build_terminal_viewport(
            rect,
            rect.to_clip_rect(),
            80,
            24,
            metrics,
            0.0,
            &theme.palette,
            true,
            false,
        );
        let mut draw_list = DrawList::new(800, 480, 1.0);
        let res = store
            .render_pane(
                &key,
                &viewport,
                Overscan::default(),
                &mut renderer,
                &mut draw_list,
            )
            .unwrap()
            .expect("ready pane");
        assert_eq!(res.kitty_placements, 2);

        let frame = draw_list.finish();
        let mut images: Vec<_> = frame
            .layers
            .values()
            .flat_map(|content| content.images.iter())
            .collect();
        images.sort_by(|a, b| a.bounds.y.total_cmp(&b.bounds.y));
        assert_eq!(images.len(), 2);

        // Image 1 (row 1..7 -> y=20..140) must NOT be clipped prematurely.
        assert!(
            images[0].clip_rect.max_y >= 140.0 - 0.01,
            "blank-backed image #1 should remain unclipped, got max_y={}",
            images[0].clip_rect.max_y
        );
        // Image 2 (row 9..15 -> y=180..300) collides with text at row 11 (y=220) -> clipped at 220!
        assert!(
            images[1].clip_rect.max_y <= 220.0 + 0.01,
            "colliding image #2 must clip above row 11 (y=220), got max_y={}",
            images[1].clip_rect.max_y
        );
    }

    #[test]
    fn kitty_placement_scales_across_different_client_cell_pixel_sizes() {
        let theme = Theme::default();
        let mut store = PaneStore::with_theme(&theme.palette).expect("create PaneStore");
        let key = BlockKey::new(LOCAL_HOST_ID, "sess-dpi", "blk-dpi");
        store.create_local_test_pane(&key, 80, 24).unwrap();

        // Simulate a 2x Retina Mac owning the terminal (`cell_w=20px, cell_h=40px`).
        if let Some(pane) = store.panes.get(&key)
            && let Some(term) = pane.terminal.as_ref()
        {
            term.resize(80, 24, 20, 40).unwrap();
        }
        let b64 = make_base64(TEST_PNG_1X1_RED);
        let seq = format!("\x1b[2;3H\x1b_Ga=T,f=100,i=7,c=10,r=8,C=1;{b64}\x1b\\");
        store.feed_local_test_bytes(&key, seq.as_bytes()).unwrap();

        // Render on a 1x Omarchy display (`cell_w=10px, cell_h=20px, scale=1.0`).
        let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless GPU");
        let metrics = CellMetrics {
            font_size: 14.0,
            scale_factor: 1.0,
            cell_width: 10.0,
            cell_height: 20.0,
            baseline_y: 15.0,
            underline_y: 17.0,
            underline_thickness: 1.0,
            strikethrough_y: 10.0,
        };
        let rect = Rect::new(0.0, 0.0, 800.0, 480.0);
        let viewport = build_terminal_viewport(
            rect,
            rect.to_clip_rect(),
            80,
            24,
            metrics,
            0.0,
            &theme.palette,
            true,
            false,
        );
        let mut draw_list = DrawList::new(800, 480, 1.0);
        store
            .render_pane(
                &key,
                &viewport,
                Overscan::default(),
                &mut renderer,
                &mut draw_list,
            )
            .unwrap()
            .expect("ready pane");

        let frame = draw_list.finish();
        let img = frame
            .layers
            .values()
            .flat_map(|content| content.images.iter())
            .next()
            .expect("expected scaled Kitty image quad");
        // 8 rows * 20px = 160px (NOT 8 * 40px = 320px!)
        assert!(
            (img.bounds.height - 160.0).abs() <= 2.0,
            "expected Retina-to-1x scaled height ~160px, got {}",
            img.bounds.height
        );
        assert!(
            (img.bounds.width - 100.0).abs() <= 2.0,
            "expected Retina-to-1x scaled width ~100px, got {}",
            img.bounds.width
        );
    }
}
