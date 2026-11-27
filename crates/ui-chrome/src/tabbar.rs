//! Horizontal top/bottom Rex tab bar (`38px`) with session switcher pill, scrollable tab pills,
//! provider icons, status dots, close buttons, active accent indicator, and right utility cluster.

use omarchy_theme::{Rgb, Theme};
use tr3x_bridge::t3_session::WorkingIndicator;

use crate::draw::{
    FontKind, FontWeight, HitTarget, TabReorderOverlay, TextAlign, UiDrawList, UiInteraction,
    cap_radius, z_layer,
};
use crate::icons::IconId;
use crate::layout::RectF;
use crate::sidebar::{SidebarTabView, TabIconColorPolicy, priority_dot_color, render_working_dots};

/// Input view model for the horizontal tab strip (`ToolbarPosition::Top` or `Bottom`).
#[derive(Debug, Clone)]
pub struct TabBarView<'a> {
    pub host_id: &'a str,
    pub session_id: &'a str,
    pub session_label: &'a str,
    pub other_session_needs_attention: bool,
    pub any_activity_attention: bool,
    pub activity_mode: bool,
    pub fill_available_space: bool,
    pub icon_color_policy: TabIconColorPolicy,
    pub hovered_tab_window_id: Option<&'a str>,
    pub tabs: &'a [SidebarTabView],
    pub scroll_offset_x: f32,
    pub is_bottom: bool,
    pub is_overlay: bool,
    pub tab_reorder_overlay: Option<&'a TabReorderOverlay>,
}

