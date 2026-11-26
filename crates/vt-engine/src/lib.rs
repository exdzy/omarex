//! Safe, zero-overhead Rust wrapper around Ghostty's `libghostty-vt` C ABI (`ghostty 1.4.0-1`).
//!
//! Designed for `omarex`'s two-phase lock-minimal GPU rendering architecture:
//! - [`Terminal`] wraps `GhosttyTerminal` behind a short `parking_lot::Mutex` with process-global
//!   PNG decoding (`GHOSTTY_SYS_OPT_DECODE_PNG`), `WRITE_PTY = NULL` (no duplicate query replies),
//!   authenticated `rex1;<nonce>;<json>` APC extraction, OSC 7501/7577/8/9/777/52/5522 callbacks,
//!   DEC 2026 synchronized-output render hold (1000ms safety timeout), and Mode 2031 theme tracking.
//! - [`RenderState`] performs microsecond-locked [`RenderState::begin_update`] followed by
//!   completely lock-free [`RenderState::end_update`] and zero-allocation dirty-row / `RowId`
//!   cell iteration.
//! - [`snapshot::SnapshotDecoder`] streams `GHOSTSNP` v1 chunks with `COMPRESS_HISTORY = true`.

pub mod ffi;
pub mod images;
pub mod input;
pub mod selection;
pub mod snapshot;

use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use thiserror::Error;

pub use images::{
    KittyImageLayer, KittyImagePixels, KittyPixelFormat, KittyPlacement, KittyPlacementKind,
    PixelRect, read_kitty_image, tick_kitty_animations,
};
pub use input::{
    KeyAction, KeyEncoder, KeyEventInput, KittyKeyboardFlags, Modifiers, MouseAction, MouseButton,
    MouseEncoder, MouseEventInput, MouseFormat, MouseGeometry, MouseTrackingMode, OptionAsAlt,
    PhysicalKey, encode_color_scheme_report, encode_focus, encode_paste, is_paste_safe,
    should_bypass_mouse_reporting,
};
pub use selection::{
    CharCellOrigin, RowCellSpan, SearchScroll, SearchStatus, SelectionAutoscroll,
    SelectionBehavior, SelectionGeometry, SelectionGesture, TerminalSearch, UnwrappedLine,
    ViewportMatch, clear_selection, hyperlink_at, kitty_placement_at, logical_line_at,
    logical_line_with_tui_unwrap_at, select_all, select_line, select_word, selection_text,
    viewport_row_span_is_blank, viewport_text_lines,
};
pub use snapshot::{BackgroundStep, SnapshotDecoder, SnapshotFeedStatus};

/// Default Kitty graphics per-screen image storage limit (128 MiB).
pub const DEFAULT_KITTY_IMAGE_STORAGE_LIMIT: u64 = 128 * 1024 * 1024;
/// Default unknown APC/OSC capture limit (64 KiB) for `rex1;<nonce>;<json>` and OSC 7577.
pub const DEFAULT_UNKNOWN_MAX_BYTES: usize = 64 * 1024;
/// Safety timeout for DEC 2026 synchronized output (`RENDER_HOLD`), matching Ghostty/Rex.
pub const RENDER_HOLD_SAFETY_TIMEOUT: Duration = Duration::from_millis(1000);

/// `GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE` from `/usr/include/ghostty/vt/terminal.h`.
const GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE: ffi::GhosttyTerminalOption = 16;
/// `GHOSTTY_TERMINAL_DATA_KITTY_IMAGE_MEDIUM_FILE` from `/usr/include/ghostty/vt/terminal.h`.
const GHOSTTY_TERMINAL_DATA_KITTY_IMAGE_MEDIUM_FILE: ffi::GhosttyTerminalData = 27;

#[derive(Debug, Error)]
pub enum VtError {
    #[error("libghostty-vt call failed with result code {0}")]
    Ffi(i32),
    #[error("invalid UTF-8 returned by libghostty-vt: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("invalid GHOSTSNP snapshot: {0}")]
    InvalidSnapshot(&'static str),
    #[error("referenced terminal has already been dropped")]
    TerminalDropped,
}

/// Compile-time metadata reported by the linked `libghostty-vt.so`.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub version: String,
    pub simd: bool,
    pub kitty_graphics: bool,
}

/// Queries compile-time metadata from the linked `libghostty-vt.so`.
pub fn build_info() -> Result<BuildInfo, VtError> {
    let mut version_raw = ffi::GhosttyString {
        ptr: ptr::null(),
        len: 0,
    };
    let mut simd = false;
    let mut kitty_graphics = false;

    // SAFETY: Each out-pointer matches `/usr/include/ghostty/vt/build_info.h`.
    unsafe {
        let rc = ffi::ghostty_build_info(
            ffi::GHOSTTY_BUILD_INFO_VERSION_STRING,
            (&raw mut version_raw).cast(),
        );
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        let rc = ffi::ghostty_build_info(ffi::GHOSTTY_BUILD_INFO_SIMD, (&raw mut simd).cast());
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        let rc = ffi::ghostty_build_info(
            ffi::GHOSTTY_BUILD_INFO_KITTY_GRAPHICS,
            (&raw mut kitty_graphics).cast(),
        );
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
    }

    let version = if version_raw.len == 0 || version_raw.ptr.is_null() {
        String::new()
    } else {
        // SAFETY: Process-static byte slice returned by `ghostty_build_info`.
        let bytes = unsafe { std::slice::from_raw_parts(version_raw.ptr, version_raw.len) };
        std::str::from_utf8(bytes)?.to_owned()
    };

    Ok(BuildInfo {
        version,
        simd,
        kitty_graphics,
    })
}

/// Returns the static ABI type manifest JSON exported by `libghostty-vt`.
pub fn type_manifest_json() -> Result<&'static str, VtError> {
    // SAFETY: Returns a static NUL-terminated C string valid for process lifetime.
    let ptr = unsafe { ffi::ghostty_type_json() };
    if ptr.is_null() {
        return Err(VtError::Ffi(ffi::GHOSTTY_NO_VALUE));
    }
    // SAFETY: Non-null static NUL-terminated string.
    Ok(unsafe { CStr::from_ptr(ptr) }.to_str()?)
}

// ---------------------------------------------------------------------------
// Process-global sys callbacks (PNG decode + tracing log)
// ---------------------------------------------------------------------------

static SYS_INIT: OnceLock<()> = OnceLock::new();

/// Installs process-global `libghostty-vt` system callbacks (idempotent, thread-safe):
/// - `GHOSTTY_SYS_OPT_DECODE_PNG` using the `image` crate (required for Kitty `f=100` PNGs!)
/// - `GHOSTTY_SYS_OPT_LOG` forwarding internal diagnostics to `tracing`.
pub fn init_sys() {
    SYS_INIT.get_or_init(|| {
        // SAFETY: `decode_png_cb` and `sys_log_cb` are `'static` extern "C" function pointers
        // matching `GhosttySysDecodePngFn` and `GhosttySysLogFn` (`sys.h`). Passing the function
        // address directly matches C's `ghostty_sys_set(OPT, &fn)`.
        unsafe {
            let rc_png = ffi::ghostty_sys_set(
                ffi::GHOSTTY_SYS_OPT_DECODE_PNG,
                decode_png_cb as *const c_void,
            );
            if rc_png != ffi::GHOSTTY_SUCCESS {
                tracing::error!(rc = rc_png, "failed to register libghostty-vt PNG decoder");
            }
            let _ = ffi::ghostty_sys_set(ffi::GHOSTTY_SYS_OPT_LOG, sys_log_cb as *const c_void);
        }
    });
}

unsafe extern "C" fn decode_png_cb(
    _userdata: *mut c_void,
    allocator: *const ffi::GhosttyAllocator,
    data: *const u8,
    data_len: usize,
    out: *mut ffi::GhosttySysImage,
) -> bool {
    if data.is_null() || data_len == 0 || out.is_null() {
        return false;
    }
    let result = std::panic::catch_unwind(|| {
        // SAFETY: `data` is valid for `data_len` bytes during this synchronous callback (`sys.h:156`).
        let image_bytes = unsafe { std::slice::from_raw_parts(data, data_len) };
        // Auto-sniff PNG, JPEG, WebP, GIF, BMP so non-PNG Kitty `f=100` payloads also decode cleanly.
        let img = image::load_from_memory(image_bytes).ok()?;
        let rgba = img.into_rgba8();
        let (width, height) = rgba.dimensions();
        let raw = rgba.as_raw();
        let len = raw.len();
        if width == 0 || height == 0 || len == 0 {
            return None;
        }
        // SAFETY: `ghostty_alloc` allocates using `allocator` (which libghostty frees with the
        // same allocator when taking ownership on `true` return, `sys.h:128-131`).
        let dst = unsafe { ffi::ghostty_alloc(allocator, len) };
        if dst.is_null() {
            return None;
        }
        // SAFETY: `dst` has `len` writable bytes and does not overlap `raw`.
        unsafe {
            ptr::copy_nonoverlapping(raw.as_ptr(), dst, len);
            (*out).width = width;
            (*out).height = height;
            (*out).data = dst;
            (*out).data_len = len;
        }
        Some(())
    });
    matches!(result, Ok(Some(())))
}

unsafe extern "C" fn sys_log_cb(
    _userdata: *mut c_void,
    level: ffi::GhosttySysLogLevel,
    scope: *const u8,
    scope_len: usize,
    message: *const u8,
    message_len: usize,
) {
    let _ = std::panic::catch_unwind(|| {
        let scope_str = if scope.is_null() || scope_len == 0 {
            "libghostty"
        } else {
            // SAFETY: Borrowed byte slice valid for duration of callback.
            std::str::from_utf8(unsafe { std::slice::from_raw_parts(scope, scope_len) })
                .unwrap_or("libghostty")
        };
        let msg_str = if message.is_null() || message_len == 0 {
            ""
        } else {
            // SAFETY: Borrowed byte slice valid for duration of callback.
            std::str::from_utf8(unsafe { std::slice::from_raw_parts(message, message_len) })
                .unwrap_or("<non-utf8>")
        };
        match level {
            ffi::GHOSTTY_SYS_LOG_LEVEL_ERROR => tracing::error!(scope = scope_str, "{msg_str}"),
            ffi::GHOSTTY_SYS_LOG_LEVEL_WARNING => tracing::warn!(scope = scope_str, "{msg_str}"),
            ffi::GHOSTTY_SYS_LOG_LEVEL_INFO => tracing::info!(scope = scope_str, "{msg_str}"),
            _ => tracing::debug!(scope = scope_str, "{msg_str}"),
        }
    });
}

// ---------------------------------------------------------------------------
// Core value types (RGB, Theme, Scrollbar, Cursor, Events)
// ---------------------------------------------------------------------------

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub(crate) const fn from_ffi(c: ffi::GhosttyColorRgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }

    pub(crate) const fn to_ffi(self) -> ffi::GhosttyColorRgb {
        ffi::GhosttyColorRgb {
            r: self.r,
            g: self.g,
            b: self.b,
        }
    }

    /// Parses `#RRGGBB`, `RRGGBB`, `rgb:r/g/b`, or X11 names via `ghostty_color_parse`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut out = ffi::GhosttyColorRgb::default();
        // SAFETY: `s` is a valid UTF-8 slice.
        let rc =
            unsafe { ffi::ghostty_color_parse(s.as_ptr().cast::<c_char>(), s.len(), &raw mut out) };
        (rc == ffi::GHOSTTY_SUCCESS).then_some(Self::from_ffi(out))
    }
}

/// Terminal light/dark color scheme for Mode 2031 (`CSI ? 996 n` / `CSI ? 997 n`).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum ColorScheme {
    Light,
    #[default]
    Dark,
}

impl ColorScheme {
    pub(crate) const fn to_ffi(self) -> ffi::GhosttyColorScheme {
        match self {
            Self::Light => ffi::GHOSTTY_COLOR_SCHEME_LIGHT,
            Self::Dark => ffi::GHOSTTY_COLOR_SCHEME_DARK,
        }
    }
}

/// Theme colors applied to a [`Terminal`] (from Omarchy live theme or Rex `rex1;` `theme` APC).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalTheme {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    /// Optional sparse overrides `(palette_index, rgb)` over Ghostty's default 256-color palette.
    pub palette_overrides: Vec<(u8, Rgb)>,
    pub scheme: Option<ColorScheme>,
}

