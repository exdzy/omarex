//! Unified `wgpu` instanced GPU renderer for `omarex` (Vulkan 1.4 primary, GLES fallback,
//! headless offscreen PNG target).
//!
//! Renders Rex-faithful UI chrome (SDF rounded rects, borders, soft Gaussian shadows, crisp
//! `resvg` SVG icons, `cosmic-text` typography), `RowId`-cached terminal grids with smooth
//! overscan scrolling, and z-ordered Kitty Graphics Protocol image layers in a single GPU pass.

pub mod atlas;
pub mod context;
pub mod draw_list;
pub mod image_pass;
pub mod terminal_pass;

use std::mem::size_of;
use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};

pub use atlas::{CellMetrics, GlyphAtlas};
pub use context::{
    AcquiredFrame, AdapterSummary, GpuConfig, GpuContext, OffscreenTarget, PresentModePreference,
    RenderError, save_rgba_png,
};
pub use draw_list::{
    ClipRect, Color, CornerRadii, DrawList, FontFamily, FontStyle, FontWeight, GLYPH_MODE_COLOR,
    GLYPH_MODE_MONO, GLYPH_MODE_TINTED_COLOR, GlyphInstance, IconDraw, ImageInstance, ImageQuad,
    LayerContent, QuadInstance, Rect, SceneFrame, TextAlign, TextRun, layers,
};
pub use image_pass::{
    ImageLayer, ImagePixelFormat, ImageTextureCache, ImageUpload, KittyPlacement,
};
pub use terminal_pass::{
    CellFlags, CursorShape, RowId, StyledCell, TerminalBuildStats, TerminalCursor,
    TerminalGridSnapshot, TerminalPaneCache, TerminalRowSource, TerminalViewport,
};

#[repr(C)]
#[derive(Debug, Copy, Clone, Pod, Zeroable)]
struct GlobalsUniform {
    viewport_size: [f32; 2],
    scale_factor: f32,
    /// bit 0: target_is_srgb, bit 1: is_light_mode
    flags: u32,
}

/// Performance and workload telemetry returned after rendering a frame.
#[derive(Debug, Clone)]
pub struct FrameStats {
    pub cpu_prepare_us: u64,
    pub gpu_encode_submit_us: u64,
    pub total_us: u64,
    pub draw_calls: u32,
    pub quad_count: u32,
    pub glyph_count: u32,
    pub image_count: u32,
    pub adapter_name: String,
    pub backend: wgpu::Backend,
}

enum DrawCommand {
    Quads(Range<u32>),
    Glyphs(Range<u32>),
    Images { image_id: u64, range: Range<u32> },
}

/// Auto-growing vertex/instance GPU buffer to avoid per-frame reallocations.
struct DynamicInstanceBuffer<T: Pod> {
    label: &'static str,
    buffer: wgpu::Buffer,
    capacity: usize,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Pod> DynamicInstanceBuffer<T> {
    fn new(device: &wgpu::Device, label: &'static str, initial_capacity: usize) -> Self {
        let cap = initial_capacity.max(16);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: (cap * size_of::<T>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            label,
            buffer,
            capacity: cap,
            _marker: std::marker::PhantomData,
        }
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, items: &[T]) {
        if items.is_empty() {
            return;
        }
        if items.len() > self.capacity {
            let new_cap = items.len().next_power_of_two().max(self.capacity * 2);
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: (new_cap * size_of::<T>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
        }
        queue.write_buffer(&self.buffer, 0, bytemuck::cast_slice(items));
    }

    fn slice(&self, count: usize) -> wgpu::BufferSlice<'_> {
        let byte_len = (count * size_of::<T>()) as u64;
        self.buffer.slice(0..byte_len)
    }
}

struct PipelineSet {
    target_format: wgpu::TextureFormat,
    quad_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    image_pipeline: wgpu::RenderPipeline,
}

impl PipelineSet {
    fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        globals_bgl: &wgpu::BindGroupLayout,
        atlas_bgl: &wgpu::BindGroupLayout,
        image_bgl: &wgpu::BindGroupLayout,
    ) -> Self {
        let blend = Some(wgpu::BlendState::ALPHA_BLENDING);
        let color_targets = [Some(wgpu::ColorTargetState {
            format: target_format,
            blend,
            write_mask: wgpu::ColorWrites::ALL,
        })];

        // 1. Quad SDF pipeline
        let quad_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("omarex-quad-wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/quad.wgsl").into()),
        });
        let quad_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("omarex-quad-pipeline-layout"),
            bind_group_layouts: &[Some(globals_bgl)],
            immediate_size: 0,
        });
        let quad_attrs = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 16,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 32,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 48,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 64,
                shader_location: 4,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 80,
                shader_location: 5,
            },
        ];
        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("omarex-quad-pipeline"),
            layout: Some(&quad_layout),
            vertex: wgpu::VertexState {
                module: &quad_shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<QuadInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &quad_attrs,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &quad_shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &color_targets,
            }),
            multiview_mask: None,
            cache: None,
        });

        // 2. Glyph & icon pipeline
        let glyph_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("omarex-glyph-wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/glyph.wgsl").into()),
        });
        let glyph_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("omarex-glyph-pipeline-layout"),
            bind_group_layouts: &[Some(globals_bgl), Some(atlas_bgl)],
            immediate_size: 0,
        });
        let glyph_attrs = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 16,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 32,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 48,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 64,
                shader_location: 4,
            },
        ];
        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("omarex-glyph-pipeline"),
            layout: Some(&glyph_layout),
            vertex: wgpu::VertexState {
                module: &glyph_shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<GlyphInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &glyph_attrs,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &glyph_shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &color_targets,
            }),
            multiview_mask: None,
            cache: None,
        });

        // 3. Textured image pipeline
        let image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("omarex-image-wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/image.wgsl").into()),
        });
        let image_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("omarex-image-pipeline-layout"),
            bind_group_layouts: &[Some(globals_bgl), Some(image_bgl)],
            immediate_size: 0,
        });
        let image_attrs = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 16,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 32,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 48,
                shader_location: 3,
            },
        ];
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("omarex-image-pipeline"),
            layout: Some(&image_layout),
            vertex: wgpu::VertexState {
                module: &image_shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<ImageInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &image_attrs,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &image_shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &color_targets,
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            target_format,
            quad_pipeline,
            glyph_pipeline,
            image_pipeline,
        }
    }
}

/// Top-level unified GPU renderer supporting both windowed surfaces and headless PNG readback.
pub struct GpuRenderer {
    ctx: GpuContext,
    globals_bgl: wgpu::BindGroupLayout,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    pipelines: PipelineSet,
    offscreen_pipelines: Option<PipelineSet>,

    quad_buffer: DynamicInstanceBuffer<QuadInstance>,
    glyph_buffer: DynamicInstanceBuffer<GlyphInstance>,
    image_buffer: DynamicInstanceBuffer<ImageInstance>,

    atlas: GlyphAtlas,
    images: ImageTextureCache,

    // Reusable per-frame staging vectors (zero steady-state heap growth).
    staged_quads: Vec<QuadInstance>,
    staged_glyphs: Vec<GlyphInstance>,
    staged_images: Vec<ImageInstance>,
    staged_commands: Vec<DrawCommand>,
}

impl GpuRenderer {
    /// Creates a headless GPU renderer (no window / display server required).
    pub fn new_headless(config: GpuConfig) -> Result<Self, RenderError> {
        let ctx = GpuContext::new_headless(&config)?;
        Ok(Self::from_context(ctx, &config))
    }

    /// Creates a GPU renderer bound to a native window surface (e.g. `Arc<winit::window::Window>`).
    pub fn new_with_surface(
        surface_target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        scale_factor: f32,
        config: GpuConfig,
    ) -> Result<Self, RenderError> {
        let ctx =
            GpuContext::new_with_surface(surface_target, width, height, scale_factor, &config)?;
        Ok(Self::from_context(ctx, &config))
    }

