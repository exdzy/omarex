//! `wgpu` GPU context initialization (Vulkan primary, GLES fallback, headless offscreen mode),
//! swapchain surface lifecycle, HiDPI resize, and offscreen RGBA8 readback + PNG export.

use std::future::Future;
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use image::{ImageBuffer, Rgba};
use thiserror::Error;
use tracing::{info, warn};

/// Errors produced by GPU initialization, rendering, or texture/image operations.
#[derive(Debug, Error)]
pub enum RenderError {
    #[error("no suitable GPU adapter found (tried Vulkan and GLES backends)")]
    NoAdapter,
    #[error("failed to request GPU device: {0}")]
    RequestDevice(String),
    #[error("failed to create surface: {0}")]
    CreateSurface(String),
    #[error("surface is not configured for presentation")]
    NoSurface,
    #[error("surface error: {0}")]
    Surface(String),
    #[error("buffer map failed: {0}")]
    BufferMap(String),
    #[error("device poll failed: {0}")]
    DevicePoll(String),
    #[error("invalid dimensions: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("image decode/encode error: {0}")]
    Image(String),
    #[error("SVG parse/render error: {0}")]
    Svg(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Swapchain presentation mode preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PresentModePreference {
    /// Tear-free VSync, ideal for Hyprland VRR and zero idle spin.
    #[default]
    Fifo,
    /// Triple-buffered ultra-low-latency presentation (falls back to Fifo if unsupported).
    Mailbox,
}

/// Configuration for initializing a [`GpuContext`].
#[derive(Debug, Clone)]
pub struct GpuConfig {
    /// Preferred present mode for windowed surfaces (`Fifo` default).
    pub present_mode: PresentModePreference,
    /// Whether translucent Wayland surfaces should prefer premultiplied/post-multiplied alpha.
    pub transparent_surface: bool,
    /// Optional power preference (`HighPerformance` default for discrete RTX GPUs).
    pub power_preference: wgpu::PowerPreference,
    /// Whether to force software fallback adapter (useful only for specialized CI tests).
    pub force_fallback_adapter: bool,
    /// Maximum VRAM budget for the Kitty / preview image cache in bytes (default: 128 MiB).
    pub image_cache_budget_bytes: usize,
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            present_mode: PresentModePreference::Fifo,
            transparent_surface: false,
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            image_cache_budget_bytes: 128 * 1024 * 1024,
        }
    }
}

/// Human-readable summary of the active GPU adapter (for diagnostics and tests).
#[derive(Debug, Clone)]
pub struct AdapterSummary {
    pub name: String,
    pub backend: wgpu::Backend,
    pub device_type: wgpu::DeviceType,
    pub driver: String,
    pub driver_info: String,
}

/// Outcome of acquiring the next swapchain frame.
pub enum AcquiredFrame {
    Ready {
        texture: wgpu::SurfaceTexture,
        suboptimal: bool,
    },
    /// Frame should be skipped (minimized/occluded or transient timeout).
    Skipped,
    /// Surface configuration was outdated and has been reconfigured; caller may retry next tick.
    Reconfigured,
}

/// Windowed swapchain state attached to a [`GpuContext`] or `SurfaceRenderer`.
pub struct SurfaceState {
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    pub capabilities: wgpu::SurfaceCapabilities,
    pub present_mode_pref: PresentModePreference,
    pub transparent_surface: bool,
    pub scale_factor: f32,
}

