//! Pane chrome renderer: inset rounded cards, active accent borders, inactive dimming,
//! `30px` pane headers with OSC 7501 chips & 5 action buttons, OSC 9;4 hairline progress bars,
//! unfocused bell badge, `ZOOMED` pill, overlay scrollbars, floating panes & minimized capsules,
//! pane DnD drop-zone overlay, and Rex empty state.

use omarchy_theme::{Rgba, Theme, UiTokens};
use tr3x_bridge::t3_session::{BlockedKind, ProgramState, RolledUpStatus};

use crate::draw::{
    FontKind, FontWeight, HitTarget, PaneHeaderButton, ShadowSpec, TextAlign, UiDrawList,
    UiInteraction, cap_radius, z_layer,
};
use crate::drawer::render_pr_drawer_chrome;
use crate::icons::IconId;
use crate::layout::{
    DividerGeometry, DropZoneHit, FloatingHitZone, FloatingPaneGeometry, MinimizedCapsuleGeometry,
    PaneGeometry, RectF, SplitAxis,
};

/// ConEmu / Ghostty OSC 9;4 terminal progress bar state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Osc94State {
    Normal,
    Error,
    Indeterminate,
    Paused,
}

/// Optional OSC 9;4 progress bar rendered as a `2px` hairline below the pane header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Osc94Progress {
    pub state: Osc94State,
    /// Percentage `0..=100`.
    pub percent: u8,
}

/// Mirror of `GHOSTTY_TERMINAL_DATA_SCROLLBAR` metrics for rendering the thin overlay scrollbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollbarMetrics {
    pub total_rows: u32,
    pub viewport_rows: u32,
    /// Distance scrolled up or top visible row offset (`0` = top of scrollback).
    pub offset_rows: u32,
    pub visible: bool,
    pub hovered: bool,
}

/// Listening TCP port badge displayed as a clickable `● :<port>` pill in pane headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortBadge {
    pub port: u16,
    pub label: Option<String>,
    pub tunneled: bool,
}

/// Per-pane chrome metadata supplied for rendering tiled or floating panes.
#[derive(Debug, Clone)]
pub struct PaneMeta {
    pub block_id: String,
    pub label: String,
    pub cwd: Option<String>,
    pub icon: IconId,
    pub focused: bool,
    pub hovered: bool,
    pub bell_unread: bool,
    pub program_status: Option<RolledUpStatus>,
    pub osc94_progress: Option<Osc94Progress>,
    pub scrollbar: ScrollbarMetrics,
    /// Optional explicit background from TUI `fill_with_program_colors`.
    pub program_bg: Option<Rgba>,
    /// True when new PTY output arrived while this pane was scrolled up into scrollback.
    pub has_unread_output: bool,
    /// Optional Vim Copy Mode badge (`"COPY"`, `"VISUAL"`, `"V-LINE"`, `"V-BLOCK"`).
    pub copy_mode_badge: Option<&'static str>,
    /// Live listening TCP ports detected for this pane (`● :<port>` pills).
    pub listening_ports: Vec<PortBadge>,
    /// True if this floating pane is a `tr3x` Pull Request drawer (`label == "Pull Request"`).
    pub is_pr_drawer: bool,
    /// Optional PR branch/number subtitle for `tr3x` `/pr` drawer chrome.
    pub pr_subtitle: Option<String>,
}

/// Visual configuration flags for pane chrome rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneChromeOptions {
    pub highlight_active_border: bool,
    pub active_border_opacity: f32,
    pub fade_inactive_panes: bool,
    pub hovered_divider_index: Option<usize>,
    pub is_t3_session: bool,
    pub program_color_header: bool,
}

impl Default for PaneChromeOptions {
    fn default() -> Self {
        Self {
            highlight_active_border: true,
            active_border_opacity: 0.85,
            fade_inactive_panes: true,
            hovered_divider_index: None,
            is_t3_session: false,
            program_color_header: true,
        }
    }
}