/// Configuration for creating or restoring a [`Terminal`].
#[derive(Debug, Clone)]
pub struct TerminalConfig {
    pub kitty_image_storage_limit: u64,
    /// Allow Kitty graphics file-medium (`t=f`) loading from the local filesystem.
    ///
    /// Defaults to `false` (safe for remote/untrusted blocks); enable only for trusted local host
    /// blocks.
    pub kitty_image_medium_file: bool,
    pub unknown_max_bytes: usize,
    pub compress_history: bool,
    pub scrollback_max_bytes: Option<usize>,
    pub initial_scheme: ColorScheme,
    /// Optional Rex `/api/data` `WELCOME.channel_nonce` used to authenticate `rex1;<nonce>;<json>` APCs.
    pub channel_nonce: Option<u64>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            kitty_image_storage_limit: DEFAULT_KITTY_IMAGE_STORAGE_LIMIT,
            kitty_image_medium_file: false,
            unknown_max_bytes: DEFAULT_UNKNOWN_MAX_BYTES,
            compress_history: true,
            scrollback_max_bytes: None,
            initial_scheme: ColorScheme::Dark,
            channel_nonce: None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ScrollViewport {
    Top,
    Bottom,
    Delta(isize),
    Row(usize),
    DeltaPrompt(isize),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressionResult {
    Unsupported,
    Pending,
    Complete,
}

/// Application-requested mouse pointer shape (OSC 22).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MouseShape(pub i32);

/// Authenticated in-band Rex control APC (`ESC _ rex1;<nonce>;<json> ESC \`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RexControlApc {
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        owner: Option<String>,
    },
    Theme {
        foreground: Option<String>,
        background: Option<String>,
        cursor: Option<String>,
        palette: Vec<String>,
        scheme: Option<String>,
    },
    ChildExited {
        exit_code: i32,
    },
    Unknown {
        kind: String,
        raw_json: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RexControlApcWire {
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_width_px: u32,
        #[serde(default)]
        cell_height_px: u32,
        #[serde(default)]
        owner: Option<String>,
    },
    Theme {
        #[serde(default)]
        foreground: Option<String>,
        #[serde(default)]
        background: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        palette: Vec<String>,
        #[serde(default)]
        scheme: Option<String>,
    },
    ChildExited {
        exit_code: i32,
    },
}

impl RexControlApc {
    /// Converts a `RexControlApc::Theme` into a [`TerminalTheme`] ready for [`Terminal::set_theme`].
    pub fn to_terminal_theme(&self) -> Option<TerminalTheme> {
        let Self::Theme {
            foreground,
            background,
            cursor,
            palette,
            scheme,
        } = self
        else {
            return None;
        };
        let mut overrides = Vec::with_capacity(palette.len());
        for entry in palette {
            let mut idx: u8 = 0;
            let mut rgb = ffi::GhosttyColorRgb::default();
            // SAFETY: `entry` is a valid UTF-8 byte slice.
            let rc = unsafe {
                ffi::ghostty_color_parse_palette_entry(
                    entry.as_ptr().cast::<c_char>(),
                    entry.len(),
                    &raw mut idx,
                    &raw mut rgb,
                )
            };
            if rc == ffi::GHOSTTY_SUCCESS {
                overrides.push((idx, Rgb::from_ffi(rgb)));
            }
        }
        let parsed_scheme = match scheme.as_deref() {
            Some("light") => Some(ColorScheme::Light),
            Some("dark") => Some(ColorScheme::Dark),
            _ => None,
        };
        Some(TerminalTheme {
            foreground: foreground.as_deref().and_then(Rgb::parse),
            background: background.as_deref().and_then(Rgb::parse),
            cursor: cursor.as_deref().and_then(Rgb::parse),
            palette_overrides: overrides,
            scheme: parsed_scheme,
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProgramStatusState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProgramStatusKind {
    None,
    Permission,
    Question,
    Auth,
}

/// Parsed OSC 7501 program status callback report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatusReport {
    pub state: ProgramStatusState,
    pub kind: ProgramStatusKind,
    pub progress: Option<u8>,
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProgressState {
    Remove,
    Set,
    Error,
    Indeterminate,
    Pause,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ClipboardLocation {
    Standard,
    Selection,
    Primary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardEntry {
    pub mime: String,
    pub data: Vec<u8>,
}

/// Asynchronous terminal events captured by C callbacks during `Terminal::write()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    RexApc(RexControlApc),
    ProgramStatus(ProgramStatusReport),
    DesktopNotification {
        title: String,
        body: String,
    },
    ProgressReport {
        state: ProgressState,
        progress: Option<u8>,
    },
    ClipboardWrite {
        location: ClipboardLocation,
        entries: Vec<ClipboardEntry>,
    },
    TitleChanged(String),
    PwdChanged(String),
    Bell,
    RenderHoldChanged(bool),
    /// Unknown OSC (such as OSC 7577 program resume or custom extensions).
    UnknownOsc {
        content: String,
        truncated: bool,
    },
}

// ---------------------------------------------------------------------------
// Terminal internal state & C callbacks
// ---------------------------------------------------------------------------

struct TerminalCallbackState {
    channel_nonce: Option<u64>,
    color_scheme: ColorScheme,
    render_hold_since: Option<Instant>,
    events: Vec<TerminalEvent>,
}

/// Zero-allocation incremental parser tracking DECSTBM (`CSI Pt ; Pb r`) vertical scroll
/// regions across live VT byte chunks so the renderer can clip fixed Kitty placements inside
/// scrolling TUI regions (such as `tr3x`'s chat pane above its pinned bottom composer).
#[derive(Debug, Default)]
struct ScrollRegionTracker {
    /// Active 0-based inclusive `(top, bottom)` scroll region on the primary screen, if strictly
    /// smaller than the full viewport.
    region: Option<(u16, u16)>,
    state: DecstbmParseState,
    params: [u16; 4],
    param_count: usize,
    cur_param: u16,
    has_cur_param: bool,
    private_prefix: bool,
    bang_intermediate: bool,
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
enum DecstbmParseState {
    #[default]
    Ground,
    Esc,
    Csi,
    /// Inside an OSC/DCS/APC/PM/SOS string (`ESC ]`, `ESC P`, `ESC _`, `ESC ^`, `ESC X`) until ST/BEL.
    String,
    StringEsc,
}

impl ScrollRegionTracker {
    #[allow(dead_code)]
    fn reset_region(&mut self) {
        self.region = None;
    }

    fn feed(&mut self, bytes: &[u8], term_rows: u16) {
        for &b in bytes {
            match self.state {
                DecstbmParseState::Ground => {
                    if b == 0x1b {
                        self.state = DecstbmParseState::Esc;
                    }
                }
                DecstbmParseState::Esc => match b {
                    b'[' => {
                        self.state = DecstbmParseState::Csi;
                        self.params = [0; 4];
                        self.param_count = 0;
                        self.cur_param = 0;
                        self.has_cur_param = false;
                        self.private_prefix = false;
                        self.bang_intermediate = false;
                    }
                    b']' | b'P' | b'_' | b'^' | b'X' => {
                        self.state = DecstbmParseState::String;
                    }
                    b'c' => {
                        // RIS (Full Reset) clears scroll region.
                        self.region = None;
                        self.state = DecstbmParseState::Ground;
                    }
                    0x1b => {
                        self.state = DecstbmParseState::Esc;
                    }
                    _ => {
                        self.state = DecstbmParseState::Ground;
                    }
                },
                DecstbmParseState::String => match b {
                    0x07 => self.state = DecstbmParseState::Ground,
                    0x1b => self.state = DecstbmParseState::StringEsc,
                    _ => {}
                },
                DecstbmParseState::StringEsc => match b {
                    b'\\' => self.state = DecstbmParseState::Ground,
                    0x1b => self.state = DecstbmParseState::StringEsc,
                    _ => self.state = DecstbmParseState::String,
                },
                DecstbmParseState::Csi => match b {
                    b'0'..=b'9' => {
                        self.cur_param = self
                            .cur_param
                            .saturating_mul(10)
                            .saturating_add(u16::from(b - b'0'));
                        self.has_cur_param = true;
                    }
                    b';' => {
                        if self.param_count < self.params.len() {
                            self.params[self.param_count] = self.cur_param;
                            self.param_count += 1;
                        }
                        self.cur_param = 0;
                        self.has_cur_param = false;
                    }
                    b'?' | b'>' | b'<' | b'=' => {
                        self.private_prefix = true;
                    }
                    b'!' => {
                        self.bang_intermediate = true;
                    }
                    0x20..=0x2f => {
                        // Other intermediate bytes
                    }
                    0x40..=0x7e => {
                        if self.has_cur_param && self.param_count < self.params.len() {
                            self.params[self.param_count] = self.cur_param;
                            self.param_count += 1;
                        }
                        if b == b'r' && !self.private_prefix && !self.bang_intermediate {
                            self.apply_decstbm(term_rows);
                        } else if b == b'p' && self.bang_intermediate {
                            // DECSTR (`CSI ! p`) resets scroll margins.
                            self.region = None;
                        }
                        self.state = DecstbmParseState::Ground;
                    }
                    0x1b => {
                        self.state = DecstbmParseState::Esc;
                    }
                    _ => {
                        self.state = DecstbmParseState::Ground;
                    }
                },
            }
        }
    }

    fn apply_decstbm(&mut self, term_rows: u16) {
        if term_rows < 2 {
            self.region = None;
            return;
        }
        let raw_top = if self.param_count >= 1 && self.params[0] > 0 {
            self.params[0]
        } else {
            1
        };
        let raw_bottom = if self.param_count >= 2 && self.params[1] > 0 {
            self.params[1].min(term_rows)
        } else {
            term_rows
        };
        if raw_top >= raw_bottom {
            self.region = None;
            return;
        }
        let top0 = raw_top - 1;
        let bottom0 = raw_bottom - 1;
        if top0 == 0 && bottom0 + 1 >= term_rows {
            self.region = None;
        } else {
            self.region = Some((top0, bottom0));
        }
    }
}

/// Queries the terminal's active DECSTBM scrolling region once via `libghostty-vt`'s VT formatter
/// (restricted to a 1-cell selection so formatting only emits the tiny trailing `\x1b[t;br`).
fn query_initial_scroll_region(raw: ffi::GhosttyTerminal) -> Option<(u16, u16)> {
    let mut rows: u16 = 0;
    // SAFETY: Caller owns/locks `raw`.
    let rc_r = unsafe {
        ffi::ghostty_terminal_get(raw, ffi::GHOSTTY_TERMINAL_DATA_ROWS, (&raw mut rows).cast())
    };
    if rc_r != ffi::GHOSTTY_SUCCESS || rows < 2 {
        return None;
    }

    // Restrict formatting to a single cell so even terminals with 100k scrollback lines run in ~µs.
    let mut cell_ref = ffi::GhosttyGridRef::default();
    let have_ref = unsafe {
        ffi::ghostty_terminal_grid_ref(raw, ffi::GhosttyPoint::active(0, 0), &raw mut cell_ref)
    } == ffi::GHOSTTY_SUCCESS;
    let one_cell_sel = ffi::GhosttySelection {
        size: std::mem::size_of::<ffi::GhosttySelection>(),
        start: cell_ref,
        end: cell_ref,
        rectangle: false,
    };

    let mut opts = ffi::GhosttyFormatterTerminalOptions {
        emit: ffi::GHOSTTY_FORMATTER_FORMAT_VT,
        trim: true,
        selection: if have_ref {
            &raw const one_cell_sel
        } else {
            ptr::null()
        },
        ..Default::default()
    };
    opts.extra.scrolling_region = true;

    let mut formatter: ffi::GhosttyFormatter = ptr::null_mut();
    // SAFETY: `raw` is live and locked; `opts` has initialized `.size` fields.
    let rc =
        unsafe { ffi::ghostty_formatter_terminal_new(ptr::null(), &raw mut formatter, raw, opts) };
    if rc != ffi::GHOSTTY_SUCCESS || formatter.is_null() {
        return None;
    }

    let mut buf = [0u8; 512];
    let mut written: usize = 0;
    let rc_fmt = unsafe {
        ffi::ghostty_formatter_format_buf(formatter, buf.as_mut_ptr(), buf.len(), &raw mut written)
    };
    let slice: Vec<u8> = if rc_fmt == ffi::GHOSTTY_SUCCESS {
        buf[..written].to_vec()
    } else if rc_fmt == ffi::GHOSTTY_OUT_OF_SPACE && written > 0 && written <= 64 * 1024 {
        let mut heap = vec![0u8; written];
        let mut actual = 0;
        let rc2 = unsafe {
            ffi::ghostty_formatter_format_buf(
                formatter,
                heap.as_mut_ptr(),
                heap.len(),
                &raw mut actual,
            )
        };
        if rc2 == ffi::GHOSTTY_SUCCESS {
            heap.truncate(actual);
            heap
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    unsafe { ffi::ghostty_formatter_free(formatter) };

    let mut tracker = ScrollRegionTracker::default();
    tracker.feed(&slice, rows);
    tracker.region
}

pub(crate) struct TerminalInner {
    pub(crate) raw: ffi::GhosttyTerminal,
    /// Pinned heap state passed via `GHOSTTY_TERMINAL_OPT_USERDATA`. Must drop AFTER `raw`.
    cb_state: Box<TerminalCallbackState>,
    scroll_region: ScrollRegionTracker,
}

// SAFETY: `TerminalInner` exclusively owns `raw` (`GhosttyTerminal`) and `cb_state` and is only
// ever accessed through `parking_lot::Mutex<TerminalInner>`, serializing all C calls.
unsafe impl Send for TerminalInner {}

impl Drop for TerminalInner {
    fn drop(&mut self) {
        // SAFETY: Free `GhosttyTerminal` BEFORE dropping `cb_state` so userdata is always valid.
        unsafe { ffi::ghostty_terminal_free(self.raw) };
    }
}

unsafe fn ghostty_str_to_string(s: ffi::GhosttyString) -> String {
    if s.ptr.is_null() || s.len == 0 {
        return String::new();
    }
    // SAFETY: Caller guarantees borrowed `GhosttyString` is valid for this callback duration.
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

unsafe extern "C" fn cb_unknown_sequence(
    _term: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    seq: *const ffi::GhosttyTerminalUnknownSequence,
) {
    if userdata.is_null() || seq.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: `userdata` points to pinned `TerminalCallbackState`; `seq` is valid for callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        let seq = unsafe { &*seq };
        match seq.tag {
            ffi::GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_APC => {
                // SAFETY: `tag == APC` selects the `apc` union arm.
                let apc = unsafe { seq.value.apc };
                if apc.truncated || apc.content.ptr.is_null() || apc.content.len == 0 {
                    return;
                }
                let bytes = unsafe { std::slice::from_raw_parts(apc.content.ptr, apc.content.len) };
                let Ok(text) = std::str::from_utf8(bytes) else {
                    return;
                };
                if let Some(rest) = text.strip_prefix("rex1;") {
                    let Some((nonce_str, json_str)) = rest.split_once(';') else {
                        return;
                    };
                    let Ok(nonce) = nonce_str.parse::<u64>() else {
                        return;
                    };
                    if let Some(expected) = state.channel_nonce
                        && nonce != expected
                    {
                        tracing::warn!(
                            received = nonce,
                            "ignoring spoofed rex1 APC with mismatched channel_nonce"
                        );
                        return;
                    }
                    let parsed = match serde_json::from_str::<RexControlApcWire>(json_str) {
                        Ok(RexControlApcWire::Resize {
                            cols,
                            rows,
                            cell_width_px,
                            cell_height_px,
                            owner,
                        }) => RexControlApc::Resize {
                            cols,
                            rows,
                            cell_width_px,
                            cell_height_px,
                            owner,
                        },
                        Ok(RexControlApcWire::Theme {
                            foreground,
                            background,
                            cursor,
                            palette,
                            scheme,
                        }) => RexControlApc::Theme {
                            foreground,
                            background,
                            cursor,
                            palette,
                            scheme,
                        },
                        Ok(RexControlApcWire::ChildExited { exit_code }) => {
                            RexControlApc::ChildExited { exit_code }
                        }
                        Err(_) => {
                            let kind = serde_json::from_str::<serde_json::Value>(json_str)
                                .ok()
                                .and_then(|v| v.get("type")?.as_str().map(str::to_owned))
                                .unwrap_or_else(|| "unknown".to_owned());
                            RexControlApc::Unknown {
                                kind,
                                raw_json: json_str.to_owned(),
                            }
                        }
                    };
                    state.events.push(TerminalEvent::RexApc(parsed));
                }
            }
            ffi::GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_OSC => {
                // SAFETY: `tag == OSC` selects the `osc` union arm.
                let osc = unsafe { seq.value.osc };
                let content = unsafe { ghostty_str_to_string(osc.content) };
                state.events.push(TerminalEvent::UnknownOsc {
                    content,
                    truncated: osc.truncated,
                });
            }
            _ => {}
        }
    });
}

unsafe extern "C" fn cb_program_status(
    _term: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    report: *const ffi::GhosttyTerminalProgramStatus,
) {
    if userdata.is_null() || report.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Valid `TerminalCallbackState` and `GhosttyTerminalProgramStatus` for callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        let r = unsafe { &*report };
        let st = match r.state {
            ffi::GHOSTTY_PROGRAM_STATUS_STATE_IDLE => ProgramStatusState::Idle,
            ffi::GHOSTTY_PROGRAM_STATUS_STATE_WORKING => ProgramStatusState::Working,
            ffi::GHOSTTY_PROGRAM_STATUS_STATE_DONE => ProgramStatusState::Done,
            ffi::GHOSTTY_PROGRAM_STATUS_STATE_BLOCKED => ProgramStatusState::Blocked,
            ffi::GHOSTTY_PROGRAM_STATUS_STATE_ERROR => ProgramStatusState::Error,
            _ => ProgramStatusState::Clear,
        };
        let kind = match r.kind {
            ffi::GHOSTTY_PROGRAM_STATUS_KIND_PERMISSION => ProgramStatusKind::Permission,
            ffi::GHOSTTY_PROGRAM_STATUS_KIND_QUESTION => ProgramStatusKind::Question,
            ffi::GHOSTTY_PROGRAM_STATUS_KIND_AUTH => ProgramStatusKind::Auth,
            _ => ProgramStatusKind::None,
        };
        let progress = (r.progress >= 0).then_some(r.progress as u8);
        state
            .events
            .push(TerminalEvent::ProgramStatus(ProgramStatusReport {
                state: st,
                kind,
                progress,
                id: unsafe { ghostty_str_to_string(r.id) },
                app: unsafe { ghostty_str_to_string(r.app) },
                title: unsafe { ghostty_str_to_string(r.title) },
                message: unsafe { ghostty_str_to_string(r.message) },
            }));
    });
}

unsafe extern "C" fn cb_desktop_notification(
    _term: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    notif: *const ffi::GhosttyTerminalDesktopNotification,
) {
    if userdata.is_null() || notif.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Valid pointers during callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        let n = unsafe { &*notif };
        state.events.push(TerminalEvent::DesktopNotification {
            title: unsafe { ghostty_str_to_string(n.title) },
            body: unsafe { ghostty_str_to_string(n.body) },
        });
    });
}

unsafe extern "C" fn cb_progress_report(
    _term: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    report: *const ffi::GhosttyTerminalProgressReport,
) {
    if userdata.is_null() || report.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Valid pointers during callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        let r = unsafe { &*report };
        let ps = match r.state {
            ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE => ProgressState::Remove,
            ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_SET => ProgressState::Set,
            ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR => ProgressState::Error,
            ffi::GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE => ProgressState::Indeterminate,
            _ => ProgressState::Pause,
        };
        state.events.push(TerminalEvent::ProgressReport {
            state: ps,
            progress: (r.progress >= 0).then_some(r.progress as u8),
        });
    });
}

unsafe extern "C" fn cb_clipboard_write(
    _term: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    write: *const ffi::GhosttyClipboardWrite,
) {
    if userdata.is_null() || write.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: `write` and its `contents` slice are borrowed for this synchronous callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        let w = unsafe { &*write };
        let location = match w.location {
            ffi::GHOSTTY_CLIPBOARD_LOCATION_SELECTION => ClipboardLocation::Selection,
            ffi::GHOSTTY_CLIPBOARD_LOCATION_PRIMARY => ClipboardLocation::Primary,
            _ => ClipboardLocation::Standard,
        };
        let mut entries = Vec::with_capacity(w.contents_len);
        if !w.contents.is_null() && w.contents_len > 0 {
            let slice = unsafe { std::slice::from_raw_parts(w.contents, w.contents_len) };
            for item in slice {
                let mime = unsafe { ghostty_str_to_string(item.mime) };
                let data = if item.data.ptr.is_null() || item.data.len == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(item.data.ptr, item.data.len) }.to_vec()
                };
                entries.push(ClipboardEntry { mime, data });
            }
        }
        state
            .events
            .push(TerminalEvent::ClipboardWrite { location, entries });

