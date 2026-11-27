//! Dual `etagere` GPU texture atlas (`R8Unorm` 2048x2048 monochrome glyphs +
//! `Rgba8UnormSrgb` 1024x1024 color emoji & SVG icons) with `cosmic-text` + `swash` shaping,
//! procedural pixel-exact box-drawing/block-element rasterization, `resvg` + `tiny-skia`
//! HiDPI SVG icon caching, and incremental `queue.write_texture` sub-rect uploads.

use std::collections::HashMap;
use std::path::Path;

use cosmic_text::{
    Align, Attrs, Buffer, CacheKey, CacheKeyFlags, Family, FontSystem, Metrics, Shaping,
    SwashCache, SwashContent, Wrap,
};
use etagere::{BucketedAtlasAllocator, size2};
use tracing::{debug, info, warn};

use crate::context::RenderError;
use crate::draw_list::{
    ClipRect, Color, FontFamily, FontStyle, FontWeight, GLYPH_MODE_COLOR, GLYPH_MODE_MONO,
    GlyphInstance, IconDraw, TextAlign, TextRun,
};

pub const MONO_ATLAS_SIZE: u32 = 2048;
pub const COLOR_ATLAS_SIZE: u32 = 1024;
const TILE_PADDING: u32 = 1;

/// Physical- and logical-space metrics for a monospace terminal cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub font_size: f32,
    pub scale_factor: f32,
    /// Logical cell width (snapped so `cell_width * scale_factor` is integer physical pixels).
    pub cell_width: f32,
    /// Logical cell height (snapped so `cell_height * scale_factor` is integer physical pixels).
    pub cell_height: f32,
    /// Logical baseline distance from cell top.
    pub baseline_y: f32,
    /// Logical underline center distance from cell top.
    pub underline_y: f32,
    /// Logical underline stroke thickness.
    pub underline_thickness: f32,
    /// Logical strikethrough center distance from cell top.
    pub strikethrough_y: f32,
}

/// Cached location of a rasterized glyph or icon inside one of the two GPU atlases.
#[derive(Debug, Clone, Copy)]
pub struct AtlasEntry {
    pub uv_bounds: [f32; 4],
    pub width_px: u32,
    pub height_px: u32,
    pub left_px: i32,
    pub top_px: i32,
    pub is_color: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BoxDrawingKey {
    ch: char,
    cell_w_px: u32,
    cell_h_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AsciiCellKey {
    ch: char,
    bold: bool,
    italic: bool,
    font_size_bits: u32,
}

#[derive(Debug, Clone, Copy)]
struct CachedAsciiGlyph {
    entry: Option<AtlasEntry>,
    /// Offset from cell baseline in physical pixels (`top` already stored in `entry`).
    x_offset_px: f32,
}

/// Mapping from a UTF-8 byte offset inside a shaped terminal run back to its grid cell column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapedRunCell {
    pub byte_start: u32,
    pub col: u16,
    pub span_cols: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TerminalRunStyleKey {
    bold: bool,
    italic: bool,
    font_size_bits: u32,
    cell_w_bits: u32,
    cell_h_bits: u32,
}

#[derive(Debug, Clone, Copy)]
struct CachedTerminalRunGlyph {
    cluster_byte: u32,
    px_dx: i32,
    py_dy: i32,
    is_rtl: bool,
    rtl_x_px: f32,
    entry: AtlasEntry,
}

const MAX_TERMINAL_RUN_CACHE_ENTRIES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IconCacheKey {
    name: String,
    px_w: u32,
    px_h: u32,
    mono_mask: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShapedRunStyleKey {
    font_size_bits: u32,
    line_height_bits: u32,
    family: FontFamily,
    weight: FontWeight,
    style: FontStyle,
    max_width_bits: Option<u32>,
    align: TextAlign,
    scale_bits: u32,
}

#[derive(Debug, Clone, Copy)]
struct RelativeGlyph {
    offset_x: f32,
    offset_y: f32,
    width: f32,
    height: f32,
    uv_bounds: [f32; 4],
    is_color: bool,
}

const MAX_SHAPED_RUN_CACHE_ENTRIES: usize = 2048;

struct AtlasPlane {
    size: u32,
    bytes_per_pixel: u32,
    allocator: BucketedAtlasAllocator,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    upload_count: u64,
    reset_count: u64,
    /// Reusable padded upload scratch buffer (avoids per-glyph heap allocs).
    scratch: Vec<u8>,
}

impl AtlasPlane {
    fn new(
        device: &wgpu::Device,
        label: &str,
        size: u32,
        format: wgpu::TextureFormat,
        bytes_per_pixel: u32,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let allocator = BucketedAtlasAllocator::new(size2(size as i32, size as i32));
        Self {
            size,
            bytes_per_pixel,
            allocator,
            texture,
            view,
            upload_count: 0,
            reset_count: 0,
            scratch: Vec::with_capacity(4096),
        }
    }

    fn clear(&mut self) {
        self.allocator.clear();
        self.reset_count = self.reset_count.saturating_add(1);
    }

    /// Allocates space with a 1px transparent padding ring and incrementally uploads ONLY
    /// the allocated sub-rectangle via `queue.write_texture`.
    fn allocate_and_upload(
        &mut self,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Option<[f32; 4]> {
        if width == 0 || height == 0 {
            return None;
        }
        let padded_w = width.checked_add(TILE_PADDING * 2)?;
        let padded_h = height.checked_add(TILE_PADDING * 2)?;
        if padded_w > self.size || padded_h > self.size {
            return None;
        }

        let alloc = self
            .allocator
            .allocate(size2(padded_w as i32, padded_h as i32))?;
        let origin_x = alloc.rectangle.min.x as u32;
        let origin_y = alloc.rectangle.min.y as u32;

        // Pack into reusable scratch buffer with a 1px zeroed border to prevent bleed.
        let bpp = self.bytes_per_pixel as usize;
        let row_bytes = (padded_w as usize) * bpp;
        let total_bytes = row_bytes * (padded_h as usize);
        self.scratch.clear();
        self.scratch.resize(total_bytes, 0);

        let src_row_bytes = (width as usize) * bpp;
        for row in 0..(height as usize) {
            let dst_start =
                ((row + TILE_PADDING as usize) * row_bytes) + (TILE_PADDING as usize) * bpp;
            let src_start = row * src_row_bytes;
            self.scratch[dst_start..dst_start + src_row_bytes]
                .copy_from_slice(&pixels[src_start..src_start + src_row_bytes]);
        }

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: origin_x,
                    y: origin_y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &self.scratch,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_w * self.bytes_per_pixel),
                rows_per_image: Some(padded_h),
            },
            wgpu::Extent3d {
                width: padded_w,
                height: padded_h,
                depth_or_array_layers: 1,
            },
        );
        self.upload_count = self.upload_count.saturating_add(1);

        let inv = 1.0 / (self.size as f32);
        let u0 = ((origin_x + TILE_PADDING) as f32) * inv;
        let v0 = ((origin_y + TILE_PADDING) as f32) * inv;
        let u1 = ((origin_x + TILE_PADDING + width) as f32) * inv;
        let v1 = ((origin_y + TILE_PADDING + height) as f32) * inv;
        Some([u0, v0, u1, v1])
    }
}

/// Dual GPU glyph & SVG icon atlas powered by `cosmic-text`, `swash`, `etagere`, and `resvg`.
pub struct GlyphAtlas {
    font_system: FontSystem,
    swash_cache: SwashCache,
    mono_family: String,
    sans_family: String,

    mono_plane: AtlasPlane,
    color_plane: AtlasPlane,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,

    glyph_cache: HashMap<CacheKey, Option<AtlasEntry>>,
    ascii_cache: HashMap<AsciiCellKey, CachedAsciiGlyph>,
    box_drawing_cache: HashMap<BoxDrawingKey, Option<AtlasEntry>>,
    terminal_run_cache: HashMap<TerminalRunStyleKey, HashMap<String, Vec<CachedTerminalRunGlyph>>>,
    terminal_run_entries: usize,
    shaped_run_cache: HashMap<ShapedRunStyleKey, HashMap<String, Vec<RelativeGlyph>>>,
    shaped_run_entries: usize,
    icon_sources: HashMap<String, Vec<u8>>,
    icon_cache: HashMap<IconCacheKey, Option<AtlasEntry>>,

    generation: u64,
    /// Reusable `cosmic_text::Buffer` for single-run shaping (avoids repeated allocations).
    scratch_buffer: Buffer,
}

impl GlyphAtlas {
    /// Initializes system font discovery, creates the 2048x2048 R8 and 1024x1024 RGBA8 GPU
    /// atlases, and registers minimal built-in fallback SVG icons.
    pub fn new(device: &wgpu::Device) -> Self {
        let mut font_system = FontSystem::new();
        let (mono_family, sans_family) = discover_preferred_families(&font_system);
        info!(
            mono_family = %mono_family,
            sans_family = %sans_family,
            faces = font_system.db().len(),
            "initialized cosmic-text font system"
        );

        let mono_plane = AtlasPlane::new(
            device,
            "omarex-glyph-atlas-r8",
            MONO_ATLAS_SIZE,
            wgpu::TextureFormat::R8Unorm,
            1,
        );
        let color_plane = AtlasPlane::new(
            device,
            "omarex-glyph-atlas-rgba8",
            COLOR_ATLAS_SIZE,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            4,
        );

        let mono_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("omarex-mono-atlas-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let color_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("omarex-color-atlas-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("omarex-glyph-atlas-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("omarex-glyph-atlas-bg"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&mono_plane.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&mono_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&color_plane.view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&color_sampler),
                },
            ],
        });

        let scratch_buffer = Buffer::new(&mut font_system, Metrics::new(14.0, 20.0));

