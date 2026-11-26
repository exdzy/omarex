//! Async local & remote File / Image / SVG / Video loading, decoding, and playback backend
//! for OmaRex's right-hand docked viewer pane.
//!
//! Designed for non-blocking integration with the main `winit` event loop:
//! - Background threads handle all disk I/O, text loading, image/SVG decoding, `ffprobe` metadata
//!   extraction, first-frame poster extraction, and streaming `ffmpeg` video playback.
//! - [`MediaViewerBackend::poll_events`] drains ready [`MediaEvent`]s without blocking.
//! - [`MediaViewerBackend::next_wakeup_instant`] computes the exact next frame deadline so `winit`
//!   only wakes and redraws when a video frame or animated GIF/WebP frame is due.
//! - Every spawned `ffmpeg`/`ffplay`/`pw-cat`/`paplay` child process is strictly tracked, killed,
//!   and reaped on pause, seek, mute, tab close, and `Drop` (never leaks zombies).

#![allow(dead_code)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use serde::Deserialize;
use tracing::{debug, warn};

/// Unique identifier for an open viewer tab managed by [`MediaViewerBackend`].
pub type ViewerTabId = u64;

/// High-bit namespace for viewer GPU textures so they never collide with Kitty image IDs
/// or `tr3x` icons (`0x7000_0000`).
pub const VIEWER_TEXTURE_KEY_BASE: u64 = 0x8000_0000_0000_0000;

/// Maximum bytes read when loading a text/code file (4 MiB).
pub const MAX_TEXT_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum bytes read into RAM for a raster image or SVG file (128 MiB).
pub const MAX_IMAGE_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// Maximum pixel dimension for decoded static/animated images and SVGs before crisp downscaling.
/// Preserves full resolution up to 8K for crisp zoom-in.
pub const MAX_IMAGE_DIMENSION: u32 = 8192;

/// Maximum decoded video frame dimensions for smooth 30-60fps GPU uploads.
pub const MAX_VIDEO_WIDTH: u32 = 1920;
pub const MAX_VIDEO_HEIGHT: u32 = 1080;

/// Safety cap on decoded animation frames for animated GIF/WebP files.
pub const MAX_ANIMATED_FRAMES: usize = 512;

/// Minimum delay enforced for animated GIF/WebP frames when the file specifies 0ms.
const MIN_ANIMATION_FRAME_DELAY: Duration = Duration::from_millis(20);

/// Maximum video playback frame rate for raw RGBA streaming.
const MAX_VIDEO_PLAYBACK_FPS: f64 = 60.0;

/// Polling wakeup cadence while background one-shot loads are pending without a custom waker.
const PENDING_LOAD_WAKEUP_INTERVAL: Duration = Duration::from_millis(16);

/// Maps a [`ViewerTabId`] into its dedicated [`gpu_renderer::ImageTextureCache`] key namespace.
#[inline]
pub const fn texture_key_for_tab(tab_id: ViewerTabId) -> u64 {
    VIEWER_TEXTURE_KEY_BASE | (tab_id & 0x7FFF_FFFF_FFFF_FFFF)
}

/// Requested media loading mode for a viewer tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// Infer kind from file extension (and magic bytes on fallback).
    Auto,
    /// Load as UTF-8/lossy text (with binary detection).
    Text,
    /// Decode as static/animated raster image (PNG, JPEG, WebP, GIF, BMP) or SVG.
    Image,
    /// Probe metadata, extract first-frame poster, and prepare playback via `ffprobe`/`ffmpeg`.
    Video,
}

impl MediaKind {
    /// Classifies a path into [`MediaKind::Image`], [`MediaKind::Video`], or [`MediaKind::Text`]
    /// based on its extension.
    pub fn from_path(path: &Path) -> Self {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return Self::Text;
        };
        match ext.to_ascii_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" => Self::Image,
            "mp4" | "m4v" | "webm" | "mov" | "mkv" | "avi" | "ogv" | "flv" | "wmv" | "ts"
            | "mts" => Self::Video,
            _ => Self::Text,
        }
    }

    /// Resolves [`MediaKind::Auto`] against `path`.
    pub fn resolve(self, path: &Path) -> Self {
        match self {
            Self::Auto => Self::from_path(path),
            other => other,
        }
    }
}

/// Tightly packed straight-alpha sRGB RGBA8 pixel buffer (`width * height * 4` bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl RgbaFrame {
    /// Expected byte length (`width * height * 4`).
    #[inline]
    pub fn expected_len(width: u32, height: u32) -> Option<usize> {
        (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
    }
}

/// One decoded frame of a static or animated image with its presentation delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimatedFrame {
    pub frame: RgbaFrame,
    pub delay: Duration,
}

/// Loaded text/code file content and metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextContent {
    /// Human-readable display path (remote path for remote files, local path otherwise).
    pub display_path: String,
    /// Local path on disk that was read.
    pub local_path: PathBuf,
    /// Decoded lines (empty if the file is empty; compact hex preview if `is_binary`).
    pub lines: Vec<String>,
    /// Total file size on disk in bytes.
    pub file_size_bytes: u64,
    /// File modification timestamp (seconds since UNIX epoch), if available.
    pub modified_unix_secs: Option<u64>,
    /// `true` when the file exceeded [`MAX_TEXT_FILE_BYTES`] and was capped.
    pub truncated: bool,
    /// `true` when binary content was detected in the file header.
    pub is_binary: bool,
    /// `true` when invalid UTF-8 sequences were replaced with U+FFFD.
    pub lossy_utf8: bool,
}

/// Metadata returned for a decoded raster image or SVG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageMetadata {
    /// Decoded frame width in pixels (capped at [`MAX_IMAGE_DIMENSION`]).
    pub width: u32,
    /// Decoded frame height in pixels (capped at [`MAX_IMAGE_DIMENSION`]).
    pub height: u32,
    /// Original source width prior to any >8K downscaling.
    pub original_width: u32,
    /// Original source height prior to any >8K downscaling.
    pub original_height: u32,
    /// Human-readable format label (e.g. `"PNG"`, `"JPEG"`, `"WebP"`, `"GIF (Animated)"`, `"SVG"`).
    pub format_label: String,
    /// Source file size in bytes.
    pub file_size_bytes: u64,
    /// Number of decoded frames (`1` for static images/SVGs, `>1` for animated GIF/WebP).
    pub frame_count: usize,
    /// `true` when `frame_count > 1`.
    pub is_animated: bool,
}

/// Complete decoded image output (1 frame for static/SVG, N frames for animated GIF/WebP).
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub metadata: ImageMetadata,
    pub frames: Vec<AnimatedFrame>,
}

/// Metadata extracted via `ffprobe` for a video file.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoMetadata {
    pub display_path: String,
    pub local_path: PathBuf,
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    /// Output width for GPU streaming (even, capped to `MAX_VIDEO_WIDTH x MAX_VIDEO_HEIGHT`).
    pub render_width: u32,
    /// Output height for GPU streaming (even, capped to `MAX_VIDEO_WIDTH x MAX_VIDEO_HEIGHT`).
    pub render_height: u32,
    pub fps: f64,
    pub video_codec: String,
    pub audio_codec: Option<String>,
    pub has_audio: bool,
    pub file_size_bytes: u64,
}

/// Snapshot of a video tab's playback transport state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoPlaybackState {
    pub playing: bool,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub muted: bool,
    pub looping: bool,
    pub ended: bool,
    pub audio_active: bool,
}

/// Non-blocking events delivered by [`MediaViewerBackend::poll_events`].
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// A text/code file finished loading.
    TextLoaded {
        tab_id: ViewerTabId,
        content: TextContent,
    },
    /// A static or animated image/SVG finished decoding (frame 0 ready for immediate GPU upload).
    ImageLoaded {
        tab_id: ViewerTabId,
        texture_key: u64,
        generation: u64,
        display_path: String,
        local_path: PathBuf,
        metadata: ImageMetadata,
        frame: RgbaFrame,
        frame_durations: Vec<Duration>,
        texture_dirty: bool,
    },
    /// Next frame of an animated GIF/WebP is ready for GPU upload.
    AnimationFrame {
        tab_id: ViewerTabId,
        texture_key: u64,
        generation: u64,
        frame_index: usize,
        frame_count: usize,
        frame: RgbaFrame,
        texture_dirty: bool,
    },
    /// Video metadata was probed and optional first-frame poster RGBA8 was extracted.
    VideoLoaded {
        tab_id: ViewerTabId,
        texture_key: u64,
        generation: u64,
        metadata: VideoMetadata,
        poster_frame: Option<RgbaFrame>,
        state: VideoPlaybackState,
        texture_dirty: bool,
    },
    /// A newly decoded video frame (from playback or seek) is ready for GPU upload.
    VideoFrame {
        tab_id: ViewerTabId,
        texture_key: u64,
        generation: u64,
        position_secs: f64,
        frame: RgbaFrame,
        state: VideoPlaybackState,
        texture_dirty: bool,
    },
    /// Video transport state changed (e.g. paused, resumed, muted, looped, or reached EOF).
    VideoStateChanged {
        tab_id: ViewerTabId,
        state: VideoPlaybackState,
    },
    /// Loading or decoding failed for `tab_id`.
    LoadError {
        tab_id: ViewerTabId,
        display_path: String,
        error: String,
    },
}

// ============================================================================
// Remote Cache Helpers (`~/.cache/omarex/viewer-remote/`)
// ============================================================================

/// Returns the viewer remote cache directory (`~/.cache/omarex/viewer-remote/`).
pub fn viewer_remote_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("omarex")
        .join("viewer-remote")
}

