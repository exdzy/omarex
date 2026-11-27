//! Zero-allocation `RowId`-cached terminal grid builder implementing the dirty-row architecture
//! from `docs/architecture.md` (Section 6.2).
//!
//! Clean rows reuse pre-shaped GPU quad & glyph instances with zero shaping, zero atlas lookups,
//! and zero per-cell `String` allocations — even when scrolled to a new Y offset or shifted by
//! fractional overscan smooth scrolling.

use std::collections::HashMap;
use std::ops::Range;

use crate::atlas::{CellMetrics, GlyphAtlas, ShapedRunCell};
use crate::draw_list::{
    ClipRect, Color, CornerRadii, DrawList, GlyphInstance, QuadInstance, Rect, layers,
};

/// Stable row identity matching `GHOSTTY_RENDER_STATE_ROW_DATA_ID`.
pub type RowId = u64;

/// Compact bitflags describing visual terminal cell attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct CellFlags(u16);

impl CellFlags {
    pub const NONE: Self = Self(0);
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const FAINT: Self = Self(1 << 2);
    pub const UNDERLINE: Self = Self(1 << 3);
    pub const DOUBLE_UNDERLINE: Self = Self(1 << 4);
    pub const CURLY_UNDERLINE: Self = Self(1 << 5);
    pub const DOTTED_UNDERLINE: Self = Self(1 << 6);
    pub const DASHED_UNDERLINE: Self = Self(1 << 7);
    pub const STRIKETHROUGH: Self = Self(1 << 8);
    pub const INVERSE: Self = Self(1 << 9);
    pub const INVISIBLE: Self = Self(1 << 10);
    pub const WIDE_TAIL: Self = Self(1 << 11);
    pub const HYPERLINK: Self = Self(1 << 12);
    pub const SELECTED: Self = Self(1 << 13);

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[inline]
    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) == flag.0
    }

    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

impl std::ops::BitOr for CellFlags {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for CellFlags {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Borrowed styled terminal cell passed to [`TerminalPaneCache`] with zero per-cell heap strings.
#[derive(Debug, Clone, Copy)]
pub struct StyledCell<'a> {
    /// Borrowed UTF-8 slice for this cell's grapheme cluster (`""` or `" "` for blank cells).
    pub text: &'a str,
    /// 0-based column index.
    pub col: u16,
    /// Cell span width (`1` normal, `2` wide CJK/emoji).
    pub width: u8,
    /// Foreground color (before `INVERSE` swap).
    pub fg: Color,
    /// Optional explicit cell background (`None` = pane default background).
    pub bg: Option<Color>,
    /// Optional explicit underline color (defaults to effective foreground).
    pub underline_color: Option<Color>,
    /// Attribute flags.
    pub flags: CellFlags,
}

/// Terminal cursor visual shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
    Hollow,
}

/// Terminal cursor placement and visual styling.
#[derive(Debug, Clone, Copy)]
pub struct TerminalCursor {
    pub col: u16,
    /// Visual row relative to viewport top (can be outside `0..rows` during scroll).
    pub row: i32,
    pub shape: CursorShape,
    pub color: Color,
    /// Optional foreground contrast color for characters under a solid `Block` cursor.
    pub text_color: Color,
    pub visible: bool,
}

/// Viewport geometry, cell metrics, smooth scroll overscan offset, and theme colors for one pane.
#[derive(Debug, Clone, Copy)]
pub struct TerminalViewport {
    /// Inner terminal grid rect in logical pixels.
    pub rect: Rect,
    /// Pane scissor clip rect in logical pixels (clips overscan smooth-scroll rows cleanly).
    pub clip_rect: ClipRect,
    pub cols: u16,
    pub rows: u16,
    pub metrics: CellMetrics,
    /// Fractional Y pixel offset in logical pixels for overscan smooth scrolling
    /// (typically in `(-cell_height, +cell_height)`).
    pub scroll_offset_y: f32,
    pub default_bg: Color,
    pub default_fg: Color,
    pub selection_bg: Color,
    pub selection_fg: Option<Color>,
    pub hyperlink_color: Color,
    /// Inactive pane fade multiplier (`1.0` active, `0.78` dimmed inactive pane).
    pub dim_alpha: f32,
}

/// Zero-allocation terminal row data source implemented by `vt-engine` adapters or snapshots.
pub trait TerminalRowSource {
    /// Total rows to render (including optional overscan rows above/below the viewport).
    fn row_count(&self) -> usize;
    /// Returns `(row_id, visual_row_index, is_dirty)`.
    ///
    /// `visual_row_index` is `0..rows` for viewport rows, `-1` for overscan above,
    /// and `rows` for overscan below.
    fn row_meta(&self, idx: usize) -> (RowId, i32, bool);
    /// Visits styled cells of row `idx` without heap-allocating `String`s.
    fn visit_row_cells(&self, idx: usize, visit: &mut dyn FnMut(StyledCell<'_>));
    /// Optional cursor to draw over the terminal grid.
    fn cursor(&self) -> Option<TerminalCursor>;
}

/// Telemetry returned by [`TerminalPaneCache::build_pane`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalBuildStats {
    pub total_rows: usize,
    pub rebuilt_rows: usize,
    pub cached_rows: usize,
    pub bg_quads: usize,
    pub fg_quads: usize,
    pub glyphs: usize,
}

#[derive(Debug, Clone)]
struct CachedRow {
    /// Row-local (`y = 0` at row top) background & selection quads in physical pixels.
    bg_quads: Vec<QuadInstance>,
    /// Row-local (`y = 0` at row top) underline & strikethrough quads in physical pixels.
    decor_quads: Vec<QuadInstance>,
    /// Row-local (`y = 0` at row top) atlas glyph instances in physical pixels.
    glyphs: Vec<GlyphInstance>,
    last_used_frame: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnderlineStyle {
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Debug, Clone, Copy)]
struct ActiveBgSpan {
    start_col: u16,
    end_col: u16,
    color: Color,
    is_selection: bool,
}

#[derive(Debug, Clone, Copy)]
struct ActiveUnderlineSpan {
    start_col: u16,
    end_col: u16,
    color: Color,
    style: UnderlineStyle,
}

#[derive(Debug, Clone, Copy)]
struct ActiveStrikeSpan {
    start_col: u16,
    end_col: u16,
    color: Color,
}

/// Reusable per-row shaping scratch buffers owned by [`TerminalPaneCache`] (0 steady-state allocs).
#[derive(Default)]
struct RowScratch {
    run_text: String,
    run_cells: Vec<ShapedRunCell>,
}

/// Per-pane incremental terminal instance cache keyed by stable `RowId`.
pub struct TerminalPaneCache {
    rows: HashMap<[u64; 2], CachedRow>,
    row_scratch: RowScratch,
    frame_counter: u64,
    last_atlas_generation: u64,
    last_scale_bits: u32,
    last_font_size_bits: u32,
    last_cell_w_bits: u32,
    last_cell_h_bits: u32,
    last_cols: u16,
    last_rows: u16,
    last_default_bg: Color,
    last_default_fg: Color,
    last_selection_bg: Color,
    last_selection_fg: Option<Color>,
    last_hyperlink_color: Color,
    max_cached_rows: usize,
}

impl Default for TerminalPaneCache {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalPaneCache {
    pub fn new() -> Self {
        Self {
            rows: HashMap::with_capacity(96),
            row_scratch: RowScratch {
                run_text: String::with_capacity(128),
                run_cells: Vec::with_capacity(64),
            },
            frame_counter: 0,
            last_atlas_generation: 0,
            last_scale_bits: 0,
            last_font_size_bits: 0,
            last_cell_w_bits: 0,
            last_cell_h_bits: 0,
            last_cols: 0,
            last_rows: 0,
            last_default_bg: Color::TRANSPARENT,
            last_default_fg: Color::TRANSPARENT,
            last_selection_bg: Color::TRANSPARENT,
            last_selection_fg: None,
            last_hyperlink_color: Color::TRANSPARENT,
            max_cached_rows: 512,
        }
    }