        let mut atlas = Self {
            font_system,
            swash_cache: SwashCache::new(),
            mono_family,
            sans_family,
            mono_plane,
            color_plane,
            bind_group_layout,
            bind_group,
            glyph_cache: HashMap::with_capacity(512),
            ascii_cache: HashMap::with_capacity(256),
            box_drawing_cache: HashMap::with_capacity(128),
            terminal_run_cache: HashMap::with_capacity(16),
            terminal_run_entries: 0,
            shaped_run_cache: HashMap::with_capacity(32),
            shaped_run_entries: 0,
            icon_sources: HashMap::new(),
            icon_cache: HashMap::new(),
            generation: 1,
            scratch_buffer,
        };
        atlas.register_builtin_fallback_icons();
        atlas
    }

    pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.bind_group_layout
    }

    pub fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    pub fn mono_family_name(&self) -> &str {
        &self.mono_family
    }

    pub fn sans_family_name(&self) -> &str {
        &self.sans_family
    }

    /// Monotonically increasing generation incremented whenever an atlas plane resets on overflow.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn mono_upload_count(&self) -> u64 {
        self.mono_plane.upload_count
    }

    pub fn color_upload_count(&self) -> u64 {
        self.color_plane.upload_count
    }

    pub fn reset_count(&self) -> u64 {
        self.mono_plane.reset_count + self.color_plane.reset_count
    }

    /// Overrides the preferred monospace font family (e.g. from Ghostty/Omarchy config).
    pub fn set_mono_family(&mut self, family: impl Into<String>) {
        let family = family.into();
        if self.mono_family == family {
            return;
        }
        self.mono_family = family;
        self.ascii_cache.clear();
        self.terminal_run_cache.clear();
        self.terminal_run_entries = 0;
        self.shaped_run_cache.clear();
        self.shaped_run_entries = 0;
        self.generation = self.generation.saturating_add(1);
    }

    /// Overrides the preferred UI sans-serif font family.
    pub fn set_sans_family(&mut self, family: impl Into<String>) {
        let family = family.into();
        if self.sans_family == family {
            return;
        }
        self.sans_family = family;
        self.shaped_run_cache.clear();
        self.shaped_run_entries = 0;
        self.generation = self.generation.saturating_add(1);
    }

    /// Registers an SVG document by icon name (overwriting any previous definition).
    pub fn register_svg(&mut self, name: impl Into<String>, svg_bytes: impl Into<Vec<u8>>) {
        let name = name.into();
        self.icon_cache.retain(|k, _| k.name != name);
        self.icon_sources.insert(name, svg_bytes.into());
    }

    /// Loads all `*.svg` files in `dir`, registering each under its file stem (e.g. `claude.svg` -> `"claude"`).
    pub fn load_icons_from_dir(&mut self, dir: impl AsRef<Path>) -> Result<usize, RenderError> {
        let mut loaded = 0usize;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("svg")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                let bytes = std::fs::read(&path)?;
                self.register_svg(stem.to_owned(), bytes);
                loaded += 1;
            }
        }
        debug!(loaded, "loaded SVG icon assets into atlas registry");
        Ok(loaded)
    }

    /// Computes exact physical-pixel-aligned monospace cell metrics for `font_size` (logical px).
    pub fn measure_monospace_cell(&mut self, font_size: f32, scale_factor: f32) -> CellMetrics {
        let scale = scale_factor.max(0.5);
        let size = font_size.max(6.0);
        let line_height_logical = (size * 1.38).ceil();

        let family =
            resolve_cosmic_family(&self.mono_family, &self.sans_family, &FontFamily::Monospace);
        let attrs = Attrs::new().family(family);

        self.scratch_buffer
            .set_metrics(Metrics::new(size, line_height_logical));
        self.scratch_buffer.set_size(None, None);
        self.scratch_buffer.set_wrap(Wrap::None);
        self.scratch_buffer
            .set_text("MMMMMMMMMM", &attrs, Shaping::Advanced, None);
        self.scratch_buffer
            .shape_until_scroll(&mut self.font_system, false);

        let mut advance_sum = 0.0f32;
        let mut glyph_count = 0u32;
        let mut baseline = size * 0.82;
        if let Some(run) = self.scratch_buffer.layout_runs().next() {
            baseline = run.line_y;
            for g in run.glyphs {
                advance_sum += g.w;
                glyph_count += 1;
            }
        }

        let raw_cell_w = if glyph_count > 0 {
            advance_sum / (glyph_count as f32)
        } else {
            size * 0.6
        };

        // Snap cell dimensions to integer physical pixels for razor-sharp grid alignment.
        let cell_w_px = (raw_cell_w * scale).round().max(1.0);
        let cell_h_px = (line_height_logical * scale).round().max(1.0);
        let cell_width = cell_w_px / scale;
        let cell_height = cell_h_px / scale;
        let baseline_y = (baseline * scale).round() / scale;
        let underline_thickness = (size * 0.085 * scale).round().max(1.0) / scale;
        let underline_y = (baseline_y + size * 0.16).min(cell_height - underline_thickness);
        let strikethrough_y = (baseline_y - size * 0.30).max(1.0);

        CellMetrics {
            font_size: size,
            scale_factor: scale,
            cell_width,
            cell_height,
            baseline_y,
            underline_y,
            underline_thickness,
            strikethrough_y,
        }
    }

    /// Measures logical `(width, height)` of a single-line or bounded text string.
    #[allow(clippy::too_many_arguments)]
    pub fn measure_text(
        &mut self,
        text: &str,
        font_size: f32,
        line_height: f32,
        family: &FontFamily,
        weight: FontWeight,
        style: FontStyle,
        max_width: Option<f32>,
    ) -> (f32, f32) {
        if text.is_empty() {
            return (0.0, line_height);
        }
        let cosmic_family = resolve_cosmic_family(&self.mono_family, &self.sans_family, family);
        let attrs = Attrs::new()
            .family(cosmic_family)
            .weight(weight.to_cosmic())
            .style(style.to_cosmic());

        self.scratch_buffer
            .set_metrics(Metrics::new(font_size.max(4.0), line_height.max(4.0)));
        self.scratch_buffer.set_size(max_width, None);
        self.scratch_buffer.set_wrap(if max_width.is_some() {
            Wrap::WordOrGlyph
        } else {
            Wrap::None
        });
        self.scratch_buffer
            .set_text(text, &attrs, Shaping::Advanced, None);
        self.scratch_buffer
            .shape_until_scroll(&mut self.font_system, false);

        let mut max_w = 0.0f32;
        let mut total_h = 0.0f32;
        for run in self.scratch_buffer.layout_runs() {
            max_w = max_w.max(run.line_w);
            total_h = total_h.max(run.line_top + run.line_height);
        }
        (max_w, total_h.max(line_height))
    }

    /// Measures exact logical horizontal advance of `text`, including leading/trailing whitespace.
    #[allow(clippy::too_many_arguments)]
    pub fn measure_text_advance(
        &mut self,
        text: &str,
        font_size: f32,
        line_height: f32,
        family: &FontFamily,
        weight: FontWeight,
        style: FontStyle,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let with_sentinel = format!("{text}|");
        let (w_all, _) = self.measure_text(
            &with_sentinel,
            font_size,
            line_height,
            family,
            weight,
            style,
            None,
        );
        let (w_pipe, _) =
            self.measure_text("|", font_size, line_height, family, weight, style, None);
        (w_all - w_pipe).max(0.0)
    }

    /// Shapes a high-level [`TextRun`] and emits physical-pixel [`GlyphInstance`]s into `out`.
    ///
    /// Caches shaped relative glyph placements per `(style, text)` so static UI chrome strings
    /// (sidebar rows, tab pills, headers, palette entries) bypass HarfBuzz shaping on steady frames.
    pub fn shape_text_run(
        &mut self,
        run: &TextRun,
        scale_factor: f32,
        queue: &wgpu::Queue,
        out: &mut Vec<GlyphInstance>,
    ) {
        if run.text.is_empty() || run.color.a <= 0.001 {
            return;
        }
        let scale = scale_factor.max(0.5);
        let origin_x_px = (run.x * scale).round();
        let origin_y_px = (run.y * scale).round();
        let clip_px = run.clip_rect.scale(scale).to_array();
        let color = run.color.to_array();

        let style_key = ShapedRunStyleKey {
            font_size_bits: run.font_size.max(4.0).to_bits(),
            line_height_bits: run.line_height.max(run.font_size.max(4.0)).to_bits(),
            family: run.family.clone(),
            weight: run.weight,
            style: run.style,
            max_width_bits: run.max_width.map(f32::to_bits),
            align: run.align,
            scale_bits: scale.to_bits(),
        };

        // Fast path: zero-allocation lookup for cached shaped UI text runs.
        if let Some(cached) = self
            .shaped_run_cache
            .get(&style_key)
            .and_then(|bucket| bucket.get(run.text.as_str()))
        {
            out.reserve(cached.len());
            for g in cached {
                out.push(GlyphInstance {
                    bounds: [
                        origin_x_px + g.offset_x,
                        origin_y_px + g.offset_y,
                        g.width,
                        g.height,
                    ],
                    uv_bounds: g.uv_bounds,
                    clip_rect: clip_px,
                    color,
                    mode: if g.is_color {
                        GLYPH_MODE_COLOR
                    } else {
                        GLYPH_MODE_MONO
                    },
                    _pad: [0; 3],
                });
            }
            return;
        }

        // Cache miss: shape at integer origin (0, 0) via cosmic-text and cache relative glyphs.
        let family = resolve_cosmic_family(&self.mono_family, &self.sans_family, &run.family);
        let attrs = Attrs::new()
            .family(family)
            .weight(run.weight.to_cosmic())
            .style(run.style.to_cosmic());

        let align = match run.align {
            TextAlign::Left => Some(Align::Left),
            TextAlign::Center => Some(Align::Center),
            TextAlign::Right => Some(Align::Right),
        };

        self.scratch_buffer.set_metrics(Metrics::new(
            run.font_size.max(4.0),
            run.line_height.max(run.font_size.max(4.0)),
        ));
        self.scratch_buffer.set_size(run.max_width, None);
        self.scratch_buffer.set_wrap(if run.max_width.is_some() {
            Wrap::WordOrGlyph
        } else {
            Wrap::None
        });
        self.scratch_buffer
            .set_text(&run.text, &attrs, Shaping::Advanced, align);
        self.scratch_buffer
            .shape_until_scroll(&mut self.font_system, false);

        let mut shaped: Vec<(CacheKey, i32, i32)> = Vec::new();
        for layout_run in self.scratch_buffer.layout_runs() {
            let line_y_px = (layout_run.line_y * scale).round();
            for glyph in layout_run.glyphs {
                let phys = glyph.physical((0.0, line_y_px), scale);
                shaped.push((phys.cache_key, phys.x, phys.y));
            }
        }

        let gen_before = self.generation;
        let mut relative_glyphs = Vec::with_capacity(shaped.len());
        for (cache_key, px, py) in shaped {
            if let Some(entry) = self.get_or_rasterize_glyph(cache_key, queue) {
                let rel = RelativeGlyph {
                    offset_x: (px + entry.left_px) as f32,
                    offset_y: (py - entry.top_px) as f32,
                    width: entry.width_px as f32,
                    height: entry.height_px as f32,
                    uv_bounds: entry.uv_bounds,
                    is_color: entry.is_color,
                };
                out.push(GlyphInstance {
                    bounds: [
                        origin_x_px + rel.offset_x,
                        origin_y_px + rel.offset_y,
                        rel.width,
                        rel.height,
                    ],
                    uv_bounds: rel.uv_bounds,
                    clip_rect: clip_px,
                    color,
                    mode: if rel.is_color {
                        GLYPH_MODE_COLOR
                    } else {
                        GLYPH_MODE_MONO
                    },
                    _pad: [0; 3],
                });
                relative_glyphs.push(rel);
            }
        }

        // Only cache if an atlas overflow reset didn't occur mid-run.
        if self.generation == gen_before {
            if self.shaped_run_entries >= MAX_SHAPED_RUN_CACHE_ENTRIES {
                self.shaped_run_cache.clear();
                self.shaped_run_entries = 0;
            }
            self.shaped_run_cache
                .entry(style_key)
                .or_default()
                .insert(run.text.clone(), relative_glyphs);
            self.shaped_run_entries += 1;
        }
    }

    /// Emits a cached SVG icon into `out` at physical pixel resolution.
    pub fn emit_icon(
        &mut self,
        icon: &IconDraw,
        scale_factor: f32,
        queue: &wgpu::Queue,
        out: &mut Vec<GlyphInstance>,
    ) {
        let scale = scale_factor.max(0.5);
        let px_w = (icon.bounds.width * scale).round().max(1.0) as u32;
        let px_h = (icon.bounds.height * scale).round().max(1.0) as u32;
        let mono_mask = icon.tint.is_some();

        let Some(entry) = self.get_or_rasterize_icon(&icon.name, px_w, px_h, mono_mask, queue)
        else {
            return;
        };

        let x = (icon.bounds.x * scale).round();
        let y = (icon.bounds.y * scale).round();
        let clip_px = icon.clip_rect.scale(scale).to_array();
        let (color, mode) = match icon.tint {
            Some(tint) => (tint.mul_alpha(icon.opacity).to_array(), GLYPH_MODE_MONO),
            None => (
                Color::WHITE.with_alpha(icon.opacity).to_array(),
                GLYPH_MODE_COLOR,
            ),
        };

        out.push(GlyphInstance {
            bounds: [x, y, entry.width_px as f32, entry.height_px as f32],
            uv_bounds: entry.uv_bounds,
            clip_rect: clip_px,
            color,
            mode,
            _pad: [0; 3],
        });
    }

    /// Fast-path single ASCII monospace glyph lookup/rasterization for terminal rows.
    ///
    /// Emits row-local (`row_top == 0.0`) physical-pixel [`GlyphInstance`].
    #[allow(clippy::too_many_arguments)]
    pub fn emit_terminal_ascii_glyph(
        &mut self,
        ch: char,
        cell_x_px: f32,
        baseline_y_px: f32,
        font_size_px: f32,
        bold: bool,
        italic: bool,
        color: Color,
        queue: &wgpu::Queue,
        out: &mut Vec<GlyphInstance>,
    ) {
        if ch == ' ' || ch.is_control() {
            return;
        }
        let key = AsciiCellKey {
            ch,
            bold,
            italic,
            font_size_bits: font_size_px.to_bits(),
        };

        let cached = if let Some(&hit) = self.ascii_cache.get(&key) {
            hit
        } else {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            let family =
                resolve_cosmic_family(&self.mono_family, &self.sans_family, &FontFamily::Monospace);
            let attrs = Attrs::new()
                .family(family)
                .weight(if bold {
                    cosmic_text::Weight::BOLD
                } else {
                    cosmic_text::Weight::NORMAL
                })
                .style(if italic {
                    cosmic_text::Style::Italic
                } else {
                    cosmic_text::Style::Normal
                });

            let line_h = (font_size_px * 1.38).ceil();
            self.scratch_buffer
                .set_metrics(Metrics::new(font_size_px, line_h));
            self.scratch_buffer.set_size(None, None);
            self.scratch_buffer.set_wrap(Wrap::None);
            self.scratch_buffer
                .set_text(s, &attrs, Shaping::Advanced, None);
            self.scratch_buffer
                .shape_until_scroll(&mut self.font_system, false);

            let first = self.scratch_buffer.layout_runs().next().and_then(|run| {
                run.glyphs.first().map(|g| {
                    let phys = g.physical((0.0, 0.0), 1.0);
                    (phys.cache_key, phys.x as f32)
                })
            });

            let computed = if let Some((cache_key, x_off)) = first {
                CachedAsciiGlyph {
                    entry: self.get_or_rasterize_glyph(cache_key, queue),
                    x_offset_px: x_off,
                }
            } else {
                CachedAsciiGlyph {
                    entry: None,
                    x_offset_px: 0.0,
                }
            };
            self.ascii_cache.insert(key, computed);
            computed
        };

        if let Some(entry) = cached.entry {
            let x = (cell_x_px + cached.x_offset_px).round() + (entry.left_px as f32);
            let y = baseline_y_px.round() - (entry.top_px as f32);
            out.push(GlyphInstance {
                bounds: [x, y, entry.width_px as f32, entry.height_px as f32],
                uv_bounds: entry.uv_bounds,
                clip_rect: ClipRect::UNBOUNDED.to_array(),
                color: color.to_array(),
                mode: if entry.is_color {
                    GLYPH_MODE_COLOR
                } else {
                    GLYPH_MODE_MONO
                },
                _pad: [0; 3],
            });
        }
    }

    /// Emits a crisp procedural box-drawing, block-element, or Powerline glyph at row-local physical coords.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_procedural_terminal_glyph(
        &mut self,
        ch: char,
        cell_x_px: f32,
        cell_w_px: f32,
        cell_h_px: f32,
        color: Color,
        queue: &wgpu::Queue,
        out: &mut Vec<GlyphInstance>,
    ) {
        if let Some(entry) = self.get_or_rasterize_box_drawing(
            ch,
            cell_w_px.round().max(1.0) as u32,
            cell_h_px.round().max(1.0) as u32,
            queue,
        ) {
            out.push(GlyphInstance {
                bounds: [
                    cell_x_px.round(),
                    0.0,
                    entry.width_px as f32,
                    entry.height_px as f32,
                ],
                uv_bounds: entry.uv_bounds,
                clip_rect: ClipRect::UNBOUNDED.to_array(),
                color: color.to_array(),
                mode: GLYPH_MODE_MONO,
                _pad: [0; 3],
            });
        }
    }

    /// Shapes a contiguous terminal run (ASCII operator/ligature sequence or complex/non-ASCII run)
    /// and anchors every glyph cluster back to its exact grid column `col * cell_w_px` so ligatures
    /// and wide characters produce zero cumulative horizontal drift.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_terminal_shaped_run(
        &mut self,
        text: &str,
        run_cells: &[ShapedRunCell],
        cell_w_px: f32,
        cell_h_px: f32,
        baseline_y_px: f32,
        font_size_px: f32,
        bold: bool,
        italic: bool,
        color: Color,
        queue: &wgpu::Queue,
        out: &mut Vec<GlyphInstance>,
    ) {
        if text.is_empty() || run_cells.is_empty() {
            return;
        }
        let style_key = TerminalRunStyleKey {
            bold,
            italic,
            font_size_bits: font_size_px.to_bits(),
            cell_w_bits: cell_w_px.to_bits(),
            cell_h_bits: cell_h_px.to_bits(),
        };

        if let Some(cached) = self
            .terminal_run_cache
            .get(&style_key)
            .and_then(|bucket| bucket.get(text))
        {
            emit_cached_terminal_run(cached, run_cells, cell_w_px, baseline_y_px, color, out);
            return;
        }

        let family =
            resolve_cosmic_family(&self.mono_family, &self.sans_family, &FontFamily::Monospace);
        let weight = if bold {
            cosmic_text::Weight::BOLD
        } else {
            cosmic_text::Weight::NORMAL
        };
        let style = if italic {
            cosmic_text::Style::Italic
        } else {
            cosmic_text::Style::Normal
        };

        let mut shaped_keys: Vec<(u32, i32, i32, bool, f32, CacheKey)> = Vec::new();
        let mut shaped_via_harfrust = false;

        // Fast HarfBuzz path directly via `cosmic_text::harfrust` for contiguous ASCII operator runs
        // (`->`, `=>`, `!==`, `===`, `::`, `|>`, `<!--`, etc.), avoiding `unicode_linebreak` splitting.
        if text.is_ascii()
            && let Some(font_id) = self.font_system.db().query(&cosmic_text::fontdb::Query {
                families: &[family, Family::Monospace],
                weight,
                stretch: cosmic_text::Stretch::Normal,
                style,
            })
            && let Some(font) = self.font_system.get_font(font_id, weight)
        {
            let mut flags = CacheKeyFlags::empty();
            if italic
                && self
                    .font_system
                    .db()
                    .face(font_id)
                    .is_some_and(|f| f.style == cosmic_text::Style::Normal)
            {
                flags |= CacheKeyFlags::FAKE_ITALIC;
            }

            let mut hb_buf = cosmic_text::harfrust::UnicodeBuffer::new();
            hb_buf.push_str(text);
            hb_buf.guess_segment_properties();
            let glyph_buf = font.shaper().shape(hb_buf, &[]);
            let infos = glyph_buf.glyph_infos();
            let positions = glyph_buf.glyph_positions();

            if !infos.is_empty()
                && infos
                    .iter()
                    .all(|info| info.glyph_id != 0 && u16::try_from(info.glyph_id).is_ok())
            {
                let upem = f32::from(font.metrics().units_per_em).max(1.0);
                let scale_em = font_size_px / upem;
                let mut pen_x_px = 0.0f32;
                let mut prev_cluster: Option<u32> = None;
                let mut cluster_base_x = 0.0f32;

                shaped_keys.reserve(infos.len());
                for (info, pos) in infos.iter().zip(positions.iter()) {
                    if prev_cluster != Some(info.cluster) {
                        prev_cluster = Some(info.cluster);
                        cluster_base_x = pen_x_px;
                    }
                    let dx_px = (pen_x_px - cluster_base_x) + (pos.x_offset as f32) * scale_em;
                    let dy_px = (pos.y_offset as f32) * scale_em;
                    pen_x_px += (pos.x_advance as f32) * scale_em;

                    let (cache_key, px_dx, py_dy) = CacheKey::new(
                        font.id(),
                        info.glyph_id as u16,
                        font_size_px,
                        (dx_px, -dy_px),
                        weight,
                        flags,
                    );
                    shaped_keys.push((info.cluster, px_dx, py_dy, false, 0.0, cache_key));
                }
                shaped_via_harfrust = true;
            }
        }

        // Complex / non-ASCII / fallback path via `cosmic_text::Buffer` (`Shaping::Advanced`).
        if !shaped_via_harfrust {
            let attrs = Attrs::new().family(family).weight(weight).style(style);
            self.scratch_buffer
                .set_metrics(Metrics::new(font_size_px, cell_h_px.max(font_size_px)));
            self.scratch_buffer.set_size(None, None);
            self.scratch_buffer.set_wrap(Wrap::None);
            self.scratch_buffer
                .set_text(text, &attrs, Shaping::Advanced, None);
            self.scratch_buffer
                .shape_until_scroll(&mut self.font_system, false);

            for run in self.scratch_buffer.layout_runs() {
                let mut prev_start: Option<usize> = None;
                let mut cluster_base_x = 0.0f32;
                for g in run.glyphs {
                    let is_rtl = g.level.is_rtl();
                    if is_rtl {
                        let phys = g.physical((0.0, 0.0), 1.0);
                        let rtl_x = g.x + g.font_size * g.x_offset;
                        shaped_keys.push((g.start as u32, 0, phys.y, true, rtl_x, phys.cache_key));
                    } else {
                        if prev_start != Some(g.start) {
                            prev_start = Some(g.start);
                            cluster_base_x = g.x;
                        }
                        let dx_px = (g.x - cluster_base_x) + g.font_size * g.x_offset;
                        let dy_px = g.font_size * g.y_offset;
                        let (cache_key, px_dx, py_dy) = CacheKey::new(
                            g.font_id,
                            g.glyph_id,
                            g.font_size,
                            (dx_px, -dy_px),
                            g.font_weight,
                            g.cache_key_flags,
                        );
                        shaped_keys.push((g.start as u32, px_dx, py_dy, false, 0.0, cache_key));
                    }
                }
            }
        }

        let gen_before = self.generation;
        let mut cached_run = Vec::with_capacity(shaped_keys.len());
        for (cluster_byte, px_dx, py_dy, is_rtl, rtl_x_px, cache_key) in shaped_keys {
            if let Some(entry) = self.get_or_rasterize_glyph(cache_key, queue) {
                cached_run.push(CachedTerminalRunGlyph {
                    cluster_byte,
                    px_dx,
                    py_dy,
                    is_rtl,
                    rtl_x_px,
                    entry,
                });
            }
        }

        emit_cached_terminal_run(&cached_run, run_cells, cell_w_px, baseline_y_px, color, out);

        if self.generation == gen_before {
            if self.terminal_run_entries >= MAX_TERMINAL_RUN_CACHE_ENTRIES {
                self.terminal_run_cache.clear();
                self.terminal_run_entries = 0;
            }
            self.terminal_run_cache
                .entry(style_key)
                .or_default()
                .insert(text.to_owned(), cached_run);
            self.terminal_run_entries += 1;
        }
    }

    /// Shapes an arbitrary terminal grapheme cluster or ligature span at row-local physical coords.
    ///
    /// Uses crisp procedural geometric rasterization for box-drawing (`U+2500..=U+257F`) and
    /// block-element (`U+2580..=U+259F`) glyphs so TUI borders join with zero pixel seams.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_terminal_grapheme(
        &mut self,
        text: &str,
        cell_x_px: f32,
        cell_w_px: f32,
        cell_h_px: f32,
        baseline_y_px: f32,
        font_size_px: f32,
        bold: bool,
        italic: bool,
        color: Color,
        queue: &wgpu::Queue,
        out: &mut Vec<GlyphInstance>,
    ) {
        if text.is_empty() || text == " " {
            return;
        }

        // Fast path: single-codepoint checks (ASCII or procedural box-drawing/block-element).
        let mut chars = text.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            if ch.is_ascii() {
                self.emit_terminal_ascii_glyph(
                    ch,
                    cell_x_px,
                    baseline_y_px,
                    font_size_px,
                    bold,
                    italic,
                    color,
                    queue,
                    out,
                );
                return;
            }
            if is_procedural_box_or_block(ch) {
                self.emit_procedural_terminal_glyph(
                    ch, cell_x_px, cell_w_px, cell_h_px, color, queue, out,
                );
                return;
            }
        }

        // General path: full cosmic-text + HarfBuzz shaping (ligatures, emoji, Nerd Fonts, CJK).
        let family =
            resolve_cosmic_family(&self.mono_family, &self.sans_family, &FontFamily::Monospace);
        let attrs = Attrs::new()
            .family(family)
            .weight(if bold {
                cosmic_text::Weight::BOLD
            } else {
                cosmic_text::Weight::NORMAL
            })
            .style(if italic {
                cosmic_text::Style::Italic
            } else {
                cosmic_text::Style::Normal
            });

        self.scratch_buffer
            .set_metrics(Metrics::new(font_size_px, cell_h_px.max(font_size_px)));
        self.scratch_buffer.set_size(None, None);
        self.scratch_buffer.set_wrap(Wrap::None);
        self.scratch_buffer
            .set_text(text, &attrs, Shaping::Advanced, None);
        self.scratch_buffer
            .shape_until_scroll(&mut self.font_system, false);

        let mut shaped: Vec<(CacheKey, i32, i32)> = Vec::new();
        for run in self.scratch_buffer.layout_runs() {
            for g in run.glyphs {
                let phys = g.physical((cell_x_px.round(), baseline_y_px.round()), 1.0);
                shaped.push((phys.cache_key, phys.x, phys.y));
            }
        }

        for (cache_key, px, py) in shaped {
            if let Some(entry) = self.get_or_rasterize_glyph(cache_key, queue) {
                let x = (px + entry.left_px) as f32;
                let y = (py - entry.top_px) as f32;
                out.push(GlyphInstance {
                    bounds: [x, y, entry.width_px as f32, entry.height_px as f32],
                    uv_bounds: entry.uv_bounds,
                    clip_rect: ClipRect::UNBOUNDED.to_array(),
                    color: color.to_array(),
                    mode: if entry.is_color {
                        GLYPH_MODE_COLOR
                    } else {
                        GLYPH_MODE_MONO
                    },
                    _pad: [0; 3],
                });
            }
        }
    }

    fn get_or_rasterize_glyph(
        &mut self,
        cache_key: CacheKey,
        queue: &wgpu::Queue,
    ) -> Option<AtlasEntry> {
        if let Some(&cached) = self.glyph_cache.get(&cache_key) {
            return cached;
        }

        let Some(image) = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, cache_key)
        else {
            self.glyph_cache.insert(cache_key, None);
            return None;
        };

        let w = image.placement.width;
        let h = image.placement.height;
        if w == 0 || h == 0 {
            self.glyph_cache.insert(cache_key, None);
            return None;
        }

        let entry = match image.content {
            SwashContent::Mask => {
                let uv = self.upload_mono_with_reset(queue, w, h, &image.data)?;
                AtlasEntry {
                    uv_bounds: uv,
                    width_px: w,
                    height_px: h,
                    left_px: image.placement.left,
                    top_px: image.placement.top,
                    is_color: false,
                }
            }
            SwashContent::Color => {
                let uv = self.upload_color_with_reset(queue, w, h, &image.data)?;
                AtlasEntry {
                    uv_bounds: uv,
                    width_px: w,
                    height_px: h,
                    left_px: image.placement.left,
                    top_px: image.placement.top,
                    is_color: true,
                }
            }
            SwashContent::SubpixelMask => {
                // Convert 3-channel LCD coverage to luminance grayscale mask for R8Unorm atlas.
                let mut mask = Vec::with_capacity((w * h) as usize);
                for rgb in image.data.as_chunks::<3>().0 {
                    let avg =
                        ((u16::from(rgb[0]) + u16::from(rgb[1]) + u16::from(rgb[2])) / 3) as u8;
                    mask.push(avg);
                }
                let uv = self.upload_mono_with_reset(queue, w, h, &mask)?;
                AtlasEntry {
                    uv_bounds: uv,
                    width_px: w,
                    height_px: h,
                    left_px: image.placement.left,
                    top_px: image.placement.top,
                    is_color: false,
                }
            }
        };

        self.glyph_cache.insert(cache_key, Some(entry));
        Some(entry)
    }

    fn get_or_rasterize_box_drawing(
        &mut self,
        ch: char,
        cell_w_px: u32,
        cell_h_px: u32,
        queue: &wgpu::Queue,
    ) -> Option<AtlasEntry> {
        let key = BoxDrawingKey {
            ch,
            cell_w_px,
            cell_h_px,
        };
        if let Some(&cached) = self.box_drawing_cache.get(&key) {
            return cached;
        }

        let mask = rasterize_box_or_block_mask(ch, cell_w_px, cell_h_px)?;
        let uv = self.upload_mono_with_reset(queue, cell_w_px, cell_h_px, &mask)?;
        let entry = AtlasEntry {
            uv_bounds: uv,
            width_px: cell_w_px,
            height_px: cell_h_px,
            left_px: 0,
            top_px: 0,
            is_color: false,
        };
        self.box_drawing_cache.insert(key, Some(entry));
        Some(entry)
    }

    fn get_or_rasterize_icon(
        &mut self,
        name: &str,
        px_w: u32,
        px_h: u32,
        mono_mask: bool,
        queue: &wgpu::Queue,
    ) -> Option<AtlasEntry> {
        let key = IconCacheKey {
            name: name.to_owned(),
            px_w,
            px_h,
            mono_mask,
        };
        if let Some(&cached) = self.icon_cache.get(&key) {
            return cached;
        }

        let svg_bytes = self.icon_sources.get(name)?.clone();
        let entry = match rasterize_svg_bytes(&svg_bytes, px_w, px_h, mono_mask) {
            Ok(RasterizedSvg::Mono(mask)) => self
                .upload_mono_with_reset(queue, px_w, px_h, &mask)
                .map(|uv| AtlasEntry {
                    uv_bounds: uv,
                    width_px: px_w,
                    height_px: px_h,
                    left_px: 0,
                    top_px: 0,
                    is_color: false,
                }),
            Ok(RasterizedSvg::Color(rgba)) => self
                .upload_color_with_reset(queue, px_w, px_h, &rgba)
                .map(|uv| AtlasEntry {
                    uv_bounds: uv,
                    width_px: px_w,
                    height_px: px_h,
                    left_px: 0,
                    top_px: 0,
                    is_color: true,
                }),
            Err(err) => {
                warn!(icon = name, error = %err, "failed to rasterize SVG icon");
                None
            }
        };

        self.icon_cache.insert(key, entry);
        entry
    }

    fn upload_mono_with_reset(
        &mut self,
        queue: &wgpu::Queue,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> Option<[f32; 4]> {
        if let Some(uv) = self.mono_plane.allocate_and_upload(queue, w, h, pixels) {
            return Some(uv);
        }
        warn!("R8 glyph atlas full; resetting monochrome atlas and incrementing generation");
        self.reset_mono_plane();
        self.mono_plane.allocate_and_upload(queue, w, h, pixels)
    }

    fn upload_color_with_reset(
        &mut self,
        queue: &wgpu::Queue,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) -> Option<[f32; 4]> {
        if let Some(uv) = self.color_plane.allocate_and_upload(queue, w, h, pixels) {
            return Some(uv);
        }
        warn!("RGBA8 color atlas full; resetting color atlas and incrementing generation");
        self.reset_color_plane();
        self.color_plane.allocate_and_upload(queue, w, h, pixels)
    }

    /// Explicitly resets the monochrome atlas plane (also triggered automatically on overflow).
    pub fn reset_mono_plane(&mut self) {
        self.mono_plane.clear();
        self.glyph_cache
            .retain(|_, v| v.is_some_and(|entry| entry.is_color));
        self.ascii_cache.clear();
        self.box_drawing_cache.clear();
        self.terminal_run_cache.clear();
        self.terminal_run_entries = 0;
        self.shaped_run_cache.clear();
        self.shaped_run_entries = 0;
        self.icon_cache.retain(|k, _| !k.mono_mask);
        self.generation = self.generation.saturating_add(1);
    }

    /// Explicitly resets the color atlas plane (also triggered automatically on overflow).
    pub fn reset_color_plane(&mut self) {
        self.color_plane.clear();
        self.glyph_cache
            .retain(|_, v| v.is_some_and(|entry| !entry.is_color));
        self.terminal_run_cache.clear();
        self.terminal_run_entries = 0;
        self.shaped_run_cache.clear();
        self.shaped_run_entries = 0;
        self.icon_cache.retain(|k, _| k.mono_mask);
        self.generation = self.generation.saturating_add(1);
    }

    fn register_builtin_fallback_icons(&mut self) {
        const BUILTINS: &[(&str, &str)] = &[
            (
                "terminal",
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 17 10 11 4 5"/><line x1="12" y1="19" x2="20" y2="19"/></svg>"##,
            ),
            (
                "claude",
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect width="24" height="24" rx="6" fill="#D96B43"/><g stroke="#ffffff" stroke-width="1.9" stroke-linecap="round"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/><line x1="7.05" y1="7.05" x2="16.95" y2="16.95"/><line x1="16.95" y1="7.05" x2="7.05" y2="16.95"/></g></svg>"##,
            ),
            (
                "plus",
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>"##,
            ),
            (
                "close",
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg>"##,
            ),
            (
                "search",
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round"><circle cx="11" cy="11" r="7"/><line x1="20" y1="20" x2="16.65" y2="16.65"/></svg>"##,
            ),
            (
                "sidebar",
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><line x1="9" y1="4" x2="9" y2="20"/></svg>"##,
            ),
            (
                "activity",
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M2 12s4-6 10-6 10 6 10 6-4 6-10 6-10-6-10-6z"/><circle cx="12" cy="12" r="2.5"/></svg>"##,
            ),
        ];

        for &(name, svg) in BUILTINS {
            self.icon_sources
                .insert(name.to_owned(), svg.as_bytes().to_vec());
        }
    }
}

