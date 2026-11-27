//! Shared GPU-ready POD instance types and high-level `DrawList` / `SceneFrame` builder
//! consumed by `ui-chrome`, `terminal_pass`, and `omarex`.

use std::collections::BTreeMap;

use bytemuck::{Pod, Zeroable};

/// Standard semantic Z-layers for deterministic ordering in a unified render pass.
pub mod layers {
    /// Window frame, sidebar background, workspace canvas.
    pub const CANVAS: i32 = 0;
    /// Kitty Graphics Protocol `BELOW_BACKGROUND` layer.
    pub const KITTY_BELOW_BG: i32 = 10;
    /// Terminal cell backgrounds, selections, and pane card fills.
    pub const TERMINAL_BG: i32 = 20;
    /// Kitty Graphics Protocol `BELOW_TEXT` layer.
    pub const KITTY_BELOW_TEXT: i32 = 30;
    /// Terminal text glyphs, decorations (underlines/strikethrough), and cursor.
    pub const TERMINAL_FG: i32 = 40;
    /// Kitty Graphics Protocol `ABOVE_TEXT` layer.
    pub const KITTY_ABOVE_TEXT: i32 = 50;
    /// Workspace UI chrome (sidebar rows, tab pills, pane headers, scrollbars, borders).
    pub const CHROME: i32 = 60;
    /// Floating terminal panes, `/pr` drawer, minimized floating capsules.
    pub const FLOATING: i32 = 70;
    /// Command palette, `rex.ui.pick` / `rex.ui.input` modals, context menus.
    pub const MODAL: i32 = 80;
    /// Hover previews, tooltips, key sequence hints, toast notifications.
    pub const TOAST: i32 = 90;
}

/// sRGB color in normalized `0.0..=1.0` floating-point representation.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Pod, Zeroable)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Self = Self::rgba(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Self = Self::rgb(0.0, 0.0, 0.0);
    pub const WHITE: Self = Self::rgb(1.0, 1.0, 1.0);

    #[inline]
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    #[inline]
    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    #[inline]
    pub fn from_rgb_u8(r: u8, g: u8, b: u8) -> Self {
        Self::from_rgba_u8(r, g, b, 255)
    }

    #[inline]
    pub fn from_rgba_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: f32::from(a) / 255.0,
        }
    }

    /// Constructs an opaque color from a `0xRRGGBB` hex integer.
    #[inline]
    pub fn from_hex(hex: u32) -> Self {
        let r = ((hex >> 16) & 0xff) as u8;
        let g = ((hex >> 8) & 0xff) as u8;
        let b = (hex & 0xff) as u8;
        Self::from_rgb_u8(r, g, b)
    }

    /// Constructs a color from a `0xRRGGBB` hex integer with explicit alpha `0.0..=1.0`.
    #[inline]
    pub fn from_hex_alpha(hex: u32, a: f32) -> Self {
        Self {
            a: a.clamp(0.0, 1.0),
            ..Self::from_hex(hex)
        }
    }

    #[inline]
    pub fn with_alpha(self, a: f32) -> Self {
        Self {
            a: a.clamp(0.0, 1.0),
            ..self
        }
    }

    #[inline]
    pub fn mul_alpha(self, factor: f32) -> Self {
        Self {
            a: (self.a * factor).clamp(0.0, 1.0),
            ..self
        }
    }

    #[inline]
    pub const fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// Converts an sRGB channel into linear light per IEC 61966-2-1.
    #[inline]
    pub fn srgb_channel_to_linear(c: f32) -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// Converts this sRGB color into a `wgpu::Color` in linear space (for sRGB render targets)
    /// or sRGB space (for non-sRGB targets).
    #[inline]
    pub fn to_wgpu_clear(self, target_is_srgb: bool) -> wgpu::Color {
        if target_is_srgb {
            wgpu::Color {
                r: f64::from(Self::srgb_channel_to_linear(self.r)),
                g: f64::from(Self::srgb_channel_to_linear(self.g)),
                b: f64::from(Self::srgb_channel_to_linear(self.b)),
                a: f64::from(self.a),
            }
        } else {
            wgpu::Color {
                r: f64::from(self.r),
                g: f64::from(self.g),
                b: f64::from(self.b),
                a: f64::from(self.a),
            }
        }
    }
}