impl SurfaceState {
    /// Configures a `wgpu::Surface` for presentation with the given adapter and device.
    pub(crate) fn configure_new(
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
        scale_factor: f32,
        config: &GpuConfig,
    ) -> Result<Self, RenderError> {
        let capabilities = surface.get_capabilities(adapter);
        if capabilities.formats.is_empty() {
            return Err(RenderError::CreateSurface(
                "adapter does not support window surface (no supported formats)".to_owned(),
            ));
        }
        let format = pick_surface_format(&capabilities);
        let present_mode = pick_present_mode(&capabilities, config.present_mode);
        let alpha_mode = pick_alpha_mode(&capabilities, config.transparent_surface);

        let safe_w = width.max(1);
        let safe_h = height.max(1);
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: safe_w,
            height: safe_h,
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(device, &surface_config);

        Ok(Self {
            surface,
            config: surface_config,
            capabilities,
            present_mode_pref: config.present_mode,
            transparent_surface: config.transparent_surface,
            scale_factor: scale_factor.max(0.5),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32, scale_factor: f32) {
        if width == 0 || height == 0 {
            return;
        }
        let clamped_scale = scale_factor.max(0.5);
        if self.config.width == width
            && self.config.height == height
            && (self.scale_factor - clamped_scale).abs() < f32::EPSILON
        {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.scale_factor = clamped_scale;
        self.surface.configure(device, &self.config);
    }

    pub fn set_present_mode(&mut self, device: &wgpu::Device, pref: PresentModePreference) {
        self.present_mode_pref = pref;
        let next = pick_present_mode(&self.capabilities, pref);
        if self.config.present_mode != next {
            self.config.present_mode = next;
            self.surface.configure(device, &self.config);
        }
    }

    pub fn acquire_frame(&mut self, device: &wgpu::Device) -> Result<AcquiredFrame, RenderError> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => Ok(AcquiredFrame::Ready {
                texture,
                suboptimal: false,
            }),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => Ok(AcquiredFrame::Ready {
                texture,
                suboptimal: true,
            }),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                Ok(AcquiredFrame::Skipped)
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                warn!("swapchain outdated/lost; reconfiguring surface");
                self.surface.configure(device, &self.config);
                Ok(AcquiredFrame::Reconfigured)
            }
            wgpu::CurrentSurfaceTexture::Validation => Err(RenderError::Surface(
                "surface validation error while acquiring texture".to_owned(),
            )),
        }
    }

    pub fn reconfigure(&mut self, device: &wgpu::Device) {
        self.surface.configure(device, &self.config);
    }
}

/// Reusable offscreen RGBA8 render target + aligned readback staging buffer for PNG export & tests.
pub struct OffscreenTarget {
    pub width: u32,
    pub height: u32,
    pub format: wgpu::TextureFormat,
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub staging_buffer: wgpu::Buffer,
    pub padded_bytes_per_row: u32,
}

impl OffscreenTarget {
    /// Creates an offscreen sRGB RGBA8 render target suitable for headless rendering and PNG export.
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::InvalidDimensions { width, height });
        }
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("omarex-offscreen-color"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let unpadded_bpr = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bpr.div_ceil(align) * align;
        let buffer_size = u64::from(padded_bytes_per_row) * u64::from(height);

        let staging_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("omarex-offscreen-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        Ok(Self {
            width,
            height,
            format,
            texture,
            view,
            staging_buffer,
            padded_bytes_per_row,
        })
    }

    /// Encodes a GPU texture-to-buffer copy from this offscreen texture to its staging buffer.
    pub fn encode_copy_to_staging(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.staging_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }
}

/// Core `wgpu` context owning the instance, adapter, logical device, queue, and optional surface.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_summary: AdapterSummary,
    pub target_format: wgpu::TextureFormat,
    pub surface: Option<SurfaceState>,
}