fn emit_cached_terminal_run(
    cached: &[CachedTerminalRunGlyph],
    run_cells: &[ShapedRunCell],
    cell_w_px: f32,
    baseline_y_px: f32,
    color: Color,
    out: &mut Vec<GlyphInstance>,
) {
    let base_y = baseline_y_px.round() as i32;
    let start_col_x = f32::from(run_cells[0].col) * cell_w_px;
    let color_arr = color.to_array();
    out.reserve(cached.len());
    for g in cached {
        let anchor_x_px = if g.is_rtl {
            (start_col_x + g.rtl_x_px).round() as i32
        } else {
            let idx = run_cells
                .partition_point(|c| c.byte_start <= g.cluster_byte)
                .saturating_sub(1);
            (f32::from(run_cells[idx].col) * cell_w_px).round() as i32 + g.px_dx
        };
        let x = (anchor_x_px + g.entry.left_px) as f32;
        let y = (base_y + g.py_dy - g.entry.top_px) as f32;
        out.push(GlyphInstance {
            bounds: [x, y, g.entry.width_px as f32, g.entry.height_px as f32],
            uv_bounds: g.entry.uv_bounds,
            clip_rect: ClipRect::UNBOUNDED.to_array(),
            color: color_arr,
            mode: if g.entry.is_color {
                GLYPH_MODE_COLOR
            } else {
                GLYPH_MODE_MONO
            },
            _pad: [0; 3],
        });
    }
}