    pub fn cached_row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    /// Incrementally builds terminal GPU instances for `source` into `draw_list`,
    /// reusing cached rows for every clean `RowId`.
    pub fn build_pane(
        &mut self,
        source: &impl TerminalRowSource,
        viewport: &TerminalViewport,
        atlas: &mut GlyphAtlas,
        queue: &wgpu::Queue,
        draw_list: &mut DrawList,
    ) -> TerminalBuildStats {
        self.frame_counter = self.frame_counter.saturating_add(1);
        let scale = viewport.metrics.scale_factor.max(0.5);
        self.invalidate_if_metrics_changed(atlas.generation(), viewport, scale);

        let cell_w_px = (viewport.metrics.cell_width * scale).round().max(1.0);
        let cell_h_px = (viewport.metrics.cell_height * scale).round().max(1.0);
        let baseline_px = (viewport.metrics.baseline_y * scale).round();
        let underline_y_px = (viewport.metrics.underline_y * scale).round();
        let underline_thick_px = (viewport.metrics.underline_thickness * scale)
            .round()
            .max(1.0);
        let strike_y_px = (viewport.metrics.strikethrough_y * scale).round();
        let font_size_px = viewport.metrics.font_size * scale;

        let origin_x_px = (viewport.rect.x * scale).round();
        let origin_y_px = (viewport.rect.y * scale).round();
        let scroll_px = viewport.scroll_offset_y * scale;
        let clip_px = viewport
            .clip_rect
            .intersect(draw_list.current_clip())
            .scale(scale)
            .to_array();
        let dim = viewport.dim_alpha.clamp(0.0, 1.0);

        let row_count = source.row_count();
        let mut stats = TerminalBuildStats {
            total_rows: row_count,
            ..Default::default()
        };

        for idx in 0..row_count {
            let (row_id, visual_row, is_dirty) = source.row_meta(idx);
            let row_key = [row_id, 0];

            let needs_rebuild = is_dirty || !self.rows.contains_key(&row_key);
            if needs_rebuild {
                let mut row_cache = self.rows.remove(&row_key).unwrap_or(CachedRow {
                    bg_quads: Vec::with_capacity(8),
                    decor_quads: Vec::with_capacity(4),
                    glyphs: Vec::with_capacity(viewport.cols as usize),
                    last_used_frame: self.frame_counter,
                });
                row_cache.bg_quads.clear();
                row_cache.decor_quads.clear();
                row_cache.glyphs.clear();
                row_cache.last_used_frame = self.frame_counter;

                let mut row_builder = RowSpanBuilder::new(
                    cell_w_px,
                    cell_h_px,
                    baseline_px,
                    underline_y_px,
                    underline_thick_px,
                    strike_y_px,
                    font_size_px,
                    scale,
                    viewport,
                    &mut self.row_scratch,
                );
                source.visit_row_cells(idx, &mut |cell| {
                    row_builder.push_cell(cell, atlas, queue, &mut row_cache);
                });
                row_builder.finish(atlas, queue, &mut row_cache);

                self.rows.insert(row_key, row_cache);
                stats.rebuilt_rows += 1;
            } else {
                stats.cached_rows += 1;
            }

            // Emit translated cached row into the unified DrawList.
            if let Some(row_cache) = self.rows.get_mut(&row_key) {
                row_cache.last_used_frame = self.frame_counter;
                let row_y_px = origin_y_px + (visual_row as f32) * cell_h_px + scroll_px;
                emit_cached_row(
                    row_cache,
                    origin_x_px,
                    row_y_px,
                    clip_px,
                    dim,
                    draw_list,
                    &mut stats,
                );
            }
        }

        // Emit cursor quad cleanly on top (mutating cursor blink costs 1 quad, zero row rebuilds!).
        if let Some(cursor) = source.cursor()
            && cursor.visible
        {
            emit_cursor(
                cursor,
                origin_x_px,
                origin_y_px,
                scroll_px,
                cell_w_px,
                cell_h_px,
                scale,
                clip_px,
                dim,
                draw_list,
                &mut stats,
            );
        }

        self.prune_stale_rows();
        stats
    }

    /// Zero-allocation direct adapter from `vt_engine::RenderState` (after `begin_update` +
    /// `end_update`) into [`DrawList`], preserving full 128-bit `libghostty-vt` `RowId` caching.
    pub fn build_from_vt_render_state(
        &mut self,
        state: &mut vt_engine::RenderState,
        viewport: &TerminalViewport,
        atlas: &mut GlyphAtlas,
        queue: &wgpu::Queue,
        draw_list: &mut DrawList,
    ) -> Result<TerminalBuildStats, vt_engine::VtError> {
        self.build_from_vt_render_state_with_cursor_override(
            state, viewport, atlas, queue, draw_list, None,
        )
    }