/// Renders tiled panes, split dividers, floating layers, minimized capsules, DnD overlay,
/// or the Rex empty state when no panes exist.
#[allow(clippy::too_many_arguments)]
pub fn render_workspace_chrome(
    draw_list: &mut UiDrawList,
    workspace: RectF,
    tiled: &[PaneGeometry<String>],
    dividers: &[DividerGeometry],
    floating: &[FloatingPaneGeometry<String>],
    capsules: &[MinimizedCapsuleGeometry<String>],
    metas: &[PaneMeta],
    options: PaneChromeOptions,
    dnd_overlay: Option<DropZoneHit>,
    show_sidebar_expand: bool,
    spring_active_border: Option<RectF>,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    anim_phase: f32,
) {
    let tokens = theme.effective_ui();

    if tiled.is_empty() && floating.is_empty() {
        render_empty_state(
            draw_list,
            workspace,
            options.is_t3_session,
            interaction,
            &tokens,
            theme.rounding,
        );
        if show_sidebar_expand {
            render_floating_sidebar_expand(
                draw_list,
                workspace,
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        return;
    }

    let multi_pane = tiled.len() >= 2;

    // Pick the top-leftmost tiled pane with a visible header to host the inline `[◧]` expand button
    // when the sidebar is manually or responsively collapsed.
    let inline_expand_pane_idx = if show_sidebar_expand {
        tiled
            .iter()
            .enumerate()
            .filter(|(_, g)| g.header_rect.is_some())
            .min_by(|(_, a), (_, b)| {
                a.card_rect
                    .x
                    .total_cmp(&b.card_rect.x)
                    .then_with(|| a.card_rect.y.total_cmp(&b.card_rect.y))
            })
            .map(|(idx, _)| idx)
    } else {
        None
    };

    // 1. Tiled panes
    for (idx, geo) in tiled.iter().enumerate() {
        let meta = metas.iter().find(|m| m.block_id == geo.id);
        render_tiled_pane(
            draw_list,
            geo,
            meta,
            multi_pane,
            inline_expand_pane_idx == Some(idx),
            options,
            spring_active_border.is_some(),
            interaction,
            &tokens,
            theme.rounding,
            anim_phase,
        );
    }

    // 1b. Spring-physics active-pane border (drawn over tiled panes, below floating panes & modals)
    if options.highlight_active_border
        && let Some(active_rect) = spring_active_border
        && !active_rect.is_empty()
    {
        let focused_geo = tiled
            .iter()
            .find(|g| metas.iter().any(|m| m.block_id == g.id && m.focused))
            .or_else(|| tiled.first());
        let active_radius =
            focused_geo.map_or(0.0, |g| cap_radius(g.corner_radius, theme.rounding));
        draw_list.push_rect(
            active_rect,
            Rgba::new(0, 0, 0, 0),
            Some(
                tokens
                    .border_active
                    .with_alpha_f32(options.active_border_opacity),
            ),
            1.5,
            active_radius,
            None,
        );
    }

    if show_sidebar_expand && inline_expand_pane_idx.is_none() {
        render_floating_sidebar_expand(draw_list, workspace, interaction, &tokens, theme.rounding);
    }

    // 2. Split dividers (hit targets + subtle hovered/dragged accent pill)
    for (idx, div) in dividers.iter().enumerate() {
        let div_target = HitTarget::SplitDivider {
            split_path: div.split_path.clone(),
        };
        let div_active = options.hovered_divider_index == Some(idx)
            || interaction.is_hovered(&div_target)
            || interaction.is_pressed(&div_target);
        draw_list.push_hit(div.hit_rect, div_target, z_layer::PANE_CHROME + 2);
        if div_active {
            let alpha = if interaction.is_pressed(&HitTarget::SplitDivider {
                split_path: div.split_path.clone(),
            }) {
                0.95
            } else {
                0.70
            };
            let pill = match div.axis {
                SplitAxis::Horizontal => RectF::new(
                    div.visual_rect.x + (div.visual_rect.w - 2.0) * 0.5,
                    div.visual_rect.y + 12.0,
                    2.0,
                    (div.visual_rect.h - 24.0).max(0.0),
                ),
                SplitAxis::Vertical => RectF::new(
                    div.visual_rect.x + 12.0,
                    div.visual_rect.y + (div.visual_rect.h - 2.0) * 0.5,
                    (div.visual_rect.w - 24.0).max(0.0),
                    2.0,
                ),
            };
            draw_list.push_rect(
                pill,
                tokens.accent_primary.with_alpha_f32(alpha),
                None,
                0.0,
                1.0,
                None,
            );
        }
    }

    // 3. Floating panes (with backdrop dim, Gaussian drop shadow, 8-way resize hit targets, and `/pr` styling)
    if !floating.is_empty() {
        draw_list.push_rect(
            workspace,
            tokens.surface_scrim.with_alpha_f32(0.22),
            None,
            0.0,
            0.0,
            None,
        );
    }
    for (layer_idx, fgeo) in floating.iter().enumerate() {
        let meta = metas.iter().find(|m| m.block_id == fgeo.id);
        let layer_z = z_layer::FLOATING_PANE + (layer_idx as u16).min(4);
        render_floating_pane(
            draw_list,
            fgeo,
            meta,
            options.program_color_header,
            interaction,
            &tokens,
            theme.rounding,
            anim_phase,
            layer_z,
        );
    }

    // 4. Corner-anchored minimized floating pane capsules
    for cap in capsules {
        let meta = metas.iter().find(|m| m.block_id == cap.id);
        render_minimized_capsule(draw_list, cap, meta, interaction, &tokens, theme.rounding);
    }

    // 5. Pane drag-and-drop drop-zone preview overlay
    if let Some(drop_hit) = dnd_overlay {
        draw_list.push_rect(
            drop_hit.preview_rect.inset_uniform(3.0),
            tokens.accent_primary.with_alpha_f32(0.18),
            Some(tokens.accent_primary.with_alpha_f32(0.85)),
            2.0,
            cap_radius(8.0, theme.rounding),
            None,
        );
    }
}

fn render_floating_sidebar_expand(
    draw_list: &mut UiDrawList,
    workspace: RectF,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    if workspace.w < 40.0 || workspace.h < 40.0 {
        return;
    }
    let btn = RectF::new(workspace.x + 12.0, workspace.y + 12.0, 28.0, 28.0);
    let hot = interaction.is_hovered(&HitTarget::SidebarToggle)
        || interaction.is_pressed(&HitTarget::SidebarToggle);
    let fill = if interaction.is_pressed(&HitTarget::SidebarToggle) {
        tokens.surface_row_selected
    } else if hot {
        tokens.surface_row_active
    } else {
        tokens.surface_modal
    };
    let border = if hot {
        tokens.accent_primary.with_alpha_f32(0.55)
    } else {
        tokens.border_subtle
    };
    draw_list.push_rect(
        btn,
        fill,
        Some(border),
        1.0,
        cap_radius(6.0, theme_rounding),
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 4.0,
            blur_radius: 14.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );
    draw_list.push_icon(
        btn.inset_uniform(6.0),
        IconId::Sidebar,
        if hot {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    draw_list.push_hit(btn, HitTarget::SidebarToggle, z_layer::OVERLAY_TOOLBAR);
}

#[allow(clippy::too_many_arguments)]
fn render_tiled_pane(
    draw_list: &mut UiDrawList,
    geo: &PaneGeometry<String>,
    meta: Option<&PaneMeta>,
    multi_pane: bool,
    show_sidebar_expand: bool,
    options: PaneChromeOptions,
    use_spring_active_border: bool,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    anim_phase: f32,
) {
    let focused = meta.is_some_and(|m| m.focused);
    let fill = meta
        .and_then(|m| m.program_bg)
        .unwrap_or(tokens.surface_pane);
    let radius = cap_radius(geo.corner_radius, theme_rounding);

    let (border_color, border_w) =
        if focused && options.highlight_active_border && !use_spring_active_border {
            (
                Some(
                    tokens
                        .border_active
                        .with_alpha_f32(options.active_border_opacity),
                ),
                1.5,
            )
        } else if radius > 0.0 || multi_pane {
            (Some(tokens.border_pane), 1.0)
        } else {
            (None, 0.0)
        };

    draw_list.push_rect(geo.card_rect, fill, border_color, border_w, radius, None);

    // Optional pane header (`30px`)
    if let Some(header_rect) = geo.header_rect {
        render_pane_header(
            draw_list,
            header_rect,
            &geo.id,
            meta,
            false,
            geo.is_zoomed,
            show_sidebar_expand,
            options.program_color_header,
            radius,
            interaction,
            tokens,
            theme_rounding,
            anim_phase,
            z_layer::PANE_CHROME,
        );
    } else {
        let mut right_edge = geo.card_rect.right() - 10.0;
        if geo.is_zoomed {
            // Floating `ZOOMED` pill in top-right when pane headers are hidden
            let badge = RectF::new(
                (right_edge - 62.0).max(geo.card_rect.x + 8.0),
                geo.card_rect.y + 8.0,
                62.0,
                20.0,
            );
            let zoom_target = HitTarget::PaneZoomBadge {
                block_id: geo.id.clone(),
            };
            let badge_fill = if interaction.is_pressed(&zoom_target) {
                tokens.accent_primary.with_alpha_f32(0.80)
            } else if interaction.is_hovered(&zoom_target) {
                tokens.accent_primary.with_alpha_f32(0.92)
            } else {
                tokens.accent_primary
            };
            draw_list.push_rect(
                badge,
                badge_fill,
                None,
                0.0,
                cap_radius(5.0, theme_rounding),
                None,
            );
            draw_list.push_text(
                badge,
                "ZOOMED",
                tokens.accent_foreground,
                FontKind::Mono,
                9.5,
                FontWeight::Bold,
                TextAlign::Center,
                false,
            );
            draw_list.push_hit(badge, zoom_target, z_layer::PANE_CHROME + 2);
            right_edge = badge.x - 6.0;
        }
        if let Some(copy_badge) = meta.and_then(|m| m.copy_mode_badge) {
            let bw = (copy_badge.len() as f32 * 6.8 + 16.0).max(52.0);
            let badge = RectF::new(
                (right_edge - bw).max(geo.card_rect.x + 8.0),
                geo.card_rect.y + 8.0,
                bw,
                20.0,
            );
            draw_list.push_rect(
                badge,
                tokens.accent_primary,
                None,
                0.0,
                cap_radius(5.0, theme_rounding),
                None,
            );
            draw_list.push_text(
                badge,
                copy_badge,
                tokens.accent_foreground,
                FontKind::Mono,
                9.5,
                FontWeight::Bold,
                TextAlign::Center,
                false,
            );
        }
    }

    // OSC 9;4 hairline progress bar (`2px`) beneath pane header or along top of terminal
    if let Some(prog) = meta.and_then(|m| m.osc94_progress) {
        let bar_y = geo.header_rect.map_or(geo.card_rect.y, RectF::bottom);
        let bar_rect = RectF::new(
            geo.card_rect.x + 1.0,
            bar_y,
            (geo.card_rect.w - 2.0).max(0.0),
            2.0,
        );
        let (frac, color) = match prog.state {
            Osc94State::Normal => (
                f32::from(prog.percent.min(100)) / 100.0,
                tokens.status_working,
            ),
            Osc94State::Error => (
                f32::from(prog.percent.min(100)) / 100.0,
                tokens.status_error,
            ),
            Osc94State::Paused => (
                f32::from(prog.percent.min(100)) / 100.0,
                tokens.status_blocked,
            ),
            Osc94State::Indeterminate => (
                0.25 + 0.55 * (0.5 + 0.5 * (anim_phase * std::f32::consts::TAU).sin()),
                tokens.accent_primary,
            ),
        };
        draw_list.push_progress(bar_rect, frac, color, tokens.border_hairline, 0.0);
    }

    // Thin overlay scrollbar (`GHOSTTY_TERMINAL_DATA_SCROLLBAR`)
    if let Some(sb) = meta.map(|m| m.scrollbar) {
        render_overlay_scrollbar(
            draw_list,
            geo.terminal_rect,
            &geo.id,
            sb,
            interaction,
            tokens,
            z_layer::PANE_CHROME + 1,
        );
    }

    // Floating `↓ New output` pill when scrolled up while live PTY bytes arrive
    if meta.is_some_and(|m| m.has_unread_output) {
        render_new_output_pill(
            draw_list,
            geo.terminal_rect,
            &geo.id,
            interaction,
            tokens,
            theme_rounding,
            z_layer::PANE_CHROME + 3,
        );
    }

    // Inactive pane subtle dim overlay when multiple panes exist
    if multi_pane && !focused && options.fade_inactive_panes {
        draw_list.push_rect(
            geo.card_rect,
            tokens.surface_frame.with_alpha_f32(0.18),
            None,
            0.0,
            radius,
            None,
        );
    }

    draw_list.push_hit(
        geo.terminal_rect,
        HitTarget::PaneSurface {
            block_id: geo.id.clone(),
        },
        z_layer::PANE_CHROME,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_floating_pane(
    draw_list: &mut UiDrawList,
    geo: &FloatingPaneGeometry<String>,
    meta: Option<&PaneMeta>,
    program_color_header: bool,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    anim_phase: f32,
    z: u16,
) {
    let is_pr = meta.is_some_and(|m| m.is_pr_drawer);
    if is_pr {
        render_pr_drawer_chrome(
            draw_list,
            geo,
            meta.and_then(|m| m.pr_subtitle.as_deref()),
            interaction,
            tokens,
            theme_rounding,
            z,
        );
    } else {
        let radius = cap_radius(geo.corner_radius, theme_rounding);
        let fill = meta
            .and_then(|m| m.program_bg)
            .unwrap_or(tokens.surface_pane);
        let border = if meta.is_some_and(|m| m.focused) {
            tokens.border_active
        } else {
            tokens.border_modal
        };
        let shadow = ShadowSpec {
            offset_x: if geo.is_right_drawer { -8.0 } else { 0.0 },
            offset_y: if geo.is_right_drawer { 0.0 } else { 14.0 },
            blur_radius: 36.0,
            spread: 0.0,
            color: tokens.shadow_color,
        };
        draw_list.push_rect(geo.card_rect, fill, Some(border), 1.0, radius, Some(shadow));
        render_pane_header(
            draw_list,
            geo.header_rect,
            &geo.id,
            meta,
            true,
            false,
            false,
            program_color_header,
            radius,
            interaction,
            tokens,
            theme_rounding,
            anim_phase,
            z,
        );
    }

    if let Some(sb) = meta.map(|m| m.scrollbar) {
        render_overlay_scrollbar(
            draw_list,
            geo.terminal_rect,
            &geo.id,
            sb,
            interaction,
            tokens,
            z + 1,
        );
    }

    if meta.is_some_and(|m| m.has_unread_output) {
        render_new_output_pill(
            draw_list,
            geo.terminal_rect,
            &geo.id,
            interaction,
            tokens,
            theme_rounding,
            z + 3,
        );
    }

    // 8px resize edge handles + drag header + content hit targets
    const EDGE: f32 = 8.0;
    let r = geo.card_rect;
    draw_list.push_hit(
        RectF::new(r.x, r.y, EDGE, r.h),
        HitTarget::FloatingHandle {
            block_id: geo.id.clone(),
            zone: FloatingHitZone::ResizeW,
        },
        z + 2,
    );
    draw_list.push_hit(
        RectF::new((r.right() - EDGE).max(r.x), r.y, EDGE, r.h),
        HitTarget::FloatingHandle {
            block_id: geo.id.clone(),
            zone: FloatingHitZone::ResizeE,
        },
        z + 2,
    );
    draw_list.push_hit(
        RectF::new(r.x, (r.bottom() - EDGE).max(r.y), r.w, EDGE),
        HitTarget::FloatingHandle {
            block_id: geo.id.clone(),
            zone: FloatingHitZone::ResizeS,
        },
        z + 2,
    );
    draw_list.push_hit(
        geo.header_rect,
        HitTarget::FloatingHandle {
            block_id: geo.id.clone(),
            zone: FloatingHitZone::MoveHeader,
        },
        z + 1,
    );
    draw_list.push_hit(
        geo.terminal_rect,
        HitTarget::PaneSurface {
            block_id: geo.id.clone(),
        },
        z,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_pane_header(
    draw_list: &mut UiDrawList,
    header_rect: RectF,
    block_id: &str,
    meta: Option<&PaneMeta>,
    is_floating: bool,
    is_zoomed: bool,
    show_sidebar_expand: bool,
    program_color_header: bool,
    card_radius: f32,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    anim_phase: f32,
    z: u16,
) {
    let raw_program_bg = meta.and_then(|m| m.program_bg);
    let effective_program_bg = if program_color_header {
        raw_program_bg
    } else {
        None
    };

    // Only fill opaque header strip in sharp/0px mode unless `program_color_header == false`
    // overrides a rounded pane card's `program_bg`, in which case we keep the rounded top corners
    // clean while painting `tokens.surface_pane_header`.
    if theme_rounding <= 0.0 {
        let header_bg = effective_program_bg.unwrap_or(tokens.surface_pane_header);
        draw_list.push_rect(header_rect, header_bg, None, 0.0, 0.0, None);
    } else if !program_color_header && raw_program_bg.is_some() {
        let inner = RectF::new(
            header_rect.x + 1.0,
            header_rect.y + 1.0,
            (header_rect.w - 2.0).max(0.0),
            (header_rect.h - 1.0).max(0.0),
        );
        let r = card_radius.min(inner.h * 0.5).max(0.0);
        draw_list.push_rect(inner, tokens.surface_pane_header, None, 0.0, r, None);
        if r > 0.0 && inner.h > r {
            draw_list.push_rect(
                RectF::new(inner.x, inner.y + r, inner.w, inner.h - r),
                tokens.surface_pane_header,
                None,
                0.0,
                0.0,
                None,
            );
        }
    }

    // WCAG AA (>= 4.5) contrast guard when program_bg paints behind the pane header.
    let header_surface =
        effective_program_bg.unwrap_or(if theme_rounding <= 0.0 || !program_color_header {
            tokens.surface_pane_header
        } else {
            tokens.surface_pane
        });
    let bg_rgb = header_surface.rgb();
    let (hdr_fg_primary, hdr_fg_secondary, hdr_fg_muted, hdr_hairline) =
        if effective_program_bg.is_some() && bg_rgb.wcag_contrast(tokens.fg_primary.rgb()) < 4.5 {
            let prim = bg_rgb.readable_foreground();
            (
                prim.opaque(),
                prim.blend(bg_rgb, 0.25).opaque(),
                prim.blend(bg_rgb, 0.45).opaque(),
                bg_rgb.blend(prim, 0.14).opaque(),
            )
        } else {
            (
                tokens.fg_primary,
                tokens.fg_secondary,
                tokens.fg_muted,
                tokens.border_hairline,
            )
        };

    draw_list.push_rect(
        RectF::new(
            header_rect.x + 1.5,
            (header_rect.bottom() - 1.0).max(header_rect.y),
            (header_rect.w - 3.0).max(0.0),
            1.0,
        ),
        hdr_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let icon = meta.map_or(IconId::Terminal, |m| m.icon);
    let label = meta.map_or("terminal", |m| m.label.as_str());
    let focused = meta.is_some_and(|m| m.focused);
    let hovered = meta.is_some_and(|m| m.hovered)
        || matches!(
            interaction.hovered,
            Some(
                HitTarget::PaneHeader { block_id: b }
                    | HitTarget::PaneButton { block_id: b, .. }
                    | HitTarget::PaneZoomBadge { block_id: b }
            ) if b == block_id
        );

    // Right-side 5 Rex pane buttons (`22×22` each, `3px` gap = `130px` cluster) with hover/press pills
    const BTN_SIZE: f32 = 22.0;
    const BTN_GAP: f32 = 3.0;
    let btn_r = cap_radius(4.0, theme_rounding);
    let buttons = [
        (PaneHeaderButton::SplitRight, IconId::SplitRight),
        (PaneHeaderButton::SplitDown, IconId::SplitDown),
        (
            PaneHeaderButton::FloatOrDock,
            if is_floating {
                IconId::Dock
            } else {
                IconId::Float
            },
        ),
        (PaneHeaderButton::Zoom, IconId::Zoom),
        (PaneHeaderButton::Close, IconId::Close),
    ];
    let cluster_w = buttons.len() as f32 * BTN_SIZE + (buttons.len() - 1) as f32 * BTN_GAP + 10.0;
    let show_buttons = focused || hovered || is_floating;
    if show_buttons {
        let mut bx = (header_rect.right() - cluster_w + 2.0).max(header_rect.x);
        let by = header_rect.y + (header_rect.h - BTN_SIZE) * 0.5;
        for (btn, btn_icon) in buttons {
            let btn_rect = RectF::new(bx, by, BTN_SIZE, BTN_SIZE);
            let target = HitTarget::PaneButton {
                block_id: block_id.to_owned(),
                button: btn,
            };
            if btn == PaneHeaderButton::Close {
                let (bg, tint) = interaction.danger_button_style(&target, hdr_fg_muted, tokens);
                if let Some((fill, border)) = bg {
                    draw_list.push_rect(btn_rect, fill, border, 1.0, btn_r, None);
                }
                draw_list.push_icon(btn_rect.inset_uniform(4.5), btn_icon, tint);
            } else {
                let is_active_btn = btn == PaneHeaderButton::Zoom && is_zoomed;
                if let Some((fill, border)) =
                    interaction.button_surface(&target, is_active_btn, tokens)
                {
                    draw_list.push_rect(btn_rect, fill, border, 1.0, btn_r, None);
                }
                let tint = if is_active_btn {
                    tokens.accent_primary
                } else if interaction.is_hovered(&target) || interaction.is_pressed(&target) {
                    tokens.fg_primary
                } else {
                    hdr_fg_muted
                };
                draw_list.push_icon(btn_rect.inset_uniform(4.5), btn_icon, tint);
            }
            draw_list.push_hit(btn_rect, target, z + 2);
            bx += BTN_SIZE + BTN_GAP;
        }
    }

    // Left-side: optional `[◧]` sidebar expand toggle + icon + label + cwd + rounded status pill + bell + zoomed pill
    let mut cx = header_rect.x + 10.0;
    let max_x = (header_rect.right() - cluster_w - 8.0).max(cx);

    if show_sidebar_expand && cx + 26.0 < max_x {
        let sb_btn = RectF::new(cx, header_rect.y + (header_rect.h - 22.0) * 0.5, 22.0, 22.0);
        if let Some((fill, border)) =
            interaction.button_surface(&HitTarget::SidebarToggle, false, tokens)
        {
            draw_list.push_rect(sb_btn, fill, border, 1.0, btn_r, None);
        }
        draw_list.push_icon(
            sb_btn.inset_uniform(4.0),
            IconId::Sidebar,
            if interaction.is_hovered(&HitTarget::SidebarToggle) {
                tokens.fg_primary
            } else {
                hdr_fg_secondary
            },
        );
        draw_list.push_hit(sb_btn, HitTarget::SidebarToggle, z + 2);
        cx += 26.0;
    } else {
        cx += 2.0;
    }

    let icon_tint = if focused {
        icon.brand_rgb()
            .map(|[r, g, b]| Rgba::new(r, g, b, 255))
            .unwrap_or(tokens.accent_primary)
    } else {
        hdr_fg_secondary
    };
    draw_list.push_icon(
        RectF::new(cx, header_rect.y + (header_rect.h - 14.0) * 0.5, 14.0, 14.0),
        icon,
        icon_tint,
    );
    cx += 20.0;

    let max_label_w = ((max_x - cx) * 0.55).max(120.0).min((max_x - cx).max(0.0));
    let label_w = ((label.chars().count() as f32) * 7.0 + 8.0).min(max_label_w);
    draw_list.push_text(
        RectF::new(cx, header_rect.y + 6.0, label_w, 18.0),
        label,
        if focused {
            hdr_fg_primary
        } else {
            hdr_fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    cx += label_w + 8.0;

    if let Some(cwd) = meta.and_then(|m| m.cwd.as_deref())
        && cx + 28.0 < max_x
    {
        let max_cwd_w = ((max_x - cx) * 0.45).max(80.0).min((max_x - cx).max(0.0));
        let cwd_w = ((cwd.chars().count() as f32) * 6.8 + 6.0).min(max_cwd_w);
        draw_list.push_text(
            RectF::new(cx, header_rect.y + 6.0, cwd_w, 18.0),
            cwd,
            hdr_fg_muted,
            FontKind::Mono,
            11.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        cx += cwd_w + 10.0;
    }

    // Rounded pill status chip (`● Working`, `● Done`, `● Needs you`, etc., matching `gpu_smoke.png`)
    if let Some(status) = meta.and_then(|m| m.program_status.as_ref())
        && let Some((chip_text, chip_color)) = format_osc7501_pill(status, anim_phase, tokens)
        && cx + 48.0 < max_x
    {
        let text_w = (chip_text.chars().count() as f32) * 6.6 + 6.0;
        let pill_w = (24.0 + text_w).min((max_x - cx).max(0.0));
        let pill_h = 19.0;
        let pill_y = header_rect.y + (header_rect.h - pill_h) * 0.5;
        let pill_rect = RectF::new(cx, pill_y, pill_w, pill_h);
        let pill_r = if theme_rounding <= 0.0 {
            0.0
        } else {
            pill_h * 0.5
        };
        draw_list.push_rect(
            pill_rect,
            chip_color.with_alpha_f32(0.14),
            Some(chip_color.with_alpha_f32(0.48)),
            1.0,
            pill_r,
            None,
        );
        draw_list.push_rect(
            RectF::new(
                pill_rect.x + 8.0,
                pill_rect.y + (pill_h - 6.0) * 0.5,
                6.0,
                6.0,
            ),
            chip_color,
            None,
            0.0,
            3.0,
            None,
        );
        draw_list.push_text(
            RectF::new(
                pill_rect.x + 19.0,
                pill_rect.y + 2.0,
                (pill_rect.w - 25.0).max(0.0),
                15.0,
            ),
            chip_text,
            chip_color,
            FontKind::Sans,
            11.0,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );
        cx += pill_w + 8.0;
    }

    // Copy Mode badge (`COPY`, `VISUAL`, `V-LINE`, `V-BLOCK`)
    if let Some(copy_badge) = meta.and_then(|m| m.copy_mode_badge) {
        let bw = (copy_badge.len() as f32 * 6.8 + 16.0).max(50.0);
        if cx + bw <= max_x {
            let pill = RectF::new(cx, header_rect.y + 6.0, bw, 18.0);
            draw_list.push_rect(
                pill,
                tokens.accent_primary,
                None,
                0.0,
                cap_radius(4.0, theme_rounding),
                None,
            );
            draw_list.push_text(
                pill,
                copy_badge,
                tokens.accent_foreground,
                FontKind::Mono,
                9.5,
                FontWeight::Bold,
                TextAlign::Center,
                false,
            );
            cx += bw + 6.0;
        }
    }

    // Listening TCP port pills (`● :<port>`)
    if let Some(ports) = meta.map(|m| m.listening_ports.as_slice())
        && !ports.is_empty()
    {
        let pill_r = if theme_rounding <= 0.0 { 0.0 } else { 9.5 };
        let pill_y = header_rect.y + (header_rect.h - 19.0) * 0.5;
        let mut shown = 0usize;
        for p in ports.iter().take(2) {
            let label = if p.tunneled {
                format!(":{} ⇄", p.port)
            } else {
                format!(":{}", p.port)
            };
            let pill_w = 22.0 + (label.chars().count() as f32) * 6.4;
            if cx + pill_w > max_x {
                break;
            }
            let pill_rect = RectF::new(cx, pill_y, pill_w, 19.0);
            let port_target = HitTarget::PanePortPill {
                block_id: block_id.to_owned(),
                port: p.port,
            };
            let hovered = interaction.is_hovered(&port_target);
            let dot_color = if p.tunneled {
                tokens.accent_primary
            } else {
                tokens.status_done
            };
            draw_list.push_rect(
                pill_rect,
                if hovered {
                    dot_color.with_alpha_f32(0.24)
                } else {
                    dot_color.with_alpha_f32(0.14)
                },
                Some(dot_color.with_alpha_f32(if hovered { 0.85 } else { 0.48 })),
                1.0,
                pill_r,
                None,
            );
            draw_list.push_rect(
                RectF::new(pill_rect.x + 7.0, pill_rect.y + 6.5, 6.0, 6.0),
                dot_color,
                None,
                0.0,
                3.0,
                None,
            );
            draw_list.push_text(
                RectF::new(
                    pill_rect.x + 17.0,
                    pill_rect.y + 2.0,
                    (pill_rect.w - 21.0).max(0.0),
                    15.0,
                ),
                label,
                dot_color,
                FontKind::Mono,
                10.5,
                FontWeight::SemiBold,
                TextAlign::Left,
                false,
            );
            draw_list.push_hit(pill_rect, port_target, z + 2);
            cx += pill_w + 6.0;
            shown += 1;
        }
        let omitted = ports.len().saturating_sub(shown);
        if omitted > 0 {
            let extra = format!("+{omitted}");
            let ew = (extra.len() as f32 * 6.4 + 10.0).max(24.0);
            if cx + ew <= max_x {
                let extra_rect = RectF::new(cx, pill_y, ew, 19.0);
                draw_list.push_rect(
                    extra_rect,
                    tokens.surface_pane,
                    Some(tokens.border_hairline),
                    1.0,
                    pill_r,
                    None,
                );
                draw_list.push_text(
                    extra_rect,
                    extra,
                    tokens.fg_muted,
                    FontKind::Mono,
                    10.0,
                    FontWeight::Medium,
                    TextAlign::Center,
                    false,
                );
                cx += ew + 6.0;
            }
        }
    }

    // Unfocused bell badge
    if meta.is_some_and(|m| m.bell_unread && !m.focused) && cx + 16.0 < max_x {
        draw_list.push_icon(
            RectF::new(cx, header_rect.y + 8.0, 14.0, 14.0),
            IconId::Bell,
            tokens.status_blocked,
        );
        cx += 18.0;
    }

    // Zoomed pill badge
    if is_zoomed && cx + 56.0 <= max_x {
        let pill = RectF::new(cx, header_rect.y + 6.0, 54.0, 18.0);
        draw_list.push_rect(
            pill,
            tokens.accent_primary,
            None,
            0.0,
            cap_radius(4.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            pill,
            "ZOOMED",
            tokens.accent_foreground,
            FontKind::Mono,
            9.5,
            FontWeight::Bold,
            TextAlign::Center,
            false,
        );
        draw_list.push_hit(
            pill,
            HitTarget::PaneZoomBadge {
                block_id: block_id.to_owned(),
            },
            z + 2,
        );
    }

    draw_list.push_hit(
        header_rect,
        HitTarget::PaneHeader {
            block_id: block_id.to_owned(),
        },
        z + 1,
    );
}

fn clean_status_detail(msg: Option<&str>) -> Option<&str> {
    let raw = msg?.trim();
    if raw.is_empty() {
        return None;
    }
    let trimmed = raw
        .strip_prefix("No project · ")
        .or_else(|| raw.strip_prefix("No project ·"))
        .unwrap_or(raw)
        .trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn format_osc7501_pill(
    status: &RolledUpStatus,
    _anim_phase: f32,
    tokens: &UiTokens,
) -> Option<(String, Rgba)> {
    let detail = clean_status_detail(status.msg.as_deref());
    match status.state {
        ProgramState::Blocked => {
            let kind = match status.kind {
                Some(BlockedKind::Permission) => "Permission",
                Some(BlockedKind::Question) => "Question",
                Some(BlockedKind::Auth) => "Auth",
                _ => "Needs you",
            };
            let text = match detail {
                Some(m) if !m.eq_ignore_ascii_case(kind) => format!("Needs you · {m}"),
                _ => format!("Needs you · {kind}"),
            };
            Some((text, tokens.status_blocked))
        }
        ProgramState::Error => {
            let text = match detail {
                Some(m) => format!("Error · {m}"),
                None => "Error".to_owned(),
            };
            Some((text, tokens.status_error))
        }
        ProgramState::Working => {
            let text = match (status.progress, detail) {
                (Some(p), Some(m)) if !m.eq_ignore_ascii_case("working") => {
                    format!("Working {p}% · {m}")
                }
                (Some(p), _) => format!("Working {p}%"),
                (None, Some(m)) if !m.eq_ignore_ascii_case("working") => {
                    format!("Working · {m}")
                }
                (None, _) => "Working".to_owned(),
            };
            Some((text, tokens.status_working))
        }
        ProgramState::Waiting => Some(("Waiting".to_owned(), tokens.status_blocked)),
        ProgramState::Done => Some(("Done".to_owned(), tokens.status_done)),
        ProgramState::Idle => match detail {
            Some(m) if m.ends_with("Done") => Some(("Done".to_owned(), tokens.status_done)),
            _ => None,
        },
    }
}

fn render_overlay_scrollbar(
    draw_list: &mut UiDrawList,
    terminal_rect: RectF,
    block_id: &str,
    sb: ScrollbarMetrics,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    z: u16,
) {
    if !sb.visible
        || sb.total_rows <= sb.viewport_rows
        || sb.viewport_rows == 0
        || terminal_rect.h < 24.0
    {
        return;
    }
    let track_target = HitTarget::PaneScrollbarTrack {
        block_id: block_id.to_owned(),
    };
    let thumb_target = HitTarget::PaneScrollbarThumb {
        block_id: block_id.to_owned(),
    };
    let hovered = sb.hovered
        || interaction.is_hovered(&track_target)
        || interaction.is_hovered(&thumb_target)
        || interaction.is_pressed(&thumb_target);
    let track_w = if hovered { 8.0 } else { 6.0 };
    let track = RectF::new(
        (terminal_rect.right() - track_w - 3.0).max(terminal_rect.x),
        terminal_rect.y + 4.0,
        track_w,
        (terminal_rect.h - 8.0).max(0.0),
    );
    let ratio = (sb.viewport_rows as f32 / sb.total_rows as f32).clamp(0.05, 1.0);
    let thumb_h = (track.h * ratio).max(24.0).min(track.h);
    let max_offset = (sb.total_rows - sb.viewport_rows).max(1) as f32;
    let frac = (sb.offset_rows as f32 / max_offset).clamp(0.0, 1.0);
    let thumb_y = track.y + (track.h - thumb_h) * frac;
    let thumb = RectF::new(track.x, thumb_y, track.w, thumb_h);

    let thumb_color = if hovered {
        tokens.scrollbar_thumb_hover
    } else {
        tokens.scrollbar_thumb
    };
    draw_list.push_rect(thumb, thumb_color, None, 0.0, track_w * 0.5, None);
    draw_list.push_hit(track, track_target, z);
    draw_list.push_hit(thumb, thumb_target, z + 1);
}

fn render_new_output_pill(
    draw_list: &mut UiDrawList,
    terminal_rect: RectF,
    block_id: &str,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    z: u16,
) {
    if terminal_rect.w < 140.0 || terminal_rect.h < 60.0 {
        return;
    }
    let pill_w = 118.0f32.min((terminal_rect.w - 24.0).max(0.0));
    let pill_h = 26.0;
    let x = terminal_rect.x + (terminal_rect.w - pill_w) * 0.5;
    let y = (terminal_rect.bottom() - pill_h - 12.0).max(terminal_rect.y + 6.0);
    let pill = RectF::new(x, y, pill_w, pill_h);

    let target = HitTarget::PaneNewOutputPill {
        block_id: block_id.to_owned(),
    };
    let hovered = interaction.is_hovered(&target);
    let pressed = interaction.is_pressed(&target);

    let fill = if pressed {
        tokens.surface_row_selected
    } else if hovered {
        tokens.surface_row_active
    } else {
        tokens.surface_modal
    };
    let border = if hovered || pressed {
        tokens.accent_primary
    } else {
        tokens.accent_primary.with_alpha_f32(0.75)
    };
    let shadow = ShadowSpec {
        offset_x: 0.0,
        offset_y: 4.0,
        blur_radius: 14.0,
        spread: 0.0,
        color: tokens.shadow_color,
    };
    draw_list.push_rect(
        pill,
        fill,
        Some(border),
        1.0,
        cap_radius(pill_h * 0.5, theme_rounding),
        Some(shadow),
    );
    draw_list.push_text(
        pill,
        "↓ New output",
        if hovered || pressed {
            tokens.accent_primary
        } else {
            tokens.fg_primary
        },
        FontKind::Sans,
        11.5,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(pill, target, z);
}

fn render_minimized_capsule(
    draw_list: &mut UiDrawList,
    cap: &MinimizedCapsuleGeometry<String>,
    meta: Option<&PaneMeta>,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let cap_target = HitTarget::MinimizedCapsule {
        block_id: cap.id.clone(),
    };
    let close_target = HitTarget::MinimizedCapsuleClose {
        block_id: cap.id.clone(),
    };
    let cap_hot = interaction.is_hovered(&cap_target) || interaction.is_hovered(&close_target);
    let radius = if theme_rounding <= 0.0 { 0.0 } else { 14.0 };
    draw_list.push_rect(
        cap.rect,
        if interaction.is_pressed(&cap_target) {
            tokens.surface_row_selected
        } else if cap_hot {
            tokens.surface_row_active
        } else {
            tokens.surface_modal
        },
        Some(if cap_hot {
            tokens.border_active.with_alpha_f32(0.6)
        } else {
            tokens.border_subtle
        }),
        1.0,
        radius,
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 6.0,
            blur_radius: 16.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );

    let icon = meta.map_or(IconId::Float, |m| m.icon);
    let label = meta.map_or("Floating", |m| m.label.as_str());
    draw_list.push_icon(
        RectF::new(cap.rect.x + 8.0, cap.rect.y + 7.0, 14.0, 14.0),
        icon,
        if cap_hot {
            tokens.accent_primary
        } else {
            tokens.fg_secondary
        },
    );
    draw_list.push_text(
        RectF::new(
            cap.rect.x + 26.0,
            cap.rect.y + 6.0,
            (cap.rect.w - 50.0).max(0.0),
            16.0,
        ),
        label,
        tokens.fg_primary,
        FontKind::Sans,
        11.5,
        FontWeight::Medium,
        TextAlign::Left,
        true,
    );

    let close_rect = RectF::new(cap.rect.right() - 22.0, cap.rect.y + 5.0, 18.0, 18.0);
    let (close_bg, close_tint) =
        interaction.danger_button_style(&close_target, tokens.fg_muted, tokens);
    if let Some((fill, border)) = close_bg {
        draw_list.push_rect(
            close_rect,
            fill,
            border,
            1.0,
            cap_radius(9.0, theme_rounding),
            None,
        );
    }
    draw_list.push_icon(close_rect.inset_uniform(3.5), IconId::Close, close_tint);
    draw_list.push_hit(close_rect, close_target, z_layer::CAPSULE + 1);
    draw_list.push_hit(cap.rect, cap_target, z_layer::CAPSULE);
}

fn render_empty_state(
    draw_list: &mut UiDrawList,
    workspace: RectF,
    is_t3_session: bool,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    if workspace.is_empty() {
        return;
    }
    let card_w = 368.0_f32.min((workspace.w - 32.0).max(0.0));
    let card_h = 186.0_f32.min((workspace.h - 32.0).max(0.0));
    let card = RectF::new(
        workspace.x + (workspace.w - card_w) * 0.5,
        workspace.y + (workspace.h - card_h) * 0.5,
        card_w,
        card_h,
    );
    draw_list.push_rect(
        card,
        tokens.surface_pane,
        Some(tokens.border_subtle),
        1.0,
        cap_radius(10.0, theme_rounding),
        None,
    );

    let icon = if is_t3_session {
        IconId::T3
    } else {
        IconId::Terminal
    };
    draw_list.push_icon(
        RectF::new(card.x + (card.w - 28.0) * 0.5, card.y + 22.0, 28.0, 28.0),
        icon,
        tokens.accent_primary,
    );

    let (heading, subtitle) = if is_t3_session {
        (
            "No open T3 threads",
            "Start a T3 thread or open a fresh Ghostty terminal",
        )
    } else {
        (
            "No open terminals",
            "Press Ctrl+Shift+T for a terminal or Ctrl+Shift+N for a session",
        )
    };

    draw_list.push_text(
        RectF::new(card.x + 16.0, card.y + 58.0, (card.w - 32.0).max(0.0), 20.0),
        heading,
        tokens.fg_primary,
        FontKind::Sans,
        14.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        true,
    );
    draw_list.push_text(
        RectF::new(card.x + 16.0, card.y + 82.0, (card.w - 32.0).max(0.0), 18.0),
        subtitle,
        tokens.fg_muted,
        FontKind::Sans,
        11.5,
        FontWeight::Regular,
        TextAlign::Center,
        true,
    );

    let btn_r = cap_radius(7.0, theme_rounding);
    if is_t3_session && card.w >= 280.0 {
        // Two side-by-side CTAs: primary `New T3 Thread` + secondary `New Terminal`
        let btn_w = 132.0;
        let gap = 10.0;
        let start_x = card.x + (card.w - (btn_w * 2.0 + gap)) * 0.5;
        let t3_btn = RectF::new(start_x, card.y + 118.0, btn_w, 32.0);
        let term_btn = RectF::new(start_x + btn_w + gap, card.y + 118.0, btn_w, 32.0);

        let t3_fill = if interaction.is_pressed(&HitTarget::EmptyStateNewTab) {
            tokens.accent_primary.with_alpha_f32(0.80)
        } else if interaction.is_hovered(&HitTarget::EmptyStateNewTab) {
            tokens.accent_primary.with_alpha_f32(0.92)
        } else {
            tokens.accent_primary
        };
        draw_list.push_rect(t3_btn, t3_fill, None, 0.0, btn_r, None);
        draw_list.push_text(
            t3_btn,
            "New T3 Thread",
            tokens.accent_foreground,
            FontKind::Sans,
            12.0,
            FontWeight::SemiBold,
            TextAlign::Center,
            false,
        );
        draw_list.push_hit(
            t3_btn,
            HitTarget::EmptyStateNewTab,
            z_layer::PANE_CHROME + 2,
        );

        let term_hot = interaction.is_hovered(&HitTarget::EmptyStateNewTerminal)
            || interaction.is_pressed(&HitTarget::EmptyStateNewTerminal);
        let term_fill = if interaction.is_pressed(&HitTarget::EmptyStateNewTerminal) {
            tokens.surface_row_selected
        } else if term_hot {
            tokens.surface_row_active
        } else {
            tokens.surface_row_hover
        };
        draw_list.push_rect(
            term_btn,
            term_fill,
            Some(if term_hot {
                tokens.accent_primary.with_alpha_f32(0.55)
            } else {
                tokens.border_subtle
            }),
            1.0,
            btn_r,
            None,
        );
        draw_list.push_text(
            term_btn,
            "New Terminal",
            tokens.fg_primary,
            FontKind::Sans,
            12.0,
            FontWeight::Medium,
            TextAlign::Center,
            false,
        );
        draw_list.push_hit(
            term_btn,
            HitTarget::EmptyStateNewTerminal,
            z_layer::PANE_CHROME + 2,
        );
    } else {
        let btn_w = 148.0_f32.min((card.w - 32.0).max(80.0));
        let btn = RectF::new(card.x + (card.w - btn_w) * 0.5, card.y + 118.0, btn_w, 32.0);
        let (target, label) = if is_t3_session {
            (HitTarget::EmptyStateNewTab, "New T3 Thread")
        } else {
            (HitTarget::EmptyStateNewTerminal, "New Terminal")
        };
        let fill = if interaction.is_pressed(&target) {
            tokens.accent_primary.with_alpha_f32(0.80)
        } else if interaction.is_hovered(&target) {
            tokens.accent_primary.with_alpha_f32(0.92)
        } else {
            tokens.accent_primary
        };
        draw_list.push_rect(btn, fill, None, 0.0, btn_r, None);
        draw_list.push_text(
            btn,
            label,
            tokens.accent_foreground,
            FontKind::Sans,
            12.5,
            FontWeight::SemiBold,
            TextAlign::Center,
            false,
        );
        draw_list.push_hit(btn, target, z_layer::PANE_CHROME + 2);
    }
}
