//! Collapsible multi-host & session sidebar featuring Rex/`tr3x` 2-line tab cards,
//! brand-colored active provider icons, OSC 7501 status dots, animated `•••` pulse,
//! Activity mode (by priority or by session), inline filter, and bottom utility bar.

use omarchy_theme::{Rgb, Rgba, Theme, UiTokens};
use tr3x_bridge::t3_session::{
    PriorityDot, ProgramState, TabCardMetadata, WorkingIndicator, is_t3_session,
};

use crate::draw::{
    FocusTarget, FontKind, FontWeight, HitTarget, TabReorderOverlay, TextAlign, UiDrawList,
    UiInteraction, cap_radius, z_layer,
};
use crate::icons::IconId;
use crate::layout::RectF;
use crate::text_input::TextInput;

/// Connection health displayed beside a host header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostConnectionState {
    #[default]
    Online,
    Reconnecting,
    Offline,
}

/// Activity view grouping mode (`client.sidebar.activity.by_priority` vs `.by_session`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivitySortMode {
    #[default]
    ByPriority,
    BySession,
}

/// Tab icon coloring policy (`interface.tabs.color`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabIconColorPolicy {
    Always,
    #[default]
    ActiveTabs,
    Never,
}

/// Single window/tab row rendered inside a sidebar session (or Activity list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarTabView {
    pub window_id: String,
    pub card: TabCardMetadata,
    pub active: bool,
    pub muted: bool,
    pub unread: bool,
    pub pane_count: usize,
    pub icon_override: Option<IconId>,
}

impl SidebarTabView {
    #[must_use]
    pub fn resolved_icon(&self) -> IconId {
        if let Some(icon) = self.icon_override {
            return icon;
        }
        IconId::for_provider_or_terminal(Some(self.card.provider.icon_name()))
    }

    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.unread
            || !matches!(self.card.priority_dot, PriorityDot::None)
            || matches!(
                self.card.state,
                ProgramState::Blocked | ProgramState::Error | ProgramState::Waiting
            )
    }

    #[must_use]
    pub fn is_active_in_activity_view(&self, include_idle: bool) -> bool {
        if include_idle {
            true
        } else {
            self.needs_attention()
                || matches!(self.card.state, ProgramState::Working | ProgramState::Done)
        }
    }
}

/// Session entry rendered beneath a host header in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarSessionView {
    pub session_id: String,
    pub label: String,
    pub active: bool,
    pub collapsed: bool,
    pub muted: bool,
    /// Tabs strictly in server-provided order (critical for `tr3x` T3 Code priority sorting!).
    pub tabs: Vec<SidebarTabView>,
}

impl SidebarSessionView {
    #[must_use]
    pub fn is_t3_session(&self) -> bool {
        is_t3_session(&self.label)
    }

    #[must_use]
    pub fn attention_count(&self) -> usize {
        self.tabs.iter().filter(|t| t.needs_attention()).count()
    }

    /// Returns the highest-priority semantic status dot color across all child tabs
    /// (for compact collapsed session badges: Blocked > Error > Unread/Done > Working).
    #[must_use]
    pub fn highest_status_dot(&self, tokens: &UiTokens) -> Option<Rgba> {
        let mut has_error = false;
        let mut has_unread = false;
        let mut has_working = false;
        for tab in &self.tabs {
            match tab.card.state {
                ProgramState::Blocked => return Some(tokens.status_blocked),
                ProgramState::Error => has_error = true,
                ProgramState::Working => has_working = true,
                _ => {}
            }
            if matches!(tab.card.priority_dot, PriorityDot::Blocked) {
                return Some(tokens.status_blocked);
            }
            if matches!(tab.card.priority_dot, PriorityDot::Error) {
                has_error = true;
            }
            if tab.unread || matches!(tab.card.priority_dot, PriorityDot::DoneUnseen) {
                has_unread = true;
            }
        }
        if has_error {
            Some(tokens.status_error)
        } else if has_unread {
            Some(tokens.accent_primary)
        } else if has_working {
            Some(tokens.status_working)
        } else {
            None
        }
    }
}

/// Host group in the sidebar (local Unix socket + remote SSH/Tailscale Rex hosts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarHostView {
    pub host_id: String,
    pub label: String,
    pub is_local: bool,
    pub connection: HostConnectionState,
    pub collapsed: bool,
    pub sessions: Vec<SidebarSessionView>,
}

/// Computed vertical layout metrics for the scrollable sidebar tree/activity list.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SidebarScrollMetrics {
    pub viewport_height: f32,
    pub content_height: f32,
    pub max_scroll: f32,
    /// Active tab card's `[top_y, bottom_y]` in un-scrolled content coordinates (`0..content_height`).
    pub active_tab_span: Option<(f32, f32)>,
}

/// Interactive sidebar UI state owned by `UiTree`.
#[derive(Debug, Clone)]
pub struct SidebarState {
    pub activity_mode: bool,
    pub activity_sort: ActivitySortMode,
    pub include_idle_in_activity: bool,
    pub icon_color_policy: TabIconColorPolicy,
    pub filter_open: bool,
    pub filter_input: TextInput,
    pub scroll_offset_y: f32,
    pub hovered_tab_window_id: Option<String>,
    pub hovered_session_id: Option<String>,
    /// Tracks the last active `(host_id, session_id, window_id)` auto-scrolled into view
    /// so active tab switches bring the tab into viewport without fighting manual wheel scrolling.
    pub last_auto_scrolled_tab: Option<(String, String, String)>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            activity_mode: false,
            activity_sort: ActivitySortMode::ByPriority,
            include_idle_in_activity: false,
            icon_color_policy: TabIconColorPolicy::ActiveTabs,
            filter_open: false,
            filter_input: TextInput::new("Filter tabs, sessions, hosts…"),
            scroll_offset_y: 0.0,
            hovered_tab_window_id: None,
            hovered_session_id: None,
            last_auto_scrolled_tab: None,
        }
    }
}

impl SidebarState {
    pub fn toggle_activity(&mut self) {
        self.activity_mode = !self.activity_mode;
        self.scroll_offset_y = 0.0;
        self.last_auto_scrolled_tab = None;
    }

    pub fn toggle_activity_sort(&mut self) {
        self.activity_sort = match self.activity_sort {
            ActivitySortMode::ByPriority => ActivitySortMode::BySession,
            ActivitySortMode::BySession => ActivitySortMode::ByPriority,
        };
    }

    pub fn open_filter(&mut self) {
        self.filter_open = true;
    }

    pub fn close_filter(&mut self) {
        self.filter_open = false;
        self.filter_input.clear();
        self.last_auto_scrolled_tab = None;
    }

    /// Clamps `scroll_offset_y` strictly into `[0.0, max_scroll]`.
    pub fn clamp_scroll(&mut self, max_scroll: f32) {
        self.scroll_offset_y = self.scroll_offset_y.clamp(0.0, max_scroll.max(0.0));
    }

    /// Applies a vertical wheel scroll delta (`delta_y_px > 0` scrolls UP toward 0),
    /// strictly clamped into `[0.0, max_scroll]`.
    pub fn scroll_by(&mut self, delta_y_px: f32, max_scroll: f32) {
        self.scroll_offset_y = (self.scroll_offset_y - delta_y_px).clamp(0.0, max_scroll.max(0.0));
    }

    /// Adjusts `scroll_offset_y` minimally so the active tab card sits cleanly inside the
    /// visible sidebar viewport (with comfortable top/bottom margin).
    pub fn ensure_active_tab_visible(&mut self, metrics: SidebarScrollMetrics) {
        let max_scroll = metrics.max_scroll.max(0.0);
        self.clamp_scroll(max_scroll);
        let Some((top, bottom)) = metrics.active_tab_span else {
            return;
        };
        if metrics.viewport_height <= 0.0 {
            return;
        }
        const MARGIN: f32 = 8.0;
        let view_top = self.scroll_offset_y;
        let view_bottom = view_top + metrics.viewport_height;
        if top - MARGIN < view_top {
            self.scroll_offset_y = (top - MARGIN).max(0.0);
        } else if bottom + MARGIN > view_bottom {
            self.scroll_offset_y = (bottom + MARGIN - metrics.viewport_height).max(0.0);
        }
        self.clamp_scroll(max_scroll);
    }
}