fn resolve_cosmic_family<'a>(
    mono_family: &'a str,
    sans_family: &'a str,
    family: &'a FontFamily,
) -> Family<'a> {
    match family {
        FontFamily::Monospace => {
            if mono_family.is_empty() {
                Family::Monospace
            } else {
                Family::Name(mono_family)
            }
        }
        FontFamily::SansSerif => {
            if sans_family.is_empty() {
                Family::SansSerif
            } else {
                Family::Name(sans_family)
            }
        }
        FontFamily::Named(name) => Family::Name(name.as_str()),
    }
}

enum RasterizedSvg {
    Mono(Vec<u8>),
    Color(Vec<u8>),
}

fn rasterize_svg_bytes(
    svg_bytes: &[u8],
    px_w: u32,
    px_h: u32,
    mono_mask: bool,
) -> Result<RasterizedSvg, RenderError> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(svg_bytes, &opt)
        .map_err(|e| RenderError::Svg(e.to_string()))?;

    let size = tree.size();
    let svg_w = size.width().max(1.0);
    let svg_h = size.height().max(1.0);
    let scale = (px_w as f32 / svg_w).min(px_h as f32 / svg_h);
    let offset_x = ((px_w as f32) - svg_w * scale) * 0.5;
    let offset_y = ((px_h as f32) - svg_h * scale) * 0.5;

    let mut pixmap = tiny_skia::Pixmap::new(px_w, px_h).ok_or(RenderError::InvalidDimensions {
        width: px_w,
        height: px_h,
    })?;

    let transform =
        tiny_skia::Transform::from_scale(scale, scale).post_translate(offset_x, offset_y);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let raw = pixmap.data();
    if mono_mask {
        let mut alpha = Vec::with_capacity((px_w * px_h) as usize);
        for px in raw.as_chunks::<4>().0 {
            alpha.push(px[3]);
        }
        Ok(RasterizedSvg::Mono(alpha))
    } else {
        // tiny-skia produces premultiplied sRGB RGBA8; unpremultiply into straight sRGB RGBA8
        // so Rgba8UnormSrgb sampling + ALPHA_BLENDING matches swash emoji and PNG textures.
        let mut rgba = Vec::with_capacity(raw.len());
        for px in raw.as_chunks::<4>().0 {
            let a = px[3];
            if a == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            } else if a == 255 {
                rgba.extend_from_slice(px);
            } else {
                let a_u16 = u16::from(a);
                let unpremul = |c: u8| -> u8 {
                    let v = (u16::from(c) * 255 + (a_u16 / 2)) / a_u16;
                    v.min(255) as u8
                };
                rgba.extend_from_slice(&[unpremul(px[0]), unpremul(px[1]), unpremul(px[2]), a]);
            }
        }
        Ok(RasterizedSvg::Color(rgba))
    }
}

