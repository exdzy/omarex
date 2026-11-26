//! Backend-agnostic GPU UI draw list, reverse-order pointer hit-testing, `FocusTarget` guard,
//! and typed `UiAction` output emitted by `ui-chrome`.

use std::ops::Range;

use omarchy_theme::{Rgba, UiTokens};
use tr3x_bridge::prompts::ClientReplyPayload;

use crate::icons::IconId;
use crate::layout::{
    FloatingHitZone, PaneDensity, PaneHeaderMode, RectF, ToolbarBehavior, ToolbarPosition,
};

/// Typography family for UI chrome primitives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontKind {
    #[default]
    Sans,
    Mono,
}

/// Font weight for UI text runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontWeight {
    #[default]
    Regular,
    Medium,
    SemiBold,
    Bold,
}

/// Horizontal text alignment within `UiPrimitive::Text::rect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Soft drop-shadow parameters for elevated surfaces (modals, floating panes, context menus, toasts).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowSpec {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur_radius: f32,
    pub spread: f32,
    pub color: Rgba,
}

/// Backend-agnostic UI draw primitive translated into `gpu-renderer::DrawList` by `omarex`.
#[derive(Debug, Clone, PartialEq)]
pub enum UiPrimitive {
    RoundedRect {
        rect: RectF,
        fill: Rgba,
        border_color: Option<Rgba>,
        border_width: f32,
        corner_radius: f32,
        shadow: Option<ShadowSpec>,
    },
    Text {
        rect: RectF,
        text: String,
        color: Rgba,
        font: FontKind,
        size_px: f32,
        weight: FontWeight,
        align: TextAlign,
        ellipsis: bool,
        /// Optional UTF-8 byte ranges highlighted (e.g. fuzzy match characters in palette).
        highlight_ranges: Vec<Range<usize>>,
        highlight_color: Option<Rgba>,
    },
    Icon {
        rect: RectF,
        icon: IconId,
        tint: Rgba,
    },
    ProgressBar {
        rect: RectF,
        /// Progress fraction clamped to `0.0..=1.0`.
        fraction: f32,
        fill: Rgba,
        track: Rgba,
        corner_radius: f32,
    },
    PushClip {
        rect: RectF,
    },
    PopClip,
}

/// Which of the 5 Rex pane header buttons was clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneHeaderButton {
    SplitRight,
    SplitDown,
    FloatOrDock,
    Zoom,
    Close,
}

/// Modal flavor currently owning keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKind {
    CommandPalette,
    SwitchSession,
    GoToAnything,
    ActivityPicker,
    Settings,
    Rename,
    AddHost,
    ShortcutsHelp,
    RexPrompt,
}

/// Strict keyboard focus owner preventing background `session_view_changed` events from
/// stealing focus away from active modals, palettes, find bars, context menus, or the docked viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusTarget {
    Terminal(Option<String>),
    Viewer,
    Modal(ModalKind),
    FindBar,
    SidebarFilter,
    ContextMenu,
}

impl Default for FocusTarget {
    fn default() -> Self {
        Self::Terminal(None)
    }
}

/// Focus guard maintaining the active `FocusTarget` and the terminal pane to restore when
/// overlays close.
#[derive(Debug, Clone, Default)]
pub struct FocusGuard {
    current: FocusTarget,
    last_terminal_block: Option<String>,
}

impl FocusGuard {
    #[must_use]
    pub fn new(initial_block: Option<String>) -> Self {
        Self {
            current: FocusTarget::Terminal(initial_block.clone()),
            last_terminal_block: initial_block,
        }
    }

    #[must_use]
    pub fn current(&self) -> &FocusTarget {
        &self.current
    }

    #[must_use]
    pub fn last_terminal_block(&self) -> Option<&str> {
        self.last_terminal_block.as_deref()
    }

    /// Returns `true` when keyboard input must stay captured by UI chrome rather than the PTY.
    #[must_use]
    pub fn is_overlay_focused(&self) -> bool {
        !matches!(self.current, FocusTarget::Terminal(_))
    }