impl From<omarchy_theme::Rgb> for Color {
    #[inline]
    fn from(c: omarchy_theme::Rgb) -> Self {
        Self::from_rgb_u8(c.r, c.g, c.b)
    }
}

impl From<omarchy_theme::Rgba> for Color {
    #[inline]
    fn from(c: omarchy_theme::Rgba) -> Self {
        Self::from_rgba_u8(c.r, c.g, c.b, c.a)
    }
}

impl From<vt_engine::Rgb> for Color {
    #[inline]
    fn from(c: vt_engine::Rgb) -> Self {
        Self::from_rgb_u8(c.r, c.g, c.b)
    }
}

/// Axis-aligned rectangle in logical or physical pixels.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Default, Pod, Zeroable)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    #[inline]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[inline]
    pub fn right(self) -> f32 {
        self.x + self.width
    }

    #[inline]
    pub fn bottom(self) -> f32 {
        self.y + self.height
    }

    #[inline]
    pub fn scale(self, factor: f32) -> Self {
        Self {
            x: self.x * factor,
            y: self.y * factor,
            width: self.width * factor,
            height: self.height * factor,
        }
    }

    #[inline]
    pub fn to_array(self) -> [f32; 4] {
        [self.x, self.y, self.width, self.height]
    }

    #[inline]
    pub fn to_clip_rect(self) -> ClipRect {
        ClipRect {
            min_x: self.x,
            min_y: self.y,
            max_x: self.x + self.width.max(0.0),
            max_y: self.y + self.height.max(0.0),
        }
    }
}

/// Scissor/clip rectangle `[min_x, min_y, max_x, max_y]`.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Pod, Zeroable)]
pub struct ClipRect {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

impl ClipRect {
    pub const UNBOUNDED: Self = Self {
        min_x: -1.0e7,
        min_y: -1.0e7,
        max_x: 1.0e7,
        max_y: 1.0e7,
    };

    #[inline]
    pub const fn new(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    #[inline]
    pub fn from_rect(rect: Rect) -> Self {
        rect.to_clip_rect()
    }

    #[inline]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.max(other.min_x),
            min_y: self.min_y.max(other.min_y),
            max_x: self.max_x.min(other.max_x),
            max_y: self.max_y.min(other.max_y),
        }
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.max_x <= self.min_x || self.max_y <= self.min_y
    }

    #[inline]
    pub fn scale(self, factor: f32) -> Self {
        Self {
            min_x: self.min_x * factor,
            min_y: self.min_y * factor,
            max_x: self.max_x * factor,
            max_y: self.max_y * factor,
        }
    }

    #[inline]
    pub const fn to_array(self) -> [f32; 4] {
        [self.min_x, self.min_y, self.max_x, self.max_y]
    }
}

/// Per-corner border radii (`[top_left, top_right, bottom_right, bottom_left]`).
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Default, Pod, Zeroable)]
pub struct CornerRadii {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl CornerRadii {
    pub const ZERO: Self = Self::uniform(0.0);

    #[inline]
    pub const fn uniform(r: f32) -> Self {
        Self {
            top_left: r,
            top_right: r,
            bottom_right: r,
            bottom_left: r,
        }
    }

    #[inline]
    pub const fn new(top_left: f32, top_right: f32, bottom_right: f32, bottom_left: f32) -> Self {
        Self {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        }
    }

    #[inline]
    pub fn scale(self, factor: f32) -> Self {
        Self {
            top_left: self.top_left * factor,
            top_right: self.top_right * factor,
            bottom_right: self.bottom_right * factor,
            bottom_left: self.bottom_left * factor,
        }
    }

