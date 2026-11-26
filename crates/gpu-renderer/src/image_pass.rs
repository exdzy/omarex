//! Generation-keyed GPU texture cache for Kitty Graphics Protocol layers
//! (`BELOW_BACKGROUND`, `BELOW_TEXT`, `ABOVE_TEXT`, z-ordered) and hover image previews,
//! with bounded VRAM LRU eviction.

use std::collections::HashMap;

use crate::context::RenderError;
use crate::draw_list::{ClipRect, ImageQuad, Rect, layers};

/// Kitty Graphics Protocol / image compositing z-layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImageLayer {
    /// Drawn behind terminal cell backgrounds (`z < -1_073_741_824` in Kitty spec).
    BelowBackground,
    /// Drawn above terminal backgrounds but behind terminal text glyphs (`z < 0`).
    BelowText,
    /// Drawn above terminal text glyphs (`z >= 0`).
    AboveText,
}

impl ImageLayer {
    /// Maps a Kitty Graphics Protocol integer `z` value to its compositing layer.
    #[inline]
    pub const fn from_kitty_z(z: i32) -> Self {
        if z < -1_073_741_824 {
            Self::BelowBackground
        } else if z < 0 {
            Self::BelowText
        } else {
            Self::AboveText
        }
    }

    /// Maps this image layer to its corresponding unified [`crate::draw_list::layers`] base Z-index.
    #[inline]
    pub const fn draw_layer(self) -> i32 {
        match self {
            Self::BelowBackground => layers::KITTY_BELOW_BG,
            Self::BelowText => layers::KITTY_BELOW_TEXT,
            Self::AboveText => layers::KITTY_ABOVE_TEXT,
        }
    }
}

/// Source pixel encoding for image uploads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePixelFormat {
    /// Tightly packed sRGB RGBA8 (`width * height * 4` bytes).
    Rgba8,
    /// Tightly packed sRGB RGB8 (`width * height * 3` bytes, alpha = 255).
    Rgb8,
    /// Encoded PNG / JPEG / WebP / GIF bytes decoded via `image`.
    Encoded,
}

/// Descriptor for uploading or updating a cached GPU image texture.
#[derive(Debug, Clone, Copy)]
pub struct ImageUpload<'a> {
    pub id: u64,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub format: ImagePixelFormat,
    pub data: &'a [u8],
}

/// A Kitty Graphics Protocol image placement ready to be emitted into a [`crate::draw_list::DrawList`].
#[derive(Debug, Clone)]
pub struct KittyPlacement {
    pub image_id: u64,
    pub generation: u64,
    pub layer: ImageLayer,
    pub z_index: i32,
    pub dest_rect: Rect,
    pub uv_rect: [f32; 4],
    pub opacity: f32,
    pub corner_radius: f32,
    pub clip_rect: ClipRect,
}

impl KittyPlacement {
    /// Converts this placement into an [`ImageQuad`].
    pub fn to_image_quad(&self) -> ImageQuad {
        ImageQuad {
            image_id: self.image_id,
            bounds: self.dest_rect,
            uv_bounds: self.uv_rect,
            opacity: self.opacity,
            corner_radius: self.corner_radius,
            clip_rect: self.clip_rect,
        }
    }
}

/// Cached GPU texture entry for one image ID.
pub struct CachedImage {
    pub id: u64,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub vram_bytes: usize,
    pub last_used_frame: u64,
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub bind_group: wgpu::BindGroup,
}

/// Generation-keyed GPU texture cache with bounded VRAM LRU eviction.
pub struct ImageTextureCache {
    entries: HashMap<u64, CachedImage>,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    total_vram_bytes: usize,
    max_vram_bytes: usize,
    frame_counter: u64,
    eviction_count: u64,
}