    /// Same as [`Self::build_from_vt_render_state`], but allows suppressing or overriding the live
    /// PTY cursor (e.g. for Vim Copy Mode).
    pub fn build_from_vt_render_state_with_cursor_override(
        &mut self,
        state: &mut vt_engine::RenderState,
        viewport: &TerminalViewport,
        atlas: &mut GlyphAtlas,
        queue: &wgpu::Queue,
        draw_list: &mut DrawList,
        cursor_override: Option<TerminalCursor>,
    ) -> Result<TerminalBuildStats, vt_engine::VtError> {
        self.frame_counter = self.frame_counter.saturating_add(1);
        let scale = viewport.metrics.scale_factor.max(0.5);
        self.invalidate_if_metrics_changed(atlas.generation(), viewport, scale);

        let cell_w_px = (viewport.metrics.cell_width * scale).round().max(1.0);
        let cell_h_px = (viewport.metrics.cell_height * scale).round().max(1.0);
        let baseline_px = (viewport.metrics.baseline_y * scale).round();
        let underline_y_px = (viewport.metrics.underline_y * scale).round();
        let underline_thick_px = (viewport.metrics.underline_thickness * scale)
            .round()
            .max(1.0);
        let strike_y_px = (viewport.metrics.strikethrough_y * scale).round();
        let font_size_px = viewport.metrics.font_size * scale;

        let origin_x_px = (viewport.rect.x * scale).round();
        let origin_y_px = (viewport.rect.y * scale).round();
        let scroll_px = viewport.scroll_offset_y * scale;
        let clip_px = viewport
            .clip_rect
            .intersect(draw_list.current_clip())
            .scale(scale)
            .to_array();
        let dim = viewport.dim_alpha.clamp(0.0, 1.0);
        let vt_cursor = state.cursor();
        let cursor_color = state
            .colors()
            .cursor
            .map(Color::from)
            .unwrap_or(viewport.default_fg);

        let mut stats = TerminalBuildStats::default();
        let frame_counter = self.frame_counter;
        let rows_cache = &mut self.rows;
        let row_scratch = &mut self.row_scratch;

        state.for_each_row(|dirty_row| {
            stats.total_rows += 1;
            let row_key = dirty_row.row_id.0;
            let needs_rebuild = dirty_row.dirty || !rows_cache.contains_key(&row_key);

            if needs_rebuild {
                let mut row_cache = rows_cache.remove(&row_key).unwrap_or(CachedRow {
                    bg_quads: Vec::with_capacity(8),
                    decor_quads: Vec::with_capacity(4),
                    glyphs: Vec::with_capacity(dirty_row.cells.len()),
                    last_used_frame: frame_counter,
                });
                row_cache.bg_quads.clear();
                row_cache.decor_quads.clear();
                row_cache.glyphs.clear();
                row_cache.last_used_frame = frame_counter;

                let mut row_builder = RowSpanBuilder::new(
                    cell_w_px,
                    cell_h_px,
                    baseline_px,
                    underline_y_px,
                    underline_thick_px,
                    strike_y_px,
                    font_size_px,
                    scale,
                    viewport,
                    row_scratch,
                );

                let mut utf8_buf = [0u8; 4];
                for (col_idx, cell) in dirty_row.cells.iter().enumerate() {
                    let col = col_idx as u16;
                    let width = match cell.wide {
                        vt_engine::CellWide::Wide => 2,
                        _ => 1,
                    };
                    let mut flags = convert_vt_flags(cell.flags, cell.underline);
                    if cell.wide == vt_engine::CellWide::SpacerTail {
                        flags |= CellFlags::WIDE_TAIL;
                    }
                    if let Some(sel) = dirty_row.selection
                        && col >= sel.start_x
                        && col <= sel.end_x
                    {
                        flags |= CellFlags::SELECTED;
                    }
                    let text: &str = match cell.content {
                        vt_engine::CellContent::Empty | vt_engine::CellContent::BgOnly => "",
                        vt_engine::CellContent::Codepoint(ch) => ch.encode_utf8(&mut utf8_buf),
                        vt_engine::CellContent::Grapheme { .. } => {
                            dirty_row.cell_grapheme(cell).unwrap_or("")
                        }
                    };

                    row_builder.push_cell(
                        StyledCell {
                            text,
                            col,
                            width,
                            fg: Color::from(cell.fg),
                            bg: cell.bg.map(Color::from),
                            underline_color: cell.underline_color.map(Color::from),
                            flags,
                        },
                        atlas,
                        queue,
                        &mut row_cache,
                    );
                }
                row_builder.finish(atlas, queue, &mut row_cache);

                rows_cache.insert(row_key, row_cache);
                stats.rebuilt_rows += 1;
            } else {
                stats.cached_rows += 1;
            }

            if let Some(row_cache) = rows_cache.get_mut(&row_key) {
                row_cache.last_used_frame = frame_counter;
                let row_y_px = origin_y_px + (dirty_row.viewport_y as f32) * cell_h_px + scroll_px;
                emit_cached_row(
                    row_cache,
                    origin_x_px,
                    row_y_px,
                    clip_px,
                    dim,
                    draw_list,
                    &mut stats,
                );
            }
        })?;

        if let Some(override_cursor) = cursor_override {
            if override_cursor.visible {
                emit_cursor(
                    override_cursor,
                    origin_x_px,
                    origin_y_px,
                    scroll_px,
                    cell_w_px,
                    cell_h_px,
                    scale,
                    clip_px,
                    dim,
                    draw_list,
                    &mut stats,
                );
            }
        } else if vt_cursor.visible
            && let Some((col, row)) = vt_cursor.viewport_pos
        {
            let shape = match vt_cursor.style {
                vt_engine::CursorStyle::Bar => CursorShape::Bar,
                vt_engine::CursorStyle::Block => CursorShape::Block,
                vt_engine::CursorStyle::Underline => CursorShape::Underline,
                vt_engine::CursorStyle::BlockHollow => CursorShape::Hollow,
            };
            emit_cursor(
                TerminalCursor {
                    col,
                    row: i32::from(row),
                    shape,
                    color: cursor_color,
                    text_color: viewport.default_bg,
                    visible: true,
                },
                origin_x_px,
                origin_y_px,
                scroll_px,
                cell_w_px,
                cell_h_px,
                scale,
                clip_px,
                dim,
                draw_list,
                &mut stats,
            );
        }

        self.prune_stale_rows();
        Ok(stats)
    }