    #[inline]
    pub const fn to_array(self) -> [f32; 4] {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
    }
}

/// GPU instance data for `shaders/quad.wgsl`.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Pod, Zeroable)]
pub struct QuadInstance {
    /// `[x, y, width, height]` in physical pixels.
    pub bounds: [f32; 4],
    /// `[min_x, min_y, max_x, max_y]` in physical pixels.
    pub clip_rect: [f32; 4],
    /// Fill or shadow/decoration color `[r, g, b, a]` in sRGB `0.0..=1.0`.
    pub fill_color: [f32; 4],
    /// Border color `[r, g, b, a]` in sRGB `0.0..=1.0`.
    pub border_color: [f32; 4],
    /// Per-corner radii `[tl, tr, br, bl]` in physical pixels.
    pub corner_radii: [f32; 4],
    /// `[border_width, shadow_blur_or_mode, offset_x_or_period, offset_y_or_amp]`.
    pub params: [f32; 4],
}

impl QuadInstance {
    pub const MODE_SOLID: f32 = 0.0;
    pub const MODE_CURLY_UNDERLINE: f32 = -1.0;
    pub const MODE_DOTTED_UNDERLINE: f32 = -2.0;
    pub const MODE_DASHED_UNDERLINE: f32 = -3.0;
    pub const MODE_SHINE: f32 = -4.0;

    /// Constructs a solid or bordered rounded rectangle in physical pixel space.
    #[inline]
    pub fn rounded_rect(
        bounds: [f32; 4],
        clip_rect: [f32; 4],
        fill: Color,
        border_color: Color,
        corner_radii: [f32; 4],
        border_width: f32,
    ) -> Self {
        Self {
            bounds,
            clip_rect,
            fill_color: fill.to_array(),
            border_color: border_color.to_array(),
            corner_radii,
            params: [border_width.max(0.0), Self::MODE_SOLID, 0.0, 0.0],
        }
    }

    /// Constructs an analytical Gaussian SDF drop shadow in physical pixel space.
    #[inline]
    pub fn shadow(
        bounds: [f32; 4],
        clip_rect: [f32; 4],
        color: Color,
        corner_radii: [f32; 4],
        blur_radius: f32,
        offset_x: f32,
        offset_y: f32,
    ) -> Self {
        Self {
            bounds,
            clip_rect,
            fill_color: color.to_array(),
            border_color: [0.0; 4],
            corner_radii,
            params: [0.0, blur_radius.max(0.5), offset_x, offset_y],
        }
    }

    /// Constructs a curly sine-wave underline in physical pixel space.
    #[inline]
    pub fn curly_underline(
        bounds: [f32; 4],
        clip_rect: [f32; 4],
        color: Color,
        thickness: f32,
        wavelength: f32,
        amplitude: f32,
    ) -> Self {
        Self {
            bounds,
            clip_rect,
            fill_color: color.to_array(),
            border_color: [0.0; 4],
            corner_radii: [0.0; 4],
            params: [
                thickness.max(1.0),
                Self::MODE_CURLY_UNDERLINE,
                wavelength.max(4.0),
                amplitude.max(1.0),
            ],
        }
    }

    /// Constructs a dotted underline in physical pixel space.
    #[inline]
    pub fn dotted_underline(
        bounds: [f32; 4],
        clip_rect: [f32; 4],
        color: Color,
        thickness: f32,
        period: f32,
    ) -> Self {
        Self {
            bounds,
            clip_rect,
            fill_color: color.to_array(),
            border_color: [0.0; 4],
            corner_radii: [0.0; 4],
            params: [
                thickness.max(1.0),
                Self::MODE_DOTTED_UNDERLINE,
                period.max(3.0),
                0.0,
            ],
        }
    }

    /// Constructs a dashed underline in physical pixel space.
    #[inline]
    pub fn dashed_underline(
        bounds: [f32; 4],
        clip_rect: [f32; 4],
        color: Color,
        thickness: f32,
        period: f32,
    ) -> Self {
        Self {
            bounds,
            clip_rect,
            fill_color: color.to_array(),
            border_color: [0.0; 4],
            corner_radii: [0.0; 4],
            params: [
                thickness.max(1.0),
                Self::MODE_DASHED_UNDERLINE,
                period.max(6.0),
                0.0,
            ],
        }
    }

