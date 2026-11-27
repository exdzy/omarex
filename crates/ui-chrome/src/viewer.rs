//! Docked right-hand tabbed Viewer Pane UI (`Text`/`Code`/`Markdown` reader with syntax
//! highlighting, git diff, selection, `Ctrl+F` search, Markdown 2.0, interactive zoom/pan
//! `Image` and `Pdf` viewers, `Audio` waveform player, and interactive `Video` player), emitting
//! backend-agnostic [`UiDrawList`] primitives and exact [`ViewerImagePlacement`] GPU quads.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use omarchy_theme::{Rgba, Theme, UiTokens};

use crate::draw::{
    FontKind, FontWeight, HitTarget, ShadowSpec, TextAlign, TextSpan, UiDrawList, UiInteraction,
    UiTextMeasurer, cap_radius, z_layer,
};
use crate::icons::IconId;
use crate::layout::{RectF, VIEWER_DEFAULT_RATIO, ViewerSplitGeometry, clamp_viewer_ratio};
use crate::syntax::{self, SyntaxLanguage, SyntaxToken};
use crate::text_input::TextInput;

/// Vertical line height in logical pixels for code/text rows in the viewer.
pub const TEXT_LINE_HEIGHT: f32 = 20.0;
/// Minimum zoom factor for image and video viewports (`10%`).
pub const MIN_ZOOM: f32 = 0.1;
/// Maximum zoom factor for image and video viewports (`1600%`).
pub const MAX_ZOOM: f32 = 16.0;
/// Multiplicative step applied by Zoom In / Zoom Out buttons (`25%` per step).
pub const ZOOM_STEP_FACTOR: f32 = 1.25;

const TAB_BAR_HEIGHT: f32 = 34.0;
const BREADCRUMB_HEIGHT: f32 = 26.0;
const IMAGE_TOOLBAR_HEIGHT: f32 = 36.0;
const VIDEO_TOOLBAR_HEIGHT: f32 = 62.0;

/// Coarse content classification driving tab icon, file-type badge, and tint color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerTabKind {
    Text,
    Code,
    Markdown,
    Image,
    Video,
    Audio,
    Pdf,
    Binary,
}

impl ViewerTabKind {
    /// Classifies a file path by extension (case-insensitive).
    #[must_use]
    pub fn from_path(path: &str) -> Self {
        let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let ext = filename
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("md" | "mdx" | "markdown") => Self::Markdown,
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "avif") => {
                Self::Image
            }
            Some("mp4" | "webm" | "mov" | "mkv" | "m4v" | "avi" | "ogv") => Self::Video,
            Some("mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma") => Self::Audio,
            Some("pdf") => Self::Pdf,
            Some(
                "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "pyi" | "go" | "c"
                | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "java" | "kt" | "kts" | "sh" | "bash"
                | "zsh" | "fish" | "toml" | "json" | "jsonc" | "jsonl" | "ndjson" | "yaml" | "yml"
                | "html" | "htm" | "css" | "scss" | "sql" | "wgsl" | "glsl" | "lua" | "nix" | "rb"
                | "php" | "zig" | "swift" | "dockerfile" | "xml" | "graphql" | "proto" | "diff"
                | "patch",
            ) => Self::Code,
            Some("txt" | "log" | "csv" | "tsv" | "env" | "ini" | "conf" | "cfg") => Self::Text,
            _ => {
                if filename.eq_ignore_ascii_case("makefile")
                    || filename.eq_ignore_ascii_case("dockerfile")
                    || filename.eq_ignore_ascii_case("cargo.lock")
                {
                    Self::Code
                } else {
                    Self::Text
                }
            }
        }
    }

    /// Short uppercase badge label rendered on each viewer tab pill.
    #[must_use]
    pub const fn badge_label(self) -> &'static str {
        match self {
            Self::Text => "TXT",
            Self::Code => "CODE",
            Self::Markdown => "MD",
            Self::Image => "IMG",
            Self::Video => "VID",
            Self::Audio => "AUD",
            Self::Pdf => "PDF",
            Self::Binary => "BIN",
        }
    }

    /// Bundled vector icon associated with this viewer tab kind.
    #[must_use]
    pub const fn icon(self) -> IconId {
        match self {
            Self::Code => IconId::Code,
            Self::Text | Self::Markdown | Self::Pdf => IconId::Document,
            Self::Image => IconId::Star,
            Self::Video => IconId::ChevronRight,
            Self::Audio => IconId::Activity,
            Self::Binary => IconId::Package,
        }
    }

    fn badge_color(self, theme: &Theme, tokens: &UiTokens) -> Rgba {
        let ansi = &theme.palette.terminal.ansi;
        match self {
            Self::Markdown => ansi[4].opaque(),
            Self::Code => ansi[6].opaque(),
            Self::Image => ansi[2].opaque(),
            Self::Video => ansi[5].opaque(),
            Self::Audio => ansi[2].opaque(),
            Self::Pdf => ansi[1].opaque(),
            Self::Text => tokens.fg_secondary,
            Self::Binary => ansi[3].opaque(),
        }
    }
}

/// Line change kind inside a [`GitDiffHunk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

/// Inline gutter marker kind for working-tree git modifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffGutterKind {
    UnstagedAdded,
    UnstagedModified,
    StagedAdded,
    StagedModified,
}

/// Single line inside a parsed unified git diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffLine {
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub kind: DiffLineKind,
    pub text: String,
}

/// Parsed unified diff hunk (`@@ -old_start,old_count +new_start,new_count @@`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffHunk {
    pub header: String,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<GitDiffLine>,
}

/// Parsed git diff & status metadata for a tracked or untracked file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitFileDiff {
    pub has_staged: bool,
    pub has_unstaged: bool,
    pub is_untracked: bool,
    pub added_count: usize,
    pub removed_count: usize,
    /// 1-based working-file line number -> inline gutter marker kind.
    pub line_marks: HashMap<usize, DiffGutterKind>,
    /// 1-based line numbers after which lines were deleted (`0` = deleted before line 1).
    pub deleted_after_lines: HashSet<usize>,
    pub hunks: Vec<GitDiffHunk>,
}

/// Cursor/selection endpoint in rendered viewer row & character column coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextDocPos {
    pub row: usize,
    pub col: usize,
}

/// Cached layout & plain-text geometry for one rendered viewer row (used for pointer selection,
/// copy, `path:line` resolution, and `Ctrl+F` search match highlighting).
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedRowMap {
    /// Top offset relative to the top of the document content (`10.0` for row 0).
    pub content_y: f32,
    pub height: f32,
    pub text_x: f32,
    pub cell_w: f32,
    pub plain_text: String,
    pub source_line: Option<usize>,
    pub is_mono: bool,
}

/// State of an inline `![alt](src)` image embedded inside a Markdown document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownInlineImage {
    Loading,
    Loaded {
        texture_key: u64,
        width: u32,
        height: u32,
    },
    Failed {
        reason: String,
    },
}

/// Single `Ctrl+F` search match inside the active text/code/markdown viewer document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerSearchMatch {
    pub row: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub source_line: Option<usize>,
}

/// In-viewer `Ctrl+F` incremental search bar state.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerSearchState {
    pub open: bool,
    pub input: TextInput,
    pub case_sensitive: bool,
    pub matches: Vec<ViewerSearchMatch>,
    pub active_match: Option<usize>,
}

impl Default for ViewerSearchState {
    fn default() -> Self {
        Self {
            open: false,
            input: TextInput::new("Find in file…"),
            case_sensitive: false,
            matches: Vec::new(),
            active_match: None,
        }
    }
}

/// Interactive state for the Text / Code / Markdown reader.
#[derive(Debug, Clone, PartialEq)]
pub struct TextViewerState {
    pub text: String,
    pub lines: Vec<String>,
    pub language: Option<String>,
    pub is_markdown: bool,
    /// When `is_markdown == true`, `false` renders formatted markdown and `true` renders raw source.
    pub raw_markdown: bool,
    pub word_wrap: bool,
    /// Optional 1-based highlighted target line from `:line` / `#L` jump.
    pub highlight_line: Option<usize>,
    /// Optional 1-based target column from `:line:col`.
    pub highlight_col: Option<usize>,
    /// Vertical scroll offset in logical pixels (`0.0` = top).
    pub scroll_y: f32,
    /// Horizontal scroll offset in logical pixels (`0.0` = left).
    pub scroll_x: f32,
    /// Last measured viewport height (updated via `sync_render_metrics`).
    pub last_viewport_h: f32,
    /// Last measured total content height (updated via `sync_render_metrics`).
    pub last_content_h: f32,
    pub file_size_bytes: Option<u64>,
    /// Physical-pixel-aligned monospace cell width measured from `GlyphAtlas`.
    pub mono_cell_w: f32,
    /// Cached per-line multi-token syntax highlights (byte ranges match `syntax::expand_tabs(line)`).
    pub syntax_tokens: Vec<Vec<SyntaxToken>>,
    /// Git diff/status metadata for this file (if inside a git repository).
    pub git_diff: Option<GitFileDiff>,
    /// When `true`, renders unified git diff hunks (`@@ ... @@`, `+`/`-` lines).
    pub diff_mode: bool,
    /// Decoded inline local images keyed by raw Markdown `src`.
    pub inline_images: HashMap<String, MarkdownInlineImage>,
    /// Pointer/keyboard selection anchor in rendered row coordinates.
    pub selection_anchor: Option<TextDocPos>,
    /// Pointer/keyboard selection focus/head in rendered row coordinates.
    pub selection_focus: Option<TextDocPos>,
    /// Rendered row geometry synced from the latest frame for selection & search.
    pub cached_row_map: Vec<RenderedRowMap>,
    /// Last rendered scrollable body rect in window coordinates.
    pub last_body_rect: Option<RectF>,
}

impl TextViewerState {
    #[must_use]
    pub fn new(text: impl Into<String>, language: Option<String>, is_markdown: bool) -> Self {
        let text = text.into();
        let lines = split_lines_owned(&text);
        let estimated_h = estimate_text_content_height(lines.len());
        let syntax_lang = language
            .as_deref()
            .map(SyntaxLanguage::from_hint)
            .unwrap_or(if is_markdown {
                SyntaxLanguage::Markdown
            } else {
                SyntaxLanguage::Plain
            });
        let syntax_tokens = syntax::tokenize_lines(&lines, syntax_lang);
        let default_row_map = build_default_row_map(&lines, 7.5);
        Self {
            file_size_bytes: Some(text.len() as u64),
            text,
            lines,
            language,
            is_markdown,
            raw_markdown: false,
            word_wrap: false,
            highlight_line: None,
            highlight_col: None,
            scroll_y: 0.0,
            scroll_x: 0.0,
            last_viewport_h: 400.0,
            last_content_h: estimated_h,
            mono_cell_w: 7.5,
            syntax_tokens,
            git_diff: None,
            diff_mode: false,
            inline_images: HashMap::new(),
            selection_anchor: None,
            selection_focus: None,
            cached_row_map: default_row_map,
            last_body_rect: None,
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.file_size_bytes = Some(self.text.len() as u64);
        self.lines = split_lines_owned(&self.text);
        self.recompute_syntax_tokens();
        if !self.is_markdown && !self.diff_mode {
            self.last_content_h = estimate_text_content_height(self.lines.len());
            self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll());
        }
        self.cached_row_map = build_default_row_map(&self.lines, self.mono_cell_w);
        self.clear_selection();
    }

    pub fn set_language(&mut self, language: Option<String>) {
        self.language = language;
        self.recompute_syntax_tokens();
    }

    pub fn recompute_syntax_tokens(&mut self) {
        let lang = self
            .language
            .as_deref()
            .map(SyntaxLanguage::from_hint)
            .unwrap_or(if self.is_markdown {
                SyntaxLanguage::Markdown
            } else {
                SyntaxLanguage::Plain
            });
        self.syntax_tokens = syntax::tokenize_lines(&self.lines, lang);
    }

    pub fn set_git_diff(&mut self, diff: Option<GitFileDiff>) {
        if diff
            .as_ref()
            .is_none_or(|d| !d.has_staged && !d.has_unstaged)
        {
            self.diff_mode = false;
        }
        self.git_diff = diff;
    }

    pub fn toggle_diff_mode(&mut self) -> bool {
        if self
            .git_diff
            .as_ref()
            .is_some_and(|d| d.has_staged || d.has_unstaged)
        {
            self.diff_mode = !self.diff_mode;
            self.clear_selection();
            if self.diff_mode {
                let max_chars = ((self.last_body_rect.map_or(600.0, |r| r.w) - 100.0)
                    / self.mono_cell_w.max(4.0))
                .floor()
                .max(16.0) as usize;
                let diff_rows = self
                    .git_diff
                    .as_ref()
                    .map(|d| build_diff_rows(self, d, max_chars))
                    .unwrap_or_default();
                let mut y_cursor = 10.0f32;
                let mut target_y: Option<f32> = None;
                let mut nearest_y: Option<(usize, f32)> = None;
                for row in &diff_rows {
                    let row_h = match row {
                        TextRenderRow::DiffHunkHeader { .. } => TEXT_LINE_HEIGHT + 2.0,
                        _ => TEXT_LINE_HEIGHT,
                    };
                    if let TextRenderRow::DiffCodeLine { new_line, .. } = row
                        && let Some(target) = self.highlight_line
                        && let Some(nl) = new_line
                    {
                        if *nl == target && target_y.is_none() {
                            target_y = Some(y_cursor);
                        }
                        let dist = nl.abs_diff(target);
                        if nearest_y.is_none_or(|(best_d, _)| dist < best_d) {
                            nearest_y = Some((dist, y_cursor));
                        }
                    }
                    y_cursor += row_h;
                }
                self.last_content_h = (y_cursor + 16.0).max(self.last_viewport_h);
                if self.highlight_line.is_some()
                    && let Some(ty) = target_y.or_else(|| nearest_y.map(|(_, y)| y))
                {
                    let desired = (ty - self.last_viewport_h * 0.35).max(0.0);
                    self.scroll_y = desired.clamp(0.0, self.max_scroll());
                } else {
                    self.scroll_y = 0.0;
                }
            } else {
                self.last_content_h = estimate_text_content_height(self.lines.len());
                self.cached_row_map = build_default_row_map(&self.lines, self.mono_cell_w);
                if let Some(target) = self.highlight_line {
                    self.jump_to_line(target, self.highlight_col, None);
                } else {
                    self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll());
                }
            }
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    #[must_use]
    pub fn max_scroll(&self) -> f32 {
        let content_h = if self.last_content_h > 0.0 {
            self.last_content_h
        } else {
            estimate_text_content_height(self.lines.len())
        };
        (content_h - self.last_viewport_h.max(0.0)).max(0.0)
    }

    /// Highlights `line` (1-based) and scrolls so it sits comfortably inside the viewport.
    pub fn jump_to_line(&mut self, line: usize, col: Option<usize>, viewport_h: Option<f32>) {
        if let Some(vh) = viewport_h
            && vh.is_finite()
            && vh > 0.0
        {
            self.last_viewport_h = vh;
        }
        let target_line = line.max(1).min(self.lines.len().max(1));
        self.highlight_line = Some(target_line);
        self.highlight_col = col.map(|c| c.max(1));
        let line_top = self
            .cached_row_map
            .iter()
            .find(|r| r.source_line == Some(target_line))
            .map_or((target_line - 1) as f32 * TEXT_LINE_HEIGHT + 10.0, |r| {
                r.content_y
            });
        let desired = (line_top - self.last_viewport_h * 0.35).max(0.0);
        self.scroll_y = desired.clamp(0.0, self.max_scroll());
    }

    /// Applies a vertical scroll delta in logical pixels (positive scrolls down).
    pub fn scroll_by(&mut self, delta_y: f32) {
        if !delta_y.is_finite() {
            return;
        }
        self.scroll_y = (self.scroll_y + delta_y).clamp(0.0, self.max_scroll());
    }

    /// Scrolls directly to a normalized vertical fraction in `0.0..=1.0`.
    pub fn scroll_to_fraction(&mut self, fraction: f32) {
        let f = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.scroll_y = f * self.max_scroll();
    }

    /// Maps window coordinates `(px, py)` into a [`TextDocPos`] using `cached_row_map`.
    #[must_use]
    pub fn hit_test_text_pos(&self, px: f32, py: f32) -> Option<TextDocPos> {
        let body = self.last_body_rect?;
        if self.cached_row_map.is_empty() {
            return None;
        }
        let doc_y = (py - body.y) + self.scroll_y;
        let row_idx = match self.cached_row_map.binary_search_by(|r| {
            if doc_y < r.content_y {
                std::cmp::Ordering::Greater
            } else if doc_y > r.content_y + r.height {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        }) {
            Ok(i) => i,
            Err(i) => i.min(self.cached_row_map.len().saturating_sub(1)),
        };
        let row = &self.cached_row_map[row_idx];
        let char_len = row.plain_text.chars().count();
        let cell_w = row.cell_w.max(4.0);
        let col = (((px - row.text_x + self.scroll_x) / cell_w).round()).clamp(0.0, char_len as f32)
            as usize;
        Some(TextDocPos { row: row_idx, col })
    }

    /// Starts a single-click (char), double-click (word), or triple-click (full row) selection.
    pub fn begin_selection(&mut self, pos: TextDocPos, click_count: u8) {
        let Some(row) = self.cached_row_map.get(pos.row) else {
            return;
        };
        let char_len = row.plain_text.chars().count();
        let col = pos.col.min(char_len);
        if click_count >= 3 {
            self.selection_anchor = Some(TextDocPos {
                row: pos.row,
                col: 0,
            });
            self.selection_focus = Some(TextDocPos {
                row: pos.row,
                col: char_len,
            });
        } else if click_count == 2 {
            let (ws, we) = word_bounds_at_char_col(&row.plain_text, col);
            self.selection_anchor = Some(TextDocPos {
                row: pos.row,
                col: ws,
            });
            self.selection_focus = Some(TextDocPos {
                row: pos.row,
                col: we,
            });
        } else {
            let p = TextDocPos { row: pos.row, col };
            self.selection_anchor = Some(p);
            self.selection_focus = Some(p);
        }
        if let Some(src_line) = row.source_line {
            self.highlight_line = Some(src_line);
        }
    }

    /// Extends the active drag selection to `pos`.
    pub fn drag_selection(&mut self, pos: TextDocPos) {
        if self.cached_row_map.is_empty() {
            return;
        }
        let clamped_row = pos.row.min(self.cached_row_map.len().saturating_sub(1));
        let char_len = self.cached_row_map[clamped_row].plain_text.chars().count();
        let clamped_col = pos.col.min(char_len);
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(TextDocPos {
                row: clamped_row,
                col: clamped_col,
            });
        }
        self.selection_focus = Some(TextDocPos {
            row: clamped_row,
            col: clamped_col,
        });
    }

    /// Selects the entire rendered document.
    pub fn select_all(&mut self) {
        if self.cached_row_map.is_empty() {
            return;
        }
        let last_row = self.cached_row_map.len() - 1;
        let last_col = self.cached_row_map[last_row].plain_text.chars().count();
        self.selection_anchor = Some(TextDocPos { row: 0, col: 0 });
        self.selection_focus = Some(TextDocPos {
            row: last_row,
            col: last_col,
        });
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_focus = None;
    }

    /// Returns ordered `(start, end)` selection bounds if non-empty.
    #[must_use]
    pub fn ordered_selection(&self) -> Option<(TextDocPos, TextDocPos)> {
        let a = self.selection_anchor?;
        let b = self.selection_focus?;
        if a == b {
            None
        } else if a <= b {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    /// Extracts currently selected text across rendered rows (empty/none if no range selected).
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.ordered_selection()?;
        if self.cached_row_map.is_empty() {
            return None;
        }
        let r_start = start.row.min(self.cached_row_map.len().saturating_sub(1));
        let r_end = end.row.min(self.cached_row_map.len().saturating_sub(1));
        let mut out = String::new();
        for (r, row_entry) in self
            .cached_row_map
            .iter()
            .enumerate()
            .take(r_end + 1)
            .skip(r_start)
        {
            let text = &row_entry.plain_text;
            let chars: Vec<char> = text.chars().collect();
            let c0 = if r == r_start {
                start.col.min(chars.len())
            } else {
                0
            };
            let c1 = if r == r_end {
                end.col.min(chars.len())
            } else {
                chars.len()
            };
            if c1 > c0 {
                for &ch in &chars[c0..c1] {
                    out.push(ch);
                }
            }
            if r < r_end {
                out.push('\n');
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Computes visible viewport rectangles covering the active selection (used both to draw
    /// selection quads and to trigger GPU Copy Shine on `Ctrl+C`).
    #[must_use]
    pub fn selection_viewport_rects(&self) -> Vec<RectF> {
        let Some((start, end)) = self.ordered_selection() else {
            return Vec::new();
        };
        let Some(body) = self.last_body_rect else {
            return Vec::new();
        };
        let mut rects = Vec::new();
        let r_start = start.row.min(self.cached_row_map.len().saturating_sub(1));
        let r_end = end.row.min(self.cached_row_map.len().saturating_sub(1));
        for r_idx in r_start..=r_end {
            let row = &self.cached_row_map[r_idx];
            let y = body.y + row.content_y - self.scroll_y;
            if y + row.height < body.y || y > body.bottom() {
                continue;
            }
            let char_len = row.plain_text.chars().count();
            let c0 = if r_idx == r_start {
                start.col.min(char_len)
            } else {
                0
            };
            let c1 = if r_idx == r_end {
                end.col.min(char_len)
            } else {
                char_len
            };
            if c1 <= c0 && r_idx == r_start && r_idx == r_end {
                continue;
            }
            let cell_w = row.cell_w.max(4.0);
            let x = row.text_x + c0 as f32 * cell_w - self.scroll_x;
            let w = ((c1.saturating_sub(c0)).max(1) as f32) * cell_w;
            let raw = RectF::new(x, y, w, row.height);
            let clipped = raw.intersect(body);
            if !clipped.is_empty() {
                rects.push(clipped);
            }
        }
        rects
    }

    /// Returns the 1-based line number for `path:line` copy (selection focus/anchor -> highlighted line -> 1).
    #[must_use]
    pub fn target_or_active_line(&self) -> usize {
        self.selection_focus
            .or(self.selection_anchor)
            .and_then(|pos| self.cached_row_map.get(pos.row))
            .and_then(|r| r.source_line)
            .or(self.highlight_line)
            .unwrap_or(1)
    }
}

fn build_default_row_map(lines: &[String], cell_w: f32) -> Vec<RenderedRowMap> {
    let mut y = 10.0f32;
    let cw = cell_w.max(4.0);
    lines
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            let row = RenderedRowMap {
                content_y: y,
                height: TEXT_LINE_HEIGHT,
                text_x: 56.0,
                cell_w: cw,
                plain_text: syntax::expand_tabs(line),
                source_line: Some(idx + 1),
                is_mono: true,
            };
            y += TEXT_LINE_HEIGHT;
            row
        })
        .collect()
}

fn word_bounds_at_char_col(text: &str, col: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return (0, 0);
    }
    let idx = col.min(chars.len().saturating_sub(1));
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    if is_word(chars[idx]) {
        let mut s = idx;
        while s > 0 && is_word(chars[s - 1]) {
            s -= 1;
        }
        let mut e = idx + 1;
        while e < chars.len() && is_word(chars[e]) {
            e += 1;
        }
        (s, e)
    } else {
        (idx, (idx + 1).min(chars.len()))
    }
}

/// Shared zoom, fit, and pan state for interactive image and video viewports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaViewportState {
    /// Manual zoom scale (`1.0` = 100%), clamped to `[MIN_ZOOM, MAX_ZOOM]`.
    pub zoom: f32,
    /// When `true`, automatically fits the media inside the canvas preserving aspect ratio.
    pub fit_mode: bool,
    /// Pan offset in logical pixels relative to centered position.
    pub pan_offset: (f32, f32),
}

impl Default for MediaViewportState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            fit_mode: true,
            pan_offset: (0.0, 0.0),
        }
    }
}

impl MediaViewportState {
    /// Computes the effective scale factor for `(media_w, media_h)` inside `canvas`.
    #[must_use]
    pub fn effective_zoom(&self, media_w: u32, media_h: u32, canvas: RectF) -> f32 {
        if self.fit_mode {
            compute_fit_zoom(media_w, media_h, canvas)
        } else if self.zoom.is_finite() {
            self.zoom.clamp(MIN_ZOOM, MAX_ZOOM)
        } else {
            1.0
        }
    }

    /// Steps zoom in by [`ZOOM_STEP_FACTOR`], exiting `fit_mode` smoothly from the current visual scale.
    pub fn zoom_in(&mut self, media_w: u32, media_h: u32, canvas: Option<RectF>) {
        let base = if self.fit_mode {
            canvas.map_or(self.zoom, |c| compute_fit_zoom(media_w, media_h, c))
        } else {
            self.zoom
        };
        self.fit_mode = false;
        self.zoom = (base * ZOOM_STEP_FACTOR).clamp(MIN_ZOOM, MAX_ZOOM);
        if let Some(c) = canvas {
            self.clamp_pan(media_w, media_h, c);
        }
    }

    /// Steps zoom out by [`ZOOM_STEP_FACTOR`], exiting `fit_mode` smoothly from the current visual scale.
    pub fn zoom_out(&mut self, media_w: u32, media_h: u32, canvas: Option<RectF>) {
        let base = if self.fit_mode {
            canvas.map_or(self.zoom, |c| compute_fit_zoom(media_w, media_h, c))
        } else {
            self.zoom
        };
        self.fit_mode = false;
        self.zoom = (base / ZOOM_STEP_FACTOR).clamp(MIN_ZOOM, MAX_ZOOM);
        if let Some(c) = canvas {
            self.clamp_pan(media_w, media_h, c);
        }
    }

    /// Sets zoom explicitly to `100%` (`1.0`) and resets pan to center.
    pub fn zoom_reset(&mut self) {
        self.fit_mode = false;
        self.zoom = 1.0;
        self.pan_offset = (0.0, 0.0);
    }

    /// Enables automatic Fit-to-Viewport mode and resets pan to center.
    pub fn zoom_fit(&mut self) {
        self.fit_mode = true;
        self.pan_offset = (0.0, 0.0);
    }

    /// Translates the viewport by `(dx, dy)` logical pixels and clamps to valid pan limits.
    pub fn pan_by(&mut self, dx: f32, dy: f32, media_w: u32, media_h: u32, canvas: Option<RectF>) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.pan_offset.0 += dx;
        self.pan_offset.1 += dy;
        if let Some(c) = canvas {
            self.clamp_pan(media_w, media_h, c);
        }
    }

    /// Clamps `pan_offset` so media edges never detach from the viewport interior when zoomed in,
    /// and stays centered along any axis where the media fits inside `canvas`.
    pub fn clamp_pan(&mut self, media_w: u32, media_h: u32, canvas: RectF) {
        let scale = self.effective_zoom(media_w, media_h, canvas);
        let disp_w = media_w as f32 * scale;
        let disp_h = media_h as f32 * scale;
        let max_x = ((disp_w - canvas.w) * 0.5).max(0.0);
        let max_y = ((disp_h - canvas.h) * 0.5).max(0.0);
        self.pan_offset.0 = if self.pan_offset.0.is_finite() {
            self.pan_offset.0.clamp(-max_x, max_x)
        } else {
            0.0
        };
        self.pan_offset.1 = if self.pan_offset.1.is_finite() {
            self.pan_offset.1.clamp(-max_y, max_y)
        } else {
            0.0
        };
    }
}

/// Result of resolving zoom, fit, and pan math for an image or video frame inside a canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComputedMediaPlacement {
    /// Visible destination rectangle in window coordinates (already clipped to `canvas`).
    pub dest_rect: RectF,
    /// Normalized `[u0, v0, u1, v1]` texture crop matching `dest_rect`.
    pub uv_bounds: [f32; 4],
    /// Canvas clip rectangle.
    pub clip_rect: RectF,
    /// Resolved visual zoom scale.
    pub effective_zoom: f32,
    /// Clamped pan offset applied during placement.
    pub clamped_pan: (f32, f32),
}