    fn invalidate_if_metrics_changed(
        &mut self,
        atlas_generation: u64,
        viewport: &TerminalViewport,
        scale: f32,
    ) {
        let scale_bits = scale.to_bits();
        let font_bits = viewport.metrics.font_size.to_bits();
        let cw_bits = viewport.metrics.cell_width.to_bits();
        let ch_bits = viewport.metrics.cell_height.to_bits();

        if self.last_atlas_generation != atlas_generation
            || self.last_scale_bits != scale_bits
            || self.last_font_size_bits != font_bits
            || self.last_cell_w_bits != cw_bits
            || self.last_cell_h_bits != ch_bits
            || self.last_cols != viewport.cols
            || self.last_rows != viewport.rows
            || self.last_default_bg != viewport.default_bg
            || self.last_default_fg != viewport.default_fg
            || self.last_selection_bg != viewport.selection_bg
            || self.last_selection_fg != viewport.selection_fg
            || self.last_hyperlink_color != viewport.hyperlink_color
        {
            self.rows.clear();
            self.last_atlas_generation = atlas_generation;
            self.last_scale_bits = scale_bits;
            self.last_font_size_bits = font_bits;
            self.last_cell_w_bits = cw_bits;
            self.last_cell_h_bits = ch_bits;
            self.last_cols = viewport.cols;
            self.last_rows = viewport.rows;
            self.last_default_bg = viewport.default_bg;
            self.last_default_fg = viewport.default_fg;
            self.last_selection_bg = viewport.selection_bg;
            self.last_selection_fg = viewport.selection_fg;
            self.last_hyperlink_color = viewport.hyperlink_color;
        }
    }