    /// Constructs a diagonal GPU copy-shine sweep in physical pixel space.
    #[inline]
    pub fn copy_shine(
        bounds: [f32; 4],
        clip_rect: [f32; 4],
        color: Color,
        corner_radii: [f32; 4],
        progress: f32,
        band_width: f32,
        tilt: f32,
    ) -> Self {
        Self {
            bounds,
            clip_rect,
            fill_color: color.to_array(),
            border_color: [0.0; 4],
            corner_radii,
            params: [
                band_width.clamp(0.08, 1.0),
                Self::MODE_SHINE,
                progress.clamp(0.0, 1.0),
                tilt.max(0.0),
            ],
        }
    }
}

/// Atlas sampling mode for [`GlyphInstance`].
pub const GLYPH_MODE_MONO: u32 = 0;
pub const GLYPH_MODE_COLOR: u32 = 1;
pub const GLYPH_MODE_TINTED_COLOR: u32 = 2;

/// GPU instance data for `shaders/glyph.wgsl`.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Pod, Zeroable)]
pub struct GlyphInstance {
    /// Destination quad `[x, y, width, height]` in physical pixels.
    pub bounds: [f32; 4],
    /// Normalized atlas UVs `[u_min, v_min, u_max, v_max]`.
    pub uv_bounds: [f32; 4],
    /// Clip rect `[min_x, min_y, max_x, max_y]` in physical pixels.
    pub clip_rect: [f32; 4],
    /// Foreground/tint color `[r, g, b, a]` in sRGB `0.0..=1.0`.
    pub color: [f32; 4],
    /// `0` = monochrome R8Unorm tinted, `1` = color Rgba8UnormSrgb, `2` = tinted RGBA8.
    pub mode: u32,
    pub _pad: [u32; 3],
}

/// GPU instance data for `shaders/image.wgsl`.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Pod, Zeroable)]
pub struct ImageInstance {
    /// Destination quad `[x, y, width, height]` in physical pixels.
    pub bounds: [f32; 4],
    /// Normalized UV rect `[u_min, v_min, u_max, v_max]`.
    pub uv_bounds: [f32; 4],
    /// Clip rect `[min_x, min_y, max_x, max_y]` in physical pixels.
    pub clip_rect: [f32; 4],
    /// `[opacity, corner_radius_px, 0.0, 0.0]`.
    pub params: [f32; 4],
}

/// Logical image placement queued in a [`DrawList`].
#[derive(Debug, Clone)]
pub struct ImageQuad {
    pub image_id: u64,
    pub bounds: Rect,
    pub uv_bounds: [f32; 4],
    pub opacity: f32,
    pub corner_radius: f32,
    pub clip_rect: ClipRect,
}

impl ImageQuad {
    pub fn new(image_id: u64, bounds: Rect) -> Self {
        Self {
            image_id,
            bounds,
            uv_bounds: [0.0, 0.0, 1.0, 1.0],
            opacity: 1.0,
            corner_radius: 0.0,
            clip_rect: ClipRect::UNBOUNDED,
        }
    }
}

/// High-level font family selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum FontFamily {
    /// Primary terminal monospace (`JetBrainsMono Nerd Font` with system monospace fallback).
    #[default]
    Monospace,
    /// Primary UI chrome sans-serif (`Liberation Sans` / system sans-serif fallback).
    SansSerif,
    /// Explicit named font family discovered via fontconfig.
    Named(String),
}

/// Font weight for UI chrome & terminal text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FontWeight {
    #[default]
    Regular,
    Medium,
    SemiBold,
    Bold,
}

impl FontWeight {
    pub fn to_cosmic(self) -> cosmic_text::Weight {
        match self {
            Self::Regular => cosmic_text::Weight::NORMAL,
            Self::Medium => cosmic_text::Weight::MEDIUM,
            Self::SemiBold => cosmic_text::Weight::SEMIBOLD,
            Self::Bold => cosmic_text::Weight::BOLD,
        }
    }
}

/// Font style for UI chrome & terminal text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
}

impl FontStyle {
    pub fn to_cosmic(self) -> cosmic_text::Style {
        match self {
            Self::Normal => cosmic_text::Style::Normal,
            Self::Italic => cosmic_text::Style::Italic,
        }
    }
}

/// Horizontal text alignment inside `max_width`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// High-level shaped text run queued into a [`DrawList`].
#[derive(Debug, Clone)]
pub struct TextRun {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub family: FontFamily,
    pub weight: FontWeight,
    pub style: FontStyle,
    pub color: Color,
    pub max_width: Option<f32>,
    pub align: TextAlign,
    pub clip_rect: ClipRect,
}