impl ImageTextureCache {
    /// Creates a new image cache with a maximum VRAM budget (clamped to at least 1 MiB).
    pub fn new(device: &wgpu::Device, max_vram_bytes: usize) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("omarex-image-bgl"),
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
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("omarex-image-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            entries: HashMap::new(),
            bind_group_layout,
            sampler,
            total_vram_bytes: 0,
            max_vram_bytes: max_vram_bytes.max(1024 * 1024),
            frame_counter: 0,
            eviction_count: 0,
        }
    }

    pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.bind_group_layout
    }

    /// Advances the internal frame clock used for LRU ordering.
    pub fn begin_frame(&mut self) {
        self.frame_counter = self.frame_counter.saturating_add(1);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_vram_bytes(&self) -> usize {
        self.total_vram_bytes
    }

    pub fn max_vram_bytes(&self) -> usize {
        self.max_vram_bytes
    }

    pub fn eviction_count(&self) -> u64 {
        self.eviction_count
    }

    pub fn set_max_vram_bytes(&mut self, max_bytes: usize) {
        self.max_vram_bytes = max_bytes.max(1024 * 1024);
        self.evict_to_budget(None);
    }

    /// Returns `true` if `id` is already cached at the exact `generation`.
    pub fn has_generation(&self, id: u64, generation: u64) -> bool {
        self.entries
            .get(&id)
            .is_some_and(|entry| entry.generation == generation)
    }

    /// Looks up a cached image and marks it used on the current frame.
    pub fn touch_and_get(&mut self, id: u64) -> Option<&CachedImage> {
        let frame = self.frame_counter;
        let entry = self.entries.get_mut(&id)?;
        entry.last_used_frame = frame;
        Some(entry)
    }

    /// Looks up a cached image without mutating LRU timestamp.
    pub fn get(&self, id: u64) -> Option<&CachedImage> {
        self.entries.get(&id)
    }

    /// Uploads or replaces an image texture if missing or if `generation` differs.
    ///
    /// Returns `Ok(true)` when a GPU texture upload occurred, or `Ok(false)` on a generation hit.
    pub fn upload_or_update(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        upload: ImageUpload<'_>,
    ) -> Result<bool, RenderError> {
        if let Some(existing) = self.entries.get_mut(&upload.id)
            && existing.generation == upload.generation
        {
            existing.last_used_frame = self.frame_counter;
            return Ok(false);
        }

        let (width, height, owned_rgba): (u32, u32, Vec<u8>) = match upload.format {
            ImagePixelFormat::Rgba8 => {
                let expected = checked_byte_len(upload.width, upload.height, 4)?;
                if upload.data.len() != expected {
                    return Err(RenderError::Image(format!(
                        "RGBA8 image {} size mismatch: got {}, expected {}",
                        upload.id,
                        upload.data.len(),
                        expected
                    )));
                }
                (upload.width, upload.height, upload.data.to_vec())
            }
            ImagePixelFormat::Rgb8 => {
                let expected = checked_byte_len(upload.width, upload.height, 3)?;
                if upload.data.len() != expected {
                    return Err(RenderError::Image(format!(
                        "RGB8 image {} size mismatch: got {}, expected {}",
                        upload.id,
                        upload.data.len(),
                        expected
                    )));
                }
                let mut rgba =
                    Vec::with_capacity((upload.width as usize) * (upload.height as usize) * 4);
                for rgb in upload.data.as_chunks::<3>().0 {
                    rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
                (upload.width, upload.height, rgba)
            }
            ImagePixelFormat::Encoded => {
                let decoded = image::load_from_memory(upload.data)
                    .map_err(|e| RenderError::Image(e.to_string()))?
                    .to_rgba8();
                let (w, h) = decoded.dimensions();
                if w == 0 || h == 0 {
                    return Err(RenderError::InvalidDimensions {
                        width: w,
                        height: h,
                    });
                }
                (w, h, decoded.into_raw())
            }
        };

        if width == 0 || height == 0 {
            return Err(RenderError::InvalidDimensions { width, height });
        }

        let vram_bytes = (width as usize) * (height as usize) * 4;

        // Remove previous version of this ID before accounting budget.
        if let Some(prev) = self.entries.remove(&upload.id) {
            self.total_vram_bytes = self.total_vram_bytes.saturating_sub(prev.vram_bytes);
        }

        // Evict LRU entries if needed before allocating the new GPU texture.
        if self.total_vram_bytes.saturating_add(vram_bytes) > self.max_vram_bytes {
            let target_limit = self.max_vram_bytes.saturating_sub(vram_bytes);
            self.evict_down_to(target_limit, Some(upload.id));
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("omarex-kitty-image"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &owned_rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("omarex-kitty-image-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        self.total_vram_bytes = self.total_vram_bytes.saturating_add(vram_bytes);
        self.entries.insert(
            upload.id,
            CachedImage {
                id: upload.id,
                generation: upload.generation,
                width,
                height,
                vram_bytes,
                last_used_frame: self.frame_counter,
                texture,
                view,
                bind_group,
            },
        );

        Ok(true)
    }

    /// Removes a cached image by ID (e.g. on Kitty `a=d` delete command).
    pub fn remove(&mut self, id: u64) -> bool {
        if let Some(prev) = self.entries.remove(&id) {
            self.total_vram_bytes = self.total_vram_bytes.saturating_sub(prev.vram_bytes);
            true
        } else {
            false
        }
    }

    /// Removes all cached images whose ID satisfies `predicate` (e.g. `tr3x` tagged icon IDs
    /// `id >= 0x7000_0000`).
    pub fn retain(&mut self, mut keep: impl FnMut(u64) -> bool) {
        self.entries.retain(|&id, entry| {
            let kept = keep(id);
            if !kept {
                self.total_vram_bytes = self.total_vram_bytes.saturating_sub(entry.vram_bytes);
            }
            kept
        });
    }

    /// Clears all cached textures.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_vram_bytes = 0;
    }

    /// Sorts Kitty placements into exact `(ImageLayer, z_index)` compositing order and pushes
    /// them into the appropriate [`crate::draw_list::DrawList`] layers.
    pub fn emit_placements(
        placements: &mut [KittyPlacement],
        draw_list: &mut crate::draw_list::DrawList,
    ) {
        placements.sort_by_key(|p| (p.layer, p.z_index));
        for placement in placements.iter() {
            let target_layer = placement.layer.draw_layer();
            draw_list.with_layer(target_layer, |dl| {
                dl.push_image(placement.to_image_quad());
            });
        }
    }

    fn evict_to_budget(&mut self, protect_id: Option<u64>) {
        self.evict_down_to(self.max_vram_bytes, protect_id);
    }

    fn evict_down_to(&mut self, target_bytes: usize, protect_id: Option<u64>) {
        while self.total_vram_bytes > target_bytes {
            let victim = self
                .entries
                .iter()
                .filter(|&(&id, _)| Some(id) != protect_id)
                .min_by_key(|&(_, entry)| (entry.last_used_frame, entry.id))
                .map(|(&id, _)| id);

            let Some(victim_id) = victim else {
                break;
            };
            if let Some(removed) = self.entries.remove(&victim_id) {
                self.total_vram_bytes = self.total_vram_bytes.saturating_sub(removed.vram_bytes);
                self.eviction_count = self.eviction_count.saturating_add(1);
            }
        }
    }
}

fn checked_byte_len(width: u32, height: u32, bpp: usize) -> Result<usize, RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::InvalidDimensions { width, height });
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(bpp))
        .ok_or(RenderError::InvalidDimensions { width, height })
}