    fn prune_stale_rows(&mut self) {
        if self.rows.len() <= self.max_cached_rows {
            return;
        }
        let cutoff = self.frame_counter.saturating_sub(120);
        self.rows.retain(|_, row| row.last_used_frame >= cutoff);
    }
}

fn resolve_underline_style(flags: CellFlags) -> Option<UnderlineStyle> {
    if flags.contains(CellFlags::CURLY_UNDERLINE) {
        Some(UnderlineStyle::Curly)
    } else if flags.contains(CellFlags::DOUBLE_UNDERLINE) {
        Some(UnderlineStyle::Double)
    } else if flags.contains(CellFlags::DOTTED_UNDERLINE) {
        Some(UnderlineStyle::Dotted)
    } else if flags.contains(CellFlags::DASHED_UNDERLINE) {
        Some(UnderlineStyle::Dashed)
    } else if flags.contains(CellFlags::UNDERLINE) || flags.contains(CellFlags::HYPERLINK) {
        Some(UnderlineStyle::Single)
    } else {
        None
    }
}

fn convert_vt_flags(vt: vt_engine::CellFlags, underline: vt_engine::UnderlineStyle) -> CellFlags {
    let mut out = CellFlags::NONE;
    if vt.contains(vt_engine::CellFlags::BOLD) {
        out |= CellFlags::BOLD;
    }
    if vt.contains(vt_engine::CellFlags::ITALIC) {
        out |= CellFlags::ITALIC;
    }
    if vt.contains(vt_engine::CellFlags::FAINT) {
        out |= CellFlags::FAINT;
    }
    // NOTE: Do NOT forward `vt_engine::CellFlags::INVERSE` into `gpu_renderer::CellFlags::INVERSE`.
    // `vt_engine` already resolves SGR 7 (inverse video) by swapping `cell.fg` and `cell.bg`
    // while building `RenderState` (keeping the flag set for metadata), whereas
    // `RowSpanBuilder::push_cell` swaps `fg`/`bg` again whenever `gpu_renderer::CellFlags::INVERSE`
    // is set (for callers using `TerminalGridSnapshot` directly). Forwarding `INVERSE` here
    // double-swapped fg/bg and cancelled out all SGR 7 reverse-video highlights.
    if vt.contains(vt_engine::CellFlags::INVISIBLE) {
        out |= CellFlags::INVISIBLE;
    }
    if vt.contains(vt_engine::CellFlags::STRIKETHROUGH) {
        out |= CellFlags::STRIKETHROUGH;
    }
    if vt.contains(vt_engine::CellFlags::HYPERLINK) {
        out |= CellFlags::HYPERLINK;
    }
    if vt.contains(vt_engine::CellFlags::SELECTED) {
        out |= CellFlags::SELECTED;
    }
    match underline {
        vt_engine::UnderlineStyle::None => {}
        vt_engine::UnderlineStyle::Single => out |= CellFlags::UNDERLINE,
        vt_engine::UnderlineStyle::Double => out |= CellFlags::DOUBLE_UNDERLINE,
        vt_engine::UnderlineStyle::Curly => out |= CellFlags::CURLY_UNDERLINE,
        vt_engine::UnderlineStyle::Dotted => out |= CellFlags::DOTTED_UNDERLINE,
        vt_engine::UnderlineStyle::Dashed => out |= CellFlags::DASHED_UNDERLINE,
    }
    out
}

#[inline]
fn is_ligature_candidate_ascii(ch: char) -> bool {
    matches!(
        ch,
        '!' | '#'
            | '$'
            | '%'
            | '&'
            | '*'
            | '+'
            | '-'
            | '.'
            | '/'
            | ':'
            | ';'
            | '<'
            | '='
            | '>'
            | '?'
            | '@'
            | '\\'
            | '^'
            | '_'
            | '|'
            | '~'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
    )
}

#[derive(Clone, Copy, PartialEq)]
struct ActiveTextRunStyle {
    fg: Color,
    bold: bool,
    italic: bool,
    selected: bool,
    is_ascii_punct: bool,
    last_end_col: u16,
}

struct RowSpanBuilder<'v> {
    cell_w_px: f32,
    cell_h_px: f32,
    baseline_px: f32,
    underline_y_px: f32,
    underline_thick_px: f32,
    strike_y_px: f32,
    font_size_px: f32,
    scale: f32,
    viewport: &'v TerminalViewport,
    bg_span: Option<ActiveBgSpan>,
    ul_span: Option<ActiveUnderlineSpan>,
    strike_span: Option<ActiveStrikeSpan>,
    text_run: Option<ActiveTextRunStyle>,
    scratch: &'v mut RowScratch,
}

impl<'v> RowSpanBuilder<'v> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        cell_w_px: f32,
        cell_h_px: f32,
        baseline_px: f32,
        underline_y_px: f32,
        underline_thick_px: f32,
        strike_y_px: f32,
        font_size_px: f32,
        scale: f32,
        viewport: &'v TerminalViewport,
        scratch: &'v mut RowScratch,
    ) -> Self {
        scratch.run_text.clear();
        scratch.run_cells.clear();
        Self {
            cell_w_px,
            cell_h_px,
            baseline_px,
            underline_y_px,
            underline_thick_px,
            strike_y_px,
            font_size_px,
            scale,
            viewport,
            bg_span: None,
            ul_span: None,
            strike_span: None,
            text_run: None,
            scratch,
        }
    }

    fn push_cell(
        &mut self,
        cell: StyledCell<'_>,
        atlas: &mut GlyphAtlas,
        queue: &wgpu::Queue,
        row_cache: &mut CachedRow,
    ) {
        if cell.flags.contains(CellFlags::WIDE_TAIL) {
            return;
        }
        let span_cols = u16::from(cell.width.max(1));
        let col = cell.col;
        let end_col = col.saturating_add(span_cols);

        // Resolve effective foreground & background (INVERSE + SELECTED + FAINT).
        let mut fg = cell.fg;
        let mut bg = cell.bg;
        if cell.flags.contains(CellFlags::INVERSE) {
            let resolved_bg = bg.unwrap_or(self.viewport.default_bg);
            bg = Some(fg);
            fg = resolved_bg;
        }
        let is_selected = cell.flags.contains(CellFlags::SELECTED);
        if is_selected {
            bg = Some(self.viewport.selection_bg);
            if let Some(sel_fg) = self.viewport.selection_fg {
                fg = sel_fg;
            }
        }
        if cell.flags.contains(CellFlags::FAINT) {
            fg = fg.mul_alpha(0.60);
        }

        // Coalesce adjacent background cells of the same color.
        if let Some(bg_col) = bg {
            match self.bg_span {
                Some(ref mut s)
                    if s.end_col == col && s.color == bg_col && s.is_selection == is_selected =>
                {
                    s.end_col = end_col;
                }
                _ => {
                    flush_bg_span(
                        &mut self.bg_span,
                        self.cell_w_px,
                        self.cell_h_px,
                        self.scale,
                        &mut row_cache.bg_quads,
                    );
                    self.bg_span = Some(ActiveBgSpan {
                        start_col: col,
                        end_col,
                        color: bg_col,
                        is_selection: is_selected,
                    });
                }
            }
        } else {
            flush_bg_span(
                &mut self.bg_span,
                self.cell_w_px,
                self.cell_h_px,
                self.scale,
                &mut row_cache.bg_quads,
            );
        }

        // Coalesce underlines (explicit SGR underline or OSC 8 hyperlink).
        if let Some(style) = resolve_underline_style(cell.flags) {
            let ul_color = cell.underline_color.unwrap_or_else(|| {
                if cell.flags.contains(CellFlags::HYPERLINK)
                    && !cell.flags.contains(CellFlags::UNDERLINE)
                    && !cell.flags.contains(CellFlags::CURLY_UNDERLINE)
                {
                    self.viewport.hyperlink_color
                } else {
                    fg
                }
            });
            match self.ul_span {
                Some(ref mut s) if s.end_col == col && s.color == ul_color && s.style == style => {
                    s.end_col = end_col;
                }
                _ => {
                    flush_underline_span(
                        &mut self.ul_span,
                        self.cell_w_px,
                        self.underline_y_px,
                        self.underline_thick_px,
                        self.scale,
                        &mut row_cache.decor_quads,
                    );
                    self.ul_span = Some(ActiveUnderlineSpan {
                        start_col: col,
                        end_col,
                        color: ul_color,
                        style,
                    });
                }
            }
        } else {
            flush_underline_span(
                &mut self.ul_span,
                self.cell_w_px,
                self.underline_y_px,
                self.underline_thick_px,
                self.scale,
                &mut row_cache.decor_quads,
            );
        }

        // Coalesce strikethrough spans.
        if cell.flags.contains(CellFlags::STRIKETHROUGH) {
            match self.strike_span {
                Some(ref mut s) if s.end_col == col && s.color == fg => {
                    s.end_col = end_col;
                }
                _ => {
                    flush_strike_span(
                        &mut self.strike_span,
                        self.cell_w_px,
                        self.strike_y_px,
                        self.underline_thick_px,
                        &mut row_cache.decor_quads,
                    );
                    self.strike_span = Some(ActiveStrikeSpan {
                        start_col: col,
                        end_col,
                        color: fg,
                    });
                }
            }
        } else {
            flush_strike_span(
                &mut self.strike_span,
                self.cell_w_px,
                self.strike_y_px,
                self.underline_thick_px,
                &mut row_cache.decor_quads,
            );
        }

        // Segment row text into:
        // 1) Pure non-ligature ASCII runs -> `emit_terminal_ascii_glyph` fastpath
        // 2) Procedural box/block/Powerline single chars -> `emit_procedural_terminal_glyph`
        // 3) Contiguous ASCII operator/ligature runs & non-ASCII/complex runs -> `emit_terminal_shaped_run`
        if cell.flags.contains(CellFlags::INVISIBLE) || cell.text.is_empty() || cell.text == " " {
            self.flush_text_run(atlas, queue, row_cache);
            return;
        }

        let bold = cell.flags.contains(CellFlags::BOLD);
        let italic = cell.flags.contains(CellFlags::ITALIC);
        let mut chars = cell.text.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            if crate::atlas::is_procedural_box_or_block(ch) {
                self.flush_text_run(atlas, queue, row_cache);
                atlas.emit_procedural_terminal_glyph(
                    ch,
                    f32::from(col) * self.cell_w_px,
                    self.cell_w_px * f32::from(span_cols),
                    self.cell_h_px,
                    fg,
                    queue,
                    &mut row_cache.glyphs,
                );
                return;
            }
            if ch.is_ascii() {
                if !is_ligature_candidate_ascii(ch) {
                    self.flush_text_run(atlas, queue, row_cache);
                    atlas.emit_terminal_ascii_glyph(
                        ch,
                        f32::from(col) * self.cell_w_px,
                        self.baseline_px,
                        self.font_size_px,
                        bold,
                        italic,
                        fg,
                        queue,
                        &mut row_cache.glyphs,
                    );
                    return;
                }
                let can_extend = matches!(
                    self.text_run,
                    Some(s)
                        if s.is_ascii_punct
                            && s.last_end_col == col
                            && s.fg == fg
                            && s.bold == bold
                            && s.italic == italic
                            && s.selected == is_selected
                );
                if can_extend {
                    if let Some(ref mut s) = self.text_run {
                        s.last_end_col = end_col;
                    }
                } else {
                    self.flush_text_run(atlas, queue, row_cache);
                    self.text_run = Some(ActiveTextRunStyle {
                        fg,
                        bold,
                        italic,
                        selected: is_selected,
                        is_ascii_punct: true,
                        last_end_col: end_col,
                    });
                }
                self.scratch.run_cells.push(ShapedRunCell {
                    byte_start: self.scratch.run_text.len() as u32,
                    col,
                    span_cols: span_cols.min(u16::from(u8::MAX)) as u8,
                });
                self.scratch.run_text.push(ch);
                return;
            }
        }

        // Non-ASCII or multi-codepoint grapheme cluster run.
        let can_extend = matches!(
            self.text_run,
            Some(s)
                if !s.is_ascii_punct
                    && s.last_end_col == col
                    && s.fg == fg
                    && s.bold == bold
                    && s.italic == italic
                    && s.selected == is_selected
        );
        if can_extend {
            if let Some(ref mut s) = self.text_run {
                s.last_end_col = end_col;
            }
        } else {
            self.flush_text_run(atlas, queue, row_cache);
            self.text_run = Some(ActiveTextRunStyle {
                fg,
                bold,
                italic,
                selected: is_selected,
                is_ascii_punct: false,
                last_end_col: end_col,
            });
        }
        self.scratch.run_cells.push(ShapedRunCell {
            byte_start: self.scratch.run_text.len() as u32,
            col,
            span_cols: span_cols.min(u16::from(u8::MAX)) as u8,
        });
        self.scratch.run_text.push_str(cell.text);
    }

    fn flush_text_run(
        &mut self,
        atlas: &mut GlyphAtlas,
        queue: &wgpu::Queue,
        row_cache: &mut CachedRow,
    ) {
        let Some(style) = self.text_run.take() else {
            return;
        };
        if self.scratch.run_cells.is_empty() {
            self.scratch.run_text.clear();
            return;
        }
        if style.is_ascii_punct
            && self.scratch.run_cells.len() == 1
            && self.scratch.run_text.len() == 1
        {
            let ch = self.scratch.run_text.as_bytes()[0] as char;
            let col = self.scratch.run_cells[0].col;
            atlas.emit_terminal_ascii_glyph(
                ch,
                f32::from(col) * self.cell_w_px,
                self.baseline_px,
                self.font_size_px,
                style.bold,
                style.italic,
                style.fg,
                queue,
                &mut row_cache.glyphs,
            );
        } else {
            atlas.emit_terminal_shaped_run(
                &self.scratch.run_text,
                &self.scratch.run_cells,
                self.cell_w_px,
                self.cell_h_px,
                self.baseline_px,
                self.font_size_px,
                style.bold,
                style.italic,
                style.fg,
                queue,
                &mut row_cache.glyphs,
            );
        }
        self.scratch.run_text.clear();
        self.scratch.run_cells.clear();
    }

    fn finish(mut self, atlas: &mut GlyphAtlas, queue: &wgpu::Queue, row_cache: &mut CachedRow) {
        self.flush_text_run(atlas, queue, row_cache);
        flush_bg_span(
            &mut self.bg_span,
            self.cell_w_px,
            self.cell_h_px,
            self.scale,
            &mut row_cache.bg_quads,
        );
        flush_underline_span(
            &mut self.ul_span,
            self.cell_w_px,
            self.underline_y_px,
            self.underline_thick_px,
            self.scale,
            &mut row_cache.decor_quads,
        );
        flush_strike_span(
            &mut self.strike_span,
            self.cell_w_px,
            self.strike_y_px,
            self.underline_thick_px,
            &mut row_cache.decor_quads,
        );
    }
}

