//! Rex-faithful GPU UI chrome (`taffy` layout, embedded vector icons, UTF-8 text input,
//! zero-regex file-path hover detector, `nucleo-matcher` fuzzy palette, multi-host/tr3x sidebar,
//! horizontal tab bar, inset pane chrome, unified modals + `rex.ui` prompts, context menus,
//! and `/pr` floating drawer styling).

pub mod anim;
pub mod draw;
pub mod drawer;
pub mod file_paths;
pub mod icons;
pub mod layout;
pub mod modal;
pub mod palette;
pub mod pane_chrome;
pub mod sidebar;
pub mod syntax;
pub mod tabbar;
pub mod text_input;
pub mod viewer;

use std::time::Instant;

use omarchy_theme::Theme;
use tr3x_bridge::prompts::PromptSession;

pub use anim::{Spring1D, SpringRect};
pub use draw::{
    FocusGuard, FocusTarget, FontKind, FontWeight, HitEntry, HitTarget, ModalKind,
    PaneHeaderButton, SettingChange, SettingsCategory, ShadowSpec, TabReorderOverlay, TextAlign,
    TextSpan, UiAction, UiDrawList, UiInteraction, UiPrimitive, UiTextMeasurer, ViewerAction,
    cap_radius, z_layer,
};
pub use drawer::{
    ContextMenuItem, ContextMenuState, HoverPreviewState, render_context_menu,
    render_hover_preview, render_pr_drawer_chrome, render_tooltip,
};
pub use file_paths::{
    DetectedFilePath, DetectedLink, ParsedFilePath, ResolvedTarget, TargetKind, classify_extension,
    classify_path, classify_uri, detect_at_byte_offset, detect_at_char_col,
    detect_bare_filename_at_char_col, detect_link_at_char_col, detect_thread_uri_at_char_col,
    detect_url_at_char_col, is_thread_uri, parse_file_uri, parse_path_with_location,
    percent_decode, resolve_bare_filename_in_cwd, resolve_bare_filename_in_viewport,
};
pub use icons::IconId;
pub use layout::{
    ChromeLayoutConfig, ChromeRects, DividerGeometry, DropZone, DropZoneHit, FloatingHitZone,
    FloatingLayerSpec, FloatingPaneGeometry, MinimizedCapsuleGeometry, MinimizedCorner,
    PaneDensity, PaneGeometry, PaneHeaderMode, RectF, SplitAxis, SplitLayoutResult, SplitNode,
    ToolbarBehavior, ToolbarPosition, VIEWER_DEFAULT_RATIO, VIEWER_MAX_RATIO, VIEWER_MIN_RATIO,
    ViewerSplitGeometry, clamp_viewer_ratio, compute_chrome_layout, compute_chrome_layout_animated,
    compute_drag_ratio, compute_viewer_drag_ratio, hit_test_divider, hit_test_drop_zone,
    hit_test_floating_pane, layout_floating_layers, layout_minimized_capsules, layout_splits,
    layout_workspace_with_viewer, layout_workspace_with_viewer_animated,
};
pub use modal::{
    DiagnosticsHostRow, DiagnosticsSnapshot, FilePickerEntry, FilePickerPurpose, FindBarState,
    ModalContent, ModalKey, ModalKeyOutcome, ModalState, SettingsViewSnapshot, ToastAnchor,
    ToastItem, ToastSeverity, WhichKeyState, render_find_bar, render_modal, render_toasts,
    render_which_key,
};
pub use palette::{PaletteItem, PaletteMatcher, ScoredMatch, highlight_spans};
pub use pane_chrome::{
    Osc94Progress, Osc94State, PaneChromeOptions, PaneMeta, PortBadge, ScrollbarMetrics,
    render_workspace_chrome,
};
pub use sidebar::{
    ActivitySortMode, HostConnectionState, SidebarHostView, SidebarScrollMetrics,
    SidebarSessionView, SidebarState, SidebarTabView, TabIconColorPolicy,
    compute_sidebar_scroll_metrics, render_sidebar,
};
pub use tabbar::{TabBarView, render_tab_bar};
pub use text_input::{ImePreedit, TextInput};
pub use viewer::{
    AudioViewerState, ComputedMediaPlacement, DiffGutterKind, DiffLineKind, GitDiffHunk,
    GitDiffLine, GitFileDiff, ImageViewerState, MAX_ZOOM, MIN_ZOOM, MarkdownInlineImage,
    MediaViewportState, PdfViewerState, RenderedRowMap, TEXT_LINE_HEIGHT, TextDocPos,
    TextViewerState, VideoViewerState, ViewerContent, ViewerImagePlacement, ViewerRenderMetrics,
    ViewerRenderOutput, ViewerSearchMatch, ViewerSearchState, ViewerState, ViewerTab,
    ViewerTabKind, ZOOM_STEP_FACTOR, compute_fit_zoom, compute_media_placement,
    extract_markdown_image_refs, format_file_size, is_closing_markdown_fence, parse_markdown_fence,
    parse_markdown_image_target, render_viewer, render_viewer_with_measurer, scrub_fraction_from_x,
    scrub_time_from_x,
};

/// Snapshot of workspace state fed into [`UiTree::render`] each frame.
#[derive(Debug, Clone)]
pub struct UiFrameInput {
    pub viewport_w: f32,
    pub viewport_h: f32,
    pub hosts: Vec<SidebarHostView>,
    pub active_host_id: String,
    pub active_session_id: String,
    pub active_session_label: String,
    pub split_root: Option<SplitNode<String>>,
    pub zoomed_pane: Option<String>,
    pub hovered_header_pane: Option<String>,
    pub floating_layers: Vec<FloatingLayerSpec<String>>,
    pub pane_metas: Vec<PaneMeta>,
    pub dnd_overlay: Option<DropZoneHit>,
    pub fill_horizontal_tabs: bool,
}

/// Rendered frame geometry alongside the backend-agnostic [`UiDrawList`].
#[derive(Debug, Clone)]
pub struct UiRenderOutput {
    pub chrome: ChromeRects,
    pub sidebar_scroll: SidebarScrollMetrics,
    pub tiled_panes: Vec<PaneGeometry<String>>,
    pub dividers: Vec<DividerGeometry>,
    pub floating_panes: Vec<FloatingPaneGeometry<String>>,
    pub hover_image_slot: Option<RectF>,
    pub viewer_split: Option<ViewerSplitGeometry>,
    pub viewer_images: Vec<ViewerImagePlacement>,
    pub viewer_metrics: ViewerRenderMetrics,
    pub draw_list: UiDrawList,
}

/// Top-level UI chrome state coordinator owned by `omarex`.
pub struct UiTree {
    pub focus: FocusGuard,
    pub layout_cfg: ChromeLayoutConfig,
    pub sidebar: SidebarState,
    pub pane_options: PaneChromeOptions,
    pub viewer: ViewerState,
    pub modal: Option<ModalState>,
    pub find_bar: FindBarState,
    pub context_menu: Option<ContextMenuState>,
    pub hover_preview: Option<HoverPreviewState>,
    pub which_key: Option<WhichKeyState>,
    pub toasts: Vec<ToastItem>,
    pub toast_anchor: ToastAnchor,
    /// Latest hovered chrome target for tactile hover pills & icon-button tooltips.
    pub hovered_hit: Option<HitTarget>,
    /// Bounding rect of `hovered_hit` (used to anchor hover tooltips cleanly).
    pub hovered_hit_rect: Option<RectF>,
    /// Currently pressed chrome target between pointer-down and pointer-up.
    pub pressed_hit: Option<HitTarget>,
    /// Last pointer coordinates in logical pixels.
    pub pointer_pos: (f32, f32),
    /// Horizontal scroll offset for overflowing top/bottom tab strips.
    pub tabbar_scroll_x: f32,
    /// Visual overlay for active tab drag-to-reorder.
    pub tab_reorder_overlay: Option<TabReorderOverlay>,
    /// Latest known online/connecting state per host (for `Disconnect` vs `Connect` menu item).
    host_connected: std::collections::HashMap<String, bool>,
    /// Spring-physics and slide/fade transition state for border, sidebar, viewer, and modals.
    anim: anim::UiAnimationState,
}

impl Default for UiTree {
    fn default() -> Self {
        Self::new()
    }
}

impl UiTree {
    #[must_use]
    pub fn new() -> Self {
        Self {
            focus: FocusGuard::default(),
            layout_cfg: ChromeLayoutConfig::default(),
            sidebar: SidebarState::default(),
            pane_options: PaneChromeOptions::default(),
            viewer: ViewerState::new(),
            modal: None,
            find_bar: FindBarState::default(),
            context_menu: None,
            hover_preview: None,
            which_key: None,
            toasts: Vec::new(),
            toast_anchor: ToastAnchor::default(),
            hovered_hit: None,
            hovered_hit_rect: None,
            pressed_hit: None,
            pointer_pos: (0.0, 0.0),
            tabbar_scroll_x: 0.0,
            tab_reorder_overlay: None,
            host_connected: std::collections::HashMap::new(),
            anim: anim::UiAnimationState::default(),
        }
    }

    /// Advances all UI chrome spring and slide/fade animations to `now`.
    /// Returns `true` if any animated visual state changed or is still in motion.
    pub fn tick_animations(&mut self, now: Instant) -> bool {
        self.anim.tick(now)
    }

    /// Returns `true` when any UI chrome spring or slide/fade animation is actively running.
    #[must_use]
    pub fn has_active_animations(&self) -> bool {
        self.anim.has_active_animations()
    }

    /// Returns `true` while a docked sidebar or right-hand viewer slide changes terminal workspace
    /// dimensions (used to defer PTY `TerminalResize` RPCs until settled).
    #[must_use]
    pub fn is_layout_animating(&self) -> bool {
        self.anim.is_layout_animating()
    }

    /// Immediately snaps all active UI animations to their target state (used by `--headless-render`).
    pub fn snap_animations(&mut self) {
        self.anim.snap_all();
    }