    /// Updates terminal focus from a background `session_view_changed` event.
    ///
    /// **Invariant:** Never steals focus away from `Modal`, `FindBar`, `SidebarFilter`, or
    /// `ContextMenu`. Only remembers `new_active_block` so dismissing the overlay returns there.
    pub fn on_session_view_changed(&mut self, new_active_block: Option<String>) {
        self.last_terminal_block = new_active_block.clone();
        if matches!(self.current, FocusTarget::Terminal(_)) {
            self.current = FocusTarget::Terminal(new_active_block);
        }
    }

    /// Explicitly focuses a terminal block (e.g., user clicked a pane surface).
    pub fn focus_terminal(&mut self, block_id: Option<String>) {
        self.last_terminal_block = block_id.clone();
        self.current = FocusTarget::Terminal(block_id);
    }

    pub fn focus_modal(&mut self, kind: ModalKind) {
        self.current = FocusTarget::Modal(kind);
    }

    pub fn focus_find_bar(&mut self) {
        self.current = FocusTarget::FindBar;
    }

    pub fn focus_sidebar_filter(&mut self) {
        self.current = FocusTarget::SidebarFilter;
    }

    pub fn focus_context_menu(&mut self) {
        self.current = FocusTarget::ContextMenu;
    }

    /// Transfers keyboard focus to the docked right-hand Viewer Pane.
    pub fn focus_viewer(&mut self) {
        self.current = FocusTarget::Viewer;
    }

    /// Restores keyboard focus to the last active terminal block.
    pub fn restore_terminal(&mut self) {
        self.current = FocusTarget::Terminal(self.last_terminal_block.clone());
    }
}

/// Interactive region recorded during chrome rendering for reverse-order pointer hit-testing.
#[derive(Debug, Clone, PartialEq)]
pub enum HitTarget {
    SidebarToggle,
    SidebarResizeHandle,
    ToolbarHotEdge,
    ActivityToggle,
    ActivitySortToggle,
    SidebarFilterToggle,
    SidebarFilterInput,
    NewSessionButton,
    HostsButton,
    SettingsButton,
    HostHeader {
        host_id: String,
    },
    HostNewSession {
        host_id: String,
    },
    SessionChevron {
        host_id: String,
        session_id: String,
    },
    SessionRow {
        host_id: String,
        session_id: String,
    },
    SessionMuteBell {
        host_id: String,
        session_id: String,
    },
    SessionNewTab {
        host_id: String,
        session_id: String,
    },
    SessionNewT3Thread {
        host_id: String,
        session_id: String,
    },
    TabCard {
        host_id: String,
        session_id: String,
        window_id: String,
    },
    TabClose {
        host_id: String,
        session_id: String,
        window_id: String,
    },
    TabNew,
    SessionSwitcherPill,
    CommandPaletteButton,
    PaneSurface {
        block_id: String,
    },
    PaneHeader {
        block_id: String,
    },
    PaneButton {
        block_id: String,
        button: PaneHeaderButton,
    },
    PaneZoomBadge {
        block_id: String,
    },
    PaneScrollbarTrack {
        block_id: String,
    },
    PaneScrollbarThumb {
        block_id: String,
    },
    SplitDivider {
        split_path: Vec<bool>,
    },
    FloatingHandle {
        block_id: String,
        zone: FloatingHitZone,
    },
    MinimizedCapsule {
        block_id: String,
    },
    MinimizedCapsuleClose {
        block_id: String,
    },
    EmptyStateNewTab,
    EmptyStateNewTerminal,
    ModalBackdrop,
    ModalInput,
    ModalRow {
        index: usize,
    },
    ModalCheckbox {
        index: usize,
    },
    ModalOtherRow,
    ModalConfirmButton,
    ModalCancelButton,
    SettingsToggle {
        key: String,
    },
    FindBarInput,
    FindBarPrev,
    FindBarNext,
    FindBarCaseToggle,
    FindBarRegexToggle,
    FindBarClose,
    ToastBody {
        id: u64,
    },
    ToastDismiss {
        id: u64,
    },
    ContextMenuBackdrop,
    ContextMenuItem {
        index: usize,
    },
    // Docked right-hand Viewer Pane interactions
    ViewerDivider,
    ViewerSurface,
    ViewerTab {
        tab_id: u64,
    },
    ViewerTabClose {
        tab_id: u64,
    },
    ViewerCopyPath,
    ViewerOpenExternal,
    ViewerMaximizeToggle,
    ViewerClose,
    ViewerWrapToggle,
    ViewerRawToggle,
    ViewerZoomOut,
    ViewerZoomReset,
    ViewerZoomIn,
    ViewerZoomFit,
    ViewerCanvas,
    ViewerVideoPlayPause,
    ViewerVideoSeekBar,
    ViewerVideoMuteToggle,
    ViewerVideoLoopToggle,
    ViewerScrollbarTrack,
    ViewerScrollbarThumb,
}