fn discover_preferred_families(font_system: &FontSystem) -> (String, String) {
    let mono_candidates = [
        "JetBrainsMono Nerd Font",
        "JetBrainsMono NF",
        "JetBrains Mono",
        "Liberation Mono",
        "DejaVu Sans Mono",
    ];
    let sans_candidates = ["Inter", "Liberation Sans", "Noto Sans", "DejaVu Sans"];

    let mut mono = String::new();
    let mut sans = String::new();

    for candidate in mono_candidates {
        if has_family(font_system, candidate) {
            mono = candidate.to_owned();
            break;
        }
    }
    for candidate in sans_candidates {
        if has_family(font_system, candidate) {
            sans = candidate.to_owned();
            break;
        }
    }
    (mono, sans)
}

fn has_family(font_system: &FontSystem, target: &str) -> bool {
    font_system.db().faces().any(|face| {
        face.families
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(target))
    })
}

/// Returns `true` if `ch` is handled by our crisp procedural box-drawing / block-element / Powerline rasterizer.
pub fn is_procedural_box_or_block(ch: char) -> bool {
    matches!(
        ch,
        '─' | '━'
            | '│'
            | '┃'
            | '┌'
            | '┏'
            | '┐'
            | '┓'
            | '└'
            | '┗'
            | '┘'
            | '┛'
            | '├'
            | '┣'
            | '┤'
            | '┫'
            | '┬'
            | '┳'
            | '┴'
            | '┻'
            | '┼'
            | '╋'
            | '═'
            | '║'
            | '╔'
            | '╗'
            | '╚'
            | '╝'
            | '╠'
            | '╣'
            | '╦'
            | '╩'
            | '╬'
            | '╭'
            | '╮'
            | '╯'
            | '╰'
            | '█'
            | '▀'
            | '▄'
            | '▌'
            | '▐'
            | '░'
            | '▒'
            | '▓'
            | '▁'..='▇'
            | '▉'..='▏'
            | '▔'
            | '▕'
            | '▖'..='▟'
            | '◢'..='◥'
            | '\u{E0B0}'..='\u{E0C8}'
            | '\u{E0CA}'
            | '\u{E0CC}'..='\u{E0D2}'
            | '\u{E0D4}'
            | '\u{E0D6}'
            | '\u{E0D7}'
    )
}