/// Ensures [`viewer_remote_cache_dir`] exists on disk and returns its path.
pub fn ensure_viewer_remote_cache_dir() -> io::Result<PathBuf> {
    let dir = viewer_remote_cache_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Computes a deterministic, collision-resistant local cache path for a remote file while
/// preserving its filename and extension.
pub fn remote_cache_path_for(host_id: &str, remote_path: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    host_id.hash(&mut hasher);
    0u8.hash(&mut hasher);
    remote_path.hash(&mut hasher);
    let key_hash = hasher.finish();

    let raw_name = Path::new(remote_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let sanitized: String = raw_name
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .take(96)
        .collect();
    let final_name = if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "file".to_owned()
    } else {
        sanitized
    };

    viewer_remote_cache_dir().join(format!("{key_hash:016x}_{final_name}"))
}

/// Atomically writes downloaded remote file bytes into `~/.cache/omarex/viewer-remote/`
/// and returns the resulting local path.
pub fn cache_remote_bytes(host_id: &str, remote_path: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let dir = ensure_viewer_remote_cache_dir()?;
    let target = remote_cache_path_for(host_id, remote_path);
    let tmp_name = format!(
        ".tmp_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp_path = dir.join(tmp_name);

    {
        let mut f = File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.flush()?;
    }

    if let Err(err) = fs::rename(&tmp_path, &target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(target)
}

// ============================================================================
// Text & Binary File Loader
// ============================================================================

/// Returns `true` if `sample` looks like binary data rather than UTF-8/ASCII text.
pub fn is_likely_binary(sample: &[u8]) -> bool {
    if sample.is_empty() {
        return false;
    }
    let slice = &sample[..sample.len().min(8192)];

    // NUL byte is a definitive binary indicator for source/text files.
    if slice.contains(&0) {
        return true;
    }

    // Trim up to 3 trailing bytes in case the 8 KiB slice split a valid UTF-8 codepoint.
    let mut utf8_end = slice.len();
    for _ in 0..3 {
        if std::str::from_utf8(&slice[..utf8_end]).is_ok() {
            break;
        }
        if utf8_end == 0 {
            break;
        }
        utf8_end -= 1;
    }

    let check_slice = &slice[..utf8_end];
    if check_slice.is_empty() {
        return false;
    }

    // Check ratio of invalid UTF-8 replacement chars if non-UTF-8.
    if std::str::from_utf8(check_slice).is_err() {
        let lossy = String::from_utf8_lossy(check_slice);
        let total_chars = lossy.chars().count().max(1);
        let replacements = lossy.chars().filter(|&c| c == '\u{FFFD}').count();
        if replacements * 10 > total_chars {
            return true;
        }
    }

    // Count unexpected ASCII control characters (allowing \t, \n, \r, \x0c, \x1b ESC).
    let ctrl_count = check_slice
        .iter()
        .filter(|&&b| b < 0x08 || (0x0E..=0x1A).contains(&b) || (0x1C..=0x1F).contains(&b))
        .count();
    ctrl_count * 100 > check_slice.len() * 15
}

/// Loads a local text file synchronously (intended to be called on a background thread),
/// enforcing [`MAX_TEXT_FILE_BYTES`], binary detection, UTF-8 lossy fallback, and line splitting.
pub fn load_text_file(path: &Path, display_path: Option<&str>) -> Result<TextContent, String> {
    let meta = fs::metadata(path).map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("not a regular file: {}", path.display()));
    }

    let file_size_bytes = meta.len();
    let modified_unix_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    let file = File::open(path).map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let cap = MAX_TEXT_FILE_BYTES as usize;
    let mut buf = Vec::with_capacity((file_size_bytes as usize).min(cap));
    let mut limited = file.take(MAX_TEXT_FILE_BYTES + 1);
    limited
        .read_to_end(&mut buf)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

    let truncated = buf.len() as u64 > MAX_TEXT_FILE_BYTES || file_size_bytes > MAX_TEXT_FILE_BYTES;
    if buf.len() > cap {
        buf.truncate(cap);
    }

    let is_binary = is_likely_binary(&buf);
    let (lines, lossy_utf8) = if is_binary {
        (format_binary_hex_preview(&buf, file_size_bytes), false)
    } else if buf.is_empty() {
        (Vec::new(), false)
    } else {
        let (text, lossy) = match std::str::from_utf8(&buf) {
            Ok(s) => (Cow::Borrowed(s), false),
            Err(_) => (String::from_utf8_lossy(&buf), true),
        };
        let split = text
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
            .collect();
        (split, lossy)
    };

    Ok(TextContent {
        display_path: display_path
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| path.display().to_string()),
        local_path: path.to_path_buf(),
        lines,
        file_size_bytes,
        modified_unix_secs,
        truncated,
        is_binary,
        lossy_utf8,
    })
}

fn format_binary_hex_preview(bytes: &[u8], total_size: u64) -> Vec<String> {
    const PREVIEW_BYTES: usize = 2048;
    let slice = &bytes[..bytes.len().min(PREVIEW_BYTES)];
    let mut lines = Vec::with_capacity(slice.len().div_ceil(16) + 2);
    lines.push(format!(
        "Binary file ({total_size} bytes) — showing first {} bytes:",
        slice.len()
    ));
    for (row_idx, chunk) in slice.chunks(16).enumerate() {
        let offset = row_idx * 16;
        let mut hex = String::with_capacity(48);
        let mut ascii = String::with_capacity(16);
        for i in 0..16 {
            if let Some(&b) = chunk.get(i) {
                use std::fmt::Write as _;
                let _ = write!(hex, "{b:02x} ");
                let ch = if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                };
                ascii.push(ch);
            } else {
                hex.push_str("   ");
            }
            if i == 7 {
                hex.push(' ');
            }
        }
        lines.push(format!("{offset:08x}  {hex} |{ascii}|"));
    }
    lines
}

// ============================================================================
// Image & SVG Decoder (PNG, JPEG, WebP, GIF incl. animated, BMP, SVG)
// ============================================================================

/// Reads and decodes a local image or SVG file into RGBA8 frame(s) and metadata.
pub fn decode_image_file(path: &Path) -> Result<DecodedImage, String> {
    let meta = fs::metadata(path).map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("not a regular file: {}", path.display()));
    }
    let file_size_bytes = meta.len();
    if file_size_bytes > MAX_IMAGE_FILE_BYTES {
        return Err(format!(
            "image file too large ({file_size_bytes} bytes > {MAX_IMAGE_FILE_BYTES} byte limit)"
        ));
    }
    let bytes = fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let hint_ext = path.extension().and_then(|e| e.to_str());
    decode_image_bytes(&bytes, hint_ext, file_size_bytes)
}

/// Decodes in-memory image or SVG bytes into RGBA8 frame(s) and metadata.
pub fn decode_image_bytes(
    bytes: &[u8],
    hint_ext: Option<&str>,
    file_size_bytes: u64,
) -> Result<DecodedImage, String> {
    if bytes.is_empty() {
        return Err("empty image data".to_owned());
    }

    if is_svg_payload(bytes, hint_ext) {
        let (width, height, rgba, original_width, original_height) =
            gpu_renderer::image_pass::rasterize_svg_rgba8(bytes, MAX_IMAGE_DIMENSION)
                .map_err(|e| format!("SVG rasterization failed: {e}"))?;
        return Ok(DecodedImage {
            metadata: ImageMetadata {
                width,
                height,
                original_width,
                original_height,
                format_label: "SVG".to_owned(),
                file_size_bytes,
                frame_count: 1,
                is_animated: false,
            },
            frames: vec![AnimatedFrame {
                frame: RgbaFrame {
                    width,
                    height,
                    rgba,
                },
                delay: Duration::ZERO,
            }],
        });
    }

    let guessed = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("failed to inspect image header: {e}"))?;
    let format = guessed
        .format()
        .or_else(|| hint_ext.and_then(ImageFormat::from_extension));

    match format {
        Some(ImageFormat::Gif) => decode_gif_bytes(bytes, file_size_bytes),
        Some(ImageFormat::WebP) => decode_webp_bytes(bytes, file_size_bytes),
        _ => decode_static_raster_bytes(bytes, format, file_size_bytes),
    }
}

fn is_svg_payload(bytes: &[u8], hint_ext: Option<&str>) -> bool {
    if hint_ext.is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
        return true;
    }
    let head_len = bytes.len().min(512);
    let Ok(head) = std::str::from_utf8(&bytes[..head_len]) else {
        return false;
    };
    let trimmed = head.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}');
    trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && trimmed.contains("<svg"))
}

fn decode_gif_bytes(bytes: &[u8], file_size_bytes: u64) -> Result<DecodedImage, String> {
    let decoder =
        GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("GIF decode failed: {e}"))?;
    let (original_width, original_height) = decoder.dimensions();
    let raw_frames = decoder
        .into_frames()
        .take(MAX_ANIMATED_FRAMES)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("GIF frame decode failed: {e}"))?;

    if raw_frames.is_empty() {
        return Err("GIF contains no frames".to_owned());
    }

    let mut frames = Vec::with_capacity(raw_frames.len());
    let mut final_w = 0u32;
    let mut final_h = 0u32;
    for raw in raw_frames {
        let delay = normalize_animation_delay(raw.delay());
        let img = DynamicImage::ImageRgba8(raw.into_buffer());
        let frame = dynamic_image_to_rgba_frame(img);
        final_w = frame.width;
        final_h = frame.height;
        frames.push(AnimatedFrame { frame, delay });
    }

    let is_animated = frames.len() > 1;
    Ok(DecodedImage {
        metadata: ImageMetadata {
            width: final_w,
            height: final_h,
            original_width,
            original_height,
            format_label: if is_animated {
                "GIF (Animated)".to_owned()
            } else {
                "GIF".to_owned()
            },
            file_size_bytes,
            frame_count: frames.len(),
            is_animated,
        },
        frames,
    })
}

fn decode_webp_bytes(bytes: &[u8], file_size_bytes: u64) -> Result<DecodedImage, String> {
    let decoder =
        WebPDecoder::new(Cursor::new(bytes)).map_err(|e| format!("WebP decode failed: {e}"))?;
    let (original_width, original_height) = decoder.dimensions();

    if decoder.has_animation() {
        let raw_frames = decoder
            .into_frames()
            .take(MAX_ANIMATED_FRAMES)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("animated WebP frame decode failed: {e}"))?;

        if raw_frames.is_empty() {
            return Err("animated WebP contains no frames".to_owned());
        }

        let mut frames = Vec::with_capacity(raw_frames.len());
        let mut final_w = 0u32;
        let mut final_h = 0u32;
        for raw in raw_frames {
            let delay = normalize_animation_delay(raw.delay());
            let img = DynamicImage::ImageRgba8(raw.into_buffer());
            let frame = dynamic_image_to_rgba_frame(img);
            final_w = frame.width;
            final_h = frame.height;
            frames.push(AnimatedFrame { frame, delay });
        }

        let is_animated = frames.len() > 1;
        return Ok(DecodedImage {
            metadata: ImageMetadata {
                width: final_w,
                height: final_h,
                original_width,
                original_height,
                format_label: if is_animated {
                    "WebP (Animated)".to_owned()
                } else {
                    "WebP".to_owned()
                },
                file_size_bytes,
                frame_count: frames.len(),
                is_animated,
            },
            frames,
        });
    }

    let img = DynamicImage::from_decoder(decoder)
        .map_err(|e| format!("WebP static decode failed: {e}"))?;
    let frame = dynamic_image_to_rgba_frame(img);
    Ok(DecodedImage {
        metadata: ImageMetadata {
            width: frame.width,
            height: frame.height,
            original_width,
            original_height,
            format_label: "WebP".to_owned(),
            file_size_bytes,
            frame_count: 1,
            is_animated: false,
        },
        frames: vec![AnimatedFrame {
            frame,
            delay: Duration::ZERO,
        }],
    })
}