impl HitTarget {
    /// Returns `true` when this target should display a pointing-hand cursor.
    #[must_use]
    pub fn is_pointer_clickable(&self) -> bool {
        matches!(
            self,
            Self::SidebarToggle
                | Self::ActivityToggle
                | Self::ActivitySortToggle
                | Self::SidebarFilterToggle
                | Self::NewSessionButton
                | Self::HostsButton
                | Self::SettingsButton
                | Self::HostHeader { .. }
                | Self::HostNewSession { .. }
                | Self::SessionChevron { .. }
                | Self::SessionRow { .. }
                | Self::SessionMuteBell { .. }
                | Self::SessionNewTab { .. }
                | Self::SessionNewT3Thread { .. }
                | Self::TabCard { .. }
                | Self::TabClose { .. }
                | Self::TabNew
                | Self::SessionSwitcherPill
                | Self::CommandPaletteButton
                | Self::PaneButton { .. }
                | Self::PaneZoomBadge { .. }
                | Self::MinimizedCapsule { .. }
                | Self::MinimizedCapsuleClose { .. }
                | Self::EmptyStateNewTab
                | Self::EmptyStateNewTerminal
                | Self::ModalRow { .. }
                | Self::ModalCheckbox { .. }
                | Self::ModalOtherRow
                | Self::ModalConfirmButton
                | Self::ModalCancelButton
                | Self::SettingsToggle { .. }
                | Self::FindBarPrev
                | Self::FindBarNext
                | Self::FindBarCaseToggle
                | Self::FindBarRegexToggle
                | Self::FindBarClose
                | Self::ToastBody { .. }
                | Self::ToastDismiss { .. }
                | Self::ContextMenuItem { .. }
                | Self::ViewerTab { .. }
                | Self::ViewerTabClose { .. }
                | Self::ViewerCopyPath
                | Self::ViewerOpenExternal
                | Self::ViewerMaximizeToggle
                | Self::ViewerClose
                | Self::ViewerWrapToggle
                | Self::ViewerRawToggle
                | Self::ViewerZoomOut
                | Self::ViewerZoomReset
                | Self::ViewerZoomIn
                | Self::ViewerZoomFit
                | Self::ViewerVideoPlayPause
                | Self::ViewerVideoSeekBar
                | Self::ViewerVideoMuteToggle
                | Self::ViewerVideoLoopToggle
        )
    }