impl TextRun {
    pub fn new(text: impl Into<String>, x: f32, y: f32, font_size: f32, color: Color) -> Self {
        Self {
            text: text.into(),
            x,
            y,
            font_size,
            line_height: (font_size * 1.35).ceil(),
            family: FontFamily::SansSerif,
            weight: FontWeight::Regular,
            style: FontStyle::Normal,
            color,
            max_width: None,
            align: TextAlign::Left,
            clip_rect: ClipRect::UNBOUNDED,
        }
    }

    pub fn monospace(mut self) -> Self {
        self.family = FontFamily::Monospace;
        self
    }

    pub fn weight(mut self, weight: FontWeight) -> Self {
        self.weight = weight;
        self
    }

    pub fn style(mut self, style: FontStyle) -> Self {
        self.style = style;
        self
    }

    pub fn max_width(mut self, max_width: f32) -> Self {
        self.max_width = Some(max_width);
        self
    }

    pub fn align(mut self, align: TextAlign) -> Self {
        self.align = align;
        self
    }

    pub fn line_height(mut self, line_height: f32) -> Self {
        self.line_height = line_height;
        self
    }
}

/// High-level cached SVG icon draw request queued into a [`DrawList`].
#[derive(Debug, Clone)]
pub struct IconDraw {
    pub name: String,
    pub bounds: Rect,
    /// `Some(color)` tints a monochrome SVG icon; `None` renders full-color brand SVG.
    pub tint: Option<Color>,
    /// Opacity multiplier `0.0..=1.0` applied to full-color icons (monochrome uses `tint.a`).
    pub opacity: f32,
    pub clip_rect: ClipRect,
}

/// Primitives collected for a single Z-layer in a [`SceneFrame`].
#[derive(Debug, Clone, Default)]
pub struct LayerContent {
    pub quads: Vec<QuadInstance>,
    pub raw_glyphs: Vec<GlyphInstance>,
    pub text_runs: Vec<TextRun>,
    pub icons: Vec<IconDraw>,
    pub images: Vec<ImageQuad>,
}

/// Immutable, self-contained frame description ready to render on GPU.
#[derive(Debug, Clone)]
pub struct SceneFrame {
    pub physical_width: u32,
    pub physical_height: u32,
    pub scale_factor: f32,
    pub clear_color: Color,
    pub is_light_mode: bool,
    pub layers: BTreeMap<i32, LayerContent>,
}

/// Ergonomic builder for constructing a layered [`SceneFrame`].
///
/// Logical coordinates passed to builder methods are automatically converted to physical pixels
/// using `scale_factor`. Raw GPU instances (e.g. from `TerminalPaneCache`) can also be pushed
/// directly with zero overhead.
#[derive(Debug, Clone)]
pub struct DrawList {
    physical_width: u32,
    physical_height: u32,
    scale_factor: f32,
    clear_color: Color,
    is_light_mode: bool,
    active_layer: i32,
    clip_stack: Vec<ClipRect>,
    layers: BTreeMap<i32, LayerContent>,
}

impl DrawList {
    /// Creates a new draw list for the given physical viewport dimensions and DPI scale factor.
    pub fn new(physical_width: u32, physical_height: u32, scale_factor: f32) -> Self {
        let clamped_scale = scale_factor.max(0.5);
        let root_clip = ClipRect::new(
            0.0,
            0.0,
            physical_width as f32 / clamped_scale,
            physical_height as f32 / clamped_scale,
        );
        Self {
            physical_width,
            physical_height,
            scale_factor: clamped_scale,
            clear_color: Color::from_hex(0x0e0e0e),
            is_light_mode: false,
            active_layer: layers::CHROME,
            clip_stack: vec![root_clip],
            layers: BTreeMap::new(),
        }
    }

    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    pub fn physical_size(&self) -> (u32, u32) {
        (self.physical_width, self.physical_height)
    }

    pub fn logical_size(&self) -> (f32, f32) {
        (
            self.physical_width as f32 / self.scale_factor,
            self.physical_height as f32 / self.scale_factor,
        )
    }