    fn from_context(ctx: GpuContext, config: &GpuConfig) -> Self {
        let globals_bgl = ctx
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("omarex-globals-bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let globals_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("omarex-globals-ubo"),
            size: size_of::<GlobalsUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("omarex-globals-bg"),
            layout: &globals_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        let atlas = GlyphAtlas::new(&ctx.device);
        let images = ImageTextureCache::new(&ctx.device, config.image_cache_budget_bytes);

        let pipelines = PipelineSet::new(
            &ctx.device,
            ctx.target_format,
            &globals_bgl,
            atlas.bind_group_layout(),
            images.bind_group_layout(),
        );

        let quad_buffer = DynamicInstanceBuffer::new(&ctx.device, "omarex-quad-instances", 2048);
        let glyph_buffer = DynamicInstanceBuffer::new(&ctx.device, "omarex-glyph-instances", 8192);
        let image_buffer = DynamicInstanceBuffer::new(&ctx.device, "omarex-image-instances", 64);

        Self {
            ctx,
            globals_bgl,
            globals_buffer,
            globals_bind_group,
            pipelines,
            offscreen_pipelines: None,
            quad_buffer,
            glyph_buffer,
            image_buffer,
            atlas,
            images,
            staged_quads: Vec::with_capacity(2048),
            staged_glyphs: Vec::with_capacity(8192),
            staged_images: Vec::with_capacity(64),
            staged_commands: Vec::with_capacity(64),
        }
    }

    pub fn context(&self) -> &GpuContext {
        &self.ctx
    }

    pub fn adapter_summary(&self) -> &AdapterSummary {
        &self.ctx.adapter_summary
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.ctx.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.ctx.queue
    }

    pub fn atlas(&self) -> &GlyphAtlas {
        &self.atlas
    }

    pub fn atlas_mut(&mut self) -> &mut GlyphAtlas {
        &mut self.atlas
    }

    pub fn images(&self) -> &ImageTextureCache {
        &self.images
    }

    pub fn images_mut(&mut self) -> &mut ImageTextureCache {
        &mut self.images
    }

    /// Uploads or updates a Kitty / hover preview image in the bounded VRAM texture cache.
    pub fn upload_image(&mut self, upload: ImageUpload<'_>) -> Result<bool, RenderError> {
        self.images
            .upload_or_update(&self.ctx.device, &self.ctx.queue, upload)
    }

    /// Resizes the windowed swapchain and updates DPI scale factor.
    pub fn resize(&mut self, width: u32, height: u32, scale_factor: f32) {
        self.ctx.resize(width, height, scale_factor);
    }

    /// Switches swapchain presentation mode (`Fifo` vs `Mailbox`).
    pub fn set_present_mode(&mut self, mode: PresentModePreference) {
        self.ctx.set_present_mode(mode);
    }

    /// Creates a reusable offscreen render target for headless screenshots or visual tests.
    pub fn create_offscreen_target(
        &self,
        width: u32,
        height: u32,
    ) -> Result<OffscreenTarget, RenderError> {
        OffscreenTarget::new(&self.ctx.device, width, height)
    }

    /// Renders `frame` to the active windowed surface and presents it.
    pub fn render_to_surface(
        &mut self,
        frame: &SceneFrame,
    ) -> Result<Option<FrameStats>, RenderError> {
        if frame.physical_width > 0 && frame.physical_height > 0 {
            let needs_resize = self.ctx.surface_size()
                != Some((frame.physical_width, frame.physical_height))
                || (self.ctx.scale_factor() - frame.scale_factor.max(0.5)).abs() > f32::EPSILON;
            if needs_resize {
                self.ctx.resize(
                    frame.physical_width,
                    frame.physical_height,
                    frame.scale_factor,
                );
            }
        }

        let (surface_tex, suboptimal) = match self.ctx.acquire_surface_frame()? {
            AcquiredFrame::Ready {
                texture,
                suboptimal,
            } => (texture, suboptimal),
            AcquiredFrame::Reconfigured => match self.ctx.acquire_surface_frame()? {
                AcquiredFrame::Ready {
                    texture,
                    suboptimal,
                } => (texture, suboptimal),
                AcquiredFrame::Skipped | AcquiredFrame::Reconfigured => return Ok(None),
            },
            AcquiredFrame::Skipped => return Ok(None),
        };

        // Guard against stale swapchain images whose dimensions lag behind a Hyprland resize.
        if frame.physical_width > 0
            && frame.physical_height > 0
            && (surface_tex.texture.width() != frame.physical_width
                || surface_tex.texture.height() != frame.physical_height)
        {
            drop(surface_tex);
            self.ctx.resize(
                frame.physical_width,
                frame.physical_height,
                frame.scale_factor,
            );
            return Ok(None);
        }

        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let format = self.ctx.target_format;
        let stats = self.render_into_view(frame, &view, format, None)?;
        self.ctx.queue.present(surface_tex);

        if suboptimal {
            self.ctx.reconfigure_surface();
            return Ok(None);
        }
        Ok(Some(stats))
    }

    /// Renders `frame` into an arbitrary caller-provided `wgpu::TextureView` (no staging readback).
    pub fn render_to_view(
        &mut self,
        frame: &SceneFrame,
        target_view: &wgpu::TextureView,
        target_format: wgpu::TextureFormat,
    ) -> Result<FrameStats, RenderError> {
        self.render_into_view(frame, target_view, target_format, None)
    }

    /// Renders `frame` into a caller-managed [`OffscreenTarget`] and copies the result into its
    /// staging buffer for readback.
    pub fn render_to_offscreen(
        &mut self,
        frame: &SceneFrame,
        target: &OffscreenTarget,
    ) -> Result<FrameStats, RenderError> {
        self.render_into_view(frame, &target.view, target.format, Some(target))
    }

    /// Renders `frame` offscreen and reads back tightly packed sRGB RGBA8 pixels.
    pub fn render_to_rgba(
        &mut self,
        frame: &SceneFrame,
    ) -> Result<(Vec<u8>, FrameStats), RenderError> {
        let target = OffscreenTarget::new(
            &self.ctx.device,
            frame.physical_width,
            frame.physical_height,
        )?;
        let stats = self.render_to_offscreen(frame, &target)?;
        let rgba = self.ctx.readback_offscreen(&target)?;
        Ok((rgba, stats))
    }

    /// Renders `frame` offscreen and writes the resulting PNG image to `path`.
    pub fn render_to_png(
        &mut self,
        frame: &SceneFrame,
        path: impl AsRef<Path>,
    ) -> Result<FrameStats, RenderError> {
        let (rgba, stats) = self.render_to_rgba(frame)?;
        save_rgba_png(path, frame.physical_width, frame.physical_height, &rgba)?;
        Ok(stats)
    }

    fn render_into_view(
        &mut self,
        frame: &SceneFrame,
        target_view: &wgpu::TextureView,
        target_format: wgpu::TextureFormat,
        offscreen_copy: Option<&OffscreenTarget>,
    ) -> Result<FrameStats, RenderError> {
        if frame.physical_width == 0 || frame.physical_height == 0 {
            return Err(RenderError::InvalidDimensions {
                width: frame.physical_width,
                height: frame.physical_height,
            });
        }

        let t_start = Instant::now();
        self.images.begin_frame();

        // Ensure pipeline matches target format (e.g. surface Bgra8UnormSrgb vs offscreen Rgba8UnormSrgb).
        let use_offscreen_pipelines = if self.pipelines.target_format == target_format {
            false
        } else {
            let needs_create = self
                .offscreen_pipelines
                .as_ref()
                .is_none_or(|p| p.target_format != target_format);
            if needs_create {
                self.offscreen_pipelines = Some(PipelineSet::new(
                    &self.ctx.device,
                    target_format,
                    &self.globals_bgl,
                    self.atlas.bind_group_layout(),
                    self.images.bind_group_layout(),
                ));
            }
            true
        };

        // Flatten all Z-layers into contiguous GPU instance buffers + compact draw commands.
        self.staged_quads.clear();
        self.staged_glyphs.clear();
        self.staged_images.clear();
        self.staged_commands.clear();

        let scale = frame.scale_factor.max(0.5);
        for layer in frame.layers.values() {
            // 1. Quads (backgrounds, rounded cards, borders, shadows, underlines, cursors)
            if !layer.quads.is_empty() {
                let start = self.staged_quads.len() as u32;
                self.staged_quads.extend_from_slice(&layer.quads);
                let end = self.staged_quads.len() as u32;
                self.staged_commands.push(DrawCommand::Quads(start..end));
            }

            // 2. Textured images (Kitty layers / previews), batched by consecutive image_id
            if !layer.images.is_empty() {
                let mut current_batch: Option<(u64, u32)> = None;
                for img in &layer.images {
                    if self.images.touch_and_get(img.image_id).is_none() {
                        continue;
                    }
                    let inst = ImageInstance {
                        bounds: img.bounds.scale(scale).to_array(),
                        uv_bounds: img.uv_bounds,
                        clip_rect: img.clip_rect.scale(scale).to_array(),
                        params: [
                            img.opacity.clamp(0.0, 1.0),
                            (img.corner_radius * scale).max(0.0),
                            0.0,
                            0.0,
                        ],
                    };
                    match current_batch {
                        Some((id, _)) if id == img.image_id => {
                            self.staged_images.push(inst);
                        }
                        Some((prev_id, start)) => {
                            let end = self.staged_images.len() as u32;
                            self.staged_commands.push(DrawCommand::Images {
                                image_id: prev_id,
                                range: start..end,
                            });
                            let next_start = self.staged_images.len() as u32;
                            self.staged_images.push(inst);
                            current_batch = Some((img.image_id, next_start));
                        }
                        None => {
                            let next_start = self.staged_images.len() as u32;
                            self.staged_images.push(inst);
                            current_batch = Some((img.image_id, next_start));
                        }
                    }
                }
                if let Some((prev_id, start)) = current_batch {
                    let end = self.staged_images.len() as u32;
                    if end > start {
                        self.staged_commands.push(DrawCommand::Images {
                            image_id: prev_id,
                            range: start..end,
                        });
                    }
                }
            }

            // 3. Glyphs (pre-shaped terminal glyphs + UI text runs + cached SVG icons)
            let glyph_start = self.staged_glyphs.len() as u32;
            if !layer.raw_glyphs.is_empty() {
                self.staged_glyphs.extend_from_slice(&layer.raw_glyphs);
            }
            for text_run in &layer.text_runs {
                self.atlas.shape_text_run(
                    text_run,
                    scale,
                    &self.ctx.queue,
                    &mut self.staged_glyphs,
                );
            }
            for icon in &layer.icons {
                self.atlas
                    .emit_icon(icon, scale, &self.ctx.queue, &mut self.staged_glyphs);
            }
            let glyph_end = self.staged_glyphs.len() as u32;
            if glyph_end > glyph_start {
                self.staged_commands
                    .push(DrawCommand::Glyphs(glyph_start..glyph_end));
            }
        }

        let t_prepared = Instant::now();

        // Update uniform buffer and upload contiguous instance buffers.
        let target_is_srgb = target_format.is_srgb();
        let mut flags = 0u32;
        if target_is_srgb {
            flags |= 1;
        }
        if frame.is_light_mode {
            flags |= 2;
        }
        let globals = GlobalsUniform {
            viewport_size: [frame.physical_width as f32, frame.physical_height as f32],
            scale_factor: scale,
            flags,
        };
        self.ctx
            .queue
            .write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));