    /// Returns a concise tooltip label for icon-only buttons and controls so nothing is cryptic.
    #[must_use]
    pub fn tooltip_text(&self) -> Option<&'static str> {
        match self {
            Self::SidebarToggle => Some("Toggle Sidebar (Ctrl+Shift+B)"),
            Self::ActivityToggle => Some("Activity Filter (Ctrl+Shift+.)"),
            Self::ActivitySortToggle => Some("Switch Activity Grouping"),
            Self::SidebarFilterToggle => Some("Filter Tabs (Ctrl+Alt+F)"),
            Self::TabNew => Some("New Terminal (Ctrl+Shift+T)"),
            Self::NewSessionButton => Some("New Session (Ctrl+Shift+N)"),
            Self::HostsButton => Some("Remote Hosts"),
            Self::SettingsButton => Some("Settings (Ctrl+Shift+,)"),
            Self::CommandPaletteButton => Some("Command Palette (Ctrl+Shift+P)"),
            Self::HostNewSession { .. } => Some("New Session on Host"),
            Self::SessionChevron { .. } => Some("Collapse / Expand Session"),
            Self::SessionNewTab { .. } => Some("New Terminal Tab"),
            Self::SessionNewT3Thread { .. } => Some("New T3 Thread (Ctrl+Alt+N)"),
            Self::SessionMuteBell { .. } => Some("Mute / Unmute Session"),
            Self::TabClose { .. } => Some("Close Tab"),
            Self::PaneButton { button, .. } => Some(match button {
                PaneHeaderButton::SplitRight => "Split Right (Ctrl+Shift+D)",
                PaneHeaderButton::SplitDown => "Split Down (Ctrl+Shift+E)",
                PaneHeaderButton::FloatOrDock => "Float / Dock Pane (Ctrl+Alt+Enter)",
                PaneHeaderButton::Zoom => "Zoom Pane (Ctrl+Shift+Z)",
                PaneHeaderButton::Close => "Close Pane (Ctrl+Shift+W)",
            }),
            Self::PaneZoomBadge { .. } => Some("Unzoom Pane (Ctrl+Shift+Z)"),
            Self::FindBarCaseToggle => Some("Match Case"),
            Self::FindBarRegexToggle => Some("Use Regular Expression"),
            Self::FindBarPrev => Some("Previous Match"),
            Self::FindBarNext => Some("Next Match"),
            Self::FindBarClose => Some("Close Find (Esc)"),
            Self::MinimizedCapsuleClose { .. } => Some("Close Floating Pane"),
            Self::ToastDismiss { .. } => Some("Dismiss"),
            Self::ViewerTabClose { .. } => Some("Close Viewer Tab"),
            Self::ViewerCopyPath => Some("Copy File Path"),
            Self::ViewerOpenExternal => Some("Open in External App"),
            Self::ViewerMaximizeToggle => Some("Maximize / Restore Viewer"),
            Self::ViewerClose => Some("Close Viewer Pane"),
            Self::ViewerWrapToggle => Some("Toggle Word Wrap"),
            Self::ViewerRawToggle => Some("Toggle Raw / Rendered Markdown"),
            Self::ViewerZoomOut => Some("Zoom Out (-)"),
            Self::ViewerZoomReset => Some("Actual Size (100%)"),
            Self::ViewerZoomIn => Some("Zoom In (+)"),
            Self::ViewerZoomFit => Some("Fit to Viewport"),
            Self::ViewerVideoPlayPause => Some("Play / Pause (Space)"),
            Self::ViewerVideoMuteToggle => Some("Mute / Unmute (M)"),
            Self::ViewerVideoLoopToggle => Some("Toggle Loop (L)"),
            _ => None,
        }
    }
}

/// Borrowed pointer hover/pressed state threaded through chrome renderers for tactile feedback.
#[derive(Debug, Clone, Copy, Default)]
pub struct UiInteraction<'a> {
    pub hovered: Option<&'a HitTarget>,
    pub pressed: Option<&'a HitTarget>,
}

impl<'a> UiInteraction<'a> {
    #[must_use]
    pub fn is_hovered(&self, target: &HitTarget) -> bool {
        self.hovered == Some(target)
    }

    #[must_use]
    pub fn is_pressed(&self, target: &HitTarget) -> bool {
        self.pressed == Some(target)
    }

    /// Computes consistent `(fill, border)` chrome styling for standard icon/pill buttons.
    #[must_use]
    pub fn button_surface(
        &self,
        target: &HitTarget,
        active: bool,
        tokens: &UiTokens,
    ) -> Option<(Rgba, Option<Rgba>)> {
        if self.is_pressed(target) {
            Some((tokens.surface_row_selected, Some(tokens.border_subtle)))
        } else if active {
            Some((tokens.surface_row_active, Some(tokens.border_subtle)))
        } else if self.is_hovered(target) {
            Some((tokens.surface_row_hover, Some(tokens.border_hairline)))
        } else {
            None
        }
    }

    /// Computes `(fill, border, icon_tint)` for destructive/close icon buttons (`×`).
    #[must_use]
    pub fn danger_button_style(
        &self,
        target: &HitTarget,
        idle_tint: Rgba,
        tokens: &UiTokens,
    ) -> (Option<(Rgba, Option<Rgba>)>, Rgba) {
        if self.is_pressed(target) {
            (
                Some((
                    tokens.status_error.with_alpha_f32(0.28),
                    Some(tokens.status_error.with_alpha_f32(0.55)),
                )),
                tokens.status_error,
            )
        } else if self.is_hovered(target) {
            (
                Some((
                    tokens.status_error.with_alpha_f32(0.16),
                    Some(tokens.status_error.with_alpha_f32(0.35)),
                )),
                tokens.status_error,
            )
        } else {
            (None, idle_tint)
        }
    }
}