/// Renders the horizontal top or bottom tab bar into `draw_list`.
pub fn render_tab_bar(
    draw_list: &mut UiDrawList,
    bar_rect: RectF,
    view: &TabBarView<'_>,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    anim_phase: f32,
) {
    if bar_rect.is_empty() {
        return;
    }

    let tokens = theme.effective_ui();
    let z = if view.is_overlay {
        z_layer::OVERLAY_TOOLBAR
    } else {
        z_layer::SIDEBAR
    };

    // Backdrop & edge separator hairline
    let shadow = view.is_overlay.then_some(crate::draw::ShadowSpec {
        offset_x: 0.0,
        offset_y: if view.is_bottom { -4.0 } else { 4.0 },
        blur_radius: 18.0,
        spread: 0.0,
        color: tokens.shadow_color,
    });
    draw_list.push_rect(bar_rect, tokens.surface_frame, None, 0.0, 0.0, shadow);

    let border_y = if view.is_bottom {
        bar_rect.y
    } else {
        (bar_rect.bottom() - 1.0).max(bar_rect.y)
    };
    draw_list.push_rect(
        RectF::new(bar_rect.x, border_y, bar_rect.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let pill_r = cap_radius(7.0, theme.rounding);
    let btn_r = cap_radius(6.0, theme.rounding);

    // 1. Left: Session Switcher Pill (`[◫ T3 Code ▾]`), adapting width on narrow windows
    let max_pill_w = (bar_rect.w * 0.28).clamp(84.0, 168.0);
    let session_pill_w =
        (view.session_label.chars().count() as f32 * 7.2 + 42.0).clamp(84.0, max_pill_w);
    let session_rect = RectF::new(bar_rect.x + 8.0, bar_rect.y + 6.0, session_pill_w, 26.0);
    let switcher_fill = if interaction.is_pressed(&HitTarget::SessionSwitcherPill) {
        tokens.surface_row_selected
    } else if interaction.is_hovered(&HitTarget::SessionSwitcherPill) {
        tokens.surface_row_active
    } else {
        tokens.surface_row_hover
    };
    let switcher_border = if interaction.is_hovered(&HitTarget::SessionSwitcherPill) {
        tokens.border_subtle
    } else {
        tokens.border_hairline
    };
    draw_list.push_rect(
        session_rect,
        switcher_fill,
        Some(switcher_border),
        1.0,
        btn_r,
        None,
    );
    draw_list.push_icon(
        RectF::new(session_rect.x + 7.0, session_rect.y + 6.0, 14.0, 14.0),
        IconId::Sidebar,
        tokens.fg_secondary,
    );
    draw_list.push_text(
        RectF::new(
            session_rect.x + 25.0,
            session_rect.y + 5.0,
            (session_rect.w - 42.0).max(0.0),
            16.0,
        ),
        view.session_label,
        tokens.fg_primary,
        FontKind::Sans,
        12.0,
        FontWeight::Medium,
        TextAlign::Left,
        true,
    );
    draw_list.push_icon(
        RectF::new(
            session_rect.right() - 16.0,
            session_rect.y + 7.0,
            12.0,
            12.0,
        ),
        IconId::ChevronDown,
        tokens.fg_muted,
    );
    if view.other_session_needs_attention {
        draw_list.push_rect(
            RectF::new(session_rect.right() - 5.0, session_rect.y - 1.0, 6.0, 6.0),
            tokens.status_blocked,
            Some(tokens.surface_frame),
            1.0,
            3.0,
            None,
        );
    }
    draw_list.push_hit(session_rect, HitTarget::SessionSwitcherPill, z + 1);

    // 2. Right utility cluster: [Eye] [Palette] [+] (New Terminal)
    let btn_y = bar_rect.y + 5.0;
    let new_tab_rect = RectF::new((bar_rect.right() - 34.0).max(bar_rect.x), btn_y, 26.0, 28.0);
    let palette_rect = RectF::new((new_tab_rect.x - 30.0).max(bar_rect.x), btn_y, 26.0, 28.0);
    let eye_rect = RectF::new((palette_rect.x - 30.0).max(bar_rect.x), btn_y, 26.0, 28.0);

    if let Some((fill, border)) = interaction.button_surface(&HitTarget::TabNew, false, &tokens) {
        draw_list.push_rect(new_tab_rect, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(
        new_tab_rect.inset_uniform(6.0),
        IconId::Plus,
        if interaction.is_hovered(&HitTarget::TabNew) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    draw_list.push_hit(new_tab_rect, HitTarget::TabNew, z + 1);

    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::CommandPaletteButton, false, &tokens)
    {
        draw_list.push_rect(palette_rect, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(
        palette_rect.inset_uniform(6.0),
        IconId::Search,
        if interaction.is_hovered(&HitTarget::CommandPaletteButton) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    draw_list.push_hit(palette_rect, HitTarget::CommandPaletteButton, z + 1);

    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::ActivityToggle, view.activity_mode, &tokens)
    {
        draw_list.push_rect(eye_rect, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(
        eye_rect.inset_uniform(6.0),
        IconId::Activity,
        if view.activity_mode {
            tokens.accent_primary
        } else if interaction.is_hovered(&HitTarget::ActivityToggle) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    if view.any_activity_attention {
        draw_list.push_rect(
            RectF::new(eye_rect.right() - 7.0, eye_rect.y + 3.0, 6.0, 6.0),
            tokens.accent_primary,
            Some(tokens.surface_frame),
            1.0,
            3.0,
            None,
        );
    }
    draw_list.push_hit(eye_rect, HitTarget::ActivityToggle, z + 1);

    // 3. Center clipped horizontal tab strip
    let strip_x = session_rect.right() + 10.0;
    let strip_w = (eye_rect.x - strip_x - 10.0).max(0.0);
    let strip_rect = RectF::new(strip_x, bar_rect.y, strip_w, bar_rect.h);
    if strip_rect.is_empty() || view.tabs.is_empty() {
        return;
    }

    draw_list.push_clip(strip_rect);
    let count = view.tabs.len();
    let gap = 4.0;
    let tab_w = if view.fill_available_space && count >= 2 {
        ((strip_w - gap * (count - 1) as f32) / count as f32).clamp(110.0, 240.0)
    } else {
        172.0_f32.min((strip_w / count.max(1) as f32).max(110.0))
    };
    let total_tabs_w = if count == 0 {
        0.0
    } else {
        count as f32 * tab_w + (count - 1) as f32 * gap
    };
    let max_scroll_x = (total_tabs_w - strip_w).max(0.0);
    let scroll_x = view.scroll_offset_x.clamp(0.0, max_scroll_x);

    let mut x = strip_rect.x - scroll_x;
    for (idx, tab) in view.tabs.iter().enumerate() {
        if x > strip_rect.right() {
            break;
        }
        if x + tab_w < strip_rect.x {
            x += tab_w + gap;
            continue;
        }
        let tab_rect = RectF::new(x, bar_rect.y + 5.0, tab_w, 28.0);
        let tab_target = HitTarget::TabCard {
            host_id: view.host_id.to_owned(),
            session_id: view.session_id.to_owned(),
            window_id: tab.window_id.clone(),
        };
        let close_target = HitTarget::TabClose {
            host_id: view.host_id.to_owned(),
            session_id: view.session_id.to_owned(),
            window_id: tab.window_id.clone(),
        };
        let hovered = view.hovered_tab_window_id == Some(tab.window_id.as_str())
            || interaction.is_hovered(&tab_target)
            || interaction.is_hovered(&close_target);

        if tab.active && (count >= 2 || !view.fill_available_space) {
            draw_list.push_rect(
                tab_rect,
                tokens.surface_tab_active,
                Some(tokens.border_subtle),
                1.0,
                pill_r,
                None,
            );
            // Crisp active accent bar indicator along bottom/top edge of active pill
            let accent_y = if view.is_bottom {
                tab_rect.y
            } else {
                tab_rect.bottom() - 2.0
            };
            draw_list.push_rect(
                RectF::new(
                    tab_rect.x + 10.0,
                    accent_y,
                    (tab_rect.w - 20.0).max(0.0),
                    2.0,
                ),
                tokens.accent_primary,
                None,
                0.0,
                1.0,
                None,
            );
        } else if interaction.is_pressed(&tab_target) {
            draw_list.push_rect(
                tab_rect,
                tokens.surface_row_selected,
                Some(tokens.border_hairline),
                1.0,
                pill_r,
                None,
            );
        } else if hovered {
            draw_list.push_rect(
                tab_rect,
                tokens.surface_row_hover,
                Some(tokens.border_hairline),
                1.0,
                pill_r,
                None,
            );
        } else if idx > 0 && !view.tabs[idx - 1].active {
            // Subtle vertical hairline separator between adjacent inactive tabs
            draw_list.push_rect(
                RectF::new(x - 2.0, bar_rect.y + 12.0, 1.0, 14.0),
                tokens.border_hairline,
                None,
                0.0,
                0.0,
                None,
            );
        }

        // Provider icon (`16x16` tile) + corner status dot
        let icon = tab.resolved_icon();
        let icon_tile = RectF::new(tab_rect.x + 8.0, tab_rect.y + 6.0, 16.0, 16.0);
        let use_brand = match view.icon_color_policy {
            TabIconColorPolicy::Always => true,
            TabIconColorPolicy::ActiveTabs => tab.active,
            TabIconColorPolicy::Never => false,
        };
        let (tile_bg, icon_tint) = if use_brand && let Some([r, g, b]) = icon.brand_rgb() {
            let bg = Rgb::new(r, g, b);
            (bg.opaque(), bg.readable_foreground().opaque())
        } else {
            (
                tokens.surface_icon_inactive,
                if tab.active || hovered {
                    tokens.fg_primary
                } else {
                    tokens.fg_secondary
                },
            )
        };
        draw_list.push_rect(
            icon_tile,
            tile_bg,
            None,
            0.0,
            cap_radius(4.0, theme.rounding),
            None,
        );
        draw_list.push_icon(icon_tile.inset_uniform(2.5), icon, icon_tint);

        if let Some(dot_color) = priority_dot_color(tab.card.priority_dot, tab.unread, &tokens) {
            draw_list.push_rect(
                RectF::new(icon_tile.right() - 4.0, icon_tile.y - 2.0, 6.0, 6.0),
                dot_color,
                Some(tokens.surface_frame),
                1.0,
                3.0,
                None,
            );
        }

        // Title text
        let mute_reserve = if tab.muted { 16.0 } else { 0.0 };
        let text_x = icon_tile.right() + 7.0;
        let text_w = (tab_rect.right() - text_x - 26.0 - mute_reserve).max(0.0);
        draw_list.push_text(
            RectF::new(text_x, tab_rect.y + 6.0, text_w, 16.0),
            &tab.card.title,
            if tab.active || tab.unread || hovered {
                tokens.fg_primary
            } else {
                tokens.fg_secondary
            },
            FontKind::Sans,
            12.0,
            if tab.active {
                FontWeight::Medium
            } else {
                FontWeight::Regular
            },
            TextAlign::Left,
            true,
        );

        if tab.muted {
            draw_list.push_icon(
                RectF::new(tab_rect.right() - 38.0, tab_rect.y + 8.5, 11.0, 11.0),
                IconId::BellOff,
                tokens.fg_muted,
            );
        }

        // Trailing close button (on hover or active, with danger hover pill) or `•••` working pulse
        let close_rect = RectF::new(tab_rect.right() - 22.0, tab_rect.y + 5.0, 18.0, 18.0);
        if hovered || tab.active {
            let (close_bg, close_tint) =
                interaction.danger_button_style(&close_target, tokens.fg_muted, &tokens);
            if let Some((fill, border)) = close_bg {
                draw_list.push_rect(
                    close_rect,
                    fill,
                    border,
                    1.0,
                    cap_radius(4.0, theme.rounding),
                    None,
                );
            }
            draw_list.push_icon(close_rect.inset_uniform(3.5), IconId::Close, close_tint);
            draw_list.push_hit(close_rect.intersect(strip_rect), close_target, z + 2);
        } else if matches!(tab.card.working_indicator, WorkingIndicator::PulseDots) {
            render_working_dots(
                draw_list,
                tab_rect.right() - 22.0,
                tab_rect.y + 12.0,
                tokens.status_working,
                anim_phase,
            );
        }

        draw_list.push_hit(tab_rect.intersect(strip_rect), tab_target, z + 1);

        if let Some(overlay) = view.tab_reorder_overlay
            && overlay.host_id == view.host_id
            && overlay.session_id == view.session_id
            && overlay.dragged_window_id == tab.window_id
        {
            draw_list.push_rect(
                tab_rect,
                tokens.accent_primary.with_alpha_f32(0.10),
                Some(tokens.accent_primary.with_alpha_f32(0.70)),
                1.25,
                pill_r,
                None,
            );
        }

        x += tab_w + gap;
    }

    if let Some(overlay) = view.tab_reorder_overlay
        && overlay.host_id == view.host_id
        && overlay.session_id == view.session_id
        && let Some(ind) = overlay.indicator_rect
    {
        draw_list.push_rect(ind, tokens.accent_primary, None, 0.0, 1.25, None);
        let cx = ind.x + ind.w * 0.5;
        draw_list.push_rect(
            RectF::new(cx - 2.5, ind.y - 2.0, 5.0, 5.0),
            tokens.accent_primary,
            None,
            0.0,
            2.5,
            None,
        );
        draw_list.push_rect(
            RectF::new(cx - 2.5, ind.bottom() - 3.0, 5.0, 5.0),
            tokens.accent_primary,
            None,
            0.0,
            2.5,
            None,
        );
    }

    // Subtle overflow edge fades when horizontal tab strip scrolls beyond visible width.
    if scroll_x > 1.0 {
        draw_list.push_rect(
            RectF::new(strip_rect.x, strip_rect.y + 4.0, 12.0, strip_rect.h - 8.0),
            tokens.surface_frame.with_alpha_f32(0.75),
            None,
            0.0,
            0.0,
            None,
        );
    }
    if max_scroll_x - scroll_x > 1.0 {
        draw_list.push_rect(
            RectF::new(
                (strip_rect.right() - 12.0).max(strip_rect.x),
                strip_rect.y + 4.0,
                12.0,
                strip_rect.h - 8.0,
            ),
            tokens.surface_frame.with_alpha_f32(0.75),
            None,
            0.0,
            0.0,
            None,
        );
    }

    draw_list.pop_clip();
}