        // Must answer synchronously within the callback (`terminal.h:1050-1054`).
        if let Some(reply_fn) = w.reply {
            let reply = ffi::GhosttyClipboardWriteReply {
                size: std::mem::size_of::<ffi::GhosttyClipboardWriteReply>(),
                result: ffi::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS,
                remember: false,
            };
            unsafe { reply_fn(write, &raw const reply) };
        }
    });
}

unsafe extern "C" fn cb_title_changed(term: ffi::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Valid `userdata` and `term` during callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        let mut s = ffi::GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        let rc = unsafe {
            ffi::ghostty_terminal_get(term, ffi::GHOSTTY_TERMINAL_DATA_TITLE, (&raw mut s).cast())
        };
        if rc == ffi::GHOSTTY_SUCCESS {
            let title = unsafe { ghostty_str_to_string(s) };
            state.events.push(TerminalEvent::TitleChanged(title));
        }
    });
}

unsafe extern "C" fn cb_pwd_changed(term: ffi::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Valid `userdata` and `term` during callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        let mut s = ffi::GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        let rc = unsafe {
            ffi::ghostty_terminal_get(term, ffi::GHOSTTY_TERMINAL_DATA_PWD, (&raw mut s).cast())
        };
        if rc == ffi::GHOSTTY_SUCCESS {
            let pwd = unsafe { ghostty_str_to_string(s) };
            state.events.push(TerminalEvent::PwdChanged(pwd));
        }
    });
}

unsafe extern "C" fn cb_bell(_term: ffi::GhosttyTerminal, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Valid `userdata` during callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        state.events.push(TerminalEvent::Bell);
    });
}

unsafe extern "C" fn cb_render_hold(
    _term: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    held: bool,
) {
    if userdata.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: Valid `userdata` during callback.
        let state = unsafe { &mut *userdata.cast::<TerminalCallbackState>() };
        state.render_hold_since = held.then(Instant::now);
        state.events.push(TerminalEvent::RenderHoldChanged(held));
    });
}

unsafe extern "C" fn cb_color_scheme(
    _term: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    out_scheme: *mut ffi::GhosttyColorScheme,
) -> bool {
    if userdata.is_null() || out_scheme.is_null() {
        return false;
    }
    // SAFETY: `userdata` is valid and `out_scheme` is non-null.
    let state = unsafe { &*userdata.cast::<TerminalCallbackState>() };
    unsafe { *out_scheme = state.color_scheme.to_ffi() };
    true
}

// ---------------------------------------------------------------------------
// Safe `Terminal` handle
// ---------------------------------------------------------------------------

/// Safe, thread-safe (`Send + Sync`) wrapper around `GhosttyTerminal`.
///
/// Cloning a `Terminal` cheaply clones its internal `Arc<Mutex<_>>` handle so IO/VT threads
/// and the GPU render thread can share it safely with microsecond lock durations.
#[derive(Clone)]
pub struct Terminal {
    pub(crate) inner: Arc<Mutex<TerminalInner>>,
}

// SAFETY: Every access to `GhosttyTerminal` is strictly serialized by `parking_lot::Mutex`.
unsafe impl Send for Terminal {}
unsafe impl Sync for Terminal {}