/// Computes the fit-to-canvas scale factor preserving aspect ratio, clamped to `[MIN_ZOOM, MAX_ZOOM]`.
#[must_use]
pub fn compute_fit_zoom(media_w: u32, media_h: u32, canvas: RectF) -> f32 {
    if media_w == 0 || media_h == 0 || canvas.is_empty() {
        return 1.0;
    }
    let sx = canvas.w / media_w as f32;
    let sy = canvas.h / media_h as f32;
    sx.min(sy).clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Computes the exact GPU destination rectangle and UV crop `[u0, v0, u1, v1]` for any image or
/// video frame under arbitrary `zoom`, `fit_mode`, and `pan_offset`.
#[must_use]
pub fn compute_media_placement(
    media_w: u32,
    media_h: u32,
    canvas: RectF,
    zoom: f32,
    fit_mode: bool,
    pan_offset: (f32, f32),
) -> Option<ComputedMediaPlacement> {
    if media_w == 0 || media_h == 0 || canvas.is_empty() {
        return None;
    }

    let effective_zoom = if fit_mode {
        compute_fit_zoom(media_w, media_h, canvas)
    } else if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    };

    let disp_w = (media_w as f32 * effective_zoom).max(1.0);
    let disp_h = (media_h as f32 * effective_zoom).max(1.0);

    let max_pan_x = ((disp_w - canvas.w) * 0.5).max(0.0);
    let max_pan_y = ((disp_h - canvas.h) * 0.5).max(0.0);
    let clamped_x = if pan_offset.0.is_finite() {
        pan_offset.0.clamp(-max_pan_x, max_pan_x)
    } else {
        0.0
    };
    let clamped_y = if pan_offset.1.is_finite() {
        pan_offset.1.clamp(-max_pan_y, max_pan_y)
    } else {
        0.0
    };

    let raw_x = canvas.x + (canvas.w - disp_w) * 0.5 + clamped_x;
    let raw_y = canvas.y + (canvas.h - disp_h) * 0.5 + clamped_y;
    let raw_rect = RectF::new(raw_x, raw_y, disp_w, disp_h);
    let dest_rect = raw_rect.intersect(canvas);
    if dest_rect.is_empty() {
        return None;
    }

    let u0 = ((dest_rect.x - raw_rect.x) / disp_w).clamp(0.0, 1.0);
    let v0 = ((dest_rect.y - raw_rect.y) / disp_h).clamp(0.0, 1.0);
    let u1 = ((dest_rect.right() - raw_rect.x) / disp_w).clamp(0.0, 1.0);
    let v1 = ((dest_rect.bottom() - raw_rect.y) / disp_h).clamp(0.0, 1.0);

    Some(ComputedMediaPlacement {
        dest_rect,
        uv_bounds: [u0, v0, u1, v1],
        clip_rect: canvas,
        effective_zoom,
        clamped_pan: (clamped_x, clamped_y),
    })
}

/// Interactive state for the Image Viewer tab.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageViewerState {
    /// GPU texture key in `gpu-renderer::ImageTextureCache`.
    pub texture_key: u64,
    pub width: u32,
    pub height: u32,
    pub format_label: String,
    pub file_size_bytes: Option<u64>,
    /// Draws a subtle transparency checkerboard behind the image when `true`.
    pub has_alpha: bool,
    pub viewport: MediaViewportState,
    /// Last rendered canvas rect for accurate zoom-from-fit & pan clamping.
    pub last_canvas_rect: Option<RectF>,
}

impl ImageViewerState {
    #[must_use]
    pub fn new(
        texture_key: u64,
        width: u32,
        height: u32,
        format_label: impl Into<String>,
        file_size_bytes: Option<u64>,
        has_alpha: bool,
    ) -> Self {
        Self {
            texture_key,
            width,
            height,
            format_label: format_label.into(),
            file_size_bytes,
            has_alpha,
            viewport: MediaViewportState::default(),
            last_canvas_rect: None,
        }
    }

    /// Formats metadata string like `"1920×1080 · PNG · 412 KB"`.
    #[must_use]
    pub fn metadata_summary(&self) -> String {
        let mut parts = vec![format!("{}×{}", self.width, self.height)];
        if !self.format_label.is_empty() {
            parts.push(self.format_label.clone());
        }
        if let Some(bytes) = self.file_size_bytes {
            parts.push(format_file_size(bytes));
        }
        parts.join(" · ")
    }
}

/// Interactive state for the Video Player tab.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoViewerState {
    /// GPU texture key for the currently decoded video frame (`0` if pending first frame).
    pub texture_key: u64,
    pub width: u32,
    pub height: u32,
    pub duration_secs: f64,
    pub position_secs: f64,
    pub playing: bool,
    pub muted: bool,
    pub looping: bool,
    pub fps: Option<f32>,
    pub codec: Option<String>,
    pub file_size_bytes: Option<u64>,
    pub viewport: MediaViewportState,
    pub last_canvas_rect: Option<RectF>,
    pub last_seek_bar_rect: Option<RectF>,
}

impl VideoViewerState {
    #[must_use]
    pub fn new(
        texture_key: u64,
        width: u32,
        height: u32,
        duration_secs: f64,
        fps: Option<f32>,
        codec: Option<String>,
        file_size_bytes: Option<u64>,
    ) -> Self {
        Self {
            texture_key,
            width,
            height,
            duration_secs: if duration_secs.is_finite() {
                duration_secs.max(0.0)
            } else {
                0.0
            },
            position_secs: 0.0,
            playing: false,
            muted: false,
            looping: false,
            fps,
            codec,
            file_size_bytes,
            viewport: MediaViewportState::default(),
            last_canvas_rect: None,
            last_seek_bar_rect: None,
        }
    }

    /// Normalized playback progress in `0.0..=1.0`.
    #[must_use]
    pub fn progress_fraction(&self) -> f32 {
        if self.duration_secs <= 0.0 || !self.position_secs.is_finite() {
            return 0.0;
        }
        (self.position_secs / self.duration_secs).clamp(0.0, 1.0) as f32
    }

    /// Seeks playback to a normalized `fraction` in `0.0..=1.0` and returns the new timestamp in seconds.
    pub fn seek_to_fraction(&mut self, fraction: f32) -> f64 {
        let clamped = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.position_secs = f64::from(clamped) * self.duration_secs.max(0.0);
        self.position_secs
    }

    /// Formats a video timestamp like `"0:04"`, `"1:28"`, or `"1:02:05"`.
    #[must_use]
    pub fn format_timestamp(secs: f64) -> String {
        let total = if secs.is_finite() && secs > 0.0 {
            secs.floor() as u64
        } else {
            0
        };
        let hours = total / 3600;
        let mins = (total % 3600) / 60;
        let rem = total % 60;
        if hours > 0 {
            format!("{hours}:{mins:02}:{rem:02}")
        } else {
            format!("{mins}:{rem:02}")
        }
    }

    /// Formats metadata string like `"1920×1080 · 60fps · H.264"`.
    #[must_use]
    pub fn metadata_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.width > 0 && self.height > 0 {
            parts.push(format!("{}×{}", self.width, self.height));
        }
        if let Some(fps) = self.fps
            && fps.is_finite()
            && fps > 0.0
        {
            if (fps.fract()).abs() < 0.05 {
                parts.push(format!("{}fps", fps.round() as u32));
            } else {
                parts.push(format!("{fps:.1}fps"));
            }
        }
        if let Some(codec) = self.codec.as_deref()
            && !codec.is_empty()
        {
            parts.push(codec.to_owned());
        }
        parts.join(" · ")
    }
}

/// Maps a pointer X coordinate over `seek_bar_rect` to a clamped normalized progress fraction `0.0..=1.0`.
#[must_use]
pub fn scrub_fraction_from_x(seek_bar_rect: RectF, px: f32) -> f32 {
    if seek_bar_rect.w <= 0.0 || !px.is_finite() {
        return 0.0;
    }
    ((px - seek_bar_rect.x) / seek_bar_rect.w).clamp(0.0, 1.0)
}

/// Maps a pointer X coordinate over `seek_bar_rect` directly to a playback timestamp in seconds.
#[must_use]
pub fn scrub_time_from_x(seek_bar_rect: RectF, px: f32, duration_secs: f64) -> f64 {
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return 0.0;
    }
    f64::from(scrub_fraction_from_x(seek_bar_rect, px)) * duration_secs
}

/// Interactive state for the Audio Player tab (waveform peaks + scrub bar + transport controls).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioViewerState {
    pub duration_secs: f64,
    pub position_secs: f64,
    pub playing: bool,
    pub muted: bool,
    pub looping: bool,
    /// Normalized waveform peaks (`0.0..=1.0`, typically 96 bars).
    pub waveform_peaks: Vec<f32>,
    pub codec: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub bitrate_kbps: Option<u32>,
    pub file_size_bytes: Option<u64>,
    pub warning_banner: Option<String>,
    pub last_waveform_rect: Option<RectF>,
    pub last_seek_bar_rect: Option<RectF>,
}

impl AudioViewerState {
    #[must_use]
    pub fn new(
        duration_secs: f64,
        waveform_peaks: Vec<f32>,
        codec: Option<String>,
        sample_rate_hz: Option<u32>,
        channels: Option<u16>,
        bitrate_kbps: Option<u32>,
        file_size_bytes: Option<u64>,
    ) -> Self {
        Self {
            duration_secs: if duration_secs.is_finite() {
                duration_secs.max(0.0)
            } else {
                0.0
            },
            position_secs: 0.0,
            playing: false,
            muted: false,
            looping: false,
            waveform_peaks,
            codec,
            sample_rate_hz,
            channels,
            bitrate_kbps,
            file_size_bytes,
            warning_banner: None,
            last_waveform_rect: None,
            last_seek_bar_rect: None,
        }
    }

    #[must_use]
    pub fn progress_fraction(&self) -> f32 {
        if self.duration_secs <= 0.0 || !self.position_secs.is_finite() {
            return 0.0;
        }
        (self.position_secs / self.duration_secs).clamp(0.0, 1.0) as f32
    }

    pub fn seek_to_fraction(&mut self, fraction: f32) -> f64 {
        let clamped = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.position_secs = f64::from(clamped) * self.duration_secs.max(0.0);
        self.position_secs
    }

    #[must_use]
    pub fn metadata_summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(codec) = self.codec.as_deref()
            && !codec.is_empty()
        {
            parts.push(codec.to_owned());
        }
        if let Some(sr) = self.sample_rate_hz
            && sr > 0
        {
            if sr.is_multiple_of(1000) {
                parts.push(format!("{} kHz", sr / 1000));
            } else {
                parts.push(format!("{:.1} kHz", f64::from(sr) / 1000.0));
            }
        }
        if let Some(ch) = self.channels
            && ch > 0
        {
            parts.push(match ch {
                1 => "Mono".to_owned(),
                2 => "Stereo".to_owned(),
                n => format!("{n}ch"),
            });
        }
        if let Some(kbps) = self.bitrate_kbps
            && kbps > 0
        {
            parts.push(format!("{kbps} kbps"));
        }
        if let Some(bytes) = self.file_size_bytes {
            parts.push(format_file_size(bytes));
        }
        parts.join(" · ")
    }
}

/// Interactive state for the PDF Document Viewer tab (rasterized page + zoom/pan + page navigation).
#[derive(Debug, Clone, PartialEq)]
pub struct PdfViewerState {
    pub texture_key: u64,
    pub current_page: usize,
    pub page_count: usize,
    pub page_width: u32,
    pub page_height: u32,
    pub viewport: MediaViewportState,
    pub file_size_bytes: Option<u64>,
    pub page_text: Option<String>,
    pub loading_page: bool,
    pub fallback_notice: Option<String>,
    pub last_canvas_rect: Option<RectF>,
}

impl PdfViewerState {
    #[must_use]
    pub fn new(
        texture_key: u64,
        current_page: usize,
        page_count: usize,
        page_width: u32,
        page_height: u32,
        file_size_bytes: Option<u64>,
        page_text: Option<String>,
    ) -> Self {
        let count = page_count.max(1);
        Self {
            texture_key,
            current_page: current_page.clamp(1, count),
            page_count: count,
            page_width,
            page_height,
            viewport: MediaViewportState::default(),
            file_size_bytes,
            page_text,
            loading_page: false,
            fallback_notice: None,
            last_canvas_rect: None,
        }
    }

    #[must_use]
    pub fn metadata_summary(&self) -> String {
        let mut parts = vec![format!("{} pages", self.page_count)];
        if self.page_width > 0 && self.page_height > 0 {
            parts.push(format!("{}×{}", self.page_width, self.page_height));
        }
        if let Some(bytes) = self.file_size_bytes {
            parts.push(format_file_size(bytes));
        }
        parts.join(" · ")
    }
}

/// Content state carried by a single [`ViewerTab`].
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ViewerContent {
    Loading { label: String },
    Error { message: String },
    Text(TextViewerState),
    Image(ImageViewerState),
    Video(VideoViewerState),
    Audio(AudioViewerState),
    Pdf(PdfViewerState),
}

impl ViewerContent {
    #[must_use]
    fn inferred_kind(&self, path: &str) -> ViewerTabKind {
        match self {
            Self::Image(_) => ViewerTabKind::Image,
            Self::Video(_) => ViewerTabKind::Video,
            Self::Audio(_) => ViewerTabKind::Audio,
            Self::Pdf(_) => ViewerTabKind::Pdf,
            Self::Text(t) => {
                if t.is_markdown {
                    ViewerTabKind::Markdown
                } else {
                    match ViewerTabKind::from_path(path) {
                        ViewerTabKind::Code => ViewerTabKind::Code,
                        _ => ViewerTabKind::Text,
                    }
                }
            }
            Self::Loading { .. } | Self::Error { .. } => ViewerTabKind::from_path(path),
        }
    }
}

/// Single open document/media tab inside the docked right-hand Viewer Pane.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerTab {
    pub id: u64,
    pub path: String,
    pub title: String,
    pub remote_host: Option<String>,
    pub kind: ViewerTabKind,
    pub content: ViewerContent,
}

impl ViewerTab {
    #[must_use]
    pub fn new(
        id: u64,
        path: impl Into<String>,
        remote_host: Option<String>,
        content: ViewerContent,
    ) -> Self {
        let path = path.into();
        let title = filename_from_path(&path);
        let kind = content.inferred_kind(&path);
        Self {
            id,
            path,
            title,
            remote_host,
            kind,
            content,
        }
    }

    #[must_use]
    pub fn loading(
        id: u64,
        path: impl Into<String>,
        remote_host: Option<String>,
        label: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            path,
            remote_host,
            ViewerContent::Loading {
                label: label.into(),
            },
        )
    }

    #[must_use]
    pub fn error(
        id: u64,
        path: impl Into<String>,
        remote_host: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            path,
            remote_host,
            ViewerContent::Error {
                message: message.into(),
            },
        )
    }

    #[must_use]
    pub fn text(
        id: u64,
        path: impl Into<String>,
        remote_host: Option<String>,
        state: TextViewerState,
    ) -> Self {
        Self::new(id, path, remote_host, ViewerContent::Text(state))
    }

    #[must_use]
    pub fn image(
        id: u64,
        path: impl Into<String>,
        remote_host: Option<String>,
        state: ImageViewerState,
    ) -> Self {
        Self::new(id, path, remote_host, ViewerContent::Image(state))
    }

    #[must_use]
    pub fn video(
        id: u64,
        path: impl Into<String>,
        remote_host: Option<String>,
        state: VideoViewerState,
    ) -> Self {
        Self::new(id, path, remote_host, ViewerContent::Video(state))
    }

    #[must_use]
    pub fn audio(
        id: u64,
        path: impl Into<String>,
        remote_host: Option<String>,
        state: AudioViewerState,
    ) -> Self {
        Self::new(id, path, remote_host, ViewerContent::Audio(state))
    }

    #[must_use]
    pub fn pdf(
        id: u64,
        path: impl Into<String>,
        remote_host: Option<String>,
        state: PdfViewerState,
    ) -> Self {
        Self::new(id, path, remote_host, ViewerContent::Pdf(state))
    }
}

/// Multi-tab state for the docked right-hand Viewer Pane owned by [`crate::UiTree`].
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerState {
    pub open: bool,
    pub maximized: bool,
    /// Width fraction of `chrome.workspace` occupied by the viewer (`[0.22, 0.78]`, default `0.45`).
    pub split_ratio: f32,
    pub tabs: Vec<ViewerTab>,
    pub active_tab: usize,
    /// Exact physical-pixel-snapped monospace cell width for viewer code (`12.5px`).
    pub mono_cell_w: f32,
    /// In-viewer `Ctrl+F` search bar state.
    pub search: ViewerSearchState,
    next_tab_id: u64,
}

impl Default for ViewerState {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewerState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: false,
            maximized: false,
            split_ratio: VIEWER_DEFAULT_RATIO,
            tabs: Vec::new(),
            active_tab: 0,
            mono_cell_w: 7.5,
            search: ViewerSearchState::default(),
            next_tab_id: 1,
        }
    }

    /// Opens a tab for `(path, remote_host)` or focuses & updates it if already open.
    /// Returns the stable `tab_id`.
    pub fn open_or_focus_tab(
        &mut self,
        path: impl Into<String>,
        remote_host: Option<String>,
        content: ViewerContent,
    ) -> u64 {
        let path = path.into();
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| t.path == path && t.remote_host == remote_host)
        {
            self.active_tab = idx;
            self.open = true;
            let tab = &mut self.tabs[idx];
            tab.kind = content.inferred_kind(&tab.path);
            tab.content = content;
            return tab.id;
        }

        let id = self.next_tab_id;
        self.next_tab_id = self.next_tab_id.saturating_add(1);
        let tab = ViewerTab::new(id, path, remote_host, content);
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.open = true;
        id
    }

    /// Opens or focuses a tab in `Loading` state while an async local/remote fetch is in flight.
    pub fn open_loading_tab(
        &mut self,
        path: impl Into<String>,
        remote_host: Option<String>,
        label: impl Into<String>,
    ) -> u64 {
        self.open_or_focus_tab(
            path,
            remote_host,
            ViewerContent::Loading {
                label: label.into(),
            },
        )
    }

    /// Replaces the content of an existing tab by `tab_id`. Returns `false` if not found.
    pub fn set_tab_content(&mut self, tab_id: u64, content: ViewerContent) -> bool {
        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return false;
        };
        tab.kind = content.inferred_kind(&tab.path);
        tab.content = content;
        true
    }

    /// Transitions an existing tab to `ViewerContent::Error`. Returns `false` if not found.
    pub fn set_tab_error(&mut self, tab_id: u64, message: impl Into<String>) -> bool {
        self.set_tab_content(
            tab_id,
            ViewerContent::Error {
                message: message.into(),
            },
        )
    }

    /// Closes the tab at `index`, updating `active_tab` and closing the pane if no tabs remain.
    pub fn close_tab(&mut self, index: usize) -> Option<ViewerTab> {
        if index >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active_tab = 0;
            self.open = false;
            self.maximized = false;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if index < self.active_tab {
            self.active_tab -= 1;
        }
        Some(removed)
    }

    /// Closes the tab matching `tab_id`.
    pub fn close_tab_by_id(&mut self, tab_id: u64) -> Option<ViewerTab> {
        let idx = self.tabs.iter().position(|t| t.id == tab_id)?;
        self.close_tab(idx)
    }

    pub fn select_tab(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.active_tab = index;
            self.open = true;
            true
        } else {
            false
        }
    }

    pub fn select_tab_by_id(&mut self, tab_id: u64) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
            return false;
        };
        self.select_tab(idx)
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_tab = if self.active_tab == 0 {
                self.tabs.len() - 1
            } else {
                self.active_tab - 1
            };
        }
    }

    #[must_use]
    pub fn active_tab(&self) -> Option<&ViewerTab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut ViewerTab> {
        self.tabs.get_mut(self.active_tab)
    }

    #[must_use]
    pub fn tab_by_id(&self, tab_id: u64) -> Option<&ViewerTab> {
        self.tabs.iter().find(|t| t.id == tab_id)
    }

    pub fn tab_by_id_mut(&mut self, tab_id: u64) -> Option<&mut ViewerTab> {
        self.tabs.iter_mut().find(|t| t.id == tab_id)
    }

    pub fn set_split_ratio(&mut self, ratio: f32) {
        self.split_ratio = clamp_viewer_ratio(ratio);
    }

    pub fn toggle_maximized(&mut self) {
        if self.open {
            self.maximized = !self.maximized;
        }
    }

    pub fn close_pane(&mut self) {
        self.open = false;
        self.maximized = false;
    }

    /// Zooms in the active Image, Video, or PDF tab. Returns `true` if the active tab supports zoom.
    pub fn zoom_in(&mut self) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Image(img) => {
                img.viewport
                    .zoom_in(img.width, img.height, img.last_canvas_rect);
                true
            }
            ViewerContent::Video(vid) => {
                vid.viewport
                    .zoom_in(vid.width, vid.height, vid.last_canvas_rect);
                true
            }
            ViewerContent::Pdf(pdf) => {
                pdf.viewport
                    .zoom_in(pdf.page_width, pdf.page_height, pdf.last_canvas_rect);
                true
            }
            _ => false,
        }
    }

    /// Zooms out the active Image, Video, or PDF tab.
    pub fn zoom_out(&mut self) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Image(img) => {
                img.viewport
                    .zoom_out(img.width, img.height, img.last_canvas_rect);
                true
            }
            ViewerContent::Video(vid) => {
                vid.viewport
                    .zoom_out(vid.width, vid.height, vid.last_canvas_rect);
                true
            }
            ViewerContent::Pdf(pdf) => {
                pdf.viewport
                    .zoom_out(pdf.page_width, pdf.page_height, pdf.last_canvas_rect);
                true
            }
            _ => false,
        }
    }

    /// Resets the active Image, Video, or PDF tab to `100%` actual pixels centered.
    pub fn zoom_reset(&mut self) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Image(img) => {
                img.viewport.zoom_reset();
                true
            }
            ViewerContent::Video(vid) => {
                vid.viewport.zoom_reset();
                true
            }
            ViewerContent::Pdf(pdf) => {
                pdf.viewport.zoom_reset();
                true
            }
            _ => false,
        }
    }

    /// Sets the active Image, Video, or PDF tab to Fit-to-Viewport mode.
    pub fn zoom_fit(&mut self) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Image(img) => {
                img.viewport.zoom_fit();
                true
            }
            ViewerContent::Video(vid) => {
                vid.viewport.zoom_fit();
                true
            }
            ViewerContent::Pdf(pdf) => {
                pdf.viewport.zoom_fit();
                true
            }
            _ => false,
        }
    }

    /// Pans the active Image, Video, or PDF tab by `(dx, dy)` logical pixels, clamped cleanly.
    pub fn pan_by(&mut self, dx: f32, dy: f32) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Image(img) => {
                img.viewport
                    .pan_by(dx, dy, img.width, img.height, img.last_canvas_rect);
                true
            }
            ViewerContent::Video(vid) => {
                vid.viewport
                    .pan_by(dx, dy, vid.width, vid.height, vid.last_canvas_rect);
                true
            }
            ViewerContent::Pdf(pdf) => {
                pdf.viewport.pan_by(
                    dx,
                    dy,
                    pdf.page_width,
                    pdf.page_height,
                    pdf.last_canvas_rect,
                );
                true
            }
            _ => false,
        }
    }

    /// Vertically scrolls the active Text/Code/Markdown tab by `delta_y` pixels.
    pub fn scroll_by(&mut self, delta_y: f32) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Text(txt) => {
                txt.scroll_by(delta_y);
                true
            }
            _ => false,
        }
    }

    /// Scrolls the active Text/Code/Markdown tab to a normalized fraction in `0.0..=1.0`.
    pub fn scroll_active_to_fraction(&mut self, fraction: f32) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Text(txt) => {
                txt.scroll_to_fraction(fraction);
                true
            }
            _ => false,
        }
    }

    /// Jumps to a 1-based line (and optional column) in the active Text/Code/Markdown tab.
    pub fn jump_active_to_line(&mut self, line: usize, col: Option<usize>) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Text(txt) => {
                txt.jump_to_line(line, col, None);
                true
            }
            _ => false,
        }
    }

    /// Toggles word-wrap on the active Text/Code/Markdown tab.
    pub fn toggle_wrap(&mut self) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Text(txt) => {
                txt.word_wrap = !txt.word_wrap;
                true
            }
            _ => false,
        }
    }

    /// Toggles Raw vs Rendered display on the active Markdown tab.
    pub fn toggle_raw_markdown(&mut self) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Text(txt) if txt.is_markdown => {
                txt.raw_markdown = !txt.raw_markdown;
                true
            }
            _ => false,
        }
    }

    /// Toggles Play/Pause on the active Video tab, returning the new `playing` state.
    pub fn toggle_video_play_pause(&mut self) -> Option<bool> {
        let tab = self.active_tab_mut()?;
        match &mut tab.content {
            ViewerContent::Video(vid) => {
                vid.playing = !vid.playing;
                Some(vid.playing)
            }
            _ => None,
        }
    }

    /// Toggles Mute/Unmute on the active Video tab, returning the new `muted` state.
    pub fn toggle_video_mute(&mut self) -> Option<bool> {
        let tab = self.active_tab_mut()?;
        match &mut tab.content {
            ViewerContent::Video(vid) => {
                vid.muted = !vid.muted;
                Some(vid.muted)
            }
            _ => None,
        }
    }

    /// Toggles Loop on the active Video tab, returning the new `looping` state.
    pub fn toggle_video_loop(&mut self) -> Option<bool> {
        let tab = self.active_tab_mut()?;
        match &mut tab.content {
            ViewerContent::Video(vid) => {
                vid.looping = !vid.looping;
                Some(vid.looping)
            }
            _ => None,
        }
    }

    /// Seeks the active Video tab to a normalized `fraction` in `0.0..=1.0`, returning new seconds.
    pub fn seek_active_video_fraction(&mut self, fraction: f32) -> Option<f64> {
        let tab = self.active_tab_mut()?;
        match &mut tab.content {
            ViewerContent::Video(vid) => Some(vid.seek_to_fraction(fraction)),
            _ => None,
        }
    }

    /// Toggles unified git-diff mode on the active Text/Code tab.
    pub fn toggle_diff_mode(&mut self) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        match &mut tab.content {
            ViewerContent::Text(txt) => txt.toggle_diff_mode(),
            _ => false,
        }
    }

    /// Toggles Play/Pause on the active Audio tab, returning the new `playing` state.
    pub fn toggle_audio_play_pause(&mut self) -> Option<bool> {
        let tab = self.active_tab_mut()?;
        match &mut tab.content {
            ViewerContent::Audio(aud) => {
                aud.playing = !aud.playing;
                Some(aud.playing)
            }
            _ => None,
        }
    }

    /// Toggles Mute/Unmute on the active Audio tab, returning the new `muted` state.
    pub fn toggle_audio_mute(&mut self) -> Option<bool> {
        let tab = self.active_tab_mut()?;
        match &mut tab.content {
            ViewerContent::Audio(aud) => {
                aud.muted = !aud.muted;
                Some(aud.muted)
            }
            _ => None,
        }
    }

    /// Toggles Loop on the active Audio tab, returning the new `looping` state.
    pub fn toggle_audio_loop(&mut self) -> Option<bool> {
        let tab = self.active_tab_mut()?;
        match &mut tab.content {
            ViewerContent::Audio(aud) => {
                aud.looping = !aud.looping;
                Some(aud.looping)
            }
            _ => None,
        }
    }

    /// Seeks the active Audio tab to a normalized `fraction` in `0.0..=1.0`, returning new seconds.
    pub fn seek_active_audio_fraction(&mut self, fraction: f32) -> Option<f64> {
        let tab = self.active_tab_mut()?;
        match &mut tab.content {
            ViewerContent::Audio(aud) => Some(aud.seek_to_fraction(fraction)),
            _ => None,
        }
    }

    /// Opens the in-viewer `Ctrl+F` search bar, optionally pre-seeding from single-line selection.
    pub fn open_search(&mut self) {
        if let Some(tab) = self.active_tab()
            && let ViewerContent::Text(txt) = &tab.content
            && let Some(sel) = txt.selected_text()
            && !sel.contains('\n')
            && sel.len() <= 120
        {
            self.search.input.set_text(sel);
        }
        self.search.open = true;
        self.search.input.select_all();
        self.recompute_search();
    }

    /// Closes the in-viewer `Ctrl+F` search bar.
    pub fn close_search(&mut self) {
        self.search.open = false;
    }

    /// Toggles case sensitivity for in-viewer search and refreshes matches.
    pub fn toggle_search_case(&mut self) {
        self.search.case_sensitive = !self.search.case_sensitive;
        self.recompute_search();
        self.scroll_to_active_search_match();
    }

    /// Recomputes all [`ViewerSearchMatch`]es in the active tab and preserves/clamps `active_match`.
    pub fn recompute_search(&mut self) {
        self.search.matches.clear();
        let query = self.search.input.text().to_owned();
        if query.is_empty() {
            self.search.active_match = None;
            return;
        }

        let Some(tab) = self.tabs.get(self.active_tab) else {
            self.search.active_match = None;
            return;
        };
        let ViewerContent::Text(txt) = &tab.content else {
            self.search.active_match = None;
            return;
        };

        let q_cmp = if self.search.case_sensitive {
            query.clone()
        } else {
            query.to_lowercase()
        };

        let mut matches = Vec::new();
        if !txt.cached_row_map.is_empty() {
            for (row_idx, row) in txt.cached_row_map.iter().enumerate() {
                collect_line_matches(
                    &row.plain_text,
                    &query,
                    &q_cmp,
                    self.search.case_sensitive,
                    row_idx,
                    row.source_line,
                    &mut matches,
                );
            }
        } else {
            for (idx, line) in txt.lines.iter().enumerate() {
                let expanded = syntax::expand_tabs(line);
                collect_line_matches(
                    &expanded,
                    &query,
                    &q_cmp,
                    self.search.case_sensitive,
                    idx,
                    Some(idx + 1),
                    &mut matches,
                );
            }
        }
        self.search.matches = matches;

        if self.search.matches.is_empty() {
            self.search.active_match = None;
        } else {
            let clamped = self
                .search
                .active_match
                .map_or(0, |i| i.min(self.search.matches.len() - 1));
            self.search.active_match = Some(clamped);
        }
    }

    /// Advances to the next search match (wrapping around) and scrolls to it.
    pub fn next_search_match(&mut self) -> bool {
        if self.search.matches.is_empty() {
            self.recompute_search();
        }
        let len = self.search.matches.len();
        if len == 0 {
            self.search.active_match = None;
            return false;
        }
        let next = match self.search.active_match {
            Some(i) => (i + 1) % len,
            None => 0,
        };
        self.search.active_match = Some(next);
        self.scroll_to_active_search_match();
        true
    }

    /// Moves to the previous search match (wrapping around) and scrolls to it.
    pub fn prev_search_match(&mut self) -> bool {
        if self.search.matches.is_empty() {
            self.recompute_search();
        }
        let len = self.search.matches.len();
        if len == 0 {
            self.search.active_match = None;
            return false;
        }
        let prev = match self.search.active_match {
            Some(0) | None => len - 1,
            Some(i) => i - 1,
        };
        self.search.active_match = Some(prev);
        self.scroll_to_active_search_match();
        true
    }

    /// Scrolls the active text tab so the currently selected search match is comfortably visible.
    pub fn scroll_to_active_search_match(&mut self) {
        let Some(m) = self
            .search
            .active_match
            .and_then(|i| self.search.matches.get(i))
            .cloned()
        else {
            return;
        };
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if let ViewerContent::Text(txt) = &mut tab.content {
            if let Some(row) = txt.cached_row_map.get(m.row) {
                let desired = (row.content_y - txt.last_viewport_h * 0.35).max(0.0);
                txt.scroll_y = desired.clamp(0.0, txt.max_scroll());
            } else if let Some(line) = m.source_line {
                txt.jump_to_line(line, None, None);
            }
        }
    }

    /// Feeds back measured viewport rects & scroll heights from the latest [`ViewerRenderMetrics`].
    pub fn sync_render_metrics(&mut self, metrics: &ViewerRenderMetrics) {
        let Some(tab_id) = metrics.active_tab_id else {
            return;
        };
        let mono_cell_w = self.mono_cell_w;
        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return;
        };
        match &mut tab.content {
            ViewerContent::Text(txt) => {
                txt.mono_cell_w = mono_cell_w;
                if metrics.text_viewport_h > 0.0 {
                    txt.last_viewport_h = metrics.text_viewport_h;
                }
                if metrics.text_content_h > 0.0 {
                    txt.last_content_h = metrics.text_content_h;
                }
                if metrics.text_body_rect.is_some() {
                    txt.last_body_rect = metrics.text_body_rect;
                }
                if !metrics.row_map.is_empty() {
                    txt.cached_row_map = metrics.row_map.clone();
                }
                txt.scroll_y = txt.scroll_y.clamp(0.0, txt.max_scroll());
            }
            ViewerContent::Image(img) => {
                img.last_canvas_rect = metrics.canvas_rect;
                if let Some(c) = metrics.canvas_rect {
                    img.viewport.clamp_pan(img.width, img.height, c);
                }
            }
            ViewerContent::Video(vid) => {
                vid.last_canvas_rect = metrics.canvas_rect;
                vid.last_seek_bar_rect = metrics.seek_bar_rect;
                if let Some(c) = metrics.canvas_rect {
                    vid.viewport.clamp_pan(vid.width, vid.height, c);
                }
            }
            ViewerContent::Audio(aud) => {
                aud.last_waveform_rect = metrics.canvas_rect;
                aud.last_seek_bar_rect = metrics.seek_bar_rect;
            }
            ViewerContent::Pdf(pdf) => {
                pdf.last_canvas_rect = metrics.canvas_rect;
                if let Some(c) = metrics.canvas_rect
                    && pdf.page_width > 0
                    && pdf.page_height > 0
                {
                    pdf.viewport.clamp_pan(pdf.page_width, pdf.page_height, c);
                }
            }
            ViewerContent::Loading { .. } | ViewerContent::Error { .. } => {}
        }
    }
}