impl GpuContext {
    /// Initializes a headless `wgpu` context requiring NO window or Wayland display.
    ///
    /// Tries Vulkan first (primary on NVIDIA RTX / Linux), then falls back to GLES.
    pub fn new_headless(config: &GpuConfig) -> Result<Self, RenderError> {
        let (instance, adapter) = select_instance_and_adapter(None, config)?;
        let (device, queue, adapter_summary) = request_device(&adapter)?;

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            adapter_summary,
            target_format: wgpu::TextureFormat::Rgba8UnormSrgb,
            surface: None,
        })
    }

    /// Initializes a `wgpu` context bound to a window surface (Wayland / X11 via `winit`).
    pub fn new_with_surface(
        surface_target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        scale_factor: f32,
        config: &GpuConfig,
    ) -> Result<Self, RenderError> {
        let (mut ctx, surface_state) =
            Self::new_for_initial_surface(surface_target, width, height, scale_factor, config)?;
        ctx.surface = Some(surface_state);
        Ok(ctx)
    }

    /// Initializes a device-wide `GpuContext` alongside the first configured `SurfaceState`
    /// without storing the `SurfaceState` inside `GpuContext`.
    pub(crate) fn new_for_initial_surface(
        surface_target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        scale_factor: f32,
        config: &GpuConfig,
    ) -> Result<(Self, SurfaceState), RenderError> {
        // Prefer Vulkan first; GLES fallback requires a separate instance creation if Vulkan fails.
        let target = surface_target.into();
        let instance_vk = create_instance(wgpu::Backends::VULKAN);
        let surface_vk = instance_vk
            .create_surface(target)
            .map_err(|e| RenderError::CreateSurface(e.to_string()))?;

        let adapter = request_adapter_for_instance(&instance_vk, Some(&surface_vk), config)
            .ok_or(RenderError::NoAdapter)?;

        let (device, queue, adapter_summary) = request_device(&adapter)?;
        let surface_state = SurfaceState::configure_new(
            &adapter,
            &device,
            surface_vk,
            width,
            height,
            scale_factor,
            config,
        )?;
        let format = surface_state.format();

        Ok((
            Self {
                instance: instance_vk,
                adapter,
                device,
                queue,
                adapter_summary,
                target_format: format,
                surface: None,
            },
            surface_state,
        ))
    }

    /// Creates and configures an additional `SurfaceState` sharing this context's `Instance`,
    /// `Adapter`, `Device`, and `Queue`.
    pub(crate) fn create_surface_state(
        &self,
        surface_target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        scale_factor: f32,
        config: &GpuConfig,
    ) -> Result<SurfaceState, RenderError> {
        let surface = self
            .instance
            .create_surface(surface_target.into())
            .map_err(|e| RenderError::CreateSurface(e.to_string()))?;
        SurfaceState::configure_new(
            &self.adapter,
            &self.device,
            surface,
            width,
            height,
            scale_factor,
            config,
        )
    }

    /// Returns the active surface physical dimensions `(width, height)`, if a surface is attached.
    pub fn surface_size(&self) -> Option<(u32, u32)> {
        self.surface.as_ref().map(SurfaceState::size)
    }

    /// Returns the current DPI scale factor (`1.0` when headless).
    pub fn scale_factor(&self) -> f32 {
        self.surface.as_ref().map_or(1.0, |s| s.scale_factor)
    }

    /// Reconfigures the swapchain after window resize or HiDPI scale factor change.
    pub fn resize(&mut self, width: u32, height: u32, scale_factor: f32) {
        if let Some(surf) = self.surface.as_mut() {
            surf.resize(&self.device, width, height, scale_factor);
        }
    }

    /// Switches swapchain presentation mode (`Fifo` VSync vs `Mailbox` low-latency) live.
    pub fn set_present_mode(&mut self, pref: PresentModePreference) {
        if let Some(surf) = self.surface.as_mut() {
            surf.set_present_mode(&self.device, pref);
        }
    }

    /// Acquires the next swapchain texture, automatically reconfiguring on `Outdated`/`Lost`.
    pub fn acquire_surface_frame(&mut self) -> Result<AcquiredFrame, RenderError> {
        let Some(surf) = self.surface.as_mut() else {
            return Err(RenderError::NoSurface);
        };
        surf.acquire_frame(&self.device)
    }

    /// Reconfigures the active surface using its existing settings (e.g. after a suboptimal frame).
    pub fn reconfigure_surface(&mut self) {
        if let Some(surf) = self.surface.as_mut() {
            surf.reconfigure(&self.device);
        }
    }

    /// Reads tightly packed RGBA8 pixels (`width * height * 4` bytes) from an [`OffscreenTarget`]
    /// after its copy command has been submitted to `self.queue`.
    pub fn readback_offscreen(&self, target: &OffscreenTarget) -> Result<Vec<u8>, RenderError> {
        let slice = target.staging_buffer.slice(..);
        let ready = Arc::new(AtomicBool::new(false));
        let map_result: Arc<parking_lot::Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> =
            Arc::new(parking_lot::Mutex::new(None));

        let ready_cb = Arc::clone(&ready);
        let result_cb = Arc::clone(&map_result);
        slice.map_async(wgpu::MapMode::Read, move |res| {
            *result_cb.lock() = Some(res);
            ready_cb.store(true, Ordering::Release);
        });

        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .map_err(|e| RenderError::DevicePoll(e.to_string()))?;

        if !ready.load(Ordering::Acquire) {
            return Err(RenderError::BufferMap(
                "buffer map callback did not complete after device poll".to_owned(),
            ));
        }

        match map_result.lock().take() {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(RenderError::BufferMap(e.to_string())),
            None => {
                return Err(RenderError::BufferMap(
                    "missing buffer map callback status".to_owned(),
                ));
            }
        }

        let mapped = slice
            .get_mapped_range()
            .map_err(|e| RenderError::BufferMap(e.to_string()))?;
        let unpadded_bpr = (target.width * 4) as usize;
        let padded_bpr = target.padded_bytes_per_row as usize;
        let height = target.height as usize;
        let mut rgba = Vec::with_capacity(unpadded_bpr * height);

        for row in 0..height {
            let start = row * padded_bpr;
            rgba.extend_from_slice(&mapped[start..start + unpadded_bpr]);
        }

        drop(mapped);
        target.staging_buffer.unmap();
        Ok(rgba)
    }
}