fn decode_static_raster_bytes(
    bytes: &[u8],
    format: Option<ImageFormat>,
    file_size_bytes: u64,
) -> Result<DecodedImage, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("failed to detect image format: {e}"))?;
    if reader.format().is_none()
        && let Some(fmt) = format
    {
        reader.set_format(fmt);
    }
    let effective_format = reader.format().or(format);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("image decoder initialization failed: {e}"))?;
    let orientation = decoder.orientation().ok();
    let mut img =
        DynamicImage::from_decoder(decoder).map_err(|e| format!("image decode failed: {e}"))?;
    if let Some(orient) = orientation {
        img.apply_orientation(orient);
    }

    let original_width = img.width();
    let original_height = img.height();
    let frame = dynamic_image_to_rgba_frame(img);

    Ok(DecodedImage {
        metadata: ImageMetadata {
            width: frame.width,
            height: frame.height,
            original_width,
            original_height,
            format_label: format_label_for(effective_format),
            file_size_bytes,
            frame_count: 1,
            is_animated: false,
        },
        frames: vec![AnimatedFrame {
            frame,
            delay: Duration::ZERO,
        }],
    })
}

fn dynamic_image_to_rgba_frame(img: DynamicImage) -> RgbaFrame {
    let w = img.width().max(1);
    let h = img.height().max(1);
    let resized = if w > MAX_IMAGE_DIMENSION || h > MAX_IMAGE_DIMENSION {
        img.resize(
            MAX_IMAGE_DIMENSION,
            MAX_IMAGE_DIMENSION,
            FilterType::Lanczos3,
        )
    } else {
        img
    };
    let rgba = resized.to_rgba8();
    let (rw, rh) = rgba.dimensions();
    RgbaFrame {
        width: rw,
        height: rh,
        rgba: rgba.into_raw(),
    }
}

fn normalize_animation_delay(delay: image::Delay) -> Duration {
    let (numer, denom) = delay.numer_denom_ms();
    let ms = if denom == 0 {
        100
    } else {
        u64::from(numer) / u64::from(denom)
    };
    let dur = Duration::from_millis(ms);
    if dur < MIN_ANIMATION_FRAME_DELAY {
        MIN_ANIMATION_FRAME_DELAY
    } else {
        dur
    }
}

fn format_label_for(format: Option<ImageFormat>) -> String {
    match format {
        Some(ImageFormat::Png) => "PNG".to_owned(),
        Some(ImageFormat::Jpeg) => "JPEG".to_owned(),
        Some(ImageFormat::WebP) => "WebP".to_owned(),
        Some(ImageFormat::Gif) => "GIF".to_owned(),
        Some(ImageFormat::Bmp) => "BMP".to_owned(),
        Some(other) => format!("{other:?}").to_uppercase(),
        None => "Image".to_owned(),
    }
}

// ============================================================================
// Video Probing, Poster Extraction & Playback Engine (`ffprobe` + `ffmpeg`)
// ============================================================================

/// Returns `true` if `ffmpeg` is executable on `PATH`.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Returns `true` if `ffprobe` is executable on `PATH`.
pub fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
    size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    duration: Option<String>,
    tags: Option<FfprobeTags>,
    #[serde(default)]
    side_data_list: Vec<FfprobeSideData>,
}

#[derive(Debug, Deserialize)]
struct FfprobeTags {
    rotate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeSideData {
    rotation: Option<i32>,
}

/// Probes video metadata via `ffprobe -v quiet -print_format json -show_format -show_streams`.
pub fn probe_video_metadata(
    path: &Path,
    display_path: Option<&str>,
) -> Result<VideoMetadata, String> {
    let meta = fs::metadata(path).map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("not a regular file: {}", path.display()));
    }

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run ffprobe: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffprobe failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }

    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("failed to parse ffprobe JSON: {e}"))?;

    let video_stream = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| "no video stream found in file".to_owned())?;

    let audio_stream = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    let mut width = video_stream.width.unwrap_or(0);
    let mut height = video_stream.height.unwrap_or(0);
    if width == 0 || height == 0 {
        return Err("video stream reported zero dimensions".to_owned());
    }

    // Account for 90°/270° rotation metadata (ffmpeg auto-rotates decoded frames by default).
    let rotation = video_stream
        .side_data_list
        .iter()
        .find_map(|sd| sd.rotation)
        .or_else(|| {
            video_stream
                .tags
                .as_ref()
                .and_then(|t| t.rotate.as_deref())
                .and_then(|r| r.parse::<i32>().ok())
        })
        .unwrap_or(0);
    if rotation.abs() % 180 == 90 {
        std::mem::swap(&mut width, &mut height);
    }

    let fps = video_stream
        .avg_frame_rate
        .as_deref()
        .and_then(parse_ffprobe_rate)
        .or_else(|| {
            video_stream
                .r_frame_rate
                .as_deref()
                .and_then(parse_ffprobe_rate)
        })
        .unwrap_or(30.0)
        .clamp(1.0, 120.0);

    let duration_secs = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            video_stream
                .duration
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok())
        })
        .filter(|d| d.is_finite() && *d >= 0.0)
        .unwrap_or(0.0);

    let file_size_bytes = parsed
        .format
        .as_ref()
        .and_then(|f| f.size.as_deref())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(meta.len());

    let (render_width, render_height) = compute_video_render_dims(width, height);
    let video_codec = format_codec_name(video_stream.codec_name.as_deref().unwrap_or("video"));
    let audio_codec = audio_stream
        .and_then(|s| s.codec_name.as_deref())
        .map(format_codec_name);

    Ok(VideoMetadata {
        display_path: display_path
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| path.display().to_string()),
        local_path: path.to_path_buf(),
        duration_secs,
        width,
        height,
        render_width,
        render_height,
        fps,
        video_codec,
        has_audio: audio_stream.is_some(),
        audio_codec,
        file_size_bytes,
    })
}

fn parse_ffprobe_rate(raw: &str) -> Option<f64> {
    if let Some((n, d)) = raw.split_once('/') {
        let num = n.parse::<f64>().ok()?;
        let den = d.parse::<f64>().ok()?;
        if den > 0.0 && num > 0.0 && num.is_finite() && den.is_finite() {
            return Some(num / den);
        }
        None
    } else {
        let v = raw.parse::<f64>().ok()?;
        (v > 0.0 && v.is_finite()).then_some(v)
    }
}

fn compute_video_render_dims(width: u32, height: u32) -> (u32, u32) {
    let w = width.max(2) as f64;
    let h = height.max(2) as f64;
    let scale = (MAX_VIDEO_WIDTH as f64 / w)
        .min(MAX_VIDEO_HEIGHT as f64 / h)
        .min(1.0);
    let mut rw = (w * scale).round().max(2.0) as u32;
    let mut rh = (h * scale).round().max(2.0) as u32;
    // Keep even dimensions for clean scaler compatibility.
    if !rw.is_multiple_of(2) {
        rw = (rw - 1).max(2);
    }
    if !rh.is_multiple_of(2) {
        rh = (rh - 1).max(2);
    }
    (rw, rh)
}

fn format_codec_name(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "h264" | "avc1" => "H.264".to_owned(),
        "hevc" | "h265" => "H.265".to_owned(),
        "vp8" => "VP8".to_owned(),
        "vp9" => "VP9".to_owned(),
        "av1" => "AV1".to_owned(),
        "aac" => "AAC".to_owned(),
        "opus" => "Opus".to_owned(),
        "mp3" => "MP3".to_owned(),
        other => other.to_ascii_uppercase(),
    }
}