/// Rasterizes pixel-exact seamless box-drawing and block-element glyphs directly to an R8 mask.
fn rasterize_box_or_block_mask(ch: char, w: u32, h: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 {
        return None;
    }
    let mut buf = vec![0u8; (w * h) as usize];
    let fill_rect = |buf: &mut [u8], x0: u32, y0: u32, x1: u32, y1: u32, val: u8| {
        let x0 = x0.min(w);
        let x1 = x1.min(w);
        let y0 = y0.min(h);
        let y1 = y1.min(h);
        for y in y0..y1 {
            let row = (y * w) as usize;
            for x in x0..x1 {
                let idx = row + x as usize;
                buf[idx] = buf[idx].max(val);
            }
        }
    };

    // Block elements & shades
    match ch {
        '█' => {
            buf.fill(255);
            return Some(buf);
        }
        '▀' => {
            fill_rect(&mut buf, 0, 0, w, h / 2, 255);
            return Some(buf);
        }
        '▄' => {
            fill_rect(&mut buf, 0, h / 2, w, h, 255);
            return Some(buf);
        }
        '▌' => {
            fill_rect(&mut buf, 0, 0, w / 2, h, 255);
            return Some(buf);
        }
        '▐' => {
            fill_rect(&mut buf, w / 2, 0, w, h, 255);
            return Some(buf);
        }
        '░' | '▒' | '▓' => {
            let alpha = match ch {
                '░' => 64,
                '▒' => 128,
                _ => 192,
            };
            buf.fill(alpha);
            return Some(buf);
        }
        '▁'..='▇' => {
            let eighths = (ch as u32) - ('▁' as u32) + 1;
            let bar_h = ((h * eighths) + 4) / 8;
            fill_rect(&mut buf, 0, h.saturating_sub(bar_h), w, h, 255);
            return Some(buf);
        }
        '▉' | '▊' | '▋' | '▍' | '▎' | '▏' => {
            let eighths = match ch {
                '▉' => 7,
                '▊' => 6,
                '▋' => 5,
                '▍' => 3,
                '▎' => 2,
                _ => 1,
            };
            let bar_w = ((w * eighths) + 4) / 8;
            fill_rect(&mut buf, 0, 0, bar_w, h, 255);
            return Some(buf);
        }
        '▔' => {
            let bar_h = ((h + 4) / 8).max(1);
            fill_rect(&mut buf, 0, 0, w, bar_h, 255);
            return Some(buf);
        }
        '▕' => {
            let bar_w = ((w + 4) / 8).max(1);
            fill_rect(&mut buf, w.saturating_sub(bar_w), 0, w, h, 255);
            return Some(buf);
        }
        '▖'..='▟' => {
            let hw = w / 2;
            let hh = h / 2;
            let (tl, tr, bl, br) = match ch {
                '▖' => (false, false, true, false),
                '▗' => (false, false, false, true),
                '▘' => (true, false, false, false),
                '▙' => (true, false, true, true),
                '▚' => (true, false, false, true),
                '▛' => (true, true, true, false),
                '▜' => (true, true, false, true),
                '▝' => (false, true, false, false),
                '▞' => (false, true, true, false),
                _ => (false, true, true, true), // '▟'
            };
            if tl {
                fill_rect(&mut buf, 0, 0, hw, hh, 255);
            }
            if tr {
                fill_rect(&mut buf, hw, 0, w, hh, 255);
            }
            if bl {
                fill_rect(&mut buf, 0, hh, hw, h, 255);
            }
            if br {
                fill_rect(&mut buf, hw, hh, w, h, 255);
            }
            return Some(buf);
        }
        '◢' | '◣' | '◤' | '◥' => {
            let wf = w as f32;
            let hf = h as f32;
            let verts = match ch {
                '◢' => [(wf, 0.0), (wf, hf), (0.0, hf)],
                '◣' => [(0.0, 0.0), (wf, hf), (0.0, hf)],
                '◤' => [(0.0, 0.0), (wf, 0.0), (0.0, hf)],
                _ => [(0.0, 0.0), (wf, 0.0), (wf, hf)],
            };
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                (0.5 - sd_polygon((px, py), &verts)).clamp(0.0, 1.0)
            });
            return Some(buf);
        }
        '\u{E0B0}'..='\u{E0C8}'
        | '\u{E0CA}'
        | '\u{E0CC}'..='\u{E0D2}'
        | '\u{E0D4}'
        | '\u{E0D6}'
        | '\u{E0D7}' => {
            return Some(rasterize_powerline_mask(ch, w, h));
        }
        _ => {}
    }

    let light = ((w.min(h) as f32) * 0.12).round().max(1.0) as u32;
    let heavy = (light * 2).min(w.min(h).max(2));
    let cx = w / 2;
    let cy = h / 2;

    // Rounded corners (`╭ ╮ ╯ ╰`) rendered with smooth SDF quarter-circle arcs.
    if matches!(ch, '╭' | '╮' | '╯' | '╰') {
        let stroke = light as f32;
        let r = (w.min(h) as f32 * 0.5).max(1.5);
        let (arc_cx, arc_cy, right, down) = match ch {
            '╭' => (w as f32, h as f32, true, true),
            '╮' => (0.0, h as f32, false, true),
            '╯' => (0.0, 0.0, false, false),
            _ => (w as f32, 0.0, true, false),
        };
        let half_s = stroke * 0.5;
        let cx_f = cx as f32 + 0.5;
        let cy_f = cy as f32 + 0.5;
        let radius = ((if right { w as f32 - cx_f } else { cx_f }).min(if down {
            h as f32 - cy_f
        } else {
            cy_f
        }))
        .max(1.0)
        .min(r);

        // Anchor quarter-circle center inside the quadrant so straight legs meet exact cell centers.
        let qx = if right { cx_f + radius } else { cx_f - radius };
        let qy = if down { cy_f + radius } else { cy_f - radius };

        for y in 0..h {
            let py = y as f32 + 0.5;
            for x in 0..w {
                let px = x as f32 + 0.5;
                let in_arc_x = if right { px <= qx } else { px >= qx };
                let in_arc_y = if down { py <= qy } else { py >= qy };

                let dist = if in_arc_x && in_arc_y {
                    let dx = px - qx;
                    let dy = py - qy;
                    ((dx * dx + dy * dy).sqrt() - radius).abs()
                } else if !in_arc_x
                    && ((down && py >= cy_f - half_s - 1.0 && py <= cy_f + half_s + 1.0)
                        || (!down && py >= cy_f - half_s - 1.0 && py <= cy_f + half_s + 1.0))
                {
                    let on_leg = if right { px >= qx } else { px <= qx };
                    if on_leg { (py - cy_f).abs() } else { 100.0 }
                } else if !in_arc_y {
                    let on_leg = if down { py >= qy } else { py <= qy };
                    if on_leg { (px - cx_f).abs() } else { 100.0 }
                } else {
                    100.0
                };

                let cov = (half_s + 0.5 - dist).clamp(0.0, 1.0);
                let _ = (arc_cx, arc_cy);
                if cov > 0.0 {
                    buf[(y * w + x) as usize] = (cov * 255.0).round() as u8;
                }
            }
        }
        return Some(buf);
    }

    // Double-line box drawing (`═ ║ ╔ ╗ ╚ ╝ ╠ ╣ ╦ ╩ ╬`) with exact inner/outer miter joints.
    if matches!(
        ch,
        '═' | '║' | '╔' | '╗' | '╚' | '╝' | '╠' | '╣' | '╦' | '╩' | '╬'
    ) {
        let gap = (light + 1).max(2);
        let t = light;
        let x_l = cx.saturating_sub(gap / 2 + t);
        let x_r = (cx + gap.div_ceil(2)).min(w.saturating_sub(t));
        let y_t = cy.saturating_sub(gap / 2 + t);
        let y_b = (cy + gap.div_ceil(2)).min(h.saturating_sub(t));

        let (left, right, up, down) = match ch {
            '═' => (true, true, false, false),
            '║' => (false, false, true, true),
            '╔' => (false, true, false, true),
            '╗' => (true, false, false, true),
            '╚' => (false, true, true, false),
            '╝' => (true, false, true, false),
            '╠' => (false, true, true, true),
            '╣' => (true, false, true, true),
            '╦' => (true, true, false, true),
            '╩' => (true, true, true, false),
            '╬' => (true, true, true, true),
            _ => return None,
        };

        // Top horizontal track (`y_t..y_t+t`): spans across unless split by an upward shaft.
        if left {
            let end_x = if up {
                x_l + t
            } else if right {
                w
            } else {
                x_r + t
            };
            fill_rect(&mut buf, 0, y_t, end_x, y_t + t, 255);
        }
        if right {
            let start_x = if up {
                x_r
            } else if left {
                0
            } else {
                x_l
            };
            fill_rect(&mut buf, start_x, y_t, w, y_t + t, 255);
        }

        // Bottom horizontal track (`y_b..y_b+t`): spans across unless split by a downward shaft.
        if left {
            let end_x = if down {
                x_l + t
            } else if right {
                w
            } else {
                x_r + t
            };
            fill_rect(&mut buf, 0, y_b, end_x, y_b + t, 255);
        }
        if right {
            let start_x = if down {
                x_r
            } else if left {
                0
            } else {
                x_l
            };
            fill_rect(&mut buf, start_x, y_b, w, y_b + t, 255);
        }

        // Left vertical track (`x_l..x_l+t`): spans vertically unless split by a leftward arm.
        if up {
            let end_y = if left {
                y_t + t
            } else if down {
                h
            } else {
                y_b + t
            };
            fill_rect(&mut buf, x_l, 0, x_l + t, end_y, 255);
        }
        if down {
            let start_y = if left {
                y_b
            } else if up {
                0
            } else {
                y_t
            };
            fill_rect(&mut buf, x_l, start_y, x_l + t, h, 255);
        }

        // Right vertical track (`x_r..x_r+t`): spans vertically unless split by a rightward arm.
        if up {
            let end_y = if right {
                y_t + t
            } else if down {
                h
            } else {
                y_b + t
            };
            fill_rect(&mut buf, x_r, 0, x_r + t, end_y, 255);
        }
        if down {
            let start_y = if right {
                y_b
            } else if up {
                0
            } else {
                y_t
            };
            fill_rect(&mut buf, x_r, start_y, x_r + t, h, 255);
        }
        return Some(buf);
    }

    // Single light & heavy straight box drawing.
    let (l_w, r_w, u_w, d_w) = match ch {
        '─' => (light, light, 0, 0),
        '━' => (heavy, heavy, 0, 0),
        '│' => (0, 0, light, light),
        '┃' => (0, 0, heavy, heavy),
        '┌' => (0, light, 0, light),
        '┏' => (0, heavy, 0, heavy),
        '┐' => (light, 0, 0, light),
        '┓' => (heavy, 0, 0, heavy),
        '└' => (0, light, light, 0),
        '┗' => (0, heavy, heavy, 0),
        '┘' => (light, 0, light, 0),
        '┛' => (heavy, 0, heavy, 0),
        '├' => (0, light, light, light),
        '┣' => (0, heavy, heavy, heavy),
        '┤' => (light, 0, light, light),
        '┫' => (heavy, 0, heavy, heavy),
        '┬' => (light, light, 0, light),
        '┳' => (heavy, heavy, 0, heavy),
        '┴' => (light, light, light, 0),
        '┻' => (heavy, heavy, heavy, 0),
        '┼' => (light, light, light, light),
        '╋' => (heavy, heavy, heavy, heavy),
        _ => return None,
    };

    let h_max = l_w.max(r_w);
    let v_max = u_w.max(d_w);
    let y0 = cy.saturating_sub(h_max / 2);
    let x0 = cx.saturating_sub(v_max / 2);

    if l_w > 0 {
        let ly = cy.saturating_sub(l_w / 2);
        fill_rect(&mut buf, 0, ly, x0 + v_max.max(l_w), ly + l_w, 255);
    }
    if r_w > 0 {
        let ry = cy.saturating_sub(r_w / 2);
        fill_rect(&mut buf, x0, ry, w, ry + r_w, 255);
    }
    if u_w > 0 {
        let ux = cx.saturating_sub(u_w / 2);
        fill_rect(&mut buf, ux, 0, ux + u_w, y0 + h_max.max(u_w), 255);
    }
    if d_w > 0 {
        let dx = cx.saturating_sub(d_w / 2);
        fill_rect(&mut buf, dx, y0, dx + d_w, h, 255);
    }

    Some(buf)
}