    pub fn set_clear_color(&mut self, color: Color) {
        self.clear_color = color;
    }

    pub fn set_light_mode(&mut self, is_light: bool) {
        self.is_light_mode = is_light;
    }

    pub fn set_layer(&mut self, layer: i32) {
        self.active_layer = layer;
    }

    pub fn active_layer(&self) -> i32 {
        self.active_layer
    }

    /// Executes `f` with `active_layer` temporarily set to `layer`, restoring the previous layer.
    pub fn with_layer(&mut self, layer: i32, f: impl FnOnce(&mut Self)) {
        let prev = self.active_layer;
        self.active_layer = layer;
        f(self);
        self.active_layer = prev;
    }

    /// Pushes a logical-pixel clip rect intersected with the current clip rect.
    pub fn push_clip(&mut self, rect: Rect) {
        let next = self.current_clip().intersect(rect.to_clip_rect());
        self.clip_stack.push(next);
    }

    /// Pops the top clip rect (preserving the viewport root clip).
    pub fn pop_clip(&mut self) {
        if self.clip_stack.len() > 1 {
            self.clip_stack.pop();
        }
    }

    /// Executes `f` with a scoped clip rect.
    pub fn with_clip(&mut self, rect: Rect, f: impl FnOnce(&mut Self)) {
        self.push_clip(rect);
        f(self);
        self.pop_clip();
    }

    /// Returns the current active clip rect in logical pixels.
    pub fn current_clip(&self) -> ClipRect {
        self.clip_stack
            .last()
            .copied()
            .unwrap_or(ClipRect::UNBOUNDED)
    }

    #[inline]
    fn layer_mut(&mut self) -> &mut LayerContent {
        self.layers.entry(self.active_layer).or_default()
    }

    #[inline]
    fn current_clip_physical(&self) -> [f32; 4] {
        self.current_clip().scale(self.scale_factor).to_array()
    }

    /// Pushes an axis-aligned filled rectangle (logical pixels).
    pub fn push_rect(&mut self, rect: Rect, fill: Color) {
        self.push_rounded_rect(rect, CornerRadii::ZERO, fill, Color::TRANSPARENT, 0.0);
    }

    /// Pushes a rounded rectangle with optional border (logical pixels).
    pub fn push_rounded_rect(
        &mut self,
        rect: Rect,
        radii: CornerRadii,
        fill: Color,
        border_color: Color,
        border_width: f32,
    ) {
        let clip = self.current_clip();
        if clip.is_empty() || rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let s = self.scale_factor;
        let quad = QuadInstance::rounded_rect(
            rect.scale(s).to_array(),
            clip.scale(s).to_array(),
            fill,
            border_color,
            radii.scale(s).to_array(),
            (border_width * s).max(0.0),
        );
        self.layer_mut().quads.push(quad);
    }

    /// Pushes a soft analytical SDF drop shadow around `rect` (logical pixels).
    pub fn push_shadow(
        &mut self,
        rect: Rect,
        radii: CornerRadii,
        color: Color,
        blur_radius: f32,
        offset_x: f32,
        offset_y: f32,
    ) {
        let clip = self.current_clip();
        if clip.is_empty() || rect.width <= 0.0 || rect.height <= 0.0 || color.a <= 0.001 {
            return;
        }
        let s = self.scale_factor;
        let quad = QuadInstance::shadow(
            rect.scale(s).to_array(),
            clip.scale(s).to_array(),
            color,
            radii.scale(s).to_array(),
            blur_radius * s,
            offset_x * s,
            offset_y * s,
        );
        self.layer_mut().quads.push(quad);
    }

    /// Pushes a curly sine-wave underline in logical pixels.
    pub fn push_curly_underline(
        &mut self,
        rect: Rect,
        color: Color,
        thickness: f32,
        wavelength: f32,
        amplitude: f32,
    ) {
        let clip = self.current_clip();
        if clip.is_empty() || rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let s = self.scale_factor;
        let quad = QuadInstance::curly_underline(
            rect.scale(s).to_array(),
            clip.scale(s).to_array(),
            color,
            thickness * s,
            wavelength * s,
            amplitude * s,
        );
        self.layer_mut().quads.push(quad);
    }