/// Extracts a single RGBA8 video frame at `seek_secs` scaled to `width x height`.
/// Guarantees the `ffmpeg` child process is cleanly killed and reaped before returning.
pub fn extract_video_frame_rgba(
    path: &Path,
    seek_secs: f64,
    width: u32,
    height: u32,
) -> Result<RgbaFrame, String> {
    let frame_bytes = RgbaFrame::expected_len(width, height)
        .ok_or_else(|| format!("invalid video frame dimensions {width}x{height}"))?;

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
    if seek_secs > 0.001 && seek_secs.is_finite() {
        cmd.args(["-ss", &format!("{seek_secs:.3}")]);
    }
    cmd.arg("-i")
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("scale={width}:{height}:flags=lanczos"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg for poster frame: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg stdout pipe unavailable".to_owned())?;

    let mut buf = vec![0u8; frame_bytes];
    let read_res = stdout.read_exact(&mut buf);
    drop(stdout);
    kill_and_reap(&mut child);

    read_res.map_err(|e| format!("failed to read decoded video frame: {e}"))?;
    Ok(RgbaFrame {
        width,
        height,
        rgba: buf,
    })
}

/// Computes sampled `(mean_luma, std_dev_luma)` in `[0.0, 255.0]` over up to ~2048 evenly spaced
/// pixels of `frame`.
fn frame_luma_stats(frame: &RgbaFrame) -> (f64, f64) {
    let pixel_count = frame.rgba.len() / 4;
    if pixel_count == 0 {
        return (0.0, 0.0);
    }
    let step = (pixel_count / 2048).max(1);
    let mut count = 0usize;
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;

    let mut idx = 0usize;
    while idx < pixel_count {
        let base = idx * 4;
        let r = f64::from(frame.rgba[base]);
        let g = f64::from(frame.rgba[base + 1]);
        let b = f64::from(frame.rgba[base + 2]);
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        sum += luma;
        sum_sq += luma * luma;
        count += 1;
        idx += step;
    }

    if count == 0 {
        return (0.0, 0.0);
    }
    let mean = sum / (count as f64);
    let variance = (sum_sq / (count as f64) - mean * mean).max(0.0);
    (mean, variance.sqrt())
}

/// Returns `true` when `frame` is near-black and low-contrast (e.g. a fade-in-from-black frame
/// at `t = 0.0s`), while returning `false` for dark-themed frames that contain crisp UI/text
/// contrast.
pub fn frame_is_near_black_or_flat(frame: &RgbaFrame) -> bool {
    let (mean, std_dev) = frame_luma_stats(frame);
    mean < 18.0 && std_dev < 10.0
}

/// Extracts a representative RGBA8 poster frame for a newly opened video.
///
/// Always tries `t = 0.0s` first. If that frame is near-black / low-contrast (such as videos
/// starting with a fade-in from black) and `duration_secs > 1.0`, probes representative
/// early timestamps (e.g. `(duration * 0.12).clamp(1.0, 3.0)` and `(duration * 0.25).clamp(2.0, 6.0)`)
/// and selects the first clear frame (or highest-contrast candidate), while playback position
/// remains `0.0s`.
pub fn extract_representative_poster_rgba(
    path: &Path,
    duration_secs: f64,
    width: u32,
    height: u32,
) -> Result<RgbaFrame, String> {
    let first = extract_video_frame_rgba(path, 0.0, width, height)?;
    if !frame_is_near_black_or_flat(&first) || !duration_secs.is_finite() || duration_secs <= 1.0 {
        return Ok(first);
    }

    let t1 = (duration_secs * 0.12)
        .clamp(1.0, 3.0)
        .min((duration_secs * 0.5).max(0.25));
    let t2 = (duration_secs * 0.25)
        .clamp(2.0, 6.0)
        .min((duration_secs * 0.75).max(0.5));

    let score = |frame: &RgbaFrame| -> f64 {
        let (mean, std_dev) = frame_luma_stats(frame);
        std_dev + 0.15 * mean
    };

    let mut best = first;
    let mut best_score = score(&best);

    let mut candidates = Vec::with_capacity(2);
    if t1 > 0.05 && t1 < duration_secs {
        candidates.push(t1);
    }
    if t2 > 0.05 && t2 < duration_secs && (t2 - t1).abs() > 0.25 {
        candidates.push(t2);
    }

    for ts in candidates {
        if let Ok(cand) = extract_video_frame_rgba(path, ts, width, height) {
            if !frame_is_near_black_or_flat(&cand) {
                return Ok(cand);
            }
            let cand_score = score(&cand);
            if cand_score > best_score {
                best = cand;
                best_score = cand_score;
            }
        }
    }

    Ok(best)
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// RAII wrapper around an audio playback process (either `ffplay` or `ffmpeg | pw-cat/paplay`).
/// Always kills and reaps every child process on [`AudioPlayer::stop`] and [`Drop`].
struct AudioPlayer {
    primary: Option<Child>,
    secondary: Option<Child>,
}

impl AudioPlayer {
    /// Attempts to start synced audio playback at `start_secs`.
    /// Returns `None` gracefully if no audio player works (e.g. headless mode or no PipeWire).
    fn spawn(path: &Path, start_secs: f64) -> Option<Self> {
        let seek = format!("{:.3}", start_secs.max(0.0));

        // 1. Prefer `ffplay -nodisp -vn -autoexit -loglevel quiet` (single process, native sync).
        if let Ok(mut child) = Command::new("ffplay")
            .args([
                "-nodisp",
                "-vn",
                "-autoexit",
                "-loglevel",
                "quiet",
                "-ss",
                &seek,
            ])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            // Confirm it didn't immediately crash due to missing audio sink / headless env.
            if matches!(child.try_wait(), Ok(Some(status)) if !status.success()) {
                let _ = child.wait();
            } else {
                return Some(Self {
                    primary: Some(child),
                    secondary: None,
                });
            }
        }

        // 2. Fallback: pipe `ffmpeg` PCM into PipeWire (`pw-cat`) or PulseAudio (`paplay`).
        for (sink_bin, sink_args) in [
            (
                "pw-cat",
                &[
                    "--playback",
                    "--raw",
                    "--rate",
                    "48000",
                    "--channels",
                    "2",
                    "--format",
                    "s16",
                    "-",
                ][..],
            ),
            (
                "paplay",
                &["--raw", "--rate=48000", "--channels=2", "--format=s16le"][..],
            ),
        ] {
            let Ok(mut decoder) = Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "quiet",
                    "-ss",
                    &seek,
                    "-i",
                ])
                .arg(path)
                .args(["-vn", "-f", "s16le", "-ac", "2", "-ar", "48000", "pipe:1"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };

            let Some(pcm_out) = decoder.stdout.take() else {
                kill_and_reap(&mut decoder);
                continue;
            };

            match Command::new(sink_bin)
                .args(sink_args)
                .stdin(Stdio::from(pcm_out))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(sink) => {
                    return Some(Self {
                        primary: Some(sink),
                        secondary: Some(decoder),
                    });
                }
                Err(_) => {
                    kill_and_reap(&mut decoder);
                }
            }
        }

        debug!("no usable Linux audio output available; falling back to silent video playback");
        None
    }

    fn is_running(&mut self) -> bool {
        if let Some(child) = self.primary.as_mut() {
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) | Err(_) => {
                    self.stop();
                    false
                }
            }
        } else {
            false
        }
    }

    fn stop(&mut self) {
        if let Some(mut primary) = self.primary.take() {
            kill_and_reap(&mut primary);
        }
        if let Some(mut secondary) = self.secondary.take() {
            kill_and_reap(&mut secondary);
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Latest-frame bounded slot shared between a video worker thread and [`MediaViewerBackend`].
struct VideoFrameSlot {
    latest_frame: Option<(RgbaFrame, f64)>,
    next_frame_due: Option<Instant>,
    position_secs: f64,
    ended: bool,
    audio_active: bool,
    state_dirty: bool,
}

/// Active background streaming `ffmpeg` playback session for one video tab.
struct ActiveVideoPlayback {
    stop_flag: Arc<AtomicBool>,
    stop_cv: Arc<(Mutex<bool>, Condvar)>,
    video_child: Arc<Mutex<Option<Child>>>,
    audio_player: Arc<Mutex<Option<AudioPlayer>>>,
    slot: Arc<Mutex<VideoFrameSlot>>,
    looping: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ActiveVideoPlayback {
    #[allow(clippy::too_many_arguments)]
    fn start(
        meta: VideoMetadata,
        start_secs: f64,
        muted: bool,
        looping: bool,
        waker: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_cv = Arc::new((Mutex::new(false), Condvar::new()));
        let video_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let audio_player: Arc<Mutex<Option<AudioPlayer>>> = Arc::new(Mutex::new(None));
        let looping_flag = Arc::new(AtomicBool::new(looping));
        let muted_flag = Arc::new(AtomicBool::new(muted));

        let slot = Arc::new(Mutex::new(VideoFrameSlot {
            latest_frame: None,
            next_frame_due: Some(Instant::now()),
            position_secs: start_secs,
            ended: false,
            audio_active: false,
            state_dirty: false,
        }));

        let worker_stop = Arc::clone(&stop_flag);
        let worker_cv = Arc::clone(&stop_cv);
        let worker_vchild = Arc::clone(&video_child);
        let worker_audio = Arc::clone(&audio_player);
        let worker_slot = Arc::clone(&slot);
        let worker_loop = Arc::clone(&looping_flag);
        let worker_muted = Arc::clone(&muted_flag);

        let handle = thread::Builder::new()
            .name("omarex-video-play".to_owned())
            .spawn(move || {
                run_video_playback_loop(
                    meta,
                    start_secs,
                    worker_stop,
                    worker_cv,
                    worker_vchild,
                    worker_audio,
                    worker_slot,
                    worker_loop,
                    worker_muted,
                    waker,
                );
            })
            .ok();

        Self {
            stop_flag,
            stop_cv,
            video_child,
            audio_player,
            slot,
            looping: looping_flag,
            muted: muted_flag,
            handle,
        }
    }

    fn set_looping(&self, looping: bool) {
        self.looping.store(looping, Ordering::SeqCst);
    }

    fn set_muted(&self, path: &Path, has_audio: bool, muted: bool) {
        let prev = self.muted.swap(muted, Ordering::SeqCst);
        if prev == muted {
            return;
        }
        if muted {
            if let Ok(mut guard) = self.audio_player.lock()
                && let Some(mut player) = guard.take()
            {
                player.stop();
            }
            if let Ok(mut s) = self.slot.lock() {
                s.audio_active = false;
                s.state_dirty = true;
            }
        } else if has_audio {
            let pos = self.slot.lock().map(|s| s.position_secs).unwrap_or(0.0);
            let spawned = AudioPlayer::spawn(path, pos);
            let active = spawned.is_some();
            if let Ok(mut guard) = self.audio_player.lock() {
                *guard = spawned;
            }
            if let Ok(mut s) = self.slot.lock() {
                s.audio_active = active;
                s.state_dirty = true;
            }
        }
    }

    /// Immediately terminates all child processes, wakes any sleep, reaps children, and joins thread.
    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        {
            let (lock, cv) = &*self.stop_cv;
            if let Ok(mut stopped) = lock.lock() {
                *stopped = true;
                cv.notify_all();
            }
        }
        // Kill active ffmpeg child immediately so any blocked stdout read returns EOF at once.
        if let Ok(mut guard) = self.video_child.lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.kill();
        }
        if let Ok(mut guard) = self.audio_player.lock()
            && let Some(mut audio) = guard.take()
        {
            audio.stop();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        // Final reap safeguard in case worker exited before taking the child.
        if let Ok(mut guard) = self.video_child.lock()
            && let Some(mut child) = guard.take()
        {
            kill_and_reap(&mut child);
        }
    }
}

impl Drop for ActiveVideoPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_video_playback_loop(
    meta: VideoMetadata,
    initial_start_secs: f64,
    stop_flag: Arc<AtomicBool>,
    stop_cv: Arc<(Mutex<bool>, Condvar)>,
    video_child: Arc<Mutex<Option<Child>>>,
    audio_player: Arc<Mutex<Option<AudioPlayer>>>,
    slot: Arc<Mutex<VideoFrameSlot>>,
    looping: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let target_fps = meta.fps.clamp(1.0, MAX_VIDEO_PLAYBACK_FPS);
    let frame_period = Duration::from_secs_f64(1.0 / target_fps);
    let Some(frame_bytes) = RgbaFrame::expected_len(meta.render_width, meta.render_height) else {
        return;
    };

    let mut current_start = initial_start_secs.max(0.0);

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        // Start audio if unmuted and video has an audio track.
        let audio_active = if meta.has_audio && !muted.load(Ordering::SeqCst) {
            let player = AudioPlayer::spawn(&meta.local_path, current_start);
            let ok = player.is_some();
            if let Ok(mut guard) = audio_player.lock() {
                *guard = player;
            }
            ok
        } else {
            if let Ok(mut guard) = audio_player.lock()
                && let Some(mut old) = guard.take()
            {
                old.stop();
            }
            false
        };

        if let Ok(mut s) = slot.lock() {
            s.audio_active = audio_active;
            s.ended = false;
            s.state_dirty = true;
        }

        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
        if current_start > 0.001 {
            cmd.args(["-ss", &format!("{current_start:.3}")]);
        }
        cmd.arg("-i")
            .arg(&meta.local_path)
            .args([
                "-an",
                "-vf",
                &format!(
                    "scale={}:{}:flags=bilinear,fps={target_fps:.3}",
                    meta.render_width, meta.render_height
                ),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                warn!("failed to spawn ffmpeg playback process: {err}");
                break;
            }
        };

        let Some(mut stdout) = child.stdout.take() else {
            kill_and_reap(&mut child);
            break;
        };

        if let Ok(mut guard) = video_child.lock() {
            *guard = Some(child);
        }

        let mut frame_index: u64 = 0;
        let mut wall_start: Option<Instant> = None;
        let mut eof_reached = false;

        loop {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }

            let mut buf = vec![0u8; frame_bytes];
            match stdout.read_exact(&mut buf) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                    eof_reached = true;
                    break;
                }
                Err(_) => {
                    break;
                }
            }

            if stop_flag.load(Ordering::SeqCst) {
                break;
            }

            let now = Instant::now();
            let origin = *wall_start.get_or_insert(now);
            let pts_offset = Duration::from_secs_f64(frame_index as f64 / target_fps);
            let target_time = origin + pts_offset;

            if target_time > now {
                let wait_dur = target_time.saturating_duration_since(now);
                let (lock, cv) = &*stop_cv;
                if let Ok(stopped) = lock.lock() {
                    let _ = cv.wait_timeout_while(stopped, wait_dur, |s| !*s);
                }
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
            }

            let mut pos = current_start + (frame_index as f64 / target_fps);
            if meta.duration_secs > 0.0 {
                pos = pos.min(meta.duration_secs);
            }

            // Refresh audio active flag without blocking.
            let audio_still_running = audio_player
                .lock()
                .ok()
                .and_then(|mut guard| guard.as_mut().map(AudioPlayer::is_running))
                .unwrap_or(false);

            let next_due = origin + Duration::from_secs_f64((frame_index + 1) as f64 / target_fps);
            if let Ok(mut s) = slot.lock() {
                s.latest_frame = Some((
                    RgbaFrame {
                        width: meta.render_width,
                        height: meta.render_height,
                        rgba: buf,
                    },
                    pos,
                ));
                s.position_secs = pos;
                s.next_frame_due = Some(next_due.max(Instant::now() + frame_period / 4));
                s.audio_active = audio_still_running;
            }

            if let Some(ref wake) = waker {
                wake();
            }

            frame_index = frame_index.saturating_add(1);
        }

        // Always close stdout and reap ffmpeg before looping or exiting.
        drop(stdout);
        if let Ok(mut guard) = video_child.lock()
            && let Some(mut c) = guard.take()
        {
            kill_and_reap(&mut c);
        }
        if let Ok(mut guard) = audio_player.lock()
            && let Some(mut a) = guard.take()
        {
            a.stop();
        }

        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        if eof_reached && looping.load(Ordering::SeqCst) && frame_index > 0 {
            current_start = 0.0;
            continue;
        }

        // Natural end-of-stream (not looping).
        if let Ok(mut s) = slot.lock() {
            s.ended = true;
            s.next_frame_due = None;
            s.audio_active = false;
            if meta.duration_secs > 0.0 {
                s.position_secs = meta.duration_secs;
            }
            s.state_dirty = true;
        }
        if let Some(ref wake) = waker {
            wake();
        }
        break;
    }
}

