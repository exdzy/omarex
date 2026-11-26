//! Docked right-hand tabbed Viewer Pane UI (`Text`/`Code`/`Markdown` reader, interactive
//! zoom/pan `Image` viewer, and interactive `Video` player with transport controls), emitting
//! backend-agnostic [`UiDrawList`] primitives and exact [`ViewerImagePlacement`] GPU quads.

use std::ops::Range;

use omarchy_theme::{Rgba, Theme, UiTokens};

use crate::draw::{
    FontKind, FontWeight, HitTarget, TextAlign, UiDrawList, UiInteraction, cap_radius, z_layer,
};
use crate::icons::IconId;
use crate::layout::{RectF, VIEWER_DEFAULT_RATIO, ViewerSplitGeometry, clamp_viewer_ratio};

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
            Some(
                "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "go" | "c" | "h"
                | "cpp" | "cc" | "cxx" | "hpp" | "java" | "kt" | "kts" | "sh" | "bash" | "zsh"
                | "fish" | "toml" | "json" | "jsonc" | "yaml" | "yml" | "html" | "htm" | "css"
                | "scss" | "sql" | "wgsl" | "glsl" | "lua" | "nix" | "rb" | "php" | "zig" | "swift"
                | "dockerfile" | "xml" | "graphql" | "proto",
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
            Self::Binary => "BIN",
        }
    }

    /// Bundled vector icon associated with this viewer tab kind.
    #[must_use]
    pub const fn icon(self) -> IconId {
        match self {
            Self::Code => IconId::Code,
            Self::Text | Self::Markdown => IconId::Document,
            Self::Image => IconId::Star,
            Self::Video => IconId::ChevronRight,
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
            Self::Text => tokens.fg_secondary,
            Self::Binary => ansi[3].opaque(),
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
    /// Last measured viewport height (updated via `sync_render_metrics`).
    pub last_viewport_h: f32,
    /// Last measured total content height (updated via `sync_render_metrics`).
    pub last_content_h: f32,
    pub file_size_bytes: Option<u64>,
}

impl TextViewerState {
    #[must_use]
    pub fn new(text: impl Into<String>, language: Option<String>, is_markdown: bool) -> Self {
        let text = text.into();
        let lines = split_lines_owned(&text);
        let estimated_h = estimate_text_content_height(lines.len());
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
            last_viewport_h: 400.0,
            last_content_h: estimated_h,
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.file_size_bytes = Some(self.text.len() as u64);
        self.lines = split_lines_owned(&self.text);
        self.last_content_h = estimate_text_content_height(self.lines.len());
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll());
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
        let line_top = (target_line - 1) as f32 * TEXT_LINE_HEIGHT + 10.0;
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

/// Content state carried by a single [`ViewerTab`].
#[derive(Debug, Clone, PartialEq)]
pub enum ViewerContent {
    Loading { label: String },
    Error { message: String },
    Text(TextViewerState),
    Image(ImageViewerState),
    Video(VideoViewerState),
}

impl ViewerContent {
    #[must_use]
    fn inferred_kind(&self, path: &str) -> ViewerTabKind {
        match self {
            Self::Image(_) => ViewerTabKind::Image,
            Self::Video(_) => ViewerTabKind::Video,
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

    /// Zooms in the active Image or Video tab. Returns `true` if the active tab supports zoom.
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
            _ => false,
        }
    }

    /// Zooms out the active Image or Video tab.
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
            _ => false,
        }
    }

    /// Resets the active Image or Video tab to `100%` actual pixels centered.
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
            _ => false,
        }
    }

    /// Sets the active Image or Video tab to Fit-to-Viewport mode.
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
            _ => false,
        }
    }

    /// Pans the active Image or Video tab by `(dx, dy)` logical pixels, clamped cleanly.
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

    /// Feeds back measured viewport rects & scroll heights from the latest [`ViewerRenderMetrics`].
    pub fn sync_render_metrics(&mut self, metrics: &ViewerRenderMetrics) {
        let Some(tab_id) = metrics.active_tab_id else {
            return;
        };
        let Some(tab) = self.tab_by_id_mut(tab_id) else {
            return;
        };
        match &mut tab.content {
            ViewerContent::Text(txt) => {
                if metrics.text_viewport_h > 0.0 {
                    txt.last_viewport_h = metrics.text_viewport_h;
                }
                if metrics.text_content_h > 0.0 {
                    txt.last_content_h = metrics.text_content_h;
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
            ViewerContent::Loading { .. } | ViewerContent::Error { .. } => {}
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
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ViewerRenderMetrics {
    pub active_tab_id: Option<u64>,
    pub canvas_rect: Option<RectF>,
    pub seek_bar_rect: Option<RectF>,
    pub text_viewport_h: f32,
    pub text_content_h: f32,
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
                    let (vh, ch) =
                        render_text_content(draw_list, body, txt, interaction, theme, &tokens);
                    output.metrics.text_viewport_h = vh;
                    output.metrics.text_content_h = ch;
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
            },
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
) {
    if bar.is_empty() {
        return;
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
        ViewerContent::Image(_) | ViewerContent::Video(_) => {}
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
        draw_list.push_text(
            RectF::new(left_x, bar.y + 4.0, path_w, (bar.h - 8.0).max(14.0)),
            &tab.path,
            tokens.fg_muted,
            FontKind::Mono,
            11.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
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
        is_target: bool,
        in_code_block: bool,
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
    MarkdownParagraph {
        text: String,
        is_target: bool,
    },
}

fn render_text_content(
    draw_list: &mut UiDrawList,
    body: RectF,
    txt: &TextViewerState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    tokens: &UiTokens,
) -> (f32, f32) {
    let scrollbar_reserve = 12.0;
    let formatted_md = txt.is_markdown && !txt.raw_markdown;
    let gutter_w = if formatted_md {
        0.0
    } else {
        let digits = txt.lines.len().max(1).to_string().len();
        (digits as f32 * 8.0 + 22.0).clamp(40.0, 72.0)
    };
    let text_x = body.x + gutter_w + 10.0;
    let text_w = (body.w - gutter_w - scrollbar_reserve - 16.0).max(24.0);
    let max_chars = ((text_w / 7.8).floor() as usize).max(16);

    let rows = build_text_rows(txt, formatted_md, max_chars);
    let content_h = (rows.len() as f32 * TEXT_LINE_HEIGHT + 24.0).max(body.h);
    let max_scroll = (content_h - body.h).max(0.0);
    let scroll_y = txt.scroll_y.clamp(0.0, max_scroll);

    draw_list.push_clip(body);

    // Subtle gutter background & vertical separator for code/raw views
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

    let first_visible = ((scroll_y - 10.0).max(0.0) / TEXT_LINE_HEIGHT).floor() as usize;
    let visible_count = (body.h / TEXT_LINE_HEIGHT).ceil() as usize + 2;
    let last_visible = (first_visible + visible_count).min(rows.len());

    let ansi = &theme.palette.terminal.ansi;
    let color_keyword = ansi[5].opaque();
    let color_string = ansi[2].opaque();
    let color_attr = ansi[3].opaque();
    let color_heading = tokens.accent_primary;
    let color_inline_code = ansi[6].opaque();

    for (idx, row) in rows
        .iter()
        .enumerate()
        .take(last_visible)
        .skip(first_visible)
    {
        let y = body.y + 10.0 + idx as f32 * TEXT_LINE_HEIGHT - scroll_y;
        let row_rect = RectF::new(
            body.x,
            y,
            (body.w - scrollbar_reserve).max(0.0),
            TEXT_LINE_HEIGHT,
        );

        match row {
            TextRenderRow::CodeLine {
                line_no,
                text,
                is_target,
                in_code_block,
            } => {
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
                        RectF::new(body.x, y, 2.5, TEXT_LINE_HEIGHT),
                        tokens.accent_primary,
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                } else if *in_code_block {
                    draw_list.push_rect(
                        RectF::new(
                            body.x + 12.0,
                            y,
                            (body.w - scrollbar_reserve - 24.0).max(0.0),
                            TEXT_LINE_HEIGHT,
                        ),
                        tokens.surface_row_hover,
                        None,
                        0.0,
                        0.0,
                        None,
                    );
                }

                if let Some(n) = line_no
                    && !formatted_md
                {
                    draw_list.push_text(
                        RectF::new(
                            body.x + 4.0,
                            y + 2.0,
                            (gutter_w - 12.0).max(0.0),
                            TEXT_LINE_HEIGHT - 2.0,
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

                let (base_color, highlights, hl_color) =
                    classify_code_line(text, tokens, color_keyword, color_string, color_attr);
                let draw_x = if *in_code_block && formatted_md {
                    body.x + 22.0
                } else {
                    text_x
                };
                let draw_w = (body.right() - scrollbar_reserve - draw_x - 8.0).max(0.0);
                if highlights.is_empty() {
                    draw_list.push_text(
                        RectF::new(draw_x, y + 2.0, draw_w, TEXT_LINE_HEIGHT - 2.0),
                        text,
                        base_color,
                        FontKind::Mono,
                        12.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        !txt.word_wrap,
                    );
                } else {
                    draw_list.push_highlighted_text(
                        RectF::new(draw_x, y + 2.0, draw_w, TEXT_LINE_HEIGHT - 2.0),
                        text,
                        base_color,
                        FontKind::Mono,
                        12.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        !txt.word_wrap,
                        highlights,
                        hl_color,
                    );
                }
            }
            TextRenderRow::MarkdownHeading {
                level,
                text,
                is_target,
                ..
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
                        y + 1.0,
                        (body.w - scrollbar_reserve - 28.0).max(0.0),
                        TEXT_LINE_HEIGHT,
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
                            y + TEXT_LINE_HEIGHT - 1.0,
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
            TextRenderRow::MarkdownQuote {
                text, is_target, ..
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
                draw_list.push_rect(
                    RectF::new(body.x + 16.0, y + 2.0, 2.5, TEXT_LINE_HEIGHT - 4.0),
                    tokens.accent_primary.with_alpha_f32(0.65),
                    None,
                    0.0,
                    1.0,
                    None,
                );
                let (clean, spans) = extract_markdown_inline_spans(text);
                let qrect = RectF::new(
                    body.x + 26.0,
                    y + 2.0,
                    (body.w - scrollbar_reserve - 40.0).max(0.0),
                    TEXT_LINE_HEIGHT - 2.0,
                );
                if spans.is_empty() {
                    draw_list.push_text(
                        qrect,
                        clean,
                        tokens.fg_secondary,
                        FontKind::Sans,
                        12.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        !txt.word_wrap,
                    );
                } else {
                    draw_list.push_highlighted_text(
                        qrect,
                        clean,
                        tokens.fg_secondary,
                        FontKind::Sans,
                        12.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        !txt.word_wrap,
                        spans,
                        color_inline_code,
                    );
                }
            }
            TextRenderRow::MarkdownListItem {
                marker,
                text,
                indent,
                is_target,
                ..
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
                    RectF::new(mx, y + 2.0, 18.0, TEXT_LINE_HEIGHT - 2.0),
                    marker,
                    tokens.accent_primary,
                    FontKind::Sans,
                    12.5,
                    FontWeight::SemiBold,
                    TextAlign::Left,
                    false,
                );
                let (clean, spans) = extract_markdown_inline_spans(text);
                let tx = mx + 18.0;
                let tw = (body.right() - scrollbar_reserve - tx - 10.0).max(0.0);
                if spans.is_empty() {
                    draw_list.push_text(
                        RectF::new(tx, y + 2.0, tw, TEXT_LINE_HEIGHT - 2.0),
                        clean,
                        tokens.fg_primary,
                        FontKind::Sans,
                        12.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        !txt.word_wrap,
                    );
                } else {
                    draw_list.push_highlighted_text(
                        RectF::new(tx, y + 2.0, tw, TEXT_LINE_HEIGHT - 2.0),
                        clean,
                        tokens.fg_primary,
                        FontKind::Sans,
                        12.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        !txt.word_wrap,
                        spans,
                        color_inline_code,
                    );
                }
            }
            TextRenderRow::MarkdownRule => {
                draw_list.push_rect(
                    RectF::new(
                        body.x + 14.0,
                        y + TEXT_LINE_HEIGHT * 0.5,
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
            TextRenderRow::MarkdownParagraph {
                text, is_target, ..
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
                let (clean, spans) = extract_markdown_inline_spans(text);
                let tx = body.x + 14.0;
                let tw = (body.w - scrollbar_reserve - 28.0).max(0.0);
                if spans.is_empty() {
                    draw_list.push_text(
                        RectF::new(tx, y + 2.0, tw, TEXT_LINE_HEIGHT - 2.0),
                        clean,
                        tokens.fg_primary,
                        FontKind::Sans,
                        12.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        !txt.word_wrap,
                    );
                } else {
                    draw_list.push_highlighted_text(
                        RectF::new(tx, y + 2.0, tw, TEXT_LINE_HEIGHT - 2.0),
                        clean,
                        tokens.fg_primary,
                        FontKind::Sans,
                        12.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        !txt.word_wrap,
                        spans,
                        color_inline_code,
                    );
                }
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
        let thumb_h = ((body.h / content_h) * track.h).clamp(24.0, track.h);
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
    (body.h, content_h)
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

fn build_text_rows(
    txt: &TextViewerState,
    formatted_markdown: bool,
    max_chars: usize,
) -> Vec<TextRenderRow> {
    let mut rows = Vec::with_capacity(txt.lines.len());
    let mut in_fenced_code = false;

    for (idx, line) in txt.lines.iter().enumerate() {
        let line_no = idx + 1;
        let is_target = txt.highlight_line == Some(line_no);

        if formatted_markdown {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                in_fenced_code = !in_fenced_code;
                continue;
            }
            if in_fenced_code {
                push_wrapped_code_rows(
                    &mut rows,
                    line_no,
                    line,
                    is_target,
                    true,
                    txt.word_wrap,
                    max_chars,
                );
                continue;
            }
            if trimmed == "---" || trimmed == "***" || trimmed == "___" {
                rows.push(TextRenderRow::MarkdownRule);
                continue;
            }
            if let Some((level, heading)) = parse_markdown_heading(trimmed) {
                rows.push(TextRenderRow::MarkdownHeading {
                    level,
                    text: heading.to_owned(),
                    is_target,
                });
                continue;
            }
            if let Some(quoted) = trimmed.strip_prefix("> ") {
                for chunk in wrap_line_chunks(quoted, txt.word_wrap, max_chars) {
                    rows.push(TextRenderRow::MarkdownQuote {
                        text: chunk,
                        is_target,
                    });
                }
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
                continue;
            }
            for chunk in wrap_line_chunks(line, txt.word_wrap, max_chars) {
                rows.push(TextRenderRow::MarkdownParagraph {
                    text: chunk,
                    is_target,
                });
            }
        } else {
            push_wrapped_code_rows(
                &mut rows,
                line_no,
                line,
                is_target,
                false,
                txt.word_wrap,
                max_chars,
            );
        }
    }

    rows
}

fn push_wrapped_code_rows(
    rows: &mut Vec<TextRenderRow>,
    line_no: usize,
    line: &str,
    is_target: bool,
    in_code_block: bool,
    word_wrap: bool,
    max_chars: usize,
) {
    for (ci, chunk) in wrap_line_chunks(line, word_wrap, max_chars)
        .into_iter()
        .enumerate()
    {
        rows.push(TextRenderRow::CodeLine {
            line_no: (ci == 0).then_some(line_no),
            text: chunk,
            is_target,
            in_code_block,
        });
    }
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
            && let Some((label, next_i)) = parse_markdown_bracket_link(input, i, is_img)
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

/// Parses `[label](url)` or `![alt](src)` starting at byte `start`, returning `(label, end_byte)`.
fn parse_markdown_bracket_link(input: &str, start: usize, is_img: bool) -> Option<(&str, usize)> {
    let bytes = input.as_bytes();
    let label_start = start + if is_img { 2 } else { 1 };
    let mut depth = 1usize;
    let mut j = label_start;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' if j + 1 < bytes.len() => j += 2,
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
            _ => j += 1,
        }
    }
    if depth != 0 || bytes.get(j + 1) != Some(&b'(') {
        return None;
    }
    let label = &input[label_start..j];
    let mut paren_depth = 1usize;
    let mut k = j + 2;
    while k < bytes.len() {
        match bytes[k] {
            b'\\' if k + 1 < bytes.len() => k += 2,
            b'(' => {
                paren_depth += 1;
                k += 1;
            }
            b')' => {
                paren_depth -= 1;
                if paren_depth == 0 {
                    return Some((label, k + 1));
                }
                k += 1;
            }
            _ => k += 1,
        }
    }
    None
}

/// Lightweight syntax coloring for code lines using Omarchy ANSI/accent colors.
fn classify_code_line(
    line: &str,
    tokens: &UiTokens,
    color_keyword: Rgba,
    color_string: Rgba,
    color_attr: Rgba,
) -> (Rgba, Vec<Range<usize>>, Rgba) {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with("# ") || trimmed.starts_with("--") {
        return (tokens.fg_muted, Vec::new(), tokens.fg_muted);
    }
    if trimmed.starts_with("#[") || trimmed.starts_with('@') {
        return (color_attr, Vec::new(), color_attr);
    }

    // Highlight quoted string spans first if present
    let string_ranges = find_quoted_ranges(line);
    if !string_ranges.is_empty() {
        return (tokens.fg_primary, string_ranges, color_string);
    }

    // Otherwise highlight common language keywords
    let kw_ranges = find_keyword_ranges(line);
    (tokens.fg_primary, kw_ranges, color_keyword)
}

fn find_quoted_ranges(line: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            let mut escaped = false;
            while i < bytes.len() {
                if escaped {
                    escaped = false;
                } else if bytes[i] == b'\\' {
                    escaped = true;
                } else if bytes[i] == b'"' {
                    i += 1;
                    ranges.push(start..i);
                    break;
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    ranges
}

fn find_keyword_ranges(line: &str) -> Vec<Range<usize>> {
    const KEYWORDS: &[&str] = &[
        "fn",
        "pub",
        "let",
        "mut",
        "struct",
        "enum",
        "impl",
        "use",
        "mod",
        "match",
        "if",
        "else",
        "for",
        "while",
        "loop",
        "return",
        "async",
        "await",
        "const",
        "static",
        "type",
        "trait",
        "where",
        "true",
        "false",
        "Some",
        "None",
        "Ok",
        "Err",
        "def",
        "class",
        "import",
        "from",
        "function",
        "export",
        "interface",
        "var",
        "try",
        "catch",
    ];
    let mut ranges = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if KEYWORDS.contains(&word) {
                ranges.push(start..i);
            }
        } else {
            i += 1;
        }
    }
    ranges
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
        let quote_prim = out.draw_list.primitives.iter().find_map(|p| match p {
            crate::draw::UiPrimitive::Text {
                text,
                highlight_ranges,
                ..
            } if text.contains("Experimental Community Project") => Some((text, highlight_ranges)),
            _ => None,
        });
        let (qtext, qspans) = quote_prim.expect("formatted quote primitive");
        assert_eq!(qtext, "Experimental Community Project for omarex via Rex");
        assert_eq!(qspans.len(), 3);
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
}