fn collect_line_matches(
    haystack: &str,
    raw_query: &str,
    cmp_query: &str,
    case_sensitive: bool,
    row: usize,
    source_line: Option<usize>,
    out: &mut Vec<ViewerSearchMatch>,
) {
    if haystack.is_empty() || raw_query.is_empty() || cmp_query.is_empty() {
        return;
    }
    if case_sensitive {
        let mut from = 0usize;
        while let Some(rel) = haystack[from..].find(raw_query) {
            let start = from + rel;
            let end = start + raw_query.len();
            out.push(ViewerSearchMatch {
                row,
                start_byte: start,
                end_byte: end,
                source_line,
            });
            from = end;
        }
    } else {
        // Build `lower` per-character with a byte-range mapping table (`lower_byte -> orig_byte_range`)
        // so expanding Unicode lowercase characters (e.g. `'İ'` -> `"i\u{307}"`) always map back
        // to exact UTF-8 boundaries in `haystack`.
        let mut lower = String::with_capacity(haystack.len());
        let mut orig_start_at_lower_byte = Vec::with_capacity(haystack.len());
        let mut orig_end_at_lower_byte = Vec::with_capacity(haystack.len());
        for (orig_start, ch) in haystack.char_indices() {
            let orig_end = orig_start + ch.len_utf8();
            let before = lower.len();
            for lc in ch.to_lowercase() {
                lower.push(lc);
            }
            let added = lower.len() - before;
            for _ in 0..added {
                orig_start_at_lower_byte.push(orig_start);
                orig_end_at_lower_byte.push(orig_end);
            }
        }
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(cmp_query) {
            let l_start = from + rel;
            let l_end = l_start + cmp_query.len();
            if l_end > l_start
                && let Some(&start_byte) = orig_start_at_lower_byte.get(l_start)
                && let Some(&end_byte) = orig_end_at_lower_byte.get(l_end - 1)
            {
                out.push(ViewerSearchMatch {
                    row,
                    start_byte,
                    end_byte,
                    source_line,
                });
            }
            from = l_end;
        }
    }
}

/// Exact GPU image/video placement emitted by [`render_viewer`] so `omarex` / `gpu-renderer`
/// can submit `ImageQuad` instances without re-deriving zoom/pan/clip math.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewerImagePlacement {
    pub texture_key: u64,
    pub dest_rect: RectF,
    pub uv_bounds: [f32; 4],
    pub clip_rect: RectF,
    pub corner_radius: f32,
    pub opacity: f32,
    /// Hint to use nearest-neighbor sampling when zoomed past `200%` for crisp pixel inspection.
    pub pixel_crisp: bool,
}

/// Layout metrics measured during [`render_viewer`] that can be synced back into [`ViewerState`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ViewerRenderMetrics {
    pub active_tab_id: Option<u64>,
    pub canvas_rect: Option<RectF>,
    pub seek_bar_rect: Option<RectF>,
    pub text_body_rect: Option<RectF>,
    pub breadcrumb_path_rect: Option<RectF>,
    pub text_viewport_h: f32,
    pub text_content_h: f32,
    pub row_map: Vec<RenderedRowMap>,
}

/// Combined image placements and layout metrics returned by [`render_viewer`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ViewerRenderOutput {
    pub images: Vec<ViewerImagePlacement>,
    pub metrics: ViewerRenderMetrics,
}

/// Renders the docked right-hand Viewer Pane (divider, card shell, tab bar, breadcrumb header,
/// and active content renderer) into `draw_list`.
#[allow(clippy::too_many_arguments)]
pub fn render_viewer(
    draw_list: &mut UiDrawList,
    split: ViewerSplitGeometry,
    state: &ViewerState,
    focused: bool,
    highlight_active_border: bool,
    active_border_opacity: f32,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    anim_phase: f32,
) -> ViewerRenderOutput {
    render_viewer_with_measurer(
        draw_list,
        split,
        state,
        focused,
        highlight_active_border,
        active_border_opacity,
        interaction,
        theme,
        anim_phase,
        None,
    )
}

/// Same as [`render_viewer`], using `measurer` for exact proportional link hit-rects & underlines.
#[allow(clippy::too_many_arguments)]
pub fn render_viewer_with_measurer(
    draw_list: &mut UiDrawList,
    split: ViewerSplitGeometry,
    state: &ViewerState,
    focused: bool,
    highlight_active_border: bool,
    active_border_opacity: f32,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    anim_phase: f32,
    mut measurer: Option<&mut dyn UiTextMeasurer>,
) -> ViewerRenderOutput {
    let mut output = ViewerRenderOutput::default();
    if split.viewer_rect.is_empty() {
        return output;
    }

    let tokens = theme.effective_ui();

    // 1. Draggable vertical divider between terminal workspace and right-hand viewer pane
    if let (Some(vis), Some(hit)) = (split.divider_visual_rect, split.divider_hit_rect) {
        draw_list.push_hit(hit, HitTarget::ViewerDivider, z_layer::PANE_CHROME + 2);
        let div_pressed = interaction.is_pressed(&HitTarget::ViewerDivider);
        let div_hovered = interaction.is_hovered(&HitTarget::ViewerDivider);
        if div_pressed || div_hovered {
            let alpha = if div_pressed { 0.95 } else { 0.72 };
            let pill = RectF::new(
                vis.x + (vis.w - 2.0) * 0.5,
                vis.y + 12.0,
                2.0,
                (vis.h - 24.0).max(0.0),
            );
            draw_list.push_rect(
                pill,
                tokens.accent_primary.with_alpha_f32(alpha),
                None,
                0.0,
                1.0,
                None,
            );
        } else {
            let hairline = RectF::new(vis.x + (vis.w - 1.0) * 0.5, vis.y, 1.0, vis.h);
            draw_list.push_rect(hairline, tokens.border_hairline, None, 0.0, 0.0, None);
        }
    }

    // 2. Outer viewer card shell
    let card = split.viewer_rect;
    let radius = cap_radius(split.corner_radius, theme.rounding);
    let (border_color, border_w) = if focused && highlight_active_border {
        (
            Some(tokens.border_active.with_alpha_f32(active_border_opacity)),
            1.5,
        )
    } else {
        (Some(tokens.border_pane), 1.0)
    };
    draw_list.push_rect(
        card,
        tokens.surface_pane,
        border_color,
        border_w,
        radius,
        None,
    );
    draw_list.push_hit(card, HitTarget::ViewerSurface, z_layer::PANE_CHROME);

    draw_list.push_clip(card);

    // 3. Top tab bar (`34px`)
    let tab_bar_h = TAB_BAR_HEIGHT.min(card.h);
    let tab_bar = RectF::new(card.x, card.y, card.w, tab_bar_h);
    render_viewer_tab_bar(draw_list, tab_bar, state, interaction, theme, &tokens);

    // 4. Subtle breadcrumb bar (`26px`)
    let crumb_y = tab_bar.bottom();
    let crumb_h = BREADCRUMB_HEIGHT.min((card.bottom() - crumb_y).max(0.0));
    let crumb_rect = RectF::new(card.x, crumb_y, card.w, crumb_h);
    let active_tab = state.active_tab();
    output.metrics.active_tab_id = active_tab.map(|t| t.id);

    if let Some(tab) = active_tab {
        output.metrics.breadcrumb_path_rect =
            render_viewer_breadcrumb(draw_list, crumb_rect, tab, interaction, theme, &tokens);
    }

    // 5. Active tab content body
    let body_y = crumb_rect.bottom();
    let body = RectF::new(card.x, body_y, card.w, (card.bottom() - body_y).max(0.0));
    if !body.is_empty() {
        match active_tab {
            None => render_empty_viewer(draw_list, body, &tokens),
            Some(tab) => match &tab.content {
                ViewerContent::Loading { label } => {
                    render_loading_content(draw_list, body, label, anim_phase, &tokens, theme);
                }
                ViewerContent::Error { message } => {
                    render_error_content(draw_list, body, message, interaction, &tokens, theme);
                }
                ViewerContent::Text(txt) => {
                    let text_out = render_text_content(
                        draw_list,
                        body,
                        txt,
                        state.mono_cell_w,
                        &state.search,
                        interaction,
                        theme,
                        &tokens,
                        &mut measurer,
                    );
                    output.metrics.text_viewport_h = text_out.viewport_h;
                    output.metrics.text_content_h = text_out.content_h;
                    output.metrics.text_body_rect = Some(body);
                    output.metrics.row_map = text_out.row_map;
                    output.images.extend(text_out.inline_images);
                }
                ViewerContent::Image(img) => {
                    let (canvas, placement) =
                        render_image_content(draw_list, body, img, interaction, theme, &tokens);
                    output.metrics.canvas_rect = Some(canvas);
                    if let Some(p) = placement {
                        output.images.push(p);
                    }
                }
                ViewerContent::Video(vid) => {
                    let (canvas, seek_bar, placement) =
                        render_video_content(draw_list, body, vid, interaction, theme, &tokens);
                    output.metrics.canvas_rect = Some(canvas);
                    output.metrics.seek_bar_rect = Some(seek_bar);
                    if let Some(p) = placement {
                        output.images.push(p);
                    }
                }
                ViewerContent::Audio(aud) => {
                    let (wave_rect, seek_bar) = render_audio_content(
                        draw_list,
                        body,
                        tab,
                        aud,
                        interaction,
                        theme,
                        &tokens,
                    );
                    output.metrics.canvas_rect = Some(wave_rect);
                    output.metrics.seek_bar_rect = Some(seek_bar);
                }
                ViewerContent::Pdf(pdf) => {
                    let (canvas, placement) =
                        render_pdf_content(draw_list, body, pdf, interaction, theme, &tokens);
                    output.metrics.canvas_rect = Some(canvas);
                    if let Some(p) = placement {
                        output.images.push(p);
                    }
                }
            },
        }

        // 6. In-viewer `Ctrl+F` search bar overlay (anchored top-right inside `body`)
        if state.search.open {
            render_viewer_search_bar(draw_list, body, &state.search, interaction, theme, &tokens);
        }
    }

    draw_list.pop_clip();
    output
}