/// Saves tightly packed sRGB RGBA8 pixel bytes to a PNG file on disk.
pub fn save_rgba_png(
    path: impl AsRef<Path>,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<(), RenderError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or(RenderError::InvalidDimensions { width, height })?;
    if rgba.len() != expected {
        return Err(RenderError::Image(format!(
            "RGBA buffer length mismatch: got {}, expected {}",
            rgba.len(),
            expected
        )));
    }

    if let Some(parent) = path.as_ref().parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let img: ImageBuffer<Rgba<u8>, &[u8]> = ImageBuffer::from_raw(width, height, rgba)
        .ok_or_else(|| RenderError::Image("failed to construct RGBA ImageBuffer".to_owned()))?;
    img.save_with_format(path.as_ref(), image::ImageFormat::Png)
        .map_err(|e| RenderError::Image(e.to_string()))
}

fn create_instance(backends: wgpu::Backends) -> wgpu::Instance {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = backends;
    wgpu::Instance::new(desc)
}

fn select_instance_and_adapter(
    compatible_surface: Option<&wgpu::Surface<'_>>,
    config: &GpuConfig,
) -> Result<(wgpu::Instance, wgpu::Adapter), RenderError> {
    // 1. Primary: Vulkan (NVIDIA RTX 4060 Ti / AMD / Intel)
    let vk_instance = create_instance(wgpu::Backends::VULKAN);
    if let Some(adapter) = request_adapter_for_instance(&vk_instance, compatible_surface, config) {
        return Ok((vk_instance, adapter));
    }

    // 2. Fallback: GLES / OpenGL
    warn!("Vulkan adapter unavailable; falling back to GLES backend");
    let gl_instance = create_instance(wgpu::Backends::GL);
    if let Some(adapter) = request_adapter_for_instance(&gl_instance, compatible_surface, config) {
        return Ok((gl_instance, adapter));
    }

    Err(RenderError::NoAdapter)
}

fn request_adapter_for_instance(
    instance: &wgpu::Instance,
    compatible_surface: Option<&wgpu::Surface<'_>>,
    config: &GpuConfig,
) -> Option<wgpu::Adapter> {
    let opts = wgpu::RequestAdapterOptions {
        power_preference: config.power_preference,
        force_fallback_adapter: config.force_fallback_adapter,
        compatible_surface,
        apply_limit_buckets: false,
    };
    block_on(instance.request_adapter(&opts)).ok()
}

fn request_device(
    adapter: &wgpu::Adapter,
) -> Result<(wgpu::Device, wgpu::Queue, AdapterSummary), RenderError> {
    let info = adapter.get_info();
    let summary = AdapterSummary {
        name: info.name,
        backend: info.backend,
        device_type: info.device_type,
        driver: info.driver,
        driver_info: info.driver_info,
    };
    info!(
        adapter = %summary.name,
        backend = ?summary.backend,
        device_type = ?summary.device_type,
        driver = %summary.driver,
        "initialized wgpu adapter"
    );

    let desc = wgpu::DeviceDescriptor {
        label: Some("omarex-gpu-device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_webgl2_defaults()
            .using_resolution(adapter.limits()),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    };

    let (device, queue) = block_on(adapter.request_device(&desc))
        .map_err(|e| RenderError::RequestDevice(e.to_string()))?;

    Ok((device, queue, summary))
}

fn pick_surface_format(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    caps.formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .or_else(|| caps.formats.first().copied())
        .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb)
}

fn pick_present_mode(
    caps: &wgpu::SurfaceCapabilities,
    pref: PresentModePreference,
) -> wgpu::PresentMode {
    let desired = match pref {
        PresentModePreference::Fifo => wgpu::PresentMode::Fifo,
        PresentModePreference::Mailbox => wgpu::PresentMode::Mailbox,
    };
    if caps.present_modes.contains(&desired) {
        desired
    } else if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
        wgpu::PresentMode::Fifo
    } else {
        caps.present_modes
            .first()
            .copied()
            .unwrap_or(wgpu::PresentMode::Fifo)
    }
}

fn pick_alpha_mode(
    caps: &wgpu::SurfaceCapabilities,
    transparent: bool,
) -> wgpu::CompositeAlphaMode {
    if transparent {
        for mode in [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
        ] {
            if caps.alpha_modes.contains(&mode) {
                return mode;
            }
        }
    }
    if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
        wgpu::CompositeAlphaMode::Opaque
    } else {
        caps.alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto)
    }
}

/// Minimal zero-dependency synchronous future executor using `thread::park` / `unpark`.
struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => thread::park(),
        }
    }
}