fn emit_cached_row(
    row_cache: &CachedRow,
    origin_x_px: f32,
    row_y_px: f32,
    clip_px: [f32; 4],
    dim: f32,
    draw_list: &mut DrawList,
    stats: &mut TerminalBuildStats,
) {
    for &q in &row_cache.bg_quads {
        let mut translated = q;
        translated.bounds[0] += origin_x_px;
        translated.bounds[1] += row_y_px;
        translated.clip_rect = clip_px;
        draw_list.push_raw_quad(layers::TERMINAL_BG, translated);
    }
    stats.bg_quads += row_cache.bg_quads.len();

    for &q in &row_cache.decor_quads {
        let mut translated = q;
        translated.bounds[0] += origin_x_px;
        translated.bounds[1] += row_y_px;
        translated.clip_rect = clip_px;
        translated.fill_color[3] *= dim;
        draw_list.push_raw_quad(layers::TERMINAL_FG, translated);
    }
    stats.fg_quads += row_cache.decor_quads.len();

    for &g in &row_cache.glyphs {
        let mut translated = g;
        translated.bounds[0] += origin_x_px;
        translated.bounds[1] += row_y_px;
        translated.clip_rect = clip_px;
        translated.color[3] *= dim;
        draw_list.push_raw_glyph(layers::TERMINAL_FG, translated);
    }
    stats.glyphs += row_cache.glyphs.len();
}

#[allow(clippy::too_many_arguments)]
fn emit_cursor(
    cursor: TerminalCursor,
    origin_x_px: f32,
    origin_y_px: f32,
    scroll_px: f32,
    cell_w_px: f32,
    cell_h_px: f32,
    scale: f32,
    clip_px: [f32; 4],
    dim: f32,
    draw_list: &mut DrawList,
    stats: &mut TerminalBuildStats,
) {
    let cx = origin_x_px + f32::from(cursor.col) * cell_w_px;
    let cy = origin_y_px + (cursor.row as f32) * cell_h_px + scroll_px;
    let color = cursor.color.mul_alpha(dim);
    let quad = match cursor.shape {
        CursorShape::Block => QuadInstance::rounded_rect(
            [cx, cy, cell_w_px, cell_h_px],
            clip_px,
            color.mul_alpha(0.88),
            Color::TRANSPARENT,
            CornerRadii::uniform((1.5 * scale).round()).to_array(),
            0.0,
        ),
        CursorShape::Bar => QuadInstance::rounded_rect(
            [cx, cy, (2.0 * scale).round().max(1.0), cell_h_px],
            clip_px,
            color,
            Color::TRANSPARENT,
            CornerRadii::ZERO.to_array(),
            0.0,
        ),
        CursorShape::Underline => {
            let thick = (2.0 * scale).round().max(1.0);
            QuadInstance::rounded_rect(
                [cx, cy + cell_h_px - thick, cell_w_px, thick],
                clip_px,
                color,
                Color::TRANSPARENT,
                CornerRadii::ZERO.to_array(),
                0.0,
            )
        }
        CursorShape::Hollow => QuadInstance::rounded_rect(
            [cx, cy, cell_w_px, cell_h_px],
            clip_px,
            Color::TRANSPARENT,
            color,
            CornerRadii::uniform((1.5 * scale).round()).to_array(),
            (1.5 * scale).max(1.0),
        ),
    };
    let cursor_layer = match cursor.shape {
        CursorShape::Block => layers::TERMINAL_BG,
        CursorShape::Bar | CursorShape::Underline | CursorShape::Hollow => layers::TERMINAL_FG,
    };
    draw_list.push_raw_quad(cursor_layer, quad);
    stats.fg_quads += 1;
}

