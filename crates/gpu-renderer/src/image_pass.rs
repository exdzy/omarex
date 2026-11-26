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

        // Fast path: reuse existing GPU texture & bind group in-place when dimensions match
        // (avoids per-frame Vulkan texture/bind-group reallocation for video & animated GIFs).
        if let Some(existing) = self.entries.get_mut(&upload.id)
            && existing.width == width
            && existing.height == height
        {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &existing.texture,
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
            existing.generation = upload.generation;
            existing.last_used_frame = self.frame_counter;
            return Ok(true);
        }

        self.insert_rgba8_texture(
            device,
            queue,
            upload.id,
            upload.generation,
            width,
            height,
            &owned_rgba,
        )?;
        Ok(true)
    }

    /// Uploads or replaces a dynamic RGBA8 frame (e.g. viewer image, animated GIF, or video frame)
    /// directly from a borrowed slice without an intermediate `Vec<u8>` copy.
    ///
    /// Reuses the existing GPU texture in-place via `queue.write_texture` when `(width, height)`
    /// matches the currently cached texture for `id`.
    /// Returns `Ok(true)` if an upload occurred, or `Ok(false)` on a generation hit.
    #[allow(clippy::too_many_arguments)]
    pub fn upload_rgba8_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: u64,
        generation: u64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<bool, RenderError> {
        if let Some(existing) = self.entries.get_mut(&id)
            && existing.generation == generation
        {
            existing.last_used_frame = self.frame_counter;
            return Ok(false);
        }

        let expected = checked_byte_len(width, height, 4)?;
        if rgba.len() != expected {
            return Err(RenderError::Image(format!(
                "RGBA8 frame {id} size mismatch: got {}, expected {expected}",
                rgba.len()
            )));
        }

        if let Some(existing) = self.entries.get_mut(&id)
            && existing.width == width
            && existing.height == height
        {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &existing.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba,
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
            existing.generation = generation;
            existing.last_used_frame = self.frame_counter;
            return Ok(true);
        }

        self.insert_rgba8_texture(device, queue, id, generation, width, height, rgba)?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_rgba8_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: u64,
        generation: u64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), RenderError> {
        let vram_bytes = (width as usize) * (height as usize) * 4;

        // Remove previous version of this ID before accounting budget.
        if let Some(prev) = self.entries.remove(&id) {
            self.total_vram_bytes = self.total_vram_bytes.saturating_sub(prev.vram_bytes);
        }

        // Evict LRU entries if needed before allocating the new GPU texture.
        if self.total_vram_bytes.saturating_add(vram_bytes) > self.max_vram_bytes {
            let target_limit = self.max_vram_bytes.saturating_sub(vram_bytes);
            self.evict_down_to(target_limit, Some(id));
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
            rgba,
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
            id,
            CachedImage {
                id,
                generation,
                width,
                height,
                vram_bytes,
                last_used_frame: self.frame_counter,
                texture,
                view,
                bind_group,
            },
        );

        Ok(())
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

/// Rasterizes an SVG document into straight-alpha sRGB RGBA8 pixels using `resvg` + `tiny-skia`.
///
/// Preserves the SVG's natural aspect ratio and dimensions, downscaling proportionally only when
/// either side exceeds `max_dim` (e.g. `8192`). Returns `(width, height, rgba_bytes, orig_w, orig_h)`.
pub fn rasterize_svg_rgba8(
    svg_bytes: &[u8],
    max_dim: u32,
) -> Result<(u32, u32, Vec<u8>, u32, u32), RenderError> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(svg_bytes, &opt)
        .map_err(|e| RenderError::Svg(e.to_string()))?;

    let size = tree.size();
    let svg_w = size.width().max(1.0);
    let svg_h = size.height().max(1.0);
    let orig_w = svg_w.round().clamp(1.0, u32::MAX as f32) as u32;
    let orig_h = svg_h.round().clamp(1.0, u32::MAX as f32) as u32;

    let cap = max_dim.max(1) as f32;
    let longest = svg_w.max(svg_h);
    let scale = if longest > cap { cap / longest } else { 1.0 };

    let px_w = (svg_w * scale).round().clamp(1.0, cap) as u32;
    let px_h = (svg_h * scale).round().clamp(1.0, cap) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(px_w, px_h).ok_or(RenderError::InvalidDimensions {
        width: px_w,
        height: px_h,
    })?;

    let sx = px_w as f32 / svg_w;
    let sy = px_h as f32 / svg_h;
    let transform = tiny_skia::Transform::from_scale(sx, sy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny-skia produces premultiplied sRGB RGBA8; unpremultiply into straight sRGB RGBA8
    // to match Rgba8UnormSrgb sampling + ALPHA_BLENDING.
    let raw = pixmap.data();
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

    Ok((px_w, px_h, rgba, orig_w, orig_h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterize_svg_rgba8_preserves_aspect_and_caps_max_dim() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100" viewBox="0 0 200 100">
            <rect x="0" y="0" width="200" height="100" fill="#1080ff"/>
        </svg>"##;

        let (w, h, rgba, ow, oh) = rasterize_svg_rgba8(svg, 8192).expect("rasterize svg");
        assert_eq!((w, h, ow, oh), (200, 100, 200, 100));
        assert_eq!(rgba.len(), 200 * 100 * 4);
        let mid = ((50 * 200) + 100) * 4;
        assert_eq!(&rgba[mid..mid + 4], &[0x10, 0x80, 0xff, 0xff]);

        let (cw, ch, capped, cow, coh) = rasterize_svg_rgba8(svg, 64).expect("rasterize capped");
        assert_eq!((cw, ch, cow, coh), (64, 32, 200, 100));
        assert_eq!(capped.len(), 64 * 32 * 4);
    }
}