// ============================================================================
// Internal Backend State & Worker Messages
// ============================================================================

enum WorkerResult {
    Text {
        tab_id: ViewerTabId,
        epoch: u64,
        content: TextContent,
    },
    Image {
        tab_id: ViewerTabId,
        epoch: u64,
        display_path: String,
        local_path: PathBuf,
        decoded: DecodedImage,
    },
    Video {
        tab_id: ViewerTabId,
        epoch: u64,
        metadata: VideoMetadata,
        poster_frame: Option<RgbaFrame>,
    },
    VideoSeekFrame {
        tab_id: ViewerTabId,
        epoch: u64,
        position_secs: f64,
        frame: RgbaFrame,
    },
    Error {
        tab_id: ViewerTabId,
        epoch: u64,
        display_path: String,
        error: String,
    },
}

struct AnimatedImageState {
    frames: Vec<AnimatedFrame>,
    current_index: usize,
    next_frame_at: Instant,
}

struct VideoTabState {
    metadata: VideoMetadata,
    playing: bool,
    position_secs: f64,
    muted: bool,
    looping: bool,
    ended: bool,
    audio_active: bool,
    playback: Option<ActiveVideoPlayback>,
}

impl VideoTabState {
    fn snapshot(&self) -> VideoPlaybackState {
        VideoPlaybackState {
            playing: self.playing,
            position_secs: self.position_secs,
            duration_secs: self.metadata.duration_secs,
            muted: self.muted,
            looping: self.looping,
            ended: self.ended,
            audio_active: self.audio_active,
        }
    }

    fn stop_playback(&mut self) {
        if let Some(mut active) = self.playback.take() {
            if let Ok(s) = active.slot.lock() {
                self.position_secs = s.position_secs;
            }
            active.stop();
        }
        self.playing = false;
        self.audio_active = false;
    }
}

impl Drop for VideoTabState {
    fn drop(&mut self) {
        self.stop_playback();
    }
}

struct TabEntry {
    epoch: u64,
    generation: u64,
    animation: Option<AnimatedImageState>,
    video: Option<VideoTabState>,
}

impl TabEntry {
    fn new(epoch: u64) -> Self {
        Self {
            epoch,
            generation: 0,
            animation: None,
            video: None,
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }

    fn stop_media(&mut self) {
        self.animation = None;
        if let Some(mut v) = self.video.take() {
            v.stop_playback();
        }
    }
}

// ============================================================================
// Public Non-Blocking Backend Manager (`MediaViewerBackend`)
// ============================================================================

/// Non-blocking async media backend driven from the main `winit` event loop.
pub struct MediaViewerBackend {
    tabs: HashMap<ViewerTabId, TabEntry>,
    tx: Sender<WorkerResult>,
    rx: Receiver<WorkerResult>,
    pending_events: Vec<MediaEvent>,
    pending_loads: Arc<AtomicU64>,
    epoch_counter: u64,
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for MediaViewerBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaViewerBackend {
    /// Creates a new backend with no external waker.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tabs: HashMap::new(),
            tx,
            rx,
            pending_events: Vec::new(),
            pending_loads: Arc::new(AtomicU64::new(0)),
            epoch_counter: 0,
            waker: None,
        }
    }

    /// Creates a new backend with a callback invoked whenever background work produces data
    /// (e.g. to wake a sleeping `winit::event_loop::EventLoopProxy`).
    pub fn with_waker(waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        let mut backend = Self::new();
        backend.waker = Some(waker);
        backend
    }

    /// Installs or replaces the wakeup callback for async completions and video frames.
    pub fn set_waker(&mut self, waker: Arc<dyn Fn() + Send + Sync>) {
        self.waker = Some(waker);
    }

    /// Starts an async background load of a local file for `tab_id`.
    pub fn request_load_local(
        &mut self,
        tab_id: ViewerTabId,
        path: impl Into<PathBuf>,
        kind: MediaKind,
    ) {
        let local_path = path.into();
        let display_path = local_path.display().to_string();
        self.spawn_load_task(tab_id, display_path, local_path, kind);
    }

    /// Starts an async background load of a remote file already downloaded into `local_cached_path`
    /// (e.g. via Rex's `HostCommand::DownloadFile`), preserving `remote_path` as the display path.
    pub fn ingest_remote_file(
        &mut self,
        tab_id: ViewerTabId,
        remote_path: impl Into<String>,
        local_cached_path: impl Into<PathBuf>,
        kind: MediaKind,
    ) {
        self.spawn_load_task(tab_id, remote_path.into(), local_cached_path.into(), kind);
    }

    /// Writes downloaded remote bytes into `~/.cache/omarex/viewer-remote/` on a background thread
    /// and immediately loads/decodes the cached file for `tab_id`.
    pub fn ingest_remote_bytes(
        &mut self,
        tab_id: ViewerTabId,
        host_id: impl Into<String>,
        remote_path: impl Into<String>,
        bytes: Vec<u8>,
        kind: MediaKind,
    ) {
        let epoch = self.reset_tab_epoch(tab_id);
        let host_id = host_id.into();
        let remote_path = remote_path.into();
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending_loads);
        let waker = self.waker.clone();
        pending.fetch_add(1, Ordering::SeqCst);