fn flush_bg_span(
    span: &mut Option<ActiveBgSpan>,
    cell_w_px: f32,
    cell_h_px: f32,
    scale: f32,
    out: &mut Vec<QuadInstance>,
) {
    let Some(s) = span.take() else {
        return;
    };
    let x = f32::from(s.start_col) * cell_w_px;
    let w = f32::from(s.end_col.saturating_sub(s.start_col)) * cell_w_px;
    if w <= 0.0 {
        return;
    }
    let radius = if s.is_selection {
        (3.0 * scale).round()
    } else {
        0.0
    };
    out.push(QuadInstance::rounded_rect(
        [x, 0.0, w, cell_h_px],
        ClipRect::UNBOUNDED.to_array(),
        s.color,
        Color::TRANSPARENT,
        CornerRadii::uniform(radius).to_array(),
        0.0,
    ));
}

fn flush_underline_span(
    span: &mut Option<ActiveUnderlineSpan>,
    cell_w_px: f32,
    underline_y_px: f32,
    thickness_px: f32,
    scale: f32,
    out: &mut Vec<QuadInstance>,
) {
    let Some(s) = span.take() else {
        return;
    };
    let x = f32::from(s.start_col) * cell_w_px;
    let w = f32::from(s.end_col.saturating_sub(s.start_col)) * cell_w_px;
    if w <= 0.0 {
        return;
    }
    let clip = ClipRect::UNBOUNDED.to_array();
    match s.style {
        UnderlineStyle::Single => {
            out.push(QuadInstance::rounded_rect(
                [x, underline_y_px, w, thickness_px],
                clip,
                s.color,
                Color::TRANSPARENT,
                CornerRadii::ZERO.to_array(),
                0.0,
            ));
        }
        UnderlineStyle::Double => {
            let gap = (thickness_px + scale).round().max(2.0);
            out.push(QuadInstance::rounded_rect(
                [x, (underline_y_px - gap * 0.5).max(0.0), w, thickness_px],
                clip,
                s.color,
                Color::TRANSPARENT,
                CornerRadii::ZERO.to_array(),
                0.0,
            ));
            out.push(QuadInstance::rounded_rect(
                [x, underline_y_px + gap * 0.5, w, thickness_px],
                clip,
                s.color,
                Color::TRANSPARENT,
                CornerRadii::ZERO.to_array(),
                0.0,
            ));
        }
        UnderlineStyle::Curly => {
            let amp = (1.8 * scale).max(1.2);
            let h = (amp * 2.0 + thickness_px + 2.0).ceil();
            out.push(QuadInstance::curly_underline(
                [x, underline_y_px - amp, w, h],
                clip,
                s.color,
                thickness_px,
                (cell_w_px * 0.9).max(5.0),
                amp,
            ));
        }
        UnderlineStyle::Dotted => {
            out.push(QuadInstance::dotted_underline(
                [x, underline_y_px - 1.0, w, thickness_px + 2.0],
                clip,
                s.color,
                thickness_px,
                (3.0 * scale).max(3.0),
            ));
        }
        UnderlineStyle::Dashed => {
            out.push(QuadInstance::dashed_underline(
                [x, underline_y_px - 1.0, w, thickness_px + 2.0],
                clip,
                s.color,
                thickness_px,
                (7.0 * scale).max(6.0),
            ));
        }
    }
}

fn flush_strike_span(
    span: &mut Option<ActiveStrikeSpan>,
    cell_w_px: f32,
    strike_y_px: f32,
    thickness_px: f32,
    out: &mut Vec<QuadInstance>,
) {
    let Some(s) = span.take() else {
        return;
    };
    let x = f32::from(s.start_col) * cell_w_px;
    let w = f32::from(s.end_col.saturating_sub(s.start_col)) * cell_w_px;
    if w <= 0.0 {
        return;
    }
    out.push(QuadInstance::rounded_rect(
        [x, strike_y_px, w, thickness_px],
        ClipRect::UNBOUNDED.to_array(),
        s.color,
        Color::TRANSPARENT,
        CornerRadii::ZERO.to_array(),
        0.0,
    ));
}

/// Arena-backed snapshot implementing [`TerminalRowSource`] with zero per-cell `String` allocations.
///
/// Handy for adapters, tests, and headless rendering benchmarks.
#[derive(Debug, Clone, Default)]
pub struct TerminalGridSnapshot {
    text_arena: String,
    cells: Vec<SnapshotCell>,
    rows: Vec<SnapshotRow>,
    cursor: Option<TerminalCursor>,
}

#[derive(Debug, Clone)]
struct SnapshotCell {
    text_range: Range<usize>,
    col: u16,
    width: u8,
    fg: Color,
    bg: Option<Color>,
    underline_color: Option<Color>,
    flags: CellFlags,
}

#[derive(Debug, Clone)]
struct SnapshotRow {
    id: RowId,
    visual_row: i32,
    dirty: bool,
    cell_range: Range<usize>,
}