    /// Pushes a 220 ms diagonal GPU shine sweep quad over copied text (logical pixels).
    pub fn push_copy_shine(&mut self, rect: Rect, radii: CornerRadii, color: Color, progress: f32) {
        let clip = self.current_clip();
        if clip.is_empty() || rect.width <= 0.0 || rect.height <= 0.0 || color.a <= 0.001 {
            return;
        }
        let s = self.scale_factor;
        let quad = QuadInstance::copy_shine(
            rect.scale(s).to_array(),
            clip.scale(s).to_array(),
            color,
            radii.scale(s).to_array(),
            progress,
            0.32,
            0.45,
        );
        self.layer_mut().quads.push(quad);
    }

    /// Pushes a shaped text run (intersected with the active clip stack).
    pub fn push_text(&mut self, mut run: TextRun) {
        let clip = self.current_clip().intersect(run.clip_rect);
        if clip.is_empty() || run.text.is_empty() {
            return;
        }
        run.clip_rect = clip;
        self.layer_mut().text_runs.push(run);
    }

    /// Pushes a cached SVG icon by registered name.
    ///
    /// Pass `tint = Some(color)` for monochrome theme-tinted UI icons, or `tint = None` for
    /// full-color brand icons (e.g. Claude, Gemini, OpenAI).
    pub fn push_icon(&mut self, name: impl Into<String>, bounds: Rect, tint: Option<Color>) {
        self.push_icon_with_opacity(name, bounds, tint, 1.0);
    }

    /// Pushes a cached SVG icon with explicit opacity.
    pub fn push_icon_with_opacity(
        &mut self,
        name: impl Into<String>,
        bounds: Rect,
        tint: Option<Color>,
        opacity: f32,
    ) {
        let clip = self.current_clip();
        if clip.is_empty() || bounds.width <= 0.0 || bounds.height <= 0.0 {
            return;
        }
        self.layer_mut().icons.push(IconDraw {
            name: name.into(),
            bounds,
            tint,
            opacity: opacity.clamp(0.0, 1.0),
            clip_rect: clip,
        });
    }

    /// Pushes a textured image quad (Kitty graphics or hover image preview).
    pub fn push_image(&mut self, mut quad: ImageQuad) {
        let clip = self.current_clip().intersect(quad.clip_rect);
        if clip.is_empty() || quad.bounds.width <= 0.0 || quad.bounds.height <= 0.0 {
            return;
        }
        quad.clip_rect = clip;
        self.layer_mut().images.push(quad);
    }

    /// Pushes a pre-built physical-pixel [`QuadInstance`] directly into `layer`.
    pub fn push_raw_quad(&mut self, layer: i32, quad: QuadInstance) {
        self.layers.entry(layer).or_default().quads.push(quad);
    }

    /// Extends `layer` with pre-built physical-pixel [`QuadInstance`]s.
    pub fn extend_raw_quads(&mut self, layer: i32, quads: impl IntoIterator<Item = QuadInstance>) {
        self.layers.entry(layer).or_default().quads.extend(quads);
    }

    /// Pushes a pre-built physical-pixel [`GlyphInstance`] directly into `layer`.
    pub fn push_raw_glyph(&mut self, layer: i32, glyph: GlyphInstance) {
        self.layers.entry(layer).or_default().raw_glyphs.push(glyph);
    }

    /// Extends `layer` with pre-built physical-pixel [`GlyphInstance`]s.
    pub fn extend_raw_glyphs(
        &mut self,
        layer: i32,
        glyphs: impl IntoIterator<Item = GlyphInstance>,
    ) {
        self.layers
            .entry(layer)
            .or_default()
            .raw_glyphs
            .extend(glyphs);
    }

    /// Returns the active physical-pixel clip rectangle.
    pub fn active_clip_physical(&self) -> [f32; 4] {
        self.current_clip_physical()
    }

    /// Consumes the builder and returns an immutable [`SceneFrame`].
    pub fn finish(self) -> SceneFrame {
        SceneFrame {
            physical_width: self.physical_width,
            physical_height: self.physical_height,
            scale_factor: self.scale_factor,
            clear_color: self.clear_color,
            is_light_mode: self.is_light_mode,
            layers: self.layers,
        }
    }
}
