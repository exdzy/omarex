use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gpu_renderer::{
    CellMetrics, ClipRect, Color, CornerRadii, DrawList, FontFamily, FontStyle,
    FontWeight as GpuFontWeight, FrameStats, GlyphAtlas, GpuConfig, GpuShared, ImageQuad, Rect,
    SceneFrame, TextAlign as GpuTextAlign, TextRun, layers, save_rgba_png,
};
use omarchy_theme::{Rgba, Theme, ThemePalette};
use rex_proto::RexEndpoint;
use rex_proto::types::{
    BlockView, LayerView, LayoutNode, PortEntry, PortProcessEntry, PortScanStatus, ProcessOwner,
    ProgramResumeRecord, ServerStatus, ServerVersionResponse, SessionSummary, SessionView,
    SplitNode as ProtoSplitNode, TailscaleAccount, TailscaleHttps, TailscaleStatus,
    TerminalPortsResponse, WindowView,
};
use tr3x_bridge::ClientCommand;
use ui_chrome::{
    FontKind, FontWeight as UiFontWeight, ModalKind, PaneHeaderMode, RectF, SettingsCategory,
    TextAlign as UiTextAlign, TextViewerState, ToolbarBehavior, ToolbarPosition, UiPrimitive,
    UiRenderOutput, ViewerContent, ViewerImagePlacement, classify_path, hit_test_drop_zone,
    parse_path_with_location,
};
use vt_engine::{RowCellSpan, ScreenPoint, SelectionMode};

use crate::config::UserSettings;
use crate::host_manager::{HostManager, RuntimeEvent};
use crate::workspace::{BlockKey, HostEntry, HostStatus, SessionKey};

use super::*;

// ============================================================================
// 5. `UiDrawList` -> `gpu_renderer::DrawList` Z-Ordered Translator
// ============================================================================

/// Renders crisp accent underline spans across every viewport row occupied by a hovered link/path.
pub(super) fn emit_hovered_link_underlines(
    spans: &[RowCellSpan],
    content_rect: RectF,
    clip_rect: Rect,
    metrics: CellMetrics,
    scroll_y: f32,
    color: Color,
    draw_list: &mut DrawList,
) {
    if spans.is_empty() {
        return;
    }
    let cell_w = metrics.cell_width.max(1.0);
    let cell_h = metrics.cell_height.max(1.0);
    let underline_h = metrics.underline_thickness.max(1.5);
    let underline_y_offset = metrics.underline_y.min((cell_h - underline_h).max(0.0));

    draw_list.with_layer(layers::TERMINAL_FG, |dl| {
        dl.with_clip(clip_rect, |dl| {
            for span in spans {
                if span.end_col <= span.start_col {
                    continue;
                }
                let x = content_rect.x + f32::from(span.start_col) * cell_w;
                let w = f32::from(span.end_col - span.start_col) * cell_w;
                let y = content_rect.y + span.row as f32 * cell_h + underline_y_offset + scroll_y;
                dl.push_rect(Rect::new(x, y, w, underline_h), color);
            }
        });
    });
}

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
    translate_ui_to_gpu_with_scale(
        ui_out,
        floating_terminal_frames,
        hover_image,
        1.0,
        atlas,
        draw_list,
    );
}