        let _ = thread::Builder::new()
            .name("omarex-media-remote".to_owned())
            .spawn(move || {
                let res = match cache_remote_bytes(&host_id, &remote_path, &bytes) {
                    Ok(local_path) => execute_load(tab_id, epoch, remote_path, local_path, kind),
                    Err(err) => WorkerResult::Error {
                        tab_id,
                        epoch,
                        display_path: remote_path,
                        error: format!("failed to cache remote file: {err}"),
                    },
                };
                pending.fetch_sub(1, Ordering::SeqCst);
                let _ = tx.send(res);
                if let Some(wake) = waker {
                    wake();
                }
            });
    }

    /// Drains all completed background loads, newly due animated image frames, and newly decoded
    /// video frames without blocking the caller.
    pub fn poll_events(&mut self) -> Vec<MediaEvent> {
        let mut out = std::mem::take(&mut self.pending_events);
        let now = Instant::now();

        // 1. Drain completed background worker jobs.
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerResult::Text {
                    tab_id,
                    epoch,
                    content,
                } => {
                    if self.tab_matches_epoch(tab_id, epoch) {
                        out.push(MediaEvent::TextLoaded { tab_id, content });
                    }
                }
                WorkerResult::Image {
                    tab_id,
                    epoch,
                    display_path,
                    local_path,
                    mut decoded,
                } => {
                    if let Some(entry) = self.tab_mut_if_epoch(tab_id, epoch) {
                        let generation = entry.next_generation();
                        let frame_durations: Vec<Duration> =
                            decoded.frames.iter().map(|f| f.delay).collect();
                        let initial_frame = if decoded.frames.len() > 1 {
                            let first = decoded.frames[0].frame.clone();
                            let first_delay = decoded.frames[0].delay;
                            entry.animation = Some(AnimatedImageState {
                                frames: decoded.frames,
                                current_index: 0,
                                next_frame_at: now + first_delay,
                            });
                            first
                        } else {
                            decoded.frames.remove(0).frame
                        };

                        out.push(MediaEvent::ImageLoaded {
                            tab_id,
                            texture_key: texture_key_for_tab(tab_id),
                            generation,
                            display_path,
                            local_path,
                            metadata: decoded.metadata,
                            frame: initial_frame,
                            frame_durations,
                            texture_dirty: true,
                        });
                    }
                }
                WorkerResult::Video {
                    tab_id,
                    epoch,
                    metadata,
                    poster_frame,
                } => {
                    if let Some(entry) = self.tab_mut_if_epoch(tab_id, epoch) {
                        let texture_dirty = poster_frame.is_some();
                        let generation = if texture_dirty {
                            entry.next_generation()
                        } else {
                            entry.generation
                        };
                        let video_state = VideoTabState {
                            metadata: metadata.clone(),
                            playing: false,
                            position_secs: 0.0,
                            muted: false,
                            looping: false,
                            ended: false,
                            audio_active: false,
                            playback: None,
                        };
                        let state = video_state.snapshot();
                        entry.video = Some(video_state);

                        out.push(MediaEvent::VideoLoaded {
                            tab_id,
                            texture_key: texture_key_for_tab(tab_id),
                            generation,
                            metadata,
                            poster_frame,
                            state,
                            texture_dirty,
                        });
                    }
                }
                WorkerResult::VideoSeekFrame {
                    tab_id,
                    epoch,
                    position_secs,
                    frame,
                } => {
                    if let Some(entry) = self.tab_mut_if_epoch(tab_id, epoch) {
                        let generation = entry.next_generation();
                        if let Some(video) = entry.video.as_mut() {
                            // Ignore stale paused-seek preview if playback already started.
                            if !video.playing {
                                video.position_secs = position_secs;
                                let state = video.snapshot();
                                out.push(MediaEvent::VideoFrame {
                                    tab_id,
                                    texture_key: texture_key_for_tab(tab_id),
                                    generation,
                                    position_secs,
                                    frame,
                                    state,
                                    texture_dirty: true,
                                });
                            }
                        }
                    }
                }
                WorkerResult::Error {
                    tab_id,
                    epoch,
                    display_path,
                    error,
                } => {
                    if self.tab_matches_epoch(tab_id, epoch) {
                        out.push(MediaEvent::LoadError {
                            tab_id,
                            display_path,
                            error,
                        });
                    }
                }
            }
        }

        // 2. Advance animated GIF/WebP frames whose presentation deadline has elapsed.
        for (&tab_id, entry) in &mut self.tabs {
            let mut next_frame: Option<(usize, usize, RgbaFrame)> = None;
            if let Some(anim) = entry.animation.as_mut()
                && now >= anim.next_frame_at
                && anim.frames.len() > 1
            {
                anim.current_index = (anim.current_index + 1) % anim.frames.len();
                let current = &anim.frames[anim.current_index];
                anim.next_frame_at = now + current.delay;
                next_frame = Some((anim.current_index, anim.frames.len(), current.frame.clone()));
            }
            if let Some((frame_index, frame_count, frame)) = next_frame {
                let generation = entry.next_generation();
                out.push(MediaEvent::AnimationFrame {
                    tab_id,
                    texture_key: texture_key_for_tab(tab_id),
                    generation,
                    frame_index,
                    frame_count,
                    frame,
                    texture_dirty: true,
                });
            }
        }

        // 3. Drain latest video frames & EOF/state changes from active playback slots.
        for (&tab_id, entry) in &mut self.tabs {
            let mut emit_frame: Option<(RgbaFrame, f64, VideoPlaybackState)> = None;
            let mut emit_state_change: Option<VideoPlaybackState> = None;

            if let Some(video) = entry.video.as_mut() {
                let mut should_stop_finished = false;
                if let Some(active) = video.playback.as_ref()
                    && let Ok(mut slot) = active.slot.lock()
                {
                    video.position_secs = slot.position_secs;
                    video.audio_active = slot.audio_active;

                    if let Some((frame, pos)) = slot.latest_frame.take() {
                        video.position_secs = pos;
                        emit_frame = Some((frame, pos, video.snapshot()));
                    }

                    if slot.ended {
                        video.playing = false;
                        video.ended = true;
                        video.audio_active = false;
                        should_stop_finished = true;
                        slot.state_dirty = false;
                        emit_state_change = Some(video.snapshot());
                    } else if slot.state_dirty {
                        slot.state_dirty = false;
                        if emit_frame.is_none() {
                            emit_state_change = Some(video.snapshot());
                        }
                    }
                }
                if should_stop_finished {
                    video.stop_playback();
                }
            }

            if let Some((frame, position_secs, state)) = emit_frame {
                let generation = entry.next_generation();
                out.push(MediaEvent::VideoFrame {
                    tab_id,
                    texture_key: texture_key_for_tab(tab_id),
                    generation,
                    position_secs,
                    frame,
                    state,
                    texture_dirty: true,
                });
            }
            if let Some(state) = emit_state_change {
                out.push(MediaEvent::VideoStateChanged { tab_id, state });
            }
        }

        out
    }

    /// Starts or resumes video playback for `tab_id`. Restarts from `0.0` if previously ended.
    pub fn video_play(&mut self, tab_id: ViewerTabId) -> bool {
        let Some(entry) = self.tabs.get_mut(&tab_id) else {
            return false;
        };
        let Some(video) = entry.video.as_mut() else {
            return false;
        };
        if video.playing {
            return true;
        }
        if video.ended {
            video.position_secs = 0.0;
            video.ended = false;
        }
        let start_secs = video.position_secs;
        let playback = ActiveVideoPlayback::start(
            video.metadata.clone(),
            start_secs,
            video.muted,
            video.looping,
            self.waker.clone(),
        );
        video.playing = true;
        video.playback = Some(playback);
        let state = video.snapshot();
        self.pending_events
            .push(MediaEvent::VideoStateChanged { tab_id, state });
        true
    }

    /// Pauses video playback for `tab_id`, immediately killing and reaping decoder/audio children.
    pub fn video_pause(&mut self, tab_id: ViewerTabId) -> bool {
        let Some(entry) = self.tabs.get_mut(&tab_id) else {
            return false;
        };
        let Some(video) = entry.video.as_mut() else {
            return false;
        };
        if !video.playing && video.playback.is_none() {
            return true;
        }
        video.stop_playback();
        let state = video.snapshot();
        self.pending_events
            .push(MediaEvent::VideoStateChanged { tab_id, state });
        true
    }

    /// Toggles video playback between playing and paused for `tab_id`.
    pub fn video_toggle_play(&mut self, tab_id: ViewerTabId) -> bool {
        let is_playing = self
            .tabs
            .get(&tab_id)
            .and_then(|e| e.video.as_ref())
            .is_some_and(|v| v.playing);
        if is_playing {
            self.video_pause(tab_id)
        } else {
            self.video_play(tab_id)
        }
    }

    /// Seeks video playback for `tab_id` to `position_secs`.
    /// - If currently playing, seamlessly restarts streaming `ffmpeg` + audio from `position_secs`.
    /// - If paused, asynchronously extracts a single RGBA8 preview frame at `position_secs`.
    pub fn video_seek(&mut self, tab_id: ViewerTabId, position_secs: f64) -> bool {
        let Some(entry) = self.tabs.get_mut(&tab_id) else {
            return false;
        };
        let epoch = entry.epoch;
        let Some(video) = entry.video.as_mut() else {
            return false;
        };

        let clamped = if video.metadata.duration_secs > 0.0 {
            position_secs.clamp(0.0, video.metadata.duration_secs)
        } else {
            position_secs.max(0.0)
        };

        let was_playing = video.playing;
        video.stop_playback();
        video.position_secs = clamped;
        video.ended = false;

        if was_playing {
            let playback = ActiveVideoPlayback::start(
                video.metadata.clone(),
                clamped,
                video.muted,
                video.looping,
                self.waker.clone(),
            );
            video.playing = true;
            video.playback = Some(playback);
        } else {
            // Decode a single preview frame in the background so scrubbing while paused updates live.
            let path = video.metadata.local_path.clone();
            let rw = video.metadata.render_width;
            let rh = video.metadata.render_height;
            let duration = video.metadata.duration_secs;
            let tx = self.tx.clone();
            let pending = Arc::clone(&self.pending_loads);
            let waker = self.waker.clone();
            pending.fetch_add(1, Ordering::SeqCst);

            let _ = thread::Builder::new()
                .name("omarex-video-seek".to_owned())
                .spawn(move || {
                    let frame_res =
                        extract_video_frame_rgba(&path, clamped, rw, rh).or_else(|_| {
                            let fallback = (duration - 0.1).max(0.0);
                            extract_video_frame_rgba(&path, fallback, rw, rh)
                        });
                    pending.fetch_sub(1, Ordering::SeqCst);
                    if let Ok(frame) = frame_res {
                        let _ = tx.send(WorkerResult::VideoSeekFrame {
                            tab_id,
                            epoch,
                            position_secs: clamped,
                            frame,
                        });
                        if let Some(wake) = waker {
                            wake();
                        }
                    }
                });
        }

        let state = video.snapshot();
        self.pending_events
            .push(MediaEvent::VideoStateChanged { tab_id, state });
        true
    }

    /// Mutes or unmutes video audio for `tab_id`.
    pub fn video_set_muted(&mut self, tab_id: ViewerTabId, muted: bool) -> bool {
        let Some(entry) = self.tabs.get_mut(&tab_id) else {
            return false;
        };
        let Some(video) = entry.video.as_mut() else {
            return false;
        };
        video.muted = muted;
        if let Some(active) = video.playback.as_ref() {
            active.set_muted(&video.metadata.local_path, video.metadata.has_audio, muted);
            if muted {
                video.audio_active = false;
            }
        }
        let state = video.snapshot();
        self.pending_events
            .push(MediaEvent::VideoStateChanged { tab_id, state });
        true
    }

    /// Enables or disables looping playback for `tab_id`.
    pub fn video_set_loop(&mut self, tab_id: ViewerTabId, looping: bool) -> bool {
        let Some(entry) = self.tabs.get_mut(&tab_id) else {
            return false;
        };
        let Some(video) = entry.video.as_mut() else {
            return false;
        };
        video.looping = looping;
        if let Some(active) = video.playback.as_ref() {
            active.set_looping(looping);
        }
        let state = video.snapshot();
        self.pending_events
            .push(MediaEvent::VideoStateChanged { tab_id, state });
        true
    }

    /// Returns the current [`VideoPlaybackState`] snapshot for `tab_id`, if it is a loaded video tab.
    pub fn video_state(&self, tab_id: ViewerTabId) -> Option<VideoPlaybackState> {
        self.tabs
            .get(&tab_id)
            .and_then(|e| e.video.as_ref())
            .map(VideoTabState::snapshot)
    }

    /// Closes `tab_id`, stopping any active video playback / animation and reaping child processes.
    pub fn close_tab(&mut self, tab_id: ViewerTabId) {
        if let Some(mut entry) = self.tabs.remove(&tab_id) {
            entry.stop_media();
        }
    }

    /// Closes all tabs and stops all active media workers/processes.
    pub fn close_all(&mut self) {
        for (_, mut entry) in self.tabs.drain() {
            entry.stop_media();
        }
    }

    /// Returns the earliest [`Instant`] at which the `winit` event loop should wake up to display
    /// the next animated image or video frame (or collect pending async loads).
    /// Returns `None` when completely idle so `winit` can sleep with 0% CPU usage.
    pub fn next_wakeup_instant(&self) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;

        if !self.pending_events.is_empty() {
            return Some(Instant::now());
        }

        if self.pending_loads.load(Ordering::SeqCst) > 0 {
            earliest = Some(Instant::now() + PENDING_LOAD_WAKEUP_INTERVAL);
        }

        for entry in self.tabs.values() {
            if let Some(anim) = entry.animation.as_ref()
                && anim.frames.len() > 1
            {
                earliest = Some(match earliest {
                    Some(prev) => prev.min(anim.next_frame_at),
                    None => anim.next_frame_at,
                });
            }
            if let Some(video) = entry.video.as_ref()
                && video.playing
                && let Some(active) = video.playback.as_ref()
                && let Ok(slot) = active.slot.lock()
            {
                if slot.latest_frame.is_some() || slot.ended || slot.state_dirty {
                    return Some(Instant::now());
                }
                if let Some(due) = slot.next_frame_due {
                    earliest = Some(match earliest {
                        Some(prev) => prev.min(due),
                        None => due,
                    });
                }
            }
        }

        earliest
    }

    /// Returns `true` if any tab has active video playback or an animated image running.
    pub fn has_active_playback(&self) -> bool {
        self.tabs.values().any(|entry| {
            entry.animation.as_ref().is_some_and(|a| a.frames.len() > 1)
                || entry.video.as_ref().is_some_and(|v| v.playing)
        })
    }

    /// Returns the number of tracked tabs currently open in the backend.
    pub fn active_tab_count(&self) -> usize {
        self.tabs.len()
    }

    fn reset_tab_epoch(&mut self, tab_id: ViewerTabId) -> u64 {
        self.epoch_counter = self.epoch_counter.saturating_add(1);
        let epoch = self.epoch_counter;
        if let Some(prev) = self.tabs.get_mut(&tab_id) {
            prev.stop_media();
            prev.epoch = epoch;
        } else {
            self.tabs.insert(tab_id, TabEntry::new(epoch));
        }
        epoch
    }

    fn tab_matches_epoch(&self, tab_id: ViewerTabId, epoch: u64) -> bool {
        self.tabs.get(&tab_id).is_some_and(|e| e.epoch == epoch)
    }

    fn tab_mut_if_epoch(&mut self, tab_id: ViewerTabId, epoch: u64) -> Option<&mut TabEntry> {
        let entry = self.tabs.get_mut(&tab_id)?;
        (entry.epoch == epoch).then_some(entry)
    }

    fn spawn_load_task(
        &mut self,
        tab_id: ViewerTabId,
        display_path: String,
        local_path: PathBuf,
        kind: MediaKind,
    ) {
        let epoch = self.reset_tab_epoch(tab_id);
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending_loads);
        let waker = self.waker.clone();
        pending.fetch_add(1, Ordering::SeqCst);

        let _ = thread::Builder::new()
            .name("omarex-media-load".to_owned())
            .spawn(move || {
                let res = execute_load(tab_id, epoch, display_path, local_path, kind);
                pending.fetch_sub(1, Ordering::SeqCst);
                let _ = tx.send(res);
                if let Some(wake) = waker {
                    wake();
                }
            });
    }
}

