//! Safe Kitty Graphics Protocol placement iterator and pixel reader.
//!
//! Reads placements from `GhosttyRenderState` (`GHOSTTY_RENDER_STATE_DATA_IMAGE_ITERATOR`)
//! and decodes/reads raw image pixels from `GhosttyKittyGraphics` so `gpu-renderer::image_pass`
//! can cache GPU textures by `(image_id, generation)`.

use std::ffi::c_void;
use std::ptr;

use crate::{Terminal, VtError, ffi};

/// Z-layer for drawing a Kitty graphics placement relative to terminal backgrounds and text.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KittyImageLayer {
    /// Drawn below terminal cell backgrounds (`z < i32::MIN / 2`).
    BelowBackground,
    /// Drawn above cell backgrounds and below text (`i32::MIN / 2 <= z < 0`, or virtual placements).
    BelowText,
    /// Drawn above terminal text (`z >= 0`).
    AboveText,
}

impl KittyImageLayer {
    pub(crate) fn from_ffi(raw: ffi::GhosttyRenderStateImageLayer) -> Self {
        match raw {
            ffi::GHOSTTY_RENDER_STATE_IMAGE_LAYER_BELOW_BACKGROUND => Self::BelowBackground,
            ffi::GHOSTTY_RENDER_STATE_IMAGE_LAYER_BELOW_TEXT => Self::BelowText,
            _ => Self::AboveText,
        }
    }
}

/// Kind of Kitty graphics placement resolved by the render state.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum KittyPlacementKind {
    /// Unicode placeholder (`U+10EEEE`) virtual placement.
    Virtual,
    /// Positioned relative to another placement.
    Relative,
    /// Positioned at a terminal cell.
    Fixed,
}

impl KittyPlacementKind {
    pub(crate) fn from_ffi(raw: ffi::GhosttyRenderStateKittyPlacementKind) -> Self {
        match raw {
            ffi::GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_VIRTUAL => Self::Virtual,
            ffi::GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_RELATIVE => Self::Relative,
            _ => Self::Fixed,
        }
    }
}

/// Rectangle in source image pixel coordinates.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A resolved Kitty graphics image placement ready for GPU rendering.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct KittyPlacement {
    pub image_id: u32,
    pub generation: u64,
    pub placement_id: u32,
    pub kind: KittyPlacementKind,
    pub z_layer: KittyImageLayer,
    pub z_index: i32,
    /// Top-left column relative to the viewport (may be negative for clipped fixed placements).
    pub viewport_col: i32,
    /// Top-left row relative to the viewport (may be negative for overscan/scrolled placements).
    pub viewport_row: i32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub cell_offset_x: u32,
    pub cell_offset_y: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub src_rect: PixelRect,
}

/// Pixel format stored inside `libghostty-vt` Kitty graphics storage.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum KittyPixelFormat {
    Rgb,
    Rgba,
    GrayAlpha,
    Gray,
}

/// Owned RGBA8 pixel data for a Kitty graphics image, ready for `wgpu::Queue::write_texture`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImagePixels {
    pub image_id: u32,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub source_format: KittyPixelFormat,
    /// Tightly packed RGBA8 (`width * height * 4` bytes).
    pub rgba8: Vec<u8>,
}