/// Same as [`translate_ui_to_gpu`], accepting `scale_factor` for caller compatibility.
pub fn translate_ui_to_gpu_with_scale(
    ui_out: &UiRenderOutput,
    floating_terminal_frames: &HashMap<String, SceneFrame>,
    hover_image: Option<(RectF, u64)>,
    _scale_factor: f32,
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
    let mut emitted_viewer_images = vec![false; ui_out.viewer_images.len()];
    let viewer_canvas_rect = ui_out.viewer_metrics.canvas_rect;

    let emit_single_viewer_image =
        |vi: &ViewerImagePlacement, layer: &mut i32, has_fg: &mut bool, dl: &mut DrawList| {
            dl.with_layer(*layer, |dl| {
                let mut quad = ImageQuad::new(vi.texture_key, rectf_to_gpu(vi.dest_rect));
                quad.uv_bounds = vi.uv_bounds;
                quad.clip_rect = rectf_to_gpu(vi.clip_rect).to_clip_rect();
                quad.corner_radius = vi.corner_radius;
                quad.opacity = vi.opacity;
                dl.push_image(quad);
            });
            // Advance layer so paused video play badges, image borders, toolbars, modals & menus
            // always composite strictly ABOVE the viewer media frame!
            *layer += 1;
            *has_fg = false;
        };

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
                if clip_depth == 0
                    && fill.a > 0
                    && tiled_cards.iter().any(|&c| rects_match(*rect, c))
                {
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

                // 4. Viewer image border rect (`fill.a == 0` matching `vi.dest_rect`): emit viewer
                //    image quad on `current_layer` (ABOVE backdrop & checkerboard) BEFORE advancing
                //    layer for the border, toolbars, and overlays!
                if fill.a == 0 {
                    for (img_idx, vi) in ui_out.viewer_images.iter().enumerate() {
                        if !emitted_viewer_images[img_idx] && rects_match(*rect, vi.dest_rect) {
                            emit_single_viewer_image(
                                vi,
                                &mut current_layer,
                                &mut layer_has_foreground,
                                draw_list,
                            );
                            emitted_viewer_images[img_idx] = true;
                        }
                    }
                }

                // 5. General UI rect: if prior glyphs/icons exist on `current_layer`, advance to
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

                // 5b. Video viewer dark canvas backdrop (`rect.inset_uniform(8.0) == canvas_rect`):
                //     emit the video frame immediately on `current_layer` (above backdrop, strictly
                //     below the paused `▶` play badge, scrub bar, and overlays).
                if let Some(canvas_r) = viewer_canvas_rect
                    && rects_match(rect.inset_uniform(8.0), canvas_r)
                {
                    for (img_idx, vi) in ui_out.viewer_images.iter().enumerate() {
                        if !emitted_viewer_images[img_idx] {
                            emit_single_viewer_image(
                                vi,
                                &mut current_layer,
                                &mut layer_has_foreground,
                                draw_list,
                            );
                            emitted_viewer_images[img_idx] = true;
                        }
                    }
                }
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
            UiPrimitive::RichText {
                rect,
                clip_rect,
                spans,
                font_size,
                line_height,
                mono,
            } => {
                if spans.is_empty() {
                    continue;
                }
                layer_has_foreground = true;
                let family = if *mono {
                    FontFamily::Monospace
                } else {
                    FontFamily::SansSerif
                };
                let lh = (*line_height).max(*font_size * 1.15);
                let y = rect.y + ((rect.h - lh) * 0.5).max(0.0);
                let effective_clip = rectf_to_gpu(clip_rect.unwrap_or(*rect)).to_clip_rect();

                let mut cursor_x = rect.x;
                draw_list.with_layer(current_layer, |dl| {
                    for span in spans {
                        if span.text.is_empty() {
                            continue;
                        }
                        let expanded = ui_chrome::syntax::expand_tabs(&span.text);
                        let gpu_weight = if span.bold {
                            GpuFontWeight::Bold
                        } else {
                            GpuFontWeight::Regular
                        };
                        let gpu_style = if span.italic {
                            FontStyle::Italic
                        } else {
                            FontStyle::Normal
                        };

                        let span_x = cursor_x;
                        let advance = atlas.measure_text_advance(
                            &expanded, *font_size, lh, &family, gpu_weight, gpu_style,
                        );
                        cursor_x += advance;

                        if expanded.bytes().all(|b| b == b' ') {
                            continue;
                        }

                        let mut run =
                            TextRun::new(expanded, span_x, y, *font_size, Color::from(span.color))
                                .line_height(lh)
                                .weight(gpu_weight)
                                .style(gpu_style);
                        run.family = family.clone();
                        run.clip_rect = effective_clip;
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

    // Fallback: if any viewer images were not spliced inline above, emit them before hover previews.
    for (img_idx, vi) in ui_out.viewer_images.iter().enumerate() {
        if !emitted_viewer_images[img_idx] {
            emit_single_viewer_image(vi, &mut current_layer, &mut layer_has_foreground, draw_list);
        }
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
        viewer_paths: Vec::new(),
        scenario: None,
    };
    render_headless_live_png_with_options(&opts, out_path)
}

/// Configurable headless live render supporting `--light` theme override in addition to
/// server, session, dimensions, and HiDPI scale factor.
/// Resolves the initial [`Theme`] from `UserSettings` (`theme_name` + `material`) and `--light`.
pub fn resolve_startup_theme(settings: &UserSettings, force_light: bool) -> Theme {
    let user_dir = ThemePalette::default_user_themes_dir();
    let system_dir = ThemePalette::default_system_themes_dir();
    let current_dir = ThemePalette::default_omarchy_dir();
    resolve_startup_theme_in(
        settings,
        force_light,
        user_dir.as_deref(),
        Some(system_dir.as_path()),
        current_dir.as_deref(),
    )
}

pub(super) fn resolve_startup_theme_in(
    settings: &UserSettings,
    force_light: bool,
    user_dir: Option<&Path>,
    system_dir: Option<&Path>,
    current_dir: Option<&Path>,
) -> Theme {
    let live_palette = current_dir
        .and_then(|d| ThemePalette::load_from_omarchy_dir(d).ok())
        .unwrap_or_else(ThemePalette::default_dark);
    let rounding = omarchy_theme::query_current_rounding_or_zero();
    let material = settings.material.to_theme_material();
    if force_light {
        Theme::from_parts(ThemePalette::default_light(), rounding, material)
    } else if let Some(pinned) = settings
        .theme_name
        .as_deref()
        .filter(|n| !n.is_empty() && *n != omarchy_theme::OMARCHY_LIVE_THEME_NAME)
        && let Ok(pal) =
            omarchy_theme::load_theme_by_name_in(pinned, user_dir, system_dir, current_dir)
    {
        Theme::from_parts(pal, rounding, material)
    } else {
        Theme::from_parts(live_palette, rounding, material)
    }
}

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
    let theme = resolve_scenario_startup_theme(&settings, opts.scenario, opts.force_light);

    let mut controller = OmarexController::new(settings, UserSettings::default_path(), theme)?;
    controller.read_only = true;
    controller.clipboard_enabled = false;
    controller.set_preferred_session(opts.session_filter.clone());
    controller.set_viewport(opts.width, opts.height, opts.scale_factor);

    let skip_connect_wait = !controller
        .settings
        .saved_hosts
        .iter()
        .any(|h| h.auto_connect)
        && matches!(&endpoint, RexEndpoint::Unix(p) if !p.exists());
    let mut host_connected = false;
    if !skip_connect_wait {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("failed to build Tokio runtime for headless render")?;

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
            let mut multiwin_secondary_opened = false;
            let mut ports_listed_blocks: HashSet<BlockKey> = HashSet::new();
            let mut active_port_keys: Vec<BlockKey> = Vec::new();

            while tokio::time::Instant::now() < deadline {
                // Once bootstrap + active view + active pane snapshot READY + metadata hydrated
                // (and any auto-connect remote hosts bootstrapped or grace window elapsed),
                // settle briefly (~180ms) for trailing history/status/ports metadata, then exit.
                let remotes_settled = expected_remote_hosts
                    .iter()
                    .all(|id| bootstrapped_hosts.contains(id))
                    || started.elapsed() >= remote_grace;
                let ports_settled = !host_connected
                    || (!active_port_keys.is_empty()
                        && active_port_keys
                            .iter()
                            .all(|k| ports_listed_blocks.contains(k)))
                    || ready_since.is_some_and(|s| s.elapsed() >= Duration::from_millis(450));
                if let Some(since) = ready_since
                    && since.elapsed() >= Duration::from_millis(180)
                    && remotes_settled
                    && ports_settled
                {
                    break;
                }
                let wait_for = if let Some(since) = ready_since
                    && remotes_settled
                {
                    let settle_target = if ports_settled {
                        Duration::from_millis(180)
                    } else {
                        Duration::from_millis(450)
                    };
                    settle_target.saturating_sub(since.elapsed())
                } else {
                    deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .min(Duration::from_millis(25))
                };

                match tokio::time::timeout(wait_for, rx.recv()).await {
                    Ok(Some(ev)) => {
                        let mut local_connect_failed = false;
                        let mut inspect = |e: &RuntimeEvent| {
                            if matches!(e, RuntimeEvent::HostConnected { .. }) {
                                host_connected = true;
                            }
                            if let RuntimeEvent::BootstrapReady { host_id, .. } = e {
                                bootstrapped_hosts.insert(host_id.clone());
                            }
                            if let RuntimeEvent::PortsListed { key, .. } = e {
                                ports_listed_blocks.insert(key.clone());
                            }
                            if let RuntimeEvent::HostError {
                                host_id, context, ..
                            } = e
                                && host_id == LOCAL_HOST_ID
                                && context == "connect"
                                && expected_remote_hosts.is_empty()
                            {
                                local_connect_failed = true;
                            }
                        };
                        inspect(&ev);
                        controller.process_runtime_event(ev, Instant::now());
                        // Drain any immediately buffered events in one batch.
                        while let Ok(more) = rx.try_recv() {
                            inspect(&more);
                            controller.process_runtime_event(more, Instant::now());
                        }
                        let _ = controller.tick(Instant::now());
                        if host_connected {
                            active_port_keys = controller.query_active_window_ports();
                        }

                        if opts.scenario == Some(HeadlessScenario::MultiWindow)
                            && !multiwin_secondary_opened
                            && let Some(primary_sk) = controller.workspace.active_session.clone()
                            && let Some((sec_sk, _)) =
                                find_secondary_session_target(&controller, &primary_sk)
                        {
                            controller.open_active_window_streams(&sec_sk, &HashMap::new());
                            multiwin_secondary_opened = true;
                        }

                        if local_connect_failed {
                            break;
                        }
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
                        if host_connected {
                            active_port_keys = controller.query_active_window_ports();
                        }
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
    }

    let shared = GpuShared::new_headless(&GpuConfig::default())
        .context("failed to initialize headless GpuShared")?;
    {
        let mut atlas = shared.atlas();
        register_all_ui_icons(&mut atlas);
    }

    let has_live_session = host_connected
        && controller.workspace.active_session.is_some()
        && controller.has_ready_active_pane();

    if opts.scenario == Some(HeadlessScenario::MultiWindow) {
        if has_live_session {
            return render_live_multi_window_scenario(
                &mut controller,
                &shared,
                opts,
                out_path.as_ref(),
            );
        }
        return render_headless_multi_window_scenario(
            &shared,
            controller.settings.clone(),
            controller.theme.clone(),
            opts,
            out_path.as_ref(),
        );
    }

    if let Some(scenario) = opts.scenario {
        if has_live_session {
            apply_scenario_to_live_session(&mut controller, scenario, opts);
        } else {
            seed_scenario_state(&mut controller, scenario, !opts.viewer_paths.is_empty());
        }
    }

    // Optional `--headless-viewer <PATH>[:LINE]` tabs: open and wait for async decode/poster load.
    open_headless_viewer_paths(&mut controller, &opts.viewer_paths);

    // Warm up glyphs/icons & sync viewer layout metrics, then render final PNG.
    controller.ui.snap_animations();
    let mut frame = controller.render_frame_shared(&shared, Instant::now())?;
    let mut need_second_pass = controller.ui.viewer.open;
    if opts.scenario == Some(HeadlessScenario::DndSplit) {
        activate_dnd_split_overlay(&mut controller);
        need_second_pass = true;
    }
    if opts.scenario == Some(HeadlessScenario::CopyMode)
        && controller.copy_shines.is_empty()
        && let Some(key) = controller.focused_block_key()
    {
        let shine_now = Instant::now() - Duration::from_millis(90);
        if !controller.trigger_copy_shine_for_block(&key, shine_now) {
            let (sx, sy, sw, sh) =
                clamped_copy_shine_rect(controller.viewport_w, controller.viewport_h);
            controller.trigger_copy_shine(
                vec![Rect::new(sx, sy, sw, sh)],
                ClipRect::UNBOUNDED,
                shine_now,
            );
        }
        need_second_pass = true;
    }
    if need_second_pass {
        // Second pass guarantees `sync_viewer_metrics`, `last_ui_output` DnD overlay, or
        // selection-aligned Copy Shine is reflected cleanly in the saved screenshot.
        frame = controller.render_frame_shared(&shared, Instant::now())?;
    }
    let frame_stats = shared.render_to_png(&frame, out_path.as_ref())?;

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

pub(super) fn resolve_scenario_startup_theme(
    settings: &UserSettings,
    scenario: Option<HeadlessScenario>,
    force_light_flag: bool,
) -> Theme {
    let user_dir = ThemePalette::default_user_themes_dir();
    let system_dir = ThemePalette::default_system_themes_dir();
    let current_dir = ThemePalette::default_omarchy_dir();
    resolve_scenario_startup_theme_in(
        settings,
        scenario,
        force_light_flag,
        user_dir.as_deref(),
        Some(system_dir.as_path()),
        current_dir.as_deref(),
    )
}

pub(super) fn resolve_scenario_startup_theme_in(
    settings: &UserSettings,
    scenario: Option<HeadlessScenario>,
    force_light_flag: bool,
    user_dir: Option<&Path>,
    system_dir: Option<&Path>,
    current_dir: Option<&Path>,
) -> Theme {
    let Some(scenario) = scenario else {
        return resolve_startup_theme_in(
            settings,
            force_light_flag,
            user_dir,
            system_dir,
            current_dir,
        );
    };
    let force_light = match scenario {
        HeadlessScenario::SidebarDark => false,
        HeadlessScenario::SidebarLight => true,
        _ => force_light_flag,
    };
    let base_theme =
        resolve_startup_theme_in(settings, force_light, user_dir, system_dir, current_dir);
    match scenario {
        HeadlessScenario::SidebarDark if !force_light_flag && !base_theme.is_dark => {
            Theme::from_parts(
                ThemePalette::default_dark(),
                base_theme.rounding,
                base_theme.material,
            )
        }
        HeadlessScenario::SidebarLight => Theme::from_parts(
            ThemePalette::default_light(),
            base_theme.rounding,
            base_theme.material,
        ),
        _ => base_theme,
    }
}

fn clamped_copy_shine_rect(viewport_w: f32, viewport_h: f32) -> (f32, f32, f32, f32) {
    let sw = 320.0_f32.min((viewport_w - 32.0).max(80.0));
    let sh = 56.0_f32.min((viewport_h - 32.0).max(24.0));
    let sx = 340.0_f32.min((viewport_w - sw - 16.0).max(8.0));
    let sy = 420.0_f32.min((viewport_h - sh - 16.0).max(8.0));
    (sx, sy, sw, sh)
}

fn open_headless_viewer_paths(controller: &mut OmarexController, viewer_paths: &[String]) {
    if viewer_paths.is_empty() {
        return;
    }
    for raw_spec in viewer_paths {
        let parsed = parse_path_with_location(raw_spec);
        let target = classify_path(&parsed.path, parsed.line, parsed.end_line, parsed.column);
        if let Some(resolved) = controller.resolve_local_file_path(&parsed.path, None) {
            controller.open_local_path_in_viewer(
                resolved,
                parsed.path,
                parsed.line,
                parsed.column,
                Some(target.kind()),
            );
        }
    }
    let load_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < load_deadline {
        controller.poll_media_events();
        let any_loading = controller.ui.viewer.tabs.iter().any(|t| {
            matches!(t.content, ViewerContent::Loading { .. })
                || matches!(&t.content, ViewerContent::Pdf(pdf) if pdf.loading_page)
        });
        if !any_loading {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    controller.poll_media_events();
}

fn find_secondary_session_target(
    ctrl: &OmarexController,
    primary_sk: &SessionKey,
) -> Option<(SessionKey, Option<String>)> {
    for hid in &ctrl.workspace.host_order {
        let Some(host) = ctrl.workspace.host(hid) else {
            continue;
        };
        for sid in &host.session_order {
            let sk = SessionKey::new(hid.clone(), sid.clone());
            if &sk == primary_sk {
                continue;
            }
            if let Some(sess) = host.sessions.get(sid)
                && let Some(view) = sess.view.as_ref()
            {
                let bid = active_window_of_view(view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);
                if bid.is_some() {
                    return Some((sk, bid));
                }
            }
        }
    }
    None
}

pub(super) fn apply_scenario_to_live_session(
    ctrl: &mut OmarexController,
    scenario: HeadlessScenario,
    opts: &HeadlessRenderOptions,
) {
    let now = Instant::now();
    match scenario {
        HeadlessScenario::SidebarDark | HeadlessScenario::SidebarLight => {
            ctrl.ui.layout_cfg.toolbar_position = ToolbarPosition::Left;
            ctrl.ui.layout_cfg.toolbar_behavior = ToolbarBehavior::Always;
            ctrl.ui.layout_cfg.pane_header_mode = PaneHeaderMode::Always;
        }
        HeadlessScenario::CopyMode => {
            let Some(key) = ctrl.focused_block_key() else {
                return;
            };
            ctrl.execute_client_command(&key.host_id, ClientCommand::EnterCopyMode, now);
            if let Some((cols, total_rows, sb)) = ctrl.pane_store.copy_mode_screen_bounds(&key) {
                let vis_top = u32::try_from(sb.offset).unwrap_or(0);
                let vis_len = u32::try_from(sb.len).unwrap_or(1).max(1);
                let max_row = total_rows.saturating_sub(1);
                let vis_bot = vis_top
                    .saturating_add(vis_len.saturating_sub(1))
                    .min(max_row);
                let anchor_row = vis_top.saturating_add(vis_len / 5).min(vis_bot);
                let cursor_row = anchor_row.saturating_add(3).min(vis_bot);
                let max_col = cols.saturating_sub(1);
                let anchor_col = 2u16.min(max_col);
                let cursor_col = 40u16.min(max_col);
                let anchor = ScreenPoint {
                    col: anchor_col,
                    row: anchor_row,
                };
                let cursor = ScreenPoint {
                    col: cursor_col,
                    row: cursor_row,
                };
                let _ = ctrl.pane_store.copy_mode_select_range(
                    &key,
                    anchor,
                    cursor,
                    SelectionMode::Char,
                );
                if let Some(cm) = ctrl.copy_mode.as_mut() {
                    cm.visual = Some((SelectionMode::Char, anchor));
                    cm.cursor = cursor;
                }
            }
        }
        HeadlessScenario::SyntaxViewer => {
            if opts.viewer_paths.is_empty() && ctrl.ui.viewer.tabs.is_empty() {
                let rust_sample = "\
use std::sync::Arc;
use gpu_renderer::{GpuShared, SurfaceRenderer};

/// Multi-window shared Vulkan device & atlas state.
pub struct WindowHandle {
    pub shared: Arc<GpuShared>,
    pub surface: SurfaceRenderer,
}

pub fn render_frame(win: &mut WindowHandle, width: u32, height: u32) -> bool {
    let ratio = (width as f32) / (height.max(1) as f32);
    ratio > 0.0 && win.surface.surface_size() != (0, 0)
}
";
                let txt = TextViewerState::new(rust_sample, Some("rust".to_owned()), false);
                ctrl.ui.viewer.open_or_focus_tab(
                    "src/multi_window.rs",
                    None,
                    ViewerContent::Text(txt),
                );
            }
        }
        HeadlessScenario::MarkdownViewer => {
            if opts.viewer_paths.is_empty() && ctrl.ui.viewer.tabs.is_empty() {
                let md_sample = "\
# Agent Companion 2.0

Rich **Markdown** preview with syntax-highlighted code blocks and GFM tables.

| Feature | Crate | Status |
|---|---|---|
| Multi-Window | `gpu-renderer` | Ready |
| 5-Zone DnD | `ui-chrome` | Ready |
| Copy Mode | `vt-engine` | Ready |

```rust
fn connect(host: &str) -> Result<Session, Error> {
    let endpoint = host.parse()?;
    Ok(Session::attach(endpoint))
}
```

> All Vulkan resources share `Arc<GpuShared>` cleanly across OS windows.
";
                let txt = TextViewerState::new(md_sample, Some("markdown".to_owned()), true);
                ctrl.ui.viewer.open_or_focus_tab(
                    "docs/overview.md",
                    None,
                    ViewerContent::Text(txt),
                );
            }
        }
        HeadlessScenario::DiagnosticsHud => {
            if ctrl.frame_times_us.is_empty() {
                for us in [920u32, 1080, 1140, 980, 1290, 1410, 1020, 890, 1190, 1050] {
                    ctrl.frame_times_us.push_back(us);
                }
            }
            ctrl.open_modal_kind(ModalKind::Diagnostics);
        }
        HeadlessScenario::ThemeSwitcher => {
            ctrl.open_modal_kind(ModalKind::ThemePicker);
        }
        HeadlessScenario::SettingsModal => {
            ctrl.open_modal_kind(ModalKind::Settings);
        }
        HeadlessScenario::SettingsIosQr => {
            ctrl.open_settings_at(SettingsCategory::ConnectIos);
        }
        HeadlessScenario::SettingsServer => {
            ctrl.open_settings_at(SettingsCategory::Server);
        }
        HeadlessScenario::ProgramResume => {}
        HeadlessScenario::PortsPill => {
            let Some(sk) = ctrl.workspace.active_session.clone() else {
                return;
            };
            let focused_bid = ctrl.focused_block_key().map(|k| k.block_id);
            let mut has_live_ports = false;
            let mut fallback_bid: Option<String> = focused_bid;
            if let Some(view) = ctrl.workspace.session(&sk).and_then(|s| s.view.as_ref())
                && let Some(win) = active_window_of_view(view)
            {
                for blk in win.layers.iter().flat_map(|l| l.blocks.iter()) {
                    if let Some(bid) = blk.block_id.as_ref() {
                        if fallback_bid.is_none() {
                            fallback_bid = Some(bid.clone());
                        }
                        let bk = BlockKey::new(&sk.host_id, &sk.session_id, bid);
                        if ctrl
                            .workspace
                            .block(&bk)
                            .is_some_and(|b| !b.listening_ports.is_empty())
                        {
                            has_live_ports = true;
                            break;
                        }
                    }
                }
            }
            if !has_live_ports && let Some(bid) = fallback_bid {
                let bk = BlockKey::new(&sk.host_id, &sk.session_id, bid);
                if let Some(blk) = ctrl.workspace.block_mut(&bk) {
                    blk.apply_ports_response(sample_ports_response());
                }
            }
        }
        HeadlessScenario::SplitPanes => {
            let Some(sk) = ctrl.workspace.active_session.clone() else {
                return;
            };
            let Some(sess) = ctrl.workspace.session_mut(&sk) else {
                return;
            };
            let Some(view) = sess.view.as_mut() else {
                return;
            };
            let win_idx = view
                .active_window_id
                .as_deref()
                .and_then(|aid| {
                    view.windows
                        .iter()
                        .position(|w| w.window_id.as_deref() == Some(aid))
                })
                .unwrap_or(0);
            let Some(win) = view.windows.get_mut(win_idx) else {
                return;
            };
            let tiled_count: usize = win
                .layers
                .iter()
                .filter(|l| l.kind.eq_ignore_ascii_case("tiled"))
                .map(|l| l.blocks.len())
                .sum();
            if tiled_count >= 2 {
                return;
            }

            let second_bid = "block:split-preview".to_owned();
            let first_bid = if let Some(tiled_layer) = win
                .layers
                .iter_mut()
                .find(|l| l.kind.eq_ignore_ascii_case("tiled"))
            {
                let first_id = tiled_layer
                    .blocks
                    .first()
                    .and_then(|b| b.block_id.clone())
                    .unwrap_or_else(|| "block:1".to_owned());
                if tiled_layer.blocks.is_empty() {
                    tiled_layer.blocks.push(BlockView {
                        block_id: Some(first_id.clone()),
                        label: "cargo build".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:local".into()),
                        owner: None,
                    });
                }
                let host_meta = tiled_layer.blocks.first().and_then(|b| b.host_id.clone());
                let owner_meta = tiled_layer.blocks.first().and_then(|b| b.owner.clone());
                tiled_layer.layout = Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: None,
                    split: Some(Box::new(ProtoSplitNode {
                        split_id: Some("split:preview".into()),
                        direction: "horizontal".into(),
                        ratio: 0.52,
                        before: LayoutNode {
                            rect: [0.0, 0.0, 0.52, 1.0],
                            block_id: Some(first_id.clone()),
                            split: None,
                        },
                        after: LayoutNode {
                            rect: [0.52, 0.0, 0.48, 1.0],
                            block_id: Some(second_bid.clone()),
                            split: None,
                        },
                    })),
                });
                tiled_layer.blocks.push(BlockView {
                    block_id: Some(second_bid.clone()),
                    label: "git diff".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: host_meta,
                    owner: owner_meta,
                });
                first_id
            } else {
                let first_id = "block:1".to_owned();
                win.layers.insert(
                    0,
                    LayerView {
                        layer_id: Some("layer:tiled".into()),
                        kind: "tiled".into(),
                        bounds: [0.0, 0.0, 1.0, 1.0],
                        state: None,
                        layout: Some(LayoutNode {
                            rect: [0.0, 0.0, 1.0, 1.0],
                            block_id: None,
                            split: Some(Box::new(ProtoSplitNode {
                                split_id: Some("split:preview".into()),
                                direction: "horizontal".into(),
                                ratio: 0.52,
                                before: LayoutNode {
                                    rect: [0.0, 0.0, 0.52, 1.0],
                                    block_id: Some(first_id.clone()),
                                    split: None,
                                },
                                after: LayoutNode {
                                    rect: [0.52, 0.0, 0.48, 1.0],
                                    block_id: Some(second_bid.clone()),
                                    split: None,
                                },
                            })),
                        }),
                        blocks: vec![
                            BlockView {
                                block_id: Some(first_id.clone()),
                                label: "cargo build".into(),
                                creator_name: Some("com.superlogical.terminal".into()),
                                flavor: Some("com.superlogical.terminal.shell".into()),
                                host_id: Some("host:local".into()),
                                owner: None,
                            },
                            BlockView {
                                block_id: Some(second_bid.clone()),
                                label: "git diff".into(),
                                creator_name: Some("com.superlogical.terminal".into()),
                                flavor: Some("com.superlogical.terminal.shell".into()),
                                host_id: Some("host:local".into()),
                                owner: None,
                            },
                        ],
                    },
                );
                first_id
            };

            let key1 = BlockKey::new(&sk.host_id, &sk.session_id, &first_bid);
            if !ctrl.pane_store.is_ready(&key1) {
                let term1 = vt_engine::Terminal::new(90, 36, vt_engine::TerminalConfig::default())
                    .expect("create split terminal 1");
                term1.write(b"\x1b[1;34m~/Work/omarex\x1b[0m \x1b[32mmain\x1b[0m cargo build\r\n");
                let _ = ctrl.pane_store.insert_terminal(key1, term1);
            }
            let term2 = vt_engine::Terminal::new(90, 36, vt_engine::TerminalConfig::default())
                .expect("create split terminal 2");
            term2.write(
                b"\x1b[1;35m[Split Pane 2]\x1b[0m git status --short\r\n\
                  \x1b[32m M\x1b[0m crates/gpu-renderer/src/lib.rs\r\n\
                  \x1b[32m M\x1b[0m crates/omarex/src/app.rs\r\n\
                  \x1b[32m M\x1b[0m crates/omarex/src/bridge/render.rs\r\n",
            );
            let _ = ctrl.pane_store.insert_terminal(
                BlockKey::new(&sk.host_id, &sk.session_id, second_bid),
                term2,
            );
            let _ = ctrl.pane_store.apply_theme_all(&ctrl.theme.palette);
        }
        HeadlessScenario::FloatingPr => {
            let Some(sk) = ctrl.workspace.active_session.clone() else {
                return;
            };
            let already_has_floating = ctrl
                .workspace
                .session(&sk)
                .and_then(|s| s.view.as_ref())
                .and_then(active_window_of_view)
                .is_some_and(|w| {
                    w.layers
                        .iter()
                        .any(|l| l.kind.eq_ignore_ascii_case("floating") && !l.blocks.is_empty())
                });
            if already_has_floating {
                return;
            }
            let Some(sess) = ctrl.workspace.session_mut(&sk) else {
                return;
            };
            let Some(view) = sess.view.as_mut() else {
                return;
            };
            let win_idx = view
                .active_window_id
                .as_deref()
                .and_then(|aid| {
                    view.windows
                        .iter()
                        .position(|w| w.window_id.as_deref() == Some(aid))
                })
                .unwrap_or(0);
            let Some(win) = view.windows.get_mut(win_idx) else {
                return;
            };
            win.layers.push(LayerView {
                layer_id: Some("layer:float-pr".into()),
                kind: "floating".into(),
                bounds: [0.22, 0.14, 0.64, 0.72],
                state: Some("normal".into()),
                layout: None,
                blocks: vec![BlockView {
                    block_id: Some("block:pr".into()),
                    label: "pr:#142 Multi-Window Vulkan".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.pr".into()),
                    host_id: Some("host:local".into()),
                    owner: None,
                }],
            });
            let term_pr = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
                .expect("create floating pr terminal");
            term_pr.write(
                b"\x1b[1;33mPR #142\x1b[0m True Multi-Window & 5-Zone DnD\r\n\
                  \x1b[32m+420\x1b[0m \x1b[31m-118\x1b[0m across 9 files\r\n\
                  Checks: \x1b[32mclippy OK\x1b[0m | \x1b[32mfmt OK\x1b[0m | \x1b[32mtests OK\x1b[0m\r\n",
            );
            let _ = ctrl.pane_store.insert_terminal(
                BlockKey::new(&sk.host_id, &sk.session_id, "block:pr"),
                term_pr,
            );
            let _ = ctrl.pane_store.apply_theme_all(&ctrl.theme.palette);
            ctrl.ui.focus.focus_terminal(Some("block:pr".to_owned()));
        }
        HeadlessScenario::DndSplit | HeadlessScenario::MultiWindow => {}
    }
}

fn render_live_multi_window_scenario(
    controller: &mut OmarexController,
    shared: &GpuShared,
    opts: &HeadlessRenderOptions,
    out_path: &Path,
) -> Result<HeadlessRenderReport> {
    let sk_primary = controller
        .workspace
        .active_session
        .clone()
        .context("missing active live session for multi-window scenario")?;
    let block_primary = controller.focused_block_key().map(|k| k.block_id);
    let (sk_secondary, block_secondary) = find_secondary_session_target(controller, &sk_primary)
        .unwrap_or_else(|| (sk_primary.clone(), block_primary.clone()));

    let (active_session_label, live_windows) = controller
        .workspace
        .session(&sk_primary)
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

    render_two_window_composite(
        controller,
        shared,
        opts,
        out_path,
        sk_primary,
        block_primary,
        sk_secondary,
        block_secondary,
        true,
        active_session_label,
        live_windows.max(2),
        ready_panes.max(2),
    )
}

fn render_headless_multi_window_scenario(
    shared: &GpuShared,
    settings: UserSettings,
    theme: Theme,
    opts: &HeadlessRenderOptions,
    out_path: &Path,
) -> Result<HeadlessRenderReport> {
    let mut controller = OmarexController::new(settings, UserSettings::default_path(), theme)?;
    controller.read_only = true;
    controller.clipboard_enabled = false;
    seed_multi_window_workspace(&mut controller);

    let sk_primary = SessionKey::new(LOCAL_HOST_ID, "session:work");
    let sk_secondary = SessionKey::new(LOCAL_HOST_ID, "session:torn");

    let ready_panes = controller
        .pane_store
        .pane_keys()
        .iter()
        .filter(|k| controller.pane_store.is_ready(k))
        .count();

    render_two_window_composite(
        &mut controller,
        shared,
        opts,
        out_path,
        sk_primary,
        Some("block:1".to_owned()),
        sk_secondary,
        Some("block:2".to_owned()),
        false,
        Some("Multi-Window (Work + Torn Tab)".to_owned()),
        2,
        ready_panes.max(2),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_two_window_composite(
    controller: &mut OmarexController,
    shared: &GpuShared,
    opts: &HeadlessRenderOptions,
    out_path: &Path,
    sk_primary: SessionKey,
    block_primary: Option<String>,
    sk_secondary: SessionKey,
    block_secondary: Option<String>,
    host_connected: bool,
    active_session_label: Option<String>,
    total_windows: usize,
    ready_panes: usize,
) -> Result<HeadlessRenderReport> {
    let total_w = opts.width.max(320);
    let total_h = opts.height.max(240);
    let left_w = total_w / 2;
    let right_w = total_w - left_w;

    let mut ctx_left = controller.create_window_context(
        left_w,
        total_h,
        opts.scale_factor,
        Some(sk_primary),
        block_primary,
    );
    ctx_left.ui.layout_cfg.toolbar_position = ToolbarPosition::Left;
    ctx_left.ui.layout_cfg.toolbar_behavior = ToolbarBehavior::Always;

    let mut ctx_right = controller.create_window_context(
        right_w,
        total_h,
        opts.scale_factor,
        Some(sk_secondary),
        block_secondary,
    );
    ctx_right.ui.layout_cfg.toolbar_position = ToolbarPosition::Top;
    ctx_right.ui.layout_cfg.toolbar_behavior = ToolbarBehavior::Always;

    let target_left = shared.create_offscreen_target(left_w, total_h)?;
    let target_right = shared.create_offscreen_target(right_w, total_h)?;

    let frame_left = controller.with_window_context(&mut ctx_left, |ctrl| {
        ctrl.ui.snap_animations();
        ctrl.render_frame_shared(shared, Instant::now())
    })?;
    let frame_right = controller.with_window_context(&mut ctx_right, |ctrl| {
        ctrl.ui.snap_animations();
        ctrl.render_frame_shared(shared, Instant::now())
    })?;

    let stats_left = shared.render_to_offscreen(&frame_left, &target_left)?;
    let stats_right = shared.render_to_offscreen(&frame_right, &target_right)?;
    let rgba_left = shared.readback_offscreen(&target_left)?;
    let rgba_right = shared.readback_offscreen(&target_right)?;

    let mut combined = vec![0u8; (total_w * total_h * 4) as usize];
    for row in 0..total_h as usize {
        let dst_row = row * total_w as usize * 4;
        let left_row = row * left_w as usize * 4;
        let right_row = row * right_w as usize * 4;
        let left_bytes = left_w as usize * 4;
        let right_bytes = right_w as usize * 4;
        combined[dst_row..dst_row + left_bytes]
            .copy_from_slice(&rgba_left[left_row..left_row + left_bytes]);
        combined[dst_row + left_bytes..dst_row + left_bytes + right_bytes]
            .copy_from_slice(&rgba_right[right_row..right_row + right_bytes]);
        // Draw a crisp 2px divider seam between Window 1 and Window 2.
        if left_w >= 2 {
            let seam_x = left_w as usize - 1;
            for sx in [seam_x, (seam_x + 1).min(total_w as usize - 1)] {
                let p = dst_row + sx * 4;
                combined[p] = 226;
                combined[p + 1] = 130;
                combined[p + 2] = 3;
                combined[p + 3] = 255;
            }
        }
    }
    save_rgba_png(out_path, total_w, total_h, &combined)?;

    let combined_stats = FrameStats {
        cpu_prepare_us: stats_left.cpu_prepare_us + stats_right.cpu_prepare_us,
        gpu_encode_submit_us: stats_left.gpu_encode_submit_us + stats_right.gpu_encode_submit_us,
        total_us: stats_left.total_us + stats_right.total_us,
        draw_calls: stats_left.draw_calls + stats_right.draw_calls,
        quad_count: stats_left.quad_count + stats_right.quad_count,
        glyph_count: stats_left.glyph_count + stats_right.glyph_count,
        image_count: stats_left.image_count + stats_right.image_count,
        adapter_name: stats_left.adapter_name,
        backend: stats_left.backend,
    };

    Ok(HeadlessRenderReport {
        output_path: out_path.to_path_buf(),
        width: total_w,
        height: total_h,
        scale_factor: opts.scale_factor,
        host_connected,
        active_session_label,
        total_windows,
        ready_panes,
        frame_stats: combined_stats,
    })
}

fn seed_multi_window_workspace(ctrl: &mut OmarexController) {
    let mut host = HostEntry::new(
        LOCAL_HOST_ID,
        "This Machine",
        RexEndpoint::parse("unix:///tmp/headless-multiwin.sock").unwrap(),
    );
    host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:local".into()),
        api_revision: 24,
        resumed: false,
    };
    host.on_session_created(SessionSummary {
        session_id: "session:work".into(),
        label: "Work".into(),
    });
    host.on_session_created(SessionSummary {
        session_id: "session:torn".into(),
        label: "Torn Tab".into(),
    });
    ctrl.workspace.add_host(host);

    let view1 = SessionView {
        revision: 1,
        session_id: Some("session:work".into()),
        label: "Work".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Editor & Build".into(),
            focused_block_id: Some("block:1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled-1".into()),
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
                    label: "cargo test".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        }],
    };
    let view2 = SessionView {
        revision: 1,
        session_id: Some("session:torn".into()),
        label: "Torn Tab".into(),
        active_window_id: Some("window:2".into()),
        windows: vec![WindowView {
            window_id: Some("window:2".into()),
            label: "Torn Telemetry".into(),
            focused_block_id: Some("block:2".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled-2".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: Some("block:2".into()),
                    split: None,
                }),
                blocks: vec![BlockView {
                    block_id: Some("block:2".into()),
                    label: "gpu monitor".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        }],
    };

    let now = Instant::now();
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:work".into(),
            view: Box::new(view1),
            seq: 1,
        },
        now,
    );
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:torn".into(),
            view: Box::new(view2),
            seq: 2,
        },
        now,
    );

    let term1 = vt_engine::Terminal::new(90, 32, vt_engine::TerminalConfig::default())
        .expect("create term1");
    term1.write(
        b"\x1b[1;36m[Window 1 - Work]\x1b[0m cargo test --workspace\r\n\
          \x1b[32m  ok\x1b[0m gpu-renderer::Arc<GpuShared> initialized\r\n\
          \x1b[32m  ok\x1b[0m fn transfer(a -> b) => Result<()>\r\n",
    );
    let term2 = vt_engine::Terminal::new(90, 32, vt_engine::TerminalConfig::default())
        .expect("create term2");
    term2.write(
        b"\x1b[1;33m[Window 2 - Torn Tab]\x1b[0m session.transfer_window\r\n\
          \x1b[35m  streams\x1b[0m reopened on target_session_id\r\n\
          \x1b[32m  atlas\x1b[0m shared GlyphAtlas + ImageTextureCache\r\n",
    );
    let _ = ctrl.pane_store.insert_terminal(
        BlockKey::new(LOCAL_HOST_ID, "session:work", "block:1"),
        term1,
    );
    let _ = ctrl.pane_store.insert_terminal(
        BlockKey::new(LOCAL_HOST_ID, "session:torn", "block:2"),
        term2,
    );
    ctrl.drain_pending_commands();
}

pub(super) fn seed_scenario_state(
    ctrl: &mut OmarexController,
    scenario: HeadlessScenario,
    has_explicit_viewer_paths: bool,
) {
    let now = Instant::now();
    let mut local_host = HostEntry::new(
        LOCAL_HOST_ID,
        "This Machine",
        RexEndpoint::parse("unix:///tmp/headless-scenario.sock").unwrap(),
    );
    local_host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:local".into()),
        api_revision: 24,
        resumed: false,
    };
    local_host.tailscale_status = Some(TailscaleStatus {
        state: "running".into(),
        dns_name: Some("omarchy-workstation.tail000000.ts.net".into()),
        error: None,
        auth_url: None,
        account: Some(TailscaleAccount {
            login_name: "mike@omarchy.org".into(),
        }),
        tailnet_name: Some("omarchy.ts.net".into()),
        allow_tailnet_members: false,
        tags: Vec::new(),
        https: Some(TailscaleHttps {
            state: "available".into(),
            url: Some("https://omarchy-workstation.tail000000.ts.net".into()),
        }),
        control_url: None,
    });
    if matches!(
        scenario,
        HeadlessScenario::SettingsServer | HeadlessScenario::ProgramResume
    ) {
        local_host.server_status = Some(ServerStatus {
            pid: 1031,
            instance_id: "01a6468f-9065-47c0-951e-72119dd0e815".into(),
            host_id: Some("host:local".into()),
            started_at: "2026-11-27T09:56:52Z".into(),
            exit_empty: false,
            session_count: 2,
            tailscale: local_host.tailscale_status.clone(),
            api_revision: Some(24),
            api_min_revision: Some(0),
            config: None,
            listeners: Vec::new(),
        });
        local_host.server_version = Some(ServerVersionResponse {
            rex_version: "0.1.0-preview.25".into(),
            platform: "linux/amd64".into(),
            commit: Some("b070aae".into()),
            built_at: Some("2026-11-25T21:59:11Z".into()),
            dirty: false,
            api_revision: Some(24),
            api_min_revision: Some(0),
        });
    }
    local_host.on_session_created(SessionSummary {
        session_id: "session:work".into(),
        label: "Work".into(),
    });
    local_host.on_session_created(SessionSummary {
        session_id: "session:t3".into(),
        label: "T3 Code".into(),
    });
    ctrl.workspace.add_host(local_host);

    let mut remote_host = HostEntry::new(
        "remote:mac",
        "macbook-pro",
        RexEndpoint::parse("ssh://mike@macbook-pro").unwrap(),
    );
    remote_host.is_local = false;
    remote_host.status = HostStatus::Connected {
        client_id: "client:2".into(),
        server_host_id: Some("host:remote".into()),
        api_revision: 24,
        resumed: false,
    };
    remote_host.on_session_created(SessionSummary {
        session_id: "session:prod".into(),
        label: "Build Farm".into(),
    });
    ctrl.workspace.add_host(remote_host);

    let active_sk = SessionKey::new(LOCAL_HOST_ID, "session:work");
    ctrl.workspace.select_session(active_sk.clone());

    let (layout_node, blocks, extra_layers) = match scenario {
        HeadlessScenario::SplitPanes
        | HeadlessScenario::DndSplit
        | HeadlessScenario::ProgramResume => (
            LayoutNode {
                rect: [0.0, 0.0, 1.0, 1.0],
                block_id: None,
                split: Some(Box::new(ProtoSplitNode {
                    split_id: Some("split:1".into()),
                    direction: "horizontal".into(),
                    ratio: 0.52,
                    before: LayoutNode {
                        rect: [0.0, 0.0, 0.52, 1.0],
                        block_id: Some("block:1".into()),
                        split: None,
                    },
                    after: LayoutNode {
                        rect: [0.52, 0.0, 0.48, 1.0],
                        block_id: Some("block:2".into()),
                        split: None,
                    },
                })),
            },
            vec![
                BlockView {
                    block_id: Some("block:1".into()),
                    label: if scenario == HeadlessScenario::ProgramResume {
                        "claude — omarex".into()
                    } else {
                        "cargo build".into()
                    },
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                },
                BlockView {
                    block_id: Some("block:2".into()),
                    label: if scenario == HeadlessScenario::ProgramResume {
                        "nvim — README.md".into()
                    } else {
                        "git diff".into()
                    },
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                },
            ],
            Vec::new(),
        ),
        HeadlessScenario::FloatingPr => (
            LayoutNode {
                rect: [0.0, 0.0, 1.0, 1.0],
                block_id: Some("block:1".into()),
                split: None,
            },
            vec![BlockView {
                block_id: Some("block:1".into()),
                label: "workspace shell".into(),
                creator_name: Some("com.superlogical.terminal".into()),
                flavor: Some("com.superlogical.terminal.shell".into()),
                host_id: Some("host:local".into()),
                owner: Some("client:1".into()),
            }],
            vec![LayerView {
                layer_id: Some("layer:float-pr".into()),
                kind: "floating".into(),
                bounds: [0.22, 0.14, 0.64, 0.72],
                state: Some("normal".into()),
                layout: None,
                blocks: vec![BlockView {
                    block_id: Some("block:pr".into()),
                    label: "pr:#142 Multi-Window Vulkan".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.pr".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        ),
        _ => (
            LayoutNode {
                rect: [0.0, 0.0, 1.0, 1.0],
                block_id: Some("block:1".into()),
                split: None,
            },
            vec![BlockView {
                block_id: Some("block:1".into()),
                label: "zsh — omarex".into(),
                creator_name: Some("com.superlogical.terminal".into()),
                flavor: Some("com.superlogical.terminal.shell".into()),
                host_id: Some("host:local".into()),
                owner: Some("client:1".into()),
            }],
            Vec::new(),
        ),
    };

    let mut layers = vec![LayerView {
        layer_id: Some("layer:tiled".into()),
        kind: "tiled".into(),
        bounds: [0.0, 0.0, 1.0, 1.0],
        state: None,
        layout: Some(layout_node),
        blocks,
    }];
    layers.extend(extra_layers);

    let view = SessionView {
        revision: 1,
        session_id: Some("session:work".into()),
        label: "Work".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![
            WindowView {
                window_id: Some("window:1".into()),
                label: "Main Shell".into(),
                focused_block_id: Some("block:1".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers,
            },
            WindowView {
                window_id: Some("window:2".into()),
                label: "Test Runner".into(),
                focused_block_id: None,
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: Vec::new(),
            },
        ],
    };

    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:work".into(),
            view: Box::new(view),
            seq: 1,
        },
        now,
    );

    ctrl.ui.layout_cfg.toolbar_position = ToolbarPosition::Left;
    ctrl.ui.layout_cfg.toolbar_behavior = ToolbarBehavior::Always;
    ctrl.ui.layout_cfg.pane_header_mode = PaneHeaderMode::Always;

    let term1 = vt_engine::Terminal::new(110, 36, vt_engine::TerminalConfig::default())
        .expect("create scenario terminal 1");
    if scenario == HeadlessScenario::CopyMode {
        for idx in 1..=45 {
            let line = format!(
                "\x1b[36m[{idx:02}]\x1b[0m fn process_event(ev: Event) -> Result<Status> {{ match ev {{ Ok => \"ready\" }} }}\r\n"
            );
            term1.write(line.as_bytes());
        }
    } else {
        term1.write(
            b"\x1b[1;34m~/Work/omarex\x1b[0m \x1b[32mmain\x1b[0m \xee\x82\xb0 cargo test --workspace\r\n\
              \x1b[32m   Compiling\x1b[0m gpu-renderer v0.1.0\r\n\
              \x1b[32m   Compiling\x1b[0m ui-chrome v0.1.0\r\n\
              \x1b[32m   Compiling\x1b[0m omarex v0.1.0\r\n\
              \x1b[1;32m    Finished\x1b[0m `test` profile: a -> b, x => y, ptr !== null, mod::item |> run\r\n\
              \x1b[33m$ \x1b[0m",
        );
    }
    let key1 = BlockKey::new(LOCAL_HOST_ID, "session:work", "block:1");
    let _ = ctrl.pane_store.insert_terminal(key1.clone(), term1);

    if matches!(
        scenario,
        HeadlessScenario::SplitPanes | HeadlessScenario::DndSplit | HeadlessScenario::ProgramResume
    ) {
        let term2 = vt_engine::Terminal::new(90, 36, vt_engine::TerminalConfig::default())
            .expect("create scenario terminal 2");
        if scenario == HeadlessScenario::ProgramResume {
            term2.write(
                b"\x1b[1;36m[OSC 7577]\x1b[0m Program Resume status\r\n\
                  \x1b[32m  trusted:\x1b[0m   claude  (auto-resumes on Rex restart)\r\n\
                  \x1b[33m  untrusted:\x1b[0m nvim    (no_rule in init.lua)\r\n\r\n\
                  \x1b[90mRestart Rex from Settings -> Server anytime.\x1b[0m\r\n",
            );
        } else {
            term2.write(
                b"\x1b[1;35m[Split Pane 2]\x1b[0m git status --short\r\n\
                  \x1b[32m M\x1b[0m crates/gpu-renderer/src/lib.rs\r\n\
                  \x1b[32m M\x1b[0m crates/omarex/src/app.rs\r\n\
                  \x1b[32m M\x1b[0m crates/omarex/src/bridge/render.rs\r\n",
            );
        }
        let _ = ctrl.pane_store.insert_terminal(
            BlockKey::new(LOCAL_HOST_ID, "session:work", "block:2"),
            term2,
        );
    }

    if scenario == HeadlessScenario::FloatingPr {
        let term_pr = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
            .expect("create floating pr terminal");
        term_pr.write(
            b"\x1b[1;33mPR #142\x1b[0m True Multi-Window & 5-Zone DnD\r\n\
              \x1b[32m+420\x1b[0m \x1b[31m-118\x1b[0m across 9 files\r\n\
              Checks: \x1b[32mclippy OK\x1b[0m | \x1b[32mfmt OK\x1b[0m | \x1b[32mtests OK\x1b[0m\r\n",
        );
        let _ = ctrl.pane_store.insert_terminal(
            BlockKey::new(LOCAL_HOST_ID, "session:work", "block:pr"),
            term_pr,
        );
        ctrl.ui.focus.focus_terminal(Some("block:pr".to_owned()));
    } else {
        ctrl.ui.focus.focus_terminal(Some("block:1".to_owned()));
    }

    match scenario {
        HeadlessScenario::SidebarDark | HeadlessScenario::SidebarLight => {}
        HeadlessScenario::SplitPanes
        | HeadlessScenario::FloatingPr
        | HeadlessScenario::DndSplit => {}
        HeadlessScenario::CopyMode => {
            ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::EnterCopyMode, now);
            let _ = ctrl.pane_store.copy_mode_select_range(
                &key1,
                ScreenPoint { col: 5, row: 32 },
                ScreenPoint { col: 44, row: 35 },
                SelectionMode::Char,
            );
            if let Some(cm) = ctrl.copy_mode.as_mut() {
                cm.visual = Some((SelectionMode::Char, ScreenPoint { col: 5, row: 32 }));
                cm.cursor = ScreenPoint { col: 44, row: 35 };
            }
            let (sx, sy, sw, sh) = clamped_copy_shine_rect(ctrl.viewport_w, ctrl.viewport_h);
            ctrl.trigger_copy_shine(
                vec![Rect::new(sx, sy, sw, sh)],
                ClipRect::UNBOUNDED,
                now - Duration::from_millis(90),
            );
        }
        HeadlessScenario::SyntaxViewer => {
            if !has_explicit_viewer_paths {
                let rust_sample = "\
use std::sync::Arc;
use gpu_renderer::{GpuShared, SurfaceRenderer};

/// Multi-window shared Vulkan device & atlas state.
pub struct WindowHandle {
    pub shared: Arc<GpuShared>,
    pub surface: SurfaceRenderer,
}

pub fn render_frame(win: &mut WindowHandle, width: u32, height: u32) -> bool {
    let ratio = (width as f32) / (height.max(1) as f32);
    ratio > 0.0 && win.surface.surface_size() != (0, 0)
}
";
                let txt = TextViewerState::new(rust_sample, Some("rust".to_owned()), false);
                ctrl.ui.viewer.open_or_focus_tab(
                    "src/multi_window.rs",
                    None,
                    ViewerContent::Text(txt),
                );
            }
        }
        HeadlessScenario::MarkdownViewer => {
            if !has_explicit_viewer_paths {
                let md_sample = "\
# Agent Companion 2.0

Rich **Markdown** preview with syntax-highlighted code blocks and GFM tables.

| Feature | Crate | Status |
|---|---|---|
| Multi-Window | `gpu-renderer` | Ready |
| 5-Zone DnD | `ui-chrome` | Ready |
| Copy Mode | `vt-engine` | Ready |

```rust
fn connect(host: &str) -> Result<Session, Error> {
    let endpoint = host.parse()?;
    Ok(Session::attach(endpoint))
}
```

> All Vulkan resources share `Arc<GpuShared>` cleanly across OS windows.
";
                let txt = TextViewerState::new(md_sample, Some("markdown".to_owned()), true);
                ctrl.ui.viewer.open_or_focus_tab(
                    "docs/overview.md",
                    None,
                    ViewerContent::Text(txt),
                );
            }
        }
        HeadlessScenario::DiagnosticsHud => {
            for us in [920u32, 1080, 1140, 980, 1290, 1410, 1020, 890, 1190, 1050] {
                ctrl.frame_times_us.push_back(us);
            }
            ctrl.open_modal_kind(ModalKind::Diagnostics);
        }
        HeadlessScenario::PortsPill => {
            if let Some(blk) = ctrl.workspace.block_mut(&key1) {
                blk.apply_ports_response(sample_ports_response());
            }
        }
        HeadlessScenario::ThemeSwitcher => {
            ctrl.open_modal_kind(ModalKind::ThemePicker);
        }
        HeadlessScenario::SettingsModal => {
            ctrl.open_modal_kind(ModalKind::Settings);
        }
        HeadlessScenario::SettingsIosQr => {
            ctrl.open_settings_at(SettingsCategory::ConnectIos);
        }
        HeadlessScenario::SettingsServer => {
            if let Some(blk) = ctrl.workspace.block_mut(&key1) {
                blk.program_resume_records = vec![ProgramResumeRecord {
                    owner: Some(ProcessOwner {
                        pid: 74877,
                        name: "claude".into(),
                    }),
                    program: "claude".into(),
                    cwd: Some("/home/mike/Work/omarex".into()),
                    reported_at: "2026-11-27T13:05:00Z".into(),
                    chosen: true,
                    trusted: true,
                    rule: Some("claude".into()),
                    untrusted_reason: None,
                    command: Vec::new(),
                    reported_command: Vec::new(),
                }];
            }
            ctrl.open_settings_at(SettingsCategory::Server);
        }
        HeadlessScenario::ProgramResume => {
            if let Some(blk1) = ctrl.workspace.block_mut(&key1) {
                blk1.program_resume_records = vec![ProgramResumeRecord {
                    owner: Some(ProcessOwner {
                        pid: 74877,
                        name: "claude".into(),
                    }),
                    program: "claude".into(),
                    cwd: Some("/home/mike/Work/omarex".into()),
                    reported_at: "2026-11-27T13:05:00Z".into(),
                    chosen: true,
                    trusted: true,
                    rule: Some("claude".into()),
                    untrusted_reason: None,
                    command: Vec::new(),
                    reported_command: Vec::new(),
                }];
            }
            let key2 = BlockKey::new(LOCAL_HOST_ID, "session:work", "block:2");
            if let Some(blk2) = ctrl.workspace.block_mut(&key2) {
                blk2.program_resume_records = vec![ProgramResumeRecord {
                    owner: Some(ProcessOwner {
                        pid: 74912,
                        name: "nvim".into(),
                    }),
                    program: "nvim".into(),
                    cwd: Some("/home/mike/Work/omarex".into()),
                    reported_at: "2026-11-27T13:06:00Z".into(),
                    chosen: true,
                    trusted: false,
                    rule: None,
                    untrusted_reason: Some("no_rule".into()),
                    command: Vec::new(),
                    reported_command: Vec::new(),
                }];
            }
        }
        HeadlessScenario::MultiWindow => {}
    }

    let _ = ctrl.pane_store.apply_theme_all(&ctrl.theme.palette);
    ctrl.drain_pending_commands();
}

fn sample_ports_response() -> TerminalPortsResponse {
    TerminalPortsResponse {
        status: PortScanStatus::Ok,
        ports: vec![
            PortEntry {
                port: 3000,
                address: "127.0.0.1".into(),
                protocol: "tcp".into(),
                first_seen_at: "2026-11-27T00:00:00Z".into(),
                pid: Some(4242),
            },
            PortEntry {
                port: 8080,
                address: "127.0.0.1".into(),
                protocol: "tcp".into(),
                first_seen_at: "2026-11-27T00:00:00Z".into(),
                pid: Some(4243),
            },
        ],
        processes: vec![
            PortProcessEntry {
                pid: 4242,
                user_id: 1000,
                name: "vite".into(),
                user: Some("mike".into()),
                path: None,
                invoked_path: None,
                argv0: None,
                script: None,
                cwd: None,
            },
            PortProcessEntry {
                pid: 4243,
                user_id: 1000,
                name: "api-server".into(),
                user: Some("mike".into()),
                path: None,
                invoked_path: None,
                argv0: None,
                script: None,
                cwd: None,
            },
        ],
        truncated: Some(false),
        id: None,
    }
}

pub(super) fn activate_dnd_split_overlay(ctrl: &mut OmarexController) {
    let Some(out) = ctrl.last_ui_output.as_ref() else {
        return;
    };
    let Some(target) = out.tiled_panes.get(1).or_else(|| out.tiled_panes.first()) else {
        return;
    };
    let px = target.card_rect.right() - (target.card_rect.w * 0.10).max(8.0);
    let py = target.card_rect.y + target.card_rect.h * 0.5;
    let Some(hit) = hit_test_drop_zone(target.card_rect, px, py) else {
        return;
    };
    let key = ctrl.focused_block_key().or_else(|| {
        let sk = ctrl.workspace.active_session.as_ref()?;
        let first_id = out.tiled_panes.first().map(|p| p.id.clone())?;
        Some(BlockKey::new(
            sk.host_id.clone(),
            sk.session_id.clone(),
            first_id,
        ))
    });
    let Some(key) = key else {
        return;
    };
    ctrl.active_drag = Some(ActiveDrag::PaneMove {
        host_id: key.host_id,
        session_id: key.session_id,
        block_id: key.block_id,
        start_px: (100.0, 100.0),
        activated: true,
        hover_target: Some((target.id.clone(), hit)),
    });
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

fn rects_match(a: RectF, b: RectF) -> bool {
    (a.x - b.x).abs() < 0.5
        && (a.y - b.y).abs() < 0.5
        && (a.w - b.w).abs() < 0.5
        && (a.h - b.h).abs() < 0.5
}