/// Z-order layers ensuring modals and context menus always hit-test above panes and chrome.
pub mod z_layer {
    pub const BASE: u16 = 0;
    pub const PANE_CHROME: u16 = 10;
    pub const SIDEBAR: u16 = 20;
    pub const FLOATING_PANE: u16 = 30;
    pub const CAPSULE: u16 = 35;
    pub const OVERLAY_TOOLBAR: u16 = 40;
    pub const FIND_BAR: u16 = 50;
    pub const WHICH_KEY: u16 = 55;
    pub const TOAST: u16 = 60;
    pub const HOVER_TOOLTIP: u16 = 65;
    pub const MODAL_BACKDROP: u16 = 70;
    pub const MODAL: u16 = 75;
    pub const CONTEXT_MENU_BACKDROP: u16 = 80;
    pub const CONTEXT_MENU: u16 = 85;
}

/// Positioned hit target entry in [`UiDrawList`].
#[derive(Debug, Clone, PartialEq)]
pub struct HitEntry {
    pub rect: RectF,
    pub target: HitTarget,
    pub z_order: u16,
}

/// Complete single-frame output from `ui-chrome`: primitives to paint + hit regions for input.
#[derive(Debug, Clone, Default)]
pub struct UiDrawList {
    pub primitives: Vec<UiPrimitive>,
    pub hits: Vec<HitEntry>,
}