#[inline]
fn rasterize_sdf_mask(buf: &mut [u8], w: u32, h: u32, mut cov_fn: impl FnMut(f32, f32) -> f32) {
    for y in 0..h {
        let py = y as f32 + 0.5;
        let row = (y * w) as usize;
        for x in 0..w {
            let px = x as f32 + 0.5;
            let cov = cov_fn(px, py).clamp(0.0, 1.0);
            buf[row + x as usize] = (cov * 255.0).round() as u8;
        }
    }
}

#[inline]
fn sd_segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let pax = p.0 - a.0;
    let pay = p.1 - a.1;
    let bax = b.0 - a.0;
    let bay = b.1 - a.1;
    let denom = (bax * bax + bay * bay).max(1e-6);
    let h = ((pax * bax + pay * bay) / denom).clamp(0.0, 1.0);
    (pax - bax * h).hypot(pay - bay * h)
}

fn sd_polygon(p: (f32, f32), verts: &[(f32, f32)]) -> f32 {
    let n = verts.len();
    let mut d = (p.0 - verts[0].0) * (p.0 - verts[0].0) + (p.1 - verts[0].1) * (p.1 - verts[0].1);
    let mut s = 1.0f32;
    for i in 0..n {
        let j = if i == 0 { n - 1 } else { i - 1 };
        let vi = verts[i];
        let vj = verts[j];
        let ex = vj.0 - vi.0;
        let ey = vj.1 - vi.1;
        let wx = p.0 - vi.0;
        let wy = p.1 - vi.1;
        let denom = (ex * ex + ey * ey).max(1e-6);
        let t = ((wx * ex + wy * ey) / denom).clamp(0.0, 1.0);
        let bx = wx - ex * t;
        let by = wy - ey * t;
        d = d.min(bx * bx + by * by);
        let c0 = p.1 >= vi.1;
        let c1 = p.1 < vj.1;
        let c2 = ex * wy > ey * wx;
        if (c0 && c1 && c2) || (!c0 && !c1 && !c2) {
            s = -s;
        }
    }
    s * d.sqrt()
}

#[inline]
fn sd_ellipse(p: (f32, f32), center: (f32, f32), rx: f32, ry: f32) -> f32 {
    let rx = rx.max(1e-3);
    let ry = ry.max(1e-3);
    let dx = (p.0 - center.0).abs();
    let dy = (p.1 - center.1).abs();
    let len = (dx / rx).hypot(dy / ry);
    if len < 1e-4 {
        return -rx.min(ry);
    }
    (len - 1.0) * (rx * ry) / ((dy * rx).hypot(dx * ry).max(1e-4))
}

#[inline]
fn sd_box(p: (f32, f32), center: (f32, f32), half_w: f32, half_h: f32) -> f32 {
    let dx = (p.0 - center.0).abs() - half_w;
    let dy = (p.1 - center.1).abs() - half_h;
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0)
}

#[inline]
fn sd_hexagon(p: (f32, f32), center: (f32, f32), r: f32) -> f32 {
    // Pointy-top regular hexagon SDF.
    let mut px = (p.0 - center.0).abs();
    let mut py = (p.1 - center.1).abs();
    let kx = -0.866_025_4_f32;
    let ky = 0.5_f32;
    let kz = 0.577_350_26_f32;
    let dot = 2.0 * (kx * px + ky * py).min(0.0);
    px -= dot * kx;
    py -= dot * ky;
    px -= px.clamp(-kz * r, kz * r);
    py -= r;
    px.hypot(py) * py.signum()
}