fn render_viewer_tab_bar(
    draw_list: &mut UiDrawList,
    bar: RectF,
    state: &ViewerState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) {
    if bar.is_empty() {
        return;
    }
    let z = z_layer::PANE_CHROME + 2;
    let btn_r = cap_radius(5.0, theme.rounding);

    draw_list.push_rect(bar, tokens.surface_pane_header, None, 0.0, 0.0, None);
    draw_list.push_rect(
        RectF::new(bar.x, (bar.bottom() - 1.0).max(bar.y), bar.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    // Right-hand action buttons: [Copy Path] [Open External] [Maximize/Restore] [Close]
    let btn_size = 24.0;
    let btn_gap = 3.0;
    let btn_y = bar.y + (bar.h - btn_size) * 0.5;
    let has_tab = state.active_tab().is_some();

    let mut right_x = bar.right() - 6.0;

    let close_rect = RectF::new((right_x - btn_size).max(bar.x), btn_y, btn_size, btn_size);
    right_x = close_rect.x - btn_gap;
    let (close_surf, close_tint) =
        interaction.danger_button_style(&HitTarget::ViewerClose, tokens.fg_secondary, tokens);
    if let Some((fill, border)) = close_surf {
        draw_list.push_rect(close_rect, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(close_rect.inset_uniform(5.0), IconId::Close, close_tint);
    draw_list.push_hit(close_rect, HitTarget::ViewerClose, z + 1);

    let max_rect = RectF::new((right_x - btn_size).max(bar.x), btn_y, btn_size, btn_size);
    right_x = max_rect.x - btn_gap;
    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::ViewerMaximizeToggle, state.maximized, tokens)
    {
        draw_list.push_rect(max_rect, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(
        max_rect.inset_uniform(5.0),
        if state.maximized {
            IconId::Dock
        } else {
            IconId::Zoom
        },
        if state.maximized {
            tokens.accent_primary
        } else if interaction.is_hovered(&HitTarget::ViewerMaximizeToggle) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    draw_list.push_hit(max_rect, HitTarget::ViewerMaximizeToggle, z + 1);

    if has_tab {
        let ext_rect = RectF::new((right_x - btn_size).max(bar.x), btn_y, btn_size, btn_size);
        right_x = ext_rect.x - btn_gap;
        if let Some((fill, border)) =
            interaction.button_surface(&HitTarget::ViewerOpenExternal, false, tokens)
        {
            draw_list.push_rect(ext_rect, fill, border, 1.0, btn_r, None);
        }
        draw_list.push_icon(
            ext_rect.inset_uniform(5.0),
            IconId::Float,
            if interaction.is_hovered(&HitTarget::ViewerOpenExternal) {
                tokens.fg_primary
            } else {
                tokens.fg_secondary
            },
        );
        draw_list.push_hit(ext_rect, HitTarget::ViewerOpenExternal, z + 1);

        let copy_rect = RectF::new((right_x - btn_size).max(bar.x), btn_y, btn_size, btn_size);
        right_x = copy_rect.x - btn_gap;
        if let Some((fill, border)) =
            interaction.button_surface(&HitTarget::ViewerCopyPath, false, tokens)
        {
            draw_list.push_rect(copy_rect, fill, border, 1.0, btn_r, None);
        }
        draw_list.push_icon(
            copy_rect.inset_uniform(5.0),
            IconId::Link,
            if interaction.is_hovered(&HitTarget::ViewerCopyPath) {
                tokens.fg_primary
            } else {
                tokens.fg_secondary
            },
        );
        draw_list.push_hit(copy_rect, HitTarget::ViewerCopyPath, z + 1);
    }

    // Left tab strip
    let strip = RectF::new(bar.x + 6.0, bar.y, (right_x - bar.x - 10.0).max(0.0), bar.h);
    if strip.is_empty() || state.tabs.is_empty() {
        return;
    }

    draw_list.push_clip(strip);
    let count = state.tabs.len();
    let tab_gap = 4.0;
    let ideal_w =
        ((strip.w - tab_gap * count.saturating_sub(1) as f32) / count as f32).clamp(96.0, 210.0);
    let pill_r = cap_radius(6.0, theme.rounding);

    let mut x = strip.x;
    for (idx, tab) in state.tabs.iter().enumerate() {
        if x > strip.right() {
            break;
        }
        let is_active = idx == state.active_tab;
        let tab_rect = RectF::new(
            x,
            bar.y + 4.0,
            ideal_w.min((strip.right() - x).max(0.0)),
            bar.h - 8.0,
        );
        if tab_rect.w < 36.0 {
            break;
        }

        let tab_hit = HitTarget::ViewerTab { tab_id: tab.id };
        let close_hit = HitTarget::ViewerTabClose { tab_id: tab.id };
        let hovered = interaction.is_hovered(&tab_hit) || interaction.is_hovered(&close_hit);

        let (fill, border) = if is_active {
            (tokens.surface_tab_active, Some(tokens.border_subtle))
        } else if hovered {
            (tokens.surface_row_hover, Some(tokens.border_hairline))
        } else {
            (tokens.surface_pane_header, None)
        };
        draw_list.push_rect(tab_rect, fill, border, 1.0, pill_r, None);

        if is_active {
            draw_list.push_rect(
                RectF::new(
                    tab_rect.x + 6.0,
                    tab_rect.bottom() - 2.0,
                    (tab_rect.w - 12.0).max(0.0),
                    2.0,
                ),
                tokens.accent_primary,
                None,
                0.0,
                1.0,
                None,
            );
        }

        // File-type icon + compact badge
        let badge_col = tab.kind.badge_color(theme, tokens);
        let badge_w = if tab.kind.badge_label().len() >= 4 {
            32.0
        } else {
            26.0
        };
        let badge_rect = RectF::new(
            tab_rect.x + 6.0,
            tab_rect.y + 5.0,
            badge_w,
            (tab_rect.h - 10.0).max(12.0),
        );
        draw_list.push_rect(
            badge_rect,
            badge_col.with_alpha_f32(0.18),
            Some(badge_col.with_alpha_f32(0.38)),
            1.0,
            cap_radius(4.0, theme.rounding),
            None,
        );
        draw_list.push_text(
            badge_rect,
            tab.kind.badge_label(),
            badge_col,
            FontKind::Mono,
            9.5,
            FontWeight::SemiBold,
            TextAlign::Center,
            false,
        );

        // Close `×` button on right of tab pill
        let tab_close_rect = RectF::new(
            (tab_rect.right() - 20.0).max(tab_rect.x),
            tab_rect.y + (tab_rect.h - 16.0) * 0.5,
            16.0,
            16.0,
        );
        let title_x = badge_rect.right() + 6.0;
        let title_w = (tab_close_rect.x - title_x - 4.0).max(0.0);
        draw_list.push_text(
            RectF::new(
                title_x,
                tab_rect.y + 4.0,
                title_w,
                (tab_rect.h - 8.0).max(12.0),
            ),
            &tab.title,
            if is_active {
                tokens.fg_primary
            } else {
                tokens.fg_secondary
            },
            FontKind::Sans,
            11.5,
            if is_active {
                FontWeight::SemiBold
            } else {
                FontWeight::Regular
            },
            TextAlign::Left,
            true,
        );

        draw_list.push_hit(tab_rect, tab_hit, z);

        let (c_surf, c_tint) = interaction.danger_button_style(
            &close_hit,
            if is_active || hovered {
                tokens.fg_secondary
            } else {
                tokens.fg_muted
            },
            tokens,
        );
        if let Some((cf, cb)) = c_surf {
            draw_list.push_rect(
                tab_close_rect,
                cf,
                cb,
                1.0,
                cap_radius(4.0, theme.rounding),
                None,
            );
        }
        draw_list.push_icon(tab_close_rect.inset_uniform(3.0), IconId::Close, c_tint);
        draw_list.push_hit(tab_close_rect, close_hit, z + 1);

        x += tab_rect.w + tab_gap;
    }
    draw_list.pop_clip();
}

fn render_viewer_breadcrumb(
    draw_list: &mut UiDrawList,
    bar: RectF,
    tab: &ViewerTab,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) -> Option<RectF> {
    if bar.is_empty() {
        return None;
    }
    let z = z_layer::PANE_CHROME + 2;
    let pill_r = cap_radius(4.0, theme.rounding);

    draw_list.push_rect(
        bar,
        tokens.surface_pane_header.with_alpha_f32(0.55),
        None,
        0.0,
        0.0,
        None,
    );
    draw_list.push_rect(
        RectF::new(bar.x, (bar.bottom() - 1.0).max(bar.y), bar.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    // Right-side contextual controls for Text/Markdown tabs or Loading/Error status
    let mut right_x = bar.right() - 8.0;
    match &tab.content {
        ViewerContent::Text(txt) => {
            // [L:N] copy path:line pill
            let active_line = txt.target_or_active_line();
            let line_label = format!("L:{active_line}");
            let line_w = (line_label.len() as f32 * 6.5 + 14.0).clamp(38.0, 72.0);
            let line_rect = RectF::new(
                (right_x - line_w).max(bar.x),
                bar.y + 4.0,
                line_w,
                (bar.h - 8.0).max(14.0),
            );
            right_x = line_rect.x - 4.0;
            let line_hit = HitTarget::ViewerCopyPathLine;
            let (lfill, lborder) = interaction
                .button_surface(&line_hit, false, tokens)
                .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
            draw_list.push_rect(line_rect, lfill, lborder, 1.0, pill_r, None);
            draw_list.push_text(
                line_rect,
                line_label,
                if interaction.is_hovered(&line_hit) {
                    tokens.fg_primary
                } else {
                    tokens.fg_secondary
                },
                FontKind::Mono,
                10.0,
                FontWeight::Medium,
                TextAlign::Center,
                false,
            );
            draw_list.push_hit(line_rect, line_hit, z);

            // [Wrap] pill
            let wrap_w = 42.0;
            let wrap_rect = RectF::new(
                (right_x - wrap_w).max(bar.x),
                bar.y + 4.0,
                wrap_w,
                (bar.h - 8.0).max(14.0),
            );
            right_x = wrap_rect.x - 4.0;
            let wrap_hit = HitTarget::ViewerWrapToggle;
            let (fill, border) = interaction
                .button_surface(&wrap_hit, txt.word_wrap, tokens)
                .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
            draw_list.push_rect(wrap_rect, fill, border, 1.0, pill_r, None);
            draw_list.push_text(
                wrap_rect,
                "Wrap",
                if txt.word_wrap {
                    tokens.accent_primary
                } else {
                    tokens.fg_secondary
                },
                FontKind::Sans,
                10.5,
                FontWeight::Medium,
                TextAlign::Center,
                false,
            );
            draw_list.push_hit(wrap_rect, wrap_hit, z);

            // [Rendered / Raw] pill for Markdown
            if txt.is_markdown {
                let raw_label = if txt.raw_markdown { "Raw" } else { "Preview" };
                let raw_w = 56.0;
                let raw_rect = RectF::new(
                    (right_x - raw_w).max(bar.x),
                    bar.y + 4.0,
                    raw_w,
                    (bar.h - 8.0).max(14.0),
                );
                right_x = raw_rect.x - 4.0;
                let raw_hit = HitTarget::ViewerRawToggle;
                let (rfill, rborder) = interaction
                    .button_surface(&raw_hit, !txt.raw_markdown, tokens)
                    .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
                draw_list.push_rect(raw_rect, rfill, rborder, 1.0, pill_r, None);
                draw_list.push_text(
                    raw_rect,
                    raw_label,
                    if !txt.raw_markdown {
                        tokens.accent_primary
                    } else {
                        tokens.fg_secondary
                    },
                    FontKind::Sans,
                    10.5,
                    FontWeight::Medium,
                    TextAlign::Center,
                    false,
                );
                draw_list.push_hit(raw_rect, raw_hit, z);
            }

            // [Diff +A -R] toggle pill or [Untracked] badge when git diff metadata is present
            if let Some(diff) = &txt.git_diff {
                if diff.has_staged || diff.has_unstaged {
                    let kind_label = if diff.has_staged && !diff.has_unstaged {
                        "Staged"
                    } else {
                        "Diff"
                    };
                    let diff_label =
                        format!("{kind_label} +{} -{}", diff.added_count, diff.removed_count);
                    let diff_w = (diff_label.len() as f32 * 6.3 + 14.0).clamp(62.0, 128.0);
                    let diff_rect = RectF::new(
                        (right_x - diff_w).max(bar.x),
                        bar.y + 4.0,
                        diff_w,
                        (bar.h - 8.0).max(14.0),
                    );
                    right_x = diff_rect.x - 4.0;
                    let diff_hit = HitTarget::ViewerDiffToggle;
                    let (dfill, dborder) = interaction
                        .button_surface(&diff_hit, txt.diff_mode, tokens)
                        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
                    draw_list.push_rect(diff_rect, dfill, dborder, 1.0, pill_r, None);
                    draw_list.push_text(
                        diff_rect,
                        diff_label,
                        if txt.diff_mode {
                            tokens.accent_primary
                        } else {
                            tokens.fg_secondary
                        },
                        FontKind::Mono,
                        10.0,
                        FontWeight::SemiBold,
                        TextAlign::Center,
                        false,
                    );
                    draw_list.push_hit(diff_rect, diff_hit, z);
                } else if diff.is_untracked {
                    let untrack_w = 64.0;
                    let untrack_rect = RectF::new(
                        (right_x - untrack_w).max(bar.x),
                        bar.y + 4.0,
                        untrack_w,
                        (bar.h - 8.0).max(14.0),
                    );
                    right_x = untrack_rect.x - 4.0;
                    draw_list.push_rect(
                        untrack_rect,
                        tokens.surface_row_hover,
                        Some(tokens.border_hairline),
                        1.0,
                        pill_r,
                        None,
                    );
                    draw_list.push_text(
                        untrack_rect,
                        "Untracked",
                        tokens.fg_muted,
                        FontKind::Sans,
                        10.0,
                        FontWeight::Medium,
                        TextAlign::Center,
                        false,
                    );
                }
            }
        }
        ViewerContent::Loading { .. } => {
            let badge = RectF::new(
                (right_x - 64.0).max(bar.x),
                bar.y + 4.0,
                64.0,
                (bar.h - 8.0).max(14.0),
            );
            right_x = badge.x - 6.0;
            draw_list.push_text(
                badge,
                "Loading…",
                tokens.status_working,
                FontKind::Sans,
                10.5,
                FontWeight::Medium,
                TextAlign::Right,
                false,
            );
        }
        ViewerContent::Error { .. } => {
            let badge = RectF::new(
                (right_x - 52.0).max(bar.x),
                bar.y + 4.0,
                52.0,
                (bar.h - 8.0).max(14.0),
            );
            right_x = badge.x - 6.0;
            draw_list.push_text(
                badge,
                "Error",
                tokens.status_error,
                FontKind::Sans,
                10.5,
                FontWeight::SemiBold,
                TextAlign::Right,
                false,
            );
        }
        ViewerContent::Image(_)
        | ViewerContent::Video(_)
        | ViewerContent::Audio(_)
        | ViewerContent::Pdf(_) => {}
    }

    // Left: optional remote host pill + full path
    let mut left_x = bar.x + 8.0;
    if let Some(host) = tab.remote_host.as_deref()
        && !host.is_empty()
    {
        let host_w = (host.chars().count() as f32 * 6.5 + 24.0).clamp(48.0, 128.0);
        let host_rect = RectF::new(left_x, bar.y + 4.0, host_w, (bar.h - 8.0).max(14.0));
        draw_list.push_rect(
            host_rect,
            tokens.accent_primary.with_alpha_f32(0.16),
            Some(tokens.accent_primary.with_alpha_f32(0.35)),
            1.0,
            pill_r,
            None,
        );
        draw_list.push_icon(
            RectF::new(host_rect.x + 4.0, host_rect.y + 3.0, 11.0, 11.0),
            IconId::Server,
            tokens.accent_primary,
        );
        draw_list.push_text(
            RectF::new(
                host_rect.x + 18.0,
                host_rect.y,
                (host_rect.w - 22.0).max(0.0),
                host_rect.h,
            ),
            host,
            tokens.accent_primary,
            FontKind::Sans,
            10.5,
            FontWeight::SemiBold,
            TextAlign::Left,
            true,
        );
        left_x = host_rect.right() + 8.0;
    }

    let path_w = (right_x - left_x - 4.0).max(0.0);
    if path_w > 12.0 {
        let path_rect = RectF::new(left_x, bar.y + 4.0, path_w, (bar.h - 8.0).max(14.0));
        draw_list.push_text(
            path_rect,
            &tab.path,
            tokens.fg_muted,
            FontKind::Mono,
            11.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        Some(path_rect)
    } else {
        None
    }
}

fn render_empty_viewer(draw_list: &mut UiDrawList, body: RectF, tokens: &UiTokens) {
    let msg_rect = RectF::new(
        body.x + 24.0,
        body.y + body.h * 0.45,
        (body.w - 48.0).max(0.0),
        24.0,
    );
    draw_list.push_text(
        msg_rect,
        "No file open in Viewer",
        tokens.fg_muted,
        FontKind::Sans,
        13.0,
        FontWeight::Medium,
        TextAlign::Center,
        true,
    );
}

fn render_loading_content(
    draw_list: &mut UiDrawList,
    body: RectF,
    label: &str,
    anim_phase: f32,
    tokens: &UiTokens,
    theme: &Theme,
) {
    let card_w = (body.w - 48.0).clamp(160.0, 320.0);
    let card_h = 92.0_f32.min(body.h);
    let card = RectF::new(
        body.x + (body.w - card_w) * 0.5,
        body.y + (body.h - card_h) * 0.45,
        card_w,
        card_h,
    );
    draw_list.push_rect(
        card,
        tokens.surface_pane_header,
        Some(tokens.border_subtle),
        1.0,
        cap_radius(8.0, theme.rounding),
        None,
    );
    draw_list.push_icon(
        RectF::new(card.x + (card.w - 20.0) * 0.5, card.y + 14.0, 20.0, 20.0),
        IconId::Clock,
        tokens.accent_primary,
    );
    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 40.0, (card.w - 28.0).max(0.0), 18.0),
        label,
        tokens.fg_secondary,
        FontKind::Sans,
        12.0,
        FontWeight::Medium,
        TextAlign::Center,
        true,
    );
    let progress = ((anim_phase.fract() + 1.0).fract()).clamp(0.08, 0.95);
    draw_list.push_progress(
        RectF::new(card.x + 24.0, card.y + 68.0, (card.w - 48.0).max(0.0), 4.0),
        progress,
        tokens.accent_primary,
        tokens.surface_row_active,
        2.0,
    );
}

fn render_error_content(
    draw_list: &mut UiDrawList,
    body: RectF,
    message: &str,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme: &Theme,
) {
    let card_w = (body.w - 48.0).clamp(180.0, 380.0);
    let card_h = 128.0_f32.min(body.h);
    let card = RectF::new(
        body.x + (body.w - card_w) * 0.5,
        body.y + (body.h - card_h) * 0.45,
        card_w,
        card_h,
    );
    draw_list.push_rect(
        card,
        tokens.surface_pane_header,
        Some(tokens.status_error.with_alpha_f32(0.45)),
        1.0,
        cap_radius(8.0, theme.rounding),
        None,
    );
    draw_list.push_icon(
        RectF::new(card.x + (card.w - 22.0) * 0.5, card.y + 14.0, 22.0, 22.0),
        IconId::Error,
        tokens.status_error,
    );
    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 42.0, (card.w - 28.0).max(0.0), 18.0),
        "Unable to preview file",
        tokens.fg_primary,
        FontKind::Sans,
        12.5,
        FontWeight::SemiBold,
        TextAlign::Center,
        true,
    );
    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 64.0, (card.w - 28.0).max(0.0), 20.0),
        message,
        tokens.fg_muted,
        FontKind::Mono,
        11.0,
        FontWeight::Regular,
        TextAlign::Center,
        true,
    );
    let btn = RectF::new(card.x + (card.w - 118.0) * 0.5, card.y + 92.0, 118.0, 24.0);
    let (bfill, bborder) = interaction
        .button_surface(&HitTarget::ViewerOpenExternal, false, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_subtle)));
    draw_list.push_rect(
        btn,
        bfill,
        bborder,
        1.0,
        cap_radius(5.0, theme.rounding),
        None,
    );
    draw_list.push_text(
        btn,
        "Open External",
        tokens.fg_primary,
        FontKind::Sans,
        11.5,
        FontWeight::Medium,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(btn, HitTarget::ViewerOpenExternal, z_layer::PANE_CHROME + 2);
}

#[derive(Debug, Clone)]
enum TextRenderRow {
    CodeLine {
        line_no: Option<usize>,
        text: String,
        tokens: Vec<SyntaxToken>,
        lang_badge: Option<String>,
        is_target: bool,
        in_code_block: bool,
    },
    DiffHunkHeader {
        text: String,
    },
    DiffCodeLine {
        old_line: Option<usize>,
        new_line: Option<usize>,
        kind: DiffLineKind,
        text: String,
        tokens: Vec<SyntaxToken>,
        is_target: bool,
    },
    MarkdownHeading {
        level: u8,
        text: String,
        is_target: bool,
    },
    MarkdownQuote {
        text: String,
        is_target: bool,
    },
    MarkdownListItem {
        marker: String,
        text: String,
        indent: f32,
        is_target: bool,
    },
    MarkdownRule,
    MarkdownTableRow {
        cells: Vec<String>,
        alignments: Vec<TextAlign>,
        col_weights: Vec<usize>,
        is_header: bool,
        is_even_row: bool,
        is_target: bool,
    },
    MarkdownImage {
        alt: String,
        src: String,
        is_target: bool,
    },
    MarkdownParagraph {
        text: String,
        is_target: bool,
    },
}

struct TextRenderOutput {
    viewport_h: f32,
    content_h: f32,
    row_map: Vec<RenderedRowMap>,
    inline_images: Vec<ViewerImagePlacement>,
}

#[allow(clippy::too_many_arguments)]
fn render_text_content(
    draw_list: &mut UiDrawList,
    body: RectF,
    txt: &TextViewerState,
    mono_cell_w: f32,
    search: &ViewerSearchState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
    measurer: &mut Option<&mut dyn UiTextMeasurer>,
) -> TextRenderOutput {
    let scrollbar_reserve = 12.0;
    let formatted_md = txt.is_markdown && !txt.raw_markdown && !txt.diff_mode;
    let effective_cell_w = if mono_cell_w > 1.0 {
        mono_cell_w
    } else {
        txt.mono_cell_w.max(4.0)
    };
    let gutter_w = if formatted_md {
        0.0
    } else if txt.diff_mode {
        let digits = txt.lines.len().max(1).to_string().len().max(3);
        (digits as f32 * 14.0 + 30.0).clamp(64.0, 104.0)
    } else {
        let digits = txt.lines.len().max(1).to_string().len();
        (digits as f32 * 8.0 + 26.0).clamp(44.0, 76.0)
    };
    let text_x = body.x + gutter_w + 10.0;
    let text_w = (body.w - gutter_w - scrollbar_reserve - 16.0).max(24.0);
    let max_chars = ((text_w / effective_cell_w).floor() as usize).max(16);

    let rows = build_text_rows(txt, formatted_md, max_chars);

    // Precompute exact variable row heights, Y offsets, and RenderedRowMap for selection/search.
    let max_img_w = (body.w - scrollbar_reserve - 32.0).max(64.0);
    let mut row_map = Vec::with_capacity(rows.len());
    let mut y_cursor = 10.0f32;
    for row in &rows {
        let (row_h, row_text_x, plain_text, source_line, is_mono, cell_w) = match row {
            TextRenderRow::CodeLine {
                line_no,
                text,
                in_code_block,
                ..
            } => {
                let rx = if *in_code_block && formatted_md {
                    body.x + 22.0
                } else {
                    text_x
                };
                (
                    TEXT_LINE_HEIGHT,
                    rx,
                    text.clone(),
                    *line_no,
                    true,
                    effective_cell_w,
                )
            }
            TextRenderRow::DiffHunkHeader { text } => (
                TEXT_LINE_HEIGHT + 2.0,
                body.x + 12.0,
                text.clone(),
                None,
                true,
                effective_cell_w,
            ),
            TextRenderRow::DiffCodeLine {
                new_line,
                old_line,
                text,
                ..
            } => (
                TEXT_LINE_HEIGHT,
                text_x,
                text.clone(),
                (*new_line).or(*old_line),
                true,
                effective_cell_w,
            ),
            TextRenderRow::MarkdownHeading { level, text, .. } => {
                let sz = match level {
                    1 => 16.0,
                    2 => 14.5,
                    _ => 13.0,
                };
                let (clean, _) = extract_markdown_inline_spans(text);
                let h = if *level <= 2 { 26.0 } else { 22.0 };
                (h, body.x + 14.0, clean, None, false, sz * 0.52)
            }
            TextRenderRow::MarkdownQuote { text, .. } => {
                let (clean, _) = extract_markdown_inline_spans(text);
                (
                    TEXT_LINE_HEIGHT,
                    body.x + 26.0,
                    clean,
                    None,
                    false,
                    12.5 * 0.52,
                )
            }
            TextRenderRow::MarkdownListItem { text, indent, .. } => {
                let (clean, _) = extract_markdown_inline_spans(text);
                (
                    TEXT_LINE_HEIGHT,
                    body.x + 32.0 + *indent,
                    clean,
                    None,
                    false,
                    12.5 * 0.52,
                )
            }
            TextRenderRow::MarkdownRule => (
                TEXT_LINE_HEIGHT,
                body.x + 14.0,
                String::new(),
                None,
                false,
                6.5,
            ),
            TextRenderRow::MarkdownTableRow { cells, .. } => (
                TEXT_LINE_HEIGHT + 4.0,
                body.x + 14.0,
                cells.join(" | "),
                None,
                false,
                12.0 * 0.52,
            ),
            TextRenderRow::MarkdownImage { alt, src, .. } => {
                let h = match txt.inline_images.get(src) {
                    Some(MarkdownInlineImage::Loaded { width, height, .. })
                        if *width > 0 && *height > 0 =>
                    {
                        let scale = (max_img_w / *width as f32)
                            .min(280.0 / *height as f32)
                            .min(1.0);
                        (*height as f32 * scale).max(24.0) + 14.0
                    }
                    _ => 28.0,
                };
                (h, body.x + 16.0, alt.clone(), None, false, 6.5)
            }
            TextRenderRow::MarkdownParagraph { text, .. } => {
                let (clean, _) = extract_markdown_inline_spans(text);
                (
                    TEXT_LINE_HEIGHT,
                    body.x + 14.0,
                    clean,
                    None,
                    false,
                    12.5 * 0.52,
                )
            }
        };
        row_map.push(RenderedRowMap {
            content_y: y_cursor,
            height: row_h,
            text_x: row_text_x,
            cell_w,
            plain_text,
            source_line,
            is_mono,
        });
        y_cursor += row_h;
    }

    let content_h = (y_cursor + 16.0).max(body.h);
    let max_scroll = (content_h - body.h).max(0.0);
    let scroll_y = txt.scroll_y.clamp(0.0, max_scroll);

    draw_list.push_clip(body);

    // Hit target for text selection & I-beam cursor across body (excluding scrollbar)
    draw_list.push_hit(
        RectF::new(
            body.x,
            body.y,
            (body.w - scrollbar_reserve).max(0.0),
            body.h,
        ),
        HitTarget::ViewerTextBody,
        z_layer::PANE_CHROME + 1,
    );

    // Subtle gutter background & vertical separator for code/raw/diff views
    if !formatted_md {
        draw_list.push_rect(
            RectF::new(body.x, body.y, gutter_w, body.h),
            tokens.surface_pane_header.with_alpha_f32(0.35),
            None,
            0.0,
            0.0,
            None,
        );
        draw_list.push_rect(
            RectF::new(body.x + gutter_w, body.y, 1.0, body.h),
            tokens.border_hairline,
            None,
            0.0,
            0.0,
            None,
        );
    }

    let ansi = &theme.palette.terminal.ansi;
    let color_heading = tokens.accent_primary;
    let color_inline_code = ansi[6].opaque();
    let sel_bg = theme.palette.terminal.selection_bg.with_alpha_f32(0.75);
    let ordered_sel = txt.ordered_selection();

    let mut inline_images = Vec::new();

    for (idx, row) in rows.iter().enumerate() {
        let rm = &row_map[idx];
        let y = body.y + rm.content_y - scroll_y;
        let row_h = rm.height;
        if y + row_h < body.y || y > body.bottom() {
            continue;
        }
        let row_rect = RectF::new(body.x, y, (body.w - scrollbar_reserve).max(0.0), row_h);

        // Draw selection highlight quad behind text for this row
        if let Some((sel_start, sel_end)) = ordered_sel
            && idx >= sel_start.row
            && idx <= sel_end.row
        {
            let char_len = rm.plain_text.chars().count();
            let c0 = if idx == sel_start.row {
                sel_start.col.min(char_len)
            } else {
                0
            };
            let c1 = if idx == sel_end.row {
                sel_end.col.min(char_len)
            } else {
                char_len
            };
            if c1 > c0 {
                let sx = rm.text_x + c0 as f32 * rm.cell_w - txt.scroll_x;
                let sw = (c1 - c0) as f32 * rm.cell_w;
                let srect = RectF::new(sx, y, sw, row_h).intersect(row_rect);
                if !srect.is_empty() {
                    draw_list.push_rect(srect, sel_bg, None, 0.0, 2.0, None);
                }
            }
        }

        // Draw Ctrl+F search match highlight quads behind text for this row
        if search.open && !search.matches.is_empty() {
            for (m_idx, m) in search.matches.iter().enumerate() {
                if m.row != idx {
                    continue;
                }
                let sb = m.start_byte.min(rm.plain_text.len());
                let eb = m.end_byte.min(rm.plain_text.len());
                if eb <= sb
                    || !rm.plain_text.is_char_boundary(sb)
                    || !rm.plain_text.is_char_boundary(eb)
                {
                    continue;
                }
                let c0 = rm.plain_text[..sb].chars().count();
                let c1 = rm.plain_text[..eb].chars().count();
                let mx = rm.text_x + c0 as f32 * rm.cell_w - txt.scroll_x;
                let mw = ((c1 - c0).max(1) as f32) * rm.cell_w;
                let mrect = RectF::new(mx, y + 1.0, mw, (row_h - 2.0).max(4.0)).intersect(row_rect);
                if !mrect.is_empty() {
                    let is_active = search.active_match == Some(m_idx);
                    if is_active {
                        draw_list.push_rect(
                            mrect,
                            tokens.accent_primary.with_alpha_f32(0.45),
                            Some(tokens.accent_primary),
                            1.0,
                            2.0,
                            None,
                        );
                    } else {
                        draw_list.push_rect(
                            mrect,
                            ansi[3].with_alpha_f32(0.30),
                            None,
                            0.0,
                            2.0,
                            None,
                        );
                    }
                }
            }
        }

        match row {
            TextRenderRow::CodeLine {
                line_no,
                text,
                tokens: row_tokens,
                lang_badge,
                is_target,
                in_code_block,
            } => {
                let gutter_mark = if !formatted_md {
                    line_no.and_then(|ln| {
                        txt.git_diff
                            .as_ref()
                            .and_then(|d| d.line_marks.get(&ln).copied())
                    })
                } else {
                    None
                };
                if *is_target {
                    draw_list.push_rect(
                        row_rect,
                        tokens.accent_primary.with_alpha_f32(0.16),
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                    draw_list.push_rect(
                        RectF::new(body.x, y, 2.5, row_h),
                        tokens.accent_primary,
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }
                if let Some(mark) = gutter_mark {
                    let (bar_col, tint_alpha) = match mark {
                        DiffGutterKind::UnstagedAdded => (ansi[2].opaque(), 0.08),
                        DiffGutterKind::UnstagedModified => (ansi[3].opaque(), 0.08),
                        DiffGutterKind::StagedAdded => (ansi[6].opaque(), 0.09),
                        DiffGutterKind::StagedModified => (ansi[4].opaque(), 0.09),
                    };
                    draw_list.push_rect(
                        RectF::new(
                            body.x + gutter_w + 1.0,
                            y,
                            (row_rect.w - gutter_w - 1.0).max(0.0),
                            row_h,
                        ),
                        bar_col.with_alpha_f32(tint_alpha),
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                    draw_list.push_rect(
                        RectF::new(
                            (body.x + gutter_w - 3.0).max(body.x),
                            y + 1.0,
                            3.0,
                            row_h - 2.0,
                        ),
                        bar_col,
                        None,
                        0.0,
                        1.0,
                        None,
                    );
                } else if !*is_target && *in_code_block {
                    draw_list.push_rect(
                        RectF::new(
                            body.x + 12.0,
                            y,
                            (body.w - scrollbar_reserve - 24.0).max(0.0),
                            row_h,
                        ),
                        tokens.surface_row_hover,
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                    draw_list.push_rect(
                        RectF::new(body.x + 12.0, y, 2.0, row_h),
                        tokens.border_subtle,
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }

                // Deleted-line tick marker in gutter
                if !formatted_md
                    && let Some(ln) = line_no
                    && let Some(diff) = &txt.git_diff
                {
                    if *ln == 1 && diff.deleted_after_lines.contains(&0) {
                        draw_list.push_rect(
                            RectF::new((body.x + gutter_w - 8.0).max(body.x), y, 8.0, 2.0),
                            ansi[1].opaque(),
                            None,
                            0.0,
                            0.0,
                            None,
                        );
                    }
                    if diff.deleted_after_lines.contains(ln) {
                        draw_list.push_rect(
                            RectF::new(
                                (body.x + gutter_w - 8.0).max(body.x),
                                y + row_h - 2.0,
                                8.0,
                                2.0,
                            ),
                            ansi[1].opaque(),
                            None,
                            0.0,
                            0.0,
                            None,
                        );
                    }
                }

                if let Some(n) = line_no
                    && !formatted_md
                {
                    draw_list.push_text(
                        RectF::new(
                            body.x + 4.0,
                            y + 2.0,
                            (gutter_w - 14.0).max(0.0),
                            row_h - 2.0,
                        ),
                        n.to_string(),
                        if *is_target {
                            tokens.accent_primary
                        } else {
                            tokens.fg_muted
                        },
                        FontKind::Mono,
                        11.5,
                        if *is_target {
                            FontWeight::SemiBold
                        } else {
                            FontWeight::Regular
                        },
                        TextAlign::Right,
                        false,
                    );
                }

                if let Some(badge) = lang_badge
                    && *in_code_block
                    && !badge.is_empty()
                {
                    let bw = (badge.len() as f32 * 6.2 + 10.0).clamp(26.0, 72.0);
                    let bx = (body.right() - scrollbar_reserve - 16.0 - bw).max(body.x + 24.0);
                    draw_list.push_text(
                        RectF::new(bx, y + 2.0, bw, row_h - 4.0),
                        badge,
                        tokens.fg_muted,
                        FontKind::Mono,
                        9.5,
                        FontWeight::SemiBold,
                        TextAlign::Right,
                        false,
                    );
                }

                let draw_x = rm.text_x - txt.scroll_x;
                let draw_w = (body.right() - scrollbar_reserve - rm.text_x - 8.0).max(0.0);
                let spans =
                    syntax::slice_tokens_to_spans(text, row_tokens, 0, text.len(), theme, tokens);
                let text_rect = RectF::new(draw_x, y + 2.0, draw_w + txt.scroll_x, row_h - 2.0);
                let clip_r = RectF::new(rm.text_x, y, draw_w, row_h).intersect(body);
                draw_list.push_rich_text(
                    text_rect,
                    Some(clip_r),
                    spans,
                    12.5,
                    (12.5_f32 * 1.25).ceil(),
                    true,
                );
            }
            TextRenderRow::DiffHunkHeader { text } => {
                draw_list.push_rect(
                    row_rect,
                    tokens.accent_primary.with_alpha_f32(0.12),
                    None,
                    0.0,
                    0.0,
                    None,
                );
                draw_list.push_text(
                    RectF::new(
                        body.x + 12.0,
                        y + 2.0,
                        (row_rect.w - 24.0).max(0.0),
                        row_h - 2.0,
                    ),
                    text,
                    tokens.accent_primary,
                    FontKind::Mono,
                    11.5,
                    FontWeight::SemiBold,
                    TextAlign::Left,
                    true,
                );
            }
            TextRenderRow::DiffCodeLine {
                old_line,
                new_line,
                kind,
                text,
                tokens: row_tokens,
                is_target,
            } => {
                let (bg_col, prefix, prefix_col) = match kind {
                    DiffLineKind::Added => {
                        (Some(ansi[2].with_alpha_f32(0.14)), "+", ansi[2].opaque())
                    }
                    DiffLineKind::Removed => {
                        (Some(ansi[1].with_alpha_f32(0.14)), "-", ansi[1].opaque())
                    }
                    DiffLineKind::Context => (None, " ", tokens.fg_muted),
                };
                if let Some(bg) = bg_col {
                    draw_list.push_rect(row_rect, bg, None, 0.0, 0.0, None);
                }
                if *is_target {
                    draw_list.push_rect(
                        RectF::new(body.x, y, 2.5, row_h),
                        tokens.accent_primary,
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }
                let old_s = old_line.map_or_else(String::new, |n| n.to_string());
                let new_s = new_line.map_or_else(String::new, |n| n.to_string());
                let half_gw = ((gutter_w - 20.0) * 0.5).max(18.0);
                draw_list.push_text(
                    RectF::new(body.x + 4.0, y + 2.0, half_gw, row_h - 2.0),
                    old_s,
                    tokens.fg_muted,
                    FontKind::Mono,
                    11.0,
                    FontWeight::Regular,
                    TextAlign::Right,
                    false,
                );
                draw_list.push_text(
                    RectF::new(body.x + 6.0 + half_gw, y + 2.0, half_gw, row_h - 2.0),
                    new_s,
                    if *is_target {
                        tokens.accent_primary
                    } else {
                        tokens.fg_muted
                    },
                    FontKind::Mono,
                    11.0,
                    if *is_target {
                        FontWeight::SemiBold
                    } else {
                        FontWeight::Regular
                    },
                    TextAlign::Right,
                    false,
                );
                draw_list.push_text(
                    RectF::new(
                        (body.x + gutter_w - 12.0).max(body.x),
                        y + 2.0,
                        12.0,
                        row_h - 2.0,
                    ),
                    prefix,
                    prefix_col,
                    FontKind::Mono,
                    12.0,
                    FontWeight::Bold,
                    TextAlign::Center,
                    false,
                );

                let draw_x = rm.text_x - txt.scroll_x;
                let draw_w = (body.right() - scrollbar_reserve - rm.text_x - 8.0).max(0.0);
                let spans =
                    syntax::slice_tokens_to_spans(text, row_tokens, 0, text.len(), theme, tokens);
                let clip_r = RectF::new(rm.text_x, y, draw_w, row_h).intersect(body);
                draw_list.push_rich_text(
                    RectF::new(draw_x, y + 2.0, draw_w + txt.scroll_x, row_h - 2.0),
                    Some(clip_r),
                    spans,
                    12.5,
                    (12.5_f32 * 1.25).ceil(),
                    true,
                );
            }
            TextRenderRow::MarkdownHeading {
                level,
                text,
                is_target,
            } => {
                if *is_target {
                    draw_list.push_rect(
                        row_rect,
                        tokens.accent_primary.with_alpha_f32(0.14),
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }
                let size_px = match level {
                    1 => 16.0,
                    2 => 14.5,
                    _ => 13.0,
                };
                let (clean, _) = extract_markdown_inline_spans(text);
                draw_list.push_text(
                    RectF::new(
                        body.x + 14.0,
                        y + 2.0,
                        (body.w - scrollbar_reserve - 28.0).max(0.0),
                        row_h - 4.0,
                    ),
                    clean,
                    if *level <= 2 {
                        color_heading
                    } else {
                        tokens.fg_primary
                    },
                    FontKind::Sans,
                    size_px,
                    FontWeight::Bold,
                    TextAlign::Left,
                    true,
                );
                if *level <= 2 {
                    draw_list.push_rect(
                        RectF::new(
                            body.x + 14.0,
                            y + row_h - 2.0,
                            (body.w - scrollbar_reserve - 28.0).max(0.0),
                            1.0,
                        ),
                        tokens.border_hairline,
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }
            }
            TextRenderRow::MarkdownQuote { text, is_target } => {
                if *is_target {
                    draw_list.push_rect(
                        row_rect,
                        tokens.accent_primary.with_alpha_f32(0.14),
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }
                draw_list.push_rect(
                    RectF::new(body.x + 16.0, y + 2.0, 2.5, row_h - 4.0),
                    tokens.accent_primary.with_alpha_f32(0.65),
                    None,
                    0.0,
                    1.0,
                    None,
                );
                let qrect = RectF::new(
                    body.x + 26.0,
                    y + 2.0,
                    (body.w - scrollbar_reserve - 40.0).max(0.0),
                    row_h - 2.0,
                );
                emit_markdown_inline_rich_text(
                    draw_list,
                    qrect,
                    body,
                    text,
                    tokens.fg_secondary,
                    color_inline_code,
                    tokens,
                    measurer,
                );
            }
            TextRenderRow::MarkdownListItem {
                marker,
                text,
                indent,
                is_target,
            } => {
                if *is_target {
                    draw_list.push_rect(
                        row_rect,
                        tokens.accent_primary.with_alpha_f32(0.14),
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }
                let mx = body.x + 14.0 + *indent;
                draw_list.push_text(
                    RectF::new(mx, y + 2.0, 18.0, row_h - 2.0),
                    marker,
                    tokens.accent_primary,
                    FontKind::Sans,
                    12.5,
                    FontWeight::SemiBold,
                    TextAlign::Left,
                    false,
                );
                let tx = mx + 18.0;
                let tw = (body.right() - scrollbar_reserve - tx - 10.0).max(0.0);
                emit_markdown_inline_rich_text(
                    draw_list,
                    RectF::new(tx, y + 2.0, tw, row_h - 2.0),
                    body,
                    text,
                    tokens.fg_primary,
                    color_inline_code,
                    tokens,
                    measurer,
                );
            }
            TextRenderRow::MarkdownRule => {
                draw_list.push_rect(
                    RectF::new(
                        body.x + 14.0,
                        y + row_h * 0.5,
                        (body.w - scrollbar_reserve - 28.0).max(0.0),
                        1.0,
                    ),
                    tokens.border_subtle,
                    None,
                    0.0,
                    0.0,
                    None,
                );
            }
            TextRenderRow::MarkdownTableRow {
                cells,
                alignments,
                col_weights,
                is_header,
                is_even_row,
                is_target,
            } => {
                let table_x = body.x + 14.0;
                let table_w = (body.w - scrollbar_reserve - 28.0).max(40.0);
                let table_rect = RectF::new(table_x, y, table_w, row_h);
                let bg = if *is_target {
                    Some(tokens.accent_primary.with_alpha_f32(0.14))
                } else if *is_header {
                    Some(tokens.surface_pane_header)
                } else if *is_even_row {
                    Some(tokens.surface_row_hover)
                } else {
                    None
                };
                if let Some(fill) = bg {
                    draw_list.push_rect(table_rect, fill, None, 0.0, 0.0, None);
                }
                draw_list.push_rect(
                    RectF::new(table_x, y + row_h - 1.0, table_w, 1.0),
                    if *is_header {
                        tokens.border_subtle
                    } else {
                        tokens.border_hairline
                    },
                    None,
                    0.0,
                    0.0,
                    None,
                );
                let total_weight: usize = col_weights.iter().sum::<usize>().max(1);
                let mut cx = table_x;
                for (ci, cell) in cells.iter().enumerate() {
                    let w_share = col_weights.get(ci).copied().unwrap_or(1).max(1);
                    let cw = table_w * (w_share as f32 / total_weight as f32);
                    if ci > 0 {
                        draw_list.push_rect(
                            RectF::new(cx, y, 1.0, row_h),
                            tokens.border_hairline,
                            None,
                            0.0,
                            0.0,
                            None,
                        );
                    }
                    let align = alignments.get(ci).copied().unwrap_or(TextAlign::Left);
                    let (clean, _) = extract_markdown_inline_spans(cell);
                    draw_list.push_text(
                        RectF::new(cx + 6.0, y + 3.0, (cw - 12.0).max(4.0), row_h - 5.0),
                        clean,
                        if *is_header {
                            tokens.fg_primary
                        } else {
                            tokens.fg_secondary
                        },
                        FontKind::Sans,
                        12.0,
                        if *is_header {
                            FontWeight::Bold
                        } else {
                            FontWeight::Regular
                        },
                        align,
                        true,
                    );
                    cx += cw;
                }
            }
            TextRenderRow::MarkdownImage {
                alt,
                src,
                is_target,
            } => {
                if *is_target {
                    draw_list.push_rect(
                        row_rect,
                        tokens.accent_primary.with_alpha_f32(0.10),
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }
                let img_x = body.x + 16.0;
                match txt.inline_images.get(src) {
                    Some(MarkdownInlineImage::Loaded {
                        texture_key,
                        width,
                        height,
                    }) if *width > 0 && *height > 0 => {
                        let scale = (max_img_w / *width as f32)
                            .min(280.0 / *height as f32)
                            .min(1.0);
                        let disp_w = (*width as f32 * scale).max(1.0);
                        let disp_h = (*height as f32 * scale).max(1.0);
                        let raw_rect = RectF::new(img_x, y + 6.0, disp_w, disp_h);
                        let vis_rect = raw_rect.intersect(body);
                        if !vis_rect.is_empty() {
                            let u0 = ((vis_rect.x - raw_rect.x) / disp_w).clamp(0.0, 1.0);
                            let v0 = ((vis_rect.y - raw_rect.y) / disp_h).clamp(0.0, 1.0);
                            let u1 = ((vis_rect.right() - raw_rect.x) / disp_w).clamp(0.0, 1.0);
                            let v1 = ((vis_rect.bottom() - raw_rect.y) / disp_h).clamp(0.0, 1.0);
                            let corner_r = cap_radius(4.0, theme.rounding);
                            draw_list.push_rect(
                                vis_rect,
                                Rgba::new(0, 0, 0, 0),
                                Some(tokens.border_subtle),
                                1.0,
                                corner_r,
                                None,
                            );
                            inline_images.push(ViewerImagePlacement {
                                texture_key: *texture_key,
                                dest_rect: vis_rect,
                                uv_bounds: [u0, v0, u1, v1],
                                clip_rect: body,
                                corner_radius: corner_r,
                                opacity: 1.0,
                                pixel_crisp: false,
                            });
                        }
                    }
                    Some(MarkdownInlineImage::Loading) => {
                        let pill = RectF::new(img_x, y + 4.0, (max_img_w).min(260.0), 20.0);
                        draw_list.push_rect(
                            pill,
                            tokens.surface_pane_header,
                            Some(tokens.border_hairline),
                            1.0,
                            cap_radius(4.0, theme.rounding),
                            None,
                        );
                        draw_list.push_text(
                            pill,
                            format!("Loading image: {alt}…"),
                            tokens.fg_muted,
                            FontKind::Sans,
                            11.0,
                            FontWeight::Regular,
                            TextAlign::Center,
                            true,
                        );
                    }
                    _ => {
                        let label = if alt.is_empty() {
                            format!("Image unavailable: {src}")
                        } else {
                            format!("Image unavailable: {alt}")
                        };
                        let pill = RectF::new(img_x, y + 4.0, (max_img_w).min(300.0), 20.0);
                        draw_list.push_rect(
                            pill,
                            tokens.surface_pane_header,
                            Some(tokens.border_subtle),
                            1.0,
                            cap_radius(4.0, theme.rounding),
                            None,
                        );
                        draw_list.push_icon(
                            RectF::new(pill.x + 6.0, pill.y + 4.0, 12.0, 12.0),
                            IconId::Warning,
                            tokens.fg_muted,
                        );
                        draw_list.push_text(
                            RectF::new(pill.x + 22.0, pill.y, (pill.w - 28.0).max(0.0), pill.h),
                            label,
                            tokens.fg_muted,
                            FontKind::Sans,
                            11.0,
                            FontWeight::Regular,
                            TextAlign::Left,
                            true,
                        );
                    }
                }
            }
            TextRenderRow::MarkdownParagraph { text, is_target } => {
                if *is_target {
                    draw_list.push_rect(
                        row_rect,
                        tokens.accent_primary.with_alpha_f32(0.14),
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }
                let tx = body.x + 14.0;
                let tw = (body.w - scrollbar_reserve - 28.0).max(0.0);
                emit_markdown_inline_rich_text(
                    draw_list,
                    RectF::new(tx, y + 2.0, tw, row_h - 2.0),
                    body,
                    text,
                    tokens.fg_primary,
                    color_inline_code,
                    tokens,
                    measurer,
                );
            }
        }
    }

    // Thin vertical scrollbar when content overflows
    if max_scroll > 0.5 && body.h > 20.0 {
        let track = RectF::new(
            body.right() - 10.0,
            body.y + 4.0,
            8.0,
            (body.h - 8.0).max(0.0),
        );
        let min_thumb = 24.0_f32.min(track.h);
        let thumb_h = ((body.h / content_h) * track.h).clamp(min_thumb, track.h);
        let frac = (scroll_y / max_scroll).clamp(0.0, 1.0);
        let thumb_y = track.y + frac * (track.h - thumb_h);
        let thumb = RectF::new(track.x + 1.0, thumb_y, 6.0, thumb_h);

        let sb_hot = interaction.is_hovered(&HitTarget::ViewerScrollbarThumb)
            || interaction.is_pressed(&HitTarget::ViewerScrollbarThumb)
            || interaction.is_hovered(&HitTarget::ViewerScrollbarTrack);
        draw_list.push_rect(
            thumb,
            if sb_hot {
                tokens.scrollbar_thumb_hover
            } else {
                tokens.scrollbar_thumb
            },
            None,
            0.0,
            3.0,
            None,
        );
        draw_list.push_hit(
            track,
            HitTarget::ViewerScrollbarTrack,
            z_layer::PANE_CHROME + 2,
        );
        draw_list.push_hit(
            thumb,
            HitTarget::ViewerScrollbarThumb,
            z_layer::PANE_CHROME + 3,
        );
    }

    draw_list.pop_clip();
    TextRenderOutput {
        viewport_h: body.h,
        content_h,
        row_map,
        inline_images,
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_markdown_inline_rich_text(
    draw_list: &mut UiDrawList,
    rect: RectF,
    body_clip: RectF,
    raw_text: &str,
    base_color: Rgba,
    code_color: Rgba,
    tokens: &UiTokens,
    measurer: &mut Option<&mut dyn UiTextMeasurer>,
) {
    let segs = parse_markdown_inline_segments(raw_text);
    if segs.is_empty() {
        return;
    }
    let mut spans = Vec::with_capacity(segs.len());
    let mut cur_x = rect.x;
    let avg_char_w = 12.5 * 0.52;
    for seg in segs {
        if seg.text.is_empty() {
            continue;
        }
        let (bold, italic) = match &seg.kind {
            MarkdownSpanKind::Bold => (true, false),
            MarkdownSpanKind::Italic => (false, true),
            MarkdownSpanKind::BoldItalic => (true, true),
            _ => (false, false),
        };
        let seg_w = if let Some(m) = measurer.as_deref_mut() {
            let expanded = syntax::expand_tabs(&seg.text);
            m.measure_text(&expanded, 12.5, bold, italic, false)
        } else {
            (seg.text.chars().count() as f32) * avg_char_w
        };
        match &seg.kind {
            MarkdownSpanKind::Text => {
                spans.push(TextSpan::new(seg.text, base_color));
            }
            MarkdownSpanKind::Bold => {
                spans.push(TextSpan::styled(seg.text, base_color, true, false));
            }
            MarkdownSpanKind::Italic => {
                spans.push(TextSpan::styled(seg.text, base_color, false, true));
            }
            MarkdownSpanKind::BoldItalic => {
                spans.push(TextSpan::styled(seg.text, base_color, true, true));
            }
            MarkdownSpanKind::Code => {
                spans.push(TextSpan::styled(seg.text, code_color, false, false));
            }
            MarkdownSpanKind::Link { url } => {
                let link_rect = RectF::new(
                    cur_x,
                    rect.y,
                    seg_w.min((rect.right() - cur_x).max(0.0)),
                    rect.h,
                )
                .intersect(body_clip);
                if !link_rect.is_empty() {
                    draw_list.push_rect(
                        RectF::new(link_rect.x, link_rect.bottom() - 2.0, link_rect.w, 1.0),
                        tokens.accent_primary.with_alpha_f32(0.75),
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                    draw_list.push_hit(
                        link_rect,
                        HitTarget::ViewerLink { url: url.clone() },
                        z_layer::PANE_CHROME + 2,
                    );
                }
                spans.push(TextSpan::styled(
                    seg.text,
                    tokens.accent_primary,
                    false,
                    false,
                ));
            }
        }
        cur_x += seg_w;
    }
    let clip_r = rect.intersect(body_clip);
    draw_list.push_rich_text(
        rect,
        Some(clip_r),
        spans,
        12.5,
        (12.5_f32 * 1.25).ceil(),
        false,
    );
}

fn render_viewer_search_bar(
    draw_list: &mut UiDrawList,
    body: RectF,
    state: &ViewerSearchState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) {
    if !state.open || body.is_empty() {
        return;
    }
    let w = 316.0_f32.min((body.w - 20.0).max(160.0));
    let bar = RectF::new(
        (body.right() - w - 12.0).max(body.x + 6.0),
        body.y + 8.0,
        w,
        34.0,
    );
    draw_list.push_rect(
        bar,
        tokens.surface_modal,
        Some(tokens.border_active),
        1.0,
        cap_radius(8.0, theme.rounding),
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 8.0,
            blur_radius: 18.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );

    let close_btn = RectF::new(bar.right() - 26.0, bar.y + 6.0, 22.0, 22.0);
    let next_btn = RectF::new(close_btn.x - 22.0, bar.y + 6.0, 20.0, 22.0);
    let prev_btn = RectF::new(next_btn.x - 22.0, bar.y + 6.0, 20.0, 22.0);
    let case_btn = RectF::new(prev_btn.x - 26.0, bar.y + 6.0, 24.0, 22.0);
    let count_rect = RectF::new(case_btn.x - 52.0, bar.y + 8.0, 48.0, 18.0);
    let input_rect = RectF::new(
        bar.x + 8.0,
        bar.y + 7.0,
        (count_rect.x - bar.x - 10.0).max(20.0),
        20.0,
    );

    let (display, color) = if state.input.is_empty() {
        (state.input.placeholder().to_owned(), tokens.fg_muted)
    } else {
        (state.input.text().to_owned(), tokens.fg_primary)
    };
    draw_list.push_text(
        input_rect,
        display,
        color,
        FontKind::Mono,
        12.0,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    let caret_x = input_rect.x
        + (state.input.text().chars().count() as f32 * 7.2).min((input_rect.w - 2.0).max(0.0));
    draw_list.push_rect(
        RectF::new(caret_x, input_rect.y + 3.0, 1.5, 14.0),
        tokens.accent_primary,
        None,
        0.0,
        0.75,
        None,
    );
    draw_list.push_hit(
        input_rect,
        HitTarget::ViewerSearchInput,
        z_layer::FIND_BAR + 1,
    );

    let total = state.matches.len();
    let count_label = if total == 0 {
        "0 / 0".to_owned()
    } else {
        format!(
            "{} / {}",
            state.active_match.map_or(0, |i| i + 1).min(total),
            total
        )
    };
    draw_list.push_text(
        count_rect,
        count_label,
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );

    let toggle_r = cap_radius(4.0, theme.rounding);
    if state.case_sensitive {
        draw_list.push_rect(case_btn, tokens.accent_primary, None, 0.0, toggle_r, None);
    } else if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::ViewerSearchCaseToggle, false, tokens)
    {
        draw_list.push_rect(case_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        case_btn,
        "Aa",
        if state.case_sensitive {
            tokens.accent_foreground
        } else if interaction.is_hovered(&HitTarget::ViewerSearchCaseToggle) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Mono,
        10.5,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(
        case_btn,
        HitTarget::ViewerSearchCaseToggle,
        z_layer::FIND_BAR + 1,
    );

    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::ViewerSearchPrev, false, tokens)
    {
        draw_list.push_rect(prev_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        prev_btn,
        "↑",
        if interaction.is_hovered(&HitTarget::ViewerSearchPrev) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(prev_btn, HitTarget::ViewerSearchPrev, z_layer::FIND_BAR + 1);

    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::ViewerSearchNext, false, tokens)
    {
        draw_list.push_rect(next_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        next_btn,
        "↓",
        if interaction.is_hovered(&HitTarget::ViewerSearchNext) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(next_btn, HitTarget::ViewerSearchNext, z_layer::FIND_BAR + 1);

    let (close_bg, close_tint) =
        interaction.danger_button_style(&HitTarget::ViewerSearchClose, tokens.fg_secondary, tokens);
    if let Some((fill, border)) = close_bg {
        draw_list.push_rect(close_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_icon(close_btn.inset_uniform(4.0), IconId::Close, close_tint);
    draw_list.push_hit(
        close_btn,
        HitTarget::ViewerSearchClose,
        z_layer::FIND_BAR + 1,
    );
}

fn render_audio_content(
    draw_list: &mut UiDrawList,
    body: RectF,
    tab: &ViewerTab,
    aud: &AudioViewerState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) -> (RectF, RectF) {
    let toolbar_h = VIDEO_TOOLBAR_HEIGHT.min(body.h);
    let stage = RectF::new(body.x, body.y, body.w, (body.h - toolbar_h).max(0.0));
    let toolbar = RectF::new(body.x, stage.bottom(), body.w, toolbar_h);

    // Centered audio waveform card
    let card_w = (stage.w - 32.0).clamp(160.0, 720.0);
    let card_h = (stage.h - 32.0).clamp(120.0, 280.0);
    let card = RectF::new(
        stage.x + (stage.w - card_w) * 0.5,
        stage.y + (stage.h - card_h) * 0.5,
        card_w,
        card_h,
    );
    draw_list.push_rect(
        card,
        tokens.surface_pane_header,
        Some(tokens.border_subtle),
        1.0,
        cap_radius(8.0, theme.rounding),
        None,
    );

    // Title & metadata
    draw_list.push_text(
        RectF::new(card.x + 16.0, card.y + 14.0, (card.w - 32.0).max(0.0), 20.0),
        &tab.title,
        tokens.fg_primary,
        FontKind::Sans,
        13.5,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    let meta = aud.metadata_summary();
    if !meta.is_empty() {
        draw_list.push_text(
            RectF::new(card.x + 16.0, card.y + 36.0, (card.w - 32.0).max(0.0), 16.0),
            meta,
            tokens.fg_muted,
            FontKind::Mono,
            11.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
    }
    if let Some(warn) = aud.warning_banner.as_deref()
        && !warn.is_empty()
    {
        draw_list.push_text(
            RectF::new(card.x + 16.0, card.y + 54.0, (card.w - 32.0).max(0.0), 16.0),
            warn,
            tokens.status_blocked,
            FontKind::Sans,
            10.5,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );
    }

    let wave_top = card.y
        + if aud.warning_banner.is_some() {
            76.0
        } else {
            62.0
        };
    let wave_rect = RectF::new(
        card.x + 16.0,
        wave_top,
        (card.w - 32.0).max(0.0),
        (card.bottom() - wave_top - 16.0).max(24.0),
    );
    draw_list.push_rect(
        wave_rect,
        tokens.surface_frame.with_alpha_f32(0.55),
        Some(tokens.border_hairline),
        1.0,
        cap_radius(6.0, theme.rounding),
        None,
    );

    let progress = aud.progress_fraction();
    let bar_count = ((wave_rect.w / 5.0).floor() as usize).clamp(24, 140);
    let inner = wave_rect.inset_uniform(8.0);
    if !inner.is_empty() {
        let step_w = inner.w / bar_count as f32;
        let bar_w = (step_w * 0.62).clamp(1.5, 4.0);
        let mid_y = inner.y + inner.h * 0.5;
        let max_bar_h = inner.h;
        for i in 0..bar_count {
            let peak = if aud.waveform_peaks.is_empty() {
                0.25
            } else {
                let p_idx = (i * aud.waveform_peaks.len()) / bar_count;
                aud.waveform_peaks
                    .get(p_idx)
                    .copied()
                    .unwrap_or(0.2)
                    .clamp(0.05, 1.0)
            };
            let bh = (peak * max_bar_h).max(3.0);
            let bx = inner.x + i as f32 * step_w + (step_w - bar_w) * 0.5;
            let by = mid_y - bh * 0.5;
            let bar_frac = (i as f32 + 0.5) / bar_count as f32;
            let col = if bar_frac <= progress {
                tokens.accent_primary
            } else {
                tokens.fg_muted.with_alpha_f32(0.45)
            };
            draw_list.push_rect(RectF::new(bx, by, bar_w, bh), col, None, 0.0, 1.0, None);
        }

        // Playhead vertical line
        let head_x =
            (inner.x + inner.w * progress).clamp(inner.x, (inner.right() - 2.0).max(inner.x));
        draw_list.push_rect(
            RectF::new(head_x, inner.y, 2.0, inner.h),
            tokens.accent_primary,
            None,
            0.0,
            1.0,
            None,
        );
    }
    draw_list.push_hit(
        wave_rect,
        HitTarget::ViewerAudioWaveform,
        z_layer::PANE_CHROME + 2,
    );

    // Lower transport toolbar
    let seek_bar = render_audio_toolbar(draw_list, toolbar, aud, interaction, theme, tokens);
    (wave_rect, seek_bar)
}

fn render_audio_toolbar(
    draw_list: &mut UiDrawList,
    bar: RectF,
    aud: &AudioViewerState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) -> RectF {
    if bar.is_empty() {
        return RectF::default();
    }
    let z = z_layer::PANE_CHROME + 2;
    let r = cap_radius(5.0, theme.rounding);

    draw_list.push_rect(bar, tokens.surface_pane_header, None, 0.0, 0.0, None);
    draw_list.push_rect(
        RectF::new(bar.x, bar.y, bar.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let scrub_hit_rect = RectF::new(bar.x + 10.0, bar.y + 4.0, (bar.w - 20.0).max(0.0), 18.0);
    let scrub_vis_rect = RectF::new(
        scrub_hit_rect.x,
        scrub_hit_rect.y + 6.0,
        scrub_hit_rect.w,
        6.0,
    );
    let progress = aud.progress_fraction();
    draw_list.push_progress(
        scrub_vis_rect,
        progress,
        tokens.accent_primary,
        tokens.surface_row_active,
        3.0,
    );
    if scrub_vis_rect.w > 0.0 {
        let knob_size = if interaction.is_hovered(&HitTarget::ViewerAudioSeekBar)
            || interaction.is_pressed(&HitTarget::ViewerAudioSeekBar)
        {
            14.0
        } else {
            11.0
        };
        let knob_x = (scrub_vis_rect.x + scrub_vis_rect.w * progress - knob_size * 0.5).clamp(
            scrub_vis_rect.x,
            (scrub_vis_rect.right() - knob_size).max(scrub_vis_rect.x),
        );
        let knob_y = scrub_vis_rect.y + (scrub_vis_rect.h - knob_size) * 0.5;
        draw_list.push_rect(
            RectF::new(knob_x, knob_y, knob_size, knob_size),
            tokens.accent_primary,
            Some(tokens.surface_pane_header),
            1.5,
            knob_size * 0.5,
            None,
        );
    }
    draw_list.push_hit(scrub_hit_rect, HitTarget::ViewerAudioSeekBar, z);

    let row_y = bar.y + 28.0;
    let btn_h = 24.0;
    let mut x = bar.x + 8.0;

    let play_rect = RectF::new(x, row_y, 28.0, btn_h);
    let (pfill, pborder) = interaction
        .button_surface(&HitTarget::ViewerAudioPlayPause, aud.playing, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
    draw_list.push_rect(play_rect, pfill, pborder, 1.0, r, None);
    draw_list.push_text(
        play_rect,
        if aud.playing { "❚❚" } else { "▶" },
        if aud.playing {
            tokens.accent_primary
        } else {
            tokens.fg_primary
        },
        FontKind::Sans,
        11.5,
        FontWeight::Bold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(play_rect, HitTarget::ViewerAudioPlayPause, z);
    x = play_rect.right() + 6.0;

    let time_label = format!(
        "{} / {}",
        VideoViewerState::format_timestamp(aud.position_secs),
        VideoViewerState::format_timestamp(aud.duration_secs)
    );
    let time_w = (time_label.len() as f32 * 6.4 + 2.0).max(66.0);
    draw_list.push_text(
        RectF::new(x, row_y, time_w, btn_h),
        time_label,
        tokens.fg_primary,
        FontKind::Mono,
        10.5,
        FontWeight::Medium,
        TextAlign::Left,
        false,
    );
    x += time_w + 4.0;

    let mute_rect = RectF::new(x, row_y, 36.0, btn_h);
    let (mfill, mborder) = interaction
        .button_surface(&HitTarget::ViewerAudioMuteToggle, aud.muted, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
    draw_list.push_rect(mute_rect, mfill, mborder, 1.0, r, None);
    draw_list.push_text(
        mute_rect,
        if aud.muted { "Mute" } else { "Vol" },
        if aud.muted {
            tokens.status_error
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        10.5,
        FontWeight::Medium,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(mute_rect, HitTarget::ViewerAudioMuteToggle, z);
    x = mute_rect.right() + 3.0;

    let loop_rect = RectF::new(x, row_y, 38.0, btn_h);
    let (lfill, lborder) = interaction
        .button_surface(&HitTarget::ViewerAudioLoopToggle, aud.looping, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
    draw_list.push_rect(loop_rect, lfill, lborder, 1.0, r, None);
    draw_list.push_text(
        loop_rect,
        "Loop",
        if aud.looping {
            tokens.accent_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        10.5,
        FontWeight::Medium,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(loop_rect, HitTarget::ViewerAudioLoopToggle, z);
    x = loop_rect.right() + 8.0;

    let meta = aud.metadata_summary();
    let meta_w = (bar.right() - x - 8.0).max(0.0);
    if meta_w > 48.0 && !meta.is_empty() {
        draw_list.push_text(
            RectF::new(x, row_y, meta_w, btn_h),
            meta,
            tokens.fg_muted,
            FontKind::Mono,
            10.5,
            FontWeight::Regular,
            TextAlign::Right,
            true,
        );
    }

    scrub_hit_rect
}

fn render_pdf_content(
    draw_list: &mut UiDrawList,
    body: RectF,
    pdf: &PdfViewerState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) -> (RectF, Option<ViewerImagePlacement>) {
    let toolbar_h = IMAGE_TOOLBAR_HEIGHT.min(body.h);
    let canvas_outer = RectF::new(body.x, body.y, body.w, (body.h - toolbar_h).max(0.0));
    let toolbar = RectF::new(body.x, canvas_outer.bottom(), body.w, toolbar_h);
    let canvas = canvas_outer.inset_uniform(10.0);

    draw_list.push_rect(
        canvas_outer,
        tokens.surface_frame.with_alpha_f32(0.65),
        None,
        0.0,
        0.0,
        None,
    );
    draw_list.push_hit(
        canvas_outer,
        HitTarget::ViewerCanvas,
        z_layer::PANE_CHROME + 1,
    );

    let computed = if pdf.texture_key != 0 && pdf.page_width > 0 && pdf.page_height > 0 {
        compute_media_placement(
            pdf.page_width,
            pdf.page_height,
            canvas,
            pdf.viewport.zoom,
            pdf.viewport.fit_mode,
            pdf.viewport.pan_offset,
        )
    } else {
        None
    };

    let placement = computed.map(|c| {
        draw_list.push_rect(
            c.dest_rect,
            Rgba::new(0, 0, 0, 0),
            Some(tokens.border_subtle),
            1.0,
            cap_radius(2.0, theme.rounding),
            None,
        );
        ViewerImagePlacement {
            texture_key: pdf.texture_key,
            dest_rect: c.dest_rect,
            uv_bounds: c.uv_bounds,
            clip_rect: c.clip_rect,
            corner_radius: cap_radius(2.0, theme.rounding),
            opacity: 1.0,
            pixel_crisp: c.effective_zoom > 2.0,
        }
    });

    // Text-only fallback when rasterizer (`pdftoppm`) is unavailable
    if placement.is_none() && !canvas.is_empty() {
        let card = canvas.inset_uniform(8.0);
        draw_list.push_rect(
            card,
            tokens.surface_pane_header,
            Some(tokens.border_subtle),
            1.0,
            cap_radius(6.0, theme.rounding),
            None,
        );
        let notice = pdf
            .fallback_notice
            .as_deref()
            .unwrap_or("PDF text preview (raster page unavailable)");
        draw_list.push_text(
            RectF::new(card.x + 12.0, card.y + 10.0, (card.w - 24.0).max(0.0), 18.0),
            notice,
            tokens.fg_muted,
            FontKind::Sans,
            11.0,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );
        if let Some(txt) = pdf.page_text.as_deref() {
            let mut ty = card.y + 34.0;
            for line in txt.lines() {
                if ty + TEXT_LINE_HEIGHT > card.bottom() - 8.0 {
                    break;
                }
                draw_list.push_text(
                    RectF::new(
                        card.x + 12.0,
                        ty,
                        (card.w - 24.0).max(0.0),
                        TEXT_LINE_HEIGHT,
                    ),
                    line,
                    tokens.fg_primary,
                    FontKind::Mono,
                    12.0,
                    FontWeight::Regular,
                    TextAlign::Left,
                    true,
                );
                ty += TEXT_LINE_HEIGHT;
            }
        }
    }

    // Bottom PDF toolbar: [◀] Page X / N [▶]   [-][100%][+][Fit]   100%   metadata
    let eff_zoom = computed.map_or(pdf.viewport.zoom, |c| c.effective_zoom);
    render_pdf_toolbar(
        draw_list,
        toolbar,
        pdf,
        eff_zoom,
        interaction,
        theme,
        tokens,
    );

    (canvas, placement)
}

fn render_pdf_toolbar(
    draw_list: &mut UiDrawList,
    bar: RectF,
    pdf: &PdfViewerState,
    effective_zoom: f32,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) {
    if bar.is_empty() {
        return;
    }
    let z = z_layer::PANE_CHROME + 2;
    let r = cap_radius(5.0, theme.rounding);

    draw_list.push_rect(bar, tokens.surface_pane_header, None, 0.0, 0.0, None);
    draw_list.push_rect(
        RectF::new(bar.x, bar.y, bar.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let btn_y = bar.y + (bar.h - 24.0) * 0.5;
    let mut x = bar.x + 8.0;

    // Prev page button
    let prev_rect = RectF::new(x, btn_y, 24.0, 24.0);
    let (pf, pb) = interaction
        .button_surface(&HitTarget::ViewerPdfPrevPage, false, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
    draw_list.push_rect(prev_rect, pf, pb, 1.0, r, None);
    draw_list.push_text(
        prev_rect,
        "◀",
        if pdf.current_page > 1 {
            tokens.fg_primary
        } else {
            tokens.fg_muted
        },
        FontKind::Sans,
        10.5,
        FontWeight::Bold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(prev_rect, HitTarget::ViewerPdfPrevPage, z);
    x = prev_rect.right() + 4.0;

    let page_label = format!("Page {} / {}", pdf.current_page, pdf.page_count);
    let page_w = (page_label.len() as f32 * 6.4 + 8.0).clamp(68.0, 124.0);
    draw_list.push_text(
        RectF::new(x, btn_y, page_w, 24.0),
        page_label,
        tokens.fg_primary,
        FontKind::Mono,
        11.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    x += page_w + 4.0;

    // Next page button
    let next_rect = RectF::new(x, btn_y, 24.0, 24.0);
    let (nf, nb) = interaction
        .button_surface(&HitTarget::ViewerPdfNextPage, false, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
    draw_list.push_rect(next_rect, nf, nb, 1.0, r, None);
    draw_list.push_text(
        next_rect,
        "▶",
        if pdf.current_page < pdf.page_count {
            tokens.fg_primary
        } else {
            tokens.fg_muted
        },
        FontKind::Sans,
        10.5,
        FontWeight::Bold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(next_rect, HitTarget::ViewerPdfNextPage, z);
    x = next_rect.right() + 8.0;

    // Zoom controls if space fits
    if bar.right() - x >= 150.0 {
        let buttons: [(&str, f32, HitTarget, bool); 4] = [
            ("-", 24.0, HitTarget::ViewerZoomOut, false),
            (
                "100%",
                40.0,
                HitTarget::ViewerZoomReset,
                !pdf.viewport.fit_mode && (effective_zoom - 1.0).abs() < 0.01,
            ),
            ("+", 24.0, HitTarget::ViewerZoomIn, false),
            ("Fit", 30.0, HitTarget::ViewerZoomFit, pdf.viewport.fit_mode),
        ];
        for (label, w, hit, active) in buttons {
            let rect = RectF::new(x, btn_y, w, 24.0);
            let (fill, border) = interaction
                .button_surface(&hit, active, tokens)
                .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
            draw_list.push_rect(rect, fill, border, 1.0, r, None);
            draw_list.push_text(
                rect,
                label,
                if active {
                    tokens.accent_primary
                } else {
                    tokens.fg_primary
                },
                FontKind::Mono,
                10.5,
                FontWeight::Medium,
                TextAlign::Center,
                false,
            );
            draw_list.push_hit(rect, hit, z);
            x = rect.right() + 3.0;
        }
    }

    let meta = pdf.metadata_summary();
    let meta_x = x + 6.0;
    let meta_w = (bar.right() - meta_x - 8.0).max(0.0);
    if meta_w > 40.0 && !meta.is_empty() {
        draw_list.push_text(
            RectF::new(meta_x, btn_y, meta_w, 24.0),
            meta,
            tokens.fg_muted,
            FontKind::Mono,
            10.5,
            FontWeight::Regular,
            TextAlign::Right,
            true,
        );
    }
}

fn render_image_content(
    draw_list: &mut UiDrawList,
    body: RectF,
    img: &ImageViewerState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) -> (RectF, Option<ViewerImagePlacement>) {
    let toolbar_h = IMAGE_TOOLBAR_HEIGHT.min(body.h);
    let canvas_outer = RectF::new(body.x, body.y, body.w, (body.h - toolbar_h).max(0.0));
    let toolbar = RectF::new(body.x, canvas_outer.bottom(), body.w, toolbar_h);
    let canvas = canvas_outer.inset_uniform(10.0);

    // Canvas backdrop
    draw_list.push_rect(
        canvas_outer,
        tokens.surface_frame.with_alpha_f32(0.65),
        None,
        0.0,
        0.0,
        None,
    );
    draw_list.push_hit(
        canvas_outer,
        HitTarget::ViewerCanvas,
        z_layer::PANE_CHROME + 1,
    );

    let computed = compute_media_placement(
        img.width,
        img.height,
        canvas,
        img.viewport.zoom,
        img.viewport.fit_mode,
        img.viewport.pan_offset,
    );

    let placement = computed.map(|c| {
        if img.has_alpha {
            render_checkerboard(draw_list, c.dest_rect, theme.is_dark);
        }
        draw_list.push_rect(
            c.dest_rect,
            Rgba::new(0, 0, 0, 0),
            Some(tokens.border_subtle),
            1.0,
            cap_radius(2.0, theme.rounding),
            None,
        );
        ViewerImagePlacement {
            texture_key: img.texture_key,
            dest_rect: c.dest_rect,
            uv_bounds: c.uv_bounds,
            clip_rect: c.clip_rect,
            corner_radius: cap_radius(2.0, theme.rounding),
            opacity: 1.0,
            pixel_crisp: c.effective_zoom > 2.0,
        }
    });

    // Footer toolbar: [-][100%][+][Fit]   125%        1920×1080 · PNG · 412 KB
    let eff_zoom = computed.map_or(img.viewport.zoom, |c| c.effective_zoom);
    render_image_toolbar(
        draw_list,
        toolbar,
        eff_zoom,
        img.viewport.fit_mode,
        &img.metadata_summary(),
        interaction,
        theme,
        tokens,
    );

    (canvas, placement)
}

#[allow(clippy::too_many_arguments)]
fn render_image_toolbar(
    draw_list: &mut UiDrawList,
    bar: RectF,
    effective_zoom: f32,
    fit_mode: bool,
    metadata: &str,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) {
    if bar.is_empty() {
        return;
    }
    let z = z_layer::PANE_CHROME + 2;
    let r = cap_radius(5.0, theme.rounding);

    draw_list.push_rect(bar, tokens.surface_pane_header, None, 0.0, 0.0, None);
    draw_list.push_rect(
        RectF::new(bar.x, bar.y, bar.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let btn_y = bar.y + (bar.h - 24.0) * 0.5;
    let mut x = bar.x + 8.0;

    let buttons: [(&str, f32, HitTarget, bool); 4] = [
        ("-", 26.0, HitTarget::ViewerZoomOut, false),
        (
            "100%",
            44.0,
            HitTarget::ViewerZoomReset,
            !fit_mode && (effective_zoom - 1.0).abs() < 0.01,
        ),
        ("+", 26.0, HitTarget::ViewerZoomIn, false),
        ("Fit", 34.0, HitTarget::ViewerZoomFit, fit_mode),
    ];

    for (label, w, hit, active) in buttons {
        let rect = RectF::new(x, btn_y, w, 24.0);
        let (fill, border) = interaction
            .button_surface(&hit, active, tokens)
            .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
        draw_list.push_rect(rect, fill, border, 1.0, r, None);
        draw_list.push_text(
            rect,
            label,
            if active {
                tokens.accent_primary
            } else {
                tokens.fg_primary
            },
            FontKind::Mono,
            11.0,
            FontWeight::Medium,
            TextAlign::Center,
            false,
        );
        draw_list.push_hit(rect, hit, z);
        x = rect.right() + 4.0;
    }

    // Current zoom percentage readout
    let pct_text = format!("{}%", (effective_zoom * 100.0).round() as u32);
    let pct_rect = RectF::new(x + 4.0, btn_y, 48.0, 24.0);
    draw_list.push_text(
        pct_rect,
        pct_text,
        tokens.fg_secondary,
        FontKind::Mono,
        11.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        false,
    );

    // Metadata on the right
    let meta_x = pct_rect.right() + 8.0;
    let meta_w = (bar.right() - meta_x - 10.0).max(0.0);
    if meta_w > 32.0 && !metadata.is_empty() {
        draw_list.push_text(
            RectF::new(meta_x, btn_y, meta_w, 24.0),
            metadata,
            tokens.fg_muted,
            FontKind::Mono,
            11.0,
            FontWeight::Regular,
            TextAlign::Right,
            true,
        );
    }
}

fn render_video_content(
    draw_list: &mut UiDrawList,
    body: RectF,
    vid: &VideoViewerState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) -> (RectF, RectF, Option<ViewerImagePlacement>) {
    let toolbar_h = VIDEO_TOOLBAR_HEIGHT.min(body.h);
    let canvas_outer = RectF::new(body.x, body.y, body.w, (body.h - toolbar_h).max(0.0));
    let toolbar = RectF::new(body.x, canvas_outer.bottom(), body.w, toolbar_h);
    let canvas = canvas_outer.inset_uniform(8.0);

    // Dark cinematic backdrop for video
    draw_list.push_rect(
        canvas_outer,
        if theme.is_dark {
            Rgba::new(8, 8, 10, 255)
        } else {
            tokens.surface_frame
        },
        None,
        0.0,
        0.0,
        None,
    );
    draw_list.push_hit(
        canvas_outer,
        HitTarget::ViewerCanvas,
        z_layer::PANE_CHROME + 1,
    );

    let computed = compute_media_placement(
        vid.width,
        vid.height,
        canvas,
        vid.viewport.zoom,
        vid.viewport.fit_mode,
        vid.viewport.pan_offset,
    );

    let placement = computed.and_then(|c| {
        (vid.texture_key != 0).then_some(ViewerImagePlacement {
            texture_key: vid.texture_key,
            dest_rect: c.dest_rect,
            uv_bounds: c.uv_bounds,
            clip_rect: c.clip_rect,
            corner_radius: cap_radius(2.0, theme.rounding),
            opacity: 1.0,
            pixel_crisp: c.effective_zoom > 2.0,
        })
    });

    // Center play badge overlay when paused
    if !vid.playing && canvas_outer.w >= 64.0 && canvas_outer.h >= 64.0 {
        let badge_size = 56.0;
        let badge = RectF::new(
            canvas_outer.x + (canvas_outer.w - badge_size) * 0.5,
            canvas_outer.y + (canvas_outer.h - badge_size) * 0.5,
            badge_size,
            badge_size,
        );
        let hot = interaction.is_hovered(&HitTarget::ViewerVideoPlayPause)
            || interaction.is_pressed(&HitTarget::ViewerVideoPlayPause);
        draw_list.push_rect(
            badge,
            if hot {
                tokens.accent_primary.with_alpha_f32(0.92)
            } else {
                Rgba::new(14, 14, 18, 210)
            },
            Some(if hot {
                tokens.accent_primary
            } else {
                tokens.border_subtle
            }),
            1.5,
            badge_size * 0.5,
            None,
        );
        draw_list.push_text(
            RectF::new(badge.x + 3.0, badge.y, badge.w - 3.0, badge.h),
            "▶",
            if hot {
                tokens.accent_foreground
            } else {
                Rgba::new(255, 255, 255, 245)
            },
            FontKind::Sans,
            22.0,
            FontWeight::Bold,
            TextAlign::Center,
            false,
        );
        draw_list.push_hit(
            badge,
            HitTarget::ViewerVideoPlayPause,
            z_layer::PANE_CHROME + 3,
        );
    }

    // Bottom transport bar
    let eff_zoom = computed.map_or(vid.viewport.zoom, |c| c.effective_zoom);
    let seek_bar = render_video_toolbar(
        draw_list,
        toolbar,
        vid,
        eff_zoom,
        interaction,
        theme,
        tokens,
    );

    (canvas, seek_bar, placement)
}

fn render_video_toolbar(
    draw_list: &mut UiDrawList,
    bar: RectF,
    vid: &VideoViewerState,
    effective_zoom: f32,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) -> RectF {
    if bar.is_empty() {
        return RectF::default();
    }
    let z = z_layer::PANE_CHROME + 2;
    let r = cap_radius(5.0, theme.rounding);

    draw_list.push_rect(bar, tokens.surface_pane_header, None, 0.0, 0.0, None);
    draw_list.push_rect(
        RectF::new(bar.x, bar.y, bar.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    // Row 1: Progress scrub bar (`22px` hit strip, `6px` visual bar + `12px` knob)
    let scrub_hit_rect = RectF::new(bar.x + 10.0, bar.y + 4.0, (bar.w - 20.0).max(0.0), 18.0);
    let scrub_vis_rect = RectF::new(
        scrub_hit_rect.x,
        scrub_hit_rect.y + 6.0,
        scrub_hit_rect.w,
        6.0,
    );
    let progress = vid.progress_fraction();
    draw_list.push_progress(
        scrub_vis_rect,
        progress,
        tokens.accent_primary,
        tokens.surface_row_active,
        3.0,
    );
    if scrub_vis_rect.w > 0.0 {
        let knob_size = if interaction.is_hovered(&HitTarget::ViewerVideoSeekBar)
            || interaction.is_pressed(&HitTarget::ViewerVideoSeekBar)
        {
            14.0
        } else {
            11.0
        };
        let knob_x = (scrub_vis_rect.x + scrub_vis_rect.w * progress - knob_size * 0.5).clamp(
            scrub_vis_rect.x,
            (scrub_vis_rect.right() - knob_size).max(scrub_vis_rect.x),
        );
        let knob_y = scrub_vis_rect.y + (scrub_vis_rect.h - knob_size) * 0.5;
        draw_list.push_rect(
            RectF::new(knob_x, knob_y, knob_size, knob_size),
            tokens.accent_primary,
            Some(tokens.surface_pane_header),
            1.5,
            knob_size * 0.5,
            None,
        );
    }
    draw_list.push_hit(scrub_hit_rect, HitTarget::ViewerVideoSeekBar, z);

    // Row 2: Transport controls (Play/Pause, Time, Mute, Loop) + Zoom + Metadata
    let row_y = bar.y + 28.0;
    let btn_h = 24.0;
    let mut x = bar.x + 8.0;

    // Play / Pause
    let play_rect = RectF::new(x, row_y, 28.0, btn_h);
    let (pfill, pborder) = interaction
        .button_surface(&HitTarget::ViewerVideoPlayPause, vid.playing, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
    draw_list.push_rect(play_rect, pfill, pborder, 1.0, r, None);
    draw_list.push_text(
        play_rect,
        if vid.playing { "❚❚" } else { "▶" },
        if vid.playing {
            tokens.accent_primary
        } else {
            tokens.fg_primary
        },
        FontKind::Sans,
        11.5,
        FontWeight::Bold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(play_rect, HitTarget::ViewerVideoPlayPause, z);
    x = play_rect.right() + 6.0;

    // Elapsed / Total timestamp (`0:04 / 0:28`)
    let time_label = format!(
        "{} / {}",
        VideoViewerState::format_timestamp(vid.position_secs),
        VideoViewerState::format_timestamp(vid.duration_secs)
    );
    let time_w = (time_label.len() as f32 * 6.4 + 2.0).max(66.0);
    draw_list.push_text(
        RectF::new(x, row_y, time_w, btn_h),
        time_label,
        tokens.fg_primary,
        FontKind::Mono,
        10.5,
        FontWeight::Medium,
        TextAlign::Left,
        false,
    );
    x += time_w + 4.0;

    // Mute toggle
    let mute_rect = RectF::new(x, row_y, 36.0, btn_h);
    let (mfill, mborder) = interaction
        .button_surface(&HitTarget::ViewerVideoMuteToggle, vid.muted, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
    draw_list.push_rect(mute_rect, mfill, mborder, 1.0, r, None);
    draw_list.push_text(
        mute_rect,
        if vid.muted { "Mute" } else { "Vol" },
        if vid.muted {
            tokens.status_error
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        10.5,
        FontWeight::Medium,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(mute_rect, HitTarget::ViewerVideoMuteToggle, z);
    x = mute_rect.right() + 3.0;

    // Loop toggle
    let loop_rect = RectF::new(x, row_y, 38.0, btn_h);
    let (lfill, lborder) = interaction
        .button_surface(&HitTarget::ViewerVideoLoopToggle, vid.looping, tokens)
        .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
    draw_list.push_rect(loop_rect, lfill, lborder, 1.0, r, None);
    draw_list.push_text(
        loop_rect,
        "Loop",
        if vid.looping {
            tokens.accent_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        10.5,
        FontWeight::Medium,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(loop_rect, HitTarget::ViewerVideoLoopToggle, z);
    x = loop_rect.right() + 6.0;

    // Compact zoom controls: [-] [Fit] [100%] [+]
    if bar.right() - x >= 116.0 {
        let z_buttons: [(&str, f32, HitTarget, bool); 4] = [
            ("-", 20.0, HitTarget::ViewerZoomOut, false),
            ("Fit", 26.0, HitTarget::ViewerZoomFit, vid.viewport.fit_mode),
            (
                "100%",
                36.0,
                HitTarget::ViewerZoomReset,
                !vid.viewport.fit_mode && (effective_zoom - 1.0).abs() < 0.01,
            ),
            ("+", 20.0, HitTarget::ViewerZoomIn, false),
        ];
        for (label, w, hit, active) in z_buttons {
            let rect = RectF::new(x, row_y, w, btn_h);
            let (zf, zb) = interaction
                .button_surface(&hit, active, tokens)
                .unwrap_or((tokens.surface_row_hover, Some(tokens.border_hairline)));
            draw_list.push_rect(rect, zf, zb, 1.0, r, None);
            draw_list.push_text(
                rect,
                label,
                if active {
                    tokens.accent_primary
                } else {
                    tokens.fg_secondary
                },
                FontKind::Mono,
                10.0,
                FontWeight::Medium,
                TextAlign::Center,
                false,
            );
            draw_list.push_hit(rect, hit, z);
            x = rect.right() + 2.0;
        }
    }

    // Metadata (`1920×1080 · 60fps · H.264`) on far right
    let meta = vid.metadata_summary();
    let meta_x = x + 4.0;
    let meta_w = (bar.right() - meta_x - 8.0).max(0.0);
    if meta_w > 48.0 && !meta.is_empty() {
        draw_list.push_text(
            RectF::new(meta_x, row_y, meta_w, btn_h),
            meta,
            tokens.fg_muted,
            FontKind::Mono,
            10.5,
            FontWeight::Regular,
            TextAlign::Right,
            true,
        );
    }

    scrub_hit_rect
}

fn render_checkerboard(draw_list: &mut UiDrawList, rect: RectF, is_dark: bool) {
    if rect.is_empty() {
        return;
    }
    let (c1, c2) = if is_dark {
        (Rgba::new(34, 34, 38, 255), Rgba::new(46, 46, 52, 255))
    } else {
        (Rgba::new(228, 230, 235, 255), Rgba::new(244, 245, 248, 255))
    };
    draw_list.push_rect(rect, c1, None, 0.0, 0.0, None);

    // Choose adaptive tile size so total dark/light overlay quads stay bounded (<= ~320 quads)
    let tile = ((rect.w * rect.h / 320.0).sqrt()).clamp(12.0, 48.0);
    let cols = (rect.w / tile).ceil() as usize;
    let rows = (rect.h / tile).ceil() as usize;
    for r in 0..rows {
        let ty = rect.y + r as f32 * tile;
        let th = (rect.bottom() - ty).min(tile);
        if th <= 0.0 {
            break;
        }
        for c in 0..cols {
            if (r + c) % 2 == 0 {
                continue;
            }
            let tx = rect.x + c as f32 * tile;
            let tw = (rect.right() - tx).min(tile);
            if tw <= 0.0 {
                break;
            }
            draw_list.push_rect(RectF::new(tx, ty, tw, th), c2, None, 0.0, 0.0, None);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkdownSpanKind {
    Text,
    Bold,
    Italic,
    BoldItalic,
    Code,
    Link { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkdownInlineSeg {
    text: String,
    kind: MarkdownSpanKind,
}

fn build_text_rows(
    txt: &TextViewerState,
    formatted_markdown: bool,
    max_chars: usize,
) -> Vec<TextRenderRow> {
    if txt.diff_mode
        && let Some(diff) = &txt.git_diff
    {
        return build_diff_rows(txt, diff, max_chars);
    }

    let mut rows = Vec::with_capacity(txt.lines.len());
    let mut idx = 0usize;

    while idx < txt.lines.len() {
        let line = &txt.lines[idx];
        let line_no = idx + 1;
        let is_target = txt.highlight_line == Some(line_no);

        if formatted_markdown {
            let trimmed = line.trim_start();
            // 1. Fenced code block: ```lang ... ``` or ~~~lang ... ~~~ (3+ matching fence chars)
            if let Some((fence_char, fence_len, after_fence)) = parse_markdown_fence(trimmed) {
                let lang_tag = after_fence.trim().to_owned();
                let block_lang = SyntaxLanguage::from_hint(&lang_tag);
                let mut block_lines = Vec::new();
                let mut block_line_nos = Vec::new();
                idx += 1;
                while idx < txt.lines.len() {
                    let inner = &txt.lines[idx];
                    if is_closing_markdown_fence(inner.trim_start(), fence_char, fence_len) {
                        idx += 1;
                        break;
                    }
                    block_lines.push(inner.clone());
                    block_line_nos.push(idx + 1);
                    idx += 1;
                }
                let tokenized = syntax::tokenize_lines(&block_lines, block_lang);
                for (bi, raw_code) in block_lines.iter().enumerate() {
                    let bln = block_line_nos[bi];
                    let btarg = txt.highlight_line == Some(bln);
                    let badge = (bi == 0 && !lang_tag.is_empty()).then(|| lang_tag.clone());
                    let expanded = syntax::expand_tabs(raw_code);
                    let toks = tokenized.get(bi).map_or(&[][..], Vec::as_slice);
                    push_wrapped_code_rows(
                        &mut rows,
                        bln,
                        &expanded,
                        toks,
                        badge,
                        btarg,
                        true,
                        txt.word_wrap,
                        max_chars,
                    );
                }
                continue;
            }

            // 2. GFM Pipe Table: header + delimiter `| :--- | :---: | ---: |` + body rows
            if line.contains('|')
                && idx + 1 < txt.lines.len()
                && let Some(alignments) = parse_gfm_table_separator(&txt.lines[idx + 1])
            {
                let header_cells = parse_gfm_table_cells(line, alignments.len());
                let header_target = is_target;
                let mut body_rows: Vec<(Vec<String>, bool)> = Vec::new();
                let mut ti = idx + 2;
                while ti < txt.lines.len() {
                    let trow = &txt.lines[ti];
                    let ttrim = trow.trim();
                    if ttrim.is_empty() || !ttrim.contains('|') {
                        break;
                    }
                    let cells = parse_gfm_table_cells(trow, alignments.len());
                    let row_targ = txt.highlight_line == Some(ti + 1);
                    body_rows.push((cells, row_targ));
                    ti += 1;
                }
                let mut col_weights = vec![4usize; alignments.len()];
                for (ci, c) in header_cells.iter().enumerate() {
                    col_weights[ci] = col_weights[ci].max(c.chars().count()).min(36);
                }
                for (cells, _) in &body_rows {
                    for (ci, c) in cells.iter().enumerate() {
                        if ci < col_weights.len() {
                            col_weights[ci] = col_weights[ci].max(c.chars().count()).min(36);
                        }
                    }
                }
                rows.push(TextRenderRow::MarkdownTableRow {
                    cells: header_cells,
                    alignments: alignments.clone(),
                    col_weights: col_weights.clone(),
                    is_header: true,
                    is_even_row: false,
                    is_target: header_target,
                });
                for (ri, (cells, row_targ)) in body_rows.into_iter().enumerate() {
                    rows.push(TextRenderRow::MarkdownTableRow {
                        cells,
                        alignments: alignments.clone(),
                        col_weights: col_weights.clone(),
                        is_header: false,
                        is_even_row: ri % 2 == 1,
                        is_target: row_targ,
                    });
                }
                idx = ti;
                continue;
            }

            // 3. Standalone Markdown image `![alt](src)`
            if let Some((alt, src)) = parse_standalone_markdown_image(trimmed) {
                rows.push(TextRenderRow::MarkdownImage {
                    alt,
                    src,
                    is_target,
                });
                idx += 1;
                continue;
            }

            if trimmed == "---" || trimmed == "***" || trimmed == "___" {
                rows.push(TextRenderRow::MarkdownRule);
                idx += 1;
                continue;
            }
            if let Some((level, heading)) = parse_markdown_heading(trimmed) {
                rows.push(TextRenderRow::MarkdownHeading {
                    level,
                    text: heading.to_owned(),
                    is_target,
                });
                idx += 1;
                continue;
            }
            if let Some(quoted) = trimmed.strip_prefix("> ") {
                for chunk in wrap_line_chunks(quoted, txt.word_wrap, max_chars) {
                    rows.push(TextRenderRow::MarkdownQuote {
                        text: chunk,
                        is_target,
                    });
                }
                for (alt, src) in extract_markdown_image_refs(quoted) {
                    rows.push(TextRenderRow::MarkdownImage {
                        alt,
                        src,
                        is_target,
                    });
                }
                idx += 1;
                continue;
            }
            if let Some((marker, body_text, indent)) = parse_markdown_list(line) {
                for (ci, chunk) in wrap_line_chunks(body_text, txt.word_wrap, max_chars)
                    .into_iter()
                    .enumerate()
                {
                    rows.push(TextRenderRow::MarkdownListItem {
                        marker: if ci == 0 {
                            marker.clone()
                        } else {
                            String::new()
                        },
                        text: chunk,
                        indent,
                        is_target,
                    });
                }
                for (alt, src) in extract_markdown_image_refs(body_text) {
                    rows.push(TextRenderRow::MarkdownImage {
                        alt,
                        src,
                        is_target,
                    });
                }
                idx += 1;
                continue;
            }
            for chunk in wrap_line_chunks(line, txt.word_wrap, max_chars) {
                rows.push(TextRenderRow::MarkdownParagraph {
                    text: chunk,
                    is_target,
                });
            }
            for (alt, src) in extract_markdown_image_refs(line) {
                rows.push(TextRenderRow::MarkdownImage {
                    alt,
                    src,
                    is_target,
                });
            }
            idx += 1;
        } else {
            let expanded = syntax::expand_tabs(line);
            let toks = txt.syntax_tokens.get(idx).map_or(&[][..], Vec::as_slice);
            push_wrapped_code_rows(
                &mut rows,
                line_no,
                &expanded,
                toks,
                None,
                is_target,
                false,
                txt.word_wrap,
                max_chars,
            );
            idx += 1;
        }
    }

    rows
}

fn build_diff_rows(
    txt: &TextViewerState,
    diff: &GitFileDiff,
    max_chars: usize,
) -> Vec<TextRenderRow> {
    let mut rows = Vec::new();
    let lang = txt
        .language
        .as_deref()
        .map(SyntaxLanguage::from_hint)
        .unwrap_or(SyntaxLanguage::Plain);

    for hunk in &diff.hunks {
        rows.push(TextRenderRow::DiffHunkHeader {
            text: hunk.header.clone(),
        });
        let raw_lines: Vec<String> = hunk.lines.iter().map(|l| l.text.clone()).collect();
        let tokenized = syntax::tokenize_lines(&raw_lines, lang);
        for (li, dline) in hunk.lines.iter().enumerate() {
            let expanded = syntax::expand_tabs(&dline.text);
            let line_toks = tokenized.get(li).map_or(&[][..], Vec::as_slice);
            let is_target = dline
                .new_line
                .is_some_and(|n| txt.highlight_line == Some(n));
            for (ci, (chunk, bs, be)) in
                wrap_line_chunks_with_offsets(&expanded, txt.word_wrap, max_chars)
                    .into_iter()
                    .enumerate()
            {
                let sliced = slice_tokens_rebased(line_toks, bs, be);
                rows.push(TextRenderRow::DiffCodeLine {
                    old_line: (ci == 0).then_some(dline.old_line).flatten(),
                    new_line: (ci == 0).then_some(dline.new_line).flatten(),
                    kind: dline.kind,
                    text: chunk,
                    tokens: sliced,
                    is_target,
                });
            }
        }
    }
    if rows.is_empty() {
        rows.push(TextRenderRow::DiffHunkHeader {
            text: "No diff hunks to display".to_owned(),
        });
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn push_wrapped_code_rows(
    rows: &mut Vec<TextRenderRow>,
    line_no: usize,
    expanded_line: &str,
    line_tokens: &[SyntaxToken],
    first_lang_badge: Option<String>,
    is_target: bool,
    in_code_block: bool,
    word_wrap: bool,
    max_chars: usize,
) {
    let chunks = wrap_line_chunks_with_offsets(expanded_line, word_wrap, max_chars);
    for (ci, (chunk, bs, be)) in chunks.into_iter().enumerate() {
        let sliced = slice_tokens_rebased(line_tokens, bs, be);
        rows.push(TextRenderRow::CodeLine {
            line_no: (ci == 0).then_some(line_no),
            text: chunk,
            tokens: sliced,
            lang_badge: if ci == 0 {
                first_lang_badge.clone()
            } else {
                None
            },
            is_target,
            in_code_block,
        });
    }
}

fn slice_tokens_rebased(
    tokens: &[SyntaxToken],
    slice_start: usize,
    slice_end: usize,
) -> Vec<SyntaxToken> {
    if slice_start == 0 && tokens.last().is_none_or(|t| t.end <= slice_end) {
        return tokens.to_vec();
    }
    let mut out = Vec::new();
    for t in tokens {
        if t.end <= slice_start {
            continue;
        }
        if t.start >= slice_end {
            break;
        }
        let s = t.start.max(slice_start) - slice_start;
        let e = t.end.min(slice_end) - slice_start;
        if e > s {
            out.push(SyntaxToken {
                start: s,
                end: e,
                kind: t.kind,
            });
        }
    }
    out
}

fn wrap_line_chunks_with_offsets(
    line: &str,
    word_wrap: bool,
    max_chars: usize,
) -> Vec<(String, usize, usize)> {
    if !word_wrap || line.chars().count() <= max_chars {
        return vec![(line.to_owned(), 0, line.len())];
    }
    let mut out = Vec::new();
    let mut chunk_start = 0usize;
    let mut char_count = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        char_count += 1;
        if char_count >= max_chars {
            let chunk_end = byte_idx + ch.len_utf8();
            out.push((
                line[chunk_start..chunk_end].to_owned(),
                chunk_start,
                chunk_end,
            ));
            chunk_start = chunk_end;
            char_count = 0;
        }
    }
    if chunk_start < line.len() || out.is_empty() {
        out.push((line[chunk_start..].to_owned(), chunk_start, line.len()));
    }
    out
}

fn wrap_line_chunks(line: &str, word_wrap: bool, max_chars: usize) -> Vec<String> {
    if !word_wrap || line.chars().count() <= max_chars {
        return vec![line.to_owned()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in line.chars() {
        current.push(ch);
        count += 1;
        if count >= max_chars {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

fn parse_markdown_heading(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) {
        let rest = trimmed[hashes..].strip_prefix(' ')?;
        Some((hashes as u8, rest.trim()))
    } else {
        None
    }
}

fn parse_markdown_list(line: &str) -> Option<(String, &str, f32)> {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    let trimmed = &line[leading_spaces..];
    let indent = (leading_spaces / 2) as f32 * 14.0;
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return Some(("•".to_owned(), rest, indent));
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let after = &trimmed[digits..];
        if let Some(rest) = after.strip_prefix(". ") {
            return Some((format!("{}.", &trimmed[..digits]), rest, indent));
        }
    }
    None
}

fn parse_gfm_table_cells(line: &str, min_cols: usize) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells: Vec<String> = inner.split('|').map(|c| c.trim().to_owned()).collect();
    while cells.len() < min_cols {
        cells.push(String::new());
    }
    cells
}

fn parse_gfm_table_separator(line: &str) -> Option<Vec<TextAlign>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') || !trimmed.contains('-') {
        return None;
    }
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut aligns = Vec::new();
    for part in inner.split('|') {
        let cell = part.trim();
        if cell.is_empty() {
            return None;
        }
        let left_colon = cell.starts_with(':');
        let right_colon = cell.ends_with(':');
        let core = cell.trim_matches(':');
        if core.is_empty() || !core.bytes().all(|b| b == b'-') {
            return None;
        }
        aligns.push(match (left_colon, right_colon) {
            (true, true) => TextAlign::Center,
            (false, true) => TextAlign::Right,
            _ => TextAlign::Left,
        });
    }
    (!aligns.is_empty()).then_some(aligns)
}

/// Parses an opening Markdown code fence (` ```lang ` or ` ~~~lang `) at the start of `trimmed`.
#[must_use]
pub fn parse_markdown_fence(trimmed: &str) -> Option<(char, usize, &str)> {
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.bytes().take_while(|&b| b == first as u8).count();
    if count < 3 {
        return None;
    }
    let after = &trimmed[count..];
    if first == '`' && after.contains('`') {
        return None;
    }
    Some((first, count, after))
}

/// Returns `true` when `trimmed` is a valid closing fence for `(fence_char, min_len)`.
#[must_use]
pub fn is_closing_markdown_fence(trimmed: &str, fence_char: char, min_len: usize) -> bool {
    let count = trimmed
        .bytes()
        .take_while(|&b| b == fence_char as u8)
        .count();
    count >= min_len && trimmed[count..].trim().is_empty()
}

/// Normalizes a raw Markdown image or link target (`<path> "title"`, `path "title"`, or
/// `dir/img(1).png`) into the clean destination path/URL without angle brackets or title suffixes.
#[must_use]
pub fn parse_markdown_image_target(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(after_lt) = trimmed.strip_prefix('<') {
        let mut out = String::with_capacity(after_lt.len());
        let mut chars = after_lt.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next_ch) = chars.next() {
                    out.push(next_ch);
                }
            } else if ch == '>' {
                break;
            } else {
                out.push(ch);
            }
        }
        return out.trim().to_owned();
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next_ch) = chars.next() {
                out.push(next_ch);
            }
        } else if ch.is_whitespace() {
            break;
        } else {
            out.push(ch);
        }
    }
    out
}

fn parse_standalone_markdown_image(trimmed: &str) -> Option<(String, String)> {
    if !trimmed.starts_with("![") {
        return None;
    }
    let (label, target, end_i) = parse_markdown_link_or_img(trimmed, 0, true)?;
    if trimmed[end_i..].trim().is_empty() {
        let cleaned = parse_markdown_image_target(target);
        (!cleaned.is_empty()).then(|| (label.to_owned(), cleaned))
    } else {
        None
    }
}

/// Extracts all inline `![alt](target)` image references from a single Markdown line,
/// tracking balanced parentheses, skipping inline code spans, and normalizing targets
/// identically across `ui-chrome` and `omarex::media_viewer`.
#[must_use]
pub fn extract_markdown_image_refs(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1 + line[i + 1..].chars().next().map_or(0, char::len_utf8);
            continue;
        }
        if bytes[i] == b'`'
            && let Some(rel_end) = line[i + 1..].find('`')
        {
            i += rel_end + 2;
            continue;
        }
        if bytes[i] == b'!'
            && bytes.get(i + 1) == Some(&b'[')
            && let Some((alt, target, next_i)) = parse_markdown_link_or_img(line, i, true)
        {
            let cleaned = parse_markdown_image_target(target);
            if !cleaned.is_empty() {
                out.push((alt.to_owned(), cleaned));
            }
            i = next_i;
            continue;
        }
        i += line[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

fn parse_markdown_inline_segments(input: &str) -> Vec<MarkdownInlineSeg> {
    let mut segs: Vec<MarkdownInlineSeg> = Vec::new();
    let push_seg = |segs: &mut Vec<MarkdownInlineSeg>, text: &str, kind: MarkdownSpanKind| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = segs.last_mut()
            && last.kind == kind
        {
            last.text.push_str(text);
        } else {
            segs.push(MarkdownInlineSeg {
                text: text.to_owned(),
                kind,
            });
        }
    };

    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // 1. Inline code `...`
        if bytes[i] == b'`'
            && let Some(rel_end) = input[i + 1..].find('`')
        {
            let inner = &input[i + 1..i + 1 + rel_end];
            push_seg(&mut segs, inner, MarkdownSpanKind::Code);
            i += rel_end + 2;
            continue;
        }
        // 2. Bold+Italic ***...***
        if bytes[i..].starts_with(b"***")
            && let Some(rel_end) = input[i + 3..].find("***")
        {
            let inner = &input[i + 3..i + 3 + rel_end];
            let (clean, _) = extract_markdown_inline_spans(inner);
            push_seg(&mut segs, &clean, MarkdownSpanKind::BoldItalic);
            i += rel_end + 6;
            continue;
        }
        // 3. Bold **...**
        if bytes[i..].starts_with(b"**")
            && let Some(rel_end) = input[i + 2..].find("**")
        {
            let inner = &input[i + 2..i + 2 + rel_end];
            let (clean, _) = extract_markdown_inline_spans(inner);
            push_seg(&mut segs, &clean, MarkdownSpanKind::Bold);
            i += rel_end + 4;
            continue;
        }
        // 4. Italic *...* (not followed by space) or _..._ (not inside identifier)
        if bytes[i] == b'*'
            && bytes.get(i + 1).is_some_and(|b| !b.is_ascii_whitespace())
            && let Some(rel_end) = input[i + 1..].find('*')
            && rel_end > 0
        {
            let inner = &input[i + 1..i + 1 + rel_end];
            push_seg(&mut segs, inner, MarkdownSpanKind::Italic);
            i += rel_end + 2;
            continue;
        }
        let prev_alnum = i > 0 && bytes[i - 1].is_ascii_alphanumeric();
        if bytes[i] == b'_'
            && !prev_alnum
            && bytes.get(i + 1).is_some_and(|b| !b.is_ascii_whitespace())
            && let Some(rel_end) = input[i + 1..].find('_')
            && rel_end > 0
            && !bytes
                .get(i + 1 + rel_end + 1)
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            let inner = &input[i + 1..i + 1 + rel_end];
            push_seg(&mut segs, inner, MarkdownSpanKind::Italic);
            i += rel_end + 2;
            continue;
        }
        // 5. Image ![alt](src) or Link [label](url)
        let is_img = bytes[i] == b'!' && bytes.get(i + 1) == Some(&b'[');
        if (is_img || bytes[i] == b'[')
            && let Some((label, target, next_i)) = parse_markdown_link_or_img(input, i, is_img)
        {
            let (clean, _) = extract_markdown_inline_spans(label);
            let kind = if is_img {
                MarkdownSpanKind::Text
            } else {
                MarkdownSpanKind::Link {
                    url: parse_markdown_image_target(target),
                }
            };
            push_seg(&mut segs, &clean, kind);
            i = next_i;
            continue;
        }

        let ch = input[i..].chars().next().expect("valid utf-8 boundary");
        let ch_len = ch.len_utf8();
        push_seg(&mut segs, &input[i..i + ch_len], MarkdownSpanKind::Text);
        i += ch_len;
    }
    segs
}

/// Strips inline backtick, `**bold**` (including nested `` **`code`** ``), `[label](url)`,
/// and `![alt](src)` markdown syntax cleanly and returns sorted non-overlapping byte ranges to highlight.
fn extract_markdown_inline_spans(input: &str) -> (String, Vec<Range<usize>>) {
    let mut out = String::with_capacity(input.len());
    let mut spans = Vec::new();
    parse_markdown_inline_into(input, &mut out, &mut spans, false);
    (out, spans)
}

fn parse_markdown_inline_into(
    input: &str,
    out: &mut String,
    spans: &mut Vec<Range<usize>>,
    inside_highlight: bool,
) {
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // 1. Inline code span: `...`
        if bytes[i] == b'`'
            && let Some(rel_end) = input[i + 1..].find('`')
        {
            let start = out.len();
            let inner = &input[i + 1..i + 1 + rel_end];
            out.push_str(inner);
            let end = out.len();
            if !inside_highlight && end > start {
                spans.push(start..end);
            }
            i += rel_end + 2;
            continue;
        }

        // 2. Bold span: **...** (recursively cleans nested `code` and [links](url))
        if bytes[i..].starts_with(b"**")
            && let Some(rel_end) = input[i + 2..].find("**")
        {
            let start = out.len();
            let inner = &input[i + 2..i + 2 + rel_end];
            parse_markdown_inline_into(inner, out, spans, true);
            let end = out.len();
            if !inside_highlight && end > start {
                spans.push(start..end);
            }
            i += rel_end + 4;
            continue;
        }

        // 3. Markdown image `![alt](src)` or link `[label](url)`
        let is_img = bytes[i] == b'!' && bytes.get(i + 1) == Some(&b'[');
        if (is_img || bytes[i] == b'[')
            && let Some((label, _target, next_i)) = parse_markdown_link_or_img(input, i, is_img)
        {
            let start = out.len();
            parse_markdown_inline_into(label, out, spans, true);
            let end = out.len();
            if !inside_highlight && end > start {
                spans.push(start..end);
            }
            i = next_i;
            continue;
        }

        let ch = input[i..].chars().next().expect("valid utf-8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
}

/// Parses `[label](url)` or `![alt](src)` starting at byte `start`, returning `(label, url, end_byte)`.
fn parse_markdown_link_or_img(
    input: &str,
    start: usize,
    is_img: bool,
) -> Option<(&str, &str, usize)> {
    let bytes = input.as_bytes();
    let label_start = start + if is_img { 2 } else { 1 };
    let mut depth = 1usize;
    let mut j = label_start;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' if j + 1 < bytes.len() => {
                j += 1 + input[j + 1..].chars().next().map_or(0, char::len_utf8);
            }
            b'[' => {
                depth += 1;
                j += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                j += 1;
            }
            _ => j += input[j..].chars().next().map_or(1, char::len_utf8),
        }
    }
    if depth != 0 || bytes.get(j + 1) != Some(&b'(') {
        return None;
    }
    let label = &input[label_start..j];
    let target_start = j + 2;
    let mut paren_depth = 1usize;
    let mut in_quote: Option<u8> = None;
    let mut k = target_start;
    while k < bytes.len() {
        let b = bytes[k];
        if b == b'\\' && k + 1 < bytes.len() {
            k += 1 + input[k + 1..].chars().next().map_or(0, char::len_utf8);
            continue;
        }
        if let Some(q) = in_quote {
            if b == q {
                in_quote = None;
            }
            k += input[k..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        match b {
            b'"' | b'\'' if k > target_start && bytes[k - 1].is_ascii_whitespace() => {
                in_quote = Some(b);
                k += 1;
            }
            b'(' => {
                paren_depth += 1;
                k += 1;
            }
            b')' => {
                paren_depth -= 1;
                if paren_depth == 0 {
                    let target = input[target_start..k].trim();
                    return Some((label, target, k + 1));
                }
                k += 1;
            }
            _ => k += input[k..].chars().next().map_or(1, char::len_utf8),
        }
    }
    None
}

fn split_lines_owned(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    text.lines().map(ToOwned::to_owned).collect()
}

fn estimate_text_content_height(line_count: usize) -> f32 {
    line_count as f32 * TEXT_LINE_HEIGHT + 24.0
}

fn filename_from_path(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// Formats a byte count cleanly for image/video/file metadata footers.
#[must_use]
pub fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        let kb = b / KB;
        if kb >= 100.0 {
            format!("{} KB", kb.round() as u64)
        } else {
            format!("{kb:.1} KB")
        }
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use omarchy_theme::Theme;

    use super::*;
    use crate::draw::{FocusGuard, FocusTarget, HitTarget, UiAction, ViewerAction};
    use crate::layout::{
        ChromeLayoutConfig, PaneDensity, VIEWER_MAX_RATIO, VIEWER_MIN_RATIO,
        compute_viewer_drag_ratio, layout_workspace_with_viewer,
    };
    use crate::{UiFrameInput, UiTree};

    #[test]
    fn split_layout_bounds_clamp_ratios_and_support_maximize() {
        let ws = RectF::new(248.0, 0.0, 1200.0, 900.0);
        let cfg = ChromeLayoutConfig {
            density: PaneDensity::Comfortable,
            ..ChromeLayoutConfig::default()
        };

        // Closed -> None
        assert!(layout_workspace_with_viewer(ws, false, false, 0.45, &cfg).is_none());

        // Open at default 0.45 -> disjoint [terminal | divider | viewer]
        let split = layout_workspace_with_viewer(ws, true, false, 0.45, &cfg).expect("open split");
        assert!((split.effective_ratio - 0.45).abs() < 1e-4);
        let div_vis = split.divider_visual_rect.expect("divider visual");
        let div_hit = split.divider_hit_rect.expect("divider hit");
        assert!(div_hit.w >= 6.0);
        assert!(split.terminal_workspace.w > 0.0);
        assert!(split.viewer_rect.w > 0.0);
        assert!(split.terminal_workspace.right() <= div_vis.x + 0.01);
        assert!(div_vis.right() <= split.viewer_rect.x + 0.01);
        assert!((split.viewer_rect.right() - (ws.right() - cfg.outer_margin())).abs() < 1.0);

        // Extreme ratios clamp cleanly to [0.22, 0.78]
        let low = layout_workspace_with_viewer(ws, true, false, 0.05, &cfg).unwrap();
        assert!((low.effective_ratio - VIEWER_MIN_RATIO).abs() < 1e-4);
        let high = layout_workspace_with_viewer(ws, true, false, 0.99, &cfg).unwrap();
        assert!((high.effective_ratio - VIEWER_MAX_RATIO).abs() < 1e-4);
        let nan_ratio = layout_workspace_with_viewer(ws, true, false, f32::NAN, &cfg).unwrap();
        assert!((nan_ratio.effective_ratio - VIEWER_DEFAULT_RATIO).abs() < 1e-4);

        // Compact density has 0 outer margin and 1px hairline divider
        let compact = ChromeLayoutConfig {
            density: PaneDensity::Compact,
            ..ChromeLayoutConfig::default()
        };
        let cs = layout_workspace_with_viewer(ws, true, false, 0.5, &compact).unwrap();
        assert_eq!(cs.corner_radius, 0.0);
        assert_eq!(cs.divider_visual_rect.unwrap().w, 1.0);
        assert_eq!(cs.viewer_rect.h, ws.h);

        // Maximized fills padded workspace and hides divider
        let max_split = layout_workspace_with_viewer(ws, true, true, 0.45, &cfg).unwrap();
        assert_eq!(max_split.terminal_workspace.w, 0.0);
        assert!(max_split.divider_visual_rect.is_none());
        assert!(max_split.divider_hit_rect.is_none());
        assert_eq!(max_split.viewer_rect, ws.inset_uniform(cfg.outer_margin()));
        assert_eq!(max_split.effective_ratio, 1.0);

        // Drag ratio mapping: dragging near right edge shrinks viewer; left edge grows viewer
        let drag_right = compute_viewer_drag_ratio(ws, ws.right() - 100.0);
        assert!((drag_right - VIEWER_MIN_RATIO).abs() < 1e-4);
        let drag_mid = compute_viewer_drag_ratio(ws, ws.x + ws.w * 0.5);
        assert!((drag_mid - 0.5).abs() < 1e-3);
        let drag_left = compute_viewer_drag_ratio(ws, ws.x + 10.0);
        assert!((drag_left - VIEWER_MAX_RATIO).abs() < 1e-4);
    }

    #[test]
    fn tab_lifecycle_deduplicates_switches_and_auto_closes_when_empty() {
        let mut st = ViewerState::new();
        assert!(!st.open);

        let id1 = st.open_loading_tab("/home/user/README.md", None, "Loading README.md…");
        assert!(st.open);
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.active_tab().unwrap().id, id1);
        assert_eq!(st.active_tab().unwrap().kind, ViewerTabKind::Markdown);

        // Opening same local path deduplicates and upgrades content
        let id1_again = st.open_or_focus_tab(
            "/home/user/README.md",
            None,
            ViewerContent::Text(TextViewerState::new("# Hello", Some("md".into()), true)),
        );
        assert_eq!(id1_again, id1);
        assert_eq!(st.tabs.len(), 1);
        assert!(matches!(
            st.active_tab().unwrap().content,
            ViewerContent::Text(_)
        ));

        // Same path on a remote host creates a distinct tab
        let id2 = st.open_loading_tab(
            "/home/user/README.md",
            Some("mac-mini".into()),
            "Downloading from mac-mini…",
        );
        assert_ne!(id2, id1);
        assert_eq!(st.tabs.len(), 2);
        assert_eq!(st.active_tab, 1);

        let id3 = st.open_or_focus_tab(
            "/tmp/video.mp4",
            None,
            ViewerContent::Video(VideoViewerState::new(
                42,
                1920,
                1080,
                28.0,
                Some(60.0),
                Some("H.264".into()),
                None,
            )),
        );
        assert_eq!(st.tabs.len(), 3);
        assert_eq!(st.active_tab().unwrap().id, id3);

        st.prev_tab();
        assert_eq!(st.active_tab().unwrap().id, id2);
        st.next_tab();
        assert_eq!(st.active_tab().unwrap().id, id3);

        // Closing middle tab shifts active index cleanly
        st.close_tab_by_id(id2).expect("remove id2");
        assert_eq!(st.tabs.len(), 2);
        assert_eq!(st.active_tab().unwrap().id, id3);

        // Closing remaining tabs auto-closes viewer pane & clears maximize
        st.toggle_maximized();
        assert!(st.maximized);
        st.close_tab_by_id(id3);
        st.close_tab_by_id(id1);
        assert!(st.tabs.is_empty());
        assert!(!st.open);
        assert!(!st.maximized);
    }

    #[test]
    fn media_zoom_fit_pan_and_uv_crop_math_are_exact() {
        let canvas = RectF::new(100.0, 50.0, 800.0, 600.0);

        // 1. Fit mode on 1600x1200 image -> exact 0.5x scale filling 800x600 with full UVs
        let fit = compute_media_placement(1600, 1200, canvas, 1.0, true, (0.0, 0.0)).unwrap();
        assert!((fit.effective_zoom - 0.5).abs() < 1e-4);
        assert_eq!(fit.dest_rect, canvas);
        assert_eq!(fit.uv_bounds, [0.0, 0.0, 1.0, 1.0]);

        // 2. Fit mode on wide 1600x400 image -> letterboxed vertically centered
        let wide = compute_media_placement(1600, 400, canvas, 1.0, true, (0.0, 0.0)).unwrap();
        assert!((wide.effective_zoom - 0.5).abs() < 1e-4);
        assert_eq!(wide.dest_rect, RectF::new(100.0, 250.0, 800.0, 200.0));
        assert_eq!(wide.uv_bounds, [0.0, 0.0, 1.0, 1.0]);

        // 3. 100% zoom on 1600x1200 inside 800x600 canvas (2x larger) -> centered 50% UV crop
        let centered_crop =
            compute_media_placement(1600, 1200, canvas, 1.0, false, (0.0, 0.0)).unwrap();
        assert_eq!(centered_crop.dest_rect, canvas);
        assert!((centered_crop.uv_bounds[0] - 0.25).abs() < 1e-4);
        assert!((centered_crop.uv_bounds[1] - 0.25).abs() < 1e-4);
        assert!((centered_crop.uv_bounds[2] - 0.75).abs() < 1e-4);
        assert!((centered_crop.uv_bounds[3] - 0.75).abs() < 1e-4);

        // 4. Excessive pan clamps cleanly to exact edge (u0 == 0.0, u1 == 0.5)
        let panned_left_edge =
            compute_media_placement(1600, 1200, canvas, 1.0, false, (9999.0, 9999.0)).unwrap();
        assert_eq!(panned_left_edge.clamped_pan, (400.0, 300.0));
        assert!((panned_left_edge.uv_bounds[0] - 0.0).abs() < 1e-4);
        assert!((panned_left_edge.uv_bounds[1] - 0.0).abs() < 1e-4);
        assert!((panned_left_edge.uv_bounds[2] - 0.5).abs() < 1e-4);
        assert!((panned_left_edge.uv_bounds[3] - 0.5).abs() < 1e-4);

        // 5. Viewport state zoom_in / zoom_out / reset / fit clamping [0.1, 16.0]
        let mut vp = MediaViewportState::default();
        vp.zoom_in(1600, 1200, Some(canvas));
        assert!(!vp.fit_mode);
        assert!((vp.zoom - 0.5 * ZOOM_STEP_FACTOR).abs() < 1e-4);
        for _ in 0..30 {
            vp.zoom_in(1600, 1200, Some(canvas));
        }
        assert_eq!(vp.zoom, MAX_ZOOM);
        for _ in 0..40 {
            vp.zoom_out(1600, 1200, Some(canvas));
        }
        assert_eq!(vp.zoom, MIN_ZOOM);
        vp.zoom_reset();
        assert_eq!(vp.zoom, 1.0);
        assert!(!vp.fit_mode);
        vp.zoom_fit();
        assert!(vp.fit_mode);
    }

    #[test]
    fn video_scrub_bar_time_mapping_and_formatting_work() {
        let bar = RectF::new(200.0, 800.0, 400.0, 18.0);
        assert_eq!(scrub_fraction_from_x(bar, 150.0), 0.0);
        assert!((scrub_fraction_from_x(bar, 300.0) - 0.25).abs() < 1e-4);
        assert!((scrub_fraction_from_x(bar, 400.0) - 0.5).abs() < 1e-4);
        assert_eq!(scrub_fraction_from_x(bar, 900.0), 1.0);

        assert!((scrub_time_from_x(bar, 300.0, 28.0) - 7.0).abs() < 1e-4);
        assert_eq!(scrub_time_from_x(bar, 300.0, 0.0), 0.0);

        let mut vid = VideoViewerState::new(
            77,
            1920,
            1080,
            28.0,
            Some(60.0),
            Some("H.264".into()),
            Some(5_400_000),
        );
        assert_eq!(vid.metadata_summary(), "1920×1080 · 60fps · H.264");
        assert_eq!(VideoViewerState::format_timestamp(4.2), "0:04");
        assert_eq!(VideoViewerState::format_timestamp(28.0), "0:28");
        assert_eq!(VideoViewerState::format_timestamp(3725.0), "1:02:05");

        let pos = vid.seek_to_fraction(0.5);
        assert!((pos - 14.0).abs() < 1e-4);
        assert!((vid.progress_fraction() - 0.5).abs() < 1e-4);
    }

    #[test]
    fn text_line_jump_scroll_offset_and_toggles_work() {
        let mut lines = Vec::new();
        for i in 1..=200 {
            lines.push(format!("fn line_{i}() {{ let s = \"hello\"; }}"));
        }
        let mut txt = TextViewerState::new(lines.join("\n"), Some("rs".into()), false);
        assert_eq!(txt.line_count(), 200);
        assert_eq!(txt.scroll_y, 0.0);

        // Jump to line 100 in a 400px viewport -> centers line comfortably above top
        txt.jump_to_line(100, Some(5), Some(400.0));
        assert_eq!(txt.highlight_line, Some(100));
        assert_eq!(txt.highlight_col, Some(5));
        let expected_line_top = 99.0 * TEXT_LINE_HEIGHT + 10.0;
        let expected_scroll = expected_line_top - 400.0 * 0.35;
        assert!((txt.scroll_y - expected_scroll).abs() < 1.0);

        // Jump to line 1 clamps to 0.0
        txt.jump_to_line(1, None, Some(400.0));
        assert_eq!(txt.scroll_y, 0.0);

        // Scroll past bottom clamps cleanly to max_scroll
        txt.scroll_by(999_999.0);
        assert_eq!(txt.scroll_y, txt.max_scroll());
        txt.scroll_to_fraction(0.5);
        assert!((txt.scroll_y - txt.max_scroll() * 0.5).abs() < 0.5);
    }

    #[test]
    fn focus_guard_and_ui_tree_viewer_interactions_pass_invariants() {
        let mut fg = FocusGuard::new(Some("blk:term".into()));
        fg.focus_viewer();
        assert_eq!(fg.current(), &FocusTarget::Viewer);
        assert!(fg.is_overlay_focused());

        // Background session_view_changed must NEVER steal focus from Viewer
        fg.on_session_view_changed(Some("blk:other".into()));
        assert_eq!(fg.current(), &FocusTarget::Viewer);
        assert_eq!(fg.last_terminal_block(), Some("blk:other"));

        fg.restore_terminal();
        assert_eq!(
            fg.current(),
            &FocusTarget::Terminal(Some("blk:other".into()))
        );

        // Full UiTree render + click round-trip across Text, Image, and Video tabs
        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };

        // 1. Markdown tab
        let md_id = tree.viewer.open_or_focus_tab(
            "/home/user/docs/guide.md",
            Some("mac-mini".into()),
            ViewerContent::Text(TextViewerState::new(
                "# Title\n\n> Quoted\n\n- Item with `code` and **bold**\n\n```rs\nfn main() {}\n```\n",
                Some("md".into()),
                true,
            )),
        );
        let out_md = tree.render(&input, &theme, Instant::now(), 0.25);
        out_md
            .draw_list
            .validate_invariants()
            .expect("markdown viewer draw list valid");
        assert!(out_md.viewer_split.is_some());
        assert!(out_md.viewer_images.is_empty());

        // Click Raw toggle -> switches markdown raw mode and focuses viewer
        let raw_hit = out_md
            .draw_list
            .hits
            .iter()
            .find(|h| h.target == HitTarget::ViewerRawToggle)
            .expect("raw toggle hit");
        let (rx, ry) = raw_hit.rect.center();
        assert_eq!(
            tree.handle_click(&out_md.draw_list, rx, ry, false),
            UiAction::Viewer(ViewerAction::ToggleRawMarkdown)
        );
        assert_eq!(tree.focus.current(), &FocusTarget::Viewer);

        // 2. Image tab -> emits exact ViewerImagePlacement
        let _img_id = tree.viewer.open_or_focus_tab(
            "/home/user/screenshot.png",
            None,
            ViewerContent::Image(ImageViewerState::new(
                101,
                1600,
                900,
                "PNG",
                Some(421_888),
                true,
            )),
        );
        let out_img = tree.render(&input, &theme, Instant::now(), 0.5);
        out_img
            .draw_list
            .validate_invariants()
            .expect("image viewer draw list valid");
        assert_eq!(out_img.viewer_images.len(), 1);
        assert_eq!(out_img.viewer_images[0].texture_key, 101);
        assert_eq!(out_img.viewer_images[0].uv_bounds, [0.0, 0.0, 1.0, 1.0]);

        // Click Zoom In
        let zin = out_img
            .draw_list
            .hits
            .iter()
            .find(|h| h.target == HitTarget::ViewerZoomIn)
            .expect("zoom in hit");
        let (zx, zy) = zin.rect.center();
        assert_eq!(
            tree.handle_click(&out_img.draw_list, zx, zy, false),
            UiAction::Viewer(ViewerAction::ZoomIn)
        );

        // 3. Video tab -> emits placement + center play overlay + scrub bar
        let vid_id = tree.viewer.open_or_focus_tab(
            "/home/user/demo.mp4",
            None,
            ViewerContent::Video(VideoViewerState::new(
                202,
                1920,
                1080,
                28.0,
                Some(60.0),
                Some("H.264".into()),
                Some(12_000_000),
            )),
        );
        let out_vid = tree.render(&input, &theme, Instant::now(), 0.75);
        out_vid
            .draw_list
            .validate_invariants()
            .expect("video viewer draw list valid");
        assert_eq!(out_vid.viewer_images.len(), 1);
        assert_eq!(out_vid.viewer_images[0].texture_key, 202);

        // Click scrub bar at midpoint -> seeks to ~14s
        let seek_hit = out_vid
            .draw_list
            .hits
            .iter()
            .find(|h| h.target == HitTarget::ViewerVideoSeekBar)
            .expect("seek bar hit");
        let (sx, sy) = seek_hit.rect.center();
        let action = tree.handle_click(&out_vid.draw_list, sx, sy, false);
        match action {
            UiAction::Viewer(ViewerAction::SeekVideo { tab_id, fraction }) => {
                assert_eq!(tab_id, vid_id);
                assert!((fraction - 0.5).abs() < 0.02);
            }
            other => panic!("expected SeekVideo, got {other:?}"),
        }

        // Click Close Pane -> closes viewer and restores terminal focus
        let close_hit = out_vid
            .draw_list
            .hits
            .iter()
            .find(|h| h.target == HitTarget::ViewerClose)
            .expect("close viewer hit");
        let (cx, cy) = close_hit.rect.center();
        assert_eq!(
            tree.handle_click(&out_vid.draw_list, cx, cy, false),
            UiAction::Viewer(ViewerAction::ClosePane)
        );
        assert!(!tree.viewer.open);
        assert!(matches!(tree.focus.current(), FocusTarget::Terminal(_)));

        // Re-selecting md_id re-opens cleanly
        assert!(tree.viewer.select_tab_by_id(md_id));
        assert!(tree.viewer.open);
    }

    #[test]
    fn loading_error_scrollbar_and_maximized_render_cleanly() {
        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: Some(crate::layout::SplitNode::leaf("blk:1".into())),
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };

        // 1. Loading state
        let load_id = tree.viewer.open_loading_tab(
            "/remote/video.mp4",
            Some("mac-mini".into()),
            "Downloading from mac-mini…",
        );
        let out_load = tree.render(&input, &theme, Instant::now(), 0.42);
        out_load
            .draw_list
            .validate_invariants()
            .expect("loading valid");
        assert_eq!(out_load.tiled_panes.len(), 1);
        assert!(
            out_load.tiled_panes[0].card_rect.right()
                <= out_load.viewer_split.unwrap().viewer_rect.x
        );

        // 2. Error state exposes Open External fallback hit target
        assert!(tree.viewer.set_tab_error(load_id, "permission denied"));
        let out_err = tree.render(&input, &theme, Instant::now(), 0.5);
        out_err
            .draw_list
            .validate_invariants()
            .expect("error valid");
        assert!(
            out_err
                .draw_list
                .hits
                .iter()
                .any(|h| h.target == HitTarget::ViewerOpenExternal)
        );

        // 3. Long code file with scrollbar track & maximized mode hiding terminal panes
        let long_code: String = (1..=120)
            .map(|i| format!("pub fn item_{i}() -> &'static str {{ \"value_{i}\" }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let code_id = tree.viewer.open_or_focus_tab(
            "/home/user/src/main.rs",
            None,
            ViewerContent::Text(TextViewerState::new(long_code, Some("rs".into()), false)),
        );
        tree.viewer.toggle_maximized();
        let out_max = tree.render(&input, &theme, Instant::now(), 0.0);
        out_max
            .draw_list
            .validate_invariants()
            .expect("maximized valid");
        assert!(out_max.tiled_panes.is_empty());
        tree.sync_viewer_metrics(&out_max.viewer_metrics);

        let sb_track = out_max
            .draw_list
            .hits
            .iter()
            .find(|h| h.target == HitTarget::ViewerScrollbarTrack)
            .expect("scrollbar track hit");
        let (tx, ty) = sb_track.rect.center();
        let act = tree.handle_click(&out_max.draw_list, tx, ty, false);
        match act {
            UiAction::Viewer(ViewerAction::ScrollToFraction { tab_id, fraction }) => {
                assert_eq!(tab_id, code_id);
                assert!((fraction - 0.5).abs() < 0.05);
            }
            other => panic!("expected ScrollToFraction, got {other:?}"),
        }
    }

    #[test]
    fn markdown_inline_strips_nested_bold_code_links_images_and_quotes() {
        let (clean, spans) = extract_markdown_inline_spans(
            "Built on **`wgpu` (Vulkan)** for [Rex](https://github.com/superlogical/rex) ![logo](img.png)",
        );
        assert_eq!(clean, "Built on wgpu (Vulkan) for Rex logo");
        assert!(!clean.contains('`'));
        assert!(!clean.contains("**"));
        assert!(!clean.contains("https://"));
        assert_eq!(spans.len(), 3);
        assert_eq!(&clean[spans[0].clone()], "wgpu (Vulkan)");
        assert_eq!(&clean[spans[1].clone()], "Rex");
        assert_eq!(&clean[spans[2].clone()], "logo");

        // Blockquote rows render stripped text with highlight spans (no raw ** or `)
        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };
        tree.viewer.open_or_focus_tab(
            "/home/user/README.md",
            None,
            ViewerContent::Text(TextViewerState::new(
                "> **Experimental Community Project** for `omarex` via [Rex](https://example.com)",
                Some("md".into()),
                true,
            )),
        );
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list.validate_invariants().expect("valid quote");
        let quote_spans = out
            .draw_list
            .primitives
            .iter()
            .find_map(|p| match p {
                crate::draw::UiPrimitive::RichText { spans, .. }
                    if spans
                        .iter()
                        .any(|s| s.text.contains("Experimental Community Project")) =>
                {
                    Some(spans)
                }
                _ => None,
            })
            .expect("formatted quote RichText primitive");
        let joined: String = quote_spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "Experimental Community Project for omarex via Rex");
        assert!(quote_spans.len() >= 3);
        assert!(out.draw_list.hits.iter().any(|h| matches!(
            &h.target,
            HitTarget::ViewerLink { url } if url == "https://example.com"
        )));
    }

    #[test]
    fn video_toolbar_metadata_fits_at_default_1440_split_without_truncation() {
        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };
        tree.viewer.open_or_focus_tab(
            "/home/user/demo.mp4",
            None,
            ViewerContent::Video(VideoViewerState::new(
                10,
                1920,
                1080,
                28.0,
                Some(60.0),
                Some("H.264".into()),
                None,
            )),
        );
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("valid video toolbar");
        let meta_rect = out
            .draw_list
            .primitives
            .iter()
            .find_map(|p| match p {
                crate::draw::UiPrimitive::Text { rect, text, .. }
                    if text == "1920×1080 · 60fps · H.264" =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .expect("video metadata text primitive");
        assert!(
            meta_rect.w >= 180.0,
            "expected >= 180px for full metadata at 1440x900 45% split, got {:.1}px",
            meta_rect.w
        );
    }

    #[test]
    fn f9_syntax_highlighter_multi_token_and_light_dark_contrast() {
        let lines = vec![
            "/* block".to_owned(),
            "   comment */".to_owned(),
            "pub async fn greet(name: &str) -> usize {".to_owned(),
            "    let count = 42u64; // line comment".to_owned(),
            "    \"hello\".len() + count as usize".to_owned(),
            "}".to_owned(),
        ];
        let tokenized = syntax::tokenize_lines(&lines, syntax::SyntaxLanguage::Rust);
        assert_eq!(tokenized.len(), lines.len());
        assert!(
            tokenized[0]
                .iter()
                .all(|t| t.kind == syntax::TokenKind::Comment)
        );
        assert!(
            tokenized[1]
                .iter()
                .all(|t| t.kind == syntax::TokenKind::Comment)
        );
        assert!(tokenized[2].len() >= 6);
        assert!(
            tokenized[2]
                .iter()
                .any(|t| t.kind == syntax::TokenKind::Keyword)
        );
        assert!(
            tokenized[2]
                .iter()
                .any(|t| t.kind == syntax::TokenKind::Function)
        );
        assert!(
            tokenized[3]
                .iter()
                .any(|t| t.kind == syntax::TokenKind::Number)
        );
        assert!(
            tokenized[3]
                .iter()
                .any(|t| t.kind == syntax::TokenKind::Comment)
        );
        assert!(
            tokenized[4]
                .iter()
                .any(|t| t.kind == syntax::TokenKind::String)
        );

        let dark = Theme::default();
        let light = Theme::from_parts(
            omarchy_theme::ThemePalette::default_light(),
            dark.rounding,
            dark.material,
        );
        let dark_tokens = dark.effective_ui();
        let light_tokens = light.effective_ui();
        let dk_kw = syntax::token_style(syntax::TokenKind::Keyword, &dark, &dark_tokens);
        let lt_kw = syntax::token_style(syntax::TokenKind::Keyword, &light, &light_tokens);
        assert_ne!(dk_kw.0, lt_kw.0);
        let dk_plain = syntax::token_style(syntax::TokenKind::Plain, &dark, &dark_tokens);
        let lt_plain = syntax::token_style(syntax::TokenKind::Plain, &light, &light_tokens);
        assert_ne!(dk_plain.0, lt_plain.0);
    }

    #[test]
    fn f11_git_diff_gutter_and_unified_mode_render_cleanly() {
        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };
        let mut txt = TextViewerState::new("fn a() {}\nfn b() {}\n", Some("rs".into()), false);
        let mut diff = GitFileDiff {
            has_staged: false,
            has_unstaged: true,
            is_untracked: false,
            added_count: 1,
            removed_count: 1,
            line_marks: HashMap::new(),
            deleted_after_lines: HashSet::new(),
            hunks: vec![GitDiffHunk {
                header: "@@ -1,2 +1,2 @@ fn a".to_owned(),
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 2,
                lines: vec![
                    GitDiffLine {
                        old_line: Some(1),
                        new_line: Some(1),
                        kind: DiffLineKind::Context,
                        text: "fn a() {}".to_owned(),
                    },
                    GitDiffLine {
                        old_line: Some(2),
                        new_line: None,
                        kind: DiffLineKind::Removed,
                        text: "fn old_b() {}".to_owned(),
                    },
                    GitDiffLine {
                        old_line: None,
                        new_line: Some(2),
                        kind: DiffLineKind::Added,
                        text: "fn b() {}".to_owned(),
                    },
                ],
            }],
        };
        diff.line_marks.insert(2, DiffGutterKind::UnstagedModified);
        diff.deleted_after_lines.insert(1);
        txt.set_git_diff(Some(diff));
        tree.viewer
            .open_or_focus_tab("/repo/src/lib.rs", None, ViewerContent::Text(txt));

        // 1. Breadcrumb exposes `Diff +1 -1` toggle hit target in normal code view.
        let out_inline = tree.render(&input, &theme, Instant::now(), 0.0);
        out_inline
            .draw_list
            .validate_invariants()
            .expect("valid inline diff view");
        assert!(
            out_inline
                .draw_list
                .hits
                .iter()
                .any(|h| h.target == HitTarget::ViewerDiffToggle)
        );
        assert!(out_inline.draw_list.primitives.iter().any(|p| {
            matches!(p, crate::draw::UiPrimitive::Text { text, .. } if text == "Diff +1 -1")
        }));

        // 2. Unified diff mode renders hunk header + removed/added lines.
        assert!(tree.viewer.toggle_diff_mode());
        let out_diff = tree.render(&input, &theme, Instant::now(), 0.0);
        out_diff
            .draw_list
            .validate_invariants()
            .expect("valid unified diff view");
        assert!(out_diff.draw_list.primitives.iter().any(|p| {
            matches!(p, crate::draw::UiPrimitive::Text { text, .. } if text.starts_with("@@ -1,2 +1,2 @@"))
        }));
    }

    #[test]
    fn f12_selection_clicks_search_and_path_line_work() {
        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };
        let mut txt = TextViewerState::new(
            "alpha beta_word gamma\nsecond beta_word line\nthird beta_word",
            Some("rs".into()),
            false,
        );
        txt.highlight_line = Some(2);
        assert_eq!(txt.target_or_active_line(), 2);
        txt.begin_selection(TextDocPos { row: 0, col: 8 }, 2);
        assert_eq!(txt.selected_text().as_deref(), Some("beta_word"));
        txt.begin_selection(TextDocPos { row: 1, col: 3 }, 3);
        assert_eq!(
            txt.selected_text().as_deref(),
            Some("second beta_word line")
        );
        txt.select_all();
        assert_eq!(
            txt.selected_text().as_deref(),
            Some("alpha beta_word gamma\nsecond beta_word line\nthird beta_word")
        );

        tree.viewer
            .open_or_focus_tab("/repo/main.rs", None, ViewerContent::Text(txt));
        tree.viewer.open_search();
        tree.viewer.search.input.set_text("beta_word");
        tree.viewer.recompute_search();
        assert_eq!(tree.viewer.search.matches.len(), 3);
        assert_eq!(tree.viewer.search.active_match, Some(0));
        tree.viewer.next_search_match();
        assert_eq!(tree.viewer.search.active_match, Some(1));
        tree.viewer.prev_search_match();
        assert_eq!(tree.viewer.search.active_match, Some(0));

        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("valid viewer with search bar");
        assert!(
            out.draw_list
                .hits
                .iter()
                .any(|h| h.target == HitTarget::ViewerSearchInput)
        );
        assert!(
            out.draw_list
                .hits
                .iter()
                .any(|h| h.target == HitTarget::ViewerCopyPathLine)
        );
    }

    #[test]
    fn f13_markdown_2_tables_code_inline_images_and_broken_fallback() {
        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };
        let md = "\
| Key | Align |
| :--- | ---: |
| `ctrl+f` | 100 |

```rust
let answer = 42;
```

![Diagram](diagram.png)
![Broken](missing.png)
";
        let mut txt = TextViewerState::new(md, Some("md".into()), true);
        txt.inline_images.insert(
            "diagram.png".to_owned(),
            MarkdownInlineImage::Loaded {
                texture_key: 0x9000_0001,
                width: 320,
                height: 160,
            },
        );
        tree.viewer
            .open_or_focus_tab("/repo/docs/guide.md", None, ViewerContent::Text(txt));
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        out.draw_list
            .validate_invariants()
            .expect("valid markdown 2.0");
        assert_eq!(out.viewer_images.len(), 1);
        assert_eq!(out.viewer_images[0].texture_key, 0x9000_0001);
        assert!(out.draw_list.primitives.iter().any(|p| {
            matches!(p, crate::draw::UiPrimitive::Text { text, .. } if text.eq_ignore_ascii_case("rust"))
        }));
        assert!(out.draw_list.primitives.iter().any(|p| {
            matches!(p, crate::draw::UiPrimitive::Text { text, .. } if text.contains("Broken"))
        }));
    }

    #[test]
    fn f14_audio_pdf_file_paths_and_last_tab_fade_out() {
        let audio_target = crate::file_paths::classify_path("assets/tone.wav", None, None, None);
        assert_eq!(
            audio_target,
            crate::file_paths::ResolvedTarget::Audio {
                path: "assets/tone.wav".into()
            }
        );
        assert!(audio_target.is_in_app_viewable());

        let parsed_pdf = crate::file_paths::parse_path_with_location("docs/sample.pdf:2");
        let pdf_target = crate::file_paths::classify_path(
            &parsed_pdf.path,
            parsed_pdf.line,
            parsed_pdf.end_line,
            parsed_pdf.column,
        );
        assert_eq!(
            pdf_target,
            crate::file_paths::ResolvedTarget::Pdf {
                path: "docs/sample.pdf".into(),
                page: Some(2),
            }
        );
        assert_eq!(pdf_target.line(), Some(2));
        assert!(pdf_target.is_in_app_viewable());

        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };

        let aud_id = tree.viewer.open_or_focus_tab(
            "/tmp/tone.wav",
            None,
            ViewerContent::Audio(AudioViewerState::new(
                4.2,
                vec![0.2, 0.8, 0.5, 0.9],
                Some("pcm_s16le".into()),
                Some(44100),
                Some(2),
                Some(1411),
                Some(102400),
            )),
        );
        let out_aud = tree.render(&input, &theme, Instant::now(), 0.0);
        out_aud
            .draw_list
            .validate_invariants()
            .expect("valid audio");
        assert!(
            out_aud
                .draw_list
                .hits
                .iter()
                .any(|h| matches!(h.target, HitTarget::ViewerAudioSeekBar))
        );

        let pdf_id = tree.viewer.open_or_focus_tab(
            "/tmp/sample.pdf",
            None,
            ViewerContent::Pdf(PdfViewerState::new(
                0x8000_0002,
                2,
                5,
                612,
                792,
                Some(12345),
                None,
            )),
        );
        let out_pdf = tree.render(&input, &theme, Instant::now(), 0.0);
        out_pdf.draw_list.validate_invariants().expect("valid pdf");
        assert!(out_pdf.draw_list.primitives.iter().any(|p| {
            matches!(p, crate::draw::UiPrimitive::Text { text, .. } if text == "Page 2 / 5")
        }));

        // Close first tab -> stays open; close last tab -> schedules `closing_viewer` fade-out!
        assert!(tree.close_viewer_tab(aud_id).is_some());
        assert!(tree.viewer.open);
        assert!(tree.anim.closing_viewer.is_none());
        assert!(tree.close_viewer_tab(pdf_id).is_some());
        assert!(!tree.viewer.open);
        assert!(tree.anim.closing_viewer.is_some());
    }

    #[test]
    fn f11_toggle_diff_mode_preserves_target_scroll_and_gutter_bar() {
        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };
        let mut lines = Vec::new();
        for i in 1..=70 {
            lines.push(format!("let line_{i} = {i};"));
        }
        let mut txt = TextViewerState::new(lines.join("\n"), Some("rs".into()), false);
        let mut hunk1_lines = Vec::new();
        for i in 1..=20 {
            hunk1_lines.push(GitDiffLine {
                old_line: Some(i),
                new_line: Some(i),
                kind: if i == 5 {
                    DiffLineKind::Added
                } else {
                    DiffLineKind::Context
                },
                text: format!("let line_{i} = {i};"),
            });
        }
        let mut hunk2_lines = Vec::new();
        for i in 50..=65 {
            hunk2_lines.push(GitDiffLine {
                old_line: Some(i),
                new_line: Some(i),
                kind: if i == 59 {
                    DiffLineKind::Added
                } else {
                    DiffLineKind::Context
                },
                text: format!("let line_{i} = {i};"),
            });
        }
        let mut diff = GitFileDiff {
            has_staged: false,
            has_unstaged: true,
            is_untracked: false,
            added_count: 2,
            removed_count: 0,
            line_marks: HashMap::new(),
            deleted_after_lines: HashSet::new(),
            hunks: vec![
                GitDiffHunk {
                    header: "@@ -1,20 +1,20 @@".to_owned(),
                    old_start: 1,
                    old_count: 20,
                    new_start: 1,
                    new_count: 20,
                    lines: hunk1_lines,
                },
                GitDiffHunk {
                    header: "@@ -50,16 +50,16 @@".to_owned(),
                    old_start: 50,
                    old_count: 16,
                    new_start: 50,
                    new_count: 16,
                    lines: hunk2_lines,
                },
            ],
        };
        diff.line_marks.insert(59, DiffGutterKind::UnstagedModified);
        txt.set_git_diff(Some(diff));
        txt.jump_to_line(59, None, Some(220.0));
        assert!(txt.scroll_y > 100.0);

        // Entering diff mode scrolls to line 59 instead of resetting scroll_y = 0.0.
        assert!(txt.toggle_diff_mode());
        assert!(txt.diff_mode);
        assert!(
            txt.scroll_y > 0.0,
            "expected diff mode to stay scrolled near target line 59, got {}",
            txt.scroll_y
        );
        // Leaving diff mode restores scroll to target line 59.
        assert!(txt.toggle_diff_mode());
        assert!(!txt.diff_mode);
        assert!(txt.scroll_y > 100.0);

        // Target line 59 renders BOTH the target highlight (row tint + left bar w=2.5)
        // AND the git diff row tint + 3px git gutter bar (w=3.0).
        tree.viewer
            .open_or_focus_tab("/repo/src/lib.rs", None, ViewerContent::Text(txt));
        let out = tree.render(&input, &theme, Instant::now(), 0.0);
        let tokens = theme.effective_ui();
        let gutter_col = theme.palette.terminal.ansi[3].opaque();
        let has_target_row = out.draw_list.primitives.iter().any(|p| {
            matches!(
                p,
                crate::draw::UiPrimitive::RoundedRect { rect, fill, .. }
                    if rect.w > 100.0 && *fill == tokens.accent_primary.with_alpha_f32(0.16)
            )
        });
        let has_target_bar = out.draw_list.primitives.iter().any(|p| {
            matches!(
                p,
                crate::draw::UiPrimitive::RoundedRect { rect, fill, .. }
                    if (rect.w - 2.5).abs() < 0.01 && *fill == tokens.accent_primary
            )
        });
        let has_git_diff_row_tint = out.draw_list.primitives.iter().any(|p| {
            matches!(
                p,
                crate::draw::UiPrimitive::RoundedRect { rect, fill, .. }
                    if rect.w > 100.0 && *fill == gutter_col.with_alpha_f32(0.08)
            )
        });
        let has_git_gutter_bar = out.draw_list.primitives.iter().any(|p| {
            matches!(
                p,
                crate::draw::UiPrimitive::RoundedRect { rect, fill, .. }
                    if (rect.w - 3.0).abs() < 0.01 && *fill == gutter_col
            )
        });
        assert!(has_target_row, "expected target row highlight");
        assert!(has_target_bar, "expected target left highlight bar");
        assert!(
            has_git_diff_row_tint,
            "expected git diff row tint on target line 59"
        );
        assert!(
            has_git_gutter_bar,
            "expected 3px git gutter bar on target line 59"
        );
    }

    #[test]
    fn f12_unicode_expanding_lowercase_search_and_exact_slices() {
        let mut tree = UiTree::new();
        let txt = TextViewerState::new("İstanbul alpha\nİkinci ALPHA", None, false);
        tree.viewer
            .open_or_focus_tab("/repo/uni.txt", None, ViewerContent::Text(txt));
        tree.viewer.open_search();
        tree.viewer.search.input.set_text("alpha");
        tree.viewer.recompute_search();
        assert_eq!(tree.viewer.search.matches.len(), 2);

        let lines = match &tree.viewer.active_tab().unwrap().content {
            ViewerContent::Text(t) => t.lines.clone(),
            _ => unreachable!(),
        };
        let m0 = &tree.viewer.search.matches[0];
        let m1 = &tree.viewer.search.matches[1];
        assert_eq!(&lines[0][m0.start_byte..m0.end_byte], "alpha");
        assert_eq!(&lines[1][m1.start_byte..m1.end_byte], "ALPHA");

        tree.viewer.search.input.set_text("İSTANBUL");
        tree.viewer.recompute_search();
        assert_eq!(tree.viewer.search.matches.len(), 1);
        let mi = &tree.viewer.search.matches[0];
        assert_eq!(&lines[0][mi.start_byte..mi.end_byte], "İstanbul");
    }

    #[test]
    fn f13_markdown_2_parens_titles_list_quote_images_tilde_fences_and_measurer_links() {
        struct MockMeasurer;
        impl UiTextMeasurer for MockMeasurer {
            fn measure_text(
                &mut self,
                text: &str,
                _size_px: f32,
                _bold: bool,
                _italic: bool,
                _mono: bool,
            ) -> f32 {
                text.chars()
                    .map(|c| {
                        if matches!(c, 'i' | 'l' | ' ') {
                            3.0
                        } else {
                            10.0
                        }
                    })
                    .sum()
            }
        }

        assert_eq!(
            parse_markdown_image_target("./img.png \"Blue Title\""),
            "./img.png"
        );
        assert_eq!(
            parse_markdown_image_target("dir/img(1).png 'Single Title'"),
            "dir/img(1).png"
        );
        assert_eq!(
            parse_markdown_image_target("<./my dir/a(1).png> \"T\""),
            "./my dir/a(1).png"
        );

        let mut tree = UiTree::new();
        let theme = Theme::default();
        let input = UiFrameInput {
            viewport_w: 1440.0,
            viewport_h: 900.0,
            hosts: Vec::new(),
            active_host_id: "local".into(),
            active_session_id: "sess:1".into(),
            active_session_label: "Main".into(),
            split_root: None,
            zoomed_pane: None,
            hovered_header_pane: None,
            floating_layers: Vec::new(),
            pane_metas: Vec::new(),
            dnd_overlay: None,
            fill_horizontal_tabs: false,
        };
        let md = "\
![Paren](img(1).png)
![Titled](titled.png \"Blue Title\")
- List item with ![InList](inlist.png)
> Quote with ![InQuote](inquote.png)

~~~rust
let tilde_code = 7;
~~~

iiiiilllll [Click Link](https://example.com \"Tip\") end
";
        let mut txt = TextViewerState::new(md, Some("md".into()), true);
        for (idx, src) in ["img(1).png", "titled.png", "inlist.png", "inquote.png"]
            .into_iter()
            .enumerate()
        {
            txt.inline_images.insert(
                src.to_owned(),
                MarkdownInlineImage::Loaded {
                    texture_key: 0x9000_0010 + idx as u64,
                    width: 64,
                    height: 32,
                },
            );
        }
        tree.viewer
            .open_or_focus_tab("/repo/docs/guide.md", None, ViewerContent::Text(txt));
        let mut measurer = MockMeasurer;
        let out =
            tree.render_with_measurer(&input, &theme, Instant::now(), 0.0, Some(&mut measurer));
        out.draw_list
            .validate_invariants()
            .expect("valid markdown 2.0");

        assert_eq!(
            out.viewer_images.len(),
            4,
            "expected standalone paren, titled, list, and quote images to all render"
        );
        assert!(out.draw_list.primitives.iter().any(|p| {
            matches!(p, crate::draw::UiPrimitive::Text { text, .. } if text.eq_ignore_ascii_case("rust"))
        }));
        assert!(out.draw_list.primitives.iter().any(|p| {
            matches!(
                p,
                crate::draw::UiPrimitive::RichText { spans, mono: true, .. }
                    if spans.iter().any(|s| s.text.contains("tilde_code"))
            )
        }));

        let link_hit = out
            .draw_list
            .hits
            .iter()
            .find(|h| matches!(&h.target, HitTarget::ViewerLink { url } if url == "https://example.com"))
            .expect("exact ViewerLink hit target");
        let expected_prefix_w: f32 = "iiiiilllll "
            .chars()
            .map(|c| {
                if matches!(c, 'i' | 'l' | ' ') {
                    3.0
                } else {
                    10.0
                }
            })
            .sum();
        let expected_link_w: f32 = "Click Link"
            .chars()
            .map(|c| {
                if matches!(c, 'i' | 'l' | ' ') {
                    3.0
                } else {
                    10.0
                }
            })
            .sum();
        let body_x = out.viewer_metrics.text_body_rect.unwrap().x;
        let expected_x = body_x + 14.0 + expected_prefix_w;
        assert!(
            (link_hit.rect.x - expected_x).abs() < 0.01,
            "expected exact measured link_x {expected_x}, got {}",
            link_hit.rect.x
        );
        assert!(
            (link_hit.rect.w - expected_link_w).abs() < 0.01,
            "expected exact measured link_w {expected_link_w}, got {}",
            link_hit.rect.w
        );
    }

    #[test]
    fn f10_set_text_defers_clamp_for_markdown_and_diff() {
        let mut md = TextViewerState::new("line\n", Some("md".into()), true);
        md.last_content_h = 1200.0;
        md.scroll_y = 600.0;
        md.set_text("# Updated\n![Img](big.png)\n");
        assert!(
            (md.scroll_y - 600.0).abs() < f32::EPSILON,
            "Markdown reload must defer scroll_y clamping until image heights are rendered"
        );
    }

    #[test]
    fn new_gap01_extract_markdown_image_refs_handles_escaped_multibyte_utf8_and_trailing_backslash()
    {
        assert!(extract_markdown_image_refs("foo \\€ bar").is_empty());
        assert!(extract_markdown_image_refs("umlaut \\ä end").is_empty());
        assert!(extract_markdown_image_refs("emoji \\🦀 end").is_empty());
        assert!(extract_markdown_image_refs("trailing backslash \\").is_empty());
        assert_eq!(
            extract_markdown_image_refs("before \\€ ![alt\\€](img/test.png \"\\🦀\") after"),
            vec![("alt\\€".to_owned(), "img/test.png".to_owned())]
        );
        let segs = parse_markdown_inline_segments("escaped \\€ [a\\€](b\\🦀) end");
        assert!(!segs.is_empty());
    }

    #[test]
    fn new_gap02_viewer_scrollbar_clamps_safely_on_short_viewport() {
        let mut state = ViewerState::new();
        let long_text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        state.open_or_focus_tab(
            "/repo/long.txt",
            None,
            ViewerContent::Text(TextViewerState::new(&long_text, Some("txt".into()), false)),
        );
        // Card height 85.0 -> 34.0 tab bar + 26.0 breadcrumb => body.h == 25.0 (track.h == 17.0 < 24.0).
        let split = ViewerSplitGeometry {
            terminal_workspace: RectF::new(0.0, 0.0, 400.0, 85.0),
            divider_visual_rect: None,
            divider_hit_rect: None,
            viewer_rect: RectF::new(400.0, 0.0, 400.0, 85.0),
            corner_radius: 0.0,
            effective_ratio: 0.5,
        };
        let mut dl = UiDrawList::new();
        let out = render_viewer(
            &mut dl,
            split,
            &state,
            true,
            true,
            1.0,
            UiInteraction::default(),
            &Theme::default(),
            0.0,
        );
        dl.validate_invariants()
            .expect("short viewer viewport draw list must be valid");
        let body = out.metrics.text_body_rect.expect("body rect present");
        assert!((body.h - 25.0).abs() < 0.1);
        let thumb_hit = dl
            .hits
            .iter()
            .find(|h| h.target == HitTarget::ViewerScrollbarThumb)
            .expect("scrollbar thumb hit");
        assert!(thumb_hit.rect.h <= body.h - 8.0 + 0.01);
        assert!(thumb_hit.rect.h > 0.0);
    }
}