const SIDEBAR_TOP_H: f32 = 36.0;
const SIDEBAR_FOOTER_H: f32 = 38.0;
const SIDEBAR_FILTER_H: f32 = 34.0;
const SIDEBAR_ACTIVITY_BAR_H: f32 = 28.0;
const SIDEBAR_CONTENT_PAD: f32 = 6.0;
const SIDEBAR_BOTTOM_PAD: f32 = 8.0;
const HOST_ROW_ADVANCE: f32 = 28.0;
const HOST_GROUP_GAP: f32 = 8.0;
const SESSION_ROW_HEIGHT: f32 = 26.0;
const SESSION_ROW_ADVANCE: f32 = 28.0;
const STICKY_SESSION_BAR_H: f32 = SESSION_ROW_HEIGHT + 6.0;
const TAB_ROW_HEIGHT: f32 = 50.0;
const TAB_ROW_ADVANCE: f32 = 54.0;

#[must_use]
fn sidebar_scroll_viewport_rect(sidebar_rect: RectF, state: &SidebarState) -> RectF {
    let mut content_top = sidebar_rect.y + SIDEBAR_TOP_H;
    if state.filter_open {
        content_top += SIDEBAR_FILTER_H;
    }
    if state.activity_mode {
        content_top += SIDEBAR_ACTIVITY_BAR_H;
    }
    let footer_y = (sidebar_rect.bottom() - SIDEBAR_FOOTER_H).max(content_top);
    RectF::new(
        sidebar_rect.x,
        content_top,
        sidebar_rect.w,
        (footer_y - content_top).max(0.0),
    )
}

/// Computes exact vertical scroll extent and active-tab vertical span for the sidebar tree.
#[must_use]
pub fn compute_sidebar_scroll_metrics(
    sidebar_rect: RectF,
    hosts: &[SidebarHostView],
    state: &SidebarState,
) -> SidebarScrollMetrics {
    if sidebar_rect.is_empty() {
        return SidebarScrollMetrics::default();
    }
    let scroll_rect = sidebar_scroll_viewport_rect(sidebar_rect, state);
    let viewport_height = scroll_rect.h.max(0.0);
    let filter_query = state.filter_input.text().trim().to_ascii_lowercase();

    let mut y = SIDEBAR_CONTENT_PAD;
    let mut active_tab_span: Option<(f32, f32)> = None;

    if state.activity_mode && state.activity_sort == ActivitySortMode::ByPriority {
        let mut entries: Vec<(u8, usize, &SidebarTabView)> = Vec::new();
        let mut order = 0usize;
        for host in hosts {
            for session in &host.sessions {
                for tab in &session.tabs {
                    if !tab.is_active_in_activity_view(state.include_idle_in_activity) {
                        continue;
                    }
                    if !filter_query.is_empty() && !tab_matches_query(tab, &filter_query) {
                        continue;
                    }
                    entries.push((tab.card.state.priority_rank(tab.unread), order, tab));
                    order += 1;
                }
            }
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, _, tab) in entries {
            if tab.active && active_tab_span.is_none() {
                active_tab_span = Some((y, y + TAB_ROW_HEIGHT));
            }
            y += TAB_ROW_ADVANCE;
        }
    } else {
        for (host_idx, host) in hosts.iter().enumerate() {
            if host_idx > 0 {
                y += HOST_GROUP_GAP;
            }
            y += HOST_ROW_ADVANCE;
            if host.collapsed {
                continue;
            }
            for session in &host.sessions {
                let session_matches_filter = filter_query.is_empty()
                    || session.label.to_ascii_lowercase().contains(&filter_query)
                    || host.label.to_ascii_lowercase().contains(&filter_query);

                let matching_tabs: Vec<&SidebarTabView> = session
                    .tabs
                    .iter()
                    .filter(|t| {
                        let activity_ok = !state.activity_mode
                            || t.is_active_in_activity_view(state.include_idle_in_activity);
                        let filter_ok =
                            session_matches_filter || tab_matches_query(t, &filter_query);
                        activity_ok && filter_ok
                    })
                    .collect();

                if !filter_query.is_empty() && !session_matches_filter && matching_tabs.is_empty() {
                    continue;
                }
                if state.activity_mode && matching_tabs.is_empty() {
                    continue;
                }

                y += SESSION_ROW_ADVANCE;
                if session.collapsed {
                    continue;
                }
                for tab in matching_tabs {
                    if tab.active && active_tab_span.is_none() {
                        active_tab_span = Some((y, y + TAB_ROW_HEIGHT));
                    }
                    y += TAB_ROW_ADVANCE;
                }
            }
        }
    }

    let content_height = (y + SIDEBAR_BOTTOM_PAD).max(0.0);
    let max_scroll = (content_height - viewport_height).max(0.0);
    SidebarScrollMetrics {
        viewport_height,
        content_height,
        max_scroll,
        active_tab_span,
    }
}