impl Drop for MediaViewerBackend {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn execute_load(
    tab_id: ViewerTabId,
    epoch: u64,
    display_path: String,
    local_path: PathBuf,
    kind: MediaKind,
) -> WorkerResult {
    let resolved = kind.resolve(&local_path);
    match resolved {
        MediaKind::Text | MediaKind::Auto => match load_text_file(&local_path, Some(&display_path))
        {
            Ok(content) => WorkerResult::Text {
                tab_id,
                epoch,
                content,
            },
            Err(error) => WorkerResult::Error {
                tab_id,
                epoch,
                display_path,
                error,
            },
        },
        MediaKind::Image => match decode_image_file(&local_path) {
            Ok(decoded) => WorkerResult::Image {
                tab_id,
                epoch,
                display_path,
                local_path,
                decoded,
            },
            Err(error) => WorkerResult::Error {
                tab_id,
                epoch,
                display_path,
                error,
            },
        },
        MediaKind::Video => match probe_video_metadata(&local_path, Some(&display_path)) {
            Ok(metadata) => {
                let poster_frame = extract_representative_poster_rgba(
                    &local_path,
                    metadata.duration_secs,
                    metadata.render_width,
                    metadata.render_height,
                )
                .ok();
                WorkerResult::Video {
                    tab_id,
                    epoch,
                    metadata,
                    poster_frame,
                }
            }
            Err(error) => WorkerResult::Error {
                tab_id,
                epoch,
                display_path,
                error,
            },
        },
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, Rgba, RgbaImage};
    use std::sync::atomic::AtomicUsize;

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "omarex_media_viewer_test_{}_{}_{}",
                label,
                std::process::id(),
                n
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn wait_for_event(backend: &mut MediaViewerBackend, timeout: Duration) -> Option<MediaEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let events = backend.poll_events();
            if let Some(first) = events.into_iter().next() {
                return Some(first);
            }
            thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn text_loader_handles_utf8_crlf_and_lossy_fallback() {
        let tmp = TempDirGuard::new("text");
        let utf8_path = tmp.path.join("notes.rs");
        fs::write(
            &utf8_path,
            "fn main() {\r\n    println!(\"Grüße 🚀\");\r\n}",
        )
        .unwrap();

        let loaded = load_text_file(&utf8_path, None).expect("load utf8 text");
        assert!(!loaded.is_binary);
        assert!(!loaded.lossy_utf8);
        assert!(!loaded.truncated);
        assert_eq!(loaded.lines.len(), 3);
        assert_eq!(loaded.lines[0], "fn main() {");
        assert_eq!(loaded.lines[1], "    println!(\"Grüße 🚀\");");
        assert!(loaded.modified_unix_secs.is_some());

        // Single invalid UTF-8 byte in an otherwise valid text file -> lossy UTF-8, NOT binary.
        let lossy_path = tmp.path.join("latin1.txt");
        let mut raw = b"hello world line one\nsecond line with one bad byte: ".to_vec();
        raw.push(0xFF);
        raw.extend_from_slice(b" end\n");
        fs::write(&lossy_path, &raw).unwrap();

        let lossy = load_text_file(&lossy_path, None).expect("load lossy text");
        assert!(!lossy.is_binary);
        assert!(lossy.lossy_utf8);
        assert!(lossy.lines[1].contains('\u{FFFD}'));
    }

    #[test]
    fn binary_detection_flags_nul_bytes_and_produces_hex_preview() {
        let tmp = TempDirGuard::new("binary");
        let bin_path = tmp.path.join("blob.bin");
        let mut data = vec![0x7F, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00];
        data.extend_from_slice(&[0u8; 32]);
        fs::write(&bin_path, &data).unwrap();

        assert!(is_likely_binary(&data));
        assert!(!is_likely_binary(
            "Hello Über 日本語\nLine 2\tOK".as_bytes()
        ));

        let loaded = load_text_file(&bin_path, None).expect("load binary");
        assert!(loaded.is_binary);
        assert!(!loaded.lines.is_empty());
        assert!(loaded.lines[0].contains("Binary file"));
        assert!(loaded.lines[1].contains("7f 45 4c 46"));
    }

    #[test]
    fn decodes_png_webp_bmp_and_animated_gif_with_frame_delays() {
        let tmp = TempDirGuard::new("images");

        // 1. PNG
        let mut png_img = RgbaImage::new(16, 10);
        png_img.put_pixel(3, 4, Rgba([255, 64, 32, 200]));
        let png_path = tmp.path.join("sample.png");
        png_img.save(&png_path).expect("save png");

        let decoded_png = decode_image_file(&png_path).expect("decode png");
        assert_eq!(decoded_png.metadata.width, 16);
        assert_eq!(decoded_png.metadata.height, 10);
        assert_eq!(decoded_png.metadata.format_label, "PNG");
        assert_eq!(decoded_png.metadata.frame_count, 1);
        assert!(!decoded_png.metadata.is_animated);
        let idx = ((4 * 16) + 3) * 4;
        assert_eq!(
            &decoded_png.frames[0].frame.rgba[idx..idx + 4],
            &[255, 64, 32, 200]
        );

        // 2. WebP
        let webp_path = tmp.path.join("sample.webp");
        png_img
            .save_with_format(&webp_path, ImageFormat::WebP)
            .expect("save webp");
        let decoded_webp = decode_image_file(&webp_path).expect("decode webp");
        assert_eq!(decoded_webp.metadata.width, 16);
        assert_eq!(decoded_webp.metadata.height, 10);
        assert_eq!(decoded_webp.metadata.format_label, "WebP");
        assert_eq!(decoded_webp.frames[0].frame.rgba.len(), 16 * 10 * 4);

        // 3. Animated GIF (2 frames with distinct delays)
        let gif_path = tmp.path.join("anim.gif");
        {
            let file = File::create(&gif_path).expect("create gif");
            let mut enc = GifEncoder::new(file);
            enc.set_repeat(Repeat::Infinite).expect("set repeat");

            let f1 = RgbaImage::from_pixel(8, 8, Rgba([255, 0, 0, 255]));
            let f2 = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 255, 255]));
            enc.encode_frame(Frame::from_parts(
                f1,
                0,
                0,
                Delay::from_numer_denom_ms(60, 1),
            ))
            .expect("encode frame 1");
            enc.encode_frame(Frame::from_parts(
                f2,
                0,
                0,
                Delay::from_numer_denom_ms(120, 1),
            ))
            .expect("encode frame 2");
        }

        let decoded_gif = decode_image_file(&gif_path).expect("decode gif");
        assert_eq!(decoded_gif.metadata.frame_count, 2);
        assert!(decoded_gif.metadata.is_animated);
        assert_eq!(decoded_gif.metadata.format_label, "GIF (Animated)");
        assert_eq!(decoded_gif.frames[0].delay, Duration::from_millis(60));
        assert_eq!(decoded_gif.frames[1].delay, Duration::from_millis(120));
    }

    #[test]
    fn decodes_svg_into_straight_alpha_rgba8() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32" viewBox="0 0 64 32">
            <rect x="0" y="0" width="64" height="32" fill="#22aa44"/>
        </svg>"##;

        let decoded = decode_image_bytes(svg, Some("svg"), svg.len() as u64).expect("decode svg");
        assert_eq!(decoded.metadata.width, 64);
        assert_eq!(decoded.metadata.height, 32);
        assert_eq!(decoded.metadata.format_label, "SVG");
        assert_eq!(decoded.metadata.frame_count, 1);
        assert_eq!(decoded.frames[0].frame.rgba.len(), 64 * 32 * 4);
        // Center pixel should be #22aa44ff.
        let center = ((16 * 64) + 32) * 4;
        assert_eq!(
            &decoded.frames[0].frame.rgba[center..center + 4],
            &[0x22, 0xaa, 0x44, 0xff]
        );
    }

    #[test]
    fn backend_async_load_and_remote_bytes_ingestion_work() {
        let tmp = TempDirGuard::new("backend");
        let txt_path = tmp.path.join("hello.md");
        fs::write(&txt_path, "# Title\nBody line").unwrap();

        let wake_count = Arc::new(AtomicUsize::new(0));
        let wake_clone = Arc::clone(&wake_count);
        let mut backend = MediaViewerBackend::with_waker(Arc::new(move || {
            wake_clone.fetch_add(1, Ordering::SeqCst);
        }));

        backend.request_load_local(1, &txt_path, MediaKind::Auto);
        let ev = wait_for_event(&mut backend, Duration::from_secs(3)).expect("text event");
        match ev {
            MediaEvent::TextLoaded { tab_id, content } => {
                assert_eq!(tab_id, 1);
                assert_eq!(content.lines, vec!["# Title", "Body line"]);
            }
            other => panic!("expected TextLoaded, got {other:?}"),
        }
        assert!(wake_count.load(Ordering::SeqCst) >= 1);

        // Remote bytes ingestion caches into ~/.cache/omarex/viewer-remote/ and preserves display_path.
        let svg_bytes =
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><circle cx="8" cy="8" r="6" fill="red"/></svg>"#
                .to_vec();
        backend.ingest_remote_bytes(
            2,
            "mac-mini",
            "/Users/mike/Pictures/icon.svg",
            svg_bytes,
            MediaKind::Image,
        );
        let ev2 = wait_for_event(&mut backend, Duration::from_secs(3)).expect("remote image");
        match ev2 {
            MediaEvent::ImageLoaded {
                tab_id,
                display_path,
                local_path,
                metadata,
                texture_dirty,
                ..
            } => {
                assert_eq!(tab_id, 2);
                assert_eq!(display_path, "/Users/mike/Pictures/icon.svg");
                assert!(local_path.starts_with(viewer_remote_cache_dir()));
                assert_eq!(metadata.format_label, "SVG");
                assert!(texture_dirty);
                let _ = fs::remove_file(&local_path);
            }
            other => panic!("expected ImageLoaded, got {other:?}"),
        }

        backend.close_tab(1);
        backend.close_tab(2);
        assert_eq!(backend.active_tab_count(), 0);
        assert!(backend.next_wakeup_instant().is_none());
    }

    #[test]
    fn video_probe_poster_playback_seek_and_clean_process_shutdown() {
        if !ffmpeg_available() || !ffprobe_available() {
            eprintln!("skipping video test: ffmpeg/ffprobe not on PATH");
            return;
        }

        let tmp = TempDirGuard::new("video");
        let mp4_path = tmp.path.join("tiny_test.mp4");

        // Generate a tiny 1-second 64x48 @ 15fps MP4 with a silent audio track.
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=64x48:r=15:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&mp4_path)
            .status()
            .expect("run ffmpeg to generate test mp4");
        assert!(status.success(), "ffmpeg test clip generation failed");

        let meta = probe_video_metadata(&mp4_path, None).expect("probe mp4");
        assert_eq!(meta.width, 64);
        assert_eq!(meta.height, 48);
        assert_eq!(meta.render_width, 64);
        assert_eq!(meta.render_height, 48);
        assert!(meta.duration_secs >= 0.8 && meta.duration_secs <= 1.5);
        assert!(meta.has_audio);
        assert_eq!(meta.video_codec, "H.264");

        let mut backend = MediaViewerBackend::new();
        backend.request_load_local(42, &mp4_path, MediaKind::Video);

        let loaded = wait_for_event(&mut backend, Duration::from_secs(5)).expect("video loaded");
        match loaded {
            MediaEvent::VideoLoaded {
                tab_id,
                poster_frame,
                texture_dirty,
                metadata,
                ..
            } => {
                assert_eq!(tab_id, 42);
                assert!(texture_dirty);
                assert_eq!(metadata.width, 64);
                let poster = poster_frame.expect("first-frame poster RGBA");
                assert_eq!(poster.width, 64);
                assert_eq!(poster.height, 48);
                assert_eq!(poster.rgba.len(), 64 * 48 * 4);
                // Blue frame: B channel should dominate R channel.
                let px = &poster.rgba[0..4];
                assert!(px[2] > px[0], "expected blue poster pixel, got {px:?}");
            }
            other => panic!("expected VideoLoaded, got {other:?}"),
        }

        // Start muted playback and verify streaming frames arrive and wakeup instant is set.
        assert!(backend.video_set_muted(42, true));
        assert!(backend.video_set_loop(42, true));
        assert!(backend.video_play(42));
        assert!(backend.has_active_playback());
        assert!(backend.next_wakeup_instant().is_some());

        let mut got_video_frame = false;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            for ev in backend.poll_events() {
                if let MediaEvent::VideoFrame {
                    tab_id,
                    frame,
                    texture_dirty,
                    ..
                } = ev
                {
                    assert_eq!(tab_id, 42);
                    assert!(texture_dirty);
                    assert_eq!(frame.rgba.len(), 64 * 48 * 4);
                    got_video_frame = true;
                }
            }
            if got_video_frame {
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
        assert!(
            got_video_frame,
            "expected at least one streamed video frame"
        );

        // Pause, seek while paused (produces seek preview frame), then close & verify clean stop.
        assert!(backend.video_pause(42));
        assert!(!backend.has_active_playback());
        assert!(backend.video_seek(42, 0.5));

        let mut got_seek_frame = false;
        let seek_deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < seek_deadline {
            for ev in backend.poll_events() {
                if let MediaEvent::VideoFrame {
                    tab_id,
                    position_secs,
                    ..
                } = ev
                {
                    assert_eq!(tab_id, 42);
                    assert!((position_secs - 0.5).abs() < 0.05);
                    got_seek_frame = true;
                }
            }
            if got_seek_frame {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(got_seek_frame, "expected seek preview frame while paused");

        backend.close_tab(42);
        assert_eq!(backend.active_tab_count(), 0);
        assert!(!backend.has_active_playback());
    }

    #[test]
    fn backend_advances_animated_gif_frames_and_schedules_wakeup() {
        let tmp = TempDirGuard::new("anim_gif_backend");
        let gif_path = tmp.path.join("loop.gif");
        {
            let file = File::create(&gif_path).expect("create gif");
            let mut enc = GifEncoder::new(file);
            enc.set_repeat(Repeat::Infinite).expect("set repeat");

            let f1 = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
            let f2 = RgbaImage::from_pixel(4, 4, Rgba([0, 255, 0, 255]));
            enc.encode_frame(Frame::from_parts(
                f1,
                0,
                0,
                Delay::from_numer_denom_ms(30, 1),
            ))
            .expect("frame 1");
            enc.encode_frame(Frame::from_parts(
                f2,
                0,
                0,
                Delay::from_numer_denom_ms(30, 1),
            ))
            .expect("frame 2");
        }

        let mut backend = MediaViewerBackend::new();
        backend.request_load_local(7, &gif_path, MediaKind::Image);

        let loaded = wait_for_event(&mut backend, Duration::from_secs(3)).expect("gif loaded");
        match loaded {
            MediaEvent::ImageLoaded {
                tab_id,
                metadata,
                frame_durations,
                ..
            } => {
                assert_eq!(tab_id, 7);
                assert!(metadata.is_animated);
                assert_eq!(frame_durations.len(), 2);
            }
            other => panic!("expected ImageLoaded, got {other:?}"),
        }

        assert!(backend.has_active_playback());
        assert!(backend.next_wakeup_instant().is_some());

        let mut saw_next_frame = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            for ev in backend.poll_events() {
                if let MediaEvent::AnimationFrame {
                    tab_id,
                    frame_index,
                    frame_count,
                    texture_dirty,
                    ..
                } = ev
                {
                    assert_eq!(tab_id, 7);
                    assert_eq!(frame_index, 1);
                    assert_eq!(frame_count, 2);
                    assert!(texture_dirty);
                    saw_next_frame = true;
                }
            }
            if saw_next_frame {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_next_frame, "expected AnimationFrame tick from backend");

        backend.close_tab(7);
        assert!(!backend.has_active_playback());
        assert!(backend.next_wakeup_instant().is_none());
    }

    #[test]
    fn poster_extraction_skips_black_fade_in_and_keeps_position_zero() {
        // 1. Pure black frame -> near-black/flat.
        let black = RgbaFrame {
            width: 32,
            height: 32,
            rgba: [0, 0, 0, 255].repeat(32 * 32),
        };
        assert!(frame_is_near_black_or_flat(&black));

        // 2. Dark background with bright foreground stripe (dark-mode title card) -> NOT flat.
        let mut dark_with_text = [8, 8, 12, 255].repeat(32 * 32);
        for px in dark_with_text.as_chunks_mut::<4>().0.iter_mut().take(128) {
            *px = [240, 240, 245, 255];
        }
        let title_card = RgbaFrame {
            width: 32,
            height: 32,
            rgba: dark_with_text,
        };
        assert!(!frame_is_near_black_or_flat(&title_card));

        if !ffmpeg_available() || !ffprobe_available() {
            return;
        }

        // 3. Real 3-second fade-in-from-black MP4 -> poster probes forward past black t=0.0s,
        //    while initial playback state remains at position_secs == 0.0.
        let tmp = TempDirGuard::new("fade_in_video");
        let mp4_path = tmp.path.join("fade_in.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=15:duration=3",
                "-vf",
                "fade=t=in:st=0:d=0.8",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&mp4_path)
            .status()
            .expect("generate fade-in mp4");
        assert!(status.success(), "ffmpeg fade-in clip generation failed");

        let mut backend = MediaViewerBackend::new();
        backend.request_load_local(99, &mp4_path, MediaKind::Video);
        let loaded = wait_for_event(&mut backend, Duration::from_secs(5)).expect("video loaded");
        match loaded {
            MediaEvent::VideoLoaded {
                tab_id,
                poster_frame,
                state,
                ..
            } => {
                assert_eq!(tab_id, 99);
                assert!((state.position_secs - 0.0).abs() < f64::EPSILON);
                let poster = poster_frame.expect("non-black poster frame");
                assert!(
                    !frame_is_near_black_or_flat(&poster),
                    "expected non-black poster frame for fade-in video"
                );
            }
            other => panic!("expected VideoLoaded, got {other:?}"),
        }
        backend.close_tab(99);
    }
}