impl UiDrawList {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.primitives.clear();
        self.hits.clear();
    }

    pub fn push_rect(
        &mut self,
        rect: RectF,
        fill: Rgba,
        border_color: Option<Rgba>,
        border_width: f32,
        corner_radius: f32,
        shadow: Option<ShadowSpec>,
    ) {
        if rect.is_empty() {
            return;
        }
        self.primitives.push(UiPrimitive::RoundedRect {
            rect,
            fill,
            border_color,
            border_width: border_width.max(0.0),
            corner_radius: corner_radius.max(0.0),
            shadow,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push_text(
        &mut self,
        rect: RectF,
        text: impl Into<String>,
        color: Rgba,
        font: FontKind,
        size_px: f32,
        weight: FontWeight,
        align: TextAlign,
        ellipsis: bool,
    ) {
        let text = text.into();
        if text.is_empty() || rect.is_empty() {
            return;
        }
        self.primitives.push(UiPrimitive::Text {
            rect,
            text,
            color,
            font,
            size_px: size_px.max(1.0),
            weight,
            align,
            ellipsis,
            highlight_ranges: Vec::new(),
            highlight_color: None,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push_highlighted_text(
        &mut self,
        rect: RectF,
        text: impl Into<String>,
        color: Rgba,
        font: FontKind,
        size_px: f32,
        weight: FontWeight,
        align: TextAlign,
        ellipsis: bool,
        highlight_ranges: Vec<Range<usize>>,
        highlight_color: Rgba,
    ) {
        let text = text.into();
        if text.is_empty() || rect.is_empty() {
            return;
        }
        self.primitives.push(UiPrimitive::Text {
            rect,
            text,
            color,
            font,
            size_px: size_px.max(1.0),
            weight,
            align,
            ellipsis,
            highlight_ranges,
            highlight_color: Some(highlight_color),
        });
    }

    pub fn push_icon(&mut self, rect: RectF, icon: IconId, tint: Rgba) {
        if rect.is_empty() {
            return;
        }
        self.primitives.push(UiPrimitive::Icon { rect, icon, tint });
    }

    pub fn push_progress(
        &mut self,
        rect: RectF,
        fraction: f32,
        fill: Rgba,
        track: Rgba,
        corner_radius: f32,
    ) {
        if rect.is_empty() {
            return;
        }
        self.primitives.push(UiPrimitive::ProgressBar {
            rect,
            fraction: fraction.clamp(0.0, 1.0),
            fill,
            track,
            corner_radius: corner_radius.max(0.0),
        });
    }

    pub fn push_clip(&mut self, rect: RectF) {
        self.primitives.push(UiPrimitive::PushClip { rect });
    }

    pub fn pop_clip(&mut self) {
        self.primitives.push(UiPrimitive::PopClip);
    }

    pub fn push_hit(&mut self, rect: RectF, target: HitTarget, z_order: u16) {
        if rect.is_empty() {
            return;
        }
        self.hits.push(HitEntry {
            rect,
            target,
            z_order,
        });
    }

    /// Reverse-order pointer hit test returning the highest `(z_order, insertion_order)` entry
    /// whose rectangle contains `(px, py)`.
    #[must_use]
    pub fn hit_test_entry(&self, px: f32, py: f32) -> Option<&HitEntry> {
        let mut best: Option<(u16, usize, &HitEntry)> = None;
        for (idx, entry) in self.hits.iter().enumerate() {
            if !entry.rect.contains(px, py) {
                continue;
            }
            let replace = match best {
                None => true,
                Some((best_z, best_idx, _)) => {
                    entry.z_order > best_z || (entry.z_order == best_z && idx > best_idx)
                }
            };
            if replace {
                best = Some((entry.z_order, idx, entry));
            }
        }
        best.map(|(_, _, entry)| entry)
    }

    /// Reverse-order pointer hit test returning the highest `(z_order, insertion_order)` target
    /// whose rectangle contains `(px, py)`.
    #[must_use]
    pub fn hit_test(&self, px: f32, py: f32) -> Option<&HitTarget> {
        self.hit_test_entry(px, py).map(|entry| &entry.target)
    }

    /// Verifies that every primitive & hit rect has finite non-negative geometry and balanced clips.
    pub fn validate_invariants(&self) -> Result<(), String> {
        let mut clip_depth: i32 = 0;
        for (idx, prim) in self.primitives.iter().enumerate() {
            match prim {
                UiPrimitive::RoundedRect {
                    rect,
                    border_width,
                    corner_radius,
                    ..
                } => {
                    validate_rect(rect, idx)?;
                    if !border_width.is_finite() || *border_width < 0.0 {
                        return Err(format!(
                            "primitive {idx}: invalid border_width {border_width}"
                        ));
                    }
                    if !corner_radius.is_finite() || *corner_radius < 0.0 {
                        return Err(format!(
                            "primitive {idx}: invalid corner_radius {corner_radius}"
                        ));
                    }
                }
                UiPrimitive::Text { rect, size_px, .. } => {
                    validate_rect(rect, idx)?;
                    if !size_px.is_finite() || *size_px <= 0.0 {
                        return Err(format!("primitive {idx}: invalid size_px {size_px}"));
                    }
                }
                UiPrimitive::Icon { rect, .. } => validate_rect(rect, idx)?,
                UiPrimitive::ProgressBar { rect, fraction, .. } => {
                    validate_rect(rect, idx)?;
                    if !fraction.is_finite() || !(0.0..=1.0).contains(fraction) {
                        return Err(format!("primitive {idx}: invalid progress {fraction}"));
                    }
                }
                UiPrimitive::PushClip { rect } => {
                    validate_rect(rect, idx)?;
                    clip_depth += 1;
                }
                UiPrimitive::PopClip => {
                    clip_depth -= 1;
                    if clip_depth < 0 {
                        return Err(format!("primitive {idx}: unmatched PopClip"));
                    }
                }
            }
        }
        if clip_depth != 0 {
            return Err(format!("unbalanced PushClip/PopClip (depth={clip_depth})"));
        }
        for (idx, hit) in self.hits.iter().enumerate() {
            validate_rect(&hit.rect, idx)?;
        }
        Ok(())
    }
}

fn validate_rect(rect: &RectF, idx: usize) -> Result<(), String> {
    if !rect.x.is_finite()
        || !rect.y.is_finite()
        || !rect.w.is_finite()
        || !rect.h.is_finite()
        || rect.w < 0.0
        || rect.h < 0.0
    {
        return Err(format!("entry {idx}: invalid rect {rect:?}"));
    }
    Ok(())
}

/// Caps a desired UI corner radius to Hyprland's live `theme.rounding` when sharp corners (`0`)
/// are preferred or when `theme.rounding > 0` provides a smaller cap.
#[must_use]
pub fn cap_radius(desired: f32, theme_rounding: f32) -> f32 {
    if theme_rounding <= 0.0 {
        0.0
    } else {
        desired.min(theme_rounding).max(0.0)
    }
}

/// Settings mutation emitted by the Settings modal.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingChange {
    ToolbarPosition(ToolbarPosition),
    ToolbarBehavior(ToolbarBehavior),
    PaneDensity(PaneDensity),
    PaneHeaderMode(PaneHeaderMode),
    ToggleTranslucentMaterial,
    ToggleActivePaneBorder,
    ToggleFadeInactivePanes,
    ToggleAdaptiveRounding,
}

/// Typed high-level UI action returned to `omarex` on pointer/keyboard interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    None,
    ToggleSidebar,
    ToggleActivityMode,
    ToggleActivitySort,
    ToggleHostCollapse {
        host_id: String,
    },
    ToggleSessionCollapse {
        host_id: String,
        session_id: String,
    },
    SelectSession {
        host_id: String,
        session_id: String,
    },
    ToggleSessionMute {
        host_id: String,
        session_id: String,
    },
    NewSession {
        host_id: Option<String>,
    },
    /// Opens a real interactive Rex/Ghostty shell terminal (creating or targeting a normal
    /// terminal session instead of hijacking into a `T3 Code` thread).
    NewTerminal,
    /// Starts a new T3 Code thread on a specific `(host_id, session_id)` rather than
    /// falling back to whichever host/session happens to be active.
    NewT3Thread {
        host_id: String,
        session_id: String,
    },
    /// Opens the Edit/Rename Host modal targeting a specific host.
    StartRenameHost {
        host_id: String,
    },
    /// Disconnects a saved remote host without removing it from saved hosts.
    DisconnectHost {
        host_id: String,
    },
    /// Forces a connect/reconnect on a saved remote host.
    ReconnectHost {
        host_id: String,
    },
    /// Disconnects and removes a saved remote host.
    RemoveHost {
        host_id: String,
    },
    SelectTab {
        host_id: String,
        session_id: String,
        window_id: String,
    },
    CloseTab {
        host_id: String,
        session_id: String,
        window_id: String,
    },
    NewTab {
        host_id: Option<String>,
        session_id: Option<String>,
    },
    FocusPane {
        block_id: String,
    },
    PaneButton {
        block_id: String,
        button: PaneHeaderButton,
    },
    UnzoomPane {
        block_id: String,
    },
    RestoreMinimizedFloating {
        block_id: String,
    },
    CloseMinimizedFloating {
        block_id: String,
    },
    BeginSidebarResize,
    BeginSplitDividerDrag {
        split_path: Vec<bool>,
    },
    BeginFloatingDrag {
        block_id: String,
        zone: FloatingHitZone,
    },
    OpenModal(ModalKind),
    CloseModal,
    RunCommand {
        action_id: String,
    },
    ReplyRexPrompt(ClientReplyPayload),
    SubmitRename {
        target_id: String,
        new_label: String,
    },
    SubmitAddHost {
        endpoint: String,
    },
    UpdateSetting(SettingChange),
    FindQueryChanged(String),
    FindNext,
    FindPrev,
    FindToggleCase,
    FindToggleRegex,
    CloseFindBar,
    ActivateToast {
        id: u64,
    },
    DismissToast {
        id: u64,
    },
    CopySelection {
        block_id: Option<String>,
    },
    PasteClipboard {
        block_id: Option<String>,
    },
    CloseOtherTabs {
        session_id: String,
        keep_window_id: String,
    },
    MarkTabUnread {
        window_id: String,
    },
    MoveTabToNewWindow {
        window_id: String,
    },
    CloseSession {
        session_id: String,
    },
    CollapseOtherSessions {
        keep_session_id: String,
    },
    ScrollPaneToFraction {
        block_id: String,
        fraction: f32,
    },
    Viewer(ViewerAction),
}

/// High-level actions emitted by the docked right-hand Viewer Pane.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewerAction {
    Focus,
    BeginDividerDrag,
    SelectTab {
        tab_id: u64,
    },
    CloseTab {
        tab_id: u64,
    },
    CopyPath {
        path: String,
    },
    OpenExternal {
        path: String,
        remote_host: Option<String>,
    },
    ToggleMaximize,
    ClosePane,
    ToggleWrap,
    ToggleRawMarkdown,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ZoomFit,
    BeginPan,
    ToggleVideoPlayPause {
        tab_id: u64,
    },
    SeekVideo {
        tab_id: u64,
        fraction: f32,
    },
    ToggleVideoMute {
        tab_id: u64,
    },
    ToggleVideoLoop {
        tab_id: u64,
    },
    ScrollToFraction {
        tab_id: u64,
        fraction: f32,
    },
}