/// Renders the complete Rex/tr3x sidebar into `draw_list`, returning its computed scroll metrics.
#[allow(clippy::too_many_arguments)]
pub fn render_sidebar(
    draw_list: &mut UiDrawList,
    sidebar_rect: RectF,
    resize_handle: Option<RectF>,
    is_right_side: bool,
    is_overlay: bool,
    hosts: &[SidebarHostView],
    state: &SidebarState,
    focus: &FocusTarget,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    anim_phase: f32,
    tab_reorder: Option<&TabReorderOverlay>,
) -> SidebarScrollMetrics {
    if sidebar_rect.is_empty() {
        return SidebarScrollMetrics::default();
    }
    let scroll_metrics = compute_sidebar_scroll_metrics(sidebar_rect, hosts, state);
    let effective_scroll_y = state.scroll_offset_y.clamp(0.0, scroll_metrics.max_scroll);

    let tokens = theme.effective_ui();
    let z = if is_overlay {
        z_layer::OVERLAY_TOOLBAR
    } else {
        z_layer::SIDEBAR
    };

    // 1. Sidebar backdrop & crisp inner hairline border (with accent highlight on divider hover/drag)
    let shadow = is_overlay.then_some(crate::draw::ShadowSpec {
        offset_x: if is_right_side { -6.0 } else { 6.0 },
        offset_y: 0.0,
        blur_radius: 24.0,
        spread: 0.0,
        color: tokens.shadow_color,
    });
    draw_list.push_rect(sidebar_rect, tokens.surface_sidebar, None, 0.0, 0.0, shadow);

    let resize_active = interaction.is_hovered(&HitTarget::SidebarResizeHandle)
        || interaction.is_pressed(&HitTarget::SidebarResizeHandle);
    let border_w = if resize_active { 2.0 } else { 1.0 };
    let border_x = if is_right_side {
        sidebar_rect.x
    } else {
        (sidebar_rect.right() - border_w).max(sidebar_rect.x)
    };
    let border_color = if resize_active {
        tokens.accent_primary.with_alpha_f32(0.75)
    } else {
        tokens.border_hairline
    };
    draw_list.push_rect(
        RectF::new(border_x, sidebar_rect.y, border_w, sidebar_rect.h),
        border_color,
        None,
        0.0,
        0.0,
        None,
    );

    if let Some(handle) = resize_handle {
        draw_list.push_hit(handle, HitTarget::SidebarResizeHandle, z + 5);
    }

    // Check if any unfocused tab across all hosts needs attention (drives Activity Eye badge dot).
    let any_attention = hosts.iter().any(|h| {
        h.sessions
            .iter()
            .any(|s| s.tabs.iter().any(SidebarTabView::needs_attention))
    });

    // 2. Top utility bar (`36px`, opaque surface + high hit z-order)
    let top_rect = RectF::new(
        sidebar_rect.x,
        sidebar_rect.y,
        sidebar_rect.w,
        SIDEBAR_TOP_H,
    );
    render_top_bar(
        draw_list,
        top_rect,
        state,
        any_attention,
        effective_scroll_y > 1.0,
        interaction,
        &tokens,
        theme.rounding,
        z,
    );

    let mut content_top = top_rect.bottom();

    // 3. Optional inline filter field (`34px`)
    if state.filter_open {
        let filter_rect = RectF::new(
            sidebar_rect.x + 8.0,
            content_top + 2.0,
            (sidebar_rect.w - 16.0).max(0.0),
            28.0,
        );
        let filter_focused = matches!(focus, FocusTarget::SidebarFilter);
        let border = if filter_focused {
            tokens.border_active
        } else if interaction.is_hovered(&HitTarget::SidebarFilterInput) {
            tokens.border_subtle
        } else {
            tokens.border_hairline
        };
        draw_list.push_rect(
            filter_rect,
            tokens.surface_pane,
            Some(border),
            1.0,
            cap_radius(6.0, theme.rounding),
            None,
        );
        draw_list.push_icon(
            RectF::new(filter_rect.x + 7.0, filter_rect.y + 7.0, 14.0, 14.0),
            IconId::Search,
            if filter_focused {
                tokens.accent_primary
            } else {
                tokens.fg_muted
            },
        );
        let text_rect = RectF::new(
            filter_rect.x + 26.0,
            filter_rect.y + 5.0,
            (filter_rect.w - 32.0).max(0.0),
            18.0,
        );
        let (display, color) = if state.filter_input.is_empty() {
            (state.filter_input.placeholder().to_owned(), tokens.fg_muted)
        } else {
            (state.filter_input.text().to_owned(), tokens.fg_primary)
        };
        draw_list.push_text(
            text_rect,
            display,
            color,
            FontKind::Sans,
            12.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        if filter_focused {
            let caret_offset = (state.filter_input.text().chars().count() as f32 * 6.5)
                .min((text_rect.w - 2.0).max(0.0));
            draw_list.push_rect(
                RectF::new(text_rect.x + caret_offset, text_rect.y + 2.0, 1.5, 14.0),
                tokens.accent_primary,
                None,
                0.0,
                0.75,
                None,
            );
        }
        draw_list.push_hit(filter_rect, HitTarget::SidebarFilterInput, z + 4);
        content_top += SIDEBAR_FILTER_H;
    }

    // 4. Optional Activity mode sort toggle header (`28px`)
    if state.activity_mode {
        let bar_rect = RectF::new(
            sidebar_rect.x + 8.0,
            content_top + 2.0,
            (sidebar_rect.w - 16.0).max(0.0),
            24.0,
        );
        draw_list.push_text(
            RectF::new(
                bar_rect.x + 4.0,
                bar_rect.y + 4.0,
                (bar_rect.w * 0.5).max(0.0),
                16.0,
            ),
            "Activity",
            tokens.fg_muted,
            FontKind::Sans,
            11.0,
            FontWeight::SemiBold,
            TextAlign::Left,
            true,
        );
        let pill_w = 78.0_f32.min(bar_rect.w * 0.5);
        let pill_rect = RectF::new(bar_rect.right() - pill_w, bar_rect.y + 2.0, pill_w, 20.0);
        let sort_hovered = interaction.is_hovered(&HitTarget::ActivitySortToggle);
        let sort_pressed = interaction.is_pressed(&HitTarget::ActivitySortToggle);
        let pill_fill = if sort_pressed {
            tokens.surface_row_selected
        } else if sort_hovered {
            tokens.surface_row_hover
        } else {
            tokens.surface_row_active
        };
        draw_list.push_rect(
            pill_rect,
            pill_fill,
            Some(if sort_hovered {
                tokens.border_subtle
            } else {
                tokens.border_hairline
            }),
            1.0,
            cap_radius(5.0, theme.rounding),
            None,
        );
        let sort_label = match state.activity_sort {
            ActivitySortMode::ByPriority => "Priority",
            ActivitySortMode::BySession => "Session",
        };
        draw_list.push_text(
            pill_rect,
            sort_label,
            if sort_hovered {
                tokens.fg_primary
            } else {
                tokens.fg_secondary
            },
            FontKind::Sans,
            10.5,
            FontWeight::Medium,
            TextAlign::Center,
            true,
        );
        draw_list.push_hit(pill_rect, HitTarget::ActivitySortToggle, z + 4);
        content_top += SIDEBAR_ACTIVITY_BAR_H;
    }

    // 5. Scrollable tree area (clipped strictly between top controls and bottom footer)
    let footer_y = (sidebar_rect.bottom() - SIDEBAR_FOOTER_H).max(content_top);
    let scroll_rect = RectF::new(
        sidebar_rect.x,
        content_top,
        sidebar_rect.w,
        (footer_y - content_top).max(0.0),
    );

    if !scroll_rect.is_empty() {
        draw_list.push_clip(scroll_rect);
        let filter_query = state.filter_input.text().trim().to_ascii_lowercase();
        if state.activity_mode && state.activity_sort == ActivitySortMode::ByPriority {
            render_activity_priority_list(
                draw_list,
                scroll_rect,
                effective_scroll_y,
                hosts,
                &filter_query,
                state,
                interaction,
                &tokens,
                theme.rounding,
                anim_phase,
                z,
            );
        } else {
            render_host_session_tree(
                draw_list,
                scroll_rect,
                effective_scroll_y,
                hosts,
                &filter_query,
                state,
                interaction,
                &tokens,
                theme.rounding,
                anim_phase,
                z,
                tab_reorder,
            );
        }

        // Soft bottom edge feather when more scrollable tabs continue below the viewport
        // so clipped tab cards fade smoothly into the footer bar instead of harsh half-cuts.
        if scroll_metrics.max_scroll > 0.5 && effective_scroll_y < scroll_metrics.max_scroll - 1.0 {
            let fade_h = 12.0_f32.min(scroll_rect.h * 0.25);
            let step_h = fade_h / 3.0;
            for (step, alpha) in [(0, 0.28_f32), (1, 0.58), (2, 0.88)] {
                draw_list.push_rect(
                    RectF::new(
                        scroll_rect.x,
                        scroll_rect.bottom() - fade_h + step as f32 * step_h,
                        (scroll_rect.w - 1.0).max(0.0),
                        step_h + 0.5,
                    ),
                    tokens.surface_sidebar.with_alpha_f32(alpha),
                    None,
                    0.0,
                    0.0,
                    None,
                );
            }
        }

        // Subtle overlay scroll indicator when sidebar content overflows its viewport.
        if scroll_metrics.max_scroll > 0.5 && scroll_metrics.content_height > 0.0 {
            let track_h = (scroll_rect.h - 10.0).max(16.0);
            let min_thumb = 24.0_f32.min(track_h);
            let thumb_h = ((scroll_rect.h / scroll_metrics.content_height) * track_h)
                .clamp(min_thumb, track_h);
            let ratio = (effective_scroll_y / scroll_metrics.max_scroll).clamp(0.0, 1.0);
            let thumb_y = scroll_rect.y + 5.0 + ratio * (track_h - thumb_h);
            let thumb_x = (scroll_rect.right() - 5.0).max(scroll_rect.x);
            draw_list.push_rect(
                RectF::new(thumb_x, thumb_y, 3.0, thumb_h),
                tokens.fg_muted.with_alpha_f32(0.36),
                None,
                0.0,
                1.5,
                None,
            );
        }
        draw_list.pop_clip();
    }

    // 6. Bottom utility bar (`38px`, opaque backdrop + crisp search / + Session / settings / hosts)
    let footer_rect = RectF::new(
        sidebar_rect.x,
        footer_y,
        (sidebar_rect.w - if is_right_side { 0.0 } else { 1.0 }).max(0.0),
        (sidebar_rect.bottom() - footer_y).max(0.0),
    );
    render_footer_bar(
        draw_list,
        footer_rect,
        state,
        interaction,
        &tokens,
        theme.rounding,
        z,
    );
    scroll_metrics
}

#[allow(clippy::too_many_arguments)]
fn render_top_bar(
    draw_list: &mut UiDrawList,
    top_rect: RectF,
    state: &SidebarState,
    any_attention: bool,
    scrolled: bool,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    z: u16,
) {
    draw_list.push_rect(
        RectF::new(
            top_rect.x,
            top_rect.y,
            (top_rect.w - 1.0).max(0.0),
            top_rect.h,
        ),
        tokens.surface_sidebar,
        None,
        0.0,
        0.0,
        None,
    );
    if scrolled {
        draw_list.push_rect(
            RectF::new(
                top_rect.x,
                (top_rect.bottom() - 1.0).max(top_rect.y),
                (top_rect.w - 1.0).max(0.0),
                1.0,
            ),
            tokens.border_hairline,
            None,
            0.0,
            0.0,
            None,
        );
    }

    let btn_r = cap_radius(6.0, theme_rounding);
    let toggle_btn = RectF::new(top_rect.x + 8.0, top_rect.y + 5.0, 26.0, 26.0);
    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::SidebarToggle, false, tokens)
    {
        draw_list.push_rect(toggle_btn, fill, border, 1.0, btn_r, None);
    }
    let toggle_tint = if interaction.is_hovered(&HitTarget::SidebarToggle)
        || interaction.is_pressed(&HitTarget::SidebarToggle)
    {
        tokens.fg_primary
    } else {
        tokens.fg_secondary
    };
    draw_list.push_icon(toggle_btn.inset_uniform(5.0), IconId::Sidebar, toggle_tint);
    draw_list.push_hit(toggle_btn, HitTarget::SidebarToggle, z + 4);

    // Right side: Activity Eye + `+` New Terminal (real Ghostty shell!)
    let new_term_btn = RectF::new(
        (top_rect.right() - 34.0).max(top_rect.x),
        top_rect.y + 5.0,
        26.0,
        26.0,
    );
    let new_term_hot =
        interaction.is_hovered(&HitTarget::TabNew) || interaction.is_pressed(&HitTarget::TabNew);
    if let Some((fill, border)) = interaction.button_surface(&HitTarget::TabNew, false, tokens) {
        let border = if new_term_hot {
            Some(tokens.accent_primary.with_alpha_f32(0.45))
        } else {
            border
        };
        draw_list.push_rect(new_term_btn, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(
        new_term_btn.inset_uniform(5.0),
        IconId::Plus,
        if new_term_hot {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    draw_list.push_hit(new_term_btn, HitTarget::TabNew, z + 4);

    let eye_btn = RectF::new(
        (new_term_btn.x - 30.0).max(top_rect.x),
        top_rect.y + 5.0,
        26.0,
        26.0,
    );
    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::ActivityToggle, state.activity_mode, tokens)
    {
        draw_list.push_rect(eye_btn, fill, border, 1.0, btn_r, None);
    }
    let eye_tint = if state.activity_mode {
        tokens.accent_primary
    } else if interaction.is_hovered(&HitTarget::ActivityToggle) {
        tokens.fg_primary
    } else {
        tokens.fg_secondary
    };
    draw_list.push_icon(eye_btn.inset_uniform(5.0), IconId::Activity, eye_tint);
    if any_attention {
        draw_list.push_rect(
            RectF::new(eye_btn.right() - 7.0, eye_btn.y + 3.0, 6.0, 6.0),
            tokens.accent_primary,
            Some(tokens.surface_sidebar),
            1.0,
            3.0,
            None,
        );
    }
    draw_list.push_hit(eye_btn, HitTarget::ActivityToggle, z + 4);
}

#[allow(clippy::too_many_arguments)]
fn render_footer_bar(
    draw_list: &mut UiDrawList,
    footer_rect: RectF,
    state: &SidebarState,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    z: u16,
) {
    if footer_rect.is_empty() {
        return;
    }
    let btn_r = cap_radius(6.0, theme_rounding);
    // Opaque footer fill guarantees scrolled tabs can never visually bleed under footer icons.
    draw_list.push_rect(footer_rect, tokens.surface_sidebar, None, 0.0, 0.0, None);
    draw_list.push_rect(
        RectF::new(footer_rect.x, footer_rect.y, footer_rect.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let btn_y = footer_rect.y + ((footer_rect.h - 26.0) * 0.5).max(1.0);

    // Left: Filter / Search button (crisp contrast + hover pill)
    let filter_btn = RectF::new(footer_rect.x + 8.0, btn_y, 26.0, 26.0);
    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::SidebarFilterToggle, state.filter_open, tokens)
    {
        draw_list.push_rect(filter_btn, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(
        filter_btn.inset_uniform(4.5),
        IconId::Search,
        if state.filter_open {
            tokens.accent_primary
        } else if interaction.is_hovered(&HitTarget::SidebarFilterToggle) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    draw_list.push_hit(filter_btn, HitTarget::SidebarFilterToggle, z + 4);

    // Right cluster: [Hosts Globe] [Settings Gear]
    let hosts_btn = RectF::new(
        (footer_rect.right() - 34.0).max(footer_rect.x),
        btn_y,
        26.0,
        26.0,
    );
    if let Some((fill, border)) = interaction.button_surface(&HitTarget::HostsButton, false, tokens)
    {
        draw_list.push_rect(hosts_btn, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(
        hosts_btn.inset_uniform(4.5),
        IconId::Globe,
        if interaction.is_hovered(&HitTarget::HostsButton) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    draw_list.push_hit(hosts_btn, HitTarget::HostsButton, z + 4);

    let settings_btn = RectF::new((hosts_btn.x - 28.0).max(footer_rect.x), btn_y, 26.0, 26.0);
    if settings_btn.x > filter_btn.right() + 34.0 {
        if let Some((fill, border)) =
            interaction.button_surface(&HitTarget::SettingsButton, false, tokens)
        {
            draw_list.push_rect(settings_btn, fill, border, 1.0, btn_r, None);
        }
        draw_list.push_icon(
            settings_btn.inset_uniform(5.0),
            IconId::Gear,
            if interaction.is_hovered(&HitTarget::SettingsButton) {
                tokens.fg_primary
            } else {
                tokens.fg_secondary
            },
        );
        draw_list.push_hit(settings_btn, HitTarget::SettingsButton, z + 4);
    }

    // Center/Right: explicit `+ Session` pill button (collapses gracefully to `+` icon when narrow)
    let right_anchor = if settings_btn.x > filter_btn.right() + 34.0 {
        settings_btn.x - 6.0
    } else {
        hosts_btn.x - 4.0
    };
    let avail_mid = (right_anchor - (filter_btn.right() + 6.0)).max(0.0);
    let new_sess_hot = interaction.is_hovered(&HitTarget::NewSessionButton)
        || interaction.is_pressed(&HitTarget::NewSessionButton);
    if avail_mid >= 92.0 {
        let pill_w = avail_mid.min(116.0);
        let new_sess_btn = RectF::new(right_anchor - pill_w, btn_y + 1.0, pill_w, 24.0);
        let pill_fill = if interaction.is_pressed(&HitTarget::NewSessionButton) {
            tokens.surface_row_selected
        } else if interaction.is_hovered(&HitTarget::NewSessionButton) {
            tokens.surface_row_active
        } else {
            tokens.surface_row_hover
        };
        let pill_border = if new_sess_hot {
            tokens.accent_primary.with_alpha_f32(0.45)
        } else {
            tokens.border_hairline
        };
        draw_list.push_rect(new_sess_btn, pill_fill, Some(pill_border), 1.0, btn_r, None);
        draw_list.push_icon(
            RectF::new(new_sess_btn.x + 7.0, new_sess_btn.y + 5.0, 14.0, 14.0),
            IconId::Plus,
            if new_sess_hot {
                tokens.accent_primary
            } else {
                tokens.fg_secondary
            },
        );
        draw_list.push_text(
            RectF::new(
                new_sess_btn.x + 24.0,
                new_sess_btn.y + 4.0,
                (new_sess_btn.w - 30.0).max(0.0),
                16.0,
            ),
            "New Session",
            if new_sess_hot {
                tokens.fg_primary
            } else {
                tokens.fg_secondary
            },
            FontKind::Sans,
            11.0,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );
        draw_list.push_hit(new_sess_btn, HitTarget::NewSessionButton, z + 4);
    } else if avail_mid >= 24.0 {
        let new_sess_btn = RectF::new((right_anchor - 26.0).max(footer_rect.x), btn_y, 26.0, 26.0);
        if let Some((fill, border)) =
            interaction.button_surface(&HitTarget::NewSessionButton, false, tokens)
        {
            draw_list.push_rect(new_sess_btn, fill, border, 1.0, btn_r, None);
        }
        draw_list.push_icon(
            new_sess_btn.inset_uniform(5.0),
            IconId::Plus,
            if new_sess_hot {
                tokens.fg_primary
            } else {
                tokens.fg_secondary
            },
        );
        draw_list.push_hit(new_sess_btn, HitTarget::NewSessionButton, z + 4);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_host_session_tree(
    draw_list: &mut UiDrawList,
    scroll_rect: RectF,
    scroll_offset_y: f32,
    hosts: &[SidebarHostView],
    filter_query: &str,
    state: &SidebarState,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    anim_phase: f32,
    z: u16,
    tab_reorder: Option<&TabReorderOverlay>,
) {
    let base_y = scroll_rect.y + SIDEBAR_CONTENT_PAD - scroll_offset_y.max(0.0);
    let inner_x = scroll_rect.x + 8.0;
    let inner_w = (scroll_rect.w - 16.0).max(0.0);
    let row_r = cap_radius(6.0, theme_rounding);

    // Pre-scan for a sticky session header: when an expanded session's natural header has
    // scrolled above `scroll_rect.y` while its tabs remain visible inside `scroll_rect`,
    // pin that session header cleanly at `scroll_rect.y` so collapsing long sessions
    // (e.g. `T3 Code` scrolled to tab #26) is always 1 click away!
    let mut sticky_session: Option<(&SidebarHostView, &SidebarSessionView, f32)> = None;
    if scroll_offset_y > 0.5 && scroll_rect.h >= STICKY_SESSION_BAR_H {
        let mut scan_y = base_y;
        for (host_idx, host) in hosts.iter().enumerate() {
            if host_idx > 0 {
                scan_y += HOST_GROUP_GAP;
            }
            scan_y += HOST_ROW_ADVANCE;
            if host.collapsed {
                continue;
            }
            for session in &host.sessions {
                let session_matches_filter = filter_query.is_empty()
                    || session.label.to_ascii_lowercase().contains(filter_query)
                    || host.label.to_ascii_lowercase().contains(filter_query);
                let matching_count = session
                    .tabs
                    .iter()
                    .filter(|t| {
                        let activity_ok = !state.activity_mode
                            || t.is_active_in_activity_view(state.include_idle_in_activity);
                        let filter_ok =
                            session_matches_filter || tab_matches_query(t, filter_query);
                        activity_ok && filter_ok
                    })
                    .count();
                if !filter_query.is_empty() && !session_matches_filter && matching_count == 0 {
                    continue;
                }
                if state.activity_mode && matching_count == 0 {
                    continue;
                }

                let sess_top = scan_y;
                scan_y += SESSION_ROW_ADVANCE;
                if session.collapsed {
                    continue;
                }
                let sess_bottom = scan_y + matching_count as f32 * TAB_ROW_ADVANCE;
                if matching_count > 0
                    && sess_top < scroll_rect.y + 1.0
                    && sess_bottom > scroll_rect.y + 4.0
                {
                    let sticky_top = (sess_bottom - STICKY_SESSION_BAR_H).min(scroll_rect.y);
                    sticky_session = Some((host, session, sticky_top));
                }
                scan_y = sess_bottom;
            }
        }
    }

    // Clip all scrolling content strictly below any active sticky session header so scrolled
    // tab cards never bleed underneath or steal clicks from the pinned session row.
    let content_clip = match sticky_session {
        Some((_, _, sticky_top)) => {
            let cut_y =
                (sticky_top + STICKY_SESSION_BAR_H).clamp(scroll_rect.y, scroll_rect.bottom());
            RectF::new(
                scroll_rect.x,
                cut_y,
                scroll_rect.w,
                (scroll_rect.bottom() - cut_y).max(0.0),
            )
        }
        None => scroll_rect,
    };

    if !content_clip.is_empty() {
        draw_list.push_clip(content_clip);
        let mut y = base_y;
        for (host_idx, host) in hosts.iter().enumerate() {
            if host_idx > 0 {
                y += HOST_GROUP_GAP;
            }
            let host_rect = RectF::new(inner_x, y, inner_w, 26.0);
            if host_rect.bottom() >= content_clip.y && host_rect.y <= content_clip.bottom() {
                let host_target = HitTarget::HostHeader {
                    host_id: host.host_id.clone(),
                };
                let host_plus_target = HitTarget::HostNewSession {
                    host_id: host.host_id.clone(),
                };
                let host_hot = interaction.is_hovered(&host_target)
                    || interaction.is_hovered(&host_plus_target);
                if let Some((fill, _)) = interaction.button_surface(&host_target, false, tokens) {
                    draw_list.push_rect(host_rect, fill, None, 0.0, row_r, None);
                } else if host_hot {
                    draw_list.push_rect(
                        host_rect,
                        tokens.surface_row_hover,
                        None,
                        0.0,
                        row_r,
                        None,
                    );
                }
                let host_icon = if host.is_local {
                    IconId::Terminal
                } else {
                    IconId::Server
                };
                let host_chev = if host.collapsed {
                    IconId::ChevronRight
                } else {
                    IconId::ChevronDown
                };
                draw_list.push_icon(
                    RectF::new(host_rect.x + 3.0, host_rect.y + 7.5, 11.0, 11.0),
                    host_chev,
                    if host_hot {
                        tokens.fg_secondary
                    } else {
                        tokens.fg_muted
                    },
                );
                draw_list.push_icon(
                    RectF::new(host_rect.x + 16.0, host_rect.y + 6.0, 14.0, 14.0),
                    host_icon,
                    if host_hot {
                        tokens.fg_primary
                    } else {
                        tokens.fg_secondary
                    },
                );
                draw_list.push_text(
                    RectF::new(
                        host_rect.x + 35.0,
                        host_rect.y + 4.0,
                        (host_rect.w - 74.0).max(0.0),
                        18.0,
                    ),
                    &host.label,
                    tokens.fg_primary,
                    FontKind::Sans,
                    12.5,
                    FontWeight::Medium,
                    TextAlign::Left,
                    true,
                );
                let host_plus_rect = RectF::new(
                    (host_rect.right() - 36.0).max(host_rect.x),
                    host_rect.y + 3.0,
                    20.0,
                    20.0,
                );
                if let Some((fill, stroke)) =
                    interaction.button_surface(&host_plus_target, false, tokens)
                {
                    draw_list.push_rect(host_plus_rect, fill, stroke, 1.0, row_r, None);
                }
                draw_list.push_icon(
                    RectF::new(host_plus_rect.x + 4.0, host_plus_rect.y + 4.0, 12.0, 12.0),
                    IconId::Plus,
                    if interaction.is_hovered(&host_plus_target) {
                        tokens.fg_primary
                    } else if host_hot {
                        tokens.fg_secondary
                    } else {
                        tokens.fg_muted
                    },
                );
                let conn_color = match host.connection {
                    HostConnectionState::Online => tokens.status_done,
                    HostConnectionState::Reconnecting => tokens.status_blocked,
                    HostConnectionState::Offline => tokens.status_error,
                };
                draw_list.push_rect(
                    RectF::new(
                        (host_rect.right() - 12.0).max(host_rect.x),
                        host_rect.y + 10.0,
                        6.0,
                        6.0,
                    ),
                    conn_color,
                    None,
                    0.0,
                    3.0,
                    None,
                );
                draw_list.push_hit(host_rect.intersect(content_clip), host_target, z + 1);
                let plus_hit = host_plus_rect.intersect(content_clip);
                if !plus_hit.is_empty() {
                    draw_list.push_hit(plus_hit, host_plus_target, z + 2);
                }
            }
            y += HOST_ROW_ADVANCE;

            if host.collapsed {
                continue;
            }

            for session in &host.sessions {
                let session_matches_filter = filter_query.is_empty()
                    || session.label.to_ascii_lowercase().contains(filter_query)
                    || host.label.to_ascii_lowercase().contains(filter_query);

                let matching_tabs: Vec<&SidebarTabView> = session
                    .tabs
                    .iter()
                    .filter(|t| {
                        let activity_ok = !state.activity_mode
                            || t.is_active_in_activity_view(state.include_idle_in_activity);
                        let filter_ok =
                            session_matches_filter || tab_matches_query(t, filter_query);
                        activity_ok && filter_ok
                    })
                    .collect();

                if !filter_query.is_empty() && !session_matches_filter && matching_tabs.is_empty() {
                    continue;
                }
                if state.activity_mode && matching_tabs.is_empty() {
                    continue;
                }

                let sess_rect = RectF::new(inner_x, y, inner_w, SESSION_ROW_HEIGHT);
                if sess_rect.bottom() >= content_clip.y && sess_rect.y <= content_clip.bottom() {
                    render_session_header(
                        draw_list,
                        sess_rect,
                        content_clip,
                        host,
                        session,
                        state,
                        interaction,
                        tokens,
                        theme_rounding,
                        z,
                    );
                }
                y += SESSION_ROW_ADVANCE;

                if session.collapsed {
                    continue;
                }

                // Preserve exact server tab order (critical for `tr3x` T3 Code priority sorting!).
                for tab in matching_tabs {
                    let row_rect = RectF::new(inner_x, y, inner_w, TAB_ROW_HEIGHT);
                    if row_rect.bottom() >= content_clip.y && row_rect.y <= content_clip.bottom() {
                        render_tab_card(
                            draw_list,
                            row_rect,
                            content_clip,
                            &host.host_id,
                            &session.session_id,
                            tab,
                            state,
                            interaction,
                            tokens,
                            theme_rounding,
                            anim_phase,
                            z,
                        );
                        if let Some(overlay) = tab_reorder
                            && overlay.host_id == host.host_id
                            && overlay.session_id == session.session_id
                            && overlay.dragged_window_id == tab.window_id
                        {
                            draw_list.push_rect(
                                row_rect,
                                tokens.accent_primary.with_alpha_f32(0.10),
                                Some(tokens.accent_primary.with_alpha_f32(0.70)),
                                1.25,
                                cap_radius(8.0, theme_rounding),
                                None,
                            );
                        }
                    }
                    y += TAB_ROW_ADVANCE;
                }
            }
        }
        if let Some(overlay) = tab_reorder
            && let Some(ind) = overlay.indicator_rect
            && ind.bottom() >= content_clip.y
            && ind.y <= content_clip.bottom()
        {
            draw_list.push_rect(ind, tokens.accent_primary, None, 0.0, 1.25, None);
            let cy = ind.y + ind.h * 0.5;
            draw_list.push_rect(
                RectF::new(ind.x - 2.0, cy - 2.5, 5.0, 5.0),
                tokens.accent_primary,
                None,
                0.0,
                2.5,
                None,
            );
            draw_list.push_rect(
                RectF::new(ind.right() - 3.0, cy - 2.5, 5.0, 5.0),
                tokens.accent_primary,
                None,
                0.0,
                2.5,
                None,
            );
        }
        draw_list.pop_clip();
    }

    // Paint the sticky session header cleanly pinned at the top of `scroll_rect` with an opaque
    // `surface_sidebar` backdrop, 1px bottom hairline, and elevated hit z-order (`z + 3`).
    if let Some((sticky_host, sticky_sess, sticky_top)) = sticky_session {
        let bar_rect = RectF::new(
            scroll_rect.x,
            sticky_top,
            (scroll_rect.w - 1.0).max(0.0),
            STICKY_SESSION_BAR_H,
        );
        draw_list.push_rect(bar_rect, tokens.surface_sidebar, None, 0.0, 0.0, None);
        draw_list.push_rect(
            RectF::new(
                bar_rect.x,
                (bar_rect.bottom() - 1.0).max(bar_rect.y),
                bar_rect.w,
                1.0,
            ),
            tokens.border_hairline,
            None,
            0.0,
            0.0,
            None,
        );
        let pinned_rect = RectF::new(inner_x, sticky_top + 3.0, inner_w, SESSION_ROW_HEIGHT);
        render_session_header(
            draw_list,
            pinned_rect,
            scroll_rect,
            sticky_host,
            sticky_sess,
            state,
            interaction,
            tokens,
            theme_rounding,
            z + 2,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_activity_priority_list(
    draw_list: &mut UiDrawList,
    scroll_rect: RectF,
    scroll_offset_y: f32,
    hosts: &[SidebarHostView],
    filter_query: &str,
    state: &SidebarState,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    anim_phase: f32,
    z: u16,
) {
    // Collect active tabs across all hosts and sort strictly by OSC 7501 priority rank
    // while keeping server order within identical priority tiers.
    let mut entries: Vec<(u8, usize, &str, &str, &SidebarTabView)> = Vec::new();
    let mut order = 0usize;
    for host in hosts {
        for session in &host.sessions {
            for tab in &session.tabs {
                if !tab.is_active_in_activity_view(state.include_idle_in_activity) {
                    continue;
                }
                if !filter_query.is_empty() && !tab_matches_query(tab, filter_query) {
                    continue;
                }
                let rank = tab.card.state.priority_rank(tab.unread);
                entries.push((rank, order, &host.host_id, &session.session_id, tab));
                order += 1;
            }
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut y = scroll_rect.y + SIDEBAR_CONTENT_PAD - scroll_offset_y.max(0.0);
    let inner_x = scroll_rect.x + 8.0;
    let inner_w = (scroll_rect.w - 16.0).max(0.0);

    for (_, _, host_id, session_id, tab) in entries {
        let row_rect = RectF::new(inner_x, y, inner_w, TAB_ROW_HEIGHT);
        if row_rect.bottom() >= scroll_rect.y && row_rect.y <= scroll_rect.bottom() {
            render_tab_card(
                draw_list,
                row_rect,
                scroll_rect,
                host_id,
                session_id,
                tab,
                state,
                interaction,
                tokens,
                theme_rounding,
                anim_phase,
                z,
            );
        }
        y += TAB_ROW_ADVANCE;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_session_header(
    draw_list: &mut UiDrawList,
    rect: RectF,
    scroll_rect: RectF,
    host: &SidebarHostView,
    session: &SidebarSessionView,
    state: &SidebarState,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    z: u16,
) {
    let is_t3 = session.is_t3_session();
    let chev_target = HitTarget::SessionChevron {
        host_id: host.host_id.clone(),
        session_id: session.session_id.clone(),
    };
    let row_target = HitTarget::SessionRow {
        host_id: host.host_id.clone(),
        session_id: session.session_id.clone(),
    };
    let plus_target = if is_t3 {
        HitTarget::SessionNewT3Thread {
            host_id: host.host_id.clone(),
            session_id: session.session_id.clone(),
        }
    } else {
        HitTarget::SessionNewTab {
            host_id: host.host_id.clone(),
            session_id: session.session_id.clone(),
        }
    };
    let bell_target = HitTarget::SessionMuteBell {
        host_id: host.host_id.clone(),
        session_id: session.session_id.clone(),
    };

    let hovered = state.hovered_session_id.as_deref() == Some(session.session_id.as_str())
        || interaction.is_hovered(&row_target)
        || interaction.is_hovered(&chev_target)
        || interaction.is_hovered(&plus_target)
        || interaction.is_hovered(&bell_target);
    let row_pressed = interaction.is_pressed(&row_target) || interaction.is_pressed(&chev_target);
    let row_r = cap_radius(6.0, theme_rounding);
    let btn_r = cap_radius(4.0, theme_rounding);

    if row_pressed {
        draw_list.push_rect(
            rect,
            tokens.surface_row_selected,
            Some(tokens.border_hairline),
            1.0,
            row_r,
            None,
        );
    } else if session.active || hovered {
        let fill = if session.active && hovered {
            tokens.surface_row_active.with_alpha_f32(0.85)
        } else if session.active {
            tokens.surface_row_active.with_alpha_f32(0.55)
        } else {
            tokens.surface_row_hover
        };
        let border = (session.collapsed && session.active).then_some(tokens.border_hairline);
        draw_list.push_rect(
            rect,
            fill,
            border,
            if border.is_some() { 1.0 } else { 0.0 },
            row_r,
            None,
        );
    }

    // Dedicated clickable chevron button (`v` / `>`) — ALWAYS toggles collapse cleanly!
    let chev_rect = RectF::new(rect.x + 2.0, rect.y + 3.0, 20.0, 20.0);
    if let Some((fill, border)) = interaction.button_surface(&chev_target, false, tokens) {
        draw_list.push_rect(chev_rect, fill, border, 1.0, btn_r, None);
    }
    let chev = if session.collapsed {
        IconId::ChevronRight
    } else {
        IconId::ChevronDown
    };
    let chev_tint = if interaction.is_hovered(&chev_target) || hovered {
        tokens.fg_primary
    } else {
        tokens.fg_secondary
    };
    draw_list.push_icon(chev_rect.inset_uniform(4.0), chev, chev_tint);
    draw_list.push_hit(chev_rect.intersect(scroll_rect), chev_target.clone(), z + 3);

    // Right-side controls laid out right-to-left: [Bell] [+] [Collapsed/Attention Pill]
    let bell_rect = RectF::new((rect.right() - 21.0).max(rect.x), rect.y + 3.0, 19.0, 20.0);
    let plus_rect = RectF::new((bell_rect.x - 21.0).max(rect.x), rect.y + 3.0, 19.0, 20.0);
    let mut right_edge = plus_rect.x - 4.0;

    // Inline mute bell toggle
    let show_bell = session.muted || hovered;
    if show_bell {
        if let Some((fill, border)) = interaction.button_surface(&bell_target, false, tokens) {
            draw_list.push_rect(bell_rect, fill, border, 1.0, btn_r, None);
        }
        let bell_icon = if session.muted {
            IconId::BellOff
        } else {
            IconId::Bell
        };
        let bell_tint = if session.muted {
            tokens.status_blocked
        } else if interaction.is_hovered(&bell_target) {
            tokens.fg_primary
        } else {
            tokens.fg_muted
        };
        draw_list.push_icon(bell_rect.inset_uniform(3.5), bell_icon, bell_tint);
    }
    draw_list.push_hit(bell_rect.intersect(scroll_rect), bell_target, z + 3);

    // Inline `+` button (normal session: `+ New Terminal Tab`; `T3 Code`: accent-tinted `New T3 Thread`)
    let show_plus = hovered || session.active;
    if show_plus {
        let plus_hot = interaction.is_hovered(&plus_target) || interaction.is_pressed(&plus_target);
        if let Some((fill, border)) = interaction.button_surface(&plus_target, false, tokens) {
            let border = if is_t3 && plus_hot {
                Some(tokens.accent_primary.with_alpha_f32(0.5))
            } else {
                border
            };
            draw_list.push_rect(plus_rect, fill, border, 1.0, btn_r, None);
        }
        let plus_tint = if is_t3 && plus_hot {
            tokens.accent_primary
        } else if plus_hot {
            tokens.fg_primary
        } else if hovered {
            tokens.fg_secondary
        } else {
            tokens.fg_muted.with_alpha_f32(0.75)
        };
        draw_list.push_icon(plus_rect.inset_uniform(3.5), IconId::Plus, plus_tint);
    }
    draw_list.push_hit(plus_rect.intersect(scroll_rect), plus_target, z + 3);

    // Compact badge pill:
    // - When collapsed: tab count pill (e.g. `26`) + colored status dot if urgent/working!
    // - When expanded with urgent items: attention count pill!
    let attn = session.attention_count();
    if session.collapsed {
        let count_str = session.tabs.len().to_string();
        let status_dot = session.highest_status_dot(tokens);
        let dot_space = if status_dot.is_some() { 10.0 } else { 0.0 };
        let pill_w = ((count_str.len() as f32) * 6.5 + 12.0 + dot_space).max(22.0);
        let pill_rect = RectF::new(
            (right_edge - pill_w).max(rect.x),
            rect.y + 4.0,
            pill_w,
            18.0,
        );
        let pill_r =
            cap_radius(9.0, theme_rounding).max(if theme_rounding > 0.0 { 4.0 } else { 0.0 });
        let pill_border = status_dot
            .map(|c| c.with_alpha_f32(0.55))
            .unwrap_or(tokens.border_subtle);
        draw_list.push_rect(
            pill_rect,
            tokens.surface_row_active,
            Some(pill_border),
            1.0,
            pill_r,
            None,
        );
        if let Some(dot_color) = status_dot {
            draw_list.push_rect(
                RectF::new(pill_rect.x + 6.0, pill_rect.y + 6.0, 6.0, 6.0),
                dot_color,
                None,
                0.0,
                3.0,
                None,
            );
        }
        let text_x = pill_rect.x + if status_dot.is_some() { 14.0 } else { 0.0 };
        let text_w =
            (pill_rect.right() - text_x - if status_dot.is_some() { 4.0 } else { 0.0 }).max(0.0);
        draw_list.push_text(
            RectF::new(text_x, pill_rect.y, text_w, pill_rect.h),
            count_str,
            tokens.fg_primary,
            FontKind::Mono,
            10.0,
            FontWeight::SemiBold,
            TextAlign::Center,
            false,
        );
        right_edge = pill_rect.x - 6.0;
    } else if attn > 0 {
        let attn_str = attn.to_string();
        let pill_w = ((attn_str.len() as f32) * 6.5 + 11.0).max(18.0);
        let badge_rect = RectF::new(
            (right_edge - pill_w).max(rect.x),
            rect.y + 5.0,
            pill_w,
            16.0,
        );
        draw_list.push_rect(
            badge_rect,
            tokens.accent_primary,
            None,
            0.0,
            cap_radius(8.0, theme_rounding).max(if theme_rounding > 0.0 { 4.0 } else { 0.0 }),
            None,
        );
        draw_list.push_text(
            badge_rect,
            attn_str,
            tokens.accent_foreground,
            FontKind::Mono,
            9.5,
            FontWeight::Bold,
            TextAlign::Center,
            false,
        );
        right_edge = badge_rect.x - 6.0;
    }

    let label_x = rect.x + 24.0;
    let label_w = (right_edge - label_x).max(0.0);
    draw_list.push_text(
        RectF::new(label_x, rect.y + 5.0, label_w, 16.0),
        &session.label,
        if session.active || hovered {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        11.5,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );

    // Clicking the active session header row (e.g. `v T3 Code`) toggles collapse directly;
    // clicking an inactive session header selects it (while its chevron ALWAYS toggles collapse!).
    let body_target = if session.active {
        chev_target
    } else {
        row_target
    };
    draw_list.push_hit(rect.intersect(scroll_rect), body_target, z + 1);
}

#[allow(clippy::too_many_arguments)]
fn render_tab_card(
    draw_list: &mut UiDrawList,
    row_rect: RectF,
    scroll_rect: RectF,
    host_id: &str,
    session_id: &str,
    tab: &SidebarTabView,
    state: &SidebarState,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    anim_phase: f32,
    z: u16,
) {
    let card_target = HitTarget::TabCard {
        host_id: host_id.to_owned(),
        session_id: session_id.to_owned(),
        window_id: tab.window_id.clone(),
    };
    let close_target = HitTarget::TabClose {
        host_id: host_id.to_owned(),
        session_id: session_id.to_owned(),
        window_id: tab.window_id.clone(),
    };
    let hovered = state.hovered_tab_window_id.as_deref() == Some(tab.window_id.as_str())
        || interaction.is_hovered(&card_target)
        || interaction.is_hovered(&close_target);
    let pressed = interaction.is_pressed(&card_target);
    let card_r = cap_radius(8.0, theme_rounding);

    if tab.active {
        let active_fill = blend_rgba(tokens.surface_row_active, tokens.accent_primary, 0.08);
        let active_border = blend_rgba(tokens.border_subtle, tokens.border_active, 0.45);
        draw_list.push_rect(
            row_rect,
            active_fill,
            Some(active_border),
            1.0,
            card_r,
            None,
        );
    } else if pressed {
        draw_list.push_rect(
            row_rect,
            tokens.surface_row_selected,
            Some(tokens.border_subtle),
            1.0,
            card_r,
            None,
        );
    } else if hovered {
        draw_list.push_rect(
            row_rect,
            tokens.surface_row_hover,
            Some(tokens.border_hairline),
            1.0,
            card_r,
            None,
        );
    }

    let indent = if tab.card.is_subagent { 10.0 } else { 0.0 };
    let icon = tab.resolved_icon();
    let tile_rect = RectF::new(
        row_rect.x + 8.0 + indent,
        row_rect.y + (row_rect.h - 26.0) * 0.5,
        26.0,
        26.0,
    );
    let tile_r = cap_radius(6.0, theme_rounding);

    let use_brand_color = match state.icon_color_policy {
        TabIconColorPolicy::Always => true,
        TabIconColorPolicy::ActiveTabs => tab.active,
        TabIconColorPolicy::Never => false,
    };
    let (tile_bg, tile_border, icon_tint) =
        if use_brand_color && let Some([r, g, b]) = icon.brand_rgb() {
            let bg = Rgb::new(r, g, b);
            (bg.opaque(), None, bg.readable_foreground().opaque())
        } else {
            (
                tokens.surface_icon_inactive,
                Some(tokens.border_subtle),
                if tab.active || hovered {
                    tokens.fg_primary
                } else {
                    tokens.fg_secondary
                },
            )
        };

    draw_list.push_rect(
        tile_rect,
        tile_bg,
        tile_border,
        if tile_border.is_some() { 1.0 } else { 0.0 },
        tile_r,
        None,
    );
    draw_list.push_icon(tile_rect.inset_uniform(5.0), icon, icon_tint);

    // Overlaid top-right corner status dot on the 26x26 icon tile
    if let Some(dot_color) = priority_dot_color(tab.card.priority_dot, tab.unread, tokens) {
        let cutout = if tab.active {
            blend_rgba(tokens.surface_row_active, tokens.accent_primary, 0.08)
        } else {
            tokens.surface_sidebar
        };
        draw_list.push_rect(
            RectF::new(tile_rect.right() - 5.0, tile_rect.y - 2.0, 7.0, 7.0),
            dot_color,
            Some(cutout),
            1.5,
            3.5,
            None,
        );
    }

    // Center 2-line text column (Line 1: title, Line 2: `project · Status`)
    let text_x = tile_rect.right() + 10.0;
    let right_col_w = 34.0;
    let text_w = (row_rect.right() - text_x - right_col_w - 6.0).max(0.0);

    let title_color = if tab.active || tab.unread || hovered {
        tokens.fg_primary
    } else {
        tokens.fg_secondary
    };
    let title_weight = if tab.active || tab.unread {
        FontWeight::SemiBold
    } else {
        FontWeight::Regular
    };
    draw_list.push_text(
        RectF::new(text_x, row_rect.y + 8.0, text_w, 17.0),
        &tab.card.title,
        title_color,
        FontKind::Sans,
        12.5,
        title_weight,
        TextAlign::Left,
        true,
    );

    let subtitle_text = if tab.pane_count > 1 && !tab.card.subtitle.is_empty() {
        format!("{} · {} panes", tab.card.subtitle, tab.pane_count)
    } else if tab.pane_count > 1 {
        format!("{} panes", tab.pane_count)
    } else {
        tab.card.subtitle.clone()
    };
    if !subtitle_text.is_empty() {
        draw_list.push_text(
            RectF::new(text_x, row_rect.y + 27.0, text_w, 15.0),
            subtitle_text,
            tokens.fg_muted,
            FontKind::Sans,
            11.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
    }

    // Right column Line 1: Hover `×` close button (with danger hover pill), else animated `•••` pulse or progress bar
    let right_x = (row_rect.right() - right_col_w - 6.0).max(text_x);
    let close_rect = RectF::new(row_rect.right() - 24.0, row_rect.y + 7.0, 18.0, 18.0);
    if hovered {
        let (close_bg, close_tint) =
            interaction.danger_button_style(&close_target, tokens.fg_secondary, tokens);
        if let Some((fill, border)) = close_bg {
            draw_list.push_rect(
                close_rect,
                fill,
                border,
                1.0,
                cap_radius(4.0, theme_rounding),
                None,
            );
        }
        draw_list.push_icon(close_rect.inset_uniform(3.0), IconId::Close, close_tint);
        draw_list.push_hit(close_rect.intersect(scroll_rect), close_target, z + 2);
    } else {
        match tab.card.working_indicator {
            WorkingIndicator::PulseDots => {
                render_working_dots(
                    draw_list,
                    right_x + 12.0,
                    row_rect.y + 15.0,
                    tokens.status_working,
                    anim_phase,
                );
            }
            WorkingIndicator::ProgressPie(pct) => {
                draw_list.push_progress(
                    RectF::new(right_x + 6.0, row_rect.y + 14.0, 26.0, 4.0),
                    f32::from(pct) / 100.0,
                    tokens.status_working,
                    tokens.surface_icon_inactive,
                    2.0,
                );
            }
            WorkingIndicator::None => {
                if tab.muted {
                    draw_list.push_icon(
                        RectF::new(row_rect.right() - 20.0, row_rect.y + 10.0, 12.0, 12.0),
                        IconId::BellOff,
                        tokens.fg_muted,
                    );
                }
            }
        }
    }

    // Right column Line 2: relative age (`now`, `2m`, `12m`, `1h`)
    if let Some(age) = &tab.card.relative_age {
        draw_list.push_text(
            RectF::new(right_x, row_rect.y + 27.0, right_col_w, 15.0),
            age,
            tokens.fg_muted,
            FontKind::Mono,
            10.5,
            FontWeight::Regular,
            TextAlign::Right,
            false,
        );
    }

    draw_list.push_hit(row_rect.intersect(scroll_rect), card_target, z + 1);
}

/// Renders the animated 3-dot (`•••`) Rex working pulse indicator.
pub fn render_working_dots(
    draw_list: &mut UiDrawList,
    start_x: f32,
    y: f32,
    base_color: Rgba,
    anim_phase: f32,
) {
    let phase = anim_phase.rem_euclid(1.0);
    for i in 0..3 {
        let wave = ((phase - i as f32 * 0.22).rem_euclid(1.0) * std::f32::consts::TAU).cos();
        let alpha = 0.35 + 0.65 * (0.5 + 0.5 * wave);
        draw_list.push_rect(
            RectF::new(start_x + i as f32 * 6.0, y, 3.5, 3.5),
            base_color.with_alpha_f32(alpha),
            None,
            0.0,
            1.75,
            None,
        );
    }
}

#[must_use]
pub fn priority_dot_color(dot: PriorityDot, unread: bool, tokens: &UiTokens) -> Option<Rgba> {
    match dot {
        PriorityDot::Blocked => Some(tokens.status_blocked),
        PriorityDot::Error => Some(tokens.status_error),
        PriorityDot::DoneUnseen => Some(tokens.accent_primary),
        PriorityDot::None if unread => Some(tokens.accent_primary),
        PriorityDot::None => None,
    }
}

fn blend_rgba(base: Rgba, tint: Rgba, factor: f32) -> Rgba {
    let t = factor.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    Rgba::new(
        (f32::from(base.r) * inv + f32::from(tint.r) * t).round() as u8,
        (f32::from(base.g) * inv + f32::from(tint.g) * t).round() as u8,
        (f32::from(base.b) * inv + f32::from(tint.b) * t).round() as u8,
        base.a,
    )
}

fn tab_matches_query(tab: &SidebarTabView, lower_query: &str) -> bool {
    lower_query.is_empty()
        || tab.card.title.to_ascii_lowercase().contains(lower_query)
        || tab.card.subtitle.to_ascii_lowercase().contains(lower_query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tr3x_bridge::t3_session::build_tab_card;

    #[test]
    fn new_gap02_sidebar_scrollbar_clamps_safely_on_short_viewport() {
        let tabs: Vec<SidebarTabView> = (0..10)
            .map(|i| SidebarTabView {
                window_id: format!("win:{i}"),
                card: build_tab_card(&format!("Tab {i}"), Some("omarex"), None, None, None),
                active: i == 0,
                muted: false,
                unread: false,
                pane_count: 1,
                icon_override: None,
            })
            .collect();
        let hosts = vec![SidebarHostView {
            host_id: "local".into(),
            label: "This Machine".into(),
            is_local: true,
            connection: HostConnectionState::Online,
            collapsed: false,
            sessions: vec![SidebarSessionView {
                session_id: "sess:1".into(),
                label: "Main".into(),
                active: true,
                collapsed: false,
                muted: false,
                tabs,
            }],
        }];
        let state = SidebarState::default();
        let mut dl = UiDrawList::new();
        let metrics = render_sidebar(
            &mut dl,
            RectF::new(0.0, 0.0, 240.0, 100.0),
            None,
            false,
            false,
            &hosts,
            &state,
            &FocusTarget::Terminal(None),
            UiInteraction::default(),
            &Theme::default(),
            0.0,
            None,
        );
        assert!(metrics.max_scroll > 0.5);
        dl.validate_invariants()
            .expect("short sidebar draw list must be valid");
    }
}