/// Populates `out` with every visible (and overscan) Kitty graphics placement from `iter`.
///
/// # Safety
/// `render_state` and `iter` must be valid, non-null handles, and `render_state` must
/// have completed `end_update()` without subsequent mutation.
pub(crate) unsafe fn collect_render_state_placements(
    render_state: ffi::GhosttyRenderState,
    iter: ffi::GhosttyRenderStateImageIterator,
    out: &mut Vec<KittyPlacement>,
) -> Result<(), VtError> {
    out.clear();

    // SAFETY: Caller guarantees valid `render_state` and pre-allocated `iter` handles.
    let mut iter_handle = iter;
    let rc = unsafe {
        ffi::ghostty_render_state_get(
            render_state,
            ffi::GHOSTTY_RENDER_STATE_DATA_IMAGE_ITERATOR,
            (&raw mut iter_handle).cast::<c_void>(),
        )
    };
    if rc == ffi::GHOSTTY_NO_VALUE {
        return Ok(());
    }
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }

    const KEYS: [ffi::GhosttyRenderStateImageData; 16] = [
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_ID,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_GENERATION,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_LAYER,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_Z,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_COL,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_ROW,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_GRID_COLS,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_GRID_ROWS,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_X,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_Y,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_WIDTH,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_HEIGHT,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_X,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_Y,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_WIDTH,
        ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_HEIGHT,
    ];

    // SAFETY: `iter` was just populated from `render_state` and lives for this loop.
    while unsafe { ffi::ghostty_render_state_image_iterator_next(iter) } {
        let mut image_id: u32 = 0;
        let mut generation: u64 = 0;
        let mut layer: ffi::GhosttyRenderStateImageLayer = 0;
        let mut z_index: i32 = 0;
        let mut viewport_col: i32 = 0;
        let mut viewport_row: i32 = 0;
        let mut grid_cols: u32 = 0;
        let mut grid_rows: u32 = 0;
        let mut cell_offset_x: u32 = 0;
        let mut cell_offset_y: u32 = 0;
        let mut pixel_width: u32 = 0;
        let mut pixel_height: u32 = 0;
        let mut source_x: u32 = 0;
        let mut source_y: u32 = 0;
        let mut source_width: u32 = 0;
        let mut source_height: u32 = 0;

        let mut values: [*mut c_void; 16] = [
            (&raw mut image_id).cast(),
            (&raw mut generation).cast(),
            (&raw mut layer).cast(),
            (&raw mut z_index).cast(),
            (&raw mut viewport_col).cast(),
            (&raw mut viewport_row).cast(),
            (&raw mut grid_cols).cast(),
            (&raw mut grid_rows).cast(),
            (&raw mut cell_offset_x).cast(),
            (&raw mut cell_offset_y).cast(),
            (&raw mut pixel_width).cast(),
            (&raw mut pixel_height).cast(),
            (&raw mut source_x).cast(),
            (&raw mut source_y).cast(),
            (&raw mut source_width).cast(),
            (&raw mut source_height).cast(),
        ];

        // SAFETY: Every pointer in `values` matches the documented output type of `KEYS[i]`.
        let rc = unsafe {
            ffi::ghostty_render_state_image_get_multi(
                iter,
                KEYS.len(),
                KEYS.as_ptr(),
                values.as_mut_ptr(),
                ptr::null_mut(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let mut placement_id: u32 = 0;
        let mut kitty_kind: ffi::GhosttyRenderStateKittyPlacementKind =
            ffi::GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_FIXED;
        // SAFETY: Kitty-specific fields return GHOSTTY_NO_VALUE for non-Kitty protocols.
        let _ = unsafe {
            ffi::ghostty_render_state_image_get(
                iter,
                ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_KITTY_PLACEMENT_ID,
                (&raw mut placement_id).cast(),
            )
        };
        let _ = unsafe {
            ffi::ghostty_render_state_image_get(
                iter,
                ffi::GHOSTTY_RENDER_STATE_IMAGE_DATA_KITTY_KIND,
                (&raw mut kitty_kind).cast(),
            )
        };

        out.push(KittyPlacement {
            image_id,
            generation,
            placement_id,
            kind: KittyPlacementKind::from_ffi(kitty_kind),
            z_layer: KittyImageLayer::from_ffi(layer),
            z_index,
            viewport_col,
            viewport_row,
            grid_cols,
            grid_rows,
            cell_offset_x,
            cell_offset_y,
            pixel_width,
            pixel_height,
            src_rect: PixelRect {
                x: source_x,
                y: source_y,
                width: source_width,
                height: source_height,
            },
        });
    }

    Ok(())
}

/// Reads decoded RGBA8 pixel data for `image_id` from the terminal's active screen storage.
///
/// Returns `Ok(None)` if the image does not exist, Kitty graphics are disabled, or
/// its pixel payload is still pending (e.g., streaming snapshot before image payload arrived).
pub fn read_kitty_image(
    terminal: &Terminal,
    image_id: u32,
) -> Result<Option<KittyImagePixels>, VtError> {
    let guard = terminal.inner.lock();

    let mut graphics: ffi::GhosttyKittyGraphics = ptr::null_mut();
    // SAFETY: `guard.raw` is a live `GhosttyTerminal`; `graphics` borrows from it under the lock.
    let rc = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS,
            (&raw mut graphics).cast(),
        )
    };
    if rc == ffi::GHOSTTY_NO_VALUE || graphics.is_null() {
        return Ok(None);
    }
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }

    // SAFETY: `graphics` is borrowed from `guard.raw`, which is locked and unmutated here.
    let image = unsafe { ffi::ghostty_kitty_graphics_image(graphics, image_id) };
    if image.is_null() {
        return Ok(None);
    }

    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut format: ffi::GhosttyKittyImageFormat = ffi::GHOSTTY_KITTY_IMAGE_FORMAT_RGBA;
    let mut generation: u64 = 0;
    let mut data_ptr: *const u8 = ptr::null();
    let mut data_len: usize = 0;

    // SAFETY: Output pointers match `kitty_graphics.h` types while `guard` holds the lock.
    unsafe {
        let rc = ffi::ghostty_kitty_graphics_image_get(
            image,
            ffi::GHOSTTY_KITTY_IMAGE_DATA_DATA_PTR,
            (&raw mut data_ptr).cast(),
        );
        if rc == ffi::GHOSTTY_NO_VALUE {
            // Image metadata is present (e.g. snapshot READY) but pixels are still pending.
            return Ok(None);
        }
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        for (key, out) in [
            (
                ffi::GHOSTTY_KITTY_IMAGE_DATA_WIDTH,
                (&raw mut width).cast::<c_void>(),
            ),
            (
                ffi::GHOSTTY_KITTY_IMAGE_DATA_HEIGHT,
                (&raw mut height).cast::<c_void>(),
            ),
            (
                ffi::GHOSTTY_KITTY_IMAGE_DATA_FORMAT,
                (&raw mut format).cast::<c_void>(),
            ),
            (
                ffi::GHOSTTY_KITTY_IMAGE_DATA_GENERATION,
                (&raw mut generation).cast::<c_void>(),
            ),
            (
                ffi::GHOSTTY_KITTY_IMAGE_DATA_DATA_LEN,
                (&raw mut data_len).cast::<c_void>(),
            ),
        ] {
            let r = ffi::ghostty_kitty_graphics_image_get(image, key, out);
            if r != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(r));
            }
        }
    }

    if data_ptr.is_null() || data_len == 0 || width == 0 || height == 0 {
        return Ok(None);
    }

    // SAFETY: `data_ptr` is valid for `data_len` bytes while `guard` holds the terminal lock.
    let raw = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };

    let (source_format, rgba8) = match format {
        ffi::GHOSTTY_KITTY_IMAGE_FORMAT_RGBA => (KittyPixelFormat::Rgba, raw.to_vec()),
        ffi::GHOSTTY_KITTY_IMAGE_FORMAT_RGB => {
            let mut buf = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for &[r, g, b] in raw.as_chunks::<3>().0 {
                buf.extend_from_slice(&[r, g, b, 255]);
            }
            (KittyPixelFormat::Rgb, buf)
        }
        ffi::GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA => {
            let mut buf = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for &[g, a] in raw.as_chunks::<2>().0 {
                buf.extend_from_slice(&[g, g, g, a]);
            }
            (KittyPixelFormat::GrayAlpha, buf)
        }
        ffi::GHOSTTY_KITTY_IMAGE_FORMAT_GRAY => {
            let mut buf = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for &g in raw {
                buf.extend_from_slice(&[g, g, g, 255]);
            }
            (KittyPixelFormat::Gray, buf)
        }
        _ => return Err(VtError::Ffi(ffi::GHOSTTY_INVALID_VALUE)),
    };

    Ok(Some(KittyImagePixels {
        image_id,
        generation,
        width,
        height,
        source_format,
        rgba8,
    }))
}

/// Advances any playing Kitty graphics animations to `now_ms` (monotonic ms clock) and
/// returns the timestamp of the next frame if any animation is active.
pub fn tick_kitty_animations(terminal: &Terminal, now_ms: u64) -> Result<Option<u64>, VtError> {
    let guard = terminal.inner.lock();
    let mut graphics: ffi::GhosttyKittyGraphics = ptr::null_mut();
    // SAFETY: `guard.raw` is a live locked `GhosttyTerminal`.
    let rc = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS,
            (&raw mut graphics).cast(),
        )
    };
    if rc == ffi::GHOSTTY_NO_VALUE || graphics.is_null() {
        return Ok(None);
    }
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }

    let mut next_ms: u64 = 0;
    // SAFETY: `graphics` is valid while `guard` holds the terminal lock.
    let rc =
        unsafe { ffi::ghostty_kitty_graphics_animation_tick(graphics, now_ms, &raw mut next_ms) };
    match rc {
        ffi::GHOSTTY_SUCCESS => Ok(Some(next_ms)),
        ffi::GHOSTTY_NO_VALUE => Ok(None),
        other => Err(VtError::Ffi(other)),
    }
}