    /// Borrowed interaction state snapshot passed into chrome renderers.
    #[must_use]
    pub fn interaction(&self) -> UiInteraction<'_> {
        UiInteraction {
            hovered: self.hovered_hit.as_ref(),
            pressed: self.pressed_hit.as_ref(),
        }
    }

    /// Updates hovered chrome target, syncs sidebar/context-menu hover indices, and returns
    /// `true` only when hover state actually changed (avoiding redundant frames on idle motion).
    pub fn update_pointer_hover(&mut self, draw_list: &UiDrawList, px: f32, py: f32) -> bool {
        self.pointer_pos = (px, py);
        let entry = draw_list.hit_test_entry(px, py);
        let new_hit = entry.map(|e| e.target.clone());
        let new_rect = entry.map(|e| e.rect);

        let new_tab = match new_hit.as_ref() {
            Some(HitTarget::TabCard { window_id, .. } | HitTarget::TabClose { window_id, .. }) => {
                Some(window_id.clone())
            }
            _ => None,
        };
        let new_session = match new_hit.as_ref() {
            Some(
                HitTarget::SessionRow { session_id, .. }
                | HitTarget::SessionChevron { session_id, .. }
                | HitTarget::SessionNewTab { session_id, .. }
                | HitTarget::SessionNewT3Thread { session_id, .. }
                | HitTarget::SessionMuteBell { session_id, .. },
            ) => Some(session_id.clone()),
            _ => None,
        };
        let new_menu_idx = match new_hit.as_ref() {
            Some(HitTarget::ContextMenuItem { index }) => Some(*index),
            _ => None,
        };

        let mut changed = self.hovered_hit != new_hit || self.hovered_hit_rect != new_rect;
        self.hovered_hit = new_hit;
        self.hovered_hit_rect = new_rect;

        if self.sidebar.hovered_tab_window_id != new_tab {
            self.sidebar.hovered_tab_window_id = new_tab;
            changed = true;
        }
        if self.sidebar.hovered_session_id != new_session {
            self.sidebar.hovered_session_id = new_session;
            changed = true;
        }
        if let Some(menu) = &mut self.context_menu
            && menu.hovered_index != new_menu_idx
        {
            menu.hovered_index = new_menu_idx;
            changed = true;
        }
        changed
    }

    /// Marks the hit target under `(px, py)` as pressed for tactile button feedback.
    pub fn set_pointer_pressed(&mut self, draw_list: &UiDrawList, px: f32, py: f32) -> bool {
        self.pointer_pos = (px, py);
        let next = draw_list.hit_test(px, py).cloned();
        let changed = self.pressed_hit != next;
        self.pressed_hit = next;
        changed
    }

    /// Clears pressed button state on pointer release.
    pub fn clear_pointer_pressed(&mut self) -> bool {
        self.pressed_hit.take().is_some()
    }

    /// Applies horizontal scroll delta to an overflowing top/bottom tab strip.
    pub fn scroll_tabbar_by(&mut self, delta_x_px: f32) {
        self.tabbar_scroll_x = (self.tabbar_scroll_x - delta_x_px).max(0.0);
    }

    /// Applies a background `session_view_changed` active-block update while strictly preserving
    /// focus on any active modal, palette, find bar, sidebar filter, or context menu.
    pub fn on_session_view_changed(&mut self, active_block: Option<String>) {
        self.focus.on_session_view_changed(active_block);
    }

    /// Opens a modal and transfers keyboard focus to `FocusTarget::Modal`.
    pub fn open_modal(&mut self, modal: ModalState) {
        let kind = modal.kind();
        self.context_menu = None;
        self.anim.closing_modal = None;
        self.anim.modal_open_progress = anim::Spring1D::transition_from(0.05, 1.0);
        self.modal = Some(modal);
        self.focus.focus_modal(kind);
    }

    /// Opens a native `rex.ui` prompt modal (`client_prompt_requested`).
    pub fn open_rex_prompt(&mut self, session: PromptSession) {
        self.open_modal(ModalState::open_rex_prompt(session));
    }

    /// Dismisses the active `rex.ui` prompt cleanly if `call_id` matches (`client_prompt_ended`).
    pub fn dismiss_rex_prompt_if_matches(&mut self, call_id: &str) -> bool {
        let matches = matches!(
            self.modal.as_ref().map(|m| &m.content),
            Some(ModalContent::RexPrompt { session, .. }) if session.call_id == call_id
        );
        if matches {
            self.close_modal();
            true
        } else {
            false
        }
    }

    /// Closes any active modal and restores terminal focus immediately while spawning a
    /// non-blocking visual fade-out shadow.
    pub fn close_modal(&mut self) {
        if let Some(closed) = self.modal.take() {
            let start_p = self.anim.modal_open_progress.value.clamp(0.05, 1.0);
            self.anim.closing_modal = Some((closed, anim::Spring1D::transition_from(start_p, 0.0)));
        }
        if matches!(self.focus.current(), FocusTarget::Modal(_)) {
            self.focus.restore_terminal();
        }
    }

    /// Opens the inline Find bar (`Ctrl+Shift+F`) and focuses it.
    pub fn open_find_bar(&mut self) {
        self.find_bar.open = true;
        self.find_bar.input.select_all();
        self.focus.focus_find_bar();
    }

    pub fn close_find_bar(&mut self) {
        self.find_bar.open = false;
        if matches!(self.focus.current(), FocusTarget::FindBar) {
            self.focus.restore_terminal();
        }
    }

    /// Opens a right-click context menu and moves focus to `FocusTarget::ContextMenu`.
    pub fn open_context_menu(&mut self, menu: ContextMenuState) {
        self.context_menu = Some(menu);
        self.focus.focus_context_menu();
    }

    pub fn close_context_menu(&mut self) {
        self.context_menu = None;
        if matches!(self.focus.current(), FocusTarget::ContextMenu) {
            self.focus.restore_terminal();
        }
    }

    /// Transfers keyboard focus to the docked right-hand Viewer Pane.
    pub fn focus_viewer(&mut self) {
        if self.viewer.open {
            self.focus.focus_viewer();
        }
    }

    /// Closes the docked right-hand Viewer Pane and restores terminal focus immediately while
    /// preserving a non-blocking slide-out shadow.
    pub fn close_viewer(&mut self) {
        if self.viewer.open {
            let start_p = self.anim.viewer_progress.value.clamp(0.05, 1.0);
            if let Some(last_vs) = self.anim.last_viewer_split {
                self.anim.closing_viewer = Some((
                    last_vs,
                    self.viewer.clone(),
                    anim::Spring1D::transition_from(start_p, 0.0),
                ));
            }
        }
        self.viewer.close_pane();
        self.viewer.close_search();
        self.anim.viewer_progress.snap(0.0);
        if matches!(
            self.focus.current(),
            FocusTarget::Viewer | FocusTarget::ViewerSearch
        ) {
            self.focus.restore_terminal();
        }
    }

    /// Closes a single viewer tab by `tab_id`, preserving the slide-out fade shadow when closing
    /// the last remaining tab.
    pub fn close_viewer_tab(&mut self, tab_id: u64) -> Option<viewer::ViewerTab> {
        let closing_last =
            self.viewer.open && self.viewer.tabs.len() == 1 && self.viewer.tabs[0].id == tab_id;
        if closing_last {
            let start_p = self.anim.viewer_progress.value.clamp(0.05, 1.0);
            if let Some(last_vs) = self.anim.last_viewer_split {
                self.anim.closing_viewer = Some((
                    last_vs,
                    self.viewer.clone(),
                    anim::Spring1D::transition_from(start_p, 0.0),
                ));
            }
        }
        let removed = self.viewer.close_tab_by_id(tab_id);
        if closing_last && !self.viewer.open {
            self.anim.viewer_progress.snap(0.0);
        }
        if !self.viewer.open
            && matches!(
                self.focus.current(),
                FocusTarget::Viewer | FocusTarget::ViewerSearch
            )
        {
            self.viewer.close_search();
            self.focus.restore_terminal();
        }
        removed
    }

    /// Feeds measured viewer layout metrics from [`UiRenderOutput::viewer_metrics`] back into [`ViewerState`].
    pub fn sync_viewer_metrics(&mut self, metrics: &ViewerRenderMetrics) {
        self.viewer.sync_render_metrics(metrics);
    }

    /// Routes a key event to the active modal (if any) and maps it to a [`UiAction`].
    pub fn handle_modal_key(&mut self, key: ModalKey) -> UiAction {
        let Some(modal) = self.modal.as_mut() else {
            return UiAction::None;
        };
        match modal.handle_key(key) {
            ModalKeyOutcome::Consumed => UiAction::None,
            ModalKeyOutcome::Closed => {
                self.close_modal();
                UiAction::CloseModal
            }
            ModalKeyOutcome::Action(action) => {
                let keep_open = matches!(
                    action,
                    UiAction::UpdateSetting(ref ch) if !matches!(ch, SettingChange::OpenThemePicker)
                ) || matches!(
                    action,
                    UiAction::FilePickerDescend { .. }
                        | UiAction::FilePickerParent
                        | UiAction::ThemePickerPreview { .. }
                        | UiAction::CopyDiagnosticsBundle
                );
                if !keep_open {
                    self.close_modal();
                }
                action
            }
            ModalKeyOutcome::ReplyPrompt(reply) => {
                self.close_modal();
                UiAction::ReplyRexPrompt(reply)
            }
        }
    }

    /// Resolves a pointer click at `(px, py)` against the latest frame's [`UiDrawList`].
    pub fn handle_click(
        &mut self,
        draw_list: &UiDrawList,
        px: f32,
        py: f32,
        right_click: bool,
    ) -> UiAction {
        let Some(entry) = draw_list.hit_test_entry(px, py) else {
            return UiAction::None;
        };
        let hit_rect = entry.rect;
        let hit = entry.target.clone();

        // Right-click opens contextual menus for panes, tabs, and sessions.
        if right_click {
            match hit {
                HitTarget::PaneSurface { block_id } | HitTarget::PaneHeader { block_id } => {
                    let is_floating = draw_list.hits.iter().any(|h| {
                        matches!(&h.target, HitTarget::FloatingHandle { block_id: b, .. } if b == &block_id)
                    });
                    let is_zoomed = draw_list.hits.iter().any(|h| {
                        matches!(&h.target, HitTarget::PaneZoomBadge { block_id: b } if b == &block_id)
                    });
                    self.open_context_menu(ContextMenuState::for_pane(
                        px,
                        py,
                        block_id,
                        is_floating,
                        is_zoomed,
                    ));
                    return UiAction::None;
                }
                HitTarget::TabCard {
                    host_id,
                    session_id,
                    window_id,
                } => {
                    self.open_context_menu(ContextMenuState::for_tab(
                        px, py, host_id, session_id, window_id, false,
                    ));
                    return UiAction::None;
                }
                HitTarget::HostHeader { host_id } | HitTarget::HostNewSession { host_id } => {
                    let row_rect = draw_list
                        .hits
                        .iter()
                        .find(|h| {
                            matches!(&h.target, HitTarget::HostHeader { host_id: hid } if hid == &host_id)
                        })
                        .map_or(hit_rect, |h| h.rect);
                    let is_local = host_id == "local"
                        || draw_list.primitives.iter().any(|p| {
                            matches!(
                                p,
                                UiPrimitive::Icon {
                                    rect,
                                    icon: IconId::Terminal,
                                    ..
                                } if row_rect.contains(rect.center().0, rect.center().1)
                            )
                        });
                    let collapsed = draw_list.primitives.iter().any(|p| {
                        matches!(
                            p,
                            UiPrimitive::Icon {
                                rect,
                                icon: IconId::ChevronRight,
                                ..
                            } if row_rect.contains(rect.center().0, rect.center().1)
                        )
                    });
                    let is_connected = self.host_connected.get(&host_id).copied().unwrap_or(true);
                    self.open_context_menu(ContextMenuState::for_host(
                        px,
                        py,
                        host_id,
                        is_local,
                        collapsed,
                        is_connected,
                    ));
                    return UiAction::None;
                }
                HitTarget::SessionRow {
                    host_id,
                    session_id,
                }
                | HitTarget::SessionChevron {
                    host_id,
                    session_id,
                } => {
                    let is_t3 = draw_list.hits.iter().any(|h| {
                        matches!(
                            &h.target,
                            HitTarget::SessionNewT3Thread { host_id: hid, session_id: sid }
                                if hid == &host_id && sid == &session_id
                        )
                    });
                    let has_visible_tabs = draw_list.hits.iter().any(|h| {
                        matches!(
                            &h.target,
                            HitTarget::TabCard { host_id: hid, session_id: sid, .. }
                                if hid == &host_id && sid == &session_id
                        )
                    });
                    self.open_context_menu(ContextMenuState::for_session_with_state(
                        px,
                        py,
                        host_id,
                        session_id,
                        false,
                        !has_visible_tabs,
                        is_t3,
                    ));
                    return UiAction::None;
                }
                HitTarget::ContextMenuBackdrop => {
                    self.close_context_menu();
                    return UiAction::None;
                }
                _ => return UiAction::None,
            }
        }

        match hit {
            HitTarget::ContextMenuBackdrop => {
                self.close_context_menu();
                UiAction::None
            }
            HitTarget::ContextMenuItem { index } => {
                let action = self
                    .context_menu
                    .as_ref()
                    .and_then(|m| m.activate_index(index))
                    .unwrap_or(UiAction::None);
                self.close_context_menu();
                action
            }
            HitTarget::ModalBackdrop | HitTarget::ModalCancelButton => {
                if let Some(ModalContent::RexPrompt { session, .. }) =
                    self.modal.as_ref().map(|m| &m.content)
                {
                    let reply = session.cancel();
                    self.close_modal();
                    return UiAction::ReplyRexPrompt(reply);
                }
                if matches!(
                    self.modal.as_ref().map(|m| &m.content),
                    Some(ModalContent::ThemePicker { .. })
                ) {
                    self.close_modal();
                    return UiAction::ThemePickerCancel;
                }
                self.close_modal();
                UiAction::CloseModal
            }
            HitTarget::ModalInput => UiAction::None,
            HitTarget::ModalRow { index } => self.apply_modal_outcome_for_row(index, false),
            HitTarget::ModalCheckbox { index } => self.apply_modal_outcome_for_row(index, true),
            HitTarget::ModalOtherRow => {
                if let Some(ModalContent::RexPrompt { session, .. }) =
                    self.modal.as_ref().map(|m| &m.content)
                    && let Some(reply) = session.submit_other_text()
                {
                    self.close_modal();
                    return UiAction::ReplyRexPrompt(reply);
                }
                UiAction::None
            }
            HitTarget::ModalConfirmButton => self.handle_modal_key(ModalKey::CtrlEnter),
            HitTarget::SettingsToggle { key } => {
                if let Some(modal) = self.modal.as_mut() {
                    match modal.activate_setting_key(&key) {
                        ModalKeyOutcome::Action(action) => {
                            if matches!(
                                action,
                                UiAction::UpdateSetting(SettingChange::OpenThemePicker)
                            ) {
                                self.close_modal();
                            }
                            action
                        }
                        ModalKeyOutcome::Closed => {
                            self.close_modal();
                            UiAction::CloseModal
                        }
                        ModalKeyOutcome::ReplyPrompt(reply) => {
                            self.close_modal();
                            UiAction::ReplyRexPrompt(reply)
                        }
                        ModalKeyOutcome::Consumed => UiAction::None,
                    }
                } else {
                    UiAction::None
                }
            }
            HitTarget::SettingsCategoryTab(cat) => {
                if let Some(modal) = self.modal.as_mut() {
                    modal.select_settings_category(cat);
                }
                UiAction::None
            }
            HitTarget::DiagnosticsCopyBundle => UiAction::CopyDiagnosticsBundle,
            HitTarget::SidebarToggle => {
                if self.layout_cfg.toolbar_behavior == ToolbarBehavior::Always
                    && !self.layout_cfg.sidebar_collapsed
                {
                    if self.layout_cfg.toolbar_revealed {
                        self.layout_cfg.toolbar_revealed = false;
                    } else if draw_list
                        .hits
                        .iter()
                        .any(|h| matches!(h.target, HitTarget::ToolbarHotEdge))
                    {
                        self.layout_cfg.toolbar_revealed = true;
                    } else {
                        self.layout_cfg.sidebar_collapsed = true;
                    }
                } else {
                    self.layout_cfg.sidebar_collapsed = !self.layout_cfg.sidebar_collapsed;
                    self.layout_cfg.toolbar_revealed = false;
                }
                UiAction::ToggleSidebar
            }
            HitTarget::SidebarResizeHandle => UiAction::BeginSidebarResize,
            HitTarget::ToolbarHotEdge => {
                self.layout_cfg.toolbar_revealed = true;
                UiAction::None
            }
            HitTarget::ActivityToggle => {
                self.sidebar.toggle_activity();
                UiAction::ToggleActivityMode
            }
            HitTarget::ActivitySortToggle => {
                self.sidebar.toggle_activity_sort();
                UiAction::ToggleActivitySort
            }
            HitTarget::SidebarFilterToggle => {
                if self.sidebar.filter_open {
                    self.sidebar.close_filter();
                    self.focus.restore_terminal();
                } else {
                    self.sidebar.open_filter();
                    self.focus.focus_sidebar_filter();
                }
                UiAction::None
            }
            HitTarget::SidebarFilterInput => {
                self.focus.focus_sidebar_filter();
                UiAction::None
            }
            HitTarget::NewSessionButton => UiAction::NewSession { host_id: None },
            HitTarget::HostsButton => UiAction::OpenModal(ModalKind::AddHost),
            HitTarget::SettingsButton => UiAction::OpenModal(ModalKind::Settings),
            HitTarget::HostHeader { host_id } => UiAction::ToggleHostCollapse { host_id },
            HitTarget::HostNewSession { host_id } => UiAction::NewSession {
                host_id: Some(host_id),
            },
            HitTarget::SessionChevron {
                host_id,
                session_id,
            } => UiAction::ToggleSessionCollapse {
                host_id,
                session_id,
            },
            HitTarget::SessionRow {
                host_id,
                session_id,
            } => UiAction::SelectSession {
                host_id,
                session_id,
            },
            HitTarget::SessionMuteBell {
                host_id,
                session_id,
            } => UiAction::ToggleSessionMute {
                host_id,
                session_id,
            },
            HitTarget::SessionNewTab {
                host_id,
                session_id,
            } => UiAction::NewTab {
                host_id: Some(host_id),
                session_id: Some(session_id),
            },
            HitTarget::SessionNewT3Thread {
                host_id,
                session_id,
            } => UiAction::NewT3Thread {
                host_id,
                session_id,
            },
            HitTarget::TabCard {
                host_id,
                session_id,
                window_id,
            } => UiAction::SelectTab {
                host_id,
                session_id,
                window_id,
            },
            HitTarget::TabClose {
                host_id,
                session_id,
                window_id,
            } => UiAction::CloseTab {
                host_id,
                session_id,
                window_id,
            },
            HitTarget::TabNew | HitTarget::EmptyStateNewTerminal => UiAction::NewTerminal,
            HitTarget::EmptyStateNewTab => UiAction::RunCommand {
                action_id: "t3_new".to_owned(),
            },
            HitTarget::SessionSwitcherPill => UiAction::OpenModal(ModalKind::SwitchSession),
            HitTarget::CommandPaletteButton => UiAction::OpenModal(ModalKind::CommandPalette),
            HitTarget::PaneSurface { block_id } | HitTarget::PaneHeader { block_id } => {
                self.focus.focus_terminal(Some(block_id.clone()));
                UiAction::FocusPane { block_id }
            }
            HitTarget::PaneButton { block_id, button } => UiAction::PaneButton { block_id, button },
            HitTarget::PaneZoomBadge { block_id } => UiAction::UnzoomPane { block_id },
            HitTarget::PanePortPill { block_id, port } => {
                UiAction::OpenPortAction { block_id, port }
            }
            HitTarget::PaneNewOutputPill { block_id } => {
                self.focus.focus_terminal(Some(block_id.clone()));
                UiAction::ScrollPaneToBottom { block_id }
            }
            HitTarget::PaneScrollbarTrack { block_id }
            | HitTarget::PaneScrollbarThumb { block_id } => {
                let track = draw_list
                    .hits
                    .iter()
                    .find(|h| {
                        matches!(
                            &h.target,
                            HitTarget::PaneScrollbarTrack { block_id: b } if b == &block_id
                        )
                    })
                    .map_or(hit_rect, |h| h.rect);
                let fraction = ((py - track.y) / track.h.max(1.0)).clamp(0.0, 1.0);
                UiAction::ScrollPaneToFraction { block_id, fraction }
            }
            HitTarget::SplitDivider { split_path } => {
                UiAction::BeginSplitDividerDrag { split_path }
            }
            HitTarget::FloatingHandle { block_id, zone } => {
                UiAction::BeginFloatingDrag { block_id, zone }
            }
            HitTarget::MinimizedCapsule { block_id } => {
                UiAction::RestoreMinimizedFloating { block_id }
            }
            HitTarget::MinimizedCapsuleClose { block_id } => {
                UiAction::CloseMinimizedFloating { block_id }
            }
            HitTarget::FindBarInput => {
                self.focus.focus_find_bar();
                UiAction::None
            }
            HitTarget::FindBarPrev => UiAction::FindPrev,
            HitTarget::FindBarNext => UiAction::FindNext,
            HitTarget::FindBarCaseToggle => {
                self.find_bar.case_sensitive = !self.find_bar.case_sensitive;
                UiAction::FindToggleCase
            }
            HitTarget::FindBarRegexToggle => {
                self.find_bar.regex = !self.find_bar.regex;
                UiAction::FindToggleRegex
            }
            HitTarget::FindBarClose => {
                self.close_find_bar();
                UiAction::CloseFindBar
            }
            HitTarget::ToastBody { id } => UiAction::ActivateToast { id },
            HitTarget::ToastDismiss { id } => {
                self.toasts.retain(|t| t.id != id);
                UiAction::DismissToast { id }
            }
            HitTarget::ViewerDivider => UiAction::Viewer(ViewerAction::BeginDividerDrag),
            HitTarget::ViewerSurface | HitTarget::ViewerTextBody => {
                self.focus_viewer();
                UiAction::Viewer(ViewerAction::Focus)
            }
            HitTarget::ViewerTab { tab_id } => {
                self.viewer.select_tab_by_id(tab_id);
                if self.viewer.search.open {
                    self.viewer.recompute_search();
                }
                self.focus_viewer();
                UiAction::Viewer(ViewerAction::SelectTab { tab_id })
            }
            HitTarget::ViewerTabClose { tab_id } => {
                self.close_viewer_tab(tab_id);
                UiAction::Viewer(ViewerAction::CloseTab { tab_id })
            }
            HitTarget::ViewerCopyPath => {
                self.focus_viewer();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::CopyPath {
                        path: tab.path.clone(),
                    })
                })
            }
            HitTarget::ViewerCopyPathLine => {
                self.focus_viewer();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    let line = match &tab.content {
                        ViewerContent::Text(txt) => txt.target_or_active_line(),
                        ViewerContent::Pdf(pdf) => pdf.current_page,
                        _ => 1,
                    };
                    UiAction::Viewer(ViewerAction::CopyPathLine {
                        path: tab.path.clone(),
                        line,
                    })
                })
            }
            HitTarget::ViewerOpenExternal => {
                self.focus_viewer();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::OpenExternal {
                        path: tab.path.clone(),
                        remote_host: tab.remote_host.clone(),
                    })
                })
            }
            HitTarget::ViewerMaximizeToggle => {
                self.viewer.toggle_maximized();
                self.focus_viewer();
                UiAction::Viewer(ViewerAction::ToggleMaximize)
            }
            HitTarget::ViewerClose => {
                self.close_viewer();
                UiAction::Viewer(ViewerAction::ClosePane)
            }
            HitTarget::ViewerWrapToggle => {
                self.focus_viewer();
                self.viewer.toggle_wrap();
                UiAction::Viewer(ViewerAction::ToggleWrap)
            }
            HitTarget::ViewerRawToggle => {
                self.focus_viewer();
                self.viewer.toggle_raw_markdown();
                UiAction::Viewer(ViewerAction::ToggleRawMarkdown)
            }
            HitTarget::ViewerDiffToggle => {
                self.focus_viewer();
                self.viewer.toggle_diff_mode();
                UiAction::Viewer(ViewerAction::ToggleDiff)
            }
            HitTarget::ViewerLink { url } => {
                self.focus_viewer();
                let base_path = self
                    .viewer
                    .active_tab()
                    .map_or_else(String::new, |t| t.path.clone());
                UiAction::Viewer(ViewerAction::OpenLink { url, base_path })
            }
            HitTarget::ViewerSearchInput => {
                self.focus.focus_viewer_search();
                UiAction::Viewer(ViewerAction::SearchOpen)
            }
            HitTarget::ViewerSearchPrev => {
                self.focus.focus_viewer_search();
                self.viewer.prev_search_match();
                UiAction::Viewer(ViewerAction::SearchPrev)
            }
            HitTarget::ViewerSearchNext => {
                self.focus.focus_viewer_search();
                self.viewer.next_search_match();
                UiAction::Viewer(ViewerAction::SearchNext)
            }
            HitTarget::ViewerSearchCaseToggle => {
                self.focus.focus_viewer_search();
                self.viewer.toggle_search_case();
                UiAction::Viewer(ViewerAction::SearchToggleCase)
            }
            HitTarget::ViewerSearchClose => {
                self.viewer.close_search();
                self.focus_viewer();
                UiAction::Viewer(ViewerAction::SearchClose)
            }
            HitTarget::ViewerZoomOut => {
                self.focus_viewer();
                self.viewer.zoom_out();
                UiAction::Viewer(ViewerAction::ZoomOut)
            }
            HitTarget::ViewerZoomReset => {
                self.focus_viewer();
                self.viewer.zoom_reset();
                UiAction::Viewer(ViewerAction::ZoomReset)
            }
            HitTarget::ViewerZoomIn => {
                self.focus_viewer();
                self.viewer.zoom_in();
                UiAction::Viewer(ViewerAction::ZoomIn)
            }
            HitTarget::ViewerZoomFit => {
                self.focus_viewer();
                self.viewer.zoom_fit();
                UiAction::Viewer(ViewerAction::ZoomFit)
            }
            HitTarget::ViewerCanvas => {
                self.focus_viewer();
                UiAction::Viewer(ViewerAction::BeginPan)
            }
            HitTarget::ViewerVideoPlayPause => {
                self.focus_viewer();
                self.viewer.toggle_video_play_pause();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::ToggleVideoPlayPause { tab_id: tab.id })
                })
            }
            HitTarget::ViewerVideoSeekBar => {
                self.focus_viewer();
                let fraction = scrub_fraction_from_x(hit_rect, px);
                self.viewer.seek_active_video_fraction(fraction);
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::SeekVideo {
                        tab_id: tab.id,
                        fraction,
                    })
                })
            }
            HitTarget::ViewerVideoMuteToggle => {
                self.focus_viewer();
                self.viewer.toggle_video_mute();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::ToggleVideoMute { tab_id: tab.id })
                })
            }
            HitTarget::ViewerVideoLoopToggle => {
                self.focus_viewer();
                self.viewer.toggle_video_loop();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::ToggleVideoLoop { tab_id: tab.id })
                })
            }
            HitTarget::ViewerAudioPlayPause => {
                self.focus_viewer();
                self.viewer.toggle_audio_play_pause();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::ToggleAudioPlayPause { tab_id: tab.id })
                })
            }
            HitTarget::ViewerAudioSeekBar | HitTarget::ViewerAudioWaveform => {
                self.focus_viewer();
                let fraction = scrub_fraction_from_x(hit_rect, px);
                self.viewer.seek_active_audio_fraction(fraction);
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::SeekAudio {
                        tab_id: tab.id,
                        fraction,
                    })
                })
            }
            HitTarget::ViewerAudioMuteToggle => {
                self.focus_viewer();
                self.viewer.toggle_audio_mute();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::ToggleAudioMute { tab_id: tab.id })
                })
            }
            HitTarget::ViewerAudioLoopToggle => {
                self.focus_viewer();
                self.viewer.toggle_audio_loop();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::ToggleAudioLoop { tab_id: tab.id })
                })
            }
            HitTarget::ViewerPdfPrevPage => {
                self.focus_viewer();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::PdfPrevPage { tab_id: tab.id })
                })
            }
            HitTarget::ViewerPdfNextPage => {
                self.focus_viewer();
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::PdfNextPage { tab_id: tab.id })
                })
            }
            HitTarget::ViewerScrollbarTrack | HitTarget::ViewerScrollbarThumb => {
                self.focus_viewer();
                let track = draw_list
                    .hits
                    .iter()
                    .find(|h| h.target == HitTarget::ViewerScrollbarTrack)
                    .map_or(hit_rect, |h| h.rect);
                let fraction = ((py - track.y) / track.h.max(1.0)).clamp(0.0, 1.0);
                self.viewer.scroll_active_to_fraction(fraction);
                self.viewer.active_tab().map_or(UiAction::None, |tab| {
                    UiAction::Viewer(ViewerAction::ScrollToFraction {
                        tab_id: tab.id,
                        fraction,
                    })
                })
            }
        }
    }

    fn apply_modal_outcome_for_row(&mut self, index: usize, checkbox: bool) -> UiAction {
        let Some(modal) = self.modal.as_mut() else {
            return UiAction::None;
        };
        let outcome = if checkbox {
            modal.toggle_checkbox_row(index)
        } else {
            modal.activate_row(index)
        };
        match outcome {
            ModalKeyOutcome::Consumed => UiAction::None,
            ModalKeyOutcome::Closed => {
                self.close_modal();
                UiAction::CloseModal
            }
            ModalKeyOutcome::Action(action) => {
                let keep_open = matches!(
                    action,
                    UiAction::UpdateSetting(ref ch) if !matches!(ch, SettingChange::OpenThemePicker)
                ) || matches!(
                    action,
                    UiAction::FilePickerDescend { .. }
                        | UiAction::FilePickerParent
                        | UiAction::ThemePickerPreview { .. }
                        | UiAction::CopyDiagnosticsBundle
                );
                if !keep_open {
                    self.close_modal();
                }
                action
            }
            ModalKeyOutcome::ReplyPrompt(reply) => {
                self.close_modal();
                UiAction::ReplyRexPrompt(reply)
            }
        }
    }

    /// Clamps sidebar scroll offset to the valid `[0.0, max_scroll]` range and, whenever the
    /// active tab changes (including initial attach/bootstrap), scrolls minimally so the active
    /// tab card sits cleanly inside the visible sidebar viewport.
    pub fn sync_sidebar_scroll(&mut self, input: &UiFrameInput) {
        self.host_connected.clear();
        for h in &input.hosts {
            self.host_connected.insert(
                h.host_id.clone(),
                h.connection != HostConnectionState::Offline,
            );
        }
        let chrome = compute_chrome_layout(input.viewport_w, input.viewport_h, &self.layout_cfg);
        let Some(sb_rect) = chrome.sidebar else {
            return;
        };
        let metrics = compute_sidebar_scroll_metrics(sb_rect, &input.hosts, &self.sidebar);
        self.sidebar.clamp_scroll(metrics.max_scroll);

        let active_key = input.hosts.iter().find_map(|h| {
            h.sessions.iter().find_map(|s| {
                s.tabs.iter().find_map(|t| {
                    t.active
                        .then(|| (h.host_id.clone(), s.session_id.clone(), t.window_id.clone()))
                })
            })
        });

        if active_key != self.sidebar.last_auto_scrolled_tab {
            self.sidebar.ensure_active_tab_visible(metrics);
            self.sidebar.last_auto_scrolled_tab = active_key;
        }
    }

    /// Computes layout and emits the complete backend-agnostic [`UiDrawList`] for one frame.
    pub fn render(
        &mut self,
        input: &UiFrameInput,
        theme: &Theme,
        now: Instant,
        anim_phase: f32,
    ) -> UiRenderOutput {
        self.render_with_measurer(input, theme, now, anim_phase, None)
    }

    /// Same as [`Self::render`], passing an optional [`UiTextMeasurer`] for exact proportional
    /// link hit-rects & underlines in the Markdown viewer.
    pub fn render_with_measurer(
        &mut self,
        input: &UiFrameInput,
        theme: &Theme,
        now: Instant,
        anim_phase: f32,
        measurer: Option<&mut dyn UiTextMeasurer>,
    ) -> UiRenderOutput {
        let mut draw_list = UiDrawList::new();
        let tokens = theme.effective_ui();

        let mut cfg = self.layout_cfg;
        if cfg.hyprland_rounding.is_none() {
            cfg.hyprland_rounding = Some(theme.rounding);
        }

        // Preserve sleek `8px` internal card/pill rounding in `Comfortable` mode even when
        // Hyprland's outer window `decoration:rounding` is `0`, while `Compact` stays `0px` sharp.
        let mut effective_theme = theme.clone();
        effective_theme.rounding = cfg.pane_corner_radius();
        let theme = &effective_theme;

        // 1. Taffy outer chrome layout (with smooth sidebar open/close slide)
        let chrome_target = compute_chrome_layout(input.viewport_w, input.viewport_h, &cfg);
        let sidebar_target_visible = chrome_target.sidebar.is_some();
        if !self.anim.sidebar_initialized {
            self.anim
                .sidebar_progress
                .snap(if sidebar_target_visible { 1.0 } else { 0.0 });
            self.anim.sidebar_initialized = true;
            self.anim.last_sidebar_auto_collapsed = chrome_target.sidebar_auto_collapsed;
            self.anim.last_sidebar_is_overlay = chrome_target.toolbar_is_overlay;
        } else if self.anim.last_sidebar_auto_collapsed != chrome_target.sidebar_auto_collapsed {
            self.anim
                .sidebar_progress
                .snap(if sidebar_target_visible { 1.0 } else { 0.0 });
            self.anim.last_sidebar_auto_collapsed = chrome_target.sidebar_auto_collapsed;
        } else {
            self.anim
                .sidebar_progress
                .set_target(if sidebar_target_visible { 1.0 } else { 0.0 });
            if sidebar_target_visible && self.anim.sidebar_progress.value <= 0.005 {
                self.anim.sidebar_progress.value = 0.08;
            }
        }
        let sp = self.anim.sidebar_progress.value.clamp(0.0, 1.0);
        let is_overlay_sidebar = chrome_target.toolbar_is_overlay
            || (self.anim.last_sidebar_is_overlay && !sidebar_target_visible);
        let chrome = if !is_overlay_sidebar
            && cfg.toolbar_position.is_sidebar()
            && cfg.toolbar_behavior == ToolbarBehavior::Always
        {
            compute_chrome_layout_animated(input.viewport_w, input.viewport_h, &cfg, sp)
        } else {
            chrome_target
        };
        if let Some(sb_rect) = chrome.sidebar {
            self.anim.last_sidebar_rect = chrome_target.sidebar.or(Some(sb_rect));
            self.anim.last_sidebar_is_right = cfg.toolbar_position == ToolbarPosition::Right;
            self.anim.last_sidebar_is_overlay = chrome.toolbar_is_overlay;
        }
        draw_list.push_rect(chrome.viewport, tokens.surface_frame, None, 0.0, 0.0, None);

        if let Some(edge) = chrome.hot_edge {
            draw_list.push_hit(edge, HitTarget::ToolbarHotEdge, z_layer::OVERLAY_TOOLBAR);
        }

        // 2. Optional docked right-hand Viewer Pane horizontal split inside `chrome.workspace`
        if !self.anim.viewer_initialized {
            self.anim
                .viewer_progress
                .snap(if self.viewer.open { 1.0 } else { 0.0 });
            self.anim.viewer_initialized = true;
        } else {
            self.anim
                .viewer_progress
                .set_target(if self.viewer.open { 1.0 } else { 0.0 });
        }
        let vp = self.anim.viewer_progress.value.clamp(0.0, 1.0);
        let target_viewer_split = layout_workspace_with_viewer(
            chrome.workspace,
            self.viewer.open,
            self.viewer.maximized,
            self.viewer.split_ratio,
            &cfg,
        );
        if let Some(vs) = target_viewer_split {
            self.anim.last_viewer_split = Some(vs);
        }
        let viewer_split = if self.viewer.open {
            layout_workspace_with_viewer_animated(
                chrome.workspace,
                true,
                self.viewer.maximized,
                self.viewer.split_ratio,
                &cfg,
                vp.max(0.05),
            )
        } else if let Some((ref closing_vs, _, ref closing_sp)) = self.anim.closing_viewer {
            layout_workspace_with_viewer_animated(
                chrome.workspace,
                true,
                closing_vs.effective_ratio >= 0.999,
                closing_vs.effective_ratio,
                &cfg,
                closing_sp.value,
            )
        } else {
            None
        };
        let terminal_workspace = viewer_split.map_or(chrome.workspace, |vs| vs.terminal_workspace);

        // 2b. Binary split tree, floating layers, and minimized capsules inside `terminal_workspace`
        let split_res = if terminal_workspace.is_empty() {
            None
        } else {
            input.split_root.as_ref().map(|root| {
                layout_splits(
                    terminal_workspace,
                    root,
                    &cfg,
                    input.zoomed_pane.as_ref(),
                    input.hovered_header_pane.as_ref(),
                )
            })
        };
        let (tiled_panes, dividers) = match split_res {
            Some(r) => (r.panes, r.dividers),
            None => (Vec::new(), Vec::new()),
        };
        let floating_panes = if terminal_workspace.is_empty() {
            Vec::new()
        } else {
            layout_floating_layers(terminal_workspace, &input.floating_layers, &cfg)
        };
        let capsules = if terminal_workspace.is_empty() {
            Vec::new()
        } else {
            layout_minimized_capsules(
                terminal_workspace,
                &input.floating_layers,
                cfg.minimized_corner,
            )
        };

        // Sync spring-physics active-pane border target (snaps on first pane, window resize,
        // or split-divider resize; spring-animates on focus switch or zoom toggle).
        let focused_pane = tiled_panes
            .iter()
            .find(|g| {
                input
                    .pane_metas
                    .iter()
                    .any(|m| m.block_id == g.id && m.focused)
            })
            .or_else(|| tiled_panes.first());
        let spring_active_border = if let Some(fgeo) = focused_pane {
            let target = fgeo.card_rect;
            let same_block = self.anim.active_border_block.as_deref() == Some(fgeo.id.as_str());
            let zoom_changed = self.anim.active_border_zoomed != input.zoomed_pane;
            let viewport_changed =
                self.anim.active_border_viewport != Some((input.viewport_w, input.viewport_h));

            match &mut self.anim.active_border_rect {
                None => {
                    self.anim.active_border_rect = Some(SpringRect::from_rect(target));
                }
                Some(rect) => {
                    if !same_block || zoom_changed {
                        rect.set_target(target);
                    } else if viewport_changed || rect.target() != target {
                        rect.snap(target);
                    }
                }
            }
            self.anim.active_border_block = Some(fgeo.id.clone());
            self.anim.active_border_viewport = Some((input.viewport_w, input.viewport_h));
            self.anim.active_border_zoomed = input.zoomed_pane.clone();
            self.anim.active_border_rect.map(|r| r.current())
        } else {
            self.anim.active_border_rect = None;
            self.anim.active_border_block = None;
            self.anim.active_border_viewport = Some((input.viewport_w, input.viewport_h));
            self.anim.active_border_zoomed = input.zoomed_pane.clone();
            None
        };

        if self.viewer.open {
            self.anim.closing_viewer = None;
        }
        let interaction = self.interaction();
        let show_sidebar_expand = cfg.toolbar_position.is_sidebar()
            && chrome.sidebar.is_none()
            && cfg.toolbar_behavior != ToolbarBehavior::Hidden;

        if !terminal_workspace.is_empty() {
            render_workspace_chrome(
                &mut draw_list,
                terminal_workspace,
                &tiled_panes,
                &dividers,
                &floating_panes,
                &capsules,
                &input.pane_metas,
                self.pane_options,
                input.dnd_overlay,
                show_sidebar_expand,
                spring_active_border,
                interaction,
                theme,
                anim_phase,
            );
        }

        // 2c. Docked right-hand tabbed Viewer Pane (with slide/fade open & non-blocking close shadow)
        let viewer_focused = matches!(
            self.focus.current(),
            FocusTarget::Viewer | FocusTarget::ViewerSearch
        );
        let viewer_out = if self.viewer.open {
            if let Some(target_vs) = target_viewer_split {
                if vp >= 0.995 {
                    render_viewer_with_measurer(
                        &mut draw_list,
                        target_vs,
                        &self.viewer,
                        viewer_focused,
                        self.pane_options.highlight_active_border,
                        self.pane_options.active_border_opacity,
                        interaction,
                        theme,
                        anim_phase,
                        measurer,
                    )
                } else {
                    draw_list.push_clip(chrome.workspace);
                    let start_p = draw_list.primitives.len();
                    let out = render_viewer_with_measurer(
                        &mut draw_list,
                        target_vs,
                        &self.viewer,
                        viewer_focused,
                        self.pane_options.highlight_active_border,
                        self.pane_options.active_border_opacity,
                        interaction,
                        theme,
                        anim_phase,
                        measurer,
                    );
                    let dx = (1.0 - vp) * target_vs.viewer_rect.w;
                    draw_list.apply_transition(start_p, dx, 0.0, vp.max(0.05));
                    draw_list.pop_clip();
                    out
                }
            } else {
                ViewerRenderOutput::default()
            }
        } else if let Some((ref closing_vs, ref closing_state, ref closing_sp)) =
            self.anim.closing_viewer
        {
            let p = closing_sp.value.clamp(0.0, 1.0);
            if p > 0.005 {
                draw_list.push_clip(chrome.workspace);
                let start_p = draw_list.primitives.len();
                let start_h = draw_list.hits.len();
                let _ = render_viewer_with_measurer(
                    &mut draw_list,
                    *closing_vs,
                    closing_state,
                    false,
                    self.pane_options.highlight_active_border,
                    self.pane_options.active_border_opacity,
                    interaction,
                    theme,
                    anim_phase,
                    measurer,
                );
                draw_list.hits.truncate(start_h);
                let dx = (1.0 - p) * closing_vs.viewer_rect.w;
                draw_list.apply_transition(start_p, dx, 0.0, p);
                draw_list.pop_clip();
            }
            ViewerRenderOutput::default()
        } else {
            ViewerRenderOutput::default()
        };

        // 3. Sidebar or horizontal Tab Bar (with smooth slide/fade)
        let sidebar_scroll = if let Some(sb_rect) = chrome.sidebar {
            if sp >= 0.995 {
                render_sidebar(
                    &mut draw_list,
                    sb_rect,
                    chrome.sidebar_resize_handle,
                    cfg.toolbar_position == ToolbarPosition::Right,
                    chrome.toolbar_is_overlay,
                    &input.hosts,
                    &self.sidebar,
                    self.focus.current(),
                    interaction,
                    theme,
                    anim_phase,
                    self.tab_reorder_overlay.as_ref(),
                )
            } else if sp > 0.005 {
                let target_w = cfg.responsive_sidebar_width(input.viewport_w);
                let full_rect = chrome_target.sidebar.unwrap_or_else(|| {
                    if cfg.toolbar_position == ToolbarPosition::Right {
                        RectF::new(
                            (input.viewport_w - target_w).max(0.0),
                            0.0,
                            target_w,
                            input.viewport_h,
                        )
                    } else {
                        RectF::new(0.0, 0.0, target_w, input.viewport_h)
                    }
                });
                let dx = if cfg.toolbar_position == ToolbarPosition::Right {
                    (1.0 - sp) * full_rect.w
                } else {
                    -(1.0 - sp) * full_rect.w
                };
                draw_list.push_clip(if chrome.toolbar_is_overlay {
                    chrome.viewport
                } else {
                    sb_rect
                });
                let start_p = draw_list.primitives.len();
                let start_h = draw_list.hits.len();
                let scroll = render_sidebar(
                    &mut draw_list,
                    full_rect,
                    chrome.sidebar_resize_handle,
                    cfg.toolbar_position == ToolbarPosition::Right,
                    chrome.toolbar_is_overlay,
                    &input.hosts,
                    &self.sidebar,
                    self.focus.current(),
                    interaction,
                    theme,
                    anim_phase,
                    self.tab_reorder_overlay.as_ref(),
                );
                if !sidebar_target_visible {
                    draw_list.hits.truncate(start_h);
                }
                draw_list.apply_transition(start_p, dx, 0.0, sp.max(0.05));
                draw_list.pop_clip();
                scroll
            } else {
                SidebarScrollMetrics::default()
            }
        } else if !sidebar_target_visible && sp > 0.005 && self.anim.last_sidebar_is_overlay {
            if let Some(prev_sb) = self.anim.last_sidebar_rect {
                let dx = if self.anim.last_sidebar_is_right {
                    (1.0 - sp) * prev_sb.w
                } else {
                    -(1.0 - sp) * prev_sb.w
                };
                draw_list.push_clip(chrome.viewport);
                let start_p = draw_list.primitives.len();
                let start_h = draw_list.hits.len();
                let _ = render_sidebar(
                    &mut draw_list,
                    prev_sb,
                    None,
                    self.anim.last_sidebar_is_right,
                    true,
                    &input.hosts,
                    &self.sidebar,
                    self.focus.current(),
                    interaction,
                    theme,
                    anim_phase,
                    None,
                );
                draw_list.hits.truncate(start_h);
                draw_list.apply_transition(start_p, dx, 0.0, sp);
                draw_list.pop_clip();
            }
            SidebarScrollMetrics::default()
        } else {
            if let Some(tb_rect) = chrome.tab_bar {
                let active_tabs: &[SidebarTabView] = input
                    .hosts
                    .iter()
                    .flat_map(|h| h.sessions.iter())
                    .find(|s| s.session_id == input.active_session_id)
                    .map_or(&[], |s| s.tabs.as_slice());

                let other_session_attn = input.hosts.iter().any(|h| {
                    h.sessions
                        .iter()
                        .any(|s| s.session_id != input.active_session_id && s.attention_count() > 0)
                });
                let any_attn = input
                    .hosts
                    .iter()
                    .any(|h| h.sessions.iter().any(|s| s.attention_count() > 0));

                let tb_view = TabBarView {
                    host_id: &input.active_host_id,
                    session_id: &input.active_session_id,
                    session_label: &input.active_session_label,
                    other_session_needs_attention: other_session_attn,
                    any_activity_attention: any_attn,
                    activity_mode: self.sidebar.activity_mode,
                    fill_available_space: input.fill_horizontal_tabs,
                    icon_color_policy: self.sidebar.icon_color_policy,
                    hovered_tab_window_id: self.sidebar.hovered_tab_window_id.as_deref(),
                    tabs: active_tabs,
                    scroll_offset_x: self.tabbar_scroll_x,
                    is_bottom: cfg.toolbar_position == ToolbarPosition::Bottom,
                    is_overlay: chrome.toolbar_is_overlay,
                    tab_reorder_overlay: self.tab_reorder_overlay.as_ref(),
                };
                render_tab_bar(
                    &mut draw_list,
                    tb_rect,
                    &tb_view,
                    interaction,
                    theme,
                    anim_phase,
                );
            }
            SidebarScrollMetrics::default()
        };

        // 4. Inline Find bar inside focused pane (or workspace fallback)
        if self.find_bar.open {
            let target_rect = tiled_panes
                .iter()
                .find(|p| Some(p.id.as_str()) == self.focus.last_terminal_block())
                .map_or(chrome.workspace, |p| p.terminal_rect);
            render_find_bar(
                &mut draw_list,
                target_rect,
                &self.find_bar,
                interaction,
                theme,
            );
        }

        // 5. Which-key chord/mode hint popup
        if let Some(wk) = &self.which_key {
            render_which_key(&mut draw_list, chrome.workspace, wk, theme);
        }

        // 6. Floating Toast notifications
        if !self.toasts.is_empty() {
            render_toasts(
                &mut draw_list,
                chrome.workspace,
                self.toast_anchor,
                &self.toasts,
                interaction,
                theme,
            );
        }

        // 7. File-path / image hover preview card
        let hover_image_slot = self
            .hover_preview
            .as_ref()
            .and_then(|p| render_hover_preview(&mut draw_list, chrome.viewport, p, theme));

        // 8. Active centered Modal (Command Palette, Settings, `rex.ui` prompt, etc.)
        if let Some(modal) = &self.modal {
            let mp = self.anim.modal_open_progress.value.clamp(0.05, 1.0);
            let start_p = draw_list.primitives.len();
            render_modal(
                &mut draw_list,
                chrome.viewport,
                modal,
                interaction,
                theme,
                now,
            );
            if mp < 0.995 {
                draw_list.apply_transition(start_p, 0.0, (1.0 - mp) * 10.0, mp);
            }
        } else if let Some((ref closing_modal, ref closing_sp)) = self.anim.closing_modal {
            let p = closing_sp.value.clamp(0.0, 1.0);
            if p > 0.005 {
                let start_p = draw_list.primitives.len();
                let start_h = draw_list.hits.len();
                render_modal(
                    &mut draw_list,
                    chrome.viewport,
                    closing_modal,
                    interaction,
                    theme,
                    now,
                );
                draw_list.hits.truncate(start_h);
                draw_list.apply_transition(start_p, 0.0, (1.0 - p) * 10.0, p);
            }
        }

        // 9. Topmost Right-Click Context Menu
        if let Some(menu) = &self.context_menu {
            render_context_menu(&mut draw_list, chrome.viewport, menu, interaction, theme);
        }

        // 10. Crisp hover tooltip pill for icon-only buttons (never overlaps open context menu or previews)
        if self.context_menu.is_none()
            && self.hover_preview.is_none()
            && self.pressed_hit.is_none()
            && let Some(hit) = self.hovered_hit.as_ref()
            && let Some(tip) = hit.tooltip_text()
            && let Some(anchor) = self.hovered_hit_rect
        {
            render_tooltip(&mut draw_list, chrome.viewport, anchor, tip, theme);
        }

        UiRenderOutput {
            chrome,
            sidebar_scroll,
            tiled_panes,
            dividers,
            floating_panes,
            hover_image_slot,
            viewer_split,
            viewer_images: viewer_out.images,
            viewer_metrics: viewer_out.metrics,
            draw_list,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tr3x_bridge::prompts::{PromptItem, PromptRequest, PromptSpec};
    use tr3x_bridge::t3_session::{
        BlockedKind, ProgramState, ProgramStatusRecord, build_tab_card, rollup_records,
    };

    fn sample_t3_23_window_input() -> UiFrameInput {
        let mut tabs = Vec::with_capacity(23);
        for i in 0..23 {
            let status_rec = match i {
                0 => Some(ProgramStatusRecord {
                    id: String::new(),
                    state: ProgramState::Blocked,
                    kind: Some(BlockedKind::Permission),
                    progress: None,
                    app: Some("claude".into()),
                    effective_app: None,
                    title: None,
                    msg: Some("Needs you".into()),
                    unseen: true,
                }),
                1 => Some(ProgramStatusRecord {
                    id: String::new(),
                    state: ProgramState::Error,
                    kind: None,
                    progress: None,
                    app: Some("codex".into()),
                    effective_app: None,
                    title: None,
                    msg: Some("Failed".into()),
                    unseen: true,
                }),
                2..=5 => Some(ProgramStatusRecord {
                    id: String::new(),
                    state: ProgramState::Working,
                    kind: None,
                    progress: if i == 3 { Some(45) } else { None },
                    app: Some(if i % 2 == 0 { "codex" } else { "gemini" }.into()),
                    effective_app: None,
                    title: None,
                    msg: Some("Working".into()),
                    unseen: false,
                }),
                _ => Some(ProgramStatusRecord {
                    id: String::new(),
                    state: ProgramState::Done,
                    kind: None,
                    progress: None,
                    app: Some("claude".into()),
                    effective_app: None,
                    title: None,
                    msg: Some("Done".into()),
                    unseen: i == 6,
                }),
            };
            let rolled = status_rec
                .as_ref()
                .and_then(|r| rollup_records(std::slice::from_ref(r)));
            let title = if i == 4 {
                format!("↳ Subagent worker #{i}")
            } else {
                format!("Thread #{i:02} — Omarex T3 task")
            };
            let card = build_tab_card(
                &title,
                Some("omarex"),
                None,
                rolled.as_ref(),
                Some((i as u64) * 180),
            );
            tabs.push(SidebarTabView {
                window_id: format!("win:{i:02}"),
                card,
                active: i == 0,
                muted: false,
                unread: i == 6,
                pane_count: 1,
                icon_override: None,
            });
        }

        let host = SidebarHostView {
            host_id: "local".into(),
            label: "This Machine".into(),
            is_local: true,
            connection: HostConnectionState::Online,
            collapsed: false,
            sessions: vec![SidebarSessionView {
                session_id: "sess:t3".into(),
                label: "T3 Code".into(),
                active: true,
                collapsed: false,
                muted: false,
                tabs,
            }],
        };

        UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: vec![host],
            active_host_id: "local".into(),
            active_session_id: "sess:t3".into(),
            active_session_label: "T3 Code".into(),
            split_root: Some(SplitNode::leaf("blk:00".into())),
            zoomed_pane: None,
            hovered_header_pane: Some("blk:00".into()),
            floating_layers: vec![FloatingLayerSpec::tr3x_pr_drawer("blk:pr".into(), 160)],
            pane_metas: vec![
                PaneMeta {
                    block_id: "blk:00".into(),
                    label: "Thread #00".into(),
                    cwd: Some("~/Work/omarex".into()),
                    icon: IconId::Claude,
                    focused: true,
                    hovered: true,
                    bell_unread: false,
                    program_status: None,
                    osc94_progress: Some(Osc94Progress {
                        state: Osc94State::Normal,
                        percent: 65,
                    }),
                    scrollbar: ScrollbarMetrics {
                        total_rows: 500,
                        viewport_rows: 40,
                        offset_rows: 200,
                        visible: true,
                        hovered: false,
                    },
                    program_bg: None,
                    has_unread_output: false,
                    copy_mode_badge: None,
                    listening_ports: Vec::new(),
                    is_pr_drawer: false,
                    pr_subtitle: None,
                },
                PaneMeta {
                    block_id: "blk:pr".into(),
                    label: "Pull Request".into(),
                    cwd: None,
                    icon: IconId::Branch,
                    focused: false,
                    hovered: false,
                    bell_unread: false,
                    program_status: None,
                    osc94_progress: None,
                    scrollbar: ScrollbarMetrics::default(),
                    program_bg: None,
                    has_unread_output: false,
                    copy_mode_badge: None,
                    listening_ports: Vec::new(),
                    is_pr_drawer: true,
                    pr_subtitle: Some("#177 · feat/omarex".into()),
                },
            ],
            dnd_overlay: None,
            fill_horizontal_tabs: true,
        }
    }

    #[test]
    fn focus_guard_never_steals_focus_from_active_overlays_on_view_change() {
        let mut tree = UiTree::new();
        tree.on_session_view_changed(Some("blk:1".into()));
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Terminal(Some("blk:1".into()))
        );

        // Open Command Palette -> background session_view_changed must NOT steal focus!
        tree.open_modal(ModalState::open_list(
            ModalKind::CommandPalette,
            "Command Palette",
            "Type a command…",
            Some("Ctrl+Shift+P".into()),
            vec![PaletteItem::new("t3_inbox", "Go to Thread That Needs You")],
        ));
        tree.on_session_view_changed(Some("blk:99".into()));
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Modal(ModalKind::CommandPalette)
        );
        assert_eq!(tree.focus.last_terminal_block(), Some("blk:99"));

        // Closing modal restores focus to the updated terminal block
        tree.close_modal();
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Terminal(Some("blk:99".into()))
        );

        // Same non-theft guarantee for FindBar, SidebarFilter, and ContextMenu
        tree.open_find_bar();
        tree.on_session_view_changed(Some("blk:100".into()));
        assert_eq!(tree.focus.current(), &FocusTarget::FindBar);
        tree.close_find_bar();
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Terminal(Some("blk:100".into()))
        );

        tree.open_context_menu(ContextMenuState::for_pane(
            200.0, 200.0, "blk:100", false, false,
        ));
        tree.on_session_view_changed(Some("blk:101".into()));
        assert_eq!(tree.focus.current(), &FocusTarget::ContextMenu);
        tree.close_context_menu();
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Terminal(Some("blk:101".into()))
        );
    }

    #[test]
    fn renders_23_window_t3_sidebar_preserving_exact_order_and_hit_targets() {
        let mut tree = UiTree::new();
        tree.pane_options.is_t3_session = true;
        let input = sample_t3_23_window_input();
        let theme = Theme::default();

        let out = tree.render(&input, &theme, Instant::now(), 0.35);
        out.draw_list
            .validate_invariants()
            .expect("all rects finite/non-negative & clips balanced");

        // Extract visible TabCard hit targets in rendered Y order and verify exact server order!
        let mut tab_hits: Vec<(f32, &str)> = out
            .draw_list
            .hits
            .iter()
            .filter_map(|h| match &h.target {
                HitTarget::TabCard { window_id, .. } => Some((h.rect.y, window_id.as_str())),
                _ => None,
            })
            .collect();
        tab_hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert!(
            tab_hits.len() >= 14,
            "expected at least 14 visible tabs on 900px height"
        );
        for (idx, (_, win_id)) in tab_hits.iter().enumerate() {
            assert_eq!(
                *win_id,
                format!("win:{idx:02}"),
                "tab order must match server order"
            );
        }

        // Click on the 3rd tab card (`win:02`) -> produces exact `SelectTab` action
        let tab2 = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(&h.target, HitTarget::TabCard { window_id, .. } if window_id == "win:02"))
            .expect("win:02 hit target");
        let (cx, cy) = tab2.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, cx, cy, false),
            UiAction::SelectTab {
                host_id: "local".into(),
                session_id: "sess:t3".into(),
                window_id: "win:02".into(),
            }
        );

        // Click on the `/pr` floating drawer header close button -> produces `PaneButton::Close`
        let pr_close = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::PaneButton {
                        block_id,
                        button: PaneHeaderButton::Close
                    } if block_id == "blk:pr"
                )
            })
            .expect("PR drawer close button hit target");
        let (pcx, pcy) = pr_close.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, pcx, pcy, false),
            UiAction::PaneButton {
                block_id: "blk:pr".into(),
                button: PaneHeaderButton::Close,
            }
        );
    }

    #[test]
    fn rex_ui_pick_modal_supports_multi_select_navigation_and_timeout_bar() {
        let now = Instant::now();
        let req = PromptRequest {
            call_id: "call-42".into(),
            action: Some("stage_files".into()),
            prompt: PromptSpec {
                kind: "pick".into(),
                title: "Stage Files".into(),
                placeholder: Some("Filter changed files".into()),
                value: None,
                confirm: Some("Stage".into()),
                multiple: true,
                allow_other: false,
                items: Some(vec![
                    PromptItem {
                        id: "README.md".into(),
                        title: "README.md".into(),
                        subtitle: None,
                        icon: Some("document".into()),
                        detail: Some("M".into()),
                        selected: true,
                    },
                    PromptItem {
                        id: "crates/ui-chrome/src/lib.rs".into(),
                        title: "lib.rs".into(),
                        subtitle: Some("crates/ui-chrome/src".into()),
                        icon: Some("code".into()),
                        detail: Some("??".into()),
                        selected: false,
                    },
                ]),
            },
            timeout_ms: 60_000,
        };

        let session = PromptSession::from_request(req, now).expect("valid prompt");
        let mut tree = UiTree::new();
        tree.open_rex_prompt(session);
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Modal(ModalKind::RexPrompt)
        );

        // Render and verify timeout ProgressBar primitive + valid invariants
        let input = sample_t3_23_window_input();
        let theme = Theme::default();
        let out = tree.render(&input, &theme, now, 0.0);
        out.draw_list
            .validate_invariants()
            .expect("modal draw invariants");
        assert!(
            out.draw_list
                .primitives
                .iter()
                .any(|p| matches!(p, UiPrimitive::ProgressBar { .. })),
            "rex.ui prompt must render timeout countdown bar"
        );

        // Move down to row 1 (`lib.rs`), toggle with Space, then confirm with Ctrl+Enter
        assert_eq!(tree.handle_modal_key(ModalKey::Down), UiAction::None);
        assert_eq!(tree.handle_modal_key(ModalKey::Space), UiAction::None);
        let action = tree.handle_modal_key(ModalKey::CtrlEnter);
        assert!(tree.modal.is_none());
        match action {
            UiAction::ReplyRexPrompt(reply) => {
                assert_eq!(reply.call_id, "call-42");
                assert!(reply.is_unordered());
                let ids: Vec<&str> = reply
                    .result
                    .as_ref()
                    .and_then(|v| v.get("ids"))
                    .and_then(|v| v.as_array())
                    .expect("ids array")
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect();
                assert_eq!(ids, vec!["README.md", "crates/ui-chrome/src/lib.rs"]);
            }
            other => panic!("expected ReplyRexPrompt, got {other:?}"),
        }
    }

    #[test]
    fn right_click_context_menu_clamps_to_viewport_and_executes_actions() {
        let mut tree = UiTree::new();
        let input = sample_t3_23_window_input();
        let theme = Theme::default();
        let first = tree.render(&input, &theme, Instant::now(), 0.0);

        // Right-click near bottom-right of pane surface
        let pane_hit = first
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(&h.target, HitTarget::PaneSurface { block_id } if block_id == "blk:00"))
            .unwrap();
        tree.handle_click(
            &first.draw_list,
            pane_hit.rect.x + 40.0,
            pane_hit.rect.y + 40.0,
            true,
        );
        assert!(tree.context_menu.is_some());
        assert_eq!(tree.focus.current(), &FocusTarget::ContextMenu);

        // Move anchor near bottom-right viewport corner to test clamping
        tree.context_menu.as_mut().unwrap().anchor_x = 1430.0;
        tree.context_menu.as_mut().unwrap().anchor_y = 890.0;
        let rendered = tree.render(&input, &theme, Instant::now(), 0.0);
        rendered
            .draw_list
            .validate_invariants()
            .expect("context menu valid");

        // All menu item hit rects must stay inside viewport!
        let first_item = rendered
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::ContextMenuItem { index: 0 }))
            .expect("first context menu item");
        assert!(first_item.rect.right() <= 1440.0);
        assert!(first_item.rect.bottom() <= 900.0);

        // Clicking item 0 ("Copy") executes CopySelection and closes the menu
        let (ix, iy) = first_item.rect.center();
        assert_eq!(
            tree.handle_click(&rendered.draw_list, ix, iy, false),
            UiAction::CopySelection {
                block_id: Some("blk:00".into()),
            }
        );
        assert!(tree.context_menu.is_none());
        assert!(matches!(tree.focus.current(), FocusTarget::Terminal(_)));
    }

    #[test]
    fn all_four_toolbar_layouts_and_overlays_render_cleanly() {
        let mut tree = UiTree::new();
        tree.open_find_bar();
        tree.which_key = Some(WhichKeyState {
            header: "MODE: RESIZE".into(),
            exclusive: true,
            hints: vec![
                tr3x_bridge::keymap::WhichKeyHint {
                    next_key: tr3x_bridge::keymap::KeyChord::parse("left").unwrap(),
                    display_key: "Left".into(),
                    action: Some("pane.resize".into()),
                    is_prefix: false,
                },
                tr3x_bridge::keymap::WhichKeyHint {
                    next_key: tr3x_bridge::keymap::KeyChord::parse("=").unwrap(),
                    display_key: "=".into(),
                    action: Some("pane.balance".into()),
                    is_prefix: false,
                },
            ],
        });
        tree.toasts.push(ToastItem {
            id: 1,
            severity: ToastSeverity::Warning,
            title: "T3 Code · Hotkey chips".into(),
            body: "Waiting for permission to run cargo test".into(),
        });

        let input = sample_t3_23_window_input();
        let theme = Theme::default();

        for pos in [
            ToolbarPosition::Left,
            ToolbarPosition::Right,
            ToolbarPosition::Top,
            ToolbarPosition::Bottom,
        ] {
            tree.layout_cfg.toolbar_position = pos;
            let out = tree.render(&input, &theme, Instant::now(), 0.5);
            out.draw_list
                .validate_invariants()
                .unwrap_or_else(|e| panic!("layout {pos:?} failed invariants: {e}"));
            if pos.is_sidebar() {
                assert!(out.chrome.sidebar.is_some());
                assert!(out.chrome.tab_bar.is_none());
            } else {
                assert!(out.chrome.tab_bar.is_some());
                assert!(out.chrome.sidebar.is_none());
            }
        }
    }

    #[test]
    fn rex_ui_input_preserves_whitespace_and_allow_other_pick_submits_custom_value() {
        let now = Instant::now();

        // 1. `rex.ui.input` preserves exact leading/trailing spaces
        let input_req = PromptRequest {
            call_id: "call-input-1".into(),
            action: Some("tr3x.reply".into()),
            prompt: PromptSpec {
                kind: "input".into(),
                title: "Reply to Thread".into(),
                placeholder: Some("Message...".into()),
                value: Some("  keep spaces  ".into()),
                confirm: Some("Send".into()),
                multiple: false,
                allow_other: false,
                items: None,
            },
            timeout_ms: 30_000,
        };
        let mut tree = UiTree::new();
        tree.open_rex_prompt(PromptSession::from_request(input_req, now).unwrap());
        let theme = Theme::default();
        let frame = sample_t3_23_window_input();
        let out = tree.render(&frame, &theme, now, 0.0);
        out.draw_list
            .validate_invariants()
            .expect("input prompt invariants");

        match tree.handle_modal_key(ModalKey::Enter) {
            UiAction::ReplyRexPrompt(reply) => {
                assert_eq!(reply.call_id, "call-input-1");
                assert!(reply.is_unordered());
                assert_eq!(
                    reply
                        .result
                        .as_ref()
                        .and_then(|v| v.get("text"))
                        .and_then(|v| v.as_str()),
                    Some("  keep spaces  ")
                );
            }
            other => panic!("expected ReplyRexPrompt, got {other:?}"),
        }

        // 2. `rex.ui.pick` with `allow_other: true` submits unmatched custom text
        let pick_req = PromptRequest {
            call_id: "call-pick-other".into(),
            action: Some("select_branch".into()),
            prompt: PromptSpec {
                kind: "pick".into(),
                title: "Select or Create Branch".into(),
                placeholder: Some("Type branch name".into()),
                value: None,
                confirm: Some("Checkout".into()),
                multiple: false,
                allow_other: true,
                items: Some(vec![PromptItem {
                    id: "main".into(),
                    title: "main".into(),
                    subtitle: None,
                    icon: Some("branch".into()),
                    detail: None,
                    selected: false,
                }]),
            },
            timeout_ms: 30_000,
        };
        tree.open_rex_prompt(PromptSession::from_request(pick_req, now).unwrap());
        for ch in "feat/omarex-ui".chars() {
            assert_eq!(tree.handle_modal_key(ModalKey::Char(ch)), UiAction::None);
        }
        let out2 = tree.render(&frame, &theme, now, 0.0);
        out2.draw_list
            .validate_invariants()
            .expect("allow_other prompt invariants");
        match tree.handle_modal_key(ModalKey::Enter) {
            UiAction::ReplyRexPrompt(reply) => {
                assert_eq!(reply.call_id, "call-pick-other");
                assert!(reply.is_unordered());
                assert_eq!(
                    reply
                        .result
                        .as_ref()
                        .and_then(|v| v.get("text"))
                        .and_then(|v| v.as_str()),
                    Some("feat/omarex-ui")
                );
            }
            other => panic!("expected allow_other ReplyRexPrompt, got {other:?}"),
        }
    }

    #[test]
    fn settings_modal_cycles_options_and_stays_open() {
        let mut tree = UiTree::new();
        let snap = SettingsViewSnapshot::from_config(
            &tree.layout_cfg,
            omarchy_theme::MaterialStyle::default(),
            true,
            true,
            true,
        );
        tree.open_modal(ModalState::open_settings(snap));
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Modal(ModalKind::Settings)
        );

        let frame = sample_t3_23_window_input();
        let theme = Theme::default();
        let out = tree.render(&frame, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("settings modal invariants");

        // Pressing Enter on row 0 (Toolbar Position) emits UpdateSetting and keeps Settings open
        let action = tree.handle_modal_key(ModalKey::Enter);
        assert!(matches!(
            action,
            UiAction::UpdateSetting(SettingChange::ToolbarPosition(_))
        ));
        assert!(tree.modal.is_some());
    }

    #[test]
    fn hover_image_preview_returns_gpu_slot_and_empty_state_has_new_tab_button() {
        let mut tree = UiTree::new();
        tree.hover_preview = Some(HoverPreviewState::ImageCard {
            caption: "hero.png · 1920×1080".into(),
            image_width: 1920,
            image_height: 1080,
            anchor_x: 1380.0,
            anchor_y: 840.0,
        });

        let mut empty_input = sample_t3_23_window_input();
        empty_input.split_root = None;
        empty_input.floating_layers.clear();

        let theme = Theme::default();
        let out = tree.render(&empty_input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("empty state + hover image invariants");

        let slot = out
            .hover_image_slot
            .expect("ImageCard preview must return GPU texture slot");
        assert!(slot.w > 0.0 && slot.h > 0.0);
        assert!(slot.right() <= 1440.0 && slot.bottom() <= 900.0);

        // Empty state primary CTA button inside normal workspace triggers EmptyStateNewTerminal -> NewTerminal
        let empty_cta = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(h.target, HitTarget::EmptyStateNewTerminal)
                    && out
                        .chrome
                        .workspace
                        .contains(h.rect.center().0, h.rect.center().1)
            })
            .expect("empty state New Terminal CTA hit target");
        let (cx, cy) = empty_cta.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, cx, cy, false),
            UiAction::NewTerminal
        );

        // In a T3 session empty state, BOTH `New T3 Thread` and `New Terminal` buttons are offered!
        tree.pane_options.is_t3_session = true;
        let t3_out = tree.render(&empty_input, &theme, Instant::now(), 0.0);
        let t3_cta = t3_out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::EmptyStateNewTab))
            .expect("T3 empty state New T3 Thread button");
        let (tx, ty) = t3_cta.rect.center();
        assert_eq!(
            tree.handle_click(&t3_out.draw_list, tx, ty, false),
            UiAction::RunCommand {
                action_id: "t3_new".into(),
            }
        );
        let t3_term_cta = t3_out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::EmptyStateNewTerminal))
            .expect("T3 empty state secondary New Terminal button");
        let (nx, ny) = t3_term_cta.rect.center();
        assert_eq!(
            tree.handle_click(&t3_out.draw_list, nx, ny, false),
            UiAction::NewTerminal
        );
    }

    #[test]
    fn top_tab_bar_exposes_session_switcher_tab_cards_and_close_buttons() {
        let mut tree = UiTree::new();
        tree.layout_cfg.toolbar_position = ToolbarPosition::Top;
        let input = sample_t3_23_window_input();
        let theme = Theme::default();
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("top tab bar invariants");

        let tab_bar = out.chrome.tab_bar.expect("top tab bar rect");
        assert!(tab_bar.h >= 36.0);

        let switcher = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::SessionSwitcherPill))
            .expect("session switcher pill hit");
        let (sx, sy) = switcher.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, sx, sy, false),
            UiAction::OpenModal(ModalKind::SwitchSession)
        );

        let close_btn = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(&h.target, HitTarget::TabClose { window_id, .. } if window_id == "win:00"))
            .expect("active tab close button hit");
        let (cx, cy) = close_btn.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, cx, cy, false),
            UiAction::CloseTab {
                host_id: "local".into(),
                session_id: "sess:t3".into(),
                window_id: "win:00".into(),
            }
        );
    }

    #[test]
    fn sidebar_auto_scrolls_active_tab_and_clamps_manual_wheel_scroll() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        // Activate the bottom-most tab (index 22, scrolled off a 900px window).
        for (idx, tab) in input.hosts[0].sessions[0].tabs.iter_mut().enumerate() {
            tab.active = idx == 22;
        }
        let theme = Theme {
            rounding: 0.0, // Simulate Omarchy Hyprland outer window rounding = 0.
            ..Theme::default()
        };

        tree.sync_sidebar_scroll(&input);
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        assert!(out.sidebar_scroll.max_scroll > 200.0);
        assert!(
            tree.sidebar.scroll_offset_y > 0.0,
            "active bottom tab must auto-scroll sidebar into view"
        );
        let (top, bottom) = out
            .sidebar_scroll
            .active_tab_span
            .expect("active tab span must be recorded");
        assert!(top >= tree.sidebar.scroll_offset_y);
        assert!(bottom <= tree.sidebar.scroll_offset_y + out.sidebar_scroll.viewport_height);

        // Manual wheel scrolling clamps strictly to [0, max_scroll] and does NOT snap back
        // while the active tab remains unchanged.
        tree.sidebar
            .scroll_by(10_000.0, out.sidebar_scroll.max_scroll);
        assert_eq!(tree.sidebar.scroll_offset_y, 0.0);
        tree.sync_sidebar_scroll(&input);
        assert_eq!(
            tree.sidebar.scroll_offset_y, 0.0,
            "sync must not fight user wheel scroll while active tab is unchanged"
        );

        tree.sidebar
            .scroll_by(-99_999.0, out.sidebar_scroll.max_scroll);
        assert!((tree.sidebar.scroll_offset_y - out.sidebar_scroll.max_scroll).abs() < 0.01);

        // Switching active tab back to index 0 auto-scrolls back to top.
        for (idx, tab) in input.hosts[0].sessions[0].tabs.iter_mut().enumerate() {
            tab.active = idx == 0;
        }
        tree.sync_sidebar_scroll(&input);
        assert!(
            tree.sidebar.scroll_offset_y < 60.0,
            "switching to top tab should scroll back up"
        );
    }

    #[test]
    fn comfortable_mode_keeps_rounded_cards_and_header_when_hyprland_rounding_is_zero() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        input.split_root = Some(SplitNode::leaf("blk:0".to_owned()));
        let theme = Theme {
            rounding: 0.0,
            ..Theme::default()
        };

        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        assert_eq!(out.tiled_panes.len(), 1);
        assert_eq!(out.tiled_panes[0].corner_radius, 8.0);
        assert_eq!(out.tiled_panes[0].header_rect.map(|r| r.h), Some(30.0));

        // Compact density switches cleanly back to 0px sharp corners.
        tree.layout_cfg.density = PaneDensity::Compact;
        let compact = tree.render(&input, &theme, Instant::now(), 0.0);
        assert_eq!(compact.tiled_panes[0].corner_radius, 0.0);
    }

    #[test]
    fn t3_session_collapse_click_collapsed_badge_and_context_menu_work() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        // Add a second normal terminal session beneath T3 Code to test both active and inactive collapse.
        input.hosts[0].sessions.push(SidebarSessionView {
            session_id: "sess:term".into(),
            label: "Terminal".into(),
            active: false,
            collapsed: false,
            muted: false,
            tabs: vec![SidebarTabView {
                window_id: "win:sh0".into(),
                card: build_tab_card("zsh", Some("omarex"), Some("zsh"), None, Some(10)),
                active: false,
                muted: false,
                unread: false,
                pane_count: 2,
                icon_override: None,
            }],
        });

        let theme = Theme::default();
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("valid draw list");

        // 1. Clicking T3 Code chevron toggles collapse cleanly
        let t3_chev = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::SessionChevron { session_id, .. } if session_id == "sess:t3"
                )
            })
            .expect("T3 Code chevron hit target");
        let (cx, cy) = t3_chev.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, cx, cy, false),
            UiAction::ToggleSessionCollapse {
                host_id: "local".into(),
                session_id: "sess:t3".into(),
            }
        );

        // 2. Right-clicking T3 Code opens context menu with `New T3 Thread` + `Collapse Session`
        tree.handle_click(&out.draw_list, cx, cy, true);
        let menu = tree.context_menu.as_ref().expect("session context menu");
        assert!(
            menu.items.iter().any(|i| i.label == "Collapse Session"
                && matches!(
                    &i.action,
                    UiAction::ToggleSessionCollapse { session_id, .. } if session_id == "sess:t3"
                )),
            "session context menu must include Collapse Session"
        );
        assert!(
            menu.items.iter().any(|i| i.label == "New T3 Thread"),
            "T3 session context menu must offer New T3 Thread"
        );
        tree.close_context_menu();

        // 3. When T3 Code is collapsed, zero T3 TabCards render and a compact `23` count pill appears
        input.hosts[0].sessions[0].collapsed = true;
        let collapsed_out = tree.render(&input, &theme, Instant::now(), 0.0);
        collapsed_out
            .draw_list
            .validate_invariants()
            .expect("collapsed draw invariants");
        assert!(
            !collapsed_out.draw_list.hits.iter().any(|h| matches!(
                &h.target,
                HitTarget::TabCard { session_id, .. } if session_id == "sess:t3"
            )),
            "collapsed T3 Code must hide all 23 thread cards"
        );
        assert!(
            collapsed_out.draw_list.primitives.iter().any(|p| matches!(
                p,
                UiPrimitive::Text { text, .. } if text == "23"
            )),
            "collapsed T3 Code row must render compact tab count badge `23`"
        );

        // 4. With T3 collapsed, the second normal `Terminal` session is immediately visible and
        //    its chevron toggles collapse while its label selects the session.
        let term_chev = collapsed_out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::SessionChevron { session_id, .. } if session_id == "sess:term"
                )
            })
            .expect("inactive Terminal session chevron");
        let (tcx, tcy) = term_chev.rect.center();
        assert_eq!(
            tree.handle_click(&collapsed_out.draw_list, tcx, tcy, false),
            UiAction::ToggleSessionCollapse {
                host_id: "local".into(),
                session_id: "sess:term".into(),
            }
        );

        let term_row = collapsed_out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::SessionRow { session_id, .. } if session_id == "sess:term"
                )
            })
            .expect("inactive Terminal session row");
        // Click center of label area (away from left chevron and right buttons)
        assert_eq!(
            tree.handle_click(
                &collapsed_out.draw_list,
                term_row.rect.x + term_row.rect.w * 0.45,
                term_row.rect.center().1,
                false,
            ),
            UiAction::SelectSession {
                host_id: "local".into(),
                session_id: "sess:term".into(),
            }
        );
    }

    #[test]
    fn rex_plus_buttons_distinguish_new_terminal_new_session_and_t3_thread() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        // Collapse T3 so both sessions fit cleanly without scrolling
        input.hosts[0].sessions[0].collapsed = true;
        input.hosts[0].sessions.push(SidebarSessionView {
            session_id: "sess:work".into(),
            label: "work".into(),
            active: false,
            collapsed: false,
            muted: false,
            tabs: Vec::new(),
        });

        let theme = Theme::default();
        let out = tree.render(&input, &theme, Instant::now(), 0.0);

        // 1. Top sidebar `+` (`TabNew`) -> `UiAction::NewTerminal` (real Ghostty shell!)
        let top_plus = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::TabNew))
            .expect("top bar + New Terminal button");
        let (x, y) = top_plus.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, x, y, false),
            UiAction::NewTerminal
        );

        // 2. Bottom footer `+ New Session` -> `UiAction::NewSession { host_id: None }`
        let new_sess = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::NewSessionButton))
            .expect("footer New Session button");
        let (x, y) = new_sess.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, x, y, false),
            UiAction::NewSession { host_id: None }
        );

        // 3. Bottom footer Settings gear -> `UiAction::OpenModal(ModalKind::Settings)`
        let settings_btn = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::SettingsButton))
            .expect("footer Settings gear button");
        let (x, y) = settings_btn.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, x, y, false),
            UiAction::OpenModal(ModalKind::Settings)
        );

        // 4. Per-session `+` on `work` (normal session) -> `UiAction::NewTab { host_id, session_id }`
        let work_plus = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::SessionNewTab { session_id, .. } if session_id == "sess:work"
                )
            })
            .expect("normal session inline + button");
        let (x, y) = work_plus.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, x, y, false),
            UiAction::NewTab {
                host_id: Some("local".into()),
                session_id: Some("sess:work".into()),
            }
        );

        // 5. Per-session `+` on `T3 Code` -> `UiAction::NewT3Thread { host_id, session_id }`
        let t3_plus = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::SessionNewT3Thread { session_id, .. } if session_id == "sess:t3"
                )
            })
            .expect("T3 session inline New T3 Thread button");
        let (x, y) = t3_plus.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, x, y, false),
            UiAction::NewT3Thread {
                host_id: "local".into(),
                session_id: "sess:t3".into(),
            }
        );
    }

    #[test]
    fn narrow_viewport_auto_collapses_sidebar_exposes_expand_toggle_and_restores_wide() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        let theme = Theme::default();

        // Narrow Hyprland split (640px < 720px): sidebar auto-collapses, pane header shows [◧] toggle
        input.viewport_w = 640.0;
        let narrow = tree.render(&input, &theme, Instant::now(), 0.0);
        narrow
            .draw_list
            .validate_invariants()
            .expect("narrow valid");
        assert!(narrow.chrome.sidebar_auto_collapsed);
        assert!(narrow.chrome.sidebar.is_none());
        assert_eq!(narrow.chrome.workspace.w, 640.0);

        let expand = narrow
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::SidebarToggle))
            .expect("SidebarToggle expand button must be accessible when auto-collapsed");
        let (ex, ey) = expand.rect.center();
        assert_eq!(
            tree.handle_click(&narrow.draw_list, ex, ey, false),
            UiAction::ToggleSidebar
        );
        // Clicking expand in auto-collapsed mode reveals overlay sidebar without permanently locking collapse
        assert!(!tree.layout_cfg.sidebar_collapsed);
        assert!(tree.layout_cfg.toolbar_revealed);

        let overlay = tree.render(&input, &theme, Instant::now(), 0.0);
        assert!(overlay.chrome.toolbar_is_overlay);
        assert!(overlay.chrome.sidebar.is_some());

        // Dismiss overlay
        let dismiss = overlay
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::SidebarToggle))
            .expect("overlay SidebarToggle");
        let (dx, dy) = dismiss.rect.center();
        tree.handle_click(&overlay.draw_list, dx, dy, false);
        assert!(!tree.layout_cfg.toolbar_revealed);
        assert!(!tree.layout_cfg.sidebar_collapsed);

        // Widen back to 1920px -> sidebar restores automatically!
        input.viewport_w = 1920.0;
        let wide = tree.render(&input, &theme, Instant::now(), 0.0);
        assert!(!wide.chrome.sidebar_auto_collapsed);
        assert_eq!(wide.chrome.sidebar.unwrap().w, 248.0);
    }

    #[test]
    fn hover_tooltips_settings_mouse_click_scrollbar_fraction_and_scroll_hit_clipping_work() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        // Clear floating PR drawer so tiled pane `blk:00`'s right-edge scrollbar & header buttons are unobstructed.
        input.floating_layers.clear();
        let theme = Theme::default();

        // 1. Scroll sidebar halfway so cards overlap top & bottom clip boundaries;
        //    top bar & footer buttons MUST still win hit-testing (no unclipped card bleed!).
        tree.sidebar.scroll_offset_y = 120.0;
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list.validate_invariants().expect("scrolled valid");

        let filter_btn = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::SidebarFilterToggle))
            .expect("footer filter button");
        let (fx, fy) = filter_btn.rect.center();
        assert_eq!(
            out.draw_list.hit_test(fx, fy),
            Some(&HitTarget::SidebarFilterToggle),
            "scrolled tab card must never swallow footer Search click"
        );

        let top_plus = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::TabNew))
            .expect("top bar + button");
        let (px, py) = top_plus.rect.center();
        assert_eq!(
            out.draw_list.hit_test(px, py),
            Some(&HitTarget::TabNew),
            "scrolled tab card must never swallow top bar + click"
        );

        // 2. Hovering top bar `+` triggers tooltip pill on next render and deduplicates idle motion
        assert!(tree.update_pointer_hover(&out.draw_list, px, py));
        assert!(!tree.update_pointer_hover(&out.draw_list, px, py));
        let hovered_out = tree.render(&input, &theme, Instant::now(), 0.0);
        hovered_out
            .draw_list
            .validate_invariants()
            .expect("tooltip draw invariants");
        assert!(
            hovered_out.draw_list.primitives.iter().any(|p| matches!(
                p,
                UiPrimitive::Text { text, .. } if text == "New Terminal (Ctrl+Shift+T)"
            )),
            "hovering top + button must render New Terminal tooltip"
        );

        // 3. Hovering a pane header button (`SplitRight`) produces its crisp tooltip
        let split_btn = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::PaneButton {
                        block_id,
                        button: PaneHeaderButton::SplitRight
                    } if block_id == "blk:00"
                )
            })
            .expect("pane SplitRight button hit");
        let (sx, sy) = split_btn.rect.center();
        assert!(tree.update_pointer_hover(&out.draw_list, sx, sy));
        let pane_tip_out = tree.render(&input, &theme, Instant::now(), 0.0);
        assert!(
            pane_tip_out.draw_list.primitives.iter().any(|p| matches!(
                p,
                UiPrimitive::Text { text, .. } if text == "Split Right (Ctrl+Shift+D)"
            )),
            "hovering pane SplitRight button must render tooltip"
        );

        // 4. Clicking pane scrollbar track near top (10%) computes exact fraction (not hardcoded 0.5)
        let track = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(&h.target, HitTarget::PaneScrollbarTrack { block_id } if block_id == "blk:00"))
            .expect("scrollbar track hit");
        let click_y = track.rect.y + track.rect.h * 0.10;
        match tree.handle_click(&out.draw_list, track.rect.center().0, click_y, false) {
            UiAction::ScrollPaneToFraction { block_id, fraction } => {
                assert_eq!(block_id, "blk:00");
                assert!(
                    (fraction - 0.10).abs() < 0.02,
                    "expected ~0.10 fraction, got {fraction}"
                );
            }
            other => panic!("expected ScrollPaneToFraction, got {other:?}"),
        }

        // 4. Clicking Settings row with mouse emits UpdateSetting AND updates snapshot locally
        let snap = SettingsViewSnapshot::from_config(
            &tree.layout_cfg,
            omarchy_theme::MaterialStyle::default(),
            true,
            true,
            true,
        );
        tree.open_modal(ModalState::open_settings(snap));
        let settings_out = tree.render(&input, &theme, Instant::now(), 0.0);
        let density_row = settings_out
            .draw_list
            .hits
            .iter()
            .find(
                |h| matches!(&h.target, HitTarget::SettingsToggle { key } if key == "pane_density"),
            )
            .expect("pane_density settings toggle hit");
        let (dx, dy) = density_row.rect.center();
        assert_eq!(
            tree.handle_click(&settings_out.draw_list, dx, dy, false),
            UiAction::UpdateSetting(SettingChange::PaneDensity(PaneDensity::Compact))
        );
        assert!(matches!(
            tree.modal.as_ref().map(|m| &m.content),
            Some(ModalContent::Settings { snapshot, .. }) if snapshot.density == PaneDensity::Compact
        ));
    }

    #[test]
    fn sticky_session_header_stays_pinned_and_clickable_when_scrolled_to_tab_26() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        // Expand to 26 T3 threads and activate tab #26 (index 25) so sidebar auto-scrolls to the bottom.
        for i in 23..26 {
            let card = build_tab_card(
                &format!("Thread #{i:02} — Deep scroll task"),
                Some("omarex"),
                None,
                None,
                Some((i as u64) * 60),
            );
            input.hosts[0].sessions[0].tabs.push(SidebarTabView {
                window_id: format!("win:{i:02}"),
                card,
                active: false,
                muted: false,
                unread: false,
                pane_count: 1,
                icon_override: None,
            });
        }
        for (idx, tab) in input.hosts[0].sessions[0].tabs.iter_mut().enumerate() {
            tab.active = idx == 25;
        }
        assert_eq!(input.hosts[0].sessions[0].tabs.len(), 26);

        let theme = Theme::default();
        tree.sync_sidebar_scroll(&input);
        assert!(
            tree.sidebar.scroll_offset_y > 400.0,
            "sidebar should auto-scroll deeply to reveal tab #26"
        );

        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("sticky header draw invariants");

        // Top of scroll_rect sits right below the 36px top bar (`y = 36.0`).
        let scroll_top_y = 36.0;
        let sticky_chev = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::SessionChevron { session_id, .. } if session_id == "sess:t3"
                ) && h.rect.y >= scroll_top_y
                    && h.rect.y <= scroll_top_y + 8.0
            })
            .expect("sticky T3 Code session header must be pinned at scroll_rect.y");

        // Clicking the pinned sticky chevron/header immediately collapses T3 Code without scrolling up!
        let (cx, cy) = sticky_chev.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, cx, cy, false),
            UiAction::ToggleSessionCollapse {
                host_id: "local".into(),
                session_id: "sess:t3".into(),
            }
        );

        // No scrolled TabCard hit rect may overlap the sticky session header region!
        let sticky_bottom_y = scroll_top_y + 32.0;
        for hit in &out.draw_list.hits {
            if matches!(
                hit.target,
                HitTarget::TabCard { .. } | HitTarget::TabClose { .. }
            ) {
                assert!(
                    hit.rect.y >= sticky_bottom_y - 0.01,
                    "scrolled tab hit {:?} must be clipped strictly below sticky header (y >= {sticky_bottom_y})",
                    hit.rect
                );
            }
        }

        // Inline `+` (New T3 Thread) on the sticky header also remains directly clickable!
        let sticky_plus = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::SessionNewT3Thread { session_id, .. } if session_id == "sess:t3"
                ) && h.rect.y >= scroll_top_y
                    && h.rect.bottom() <= sticky_bottom_y
            })
            .expect("sticky T3 Code header inline New T3 Thread button");
        let (px, py) = sticky_plus.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, px, py, false),
            UiAction::NewT3Thread {
                host_id: "local".into(),
                session_id: "sess:t3".into(),
            }
        );
    }

    #[test]
    fn remote_host_inline_plus_context_menu_and_targeted_t3_thread_work() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        // Keep local collapsed so the remote host & its T3 Code session are cleanly visible in viewport.
        input.hosts[0].sessions[0].collapsed = true;
        input.hosts.push(SidebarHostView {
            host_id: "remote:mac-mini".into(),
            label: "Studio Mac".into(),
            is_local: false,
            connection: HostConnectionState::Online,
            collapsed: false,
            sessions: vec![SidebarSessionView {
                session_id: "a6dd4a".into(),
                label: "T3 Code".into(),
                active: false,
                collapsed: false,
                muted: false,
                tabs: vec![SidebarTabView {
                    window_id: "win:mac-1".into(),
                    card: build_tab_card("Mac mini thread", Some("repo"), None, None, Some(60)),
                    active: false,
                    muted: false,
                    unread: false,
                    pane_count: 1,
                    icon_override: None,
                }],
            }],
        });

        let theme = Theme::default();
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("multi-host draw invariants");

        // 1. Clicking inline `+` on remote host header creates a new terminal session on THAT host!
        let remote_plus = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::HostNewSession { host_id } if host_id == "remote:mac-mini"
                )
            })
            .expect("remote host inline + hit target");
        let (hx, hy) = remote_plus.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, hx, hy, false),
            UiAction::NewSession {
                host_id: Some("remote:mac-mini".into()),
            }
        );

        // 2. Clicking `+` on Mac mini's `T3 Code` targets THAT host + session!
        let mac_t3_plus = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::SessionNewT3Thread { host_id, session_id }
                        if host_id == "remote:mac-mini" && session_id == "a6dd4a"
                )
            })
            .expect("remote T3 Code inline + hit target");
        let (tx, ty) = mac_t3_plus.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, tx, ty, false),
            UiAction::NewT3Thread {
                host_id: "remote:mac-mini".into(),
                session_id: "a6dd4a".into(),
            }
        );

        // 3. Right-clicking remote host header opens macOS-Rex-faithful context menu
        //    (`New Session on Host`, `Disconnect`, `Edit Host…`, `Collapse Host`, `Remove Host`).
        tree.sync_sidebar_scroll(&input);
        let remote_hdr = out
            .draw_list
            .hits
            .iter()
            .find(|h| {
                matches!(
                    &h.target,
                    HitTarget::HostHeader { host_id } if host_id == "remote:mac-mini"
                )
            })
            .expect("remote host header hit target");
        let (rx, ry) = (remote_hdr.rect.x + 20.0, remote_hdr.rect.center().1);
        assert_eq!(
            tree.handle_click(&out.draw_list, rx, ry, true),
            UiAction::None
        );
        let menu = tree
            .context_menu
            .as_ref()
            .expect("remote host right-click context menu");
        assert!(menu.items.iter().any(|i| i.label == "New Session on Host"
            && matches!(&i.action, UiAction::NewSession { host_id: Some(h) } if h == "remote:mac-mini")));
        assert!(menu.items.iter().any(|i| i.label == "Disconnect"
            && matches!(&i.action, UiAction::DisconnectHost { host_id } if host_id == "remote:mac-mini")));
        assert!(menu.items.iter().any(|i| i.label == "Edit Host…"
            && matches!(&i.action, UiAction::StartRenameHost { host_id } if host_id == "remote:mac-mini")));
        assert!(menu.items.iter().any(|i| i.label == "Remove Host"
            && matches!(&i.action, UiAction::RemoveHost { host_id } if host_id == "remote:mac-mini")));

        // When offline, `Disconnect` switches cleanly to `Connect`!
        input.hosts[1].connection = HostConnectionState::Offline;
        tree.sync_sidebar_scroll(&input);
        tree.close_context_menu();
        assert_eq!(
            tree.handle_click(&out.draw_list, rx, ry, true),
            UiAction::None
        );
        let offline_menu = tree.context_menu.as_ref().expect("offline host menu");
        assert!(offline_menu.items.iter().any(|i| i.label == "Connect"
            && matches!(&i.action, UiAction::ReconnectHost { host_id } if host_id == "remote:mac-mini")));
    }

    #[test]
    fn spring_active_border_and_close_modal_viewer_animations_settle_and_preserve_focus() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        input.floating_layers.clear();
        input.split_root = Some(SplitNode::split(
            SplitAxis::Horizontal,
            0.5,
            SplitNode::leaf("blk:00".into()),
            SplitNode::leaf("blk:01".into()),
        ));
        input.pane_metas.push(PaneMeta {
            block_id: "blk:01".into(),
            label: "Second".into(),
            cwd: None,
            icon: IconId::Terminal,
            focused: false,
            hovered: false,
            bell_unread: false,
            program_status: None,
            osc94_progress: None,
            scrollbar: ScrollbarMetrics::default(),
            program_bg: None,
            has_unread_output: false,
            copy_mode_badge: None,
            listening_ports: Vec::new(),
            is_pr_drawer: false,
            pr_subtitle: None,
        });
        let theme = Theme::default();
        let mut t = Instant::now();

        // First render must snap active border directly to blk:00 (no fly-in from (0,0)) and be settled.
        let out0 = tree.render(&input, &theme, t, 0.0);
        assert!(!tree.has_active_animations());
        assert_eq!(out0.tiled_panes.len(), 2);

        // Switch focus to blk:01 -> active border springs smoothly from blk:00 -> blk:01.
        input.pane_metas[0].focused = false;
        input.pane_metas[2].focused = true;
        let _ = tree.render(&input, &theme, t, 0.0);
        assert!(tree.has_active_animations());
        t += std::time::Duration::from_millis(16);
        assert!(tree.tick_animations(t));
        let out_mid = tree.render(&input, &theme, t, 0.0);
        let border_mid = out_mid
            .draw_list
            .primitives
            .iter()
            .rev()
            .find_map(|p| match p {
                UiPrimitive::RoundedRect {
                    rect,
                    fill,
                    border_width,
                    ..
                } if fill.a == 0 && (*border_width - 1.5).abs() < 0.01 => Some(*rect),
                _ => None,
            })
            .expect("spring border primitive");
        assert!(
            border_mid.x > out0.tiled_panes[0].card_rect.x
                && border_mid.x < out0.tiled_panes[1].card_rect.x,
            "expected mid-flight border x between pane 0 and pane 1, got {:?}",
            border_mid
        );

        // Advance 400ms -> settles cleanly (has_active_animations == false).
        for _ in 0..30 {
            t += std::time::Duration::from_millis(16);
            tree.tick_animations(t);
        }
        assert!(!tree.has_active_animations());

        // Modal open + close_modal immediately sets modal=None and FocusTarget::Terminal while
        // closing_modal fades out with ZERO hit targets!
        tree.open_modal(ModalState::open_shortcuts_help());
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Modal(ModalKind::ShortcutsHelp)
        );
        let _ = tree.render(&input, &theme, t, 0.0);
        tree.close_modal();
        assert!(tree.modal.is_none());
        assert!(matches!(tree.focus.current(), FocusTarget::Terminal(_)));
        assert!(tree.has_active_animations());

        let closing_out = tree.render(&input, &theme, t, 0.0);
        assert!(
            !closing_out
                .draw_list
                .hits
                .iter()
                .any(|h| matches!(h.target, HitTarget::ModalBackdrop)),
            "closing_modal shadow must be hit-free so clicks pass through immediately"
        );
        for _ in 0..30 {
            t += std::time::Duration::from_millis(16);
            tree.tick_animations(t);
        }
        assert!(!tree.has_active_animations());
    }

    #[test]
    fn new_output_pill_renders_and_clicks_to_bottom_and_program_bg_keeps_wcag_contrast() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        input.floating_layers.clear();
        // Set unread output + bright light program_bg (#fbf1c7) inside dark theme
        input.pane_metas[0].has_unread_output = true;
        input.pane_metas[0].program_bg = Some(omarchy_theme::Rgba::new(0xfb, 0xf1, 0xc7, 255));
        let theme = Theme::default();

        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("pill + program_bg invariants");

        // Verify ↓ New output pill is rendered and clickable
        let pill_hit = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(&h.target, HitTarget::PaneNewOutputPill { block_id } if block_id == "blk:00"))
            .expect("PaneNewOutputPill hit target");
        let (px, py) = pill_hit.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, px, py, false),
            UiAction::ScrollPaneToBottom {
                block_id: "blk:00".into(),
            }
        );

        // Verify pane header label text has WCAG contrast >= 4.5 against bright program_bg #fbf1c7
        let pbg_rgb = omarchy_theme::Rgb::new(0xfb, 0xf1, 0xc7);
        let title_color = out
            .draw_list
            .primitives
            .iter()
            .find_map(|p| match p {
                UiPrimitive::Text { text, color, .. } if text == "Thread #00" => Some(*color),
                _ => None,
            })
            .expect("pane header title");
        assert!(
            pbg_rgb.wcag_contrast(title_color.rgb()) >= 4.5,
            "expected WCAG AA contrast >= 4.5 on bright program_bg, got {:.2}",
            pbg_rgb.wcag_contrast(title_color.rgb())
        );
    }

    #[test]
    fn file_picker_modal_tab_and_backspace_do_not_close() {
        let mut tree = UiTree::new();
        let entries = vec![
            FilePickerEntry {
                name: "src".into(),
                path: "/work/src".into(),
                is_dir: true,
                is_symlink: false,
            },
            FilePickerEntry {
                name: "Cargo.toml".into(),
                path: "/work/Cargo.toml".into(),
                is_dir: false,
                is_symlink: false,
            },
        ];
        tree.open_modal(ModalState::open_file_picker(
            FilePickerPurpose::InsertPath,
            "/work",
            None,
            entries,
            false,
        ));
        assert_eq!(
            tree.focus.current(),
            &FocusTarget::Modal(ModalKind::FilePicker)
        );

        let descend = tree.handle_modal_key(ModalKey::Tab);
        assert_eq!(
            descend,
            UiAction::FilePickerDescend {
                path: "/work/src".into()
            }
        );
        assert!(tree.modal.is_some(), "Tab must keep file picker open");

        let parent = tree.handle_modal_key(ModalKey::Backspace);
        assert_eq!(parent, UiAction::FilePickerParent);
        assert!(
            tree.modal.is_some(),
            "Backspace on empty filter keeps modal open"
        );

        let accept = tree.handle_modal_key(ModalKey::Enter);
        assert_eq!(
            accept,
            UiAction::FilePickerAccept {
                path: "/work/src".into()
            }
        );
        assert!(tree.modal.is_none(), "Enter must close file picker");
    }

    #[test]
    fn theme_picker_preview_commit_and_cancel() {
        let mut tree = UiTree::new();
        let items = vec![
            PaletteItem::new("omarchy-live", "omarchy-live"),
            PaletteItem::new("catppuccin", "catppuccin"),
        ];
        tree.open_modal(ModalState::open_theme_picker(items, Some("omarchy-live")));
        assert_eq!(
            tree.handle_modal_key(ModalKey::Down),
            UiAction::ThemePickerPreview {
                name: "catppuccin".into()
            }
        );
        assert!(tree.modal.is_some(), "preview keeps ThemePicker open");

        assert_eq!(
            tree.handle_modal_key(ModalKey::Escape),
            UiAction::ThemePickerCancel
        );
        assert!(tree.modal.is_none(), "Esc closes ThemePicker");
    }

    #[test]
    fn tabbar_mute_icon_pane_port_pill_copy_badge_settings_categories_and_diagnostics() {
        let mut tree = UiTree::new();
        let mut input = sample_t3_23_window_input();
        input.floating_layers.clear();
        input.hosts[0].sessions[0].tabs[0].muted = true;
        input.pane_metas[0].copy_mode_badge = Some("V-LINE");
        input.pane_metas[0].listening_ports = vec![PortBadge {
            port: 3000,
            label: Some("vite".into()),
            tunneled: false,
        }];
        let theme = Theme::default();

        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("m4 chrome invariants");

        assert!(
            out.draw_list.primitives.iter().any(|p| matches!(
                p,
                UiPrimitive::Icon {
                    icon: IconId::BellOff,
                    ..
                }
            )),
            "muted tab should render BellOff icon"
        );
        assert!(
            out.draw_list
                .primitives
                .iter()
                .any(|p| matches!(p, UiPrimitive::Text { text, .. } if text == "V-LINE")),
            "copy mode badge should render"
        );
        let port_hit = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(&h.target, HitTarget::PanePortPill { port: 3000, .. }))
            .expect("PanePortPill hit target");
        let (px, py) = port_hit.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, px, py, false),
            UiAction::OpenPortAction {
                block_id: "blk:00".into(),
                port: 3000,
            }
        );

        // Settings modal category switching
        let snap = SettingsViewSnapshot::from_config(
            &tree.layout_cfg,
            omarchy_theme::MaterialStyle::default(),
            true,
            true,
            true,
        );
        tree.open_modal(ModalState::open_settings(snap));
        let set_out = tree.render(&input, &theme, Instant::now(), 0.0);
        let audio_tab = set_out
            .draw_list
            .hits
            .iter()
            .find(|h| h.target == HitTarget::SettingsCategoryTab(SettingsCategory::AudioAlerts))
            .expect("AudioAlerts tab");
        let (ax, ay) = audio_tab.rect.center();
        assert_eq!(
            tree.handle_click(&set_out.draw_list, ax, ay, false),
            UiAction::None
        );
        let audio_out = tree.render(&input, &theme, Instant::now(), 0.0);
        assert!(audio_out.draw_list.hits.iter().any(
            |h| matches!(&h.target, HitTarget::SettingsToggle { key } if key == "chime_enabled")
        ));

        // Diagnostics modal frame graph + copy debug bundle
        tree.open_modal(ModalState::open_diagnostics(DiagnosticsSnapshot {
            app_version: "0.1.0".into(),
            vt_version: "1.4.0".into(),
            gpu_adapter: "Test GPU".into(),
            gpu_backend: "Vulkan".into(),
            frame_times_us: vec![2000, 4500, 19000],
            avg_frame_ms: 8.5,
            p95_frame_ms: 19.0,
            max_frame_ms: 19.0,
            last_draw_calls: 12,
            last_quads: 40,
            last_glyphs: 500,
            hosts: vec![DiagnosticsHostRow {
                label: "Local".into(),
                status: "Connected".into(),
                tailscale_summary: "online".into(),
                dropped_events: 0,
                active_streams: 1,
            }],
            keymap_warnings: vec!["unused key".into()],
            active_ports: vec![":3000".into()],
            active_tunnels: vec![],
        }));
        let diag_out = tree.render(&input, &theme, Instant::now(), 0.0);
        diag_out
            .draw_list
            .validate_invariants()
            .expect("diagnostics invariants");
        let copy_hit = diag_out
            .draw_list
            .hits
            .iter()
            .find(|h| h.target == HitTarget::DiagnosticsCopyBundle)
            .expect("DiagnosticsCopyBundle hit");
        let (cx, cy) = copy_hit.rect.center();
        assert_eq!(
            tree.handle_click(&diag_out.draw_list, cx, cy, false),
            UiAction::CopyDiagnosticsBundle
        );
    }

    #[test]
    fn narrow_pane_renders_port_overflow_badge_when_one_pill_fits() {
        let mut tree = UiTree::new();
        tree.layout_cfg.toolbar_behavior = ToolbarBehavior::Hidden;
        tree.layout_cfg.pane_header_mode = PaneHeaderMode::Always;
        let mut input = sample_t3_23_window_input();
        input.floating_layers.clear();
        input.viewport_w = 300.0;
        input.viewport_h = 400.0;
        input.pane_metas[0].label = "sh".into();
        input.pane_metas[0].cwd = None;
        input.pane_metas[0].program_status = None;
        input.pane_metas[0].copy_mode_badge = None;
        input.pane_metas[0].listening_ports = vec![
            PortBadge {
                port: 80,
                label: None,
                tunneled: false,
            },
            PortBadge {
                port: 3000,
                label: None,
                tunneled: false,
            },
            PortBadge {
                port: 8080,
                label: None,
                tunneled: false,
            },
        ];
        let theme = Theme::default();
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("narrow port pill invariants");

        let texts: Vec<&str> = out
            .draw_list
            .primitives
            .iter()
            .filter_map(|p| match p {
                UiPrimitive::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            texts.contains(&":80"),
            "first port pill must render: {texts:?}"
        );
        assert!(
            !texts.contains(&":3000"),
            "second port pill exceeds max_x at 300px: {texts:?}"
        );
        assert!(
            texts.contains(&"+2"),
            "+2 overflow badge must render when space remains after first pill: {texts:?}"
        );

        input.viewport_w = 420.0;
        let wide_out = tree.render(&input, &theme, Instant::now(), 0.0);
        let wide_texts: Vec<&str> = wide_out
            .draw_list
            .primitives
            .iter()
            .filter_map(|p| match p {
                UiPrimitive::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(wide_texts.contains(&":80"));
        assert!(wide_texts.contains(&":3000"));
        assert!(wide_texts.contains(&"+1"));
    }

    #[test]
    fn file_picker_clears_filter_on_dir_change_and_terminal_font_size_steps() {
        let mut modal = ModalState::open_file_picker(
            FilePickerPurpose::InsertPath,
            "/work",
            None,
            vec![FilePickerEntry {
                name: "src".into(),
                path: "/work/src".into(),
                is_dir: true,
                is_symlink: false,
            }],
            false,
        );
        for ch in ['s', 'r', 'c'] {
            modal.handle_key(ModalKey::Char(ch));
        }
        // Navigating to /work/src clears the filter so child items are visible.
        modal.set_file_picker_entries(
            "/work/src",
            vec![FilePickerEntry {
                name: "main.rs".into(),
                path: "/work/src/main.rs".into(),
                is_dir: false,
                is_symlink: false,
            }],
            None,
        );
        if let ModalContent::FilePicker {
            current_dir,
            input,
            filtered_indices,
            ..
        } = &modal.content
        {
            assert_eq!(current_dir, "/work/src");
            assert_eq!(input.text(), "");
            assert_eq!(filtered_indices.len(), 1);
        } else {
            panic!("expected FilePicker");
        }

        // Error preserves current_dir and prior entries.
        modal.set_file_picker_error("Permission denied");
        if let ModalContent::FilePicker {
            current_dir,
            entries,
            error,
            ..
        } = &modal.content
        {
            assert_eq!(current_dir, "/work/src");
            assert_eq!(entries.len(), 1);
            assert_eq!(error.as_deref(), Some("Permission denied"));
        } else {
            panic!("expected FilePicker");
        }

        // Terminal Font Size row steps cleanly on click/Enter.
        let mut tree = UiTree::new();
        let snap = SettingsViewSnapshot::from_config(
            &tree.layout_cfg,
            omarchy_theme::MaterialStyle::default(),
            true,
            true,
            true,
        )
        .with_extras(None, "JetBrainsMono Nerd Font", 11.0, true, true);
        tree.open_modal(ModalState::open_settings(snap));
        let input = sample_t3_23_window_input();
        let theme = Theme::default();
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        let term_tab = out
            .draw_list
            .hits
            .iter()
            .find(|h| h.target == HitTarget::SettingsCategoryTab(SettingsCategory::Terminal))
            .expect("Terminal tab");
        let (tx, ty) = term_tab.rect.center();
        assert_eq!(
            tree.handle_click(&out.draw_list, tx, ty, false),
            UiAction::None
        );

        let term_out = tree.render(&input, &theme, Instant::now(), 0.0);
        let font_row = term_out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(&h.target, HitTarget::SettingsToggle { key } if key == "font_size"))
            .expect("font_size hit target");
        let (fx, fy) = font_row.rect.center();
        assert_eq!(
            tree.handle_click(&term_out.draw_list, fx, fy, false),
            UiAction::UpdateSetting(SettingChange::FontSizePt(12.0))
        );
    }
}