impl TerminalGridSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_cursor(&mut self, cursor: Option<TerminalCursor>) {
        self.cursor = cursor;
    }

    /// Marks every row clean (simulating subsequent frames where no VT bytes mutated the grid).
    pub fn mark_all_clean(&mut self) {
        for r in &mut self.rows {
            r.dirty = false;
        }
    }

    /// Marks a specific row index dirty.
    pub fn mark_row_dirty(&mut self, idx: usize) {
        if let Some(r) = self.rows.get_mut(idx) {
            r.dirty = true;
        }
    }

    /// Appends a row built from `(text, fg, bg, flags)` styled spans (each char occupies 1 cell).
    pub fn push_styled_row<'a>(
        &mut self,
        id: RowId,
        visual_row: i32,
        dirty: bool,
        spans: impl IntoIterator<Item = (&'a str, Color, Option<Color>, CellFlags)>,
    ) {
        let start_cell = self.cells.len();
        let mut col = 0u16;
        for (text, fg, bg, flags) in spans {
            for ch in text.chars() {
                let start = self.text_arena.len();
                self.text_arena.push(ch);
                let end = self.text_arena.len();
                self.cells.push(SnapshotCell {
                    text_range: start..end,
                    col,
                    width: 1,
                    fg,
                    bg,
                    underline_color: None,
                    flags,
                });
                col = col.saturating_add(1);
            }
        }
        let end_cell = self.cells.len();
        self.rows.push(SnapshotRow {
            id,
            visual_row,
            dirty,
            cell_range: start_cell..end_cell,
        });
    }
}

impl TerminalRowSource for TerminalGridSnapshot {
    fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn row_meta(&self, idx: usize) -> (RowId, i32, bool) {
        let r = &self.rows[idx];
        (r.id, r.visual_row, r.dirty)
    }

    fn visit_row_cells(&self, idx: usize, visit: &mut dyn FnMut(StyledCell<'_>)) {
        let range = self.rows[idx].cell_range.clone();
        for cell in &self.cells[range] {
            visit(StyledCell {
                text: &self.text_arena[cell.text_range.clone()],
                col: cell.col,
                width: cell.width,
                fg: cell.fg,
                bg: cell.bg,
                underline_color: cell.underline_color,
                flags: cell.flags,
            });
        }
    }

    fn cursor(&self) -> Option<TerminalCursor> {
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_vt_flags_does_not_reapply_inverse_already_resolved_by_vt_engine() {
        let flags = convert_vt_flags(
            vt_engine::CellFlags::BOLD
                | vt_engine::CellFlags::INVERSE
                | vt_engine::CellFlags::ITALIC,
            vt_engine::UnderlineStyle::Single,
        );
        assert!(flags.contains(CellFlags::BOLD));
        assert!(flags.contains(CellFlags::ITALIC));
        assert!(flags.contains(CellFlags::UNDERLINE));
        assert!(
            !flags.contains(CellFlags::INVERSE),
            "vt_engine already swaps fg/bg for SGR 7; forwarding INVERSE double-swaps it back"
        );
    }

    #[test]
    fn set_mono_family_bumps_generation_and_color_changes_invalidate_terminal_cache() {
        let shared =
            crate::GpuShared::new_headless(&crate::GpuConfig::default()).expect("headless GPU");
        let mut cache = TerminalPaneCache::new();
        let mut snap = TerminalGridSnapshot::new();
        snap.push_styled_row(
            1,
            0,
            true,
            [
                ("hello ", Color::WHITE, None, CellFlags::NONE),
                (
                    "link",
                    Color::WHITE,
                    None,
                    CellFlags::HYPERLINK | CellFlags::SELECTED,
                ),
            ],
        );

        let mut atlas = shared.atlas();
        let metrics1 = atlas.measure_monospace_cell(14.0, 1.0);
        let mut vp = TerminalViewport {
            rect: Rect::new(0.0, 0.0, 400.0, 100.0),
            clip_rect: ClipRect::UNBOUNDED,
            cols: 40,
            rows: 4,
            metrics: metrics1,
            scroll_offset_y: 0.0,
            default_bg: Color::from_hex(0x121212),
            default_fg: Color::from_hex(0xffffff),
            selection_bg: Color::from_hex(0x2a2a2a),
            selection_fg: Some(Color::from_hex(0xf0f0f0)),
            hyperlink_color: Color::from_hex(0x4ea1ff),
            dim_alpha: 1.0,
        };

        let mut dl = DrawList::new(400, 100, 1.0);
        let stats1 = cache.build_pane(&snap, &vp, &mut atlas, shared.queue(), &mut dl);
        assert_eq!(stats1.rebuilt_rows, 1);
        assert_eq!(stats1.cached_rows, 0);

        // Clean row hits cache when nothing changes.
        snap.mark_all_clean();
        let mut dl = DrawList::new(400, 100, 1.0);
        let stats_hit = cache.build_pane(&snap, &vp, &mut atlas, shared.queue(), &mut dl);
        assert_eq!(stats_hit.rebuilt_rows, 0);
        assert_eq!(stats_hit.cached_rows, 1);

        // Switching mono font family (even with identical cell metrics) must bump generation & invalidate cache.
        let gen_before = atlas.generation();
        let same_name = atlas.mono_family_name().to_owned();
        atlas.set_mono_family(same_name);
        assert_eq!(
            atlas.generation(),
            gen_before,
            "setting identical mono family should be a no-op"
        );
        atlas.set_mono_family("Liberation Mono");
        assert_eq!(atlas.generation(), gen_before + 1);
        vp.metrics = atlas.measure_monospace_cell(14.0, 1.0);

        let mut dl = DrawList::new(400, 100, 1.0);
        let stats_after_font = cache.build_pane(&snap, &vp, &mut atlas, shared.queue(), &mut dl);
        assert_eq!(stats_after_font.rebuilt_rows, 1);
        assert_eq!(stats_after_font.cached_rows, 0);

        // Changing selection_fg invalidates cache.
        vp.selection_fg = Some(Color::from_hex(0x111111));
        let mut dl = DrawList::new(400, 100, 1.0);
        let stats_sel_fg = cache.build_pane(&snap, &vp, &mut atlas, shared.queue(), &mut dl);
        assert_eq!(stats_sel_fg.rebuilt_rows, 1);
        assert_eq!(stats_sel_fg.cached_rows, 0);

        // Changing hyperlink_color invalidates cache.
        vp.hyperlink_color = Color::from_hex(0xff8800);
        let mut dl = DrawList::new(400, 100, 1.0);
        let stats_link = cache.build_pane(&snap, &vp, &mut atlas, shared.queue(), &mut dl);
        assert_eq!(stats_link.rebuilt_rows, 1);
        assert_eq!(stats_link.cached_rows, 0);

        // Changing default_fg invalidates cache.
        vp.default_fg = Color::from_hex(0xcccccc);
        let mut dl = DrawList::new(400, 100, 1.0);
        let stats_fg = cache.build_pane(&snap, &vp, &mut atlas, shared.queue(), &mut dl);
        assert_eq!(stats_fg.rebuilt_rows, 1);
        assert_eq!(stats_fg.cached_rows, 0);
    }
}