impl Terminal {
    /// Creates a new `cols x rows` terminal with PNG decoding, Kitty storage limit,
    /// `WRITE_PTY = NULL`, `rex1` APC extraction, OSC callbacks, and DEC 2026 render hold.
    pub fn new(cols: u16, rows: u16, config: TerminalConfig) -> Result<Self, VtError> {
        init_sys();
        let mut raw: ffi::GhosttyTerminal = ptr::null_mut();
        // SAFETY: NULL allocator selects default allocator.
        let rc = unsafe { ffi::ghostty_terminal_new(ptr::null(), &raw mut raw, cols, rows) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Self::from_raw(raw, config)
    }

    pub(crate) fn from_raw(
        raw: ffi::GhosttyTerminal,
        config: TerminalConfig,
    ) -> Result<Self, VtError> {
        init_sys();
        let mut cb_state = Box::new(TerminalCallbackState {
            channel_nonce: config.channel_nonce,
            color_scheme: config.initial_scheme,
            render_hold_since: None,
            events: Vec::new(),
        });
        let userdata_ptr = (&raw mut *cb_state).cast::<c_void>();

        // SAFETY: All function pointers match exact C callback typedefs in `terminal.h`, and
        // `userdata_ptr` points to pinned `cb_state` which drops strictly after `raw`.
        unsafe {
            // Explicitly set WRITE_PTY = NULL so client never sends duplicate replies to Rex server!
            let _ =
                ffi::ghostty_terminal_set(raw, ffi::GHOSTTY_TERMINAL_OPT_WRITE_PTY, ptr::null());
            let _ =
                ffi::ghostty_terminal_set(raw, ffi::GHOSTTY_TERMINAL_OPT_USERDATA, userdata_ptr);

            let limit = config.kitty_image_storage_limit;
            let rc_kitty = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
                (&raw const limit).cast(),
            );
            if rc_kitty != ffi::GHOSTTY_SUCCESS {
                ffi::ghostty_terminal_free(raw);
                return Err(VtError::Ffi(rc_kitty));
            }

            let medium_file = config.kitty_image_medium_file;
            let _ = ffi::ghostty_terminal_set(
                raw,
                GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE,
                (&raw const medium_file).cast(),
            );

            let max_unk = config.unknown_max_bytes;
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES,
                (&raw const max_unk).cast(),
            );
            if let Some(sb_bytes) = config.scrollback_max_bytes {
                let _ = ffi::ghostty_terminal_set(
                    raw,
                    ffi::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                    (&raw const sb_bytes).cast(),
                );
            }

            // Register callbacks (direct function pointer cast to `*const c_void`).
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE,
                cb_unknown_sequence as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS,
                cb_program_status as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
                cb_desktop_notification as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT,
                cb_progress_report as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                cb_clipboard_write as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                cb_title_changed as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_PWD_CHANGED,
                cb_pwd_changed as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_BELL,
                cb_bell as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_RENDER_HOLD,
                cb_render_hold as *const c_void,
            );
            let _ = ffi::ghostty_terminal_set(
                raw,
                ffi::GHOSTTY_TERMINAL_OPT_COLOR_SCHEME,
                cb_color_scheme as *const c_void,
            );
        }

        let scroll_region = ScrollRegionTracker {
            region: query_initial_scroll_region(raw),
            ..Default::default()
        };

        Ok(Self {
            inner: Arc::new(Mutex::new(TerminalInner {
                raw,
                cb_state,
                scroll_region,
            })),
        })
    }

    /// Sets the expected Rex `/api/data` `WELCOME.channel_nonce` for authenticating `rex1;` APCs.
    pub fn set_channel_nonce(&self, nonce: Option<u64>) {
        self.inner.lock().cb_state.channel_nonce = nonce;
    }

    /// Enables or disables Kitty graphics file-medium (`t=f`) loading for this terminal.
    ///
    /// Should only be enabled for trusted local-host blocks; keep disabled (`false`) for remote
    /// blocks.
    pub fn set_kitty_image_medium_file(&self, enabled: bool) -> Result<(), VtError> {
        let guard = self.inner.lock();
        // SAFETY: `enabled` matches `bool *` for `GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE`.
        let rc = unsafe {
            ffi::ghostty_terminal_set(
                guard.raw,
                GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE,
                (&raw const enabled).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(())
    }

    /// Queries whether Kitty graphics file-medium (`t=f`) loading is currently enabled.
    pub fn kitty_image_medium_file(&self) -> Result<bool, VtError> {
        let guard = self.inner.lock();
        let mut enabled = false;
        // SAFETY: `enabled` matches `bool *` for `GHOSTTY_TERMINAL_DATA_KITTY_IMAGE_MEDIUM_FILE`.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                GHOSTTY_TERMINAL_DATA_KITTY_IMAGE_MEDIUM_FILE,
                (&raw mut enabled).cast(),
            )
        };
        match rc {
            ffi::GHOSTTY_SUCCESS => Ok(enabled),
            ffi::GHOSTTY_NO_VALUE => Ok(false),
            other => Err(VtError::Ffi(other)),
        }
    }

    /// Feeds raw VT output bytes into the terminal emulator.
    pub fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut guard = self.inner.lock();
        // SAFETY: `guard.raw` is locked and `bytes` is valid for `bytes.len()`.
        unsafe { ffi::ghostty_terminal_vt_write(guard.raw, bytes.as_ptr(), bytes.len()) };
        let mut rows: u16 = 0;
        let rc_r = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_ROWS,
                (&raw mut rows).cast(),
            )
        };
        if rc_r == ffi::GHOSTTY_SUCCESS {
            guard.scroll_region.feed(bytes, rows);
        }
    }

    /// Resizes the terminal grid and cell pixel geometry.
    pub fn resize(
        &self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<(), VtError> {
        let mut guard = self.inner.lock();
        // SAFETY: `guard.raw` is locked.
        let rc = unsafe {
            ffi::ghostty_terminal_resize(guard.raw, cols, rows, cell_width_px, cell_height_px)
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        guard.scroll_region.region = query_initial_scroll_region(guard.raw);
        Ok(())
    }

    /// Applies foreground, background, cursor, 256-color palette, and Mode 2031 color scheme.
    pub fn set_theme(&self, theme: &TerminalTheme) -> Result<(), VtError> {
        let mut guard = self.inner.lock();

        // SAFETY: All pointers match documented option types in `terminal.h:2299-2327`.
        unsafe {
            match theme.foreground {
                Some(rgb) => {
                    let c = rgb.to_ffi();
                    let rc = ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                        (&raw const c).cast(),
                    );
                    if rc != ffi::GHOSTTY_SUCCESS {
                        return Err(VtError::Ffi(rc));
                    }
                }
                None => {
                    let _ = ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                        ptr::null(),
                    );
                }
            }

            match theme.background {
                Some(rgb) => {
                    let c = rgb.to_ffi();
                    let rc = ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                        (&raw const c).cast(),
                    );
                    if rc != ffi::GHOSTTY_SUCCESS {
                        return Err(VtError::Ffi(rc));
                    }
                }
                None => {
                    let _ = ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                        ptr::null(),
                    );
                }
            }

            match theme.cursor {
                Some(rgb) => {
                    let c = rgb.to_ffi();
                    let rc = ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                        (&raw const c).cast(),
                    );
                    if rc != ffi::GHOSTTY_SUCCESS {
                        return Err(VtError::Ffi(rc));
                    }
                }
                None => {
                    let _ = ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                        ptr::null(),
                    );
                }
            }

            if !theme.palette_overrides.is_empty() {
                let mut pal = [ffi::GhosttyColorRgb::default(); 256];
                ffi::ghostty_color_palette_default(pal.as_mut_ptr());
                for &(idx, rgb) in &theme.palette_overrides {
                    pal[usize::from(idx)] = rgb.to_ffi();
                }
                let rc = ffi::ghostty_terminal_set(
                    guard.raw,
                    ffi::GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                    pal.as_ptr().cast(),
                );
                if rc != ffi::GHOSTTY_SUCCESS {
                    return Err(VtError::Ffi(rc));
                }
            }
        }

        if let Some(scheme) = theme.scheme {
            guard.cb_state.color_scheme = scheme;
        }
        Ok(())
    }

    /// Convenience helper applying a verified [`RexControlApc`] (`Resize` or `Theme`) to this terminal.
    pub fn apply_rex_apc(&self, apc: &RexControlApc) -> Result<(), VtError> {
        match apc {
            RexControlApc::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                ..
            } => self.resize(*cols, *rows, *cell_width_px, *cell_height_px),
            RexControlApc::Theme { .. } => {
                if let Some(theme) = apc.to_terminal_theme() {
                    self.set_theme(&theme)?;
                }
                Ok(())
            }
            RexControlApc::ChildExited { .. } | RexControlApc::Unknown { .. } => Ok(()),
        }
    }

    /// Scrolls the terminal viewport and clears any active DEC 2026 render hold immediately.
    pub fn scroll_viewport(&self, scroll: ScrollViewport) {
        let mut guard = self.inner.lock();
        guard.cb_state.render_hold_since = None;
        let behavior = match scroll {
            ScrollViewport::Top => ffi::GhosttyTerminalScrollViewport {
                tag: ffi::GHOSTTY_SCROLL_VIEWPORT_TOP,
                value: ffi::GhosttyTerminalScrollViewportValue { _padding: [0; 2] },
            },
            ScrollViewport::Bottom => ffi::GhosttyTerminalScrollViewport {
                tag: ffi::GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
                value: ffi::GhosttyTerminalScrollViewportValue { _padding: [0; 2] },
            },
            ScrollViewport::Delta(d) => ffi::GhosttyTerminalScrollViewport {
                tag: ffi::GHOSTTY_SCROLL_VIEWPORT_DELTA,
                value: ffi::GhosttyTerminalScrollViewportValue { delta: d },
            },
            ScrollViewport::Row(r) => ffi::GhosttyTerminalScrollViewport {
                tag: ffi::GHOSTTY_SCROLL_VIEWPORT_ROW,
                value: ffi::GhosttyTerminalScrollViewportValue { row: r },
            },
            ScrollViewport::DeltaPrompt(d) => ffi::GhosttyTerminalScrollViewport {
                tag: ffi::GHOSTTY_SCROLL_VIEWPORT_DELTA_PROMPT,
                value: ffi::GhosttyTerminalScrollViewportValue { delta_prompt: d },
            },
        };
        // SAFETY: `guard.raw` is locked.
        unsafe { ffi::ghostty_terminal_scroll_viewport(guard.raw, behavior) };
    }

    pub fn scrollbar(&self) -> Result<Scrollbar, VtError> {
        let guard = self.inner.lock();
        let mut raw = ffi::GhosttyTerminalScrollbar::default();
        // SAFETY: `raw` matches `GhosttyTerminalScrollbar *`.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                (&raw mut raw).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(Scrollbar {
            total: raw.total,
            offset: raw.offset,
            len: raw.len,
        })
    }

    pub fn viewport_is_active(&self) -> Result<bool, VtError> {
        let guard = self.inner.lock();
        let mut active = false;
        // SAFETY: `active` matches `bool *`.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE,
                (&raw mut active).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(active)
    }

    pub fn size(&self) -> Result<(u16, u16), VtError> {
        let guard = self.inner.lock();
        let mut cols: u16 = 0;
        let mut rows: u16 = 0;
        // SAFETY: `cols` and `rows` match `uint16_t *`.
        unsafe {
            let rc1 = ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_COLS,
                (&raw mut cols).cast(),
            );
            if rc1 != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc1));
            }
            let rc2 = ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_ROWS,
                (&raw mut rows).cast(),
            );
            if rc2 != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc2));
            }
        }
        Ok((cols, rows))
    }

    /// Returns the terminal's configured pixel dimensions `(width_px, height_px)` derived from its
    /// last `resize(..., cell_width_px, cell_height_px)` call (`0` if cell pixel size was never set).
    pub fn pixel_size(&self) -> Result<(u32, u32), VtError> {
        let guard = self.inner.lock();
        let mut width_px: u32 = 0;
        let mut height_px: u32 = 0;
        // SAFETY: `width_px` and `height_px` match `uint32_t *`.
        unsafe {
            let rc1 = ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_WIDTH_PX,
                (&raw mut width_px).cast(),
            );
            if rc1 != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc1));
            }
            let rc2 = ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_HEIGHT_PX,
                (&raw mut height_px).cast(),
            );
            if rc2 != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc2));
            }
        }
        Ok((width_px, height_px))
    }

    /// Returns the active DECSTBM vertical scrolling region `(top_row, bottom_row)` (0-based
    /// inclusive) when a proper sub-screen margin is active on the primary screen.
    pub fn scroll_region(&self) -> Option<(u16, u16)> {
        let guard = self.inner.lock();
        let mut screen: ffi::GhosttyTerminalScreen = ffi::GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                (&raw mut screen).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS || screen != ffi::GHOSTTY_TERMINAL_SCREEN_PRIMARY {
            return None;
        }
        guard.scroll_region.region
    }

    pub fn mode(&self, value: u16, ansi: bool) -> Result<bool, VtError> {
        let guard = self.inner.lock();
        let mut cfg = ffi::GhosttyTerminalModeConfig {
            mode: ffi::ghostty_mode_new(value, ansi),
            value: false,
        };
        // SAFETY: `GHOSTTY_TERMINAL_DATA_MODE` reads `cfg.mode` and writes `cfg.value`.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_MODE,
                (&raw mut cfg).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(cfg.value)
    }

    /// Returns the active mouse tracking mode (checking DEC modes 1003, 1002, 1000, 9).
    pub fn mouse_tracking_mode(&self) -> Result<MouseTrackingMode, VtError> {
        let guard = self.inner.lock();
        let mut tracking = false;
        // SAFETY: `tracking` matches `bool *`.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
                (&raw mut tracking).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        if !tracking {
            return Ok(MouseTrackingMode::None);
        }
        let check = |mode: ffi::GhosttyMode| -> bool {
            let mut cfg = ffi::GhosttyTerminalModeConfig { mode, value: false };
            // SAFETY: `guard.raw` is locked.
            unsafe {
                ffi::ghostty_terminal_get(
                    guard.raw,
                    ffi::GHOSTTY_TERMINAL_DATA_MODE,
                    (&raw mut cfg).cast(),
                ) == ffi::GHOSTTY_SUCCESS
                    && cfg.value
            }
        };
        if check(ffi::GHOSTTY_MODE_ANY_MOUSE) {
            Ok(MouseTrackingMode::Any)
        } else if check(ffi::GHOSTTY_MODE_BUTTON_MOUSE) {
            Ok(MouseTrackingMode::Button)
        } else if check(ffi::GHOSTTY_MODE_NORMAL_MOUSE) {
            Ok(MouseTrackingMode::Normal)
        } else if check(ffi::GHOSTTY_MODE_X10_MOUSE) {
            Ok(MouseTrackingMode::X10)
        } else {
            Ok(MouseTrackingMode::Normal)
        }
    }

    /// Returns the active mouse encoding format (SGR-Pixels, SGR, URXVT, UTF-8, or X10).
    pub fn mouse_format(&self) -> Result<MouseFormat, VtError> {
        if self.mode(1016, false).unwrap_or(false) {
            Ok(MouseFormat::SgrPixels)
        } else if self.mode(1006, false).unwrap_or(false) {
            Ok(MouseFormat::Sgr)
        } else if self.mode(1015, false).unwrap_or(false) {
            Ok(MouseFormat::Urxvt)
        } else if self.mode(1005, false).unwrap_or(false) {
            Ok(MouseFormat::Utf8)
        } else {
            Ok(MouseFormat::X10)
        }
    }

    /// Queries the current Kitty keyboard protocol flags bitmask.
    pub fn kitty_keyboard_flags(&self) -> Result<KittyKeyboardFlags, VtError> {
        let guard = self.inner.lock();
        let mut flags: ffi::GhosttyKittyKeyFlags = 0;
        // SAFETY: `flags` matches `GhosttyKittyKeyFlags * (uint8_t *)`.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS,
                (&raw mut flags).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(KittyKeyboardFlags(flags))
    }

    pub fn is_mode_2031_enabled(&self) -> bool {
        self.mode(2031, false).unwrap_or(false)
    }

    pub fn color_scheme(&self) -> ColorScheme {
        self.inner.lock().cb_state.color_scheme
    }

    pub fn set_color_scheme(&self, scheme: ColorScheme) {
        self.inner.lock().cb_state.color_scheme = scheme;
    }

    /// Returns `true` while DEC 2026 synchronized output is holding frames (auto-clears after 1000ms).
    pub fn is_render_held(&self) -> bool {
        let mut guard = self.inner.lock();
        match guard.cb_state.render_hold_since {
            Some(since) if since.elapsed() < RENDER_HOLD_SAFETY_TIMEOUT => true,
            Some(_) => {
                guard.cb_state.render_hold_since = None;
                false
            }
            None => false,
        }
    }

    /// Immediately clears any DEC 2026 render hold (e.g. on user scroll or selection).
    pub fn clear_render_hold(&self) {
        self.inner.lock().cb_state.render_hold_since = None;
    }

    pub fn mouse_shape(&self) -> Result<MouseShape, VtError> {
        let guard = self.inner.lock();
        let mut shape: ffi::GhosttyMouseShape = 0;
        // SAFETY: `shape` matches `GhosttyMouseShape *`.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_MOUSE_SHAPE,
                (&raw mut shape).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(MouseShape(shape))
    }

    pub fn title(&self) -> Result<String, VtError> {
        let guard = self.inner.lock();
        let mut s = ffi::GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: Borrowed string is copied to an owned `String` while `guard` is held.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_TITLE,
                (&raw mut s).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(unsafe { ghostty_str_to_string(s) })
    }

    pub fn pwd(&self) -> Result<String, VtError> {
        let guard = self.inner.lock();
        let mut s = ffi::GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: Borrowed string is copied to an owned `String` while `guard` is held.
        let rc = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_PWD,
                (&raw mut s).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(unsafe { ghostty_str_to_string(s) })
    }

    /// Drains all queued callback events captured during `write()` calls.
    pub fn drain_events(&self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.inner.lock().cb_state.events)
    }

    /// Compresses scrollback pages (`full = true` compresses all eligible pages).
    pub fn compress(&self, full: bool) -> Result<CompressionResult, VtError> {
        let guard = self.inner.lock();
        let mode = if full {
            ffi::GHOSTTY_TERMINAL_COMPRESSION_MODE_FULL
        } else {
            ffi::GHOSTTY_TERMINAL_COMPRESSION_MODE_INCREMENTAL
        };
        let mut res: ffi::GhosttyTerminalCompressionResult = 0;
        // SAFETY: `guard.raw` is locked.
        let rc = unsafe { ffi::ghostty_terminal_compress(guard.raw, mode, &raw mut res) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(match res {
            ffi::GHOSTTY_TERMINAL_COMPRESSION_RESULT_COMPLETE => CompressionResult::Complete,
            ffi::GHOSTTY_TERMINAL_COMPRESSION_RESULT_PENDING => CompressionResult::Pending,
            _ => CompressionResult::Unsupported,
        })
    }

    /// Encodes a complete `GHOSTSNP` v1 snapshot of this terminal (useful for fixtures/tests).
    pub fn encode_snapshot(&self) -> Result<Vec<u8>, VtError> {
        let guard = self.inner.lock();
        let mut required: usize = 0;
        // SAFETY: NULL buffer queries exact snapshot length.
        let rc = unsafe {
            ffi::ghostty_snapshot_encode_buf(guard.raw, ptr::null_mut(), 0, &raw mut required)
        };
        if rc != ffi::GHOSTTY_SUCCESS && rc != ffi::GHOSTTY_OUT_OF_SPACE {
            return Err(VtError::Ffi(rc));
        }
        let mut buf = vec![0u8; required];
        let mut written: usize = 0;
        // SAFETY: `buf` has exact required length while `guard` holds the lock.
        let rc2 = unsafe {
            ffi::ghostty_snapshot_encode_buf(
                guard.raw,
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut written,
            )
        };
        if rc2 != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc2));
        }
        buf.truncate(written);
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Two-Phase Lock-Minimal `RenderState` with Zero-Alloc Dirty-Row Extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct Overscan {
    pub above: u16,
    pub below: u16,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct RowId(pub [u64; 2]);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DirtyState {
    Clean,
    Partial,
    Full,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CursorStyle {
    Bar,
    Block,
    Underline,
    BlockHollow,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CursorState {
    pub visible: bool,
    pub blinking: bool,
    pub password_input: bool,
    pub style: CursorStyle,
    /// `Some((col, row))` when the cursor is within the visible viewport.
    pub viewport_pos: Option<(u16, u16)>,
    pub wide_tail: bool,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            visible: false,
            blinking: false,
            password_input: false,
            style: CursorStyle::Block,
            viewport_pos: None,
            wide_tail: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderColors {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Option<Rgb>,
    pub palette: [Rgb; 256],
}

impl Default for RenderColors {
    fn default() -> Self {
        Self {
            background: Rgb::new(0, 0, 0),
            foreground: Rgb::new(255, 255, 255),
            cursor: None,
            palette: [Rgb::default(); 256],
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RowSelection {
    pub start_x: u16,
    pub end_x: u16,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CellContent {
    Empty,
    Codepoint(char),
    /// Slice into [`DirtyRow::graphemes`] UTF-8 slab (zero per-cell `String` allocations!).
    Grapheme {
        offset: u32,
        len: u16,
    },
    BgOnly,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CellWide {
    Narrow,
    Wide,
    SpacerTail,
    SpacerHead,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum UnderlineStyle {
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Compact cell style/state flags.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct CellFlags(pub u16);

impl CellFlags {
    pub const EMPTY: Self = Self(0);
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const FAINT: Self = Self(1 << 2);
    pub const BLINK: Self = Self(1 << 3);
    pub const INVERSE: Self = Self(1 << 4);
    pub const INVISIBLE: Self = Self(1 << 5);
    pub const STRIKETHROUGH: Self = Self(1 << 6);
    pub const OVERLINE: Self = Self(1 << 7);
    pub const HYPERLINK: Self = Self(1 << 8);
    pub const PROTECTED: Self = Self(1 << 9);
    pub const SELECTED: Self = Self(1 << 10);

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl std::ops::BitOr for CellFlags {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// Zero-allocation POD representation of one rendered terminal cell.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CellData {
    pub content: CellContent,
    pub wide: CellWide,
    pub fg: Rgb,
    /// `None` means default terminal background (enables translucent pane backgrounds).
    pub bg: Option<Rgb>,
    pub underline_color: Option<Rgb>,
    pub underline: UnderlineStyle,
    pub flags: CellFlags,
}

/// Borrowed view of one row yielded by [`RenderState::for_each_dirty_row`] / [`RenderState::for_each_row`].
pub struct DirtyRow<'a> {
    pub row_idx: u16,
    pub viewport_y: i32,
    pub row_id: RowId,
    pub dirty: bool,
    pub selection: Option<RowSelection>,
    pub cells: &'a [CellData],
    graphemes: &'a [u8],
}

impl<'a> DirtyRow<'a> {
    /// Returns the borrowed UTF-8 grapheme cluster slice if `cell.content` is `CellContent::Grapheme`.
    #[inline]
    pub fn cell_grapheme(&self, cell: &CellData) -> Option<&'a str> {
        match cell.content {
            CellContent::Grapheme { offset, len } => {
                let start = offset as usize;
                let end = start + usize::from(len);
                std::str::from_utf8(self.graphemes.get(start..end)?).ok()
            }
            _ => None,
        }
    }
}

/// Metadata snapshotted during the microsecond lock in [`RenderState::begin_update`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct UpdateSnapshot {
    pub scrollbar: Scrollbar,
    pub viewport_active: bool,
    pub kitty_storage_generation: u64,
}

#[derive(Copy, Clone)]
struct ResolvedStyle {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    underline_color: Option<Rgb>,
    underline: UnderlineStyle,
    flags: CellFlags,
}

/// Two-phase lock-minimal render state owning reusable C iterators and zero-alloc row buffers.
pub struct RenderState {
    raw: ffi::GhosttyRenderState,
    row_iter: ffi::GhosttyRenderStateRowIterator,
    row_cells: ffi::GhosttyRenderStateRowCells,
    image_iter: ffi::GhosttyRenderStateImageIterator,

    requested_overscan: Overscan,
    actual_overscan: Overscan,
    cols: u16,
    rows: u16,
    dirty: DirtyState,
    colors: RenderColors,
    cursor: CursorState,
    image_placements_generation: u64,

    // Reusable per-row scratch buffers (zero heap allocations in steady state).
    cell_buf: Vec<CellData>,
    grapheme_buf: Vec<u8>,
    style_cache: Vec<(u16, ResolvedStyle)>,
}

// SAFETY: `RenderState` exclusively owns all C handles and scratch buffers.
unsafe impl Send for RenderState {}

impl RenderState {
    pub fn new() -> Result<Self, VtError> {
        let mut raw: ffi::GhosttyRenderState = ptr::null_mut();
        // SAFETY: NULL allocator uses default allocator.
        let rc = unsafe { ffi::ghostty_render_state_new(ptr::null(), &raw mut raw) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let mut row_iter: ffi::GhosttyRenderStateRowIterator = ptr::null_mut();
        let rc =
            unsafe { ffi::ghostty_render_state_row_iterator_new(ptr::null(), &raw mut row_iter) };
        if rc != ffi::GHOSTTY_SUCCESS {
            unsafe { ffi::ghostty_render_state_free(raw) };
            return Err(VtError::Ffi(rc));
        }

        let mut row_cells: ffi::GhosttyRenderStateRowCells = ptr::null_mut();
        let rc =
            unsafe { ffi::ghostty_render_state_row_cells_new(ptr::null(), &raw mut row_cells) };
        if rc != ffi::GHOSTTY_SUCCESS {
            unsafe {
                ffi::ghostty_render_state_row_iterator_free(row_iter);
                ffi::ghostty_render_state_free(raw);
            }
            return Err(VtError::Ffi(rc));
        }

        let mut image_iter: ffi::GhosttyRenderStateImageIterator = ptr::null_mut();
        let rc = unsafe {
            ffi::ghostty_render_state_image_iterator_new(ptr::null(), &raw mut image_iter)
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            unsafe {
                ffi::ghostty_render_state_row_cells_free(row_cells);
                ffi::ghostty_render_state_row_iterator_free(row_iter);
                ffi::ghostty_render_state_free(raw);
            }
            return Err(VtError::Ffi(rc));
        }

        Ok(Self {
            raw,
            row_iter,
            row_cells,
            image_iter,
            requested_overscan: Overscan::default(),
            actual_overscan: Overscan::default(),
            cols: 0,
            rows: 0,
            dirty: DirtyState::Clean,
            colors: RenderColors::default(),
            cursor: CursorState::default(),
            image_placements_generation: 0,
            cell_buf: Vec::with_capacity(256),
            grapheme_buf: Vec::with_capacity(256),
            style_cache: Vec::with_capacity(32),
        })
    }

    /// **Phase 1 (microsecond lock)**: snapshots viewport pointers from `terminal`, reads scrollbar
    /// & Kitty storage generation, and drops the terminal mutex immediately.
    pub fn begin_update(
        &mut self,
        terminal: &Terminal,
        overscan: Overscan,
    ) -> Result<UpdateSnapshot, VtError> {
        if overscan != self.requested_overscan {
            let raw_os = ffi::GhosttyRenderStateOverscan {
                above: overscan.above,
                below: overscan.below,
            };
            // SAFETY: `raw_os` matches `GHOSTTY_RENDER_STATE_OPTION_OVERSCAN`.
            let rc = unsafe {
                ffi::ghostty_render_state_set(
                    self.raw,
                    ffi::GHOSTTY_RENDER_STATE_OPTION_OVERSCAN,
                    (&raw const raw_os).cast(),
                )
            };
            if rc != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc));
            }
            self.requested_overscan = overscan;
        }

        let guard = terminal.inner.lock();
        // SAFETY: `guard.raw` is locked for the duration of `begin_update` only.
        let rc = unsafe { ffi::ghostty_render_state_begin_update(self.raw, guard.raw) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let mut sb = ffi::GhosttyTerminalScrollbar::default();
        let _ = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                (&raw mut sb).cast(),
            )
        };
        let mut viewport_active = true;
        let _ = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE,
                (&raw mut viewport_active).cast(),
            )
        };
        let mut kitty_gen: u64 = 0;
        let mut graphics: ffi::GhosttyKittyGraphics = ptr::null_mut();
        if unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS,
                (&raw mut graphics).cast(),
            ) == ffi::GHOSTTY_SUCCESS
                && !graphics.is_null()
        } {
            let _ = unsafe {
                ffi::ghostty_kitty_graphics_get(
                    graphics,
                    ffi::GHOSTTY_KITTY_GRAPHICS_DATA_GENERATION,
                    (&raw mut kitty_gen).cast(),
                )
            };
        }

        // Terminal lock drops RIGHT HERE before `end_update()`.
        drop(guard);

        Ok(UpdateSnapshot {
            scrollbar: Scrollbar {
                total: sb.total,
                offset: sb.offset,
                len: sb.len,
            },
            viewport_active,
            kitty_storage_generation: kitty_gen,
        })
    }

    /// **Phase 2 (completely lock-free)**: finishes computing dirty rows, colors, cursor, and
    /// image placements without holding any terminal lock!
    pub fn end_update(&mut self) -> Result<(), VtError> {
        // SAFETY: `end_update` operates exclusively on `self.raw` (`render.h:42-52`).
        let rc = unsafe { ffi::ghostty_render_state_end_update(self.raw) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let mut cols: u16 = 0;
        let mut rows: u16 = 0;
        let mut dirty: ffi::GhosttyRenderStateDirty = ffi::GHOSTTY_RENDER_STATE_DIRTY_FALSE;
        let mut raw_colors = ffi::GhosttyRenderStateColors::default();
        let mut raw_cursor = ffi::GhosttyRenderStateCursor::default();
        let mut raw_overscan = ffi::GhosttyRenderStateOverscan::default();
        let mut img_gen: u64 = 0;

        // SAFETY: All output pointers match `render.h` types; sized structs have `.size` initialized.
        unsafe {
            let _ = ffi::ghostty_render_state_get(
                self.raw,
                ffi::GHOSTTY_RENDER_STATE_DATA_COLS,
                (&raw mut cols).cast(),
            );
            let _ = ffi::ghostty_render_state_get(
                self.raw,
                ffi::GHOSTTY_RENDER_STATE_DATA_ROWS,
                (&raw mut rows).cast(),
            );
            let _ = ffi::ghostty_render_state_get(
                self.raw,
                ffi::GHOSTTY_RENDER_STATE_DATA_DIRTY,
                (&raw mut dirty).cast(),
            );
            let _ = ffi::ghostty_render_state_get(
                self.raw,
                ffi::GHOSTTY_RENDER_STATE_DATA_COLORS,
                (&raw mut raw_colors).cast(),
            );
            let _ = ffi::ghostty_render_state_get(
                self.raw,
                ffi::GHOSTTY_RENDER_STATE_DATA_CURSOR,
                (&raw mut raw_cursor).cast(),
            );
            let _ = ffi::ghostty_render_state_get(
                self.raw,
                ffi::GHOSTTY_RENDER_STATE_DATA_OVERSCAN,
                (&raw mut raw_overscan).cast(),
            );
            let _ = ffi::ghostty_render_state_get(
                self.raw,
                ffi::GHOSTTY_RENDER_STATE_DATA_IMAGE_PLACEMENTS_GENERATION,
                (&raw mut img_gen).cast(),
            );
        }

        self.cols = cols;
        self.rows = rows;
        self.dirty = match dirty {
            ffi::GHOSTTY_RENDER_STATE_DIRTY_FULL => DirtyState::Full,
            ffi::GHOSTTY_RENDER_STATE_DIRTY_PARTIAL => DirtyState::Partial,
            _ => DirtyState::Clean,
        };
        self.actual_overscan = Overscan {
            above: raw_overscan.above,
            below: raw_overscan.below,
        };
        self.image_placements_generation = img_gen;

        let mut palette = [Rgb::default(); 256];
        for (dst, src) in palette.iter_mut().zip(raw_colors.palette.iter()) {
            *dst = Rgb::from_ffi(*src);
        }
        self.colors = RenderColors {
            background: Rgb::from_ffi(raw_colors.background),
            foreground: Rgb::from_ffi(raw_colors.foreground),
            cursor: raw_colors
                .cursor_has_value
                .then_some(Rgb::from_ffi(raw_colors.cursor)),
            palette,
        };

        self.cursor = CursorState {
            visible: raw_cursor.visible,
            blinking: raw_cursor.blinking,
            password_input: raw_cursor.password_input,
            style: match raw_cursor.visual_style {
                ffi::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorStyle::Bar,
                ffi::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorStyle::Underline,
                ffi::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => {
                    CursorStyle::BlockHollow
                }
                _ => CursorStyle::Block,
            },
            viewport_pos: raw_cursor
                .viewport_has_value
                .then_some((raw_cursor.viewport_x, raw_cursor.viewport_y)),
            wide_tail: raw_cursor.wide_tail,
        };

        Ok(())
    }

    #[inline]
    pub fn dirty_state(&self) -> DirtyState {
        self.dirty
    }

    #[inline]
    pub fn cols(&self) -> u16 {
        self.cols
    }

    #[inline]
    pub fn rows(&self) -> u16 {
        self.rows
    }

    #[inline]
    pub fn overscan(&self) -> Overscan {
        self.actual_overscan
    }

    #[inline]
    pub fn colors(&self) -> &RenderColors {
        &self.colors
    }

    #[inline]
    pub fn cursor(&self) -> CursorState {
        self.cursor
    }

    #[inline]
    pub fn image_placements_generation(&self) -> u64 {
        self.image_placements_generation
    }

    /// Visits only dirty rows using `ghostty_render_state_row_iterator_next_dirty()` with
    /// **zero per-cell heap allocations** (reusing internal `cell_buf` and `grapheme_buf`).
    pub fn for_each_dirty_row<F>(&mut self, mut f: F) -> Result<(), VtError>
    where
        F: FnMut(DirtyRow<'_>),
    {
        if self.dirty == DirtyState::Clean {
            return Ok(());
        }
        self.populate_row_iterator()?;
        let mut row_idx: u16 = 0;
        // SAFETY: `self.row_iter` was just populated from `self.raw`.
        while unsafe {
            ffi::ghostty_render_state_row_iterator_next_dirty(self.row_iter, &raw mut row_idx)
        } {
            let (viewport_y, row_id, selection) = self.decode_current_row()?;
            f(DirtyRow {
                row_idx,
                viewport_y,
                row_id,
                dirty: true,
                selection,
                cells: &self.cell_buf,
                graphemes: &self.grapheme_buf,
            });
        }
        Ok(())
    }

    /// Visits every captured row (viewport + overscan) with zero per-cell heap allocations.
    pub fn for_each_row<F>(&mut self, mut f: F) -> Result<(), VtError>
    where
        F: FnMut(DirtyRow<'_>),
    {
        self.populate_row_iterator()?;
        let mut row_idx: u16 = 0;
        // SAFETY: `self.row_iter` was just populated from `self.raw`.
        while unsafe { ffi::ghostty_render_state_row_iterator_next(self.row_iter) } {
            let mut dirty = false;
            let _ = unsafe {
                ffi::ghostty_render_state_row_get(
                    self.row_iter,
                    ffi::GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                    (&raw mut dirty).cast(),
                )
            };
            let (viewport_y, row_id, selection) = self.decode_current_row()?;
            f(DirtyRow {
                row_idx,
                viewport_y,
                row_id,
                dirty,
                selection,
                cells: &self.cell_buf,
                graphemes: &self.grapheme_buf,
            });
            row_idx = row_idx.saturating_add(1);
        }
        Ok(())
    }

    /// Extracts all visible (and overscan) Kitty graphics placements into caller's reusable `Vec`.
    pub fn kitty_placements(&mut self, out: &mut Vec<KittyPlacement>) -> Result<(), VtError> {
        // SAFETY: `self.raw` and `self.image_iter` are valid handles owned by `self`.
        unsafe { images::collect_render_state_placements(self.raw, self.image_iter, out) }
    }

    /// Marks the render state and all rows clean after GPU instance upload.
    pub fn clean(&mut self) -> Result<(), VtError> {
        // SAFETY: `self.raw` is valid.
        let rc = unsafe { ffi::ghostty_render_state_clean(self.raw) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        self.dirty = DirtyState::Clean;
        Ok(())
    }

    fn populate_row_iterator(&mut self) -> Result<(), VtError> {
        let mut iter_handle = self.row_iter;
        // SAFETY: Populates pre-allocated `self.row_iter` from `self.raw`.
        let rc = unsafe {
            ffi::ghostty_render_state_get(
                self.raw,
                ffi::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                (&raw mut iter_handle).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(())
    }

    fn decode_current_row(&mut self) -> Result<(i32, RowId, Option<RowSelection>), VtError> {
        let mut viewport_y: i32 = 0;
        let mut raw_id = ffi::GhosttyRenderStateRowId::default();
        let mut raw_sel = ffi::GhosttyRenderStateRowSelection::default();
        let mut cells_view = ffi::GhosttyCellsView {
            ptr: ptr::null(),
            len: 0,
        };

        // SAFETY: `self.row_iter` is positioned on a valid row.
        let has_selection = unsafe {
            let _ = ffi::ghostty_render_state_row_get(
                self.row_iter,
                ffi::GHOSTTY_RENDER_STATE_ROW_DATA_VIEWPORT_Y,
                (&raw mut viewport_y).cast(),
            );
            let _ = ffi::ghostty_render_state_row_get(
                self.row_iter,
                ffi::GHOSTTY_RENDER_STATE_ROW_DATA_ID,
                (&raw mut raw_id).cast(),
            );
            let rc_cells = ffi::ghostty_render_state_row_get(
                self.row_iter,
                ffi::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS_RAW,
                (&raw mut cells_view).cast(),
            );
            if rc_cells != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc_cells));
            }
            ffi::ghostty_render_state_row_get(
                self.row_iter,
                ffi::GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                (&raw mut raw_sel).cast(),
            ) == ffi::GHOSTTY_SUCCESS
        };

        let selection = has_selection.then_some(RowSelection {
            start_x: raw_sel.start_x,
            end_x: raw_sel.end_x,
        });

        self.cell_buf.clear();
        self.grapheme_buf.clear();
        self.style_cache.clear();

        if cells_view.ptr.is_null() || cells_view.len == 0 {
            return Ok((viewport_y, RowId(raw_id.bits), selection));
        }

        // SAFETY: `cells_view` is borrowed from `self.raw` and valid while `self` is unmutated.
        let raw_cells = unsafe { std::slice::from_raw_parts(cells_view.ptr, cells_view.len) };
        let mut row_cells_populated = false;

        let default_fg = self.colors.foreground;
        let default_bg = self.colors.background;

        for (col_idx, &raw_cell) in raw_cells.iter().enumerate() {
            let tag = ffi::cell_content_tag(raw_cell);
            let style_id = ffi::cell_style_id(raw_cell);
            let wide = match ffi::cell_wide(raw_cell) {
                ffi::GHOSTTY_CELL_WIDE_WIDE => CellWide::Wide,
                ffi::GHOSTTY_CELL_WIDE_SPACER_TAIL => CellWide::SpacerTail,
                ffi::GHOSTTY_CELL_WIDE_SPACER_HEAD => CellWide::SpacerHead,
                _ => CellWide::Narrow,
            };
            let selected = selection
                .is_some_and(|s| (col_idx as u16) >= s.start_x && (col_idx as u16) <= s.end_x);

            // Populate GhosttyRenderStateRowCells lazily ONLY if a cell has styling or graphemes!
            let need_row_cells =
                style_id != 0 || tag == ffi::GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME;
            if need_row_cells && !row_cells_populated {
                let mut cells_handle = self.row_cells;
                // SAFETY: Populates pre-allocated `self.row_cells` for current row.
                let rc = unsafe {
                    ffi::ghostty_render_state_row_get(
                        self.row_iter,
                        ffi::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                        (&raw mut cells_handle).cast(),
                    )
                };
                if rc != ffi::GHOSTTY_SUCCESS {
                    return Err(VtError::Ffi(rc));
                }
                row_cells_populated = true;
            }

            let content = match tag {
                ffi::GHOSTTY_CELL_CONTENT_CODEPOINT => {
                    let cp = ffi::cell_codepoint(raw_cell);
                    if cp == 0 {
                        CellContent::Empty
                    } else {
                        char::from_u32(cp)
                            .map(CellContent::Codepoint)
                            .unwrap_or(CellContent::Empty)
                    }
                }
                ffi::GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME => {
                    // SAFETY: `self.row_cells` is populated; select `col_idx` and read UTF-8 cluster.
                    unsafe {
                        let _ = ffi::ghostty_render_state_row_cells_select(
                            self.row_cells,
                            col_idx as u16,
                        );
                    }
                    let offset = self.grapheme_buf.len() as u32;
                    let mut stack = [0u8; 64];
                    let mut gbuf = ffi::GhosttyBuffer {
                        ptr: stack.as_mut_ptr(),
                        cap: stack.len(),
                        len: 0,
                    };
                    // SAFETY: `gbuf` points to writable 64-byte stack buffer.
                    let rc = unsafe {
                        ffi::ghostty_render_state_row_cells_get(
                            self.row_cells,
                            ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                            (&raw mut gbuf).cast(),
                        )
                    };
                    if rc == ffi::GHOSTTY_SUCCESS && gbuf.len > 0 {
                        self.grapheme_buf.extend_from_slice(&stack[..gbuf.len]);
                        CellContent::Grapheme {
                            offset,
                            len: gbuf.len.min(u16::MAX as usize) as u16,
                        }
                    } else if rc == ffi::GHOSTTY_OUT_OF_SPACE && gbuf.len > 0 {
                        let start = self.grapheme_buf.len();
                        self.grapheme_buf.resize(start + gbuf.len, 0);
                        let mut gbuf2 = ffi::GhosttyBuffer {
                            ptr: self.grapheme_buf[start..].as_mut_ptr(),
                            cap: gbuf.len,
                            len: 0,
                        };
                        let rc2 = unsafe {
                            ffi::ghostty_render_state_row_cells_get(
                                self.row_cells,
                                ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                                (&raw mut gbuf2).cast(),
                            )
                        };
                        if rc2 == ffi::GHOSTTY_SUCCESS && gbuf2.len > 0 {
                            self.grapheme_buf.truncate(start + gbuf2.len);
                            CellContent::Grapheme {
                                offset,
                                len: gbuf2.len.min(u16::MAX as usize) as u16,
                            }
                        } else {
                            self.grapheme_buf.truncate(start);
                            CellContent::Empty
                        }
                    } else {
                        CellContent::Empty
                    }
                }
                ffi::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE
                | ffi::GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => CellContent::BgOnly,
                _ => CellContent::Empty,
            };

            // Fast path: plain default cell (`style_id == 0`) requires ZERO style FFI calls!
            let (mut fg, mut bg, underline_color, underline, mut flags) = if style_id == 0 {
                let bg_direct = match tag {
                    ffi::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE => {
                        Some(self.colors.palette[usize::from(ffi::cell_bg_palette(raw_cell))])
                    }
                    ffi::GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => {
                        Some(Rgb::from_ffi(ffi::cell_bg_rgb(raw_cell)))
                    }
                    _ => None,
                };
                (
                    default_fg,
                    bg_direct,
                    None,
                    UnderlineStyle::None,
                    CellFlags::EMPTY,
                )
            } else {
                let resolved = if let Some(&(_, cached)) =
                    self.style_cache.iter().find(|&&(id, _)| id == style_id)
                {
                    cached
                } else {
                    let mut raw_style = ffi::GhosttyStyle::default();
                    // SAFETY: `self.row_cells` is populated; select `col_idx` and fetch `GhosttyStyle`.
                    unsafe {
                        let _ = ffi::ghostty_render_state_row_cells_select(
                            self.row_cells,
                            col_idx as u16,
                        );
                        let _ = ffi::ghostty_render_state_row_cells_get(
                            self.row_cells,
                            ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                            (&raw mut raw_style).cast(),
                        );
                    }
                    let resolved = resolve_style(&raw_style, &self.colors.palette);
                    self.style_cache.push((style_id, resolved));
                    resolved
                };
                (
                    resolved.fg.unwrap_or(default_fg),
                    resolved.bg,
                    resolved.underline_color,
                    resolved.underline,
                    resolved.flags,
                )
            };

            if flags.contains(CellFlags::INVERSE) {
                let prev_fg = fg;
                fg = bg.unwrap_or(default_bg);
                bg = Some(prev_fg);
            }
            if ffi::cell_has_hyperlink(raw_cell) {
                flags.0 |= CellFlags::HYPERLINK.0;
            }
            if ffi::cell_protected(raw_cell) {
                flags.0 |= CellFlags::PROTECTED.0;
            }
            if selected {
                flags.0 |= CellFlags::SELECTED.0;
            }

            self.cell_buf.push(CellData {
                content,
                wide,
                fg,
                bg,
                underline_color,
                underline,
                flags,
            });
        }

        Ok((viewport_y, RowId(raw_id.bits), selection))
    }
}

fn resolve_style_color(sc: &ffi::GhosttyStyleColor, palette: &[Rgb; 256]) -> Option<Rgb> {
    match sc.tag {
        ffi::GHOSTTY_STYLE_COLOR_PALETTE => {
            // SAFETY: `tag == PALETTE` selects `palette` arm.
            let idx = unsafe { sc.value.palette };
            Some(palette[usize::from(idx)])
        }
        ffi::GHOSTTY_STYLE_COLOR_RGB => {
            // SAFETY: `tag == RGB` selects `rgb` arm.
            let rgb = unsafe { sc.value.rgb };
            Some(Rgb::from_ffi(rgb))
        }
        _ => None,
    }
}

fn resolve_style(s: &ffi::GhosttyStyle, palette: &[Rgb; 256]) -> ResolvedStyle {
    let mut bits: u16 = 0;
    if s.bold {
        bits |= CellFlags::BOLD.0;
    }
    if s.italic {
        bits |= CellFlags::ITALIC.0;
    }
    if s.faint {
        bits |= CellFlags::FAINT.0;
    }
    if s.blink {
        bits |= CellFlags::BLINK.0;
    }
    if s.inverse {
        bits |= CellFlags::INVERSE.0;
    }
    if s.invisible {
        bits |= CellFlags::INVISIBLE.0;
    }
    if s.strikethrough {
        bits |= CellFlags::STRIKETHROUGH.0;
    }
    if s.overline {
        bits |= CellFlags::OVERLINE.0;
    }

    let underline = match s.underline {
        ffi::GHOSTTY_SGR_UNDERLINE_SINGLE => UnderlineStyle::Single,
        ffi::GHOSTTY_SGR_UNDERLINE_DOUBLE => UnderlineStyle::Double,
        ffi::GHOSTTY_SGR_UNDERLINE_CURLY => UnderlineStyle::Curly,
        ffi::GHOSTTY_SGR_UNDERLINE_DOTTED => UnderlineStyle::Dotted,
        ffi::GHOSTTY_SGR_UNDERLINE_DASHED => UnderlineStyle::Dashed,
        _ => UnderlineStyle::None,
    };

    ResolvedStyle {
        fg: resolve_style_color(&s.fg_color, palette),
        bg: resolve_style_color(&s.bg_color, palette),
        underline_color: resolve_style_color(&s.underline_color, palette),
        underline,
        flags: CellFlags(bits),
    }
}

impl Drop for RenderState {
    fn drop(&mut self) {
        // SAFETY: Every handle was allocated in `RenderState::new` and freed once here.
        unsafe {
            ffi::ghostty_render_state_image_iterator_free(self.image_iter);
            ffi::ghostty_render_state_row_cells_free(self.row_cells);
            ffi::ghostty_render_state_row_iterator_free(self.row_iter);
            ffi::ghostty_render_state_free(self.raw);
        }
    }
}

// ---------------------------------------------------------------------------
// Comprehensive ABI, Snapshot, Kitty PNG, Input, Selection & Search Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn abi_layout_and_discriminants_match_type_manifest() {
        let info = build_info().expect("build_info should succeed");
        assert!(!info.version.is_empty());
        assert!(
            info.kitty_graphics,
            "expected Kitty graphics enabled in libghostty-vt"
        );

        let manifest_str = type_manifest_json().expect("type_manifest_json should succeed");
        let manifest: serde_json::Value =
            serde_json::from_str(manifest_str).expect("manifest JSON must parse");
        let types = manifest["types"].as_object().expect("types map");

        // Verify key C struct sizes against the live linked library's manifest.
        let check_size = |name: &str, rust_size: usize| {
            let expected = types[name]["size"]
                .as_u64()
                .unwrap_or_else(|| panic!("missing size for {name}"))
                as usize;
            assert_eq!(rust_size, expected, "ABI size mismatch for {name}");
        };

        check_size("GhosttyCell", std::mem::size_of::<ffi::GhosttyCell>());
        check_size("GhosttyStyle", std::mem::size_of::<ffi::GhosttyStyle>());
        check_size(
            "GhosttyStyleColor",
            std::mem::size_of::<ffi::GhosttyStyleColor>(),
        );
        check_size("GhosttyPoint", std::mem::size_of::<ffi::GhosttyPoint>());
        check_size("GhosttyGridRef", std::mem::size_of::<ffi::GhosttyGridRef>());
        check_size(
            "GhosttySelection",
            std::mem::size_of::<ffi::GhosttySelection>(),
        );
        check_size(
            "GhosttyRenderStateColors",
            std::mem::size_of::<ffi::GhosttyRenderStateColors>(),
        );
        check_size(
            "GhosttyRenderStateCursor",
            std::mem::size_of::<ffi::GhosttyRenderStateCursor>(),
        );
        check_size(
            "GhosttyRenderStateRowId",
            std::mem::size_of::<ffi::GhosttyRenderStateRowId>(),
        );
        check_size(
            "GhosttyRenderStateRowSelection",
            std::mem::size_of::<ffi::GhosttyRenderStateRowSelection>(),
        );
        check_size(
            "GhosttyTerminalScrollbar",
            std::mem::size_of::<ffi::GhosttyTerminalScrollbar>(),
        );
        check_size(
            "GhosttyTerminalScrollViewport",
            std::mem::size_of::<ffi::GhosttyTerminalScrollViewport>(),
        );
        check_size(
            "GhosttyTerminalUnknownSequence",
            std::mem::size_of::<ffi::GhosttyTerminalUnknownSequence>(),
        );
        check_size(
            "GhosttySysImage",
            std::mem::size_of::<ffi::GhosttySysImage>(),
        );
        check_size(
            "GhosttyMouseEncoderSize",
            std::mem::size_of::<ffi::GhosttyMouseEncoderSize>(),
        );

        // Verify GhosttyCell packed bit positions match our fast-path bitfield decoder.
        let cell_bits = &types["GhosttyCell"]["bits"];
        assert_eq!(cell_bits["content_tag"]["lsb"].as_u64(), Some(0));
        assert_eq!(cell_bits["content_tag"]["width"].as_u64(), Some(2));
        assert_eq!(cell_bits["content"]["lsb"].as_u64(), Some(2));
        assert_eq!(cell_bits["content"]["width"].as_u64(), Some(24));
        assert_eq!(cell_bits["style_id"]["lsb"].as_u64(), Some(26));
        assert_eq!(cell_bits["style_id"]["width"].as_u64(), Some(16));
        assert_eq!(cell_bits["wide"]["lsb"].as_u64(), Some(42));
        assert_eq!(cell_bits["wide"]["width"].as_u64(), Some(2));
        assert_eq!(cell_bits["protected"]["lsb"].as_u64(), Some(44));
        assert_eq!(cell_bits["hyperlink"]["lsb"].as_u64(), Some(45));
        assert_eq!(cell_bits["semantic_content"]["lsb"].as_u64(), Some(46));

        // Verify GhosttyKey enum discriminants for PhysicalKey.
        let key_vals = types["GhosttyKey"]["values"]
            .as_object()
            .expect("GhosttyKey values");
        assert_eq!(
            key_vals["UNIDENTIFIED"].as_i64(),
            Some(PhysicalKey::Unidentified as i64)
        );
        assert_eq!(key_vals["A"].as_i64(), Some(PhysicalKey::KeyA as i64));
        assert_eq!(key_vals["Z"].as_i64(), Some(PhysicalKey::KeyZ as i64));
        assert_eq!(key_vals["ENTER"].as_i64(), Some(PhysicalKey::Enter as i64));
        assert_eq!(
            key_vals["ARROW_UP"].as_i64(),
            Some(PhysicalKey::ArrowUp as i64)
        );
        assert_eq!(
            key_vals["ESCAPE"].as_i64(),
            Some(PhysicalKey::Escape as i64)
        );
        assert_eq!(key_vals["PASTE"].as_i64(), Some(PhysicalKey::Paste as i64));
    }

    #[test]
    fn rex1_apc_nonce_authentication_and_osc_callbacks() {
        let nonce = 7125604888214214440u64;
        let term = Terminal::new(
            80,
            24,
            TerminalConfig {
                channel_nonce: Some(nonce),
                ..TerminalConfig::default()
            },
        )
        .expect("terminal creation should succeed");

        // Spoofed rex1 APC with wrong nonce MUST be rejected!
        term.write(b"\x1b_rex1;9999999;{\"type\":\"resize\",\"cols\":10,\"rows\":10}\x1b\\");
        assert!(
            term.drain_events().is_empty(),
            "spoofed rex1 APC with wrong nonce must be ignored"
        );

        // Authentic rex1 resize + theme APCs + OSC 7501 + OSC 777 + DEC 2026 hold
        let stream = format!(
            "\x1b_rex1;{nonce};{{\"type\":\"resize\",\"cols\":98,\"rows\":32,\"cell_width_px\":8,\"cell_height_px\":16,\"owner\":null}}\x1b\\\
             \x1b_rex1;{nonce};{{\"type\":\"theme\",\"foreground\":\"#ffffff\",\"background\":\"#121212\",\"scheme\":\"dark\"}}\x1b\\\
             \x1b]7501;state=working:app=claude:progress=42\x1b\\\
             \x1b]777;notify;Build Done;All tests green\x1b\\\
             \x1b[?2026h\x1b[?2031h"
        );
        term.write(stream.as_bytes());
        assert!(
            term.is_render_held(),
            "DEC 2026h should activate render hold"
        );
        assert!(term.is_mode_2031_enabled(), "DEC 2031h should be tracked");

        let events = term.drain_events();
        assert!(events.iter().any(|e| matches!(
            e,
            TerminalEvent::RexApc(RexControlApc::Resize {
                cols: 98,
                rows: 32,
                ..
            })
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TerminalEvent::RexApc(RexControlApc::Theme { .. })))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            TerminalEvent::ProgramStatus(ProgramStatusReport {
                state: ProgramStatusState::Working,
                progress: Some(42),
                ..
            })
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            TerminalEvent::DesktopNotification { title, body }
                if title == "Build Done" && body == "All tests green"
        )));

        // Apply resize & theme APCs to terminal.
        for ev in &events {
            if let TerminalEvent::RexApc(apc) = ev {
                term.apply_rex_apc(apc)
                    .expect("apply_rex_apc should succeed");
            }
        }
        assert_eq!(term.size().unwrap(), (98, 32));

        // Release DEC 2026 hold
        term.write(b"\x1b[?2026l");
        assert!(!term.is_render_held());
    }

    #[test]
    fn streaming_snapshot_decode_ready_background_and_dirty_row_ids() {
        // Build a synthetic terminal with multi-page scrollback (1,200 lines exceeds a single
        // Ghostty Page so older pages arrive after READY inside HISTORY!), encode to GHOSTSNP,
        // append a rex1 resize APC, and stream it in small 256-byte chunks!
        let src = Terminal::new(
            80,
            12,
            TerminalConfig {
                scrollback_max_bytes: Some(2 * 1024 * 1024),
                ..TerminalConfig::default()
            },
        )
        .unwrap();
        for i in 0..1200 {
            src.write(
                format!("\x1b[38;2;120;200;255mLine {i:04}\x1b[0m hello omarex scrollback\r\n")
                    .as_bytes(),
            );
        }
        let mut payload = src
            .encode_snapshot()
            .expect("snapshot encode should succeed");
        let snapshot_len = payload.len();
        let trailing_apc =
            b"\x1b_rex1;42;{\"type\":\"resize\",\"cols\":80,\"rows\":12,\"cell_width_px\":8,\"cell_height_px\":16,\"owner\":null}\x1b\\";
        payload.extend_from_slice(trailing_apc);

        let mut decoder = SnapshotDecoder::new(TerminalConfig {
            channel_nonce: Some(42),
            ..TerminalConfig::default()
        })
        .expect("decoder new");

        let mut restored: Option<Terminal> = None;
        let mut pages_total = 0usize;
        let mut collected_trailing = Vec::new();

        for chunk in payload.chunks(256) {
            match decoder.feed(chunk).expect("feed chunk") {
                SnapshotFeedStatus::NeedMoreData => {}
                SnapshotFeedStatus::Ready(term) => {
                    assert!(restored.is_none(), "Ready must fire exactly once");
                    restored = Some(term);
                }
                SnapshotFeedStatus::Buffered => {
                    match decoder.step_background(Duration::from_millis(2)).unwrap() {
                        BackgroundStep::Progress { pages_decoded, .. } => {
                            pages_total += pages_decoded;
                        }
                        BackgroundStep::Finished {
                            trailing_bytes,
                            pages_decoded,
                            ..
                        } => {
                            pages_total += pages_decoded;
                            collected_trailing.extend_from_slice(&trailing_bytes);
                        }
                        BackgroundStep::NeedMoreData => {}
                    }
                }
            }
        }

        let term = restored.expect("should reach READY while streaming");
        // Drain remaining background history to FINISH (or drain any post-FINISH overflow).
        let consumed = loop {
            match decoder.step_background(Duration::from_millis(10)).unwrap() {
                BackgroundStep::Progress { pages_decoded, .. } => pages_total += pages_decoded,
                BackgroundStep::Finished {
                    consumed_bytes,
                    trailing_bytes,
                    pages_decoded,
                    ..
                } => {
                    pages_total += pages_decoded;
                    collected_trailing.extend_from_slice(&trailing_bytes);
                    break consumed_bytes;
                }
                BackgroundStep::NeedMoreData => {
                    panic!("unexpected NeedMoreData after full payload")
                }
            }
        };

        assert!(decoder.is_finished());
        assert_eq!(
            consumed, snapshot_len,
            "SOURCE_OFFSET must match exact GHOSTSNP end"
        );
        assert_eq!(
            collected_trailing, trailing_apc,
            "trailing bytes after FINISH must be preserved across chunk boundaries"
        );
        assert!(
            pages_total >= 1,
            "expected multi-page scrollback history loaded in background (got {pages_total})"
        );

        // Feed trailing rex1 APC and verify it parses cleanly.
        term.write(&collected_trailing);
        let evs = term.drain_events();
        assert!(evs.iter().any(|e| matches!(
            e,
            TerminalEvent::RexApc(RexControlApc::Resize {
                cols: 80,
                rows: 12,
                ..
            })
        )));

        // Verify two-phase RenderState, RowId caching, and zero-alloc dirty row extraction.
        let mut rs = RenderState::new().unwrap();
        let snap = rs
            .begin_update(&term, Overscan { above: 1, below: 1 })
            .unwrap();
        rs.end_update().unwrap();
        assert!(
            snap.scrollbar.total >= 1200,
            "all 1200 scrollback rows should be restored (got {})",
            snap.scrollbar.total
        );
        assert_ne!(rs.dirty_state(), DirtyState::Clean);

        let mut seen_ids = Vec::new();
        let mut found_text = false;
        rs.for_each_dirty_row(|row| {
            seen_ids.push((row.viewport_y, row.row_id));
            let line: String = row
                .cells
                .iter()
                .filter_map(|c| match c.content {
                    CellContent::Codepoint(ch) => Some(ch),
                    _ => None,
                })
                .collect();
            if line.contains("Line 1199") {
                found_text = true;
            }
        })
        .unwrap();
        assert!(
            found_text,
            "expected active viewport text 'Line 1199' in dirty rows"
        );
        rs.clean().unwrap();

        // Second render update with no new writes MUST report DirtyState::Clean (0 dirty rows!).
        rs.begin_update(&term, Overscan { above: 1, below: 1 })
            .unwrap();
        rs.end_update().unwrap();
        assert_eq!(rs.dirty_state(), DirtyState::Clean);
        let mut dirty_count = 0;
        rs.for_each_dirty_row(|_| dirty_count += 1).unwrap();
        assert_eq!(dirty_count, 0);
    }

    #[test]
    fn live_captured_rex_ghostsnp_fixture_decodes_cleanly() {
        let sample_path = std::path::Path::new("/tmp/omarex-re/sample_stream.bin");
        let Ok(live_bytes) = std::fs::read(sample_path) else {
            return;
        };
        let (live_term, consumed_live) =
            SnapshotDecoder::decode_slice(&live_bytes, TerminalConfig::default())
                .expect("live Rex GHOSTSNP fixture should decode");
        assert_eq!(consumed_live, 11300);
        assert_eq!(live_term.size().unwrap(), (98, 32));
        // Feed trailing rex1 resize + theme APCs captured from the live Rex server!
        live_term.set_channel_nonce(Some(7125604888214214440));
        live_term.write(&live_bytes[consumed_live..]);
        let evs = live_term.drain_events();
        assert_eq!(evs.len(), 2, "expected live rex1 resize + theme APCs");
    }

    #[test]
    fn zero_alloc_grapheme_and_styled_cell_rendering() {
        let term = Terminal::new(40, 4, TerminalConfig::default()).unwrap();
        // Styled text + RGB foreground + combining grapheme cluster (e + U+0301 acute accent)
        term.write("\x1b[1;3;4:3;38;2;250;100;50mHi e\u{0301}\x1b[0m".as_bytes());

        let mut rs = RenderState::new().unwrap();
        rs.begin_update(&term, Overscan::default()).unwrap();
        rs.end_update().unwrap();

        let mut found_grapheme = None;
        let mut found_style = false;
        rs.for_each_dirty_row(|row| {
            if row.viewport_y == 0 {
                for cell in row.cells {
                    if let Some(g) = row.cell_grapheme(cell) {
                        found_grapheme = Some(g.to_owned());
                    }
                    if cell.content == CellContent::Codepoint('H') {
                        assert!(cell.flags.contains(CellFlags::BOLD));
                        assert!(cell.flags.contains(CellFlags::ITALIC));
                        assert_eq!(cell.underline, UnderlineStyle::Curly);
                        assert_eq!(cell.fg, Rgb::new(250, 100, 50));
                        found_style = true;
                    }
                }
            }
        })
        .unwrap();
        assert!(found_style, "expected bold/italic/curly/RGB styled 'H'");
        assert_eq!(found_grapheme.as_deref(), Some("e\u{0301}"));
    }

    #[test]
    fn kitty_graphics_png_decode_and_placement_iteration() {
        // Generate a real 4x4 RGBA PNG in memory using the `image` crate.
        let mut img = image::RgbaImage::new(4, 4);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgba([0x11, 0x88, 0xEE, 0xFF]);
        }
        let mut png_bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .expect("encode test PNG");

        // Base64-encode without extra deps.
        let b64 = base64_encode(&png_bytes);

        let term = Terminal::new(80, 24, TerminalConfig::default()).unwrap();
        term.resize(80, 24, 8, 16).unwrap();

        // Transmit & display Kitty PNG (`a=T,f=100,t=d,i=7,c=2,r=1`) -> triggers `decode_png_cb`!
        let seq = format!("\x1b_Ga=T,f=100,t=d,i=7,c=2,r=1;{b64}\x1b\\");
        term.write(seq.as_bytes());

        let mut rs = RenderState::new().unwrap();
        let snap = rs.begin_update(&term, Overscan::default()).unwrap();
        rs.end_update().unwrap();
        assert!(
            snap.kitty_storage_generation > 0,
            "Kitty storage generation must advance"
        );
        assert!(
            rs.image_placements_generation() > 0,
            "placements generation must advance"
        );

        let mut placements = Vec::new();
        rs.kitty_placements(&mut placements).unwrap();
        assert_eq!(placements.len(), 1, "expected 1 Kitty image placement");
        let p = placements[0];
        assert_eq!(p.image_id, 7);
        assert_eq!(p.grid_cols, 2);
        assert_eq!(p.grid_rows, 1);
        assert_eq!(p.z_layer, KittyImageLayer::AboveText);

        let pixels = read_kitty_image(&term, 7)
            .expect("read_kitty_image should succeed")
            .expect("image 7 should be present and decoded");
        assert_eq!(pixels.width, 4);
        assert_eq!(pixels.height, 4);
        assert_eq!(pixels.generation, p.generation);
        assert_eq!(pixels.rgba8.len(), 4 * 4 * 4);
        assert_eq!(&pixels.rgba8[0..4], &[0x11, 0x88, 0xEE, 0xFF]);
    }

    #[test]
    fn key_encoder_kitty_shift_enter_and_german_layout() {
        let mut enc = KeyEncoder::new().unwrap();
        enc.set_kitty_flags(KittyKeyboardFlags::DISAMBIGUATE);

        // 1. Shift+Enter under Kitty disambiguate flags -> `CSI 13 ; 2 u` (`\x1b[13;2u`)
        let mut out = Vec::new();
        enc.encode(
            &KeyEventInput {
                action: KeyAction::Press,
                key: PhysicalKey::Enter,
                mods: Modifiers::SHIFT,
                consumed_mods: Modifiers::NONE,
                utf8: None,
                unshifted_codepoint: Some('\r'),
                composing: false,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(out, b"\x1b[13;2u", "Shift+Enter must emit Kitty CSI 13;2u");

        // 2. German QWERTZ characters: ä, ö, ü, ß, and `@` via AltGr (ALT_RIGHT consumed!)
        for (phys, ch, mods, consumed) in [
            (PhysicalKey::Quote, 'ä', Modifiers::NONE, Modifiers::NONE),
            (
                PhysicalKey::Semicolon,
                'ö',
                Modifiers::NONE,
                Modifiers::NONE,
            ),
            (
                PhysicalKey::BracketLeft,
                'ü',
                Modifiers::NONE,
                Modifiers::NONE,
            ),
            (PhysicalKey::Minus, 'ß', Modifiers::NONE, Modifiers::NONE),
            (
                PhysicalKey::KeyQ,
                '@',
                Modifiers::ALT_RIGHT,
                Modifiers::ALT_RIGHT,
            ),
        ] {
            let mut buf = [0u8; 4];
            let text = ch.encode_utf8(&mut buf);
            out.clear();
            enc.encode(
                &KeyEventInput {
                    action: KeyAction::Press,
                    key: phys,
                    mods,
                    consumed_mods: consumed,
                    utf8: Some(text),
                    unshifted_codepoint: Some(ch),
                    composing: false,
                },
                &mut out,
            )
            .unwrap();
            assert_eq!(
                std::str::from_utf8(&out).unwrap(),
                text,
                "German character '{ch}' must encode cleanly without bogus ESC prefix"
            );
        }

        // 3. Mouse encoder SGR + Shift-bypass check + Paste + Focus + Mode 2031
        let mut menc = MouseEncoder::new().unwrap();
        menc.set_tracking_mode(MouseTrackingMode::Normal);
        menc.set_format(MouseFormat::Sgr);
        menc.set_geometry(MouseGeometry {
            screen_width: 800,
            screen_height: 480,
            cell_width: 10,
            cell_height: 20,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        });
        out.clear();
        let sent = menc
            .encode_with_shift_bypass(
                &MouseEventInput {
                    action: MouseAction::Press,
                    button: Some(MouseButton::Left),
                    mods: Modifiers::NONE,
                    x: 25.0,
                    y: 45.0,
                },
                MouseTrackingMode::Normal,
                &mut out,
            )
            .unwrap();
        assert!(sent);
        assert_eq!(out, b"\x1b[<0;3;3M");

        out.clear();
        let bypassed = menc
            .encode_with_shift_bypass(
                &MouseEventInput {
                    action: MouseAction::Press,
                    button: Some(MouseButton::Left),
                    mods: Modifiers::SHIFT,
                    x: 25.0,
                    y: 45.0,
                },
                MouseTrackingMode::Normal,
                &mut out,
            )
            .unwrap();
        assert!(!bypassed, "Shift+click must bypass mouse reporting");
        assert!(out.is_empty());

        assert!(is_paste_safe(b"hello world"));
        assert!(!is_paste_safe(b"rm -rf /\n"));
        assert_eq!(encode_paste(b"hi", true).unwrap(), b"\x1b[200~hi\x1b[201~");
        assert_eq!(encode_focus(true).unwrap(), b"\x1b[I");
        assert_eq!(encode_focus(false).unwrap(), b"\x1b[O");
        assert_eq!(
            encode_color_scheme_report(ColorScheme::Dark).unwrap(),
            b"\x1b[?997;1n"
        );
    }

    #[test]
    fn selection_gestures_hyperlinks_and_scrollback_search() {
        let term = Terminal::new(80, 10, TerminalConfig::default()).unwrap();
        term.write(
            b"Alpha beta gamma\r\n\
              Visit \x1b]8;;https://superlogical.com/rex\x1b\\Rex Docs\x1b]8;;\x1b\\ now\r\n\
              Target needle ALPHA here\r\n",
        );

        // OSC 8 hyperlink extraction
        let url = hyperlink_at(&term, 7, 1).unwrap();
        assert_eq!(url.as_deref(), Some("https://superlogical.com/rex"));
        assert_eq!(hyperlink_at(&term, 0, 1).unwrap(), None);

        // Word selection & plain-text extraction
        assert!(select_word(&term, 6, 0).unwrap());
        assert_eq!(
            selection_text(&term, true, true).unwrap().as_deref(),
            Some("beta")
        );

        // Drag selection gesture (linear & rectangular)
        let mut gesture = SelectionGesture::new(&term).unwrap();
        let geom = SelectionGeometry {
            columns: 80,
            cell_width: 10,
            padding_left: 0,
            screen_height: 200,
        };
        gesture.press(0, 0, 5.0, 10.0, 1_000_000).unwrap();
        assert!(gesture.drag(9, 0, 95.0, 10.0, geom, false).unwrap());
        gesture.release(Some((9, 0))).unwrap();
        let dragged = selection_text(&term, true, true)
            .unwrap()
            .unwrap_or_default();
        assert!(
            dragged.starts_with("Alpha bet"),
            "unexpected dragged selection: {dragged:?}"
        );

        // Rectangular (Alt+drag) block selection across rows 0..=1, cols 0..=4
        gesture.reset();
        gesture.press(0, 0, 1.0, 10.0, 10_000_000_000).unwrap();
        assert!(gesture.drag(4, 1, 49.0, 30.0, geom, true).unwrap());
        gesture.release(Some((4, 1))).unwrap();
        let rect_text = selection_text(&term, true, true)
            .unwrap()
            .unwrap_or_default();
        assert_eq!(
            rect_text, "Alpha\nVisit",
            "unexpected rectangular selection: {rect_text:?}"
        );
        clear_selection(&term).unwrap();
        assert_eq!(selection_text(&term, true, true).unwrap(), None);

        // Scrollback search (ASCII case-insensitive!)
        let mut search = TerminalSearch::new(&term).unwrap();
        search.set_needle("alpha").unwrap();
        search.run().unwrap();
        assert_eq!(search.status().unwrap(), SearchStatus::Complete);
        assert_eq!(search.total_matches().unwrap(), 2);

        let vmatches = search.viewport_matches().unwrap();
        assert_eq!(vmatches.len(), 2);
        assert!(search.select_next().unwrap());
        assert_eq!(search.selected_index().unwrap(), Some(0));
        assert!(search.select_next().unwrap());
        assert_eq!(search.selected_index().unwrap(), Some(1));
    }

    #[test]
    fn scroll_region_tracks_live_decstbm_reset_resize_and_snapshot_restore() {
        let term = Terminal::new(80, 24, TerminalConfig::default()).unwrap();
        assert_eq!(term.scroll_region(), None);

        // Set DECSTBM scroll region rows 1..20 (1-based) => 0..=19 (0-based).
        term.write(b"\x1b[1;20r");
        assert_eq!(term.scroll_region(), Some((0, 19)));

        // Snapshot export + restore must preserve the active DECSTBM scroll region immediately.
        let snap = term.encode_snapshot().unwrap();
        let mut decoder = SnapshotDecoder::new(TerminalConfig::default()).unwrap();
        let restored = match decoder.feed(&snap).unwrap() {
            SnapshotFeedStatus::Ready(t) => t,
            SnapshotFeedStatus::NeedMoreData => {
                panic!("expected SnapshotFeedStatus::Ready, got NeedMoreData")
            }
            SnapshotFeedStatus::Buffered => {
                panic!("expected SnapshotFeedStatus::Ready, got Buffered")
            }
        };
        assert_eq!(restored.scroll_region(), Some((0, 19)));

        // Resetting DECSTBM (`\x1b[r`) clears the restricted region.
        term.write(b"\x1b[r");
        assert_eq!(term.scroll_region(), None);

        // Resizing resets DECSTBM in `libghostty-vt` and updates tracker cleanly.
        term.write(b"\x1b[2;15r");
        assert_eq!(term.scroll_region(), Some((1, 14)));
        term.resize(90, 30, 10, 20).unwrap();
        assert_eq!(term.scroll_region(), None);
        assert_eq!(term.pixel_size().unwrap(), (900, 600));
    }

    #[test]
    fn logical_line_at_unwraps_soft_wrapped_paths_and_blank_span_checks_scrollback() {
        let term = Terminal::new(20, 6, TerminalConfig::default()).unwrap();
        // Write a long path that soft-wraps across 2 rows on a 20-column grid.
        term.write(b"open /home/user/projects/omarex/README.md now\r\n");
        let (line_row0, col_row0) = logical_line_at(&term, 6, 0).unwrap().expect("row 0 line");
        let (line_row1, col_row1) = logical_line_at(&term, 3, 1).unwrap().expect("row 1 line");
        assert!(
            line_row0.contains("/home/user/projects/omarex/README.md"),
            "expected unwrapped full path on row 0, got {line_row0:?}"
        );
        assert_eq!(line_row0, line_row1);
        assert_eq!(col_row0, 6);
        assert!(
            col_row1 >= 20,
            "expected row 1 char index to include wrapped prefix, got {col_row1}"
        );

        // Push rows into scrollback and verify `viewport_row_span_is_blank` works for negative rows.
        term.write(b"\r\n\r\n\r\n\r\n\r\nbottom line\r\n");
        assert_eq!(
            viewport_row_span_is_blank(&term, 0, 0, 10).unwrap(),
            Some(true)
        );
        // Row 0 of the original output scrolled above the viewport (`viewport_row < 0`).
        let mut saw_non_blank_scrollback = false;
        for neg_row in -8..0 {
            if viewport_row_span_is_blank(&term, neg_row, 0, 15).unwrap() == Some(false) {
                saw_non_blank_scrollback = true;
                break;
            }
        }
        assert!(
            saw_non_blank_scrollback,
            "expected viewport_row_span_is_blank to detect non-blank scrollback row"
        );
    }

    #[test]
    fn kitty_graphics_decodes_jpeg_webp_and_supports_local_file_medium_option() {
        use std::io::Cursor;

        // 1. Verify JPEG payload via Kitty `f=100,t=d` decodes through `decode_png_cb`.
        let mut img = image::RgbaImage::new(4, 4);
        for px in img.pixels_mut() {
            *px = image::Rgba([0xCC, 0x44, 0x22, 0xFF]);
        }
        let dyn_img = image::DynamicImage::ImageRgba8(img);

        let mut jpeg_bytes = Vec::new();
        dyn_img
            .to_rgb8()
            .write_to(&mut Cursor::new(&mut jpeg_bytes), image::ImageFormat::Jpeg)
            .expect("encode test JPEG");
        let jpeg_b64 = base64_encode(&jpeg_bytes);

        let term = Terminal::new(80, 24, TerminalConfig::default()).unwrap();
        term.resize(80, 24, 8, 16).unwrap();
        assert!(
            !term.kitty_image_medium_file().unwrap(),
            "file medium must default to false for remote/untrusted safety"
        );

        let seq_jpg = format!("\x1b_Ga=T,f=100,t=d,i=11,c=2,r=1;{jpeg_b64}\x1b\\");
        term.write(seq_jpg.as_bytes());
        let jpg_pixels = read_kitty_image(&term, 11)
            .expect("read_kitty_image JPEG")
            .expect("JPEG image 11 should decode cleanly");
        assert_eq!((jpg_pixels.width, jpg_pixels.height), (4, 4));

        // 2. Verify WebP payload via Kitty `f=100,t=d` decodes through `decode_png_cb`.
        let mut webp_bytes = Vec::new();
        dyn_img
            .write_to(&mut Cursor::new(&mut webp_bytes), image::ImageFormat::WebP)
            .expect("encode test WebP");
        let webp_b64 = base64_encode(&webp_bytes);
        let seq_webp = format!("\x1b_Ga=T,f=100,t=d,i=12,c=2,r=1;{webp_b64}\x1b\\");
        term.write(seq_webp.as_bytes());
        let webp_pixels = read_kitty_image(&term, 12)
            .expect("read_kitty_image WebP")
            .expect("WebP image 12 should decode cleanly");
        assert_eq!((webp_pixels.width, webp_pixels.height), (4, 4));

        // 3. Verify `kitty_image_medium_file` option toggles on Terminal & loads local `t=f` file.
        let mut png_bytes = Vec::new();
        dyn_img
            .write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .expect("encode test PNG");
        let tmp_path = std::env::temp_dir().join(format!(
            "omarex-vt-kitty-file-medium-{}.png",
            std::process::id()
        ));
        std::fs::write(&tmp_path, &png_bytes).expect("write temp png");
        let path_b64 = base64_encode(tmp_path.to_string_lossy().as_bytes());

        // With `kitty_image_medium_file = false`, `t=f` is ignored.
        let seq_file_disabled = format!("\x1b_Ga=T,f=100,t=f,i=13,c=2,r=1;{path_b64}\x1b\\");
        term.write(seq_file_disabled.as_bytes());
        assert!(
            read_kitty_image(&term, 13).unwrap().is_none(),
            "t=f must remain blocked when kitty_image_medium_file is false"
        );

        // Enable via setter & via TerminalConfig for local blocks.
        term.set_kitty_image_medium_file(true).unwrap();
        assert!(term.kitty_image_medium_file().unwrap());
        let seq_file_enabled = format!("\x1b_Ga=T,f=100,t=f,i=14,c=3,r=2;{path_b64}\x1b\\");
        term.write(seq_file_enabled.as_bytes());
        let file_pixels = read_kitty_image(&term, 14)
            .expect("read_kitty_image t=f")
            .expect("t=f image 14 should load when kitty_image_medium_file is enabled");
        assert_eq!((file_pixels.width, file_pixels.height), (4, 4));

        let local_term = Terminal::new(
            80,
            24,
            TerminalConfig {
                kitty_image_medium_file: true,
                ..TerminalConfig::default()
            },
        )
        .unwrap();
        assert!(local_term.kitty_image_medium_file().unwrap());

        let _ = std::fs::remove_file(&tmp_path);

        // 4. Verify `kitty_placement_at` hit-testing against live RenderState placements.
        let mut rs = RenderState::new().unwrap();
        let _ = rs.begin_update(&term, Overscan::default()).unwrap();
        rs.end_update().unwrap();
        let mut placements = Vec::new();
        rs.kitty_placements(&mut placements).unwrap();
        assert!(
            !placements.is_empty(),
            "expected Kitty placements from transmitted images"
        );
        let first = placements[0];
        let hit = kitty_placement_at(
            &placements,
            first.viewport_col.max(0) as u16,
            first.viewport_row.max(0) as u32,
        );
        assert!(hit.is_some(), "expected kitty_placement_at hit on image");
        assert!(
            kitty_placement_at(&placements, 79, 23).is_none(),
            "expected miss outside image bounds"
        );
    }

    #[test]
    fn logical_line_with_tui_unwrap_stitches_tr3x_gutter_wrapped_paths_and_spans() {
        // Exact screenshot layout (86 cols): `│ ` fenced `text` and `sh` blocks with wrapped path.
        let term = Terminal::new(86, 12, TerminalConfig::default()).unwrap();
        let screen = concat!(
            "  • Clickable link: omarex_demo_video_v2.mp4\r\n",
            "  text\r\n",
            "  │ /home/user/.cache/demo/2026-11-26-sample-session-id001/omarex_demo_vide\r\n",
            "  │ o_v2.mp4\r\n",
            "  sh\r\n",
            "  │ xdg-open /home/user/.cache/demo/2026-11-26-sample-session-id001/omarex_de\r\n",
            "  │   mo_video_v2.mp4\r\n",
            "  │ /tmp/a.png\r\n",
            "  │ /tmp/b.png\r\n",
        );
        term.write(screen.as_bytes());

        // Hover row 2 (`│ /home/user/.../omarex_demo_vide`) and row 3 (`│ o_v2.mp4`):
        let u2 = logical_line_with_tui_unwrap_at(&term, 10, 2)
            .unwrap()
            .expect("row 2 unwrapped");
        let u3 = logical_line_with_tui_unwrap_at(&term, 6, 3)
            .unwrap()
            .expect("row 3 unwrapped");
        let expected_path =
            "/home/user/.cache/demo/2026-11-26-sample-session-id001/omarex_demo_video_v2.mp4";
        assert!(
            u2.text.contains(expected_path),
            "expected stitched path on row 2, got {:?}",
            u2.text
        );
        assert_eq!(u2.text, u3.text);

        // Verify per-row cell spans cover BOTH row 2 and row 3 without including row 3's gutter.
        let path_start = u2.text.find(expected_path).unwrap();
        let char_start = u2.text[..path_start].chars().count();
        let char_len = expected_path.chars().count();
        let spans = u2.cell_spans_for_char_range(char_start..(char_start + char_len));
        assert_eq!(
            spans.len(),
            2,
            "expected 2 wrapped row spans, got {spans:?}"
        );
        assert_eq!(spans[0].row, 2);
        assert_eq!(spans[0].start_col, 4);
        assert_eq!(spans[1].row, 3);
        assert_eq!(spans[1].start_col, 4);
        assert_eq!(spans[1].end_col, 12);

        // Hover hanging-indent `sh` block on row 6 (`│   unch_video_v2.mp4`):
        let u6 = logical_line_with_tui_unwrap_at(&term, 8, 6)
            .unwrap()
            .expect("row 6 unwrapped");
        assert!(
            u6.text.contains(expected_path),
            "expected stitched sh path on row 6, got {:?}",
            u6.text
        );
        let sh_start = u6.text.find(expected_path).unwrap();
        let sh_cstart = u6.text[..sh_start].chars().count();
        let sh_spans = u6.cell_spans_for_char_range(sh_cstart..(sh_cstart + char_len));
        assert_eq!(sh_spans.len(), 2);
        assert_eq!(sh_spans[0].row, 5);
        assert_eq!(sh_spans[1].row, 6);
        assert_eq!(sh_spans[1].start_col, 6); // hanging indent skipped!

        // Short consecutive path lines (rows 7 & 8) must NOT be falsely stitched together!
        let u7 = logical_line_with_tui_unwrap_at(&term, 6, 7)
            .unwrap()
            .expect("row 7");
        assert!(u7.text.contains("/tmp/a.png"));
        assert!(!u7.text.contains("/tmp/b.png"));

        // `viewport_text_lines` exposes the full stitched path for bare-filename resolution.
        let all_lines = viewport_text_lines(&term).unwrap();
        assert!(
            all_lines.iter().any(|l| l.contains(expected_path)),
            "expected viewport_text_lines to contain stitched path: {all_lines:?}"
        );
    }

    fn base64_encode(input: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = if chunk.len() > 1 {
                u32::from(chunk[1])
            } else {
                0
            };
            let b2 = if chunk.len() > 2 {
                u32::from(chunk[2])
            } else {
                0
            };
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push(if chunk.len() > 1 {
                TABLE[((n >> 6) & 0x3F) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(n & 0x3F) as usize] as char
            } else {
                '='
            });
        }
        out
    }
}