fn rasterize_powerline_mask(ch: char, w: u32, h: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (w * h) as usize];
    let wf = w as f32;
    let hf = h as f32;
    let stroke = (wf.min(hf) * 0.12).max(1.0);
    let half_stroke = stroke * 0.5 + 0.5;

    match ch {
        // Exact complementary inverse hard dividers (`E0D6` complements `E0B0`, `E0D7` complements `E0B2`).
        '\u{E0D6}' => {
            let base = rasterize_powerline_mask('\u{E0B0}', w, h);
            for (dst, src) in buf.iter_mut().zip(base.iter()) {
                *dst = 255 - *src;
            }
            return buf;
        }
        '\u{E0D7}' => {
            let base = rasterize_powerline_mask('\u{E0B2}', w, h);
            for (dst, src) in buf.iter_mut().zip(base.iter()) {
                *dst = 255 - *src;
            }
            return buf;
        }
        // Hard triangles (`E0B0`, `E0B2`)
        '\u{E0B0}' | '\u{E0B2}' => {
            let mirror = ch == '\u{E0B2}';
            let verts = [(0.0, 0.0), (wf, hf * 0.5), (0.0, hf)];
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let x = if mirror { wf - px } else { px };
                (0.5 - sd_polygon((x, py), &verts)).clamp(0.0, 1.0)
            });
        }
        // Soft chevrons (`E0B1`, `E0B3`)
        '\u{E0B1}' | '\u{E0B3}' => {
            let mirror = ch == '\u{E0B3}';
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let x = if mirror { wf - px } else { px };
                let d1 = sd_segment((x, py), (0.0, 0.0), (wf, hf * 0.5));
                let d2 = sd_segment((x, py), (wf, hf * 0.5), (0.0, hf));
                (half_stroke - d1.min(d2)).clamp(0.0, 1.0)
            });
        }
        // Half circles (`E0B4`..=`E0B7`)
        '\u{E0B4}' | '\u{E0B5}' | '\u{E0B6}' | '\u{E0B7}' => {
            let left_side = matches!(ch, '\u{E0B6}' | '\u{E0B7}');
            let thin = matches!(ch, '\u{E0B5}' | '\u{E0B7}');
            let center = (if left_side { wf } else { 0.0 }, hf * 0.5);
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let sdf = sd_ellipse((px, py), center, wf, hf * 0.5);
                if thin {
                    (half_stroke - sdf.abs()).clamp(0.0, 1.0)
                } else {
                    (0.5 - sdf).clamp(0.0, 1.0)
                }
            });
        }
        // Corner triangles & diagonal slashes (`E0B8`..=`E0BF`)
        '\u{E0B8}' | '\u{E0BA}' | '\u{E0BC}' | '\u{E0BE}' => {
            let verts = match ch {
                '\u{E0B8}' => [(0.0, 0.0), (wf, hf), (0.0, hf)],
                '\u{E0BA}' => [(wf, 0.0), (wf, hf), (0.0, hf)],
                '\u{E0BC}' => [(0.0, 0.0), (wf, 0.0), (0.0, hf)],
                _ => [(0.0, 0.0), (wf, 0.0), (wf, hf)],
            };
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                (0.5 - sd_polygon((px, py), &verts)).clamp(0.0, 1.0)
            });
        }
        '\u{E0B9}' | '\u{E0BF}' => {
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let d = sd_segment((px, py), (0.0, 0.0), (wf, hf));
                (half_stroke - d).clamp(0.0, 1.0)
            });
        }
        '\u{E0BB}' | '\u{E0BD}' => {
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let d = sd_segment((px, py), (wf, 0.0), (0.0, hf));
                (half_stroke - d).clamp(0.0, 1.0)
            });
        }
        // Flames (`E0C0`..=`E0C3`)
        '\u{E0C0}' | '\u{E0C1}' | '\u{E0C2}' | '\u{E0C3}' => {
            let mirror = matches!(ch, '\u{E0C2}' | '\u{E0C3}');
            let thin = matches!(ch, '\u{E0C1}' | '\u{E0C3}');
            let verts = [
                (0.0, 0.0),
                (wf * 0.58, hf * 0.06),
                (wf * 0.88, hf * 0.17),
                (wf * 0.44, hf * 0.29),
                (wf * 0.98, hf * 0.45),
                (wf * 0.56, hf * 0.58),
                (wf * 0.90, hf * 0.74),
                (wf * 0.48, hf * 0.86),
                (wf * 0.66, hf * 0.96),
                (0.0, hf),
            ];
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let x = if mirror { wf - px } else { px };
                let sdf = sd_polygon((x, py), &verts);
                if thin {
                    (half_stroke - sdf.abs()).clamp(0.0, 1.0)
                } else {
                    (0.5 - sdf).clamp(0.0, 1.0)
                }
            });
        }
        // Pixelated squares (`E0C4`..=`E0C7`)
        '\u{E0C4}' | '\u{E0C5}' => {
            let mirror = ch == '\u{E0C5}';
            for y in 0..h {
                let row = ((y * 8) / h.max(1)).min(7);
                let out_row = (y * w) as usize;
                for x in 0..w {
                    let col_raw = ((x * 4) / w.max(1)).min(3);
                    let col = if mirror { 3 - col_raw } else { col_raw };
                    let on = match col {
                        0 => true,
                        1 => row.is_multiple_of(2) || row.is_multiple_of(3),
                        2 => (row + 1).is_multiple_of(2),
                        _ => row % 4 == 1,
                    };
                    if on {
                        buf[out_row + x as usize] = 255;
                    }
                }
            }
        }
        '\u{E0C6}' | '\u{E0C7}' => {
            let mirror = ch == '\u{E0C7}';
            for y in 0..h {
                let row = ((y * 4) / h.max(1)).min(3);
                let out_row = (y * w) as usize;
                for x in 0..w {
                    let col_raw = ((x * 2) / w.max(1)).min(1);
                    let col = if mirror { 1 - col_raw } else { col_raw };
                    let on = col == 0 || row.is_multiple_of(2);
                    if on {
                        buf[out_row + x as usize] = 255;
                    }
                }
            }
        }
        // Ice waveform (`E0C8`, `E0CA`)
        '\u{E0C8}' | '\u{E0CA}' => {
            let mirror = ch == '\u{E0CA}';
            let verts = [
                (0.0, 0.0),
                (wf * 0.85, hf * 0.14),
                (wf * 0.45, hf * 0.28),
                (wf, hf * 0.46),
                (wf * 0.50, hf * 0.64),
                (wf * 0.90, hf * 0.82),
                (0.0, hf),
            ];
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let x = if mirror { wf - px } else { px };
                (0.5 - sd_polygon((x, py), &verts)).clamp(0.0, 1.0)
            });
        }
        // Honeycomb (`E0CC` filled, `E0CD` outline)
        '\u{E0CC}' | '\u{E0CD}' => {
            let outline = ch == '\u{E0CD}';
            let r = (wf * 0.34).min(hf * 0.22).max(1.5);
            let c1 = (wf * 0.32, hf * 0.25);
            let c2 = (wf * 0.68, hf * 0.50);
            let c3 = (wf * 0.32, hf * 0.75);
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let d = sd_hexagon((px, py), c1, r)
                    .min(sd_hexagon((px, py), c2, r))
                    .min(sd_hexagon((px, py), c3, r));
                if outline {
                    (half_stroke - d.abs()).clamp(0.0, 1.0)
                } else {
                    (0.5 - d).clamp(0.0, 1.0)
                }
            });
        }
        // Lego separator & bricks (`E0CE`..=`E0D1`)
        '\u{E0CE}' | '\u{E0CF}' => {
            let outline = ch == '\u{E0CF}';
            let verts = [
                (0.0, 0.0),
                (wf * 0.62, 0.0),
                (wf * 0.62, hf * 0.16),
                (wf, hf * 0.16),
                (wf, hf * 0.38),
                (wf * 0.62, hf * 0.38),
                (wf * 0.62, hf * 0.62),
                (wf, hf * 0.62),
                (wf, hf * 0.84),
                (wf * 0.62, hf * 0.84),
                (wf * 0.62, hf),
                (0.0, hf),
            ];
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let d = sd_polygon((px, py), &verts);
                if outline {
                    (half_stroke - d.abs()).clamp(0.0, 1.0)
                } else {
                    (0.5 - d).clamp(0.0, 1.0)
                }
            });
        }
        '\u{E0D0}' => {
            // Facing Lego block with 2 circular studs
            let r = (wf.min(hf) * 0.18).max(1.2);
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let outer = sd_box((px, py), (wf * 0.5, hf * 0.5), wf * 0.44, hf * 0.42);
                let border = (half_stroke - outer.abs()).clamp(0.0, 1.0);
                let s1 = ((px - wf * 0.5).hypot(py - hf * 0.30) - r).abs();
                let s2 = ((px - wf * 0.5).hypot(py - hf * 0.70) - r).abs();
                let studs = (half_stroke - s1.min(s2)).clamp(0.0, 1.0);
                border.max(studs)
            });
        }
        '\u{E0D1}' => {
            // Sideways Lego block with left base and 2 projecting studs
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let body = sd_box((px, py), (wf * 0.32, hf * 0.5), wf * 0.26, hf * 0.42);
                let stud1 = sd_box((px, py), (wf * 0.74, hf * 0.28), wf * 0.18, hf * 0.12);
                let stud2 = sd_box((px, py), (wf * 0.74, hf * 0.72), wf * 0.18, hf * 0.12);
                (0.5 - body.min(stud1).min(stud2)).clamp(0.0, 1.0)
            });
        }
        // Trapezoid arrows (`E0D2`, `E0D4`)
        '\u{E0D2}' | '\u{E0D4}' => {
            let mirror = ch == '\u{E0D4}';
            let verts = [
                (0.0, 0.0),
                (wf * 0.62, 0.0),
                (wf, hf * 0.5),
                (wf * 0.62, hf),
                (0.0, hf),
            ];
            rasterize_sdf_mask(&mut buf, w, h, |px, py| {
                let x = if mirror { wf - px } else { px };
                (0.5 - sd_polygon((x, py), &verts)).clamp(0.0, 1.0)
            });
        }
        _ => {}
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procedural_powerline_and_blocks_flush_to_cell_edges_and_complements() {
        let valid_powerline: &[char] = &[
            '\u{E0B0}', '\u{E0B1}', '\u{E0B2}', '\u{E0B3}', '\u{E0B4}', '\u{E0B5}', '\u{E0B6}',
            '\u{E0B7}', '\u{E0B8}', '\u{E0B9}', '\u{E0BA}', '\u{E0BB}', '\u{E0BC}', '\u{E0BD}',
            '\u{E0BE}', '\u{E0BF}', '\u{E0C0}', '\u{E0C1}', '\u{E0C2}', '\u{E0C3}', '\u{E0C4}',
            '\u{E0C5}', '\u{E0C6}', '\u{E0C7}', '\u{E0C8}', '\u{E0CA}', '\u{E0CC}', '\u{E0CD}',
            '\u{E0CE}', '\u{E0CF}', '\u{E0D0}', '\u{E0D1}', '\u{E0D2}', '\u{E0D4}', '\u{E0D6}',
            '\u{E0D7}',
        ];
        assert_eq!(valid_powerline.len(), 36);
        for &ch in valid_powerline {
            assert!(
                is_procedural_box_or_block(ch),
                "expected Powerline U+{:04X} to be procedural",
                ch as u32
            );
        }
        for &unassigned in &['\u{E0C9}', '\u{E0CB}', '\u{E0D3}', '\u{E0D5}'] {
            assert!(
                !is_procedural_box_or_block(unassigned),
                "unassigned U+{:04X} must NOT be intercepted",
                unassigned as u32
            );
        }

        for &(w, h) in &[(8u32, 18u32), (15u32, 32u32)] {
            for &ch in valid_powerline {
                let mask = rasterize_box_or_block_mask(ch, w, h)
                    .unwrap_or_else(|| panic!("missing mask for U+{:04X}", ch as u32));
                assert_eq!(mask.len(), (w * h) as usize);
                assert!(
                    mask.iter().any(|&v| v > 0),
                    "blank mask for U+{:04X} at {w}x{h}",
                    ch as u32
                );
            }

            // Verify flush left edge on E0B0 and right edge on E0B2 at middle row.
            let m_e0b0 = rasterize_box_or_block_mask('\u{E0B0}', w, h).unwrap();
            let m_e0b2 = rasterize_box_or_block_mask('\u{E0B2}', w, h).unwrap();
            let mid_y = (h / 2) as usize;
            assert_eq!(m_e0b0[mid_y * w as usize], 255);
            assert_eq!(m_e0b2[mid_y * w as usize + (w as usize - 1)], 255);

            // Verify exact pixel complements for E0B0+E0D6 and E0B2+E0D7.
            let m_e0d6 = rasterize_box_or_block_mask('\u{E0D6}', w, h).unwrap();
            let m_e0d7 = rasterize_box_or_block_mask('\u{E0D7}', w, h).unwrap();
            for i in 0..(w * h) as usize {
                assert_eq!(u16::from(m_e0b0[i]) + u16::from(m_e0d6[i]), 255);
                assert_eq!(u16::from(m_e0b2[i]) + u16::from(m_e0d7[i]), 255);
            }

            // Extra blocks & corner triangles
            for ch in ('\u{2594}'..='\u{259F}').chain('\u{25E2}'..='\u{25E5}') {
                assert!(is_procedural_box_or_block(ch));
                let mask = rasterize_box_or_block_mask(ch, w, h).unwrap();
                assert!(mask.contains(&255));
            }
        }
    }
}