        self.quad_buffer
            .upload(&self.ctx.device, &self.ctx.queue, &self.staged_quads);
        self.glyph_buffer
            .upload(&self.ctx.device, &self.ctx.queue, &self.staged_glyphs);
        self.image_buffer
            .upload(&self.ctx.device, &self.ctx.queue, &self.staged_images);

        let pipelines = if use_offscreen_pipelines {
            self.offscreen_pipelines.as_ref().unwrap_or(&self.pipelines)
        } else {
            &self.pipelines
        };

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("omarex-frame-encoder"),
            });

        let mut draw_calls = 0u32;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("omarex-unified-render-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(frame.clear_color.to_wgpu_clear(target_is_srgb)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            for cmd in &self.staged_commands {
                match cmd {
                    DrawCommand::Quads(range) => {
                        pass.set_pipeline(&pipelines.quad_pipeline);
                        pass.set_bind_group(0, &self.globals_bind_group, &[]);
                        pass.set_vertex_buffer(0, self.quad_buffer.slice(self.staged_quads.len()));
                        pass.draw(0..6, range.clone());
                        draw_calls += 1;
                    }
                    DrawCommand::Glyphs(range) => {
                        pass.set_pipeline(&pipelines.glyph_pipeline);
                        pass.set_bind_group(0, &self.globals_bind_group, &[]);
                        pass.set_bind_group(1, self.atlas.bind_group(), &[]);
                        pass.set_vertex_buffer(
                            0,
                            self.glyph_buffer.slice(self.staged_glyphs.len()),
                        );
                        pass.draw(0..6, range.clone());
                        draw_calls += 1;
                    }
                    DrawCommand::Images { image_id, range } => {
                        if let Some(cached) = self.images.get(*image_id) {
                            pass.set_pipeline(&pipelines.image_pipeline);
                            pass.set_bind_group(0, &self.globals_bind_group, &[]);
                            pass.set_bind_group(1, &cached.bind_group, &[]);
                            pass.set_vertex_buffer(
                                0,
                                self.image_buffer.slice(self.staged_images.len()),
                            );
                            pass.draw(0..6, range.clone());
                            draw_calls += 1;
                        }
                    }
                }
            }
        }

        if let Some(offscreen) = offscreen_copy {
            offscreen.encode_copy_to_staging(&mut encoder);
        }

        self.ctx.queue.submit(Some(encoder.finish()));
        let t_end = Instant::now();

        Ok(FrameStats {
            cpu_prepare_us: t_prepared.duration_since(t_start).as_micros() as u64,
            gpu_encode_submit_us: t_end.duration_since(t_prepared).as_micros() as u64,
            total_us: t_end.duration_since(t_start).as_micros() as u64,
            draw_calls,
            quad_count: self.staged_quads.len() as u32,
            glyph_count: self.staged_glyphs.len() as u32,
            image_count: self.staged_images.len() as u32,
            adapter_name: self.ctx.adapter_summary.name.clone(),
            backend: self.ctx.adapter_summary.backend,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_instance_sizes_and_alignment() {
        assert_eq!(size_of::<QuadInstance>(), 96);
        assert_eq!(size_of::<GlyphInstance>(), 80);
        assert_eq!(size_of::<ImageInstance>(), 64);
        assert_eq!(size_of::<GlobalsUniform>(), 16);
    }

    #[test]
    fn clip_rect_intersection_and_draw_list_stack() {
        let mut dl = DrawList::new(1280, 720, 2.0);
        assert_eq!(dl.logical_size(), (640.0, 360.0));
        dl.push_clip(Rect::new(10.0, 10.0, 100.0, 100.0));
        dl.push_clip(Rect::new(50.0, 60.0, 200.0, 200.0));
        assert_eq!(dl.current_clip(), ClipRect::new(50.0, 60.0, 110.0, 110.0));
        dl.pop_clip();
        assert_eq!(dl.current_clip(), ClipRect::new(10.0, 10.0, 110.0, 110.0));
        dl.pop_clip();
        assert_eq!(dl.current_clip(), ClipRect::new(0.0, 0.0, 640.0, 360.0));
        // Popping beyond root preserves viewport clip.
        dl.pop_clip();
        assert_eq!(dl.current_clip(), ClipRect::new(0.0, 0.0, 640.0, 360.0));
    }

    #[test]
    fn headless_vulkan_init_and_image_cache_lru() {
        let mut renderer = GpuRenderer::new_headless(GpuConfig {
            image_cache_budget_bytes: 1024 * 1024, // 1 MiB budget
            ..GpuConfig::default()
        })
        .expect("headless wgpu renderer should initialize");

        assert!(
            !renderer.adapter_summary().name.is_empty(),
            "expected non-empty adapter name"
        );

        // 512x512 RGBA8 = 1 MiB exact; uploading two should evict the older LRU image.
        let img_a = vec![0xAAu8; 512 * 512 * 4];
        let img_b = vec![0xBBu8; 512 * 512 * 4];

        renderer.images_mut().begin_frame();
        assert!(
            renderer
                .upload_image(ImageUpload {
                    id: 1,
                    generation: 1,
                    width: 512,
                    height: 512,
                    format: ImagePixelFormat::Rgba8,
                    data: &img_a,
                })
                .expect("upload A")
        );
        // Same (id, generation) must skip upload!
        assert!(
            !renderer
                .upload_image(ImageUpload {
                    id: 1,
                    generation: 1,
                    width: 512,
                    height: 512,
                    format: ImagePixelFormat::Rgba8,
                    data: &img_a,
                })
                .expect("generation cache hit")
        );

        renderer.images_mut().begin_frame();
        assert!(
            renderer
                .upload_image(ImageUpload {
                    id: 2,
                    generation: 1,
                    width: 512,
                    height: 512,
                    format: ImagePixelFormat::Rgba8,
                    data: &img_b,
                })
                .expect("upload B")
        );

        assert_eq!(renderer.images().len(), 1);
        assert!(renderer.images().get(1).is_none());
        assert!(renderer.images().get(2).is_some());
        assert_eq!(renderer.images().eviction_count(), 1);
        assert!(renderer.images().total_vram_bytes() <= renderer.images().max_vram_bytes());
    }

    #[test]
    fn terminal_row_id_caching_and_incremental_dirty_rebuild() {
        let mut renderer =
            GpuRenderer::new_headless(GpuConfig::default()).expect("headless renderer");
        let metrics = renderer.atlas_mut().measure_monospace_cell(14.0, 1.0);
        let viewport = TerminalViewport {
            rect: Rect::new(20.0, 20.0, 600.0, 300.0),
            clip_rect: Rect::new(20.0, 20.0, 600.0, 300.0).to_clip_rect(),
            cols: 70,
            rows: 15,
            metrics,
            scroll_offset_y: -4.5, // fractional overscan smooth scroll!
            default_bg: Color::from_hex(0x121212),
            default_fg: Color::from_hex(0xffffff),
            selection_bg: Color::from_hex(0x2a2a2a),
            selection_fg: None,
            hyperlink_color: Color::from_hex(0x4e8df5),
            dim_alpha: 1.0,
        };

        let mut grid = TerminalGridSnapshot::new();
        grid.push_styled_row(
            1001,
            0,
            true,
            [(
                "fn main() { println!(\"hello omarex\"); }",
                Color::WHITE,
                None,
                CellFlags::BOLD,
            )],
        );
        grid.push_styled_row(
            1002,
            1,
            true,
            [(
                "┌──────────────────────────────┐",
                Color::from_hex(0xe28203),
                None,
                CellFlags::NONE,
            )],
        );
        grid.push_styled_row(
            1003,
            2,
            true,
            [(
                "error: curly underline test",
                Color::from_hex(0xef3827),
                Some(Color::from_hex(0x261414)),
                CellFlags::CURLY_UNDERLINE | CellFlags::ITALIC,
            )],
        );
        grid.set_cursor(Some(TerminalCursor {
            col: 5,
            row: 0,
            shape: CursorShape::Bar,
            color: Color::from_hex(0xe28203),
            text_color: Color::BLACK,
            visible: true,
        }));

        let mut cache = TerminalPaneCache::new();
        let mut dl1 = DrawList::new(800, 400, 1.0);
        let stats1 = cache.build_pane(
            &grid,
            &viewport,
            &mut renderer.atlas,
            &renderer.ctx.queue,
            &mut dl1,
        );
        assert_eq!(stats1.rebuilt_rows, 3);
        assert_eq!(stats1.cached_rows, 0);
        let uploads_after_first = renderer.atlas().mono_upload_count();
        assert!(uploads_after_first > 0);

        // Second frame: all rows clean -> ZERO rebuilt rows and ZERO new atlas uploads!
        grid.mark_all_clean();
        let mut dl2 = DrawList::new(800, 400, 1.0);
        let stats2 = cache.build_pane(
            &grid,
            &viewport,
            &mut renderer.atlas,
            &renderer.ctx.queue,
            &mut dl2,
        );
        assert_eq!(stats2.rebuilt_rows, 0);
        assert_eq!(stats2.cached_rows, 3);
        assert_eq!(renderer.atlas().mono_upload_count(), uploads_after_first);

        // Third frame: dirty only row 1 -> only 1 row rebuilt, 2 reused from cache!
        grid.mark_row_dirty(1);
        let mut dl3 = DrawList::new(800, 400, 1.0);
        let stats3 = cache.build_pane(
            &grid,
            &viewport,
            &mut renderer.atlas,
            &renderer.ctx.queue,
            &mut dl3,
        );
        assert_eq!(stats3.rebuilt_rows, 1);
        assert_eq!(stats3.cached_rows, 2);
    }

    #[test]
    fn real_vt_engine_render_state_plugs_into_gpu_renderer() {
        let mut renderer =
            GpuRenderer::new_headless(GpuConfig::default()).expect("headless renderer");
        let term = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
            .expect("create libghostty-vt terminal");
        term.write(
            b"\x1b[1;32mhello\x1b[0m \x1b[4:3;31mcurly\x1b[0m \x1b[38;2;226;130;3momarex\r\n",
        );
        let mut render_state = vt_engine::RenderState::new().expect("create RenderState");
        let _snap = render_state
            .begin_update(&term, vt_engine::Overscan { above: 1, below: 1 })
            .expect("begin_update");
        render_state.end_update().expect("end_update");

        let palette = omarchy_theme::ThemePalette::default_dark();
        let metrics = renderer.atlas_mut().measure_monospace_cell(14.0, 1.0);
        let viewport = TerminalViewport {
            rect: Rect::new(0.0, 0.0, 800.0, 480.0),
            clip_rect: Rect::new(0.0, 0.0, 800.0, 480.0).to_clip_rect(),
            cols: 80,
            rows: 24,
            metrics,
            scroll_offset_y: 0.0,
            default_bg: Color::from(palette.terminal.background),
            default_fg: Color::from(palette.terminal.foreground),
            selection_bg: Color::from(palette.terminal.selection_bg),
            selection_fg: None,
            hyperlink_color: Color::from(palette.ui.accent_primary),
            dim_alpha: 1.0,
        };

        let mut cache = TerminalPaneCache::new();
        let mut dl = DrawList::new(800, 480, 1.0);
        let stats = cache
            .build_from_vt_render_state(
                &mut render_state,
                &viewport,
                &mut renderer.atlas,
                &renderer.ctx.queue,
                &mut dl,
            )
            .expect("build from real libghostty-vt RenderState");
        assert!(
            stats.glyphs >= 16,
            "expected glyphs from VT output, got {}",
            stats.glyphs
        );
        assert_eq!(stats.rebuilt_rows, stats.total_rows);
        render_state.clean().expect("clean render state");

        // Render clean state on second frame -> 0 rebuilt rows!
        let _snap2 = render_state
            .begin_update(&term, vt_engine::Overscan { above: 1, below: 1 })
            .expect("second begin_update");
        render_state.end_update().expect("second end_update");
        let mut dl2 = DrawList::new(800, 480, 1.0);
        let stats2 = cache
            .build_from_vt_render_state(
                &mut render_state,
                &viewport,
                &mut renderer.atlas,
                &renderer.ctx.queue,
                &mut dl2,
            )
            .expect("second build from clean RenderState");
        assert_eq!(stats2.rebuilt_rows, 0);
        assert_eq!(stats2.cached_rows, stats2.total_rows);

        let frame = dl2.finish();
        let (rgba, frame_stats) = renderer
            .render_to_rgba(&frame)
            .expect("GPU render of VT frame");
        assert_eq!(rgba.len(), 800 * 480 * 4);
        assert!(frame_stats.glyph_count >= 16);
    }

    #[test]
    fn vt_engine_sgr_inverse_renders_bright_background_without_double_swap() {
        let mut renderer =
            GpuRenderer::new_headless(GpuConfig::default()).expect("headless renderer");
        let term = vt_engine::Terminal::new(40, 6, vt_engine::TerminalConfig::default())
            .expect("create libghostty-vt terminal");
        // Matches tr3x's exact selected slash-menu escape sequence (`\x1b[7m\x1b[1m goal \x1b[0m`).
        term.write(b"\x1b[7m\x1b[1m goal \x1b[0m\r\n");

        let mut render_state = vt_engine::RenderState::new().expect("create RenderState");
        let _snap = render_state
            .begin_update(&term, vt_engine::Overscan { above: 0, below: 0 })
            .expect("begin_update");
        render_state.end_update().expect("end_update");

        let palette = omarchy_theme::ThemePalette::default_dark();
        let metrics = renderer.atlas_mut().measure_monospace_cell(14.0, 1.0);
        let viewport = TerminalViewport {
            rect: Rect::new(0.0, 0.0, 400.0, 140.0),
            clip_rect: Rect::new(0.0, 0.0, 400.0, 140.0).to_clip_rect(),
            cols: 40,
            rows: 6,
            metrics,
            scroll_offset_y: 0.0,
            default_bg: Color::from(palette.terminal.background),
            default_fg: Color::from(palette.terminal.foreground),
            selection_bg: Color::from(palette.terminal.selection_bg),
            selection_fg: None,
            hyperlink_color: Color::from(palette.ui.accent_primary),
            dim_alpha: 1.0,
        };

        let mut cache = TerminalPaneCache::new();
        let mut dl = DrawList::new(400, 140, 1.0);
        dl.set_clear_color(viewport.default_bg);
        let stats = cache
            .build_from_vt_render_state(
                &mut render_state,
                &viewport,
                &mut renderer.atlas,
                &renderer.ctx.queue,
                &mut dl,
            )
            .expect("build inverted VT row");
        assert!(
            stats.bg_quads >= 1,
            "SGR 7 inverse cells must emit an explicit inverted background quad (got {})",
            stats.bg_quads
        );

        let frame = dl.finish();
        let (rgba, _) = renderer
            .render_to_rgba(&frame)
            .expect("render inverted row to RGBA");
        // Sample the first inverted space cell (col 0, row 0): its pixel must be bright
        // (resolved from default_fg) rather than dark default_bg (which happened when double-swapped).
        let sample_x = (metrics.cell_width * 0.5).round() as usize;
        let sample_y = (metrics.cell_height * 0.5).round() as usize;
        let idx = (sample_y * 400 + sample_x) * 4;
        let (r, g, b) = (rgba[idx], rgba[idx + 1], rgba[idx + 2]);
        assert!(
            r > 180 && g > 180 && b > 180,
            "expected bright inverted background on SGR 7 space cell, got rgb({r}, {g}, {b})"
        );
    }

    #[test]
    fn offscreen_smoke_scene_renders_crisp_png_under_2ms() {
        let mut renderer =
            GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");

        // Load real repository SVG icons if present.
        let icons_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("icons");
        if icons_dir.is_dir() {
            let loaded = renderer
                .atlas_mut()
                .load_icons_from_dir(&icons_dir)
                .expect("load repo SVG icons");
            assert!(loaded > 0);
        }

        // Upload a vibrant synthetic preview image (Kitty / hover preview gradient card).
        let img_w = 240u32;
        let img_h = 112u32;
        let mut preview_rgba = Vec::with_capacity((img_w * img_h * 4) as usize);
        for y in 0..img_h {
            for x in 0..img_w {
                let fx = x as f32 / img_w as f32;
                let fy = y as f32 / img_h as f32;
                let wave = ((fx * 10.0 + fy * 4.0).sin() * 0.5 + 0.5) * 0.28;
                let r = ((0.12 + fx * 0.78 + wave) * 255.0).clamp(0.0, 255.0) as u8;
                let g = ((0.18 + (1.0 - fx) * 0.45 + fy * 0.25) * 255.0).clamp(0.0, 255.0) as u8;
                let b = ((0.30 + fy * 0.62) * 255.0).clamp(0.0, 255.0) as u8;
                // Add crisp subtle chart gridlines inside the embedded Kitty image.
                let grid_line = (x % 30 == 0 || y % 28 == 0) && x > 0 && y > 0;
                if grid_line {
                    preview_rgba.extend_from_slice(&[
                        r.saturating_add(28),
                        g.saturating_add(28),
                        b.saturating_add(28),
                        255,
                    ]);
                } else {
                    preview_rgba.extend_from_slice(&[r, g, b, 255]);
                }
            }
        }
        renderer
            .upload_image(ImageUpload {
                id: 42,
                generation: 1,
                width: img_w,
                height: img_h,
                format: ImagePixelFormat::Rgba8,
                data: &preview_rgba,
            })
            .expect("upload test Kitty image");

        let width = 1280u32;
        let height = 760u32;
        let scale = 1.0f32;
        let metrics = renderer.atlas_mut().measure_monospace_cell(14.0, scale);

        // Omarchy dark theme palette tokens from docs/gui-spec.md.
        let c_frame = Color::from_hex(0x0e0e0e);
        let c_pane = Color::from_hex(0x121212);
        let c_row_active = Color::from_hex(0x1e1e1e);
        let c_icon_inactive = Color::from_hex(0x262626);
        let c_modal = Color::from_hex(0x181818);
        let c_fg = Color::from_hex(0xffffff);
        let c_fg_sec = Color::from_hex(0xbfbfbf);
        let c_fg_muted = Color::from_hex(0x9e9e9e);
        let c_accent = Color::from_hex(0xe28203);
        let c_claude = Color::from_hex(0xd96b43);
        let c_green = Color::from_hex(0x4ade80);
        let c_red = Color::from_hex(0xef3827);
        let c_blue = Color::from_hex(0x4e8df5);
        let c_border_hair = Color::from_hex_alpha(0xffffff, 0.09);
        let c_border_subtle = Color::from_hex_alpha(0xffffff, 0.16);

        let build_scene = |renderer: &mut GpuRenderer,
                           cache: &mut TerminalPaneCache|
         -> SceneFrame {
            let mut dl = DrawList::new(width, height, scale);
            dl.set_clear_color(c_frame);
            dl.set_light_mode(false);

            // --- Sidebar (260px wide) ---
            dl.with_layer(layers::CANVAS, |dl| {
                dl.push_rect(Rect::new(0.0, 0.0, 260.0, height as f32), c_frame);
                dl.push_rect(Rect::new(259.0, 0.0, 1.0, height as f32), c_border_hair);
            });

            dl.with_layer(layers::CHROME, |dl| {
                // Sidebar top utility bar
                dl.push_icon("sidebar", Rect::new(14.0, 12.0, 16.0, 16.0), Some(c_fg_sec));
                dl.push_icon(
                    "activity",
                    Rect::new(202.0, 12.0, 16.0, 16.0),
                    Some(c_fg_sec),
                );
                // Activity status dot badge
                dl.push_rounded_rect(
                    Rect::new(214.0, 10.0, 6.0, 6.0),
                    CornerRadii::uniform(3.0),
                    c_accent,
                    c_frame,
                    1.0,
                );
                dl.push_icon("plus", Rect::new(230.0, 12.0, 16.0, 16.0), Some(c_fg_sec));

                // Host + Session headers
                dl.push_text(
                    TextRun::new("This Machine (Omarchy)", 16.0, 48.0, 12.5, c_fg)
                        .weight(FontWeight::Medium),
                );
                dl.push_text(
                    TextRun::new("T3 Code", 16.0, 74.0, 11.0, c_fg_muted)
                        .weight(FontWeight::SemiBold),
                );

                // Active Tab Card #1 with subtle elevation & border
                let card1 = Rect::new(10.0, 96.0, 240.0, 52.0);
                dl.push_shadow(
                    card1,
                    CornerRadii::uniform(8.0),
                    Color::rgba(0.0, 0.0, 0.0, 0.40),
                    12.0,
                    0.0,
                    4.0,
                );
                dl.push_rounded_rect(
                    card1,
                    CornerRadii::uniform(8.0),
                    c_row_active,
                    c_border_subtle,
                    1.0,
                );
                // Claude active brand tile + orange icon
                dl.push_rounded_rect(
                    Rect::new(20.0, 109.0, 26.0, 26.0),
                    CornerRadii::uniform(6.0),
                    c_claude,
                    Color::TRANSPARENT,
                    0.0,
                );
                dl.push_icon("claude", Rect::new(25.0, 114.0, 16.0, 16.0), Some(c_fg));
                // Status dot overlay (blocked/needs-you)
                dl.push_rounded_rect(
                    Rect::new(41.0, 106.0, 8.0, 8.0),
                    CornerRadii::uniform(4.0),
                    c_accent,
                    c_row_active,
                    1.5,
                );
                dl.push_text(
                    TextRun::new("Vulkan GPU Renderer", 56.0, 106.0, 12.5, c_fg)
                        .weight(FontWeight::Medium),
                );
                dl.push_text(TextRun::new(
                    "omarex · Needs you",
                    56.0,
                    125.0,
                    11.0,
                    c_fg_muted,
                ));
                dl.push_text(TextRun::new("now", 220.0, 125.0, 10.5, c_fg_muted).monospace());

                // Tab Card #2 (Working)
                dl.push_rounded_rect(
                    Rect::new(20.0, 169.0, 26.0, 26.0),
                    CornerRadii::uniform(6.0),
                    c_icon_inactive,
                    c_border_hair,
                    1.0,
                );
                dl.push_icon("openai", Rect::new(25.0, 174.0, 16.0, 16.0), Some(c_fg_sec));
                dl.push_text(
                    TextRun::new("Rex API rev 24 stream", 56.0, 166.0, 12.5, c_fg_sec)
                        .weight(FontWeight::Medium),
                );
                dl.push_text(TextRun::new(
                    "rex-proto · Working",
                    56.0,
                    185.0,
                    11.0,
                    c_fg_muted,
                ));
                // Working dots (•••)
                for i in 0..3 {
                    dl.push_rounded_rect(
                        Rect::new(220.0 + (i as f32) * 6.0, 173.0, 3.5, 3.5),
                        CornerRadii::uniform(1.75),
                        c_accent.mul_alpha(0.45 + (i as f32) * 0.25),
                        Color::TRANSPARENT,
                        0.0,
                    );
                }
                dl.push_text(TextRun::new("4m", 226.0, 185.0, 10.5, c_fg_muted).monospace());

                // Tab Card #3 (Error)
                dl.push_rounded_rect(
                    Rect::new(20.0, 223.0, 26.0, 26.0),
                    CornerRadii::uniform(6.0),
                    c_icon_inactive,
                    c_border_hair,
                    1.0,
                );
                dl.push_icon("gemini", Rect::new(25.0, 228.0, 16.0, 16.0), Some(c_fg_sec));
                dl.push_rounded_rect(
                    Rect::new(41.0, 220.0, 8.0, 8.0),
                    CornerRadii::uniform(4.0),
                    c_red,
                    c_frame,
                    1.5,
                );
                dl.push_text(
                    TextRun::new("libghostty-vt FFI tests", 56.0, 220.0, 12.5, c_fg_sec)
                        .weight(FontWeight::Medium),
                );
                dl.push_text(TextRun::new(
                    "vt-engine · Error",
                    56.0,
                    239.0,
                    11.0,
                    c_fg_muted,
                ));
                dl.push_text(TextRun::new("12m", 220.0, 239.0, 10.5, c_fg_muted).monospace());

                // Tab Card #4 (Done)
                dl.push_rounded_rect(
                    Rect::new(20.0, 277.0, 26.0, 26.0),
                    CornerRadii::uniform(6.0),
                    c_icon_inactive,
                    c_border_hair,
                    1.0,
                );
                dl.push_icon(
                    "terminal",
                    Rect::new(25.0, 282.0, 16.0, 16.0),
                    Some(c_fg_sec),
                );
                dl.push_rounded_rect(
                    Rect::new(41.0, 274.0, 8.0, 8.0),
                    CornerRadii::uniform(4.0),
                    c_green,
                    c_frame,
                    1.5,
                );
                dl.push_text(
                    TextRun::new("Omarchy theme watcher", 56.0, 274.0, 12.5, c_fg_sec)
                        .weight(FontWeight::Medium),
                );
                dl.push_text(TextRun::new(
                    "omarchy-theme · Done",
                    56.0,
                    293.0,
                    11.0,
                    c_fg_muted,
                ));
                dl.push_text(TextRun::new("18m", 220.0, 293.0, 10.5, c_fg_muted).monospace());
            });

            // --- Main Terminal Pane Card ---
            let pane_card = Rect::new(270.0, 10.0, 1000.0, 740.0);
            dl.with_layer(layers::CANVAS, |dl| {
                dl.push_rounded_rect(
                    pane_card,
                    CornerRadii::uniform(8.0),
                    c_pane,
                    c_accent.with_alpha(0.75),
                    1.5,
                );
                // Pane header separator
                dl.push_rect(
                    Rect::new(
                        pane_card.x + 1.5,
                        pane_card.y + 34.0,
                        pane_card.width - 3.0,
                        1.0,
                    ),
                    c_border_hair,
                );
            });

            dl.with_layer(layers::CHROME, |dl| {
                dl.push_icon(
                    "terminal",
                    Rect::new(pane_card.x + 14.0, pane_card.y + 10.0, 14.0, 14.0),
                    Some(c_accent),
                );
                dl.push_text(
                    TextRun::new("omarex", pane_card.x + 34.0, pane_card.y + 9.0, 12.5, c_fg)
                        .weight(FontWeight::Medium),
                );
                dl.push_text(TextRun::new(
                    "~/Work/omarex",
                    pane_card.x + 92.0,
                    pane_card.y + 9.0,
                    12.0,
                    c_fg_muted,
                ));
                // OSC 7501 header status chip
                dl.push_rounded_rect(
                    Rect::new(pane_card.x + 210.0, pane_card.y + 8.0, 152.0, 19.0),
                    CornerRadii::uniform(9.5),
                    c_accent.with_alpha(0.16),
                    c_accent.with_alpha(0.45),
                    1.0,
                );
                dl.push_rounded_rect(
                    Rect::new(pane_card.x + 218.0, pane_card.y + 14.5, 6.0, 6.0),
                    CornerRadii::uniform(3.0),
                    c_accent,
                    Color::TRANSPARENT,
                    0.0,
                );
                dl.push_text(
                    TextRun::new(
                        "Vulkan 1.4 · <1ms",
                        pane_card.x + 230.0,
                        pane_card.y + 10.0,
                        11.0,
                        c_accent,
                    )
                    .weight(FontWeight::Medium),
                );
            });

            // --- Terminal Grid via TerminalPaneCache ---
            let grid_rect = Rect::new(pane_card.x + 16.0, pane_card.y + 46.0, 968.0, 678.0);
            let viewport = TerminalViewport {
                rect: grid_rect,
                clip_rect: grid_rect.to_clip_rect(),
                cols: 112,
                rows: 34,
                metrics,
                scroll_offset_y: 0.0,
                default_bg: c_pane,
                default_fg: c_fg,
                selection_bg: Color::from_hex(0x2a2a2a),
                selection_fg: None,
                hyperlink_color: c_blue,
                dim_alpha: 1.0,
            };

            let mut grid = TerminalGridSnapshot::new();
            grid.push_styled_row(
                1,
                0,
                true,
                [
                    ("mike@omarchy", c_green, None, CellFlags::BOLD),
                    (" ~/Work/omarex ", c_blue, None, CellFlags::NONE),
                    (
                        "cargo test -p gpu-renderer -- --nocapture",
                        c_fg,
                        None,
                        CellFlags::NONE,
                    ),
                ],
            );
            // Exact 78-column aligned box drawing table to prove zero-seam cell geometry.
            grid.push_styled_row(
                2,
                1,
                true,
                [(
                    "╭────────────────────────────────────────────────────────────────────────────╮",
                    c_accent,
                    None,
                    CellFlags::NONE,
                )],
            );
            grid.push_styled_row(
                3,
                2,
                true,
                [
                    ("│ ", c_accent, None, CellFlags::NONE),
                    ("OMAREX GPU PIPELINE", c_fg, None, CellFlags::BOLD),
                    ("  ::  ", c_fg_muted, None, CellFlags::NONE),
                    (
                        "wgpu 30 + Vulkan 1.4 + JetBrainsMono NF",
                        c_green,
                        None,
                        CellFlags::NONE,
                    ),
                    ("           │", c_accent, None, CellFlags::NONE),
                ],
            );
            grid.push_styled_row(
                4,
                3,
                true,
                [(
                    "├─────────────────────────────────────┬──────────────────────────────────────┤",
                    c_accent,
                    None,
                    CellFlags::NONE,
                )],
            );
            grid.push_styled_row(
                5,
                4,
                true,
                [
                    ("│ ", c_accent, None, CellFlags::NONE),
                    ("Normal ", c_fg, None, CellFlags::NONE),
                    ("Bold ", c_fg, None, CellFlags::BOLD),
                    ("Italic ", c_fg_sec, None, CellFlags::ITALIC),
                    (
                        "BoldItalic",
                        c_accent,
                        None,
                        CellFlags::BOLD | CellFlags::ITALIC,
                    ),
                    ("       │ ", c_accent, None, CellFlags::NONE),
                    ("Selection highlight", c_fg, None, CellFlags::SELECTED),
                    ("                  │", c_accent, None, CellFlags::NONE),
                ],
            );
            grid.push_styled_row(
                6,
                5,
                true,
                [
                    ("│ ", c_accent, None, CellFlags::NONE),
                    ("Underline ", c_blue, None, CellFlags::UNDERLINE),
                    ("CurlyErr ", c_red, None, CellFlags::CURLY_UNDERLINE),
                    ("Strike ", c_fg_muted, None, CellFlags::STRIKETHROUGH),
                    ("Inverse", c_fg, Some(c_pane), CellFlags::INVERSE),
                    ("   │ ", c_accent, None, CellFlags::NONE),
                    (
                        "https://superlogical.com/rex",
                        c_blue,
                        None,
                        CellFlags::HYPERLINK,
                    ),
                    ("         │", c_accent, None, CellFlags::NONE),
                ],
            );
            grid.push_styled_row(
                7,
                6,
                true,
                [
                    ("│ ", c_accent, None, CellFlags::NONE),
                    ("Blocks: ", c_fg_muted, None, CellFlags::NONE),
                    ("▁▂▃▄▅▆▇█", c_green, None, CellFlags::NONE),
                    (" ░▒▓█ ", c_blue, None, CellFlags::NONE),
                    ("▀▄▌▐        ", c_accent, None, CellFlags::NONE),
                    ("  │ ", c_accent, None, CellFlags::NONE),
                    ("Double: ", c_fg_muted, None, CellFlags::NONE),
                    ("╔══╦══╗ ╠══╬══╣ ╚══╩══╝", c_fg_sec, None, CellFlags::NONE),
                    ("      │", c_accent, None, CellFlags::NONE),
                ],
            );
            grid.push_styled_row(
                8,
                7,
                true,
                [(
                    "╰─────────────────────────────────────┴──────────────────────────────────────╯",
                    c_accent,
                    None,
                    CellFlags::NONE,
                )],
            );
            grid.push_styled_row(
                9,
                9,
                true,
                [
                    ("  PASS ", c_pane, Some(c_green), CellFlags::BOLD),
                    (" shaders/quad.wgsl   ", c_fg, None, CellFlags::NONE),
                    (
                        "SDF rounded rects, borders, Gaussian shadow, curly underline",
                        c_fg_muted,
                        None,
                        CellFlags::NONE,
                    ),
                ],
            );
            grid.push_styled_row(
                10,
                10,
                true,
                [
                    ("  PASS ", c_pane, Some(c_green), CellFlags::BOLD),
                    (" shaders/glyph.wgsl  ", c_fg, None, CellFlags::NONE),
                    (
                        "R8Unorm 2048x2048 + Rgba8UnormSrgb 1024x1024 dual atlas",
                        c_fg_muted,
                        None,
                        CellFlags::NONE,
                    ),
                ],
            );
            grid.push_styled_row(
                11,
                11,
                true,
                [
                    ("  PASS ", c_pane, Some(c_green), CellFlags::BOLD),
                    (" shaders/image.wgsl  ", c_fg, None, CellFlags::NONE),
                    (
                        "Kitty Graphics Protocol layers + bounded VRAM LRU cache",
                        c_fg_muted,
                        None,
                        CellFlags::NONE,
                    ),
                ],
            );
            grid.push_styled_row(
                12,
                13,
                true,
                [
                    ("❯ ", c_accent, None, CellFlags::BOLD),
                    (
                        "echo \"Ready for Rex + tr3x at 300Hz\"",
                        c_fg,
                        None,
                        CellFlags::NONE,
                    ),
                ],
            );
            grid.set_cursor(Some(TerminalCursor {
                col: 38,
                row: 13,
                shape: CursorShape::Bar,
                color: c_accent,
                text_color: c_pane,
                visible: true,
            }));

            cache.build_pane(
                &grid,
                &viewport,
                &mut renderer.atlas,
                &renderer.ctx.queue,
                &mut dl,
            );

            // --- Embedded Kitty Image Placement (ABOVE_TEXT) ---
            let mut kitty_placements = vec![KittyPlacement {
                image_id: 42,
                generation: 1,
                layer: ImageLayer::AboveText,
                z_index: 1,
                dest_rect: Rect::new(grid_rect.x + 8.0, grid_rect.y + 335.0, 260.0, 122.0),
                uv_rect: [0.0, 0.0, 1.0, 1.0],
                opacity: 1.0,
                corner_radius: 8.0,
                clip_rect: grid_rect.to_clip_rect(),
            }];
            ImageTextureCache::emit_placements(&mut kitty_placements, &mut dl);

            // Label & border around the embedded Kitty preview image
            dl.with_layer(layers::CHROME, |dl| {
                dl.push_rounded_rect(
                    Rect::new(grid_rect.x + 8.0, grid_rect.y + 335.0, 260.0, 122.0),
                    CornerRadii::uniform(8.0),
                    Color::TRANSPARENT,
                    c_border_subtle,
                    1.0,
                );
                dl.push_text(TextRun::new(
                    "Kitty Graphics Protocol Layer (ABOVE_TEXT, rounded SDF clip)",
                    grid_rect.x + 8.0,
                    grid_rect.y + 465.0,
                    11.5,
                    c_fg_muted,
                ));
            });

            // --- Floating Command Palette Card with Soft SDF Shadow (MODAL layer) ---
            let modal = Rect::new(690.0, 410.0, 520.0, 260.0);
            dl.with_layer(layers::MODAL, |dl| {
                dl.push_shadow(
                    modal,
                    CornerRadii::uniform(12.0),
                    Color::rgba(0.0, 0.0, 0.0, 0.62),
                    32.0,
                    0.0,
                    14.0,
                );
                dl.push_rounded_rect(
                    modal,
                    CornerRadii::uniform(12.0),
                    c_modal,
                    c_border_subtle,
                    1.0,
                );
                dl.push_icon(
                    "search",
                    Rect::new(modal.x + 16.0, modal.y + 15.0, 16.0, 16.0),
                    Some(c_fg_muted),
                );
                dl.push_text(TextRun::new(
                    "Type a command or T3 Code thread…",
                    modal.x + 42.0,
                    modal.y + 14.0,
                    14.0,
                    c_fg_muted,
                ));
                dl.push_rect(
                    Rect::new(modal.x + 1.0, modal.y + 46.0, modal.width - 2.0, 1.0),
                    c_border_hair,
                );

                // Selected palette row
                dl.push_rounded_rect(
                    Rect::new(modal.x + 8.0, modal.y + 56.0, modal.width - 16.0, 40.0),
                    CornerRadii::uniform(7.0),
                    Color::from_hex(0x2a2a2a),
                    c_accent.with_alpha(0.5),
                    1.0,
                );
                dl.push_icon(
                    "claude",
                    Rect::new(modal.x + 18.0, modal.y + 68.0, 16.0, 16.0),
                    Some(c_claude),
                );
                dl.push_text(
                    TextRun::new(
                        "T3 Code: Answer Next Thread…",
                        modal.x + 44.0,
                        modal.y + 67.0,
                        13.0,
                        c_fg,
                    )
                    .weight(FontWeight::Medium),
                );
                // Keycap hint
                dl.push_rounded_rect(
                    Rect::new(modal.x + 414.0, modal.y + 65.0, 86.0, 22.0),
                    CornerRadii::uniform(5.0),
                    c_frame,
                    c_border_subtle,
                    1.0,
                );
                dl.push_text(
                    TextRun::new(
                        "Ctrl+Alt+A",
                        modal.x + 425.0,
                        modal.y + 69.0,
                        10.5,
                        c_fg_sec,
                    )
                    .monospace(),
                );

                // Second & third rows
                dl.push_icon(
                    "terminal",
                    Rect::new(modal.x + 18.0, modal.y + 114.0, 16.0, 16.0),
                    Some(c_fg_sec),
                );
                dl.push_text(TextRun::new(
                    "T3 Code: Show Pull Request Drawer (/pr)",
                    modal.x + 44.0,
                    modal.y + 113.0,
                    13.0,
                    c_fg_sec,
                ));
                dl.push_icon(
                    "activity",
                    Rect::new(modal.x + 18.0, modal.y + 156.0, 16.0, 16.0),
                    Some(c_fg_sec),
                );
                dl.push_text(TextRun::new(
                    "Fit Session to This Device (t3_fit)",
                    modal.x + 44.0,
                    modal.y + 155.0,
                    13.0,
                    c_fg_sec,
                ));
                dl.push_text(TextRun::new(
                    "Unified single-pass wgpu Vulkan 1.4 renderer · SDF shadows · Dual atlas",
                    modal.x + 18.0,
                    modal.y + 224.0,
                    11.0,
                    c_fg_muted,
                ));
            });

            dl.finish()
        };

        let mut pane_cache = TerminalPaneCache::new();
        // Warm-up frame populates glyph & icon atlases cleanly.
        let warmup_frame = build_scene(&mut renderer, &mut pane_cache);
        let offscreen = renderer
            .create_offscreen_target(width, height)
            .expect("create offscreen target");
        let _warmup_stats = renderer
            .render_to_offscreen(&warmup_frame, &offscreen)
            .expect("warmup render");

        // Timed steady-state frame (atlas + pipelines warm!).
        let steady_frame = build_scene(&mut renderer, &mut pane_cache);
        let steady_stats = renderer
            .render_to_offscreen(&steady_frame, &offscreen)
            .expect("steady offscreen render");

        let rgba = renderer
            .context()
            .readback_offscreen(&offscreen)
            .expect("readback RGBA");

        // Verify non-blank rendered content and exact background color calibration (#0e0e0e).
        assert_eq!(rgba.len(), (width * height * 4) as usize);
        assert!(
            rgba[0].abs_diff(0x0e) <= 1
                && rgba[1].abs_diff(0x0e) <= 1
                && rgba[2].abs_diff(0x0e) <= 1,
            "expected sRGB-calibrated #0e0e0e canvas pixel at (0,0), got {:?}",
            &rgba[0..4]
        );
        let non_canvas_pixels = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[0] != rgba[0] || px[1] != rgba[1] || px[2] != rgba[2])
            .count();
        assert!(
            non_canvas_pixels > 100_000,
            "expected rich rendered scene, only {non_canvas_pixels} non-background pixels"
        );

        let png_path = std::env::var("OMAREX_SMOKE_PNG").unwrap_or_else(|_| {
            std::env::temp_dir()
                .join("omarex_gpu_smoke.png")
                .to_string_lossy()
                .into_owned()
        });
        save_rgba_png(&png_path, width, height, &rgba).expect("save smoke PNG");

        eprintln!(
            "GPU_SMOKE adapter={:?} backend={:?} steady_total_us={} cpu_us={} gpu_submit_us={} draws={} quads={} glyphs={} images={} png={}",
            steady_stats.adapter_name,
            steady_stats.backend,
            steady_stats.total_us,
            steady_stats.cpu_prepare_us,
            steady_stats.gpu_encode_submit_us,
            steady_stats.draw_calls,
            steady_stats.quad_count,
            steady_stats.glyph_count,
            steady_stats.image_count,
            png_path
        );
    }

    #[test]
    fn light_mode_gamma_text_and_atlas_reset_invalidation() {
        let mut renderer =
            GpuRenderer::new_headless(GpuConfig::default()).expect("headless renderer");

        let mut dl = DrawList::new(400, 160, 2.0); // HiDPI 2x scale
        dl.set_clear_color(Color::from_hex(0xf8f9fb));
        dl.set_light_mode(true);
        dl.push_rounded_rect(
            Rect::new(12.0, 12.0, 176.0, 56.0),
            CornerRadii::uniform(8.0),
            Color::WHITE,
            Color::from_hex_alpha(0x000000, 0.14),
            1.0,
        );
        dl.push_text(
            TextRun::new(
                "Alpine Light · 2x HiDPI",
                22.0,
                28.0,
                13.0,
                Color::from_hex(0x1c1f26),
            )
            .weight(FontWeight::Medium),
        );
        dl.push_icon("claude", Rect::new(160.0, 28.0, 16.0, 16.0), None);
        let frame = dl.finish();

        let (rgba, stats) = renderer
            .render_to_rgba(&frame)
            .expect("light-mode HiDPI render");
        assert_eq!(rgba.len(), 400 * 160 * 4);
        assert!(stats.glyph_count > 10);
        // Verify light canvas pixel calibration (#f8f9fb).
        assert!(
            rgba[0].abs_diff(0xf8) <= 1
                && rgba[1].abs_diff(0xf9) <= 1
                && rgba[2].abs_diff(0xfb) <= 1,
            "expected light-mode #f8f9fb canvas at (0,0), got {:?}",
            &rgba[0..4]
        );

        // Verify clean atlas reset bumps generation and invalidates terminal cache cleanly.
        let gen_before = renderer.atlas().generation();
        renderer.atlas_mut().reset_mono_plane();
        assert_eq!(renderer.atlas().generation(), gen_before + 1);
        assert_eq!(renderer.atlas().reset_count(), 1);
    }
}
