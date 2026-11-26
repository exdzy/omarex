//! Audited raw C ABI bindings to `libghostty-vt` (`/usr/include/ghostty/vt/*.h`).
//!
//! Every `unsafe extern "C"` declaration is isolated in this module and paired with
//! compile-time/unit-test ABI verification against `ghostty_type_json()` so any
//! upstream ABI drift fails tests immediately rather than causing silent memory
//! corruption.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_void};

// ---------------------------------------------------------------------------
// Result codes (types.h)
// ---------------------------------------------------------------------------

pub type GhosttyResult = c_int;

pub const GHOSTTY_SUCCESS: GhosttyResult = 0;
pub const GHOSTTY_OUT_OF_MEMORY: GhosttyResult = -1;
pub const GHOSTTY_INVALID_VALUE: GhosttyResult = -2;
pub const GHOSTTY_OUT_OF_SPACE: GhosttyResult = -3;
pub const GHOSTTY_NO_VALUE: GhosttyResult = -4;
pub const GHOSTTY_IO_ERROR: GhosttyResult = -5;
pub const GHOSTTY_LIMIT_EXCEEDED: GhosttyResult = -6;
pub const GHOSTTY_REJECTED: GhosttyResult = -7;

// ---------------------------------------------------------------------------
// Opaque handles (types.h, selection.h, key/*.h, mouse/*.h)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct GhosttyTerminalImpl {
    _opaque: [u8; 0],
}
pub type GhosttyTerminal = *mut GhosttyTerminalImpl;

#[repr(C)]
pub struct GhosttySnapshotDecoderImpl {
    _opaque: [u8; 0],
}
pub type GhosttySnapshotDecoder = *mut GhosttySnapshotDecoderImpl;

#[repr(C)]
pub struct GhosttyTrackedGridRefImpl {
    _opaque: [u8; 0],
}
pub type GhosttyTrackedGridRef = *mut GhosttyTrackedGridRefImpl;

#[repr(C)]
pub struct GhosttyKittyGraphicsImpl {
    _opaque: [u8; 0],
}
pub type GhosttyKittyGraphics = *mut GhosttyKittyGraphicsImpl;

#[repr(C)]
pub struct GhosttyKittyGraphicsImageImpl {
    _opaque: [u8; 0],
}
pub type GhosttyKittyGraphicsImage = *const GhosttyKittyGraphicsImageImpl;

#[repr(C)]
pub struct GhosttyKittyGraphicsPlacementIteratorImpl {
    _opaque: [u8; 0],
}
pub type GhosttyKittyGraphicsPlacementIterator = *mut GhosttyKittyGraphicsPlacementIteratorImpl;

#[repr(C)]
pub struct GhosttyRenderStateImpl {
    _opaque: [u8; 0],
}
pub type GhosttyRenderState = *mut GhosttyRenderStateImpl;

#[repr(C)]
pub struct GhosttyRenderStateRowIteratorImpl {
    _opaque: [u8; 0],
}
pub type GhosttyRenderStateRowIterator = *mut GhosttyRenderStateRowIteratorImpl;

#[repr(C)]
pub struct GhosttyRenderStateRowCellsImpl {
    _opaque: [u8; 0],
}
pub type GhosttyRenderStateRowCells = *mut GhosttyRenderStateRowCellsImpl;

#[repr(C)]
pub struct GhosttyRenderStateImageIteratorImpl {
    _opaque: [u8; 0],
}
pub type GhosttyRenderStateImageIterator = *mut GhosttyRenderStateImageIteratorImpl;

#[repr(C)]
pub struct GhosttySearchImpl {
    _opaque: [u8; 0],
}
pub type GhosttySearch = *mut GhosttySearchImpl;

#[repr(C)]
pub struct GhosttySelectionGestureImpl {
    _opaque: [u8; 0],
}
pub type GhosttySelectionGesture = *mut GhosttySelectionGestureImpl;

#[repr(C)]
pub struct GhosttySelectionGestureEventImpl {
    _opaque: [u8; 0],
}
pub type GhosttySelectionGestureEvent = *mut GhosttySelectionGestureEventImpl;

#[repr(C)]
pub struct GhosttyKeyEventImpl {
    _opaque: [u8; 0],
}
pub type GhosttyKeyEvent = *mut GhosttyKeyEventImpl;

#[repr(C)]
pub struct GhosttyKeyEncoderImpl {
    _opaque: [u8; 0],
}
pub type GhosttyKeyEncoder = *mut GhosttyKeyEncoderImpl;

#[repr(C)]
pub struct GhosttyMouseEventImpl {
    _opaque: [u8; 0],
}
pub type GhosttyMouseEvent = *mut GhosttyMouseEventImpl;

#[repr(C)]
pub struct GhosttyMouseEncoderImpl {
    _opaque: [u8; 0],
}
pub type GhosttyMouseEncoder = *mut GhosttyMouseEncoderImpl;

// ---------------------------------------------------------------------------
// Common value types (types.h, allocator.h, io.h, build_info.h)
// ---------------------------------------------------------------------------

pub const GHOSTTY_BUILD_INFO_SIMD: c_int = 1;
pub const GHOSTTY_BUILD_INFO_KITTY_GRAPHICS: c_int = 2;
pub const GHOSTTY_BUILD_INFO_TMUX_CONTROL_MODE: c_int = 3;
pub const GHOSTTY_BUILD_INFO_OPTIMIZE: c_int = 4;
pub const GHOSTTY_BUILD_INFO_VERSION_STRING: c_int = 5;

pub type GhosttyFormatterFormat = c_int;
pub const GHOSTTY_FORMATTER_FORMAT_PLAIN: GhosttyFormatterFormat = 0;
pub const GHOSTTY_FORMATTER_FORMAT_VT: GhosttyFormatterFormat = 1;
pub const GHOSTTY_FORMATTER_FORMAT_HTML: GhosttyFormatterFormat = 2;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyString {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyBuffer {
    pub ptr: *mut u8,
    pub cap: usize,
    pub len: usize,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttySurfacePosition {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyCodepoints {
    pub ptr: *const u32,
    pub len: usize,
}

#[repr(C)]
pub struct GhosttyAllocatorVtable {
    pub alloc: Option<unsafe extern "C" fn(*mut c_void, usize, u8, usize) -> *mut c_void>,
    pub resize:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize, u8, usize, usize) -> bool>,
    pub remap: Option<
        unsafe extern "C" fn(*mut c_void, *mut c_void, usize, u8, usize, usize) -> *mut c_void,
    >,
    pub free: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize, u8, usize)>,
}

#[repr(C)]
pub struct GhosttyAllocator {
    pub ctx: *mut c_void,
    pub vtable: *const GhosttyAllocatorVtable,
}

pub type GhosttyReaderFn = unsafe extern "C" fn(
    userdata: *mut c_void,
    buffer: *mut u8,
    capacity: usize,
    out_read: *mut usize,
) -> bool;

pub type GhosttyWriterFn =
    unsafe extern "C" fn(userdata: *mut c_void, data: *const u8, len: usize) -> bool;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyReader {
    pub read: GhosttyReaderFn,
    pub userdata: *mut c_void,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyWriter {
    pub write: GhosttyWriterFn,
    pub userdata: *mut c_void,
}

// ---------------------------------------------------------------------------
// System interface (sys.h)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttySysImage {
    pub width: u32,
    pub height: u32,
    pub data: *mut u8,
    pub data_len: usize,
}

pub type GhosttySysLogLevel = c_int;
pub const GHOSTTY_SYS_LOG_LEVEL_ERROR: GhosttySysLogLevel = 0;
pub const GHOSTTY_SYS_LOG_LEVEL_WARNING: GhosttySysLogLevel = 1;
pub const GHOSTTY_SYS_LOG_LEVEL_INFO: GhosttySysLogLevel = 2;
pub const GHOSTTY_SYS_LOG_LEVEL_DEBUG: GhosttySysLogLevel = 3;

pub type GhosttySysOption = c_int;
pub const GHOSTTY_SYS_OPT_USERDATA: GhosttySysOption = 0;
pub const GHOSTTY_SYS_OPT_DECODE_PNG: GhosttySysOption = 1;
pub const GHOSTTY_SYS_OPT_LOG: GhosttySysOption = 2;
pub const GHOSTTY_SYS_OPT_RANDOM_SECURE: GhosttySysOption = 3;

pub type GhosttySysDecodePngFn = unsafe extern "C" fn(
    userdata: *mut c_void,
    allocator: *const GhosttyAllocator,
    data: *const u8,
    data_len: usize,
    out: *mut GhosttySysImage,
) -> bool;

pub type GhosttySysLogFn = unsafe extern "C" fn(
    userdata: *mut c_void,
    level: GhosttySysLogLevel,
    scope: *const u8,
    scope_len: usize,
    message: *const u8,
    message_len: usize,
);

// ---------------------------------------------------------------------------
// Color, Style, Screen, Point, GridRef, Modes, Device (color.h..modes.h)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct GhosttyColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub type GhosttyColorPaletteIndex = u8;
pub type GhosttyStyleId = u16;
pub type GhosttyCell = u64;
pub type GhosttyRow = u64;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyCellsView {
    pub ptr: *const GhosttyCell,
    pub len: usize,
}

pub type GhosttyCellContentTag = c_int;
pub const GHOSTTY_CELL_CONTENT_CODEPOINT: GhosttyCellContentTag = 0;
pub const GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME: GhosttyCellContentTag = 1;
pub const GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE: GhosttyCellContentTag = 2;
pub const GHOSTTY_CELL_CONTENT_BG_COLOR_RGB: GhosttyCellContentTag = 3;

pub type GhosttyCellWide = c_int;
pub const GHOSTTY_CELL_WIDE_NARROW: GhosttyCellWide = 0;
pub const GHOSTTY_CELL_WIDE_WIDE: GhosttyCellWide = 1;
pub const GHOSTTY_CELL_WIDE_SPACER_TAIL: GhosttyCellWide = 2;
pub const GHOSTTY_CELL_WIDE_SPACER_HEAD: GhosttyCellWide = 3;

pub type GhosttyCellSemanticContent = c_int;
pub const GHOSTTY_CELL_SEMANTIC_OUTPUT: GhosttyCellSemanticContent = 0;
pub const GHOSTTY_CELL_SEMANTIC_INPUT: GhosttyCellSemanticContent = 1;
pub const GHOSTTY_CELL_SEMANTIC_PROMPT: GhosttyCellSemanticContent = 2;

pub type GhosttyStyleColorTag = c_int;
pub const GHOSTTY_STYLE_COLOR_NONE: GhosttyStyleColorTag = 0;
pub const GHOSTTY_STYLE_COLOR_PALETTE: GhosttyStyleColorTag = 1;
pub const GHOSTTY_STYLE_COLOR_RGB: GhosttyStyleColorTag = 2;

#[repr(C)]
#[derive(Copy, Clone)]
pub union GhosttyStyleColorValue {
    pub palette: GhosttyColorPaletteIndex,
    pub rgb: GhosttyColorRgb,
    pub _padding: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyStyleColor {
    pub tag: GhosttyStyleColorTag,
    pub value: GhosttyStyleColorValue,
}

pub type GhosttySgrUnderline = c_int;
pub const GHOSTTY_SGR_UNDERLINE_NONE: GhosttySgrUnderline = 0;
pub const GHOSTTY_SGR_UNDERLINE_SINGLE: GhosttySgrUnderline = 1;
pub const GHOSTTY_SGR_UNDERLINE_DOUBLE: GhosttySgrUnderline = 2;
pub const GHOSTTY_SGR_UNDERLINE_CURLY: GhosttySgrUnderline = 3;
pub const GHOSTTY_SGR_UNDERLINE_DOTTED: GhosttySgrUnderline = 4;
pub const GHOSTTY_SGR_UNDERLINE_DASHED: GhosttySgrUnderline = 5;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyStyle {
    pub size: usize,
    pub fg_color: GhosttyStyleColor,
    pub bg_color: GhosttyStyleColor,
    pub underline_color: GhosttyStyleColor,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: c_int,
}

impl Default for GhosttyStyle {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            fg_color: GhosttyStyleColor {
                tag: GHOSTTY_STYLE_COLOR_NONE,
                value: GhosttyStyleColorValue { _padding: 0 },
            },
            bg_color: GhosttyStyleColor {
                tag: GHOSTTY_STYLE_COLOR_NONE,
                value: GhosttyStyleColorValue { _padding: 0 },
            },
            underline_color: GhosttyStyleColor {
                tag: GHOSTTY_STYLE_COLOR_NONE,
                value: GhosttyStyleColorValue { _padding: 0 },
            },
            bold: false,
            italic: false,
            faint: false,
            blink: false,
            inverse: false,
            invisible: false,
            strikethrough: false,
            overline: false,
            underline: GHOSTTY_SGR_UNDERLINE_NONE,
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct GhosttyPointCoordinate {
    pub x: u16,
    pub y: u32,
}

pub type GhosttyPointTag = c_int;
pub const GHOSTTY_POINT_TAG_ACTIVE: GhosttyPointTag = 0;
pub const GHOSTTY_POINT_TAG_VIEWPORT: GhosttyPointTag = 1;
pub const GHOSTTY_POINT_TAG_SCREEN: GhosttyPointTag = 2;
pub const GHOSTTY_POINT_TAG_HISTORY: GhosttyPointTag = 3;

#[repr(C)]
#[derive(Copy, Clone)]
pub union GhosttyPointValue {
    pub coordinate: GhosttyPointCoordinate,
    pub _padding: [u64; 2],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyPoint {
    pub tag: GhosttyPointTag,
    pub value: GhosttyPointValue,
}

impl GhosttyPoint {
    pub fn viewport(x: u16, y: u32) -> Self {
        Self {
            tag: GHOSTTY_POINT_TAG_VIEWPORT,
            value: GhosttyPointValue {
                coordinate: GhosttyPointCoordinate { x, y },
            },
        }
    }

    pub fn active(x: u16, y: u32) -> Self {
        Self {
            tag: GHOSTTY_POINT_TAG_ACTIVE,
            value: GhosttyPointValue {
                coordinate: GhosttyPointCoordinate { x, y },
            },
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyGridRef {
    pub size: usize,
    pub node: *mut c_void,
    pub x: u16,
    pub y: u16,
}

impl Default for GhosttyGridRef {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            node: std::ptr::null_mut(),
            x: 0,
            y: 0,
        }
    }
}

pub type GhosttyMode = u16;

#[inline]
pub const fn ghostty_mode_new(value: u16, ansi: bool) -> GhosttyMode {
    (value & 0x7FFF) | ((ansi as u16) << 15)
}

pub const GHOSTTY_MODE_X10_MOUSE: GhosttyMode = ghostty_mode_new(9, false);
pub const GHOSTTY_MODE_NORMAL_MOUSE: GhosttyMode = ghostty_mode_new(1000, false);
pub const GHOSTTY_MODE_BUTTON_MOUSE: GhosttyMode = ghostty_mode_new(1002, false);
pub const GHOSTTY_MODE_ANY_MOUSE: GhosttyMode = ghostty_mode_new(1003, false);
pub const GHOSTTY_MODE_FOCUS_EVENT: GhosttyMode = ghostty_mode_new(1004, false);
pub const GHOSTTY_MODE_UTF8_MOUSE: GhosttyMode = ghostty_mode_new(1005, false);
pub const GHOSTTY_MODE_SGR_MOUSE: GhosttyMode = ghostty_mode_new(1006, false);
pub const GHOSTTY_MODE_ALT_SCROLL: GhosttyMode = ghostty_mode_new(1007, false);
pub const GHOSTTY_MODE_URXVT_MOUSE: GhosttyMode = ghostty_mode_new(1015, false);
pub const GHOSTTY_MODE_SGR_PIXELS_MOUSE: GhosttyMode = ghostty_mode_new(1016, false);
pub const GHOSTTY_MODE_BRACKETED_PASTE: GhosttyMode = ghostty_mode_new(2004, false);
pub const GHOSTTY_MODE_SYNC_OUTPUT: GhosttyMode = ghostty_mode_new(2026, false);
pub const GHOSTTY_MODE_COLOR_SCHEME_REPORT: GhosttyMode = ghostty_mode_new(2031, false);
pub const GHOSTTY_MODE_PASTE_EVENTS: GhosttyMode = ghostty_mode_new(5522, false);

pub type GhosttyColorScheme = c_int;
pub const GHOSTTY_COLOR_SCHEME_LIGHT: GhosttyColorScheme = 0;
pub const GHOSTTY_COLOR_SCHEME_DARK: GhosttyColorScheme = 1;

pub type GhosttyOscTerminator = c_int;
pub const GHOSTTY_OSC_TERMINATOR_ST: GhosttyOscTerminator = 0;
pub const GHOSTTY_OSC_TERMINATOR_BEL: GhosttyOscTerminator = 1;

// ---------------------------------------------------------------------------
// Terminal (terminal.h)
// ---------------------------------------------------------------------------

pub type GhosttyTerminalCompressionMode = c_int;
pub const GHOSTTY_TERMINAL_COMPRESSION_MODE_INCREMENTAL: GhosttyTerminalCompressionMode = 0;
pub const GHOSTTY_TERMINAL_COMPRESSION_MODE_FULL: GhosttyTerminalCompressionMode = 1;

pub type GhosttyTerminalCompressionResult = c_int;
pub const GHOSTTY_TERMINAL_COMPRESSION_RESULT_UNSUPPORTED: GhosttyTerminalCompressionResult = 0;
pub const GHOSTTY_TERMINAL_COMPRESSION_RESULT_PENDING: GhosttyTerminalCompressionResult = 1;
pub const GHOSTTY_TERMINAL_COMPRESSION_RESULT_COMPLETE: GhosttyTerminalCompressionResult = 2;

pub type GhosttyTerminalScrollViewportTag = c_int;
pub const GHOSTTY_SCROLL_VIEWPORT_TOP: GhosttyTerminalScrollViewportTag = 0;
pub const GHOSTTY_SCROLL_VIEWPORT_BOTTOM: GhosttyTerminalScrollViewportTag = 1;
pub const GHOSTTY_SCROLL_VIEWPORT_DELTA: GhosttyTerminalScrollViewportTag = 2;
pub const GHOSTTY_SCROLL_VIEWPORT_ROW: GhosttyTerminalScrollViewportTag = 3;
pub const GHOSTTY_SCROLL_VIEWPORT_DELTA_PROMPT: GhosttyTerminalScrollViewportTag = 4;

#[repr(C)]
#[derive(Copy, Clone)]
pub union GhosttyTerminalScrollViewportValue {
    pub delta: isize,
    pub row: usize,
    pub delta_prompt: isize,
    pub _padding: [u64; 2],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyTerminalScrollViewport {
    pub tag: GhosttyTerminalScrollViewportTag,
    pub value: GhosttyTerminalScrollViewportValue,
}

pub type GhosttyTerminalScreen = c_int;
pub const GHOSTTY_TERMINAL_SCREEN_PRIMARY: GhosttyTerminalScreen = 0;
pub const GHOSTTY_TERMINAL_SCREEN_ALTERNATE: GhosttyTerminalScreen = 1;

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct GhosttyTerminalScrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalModeConfig {
    pub mode: GhosttyMode,
    pub value: bool,
}

pub type GhosttyTerminalUnknownSequenceTag = c_int;
pub const GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_APC: GhosttyTerminalUnknownSequenceTag = 0;
pub const GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_OSC: GhosttyTerminalUnknownSequenceTag = 1;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalUnknownStringSequence {
    pub truncated: bool,
    pub content: GhosttyString,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalUnknownOscSequence {
    pub truncated: bool,
    pub content: GhosttyString,
    pub terminator: GhosttyOscTerminator,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union GhosttyTerminalUnknownSequenceValue {
    pub apc: GhosttyTerminalUnknownStringSequence,
    pub osc: GhosttyTerminalUnknownOscSequence,
    pub _padding: [u64; 16],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct GhosttyTerminalUnknownSequence {
    pub tag: GhosttyTerminalUnknownSequenceTag,
    pub value: GhosttyTerminalUnknownSequenceValue,
}

pub type GhosttyClipboardLocation = c_int;
pub const GHOSTTY_CLIPBOARD_LOCATION_STANDARD: GhosttyClipboardLocation = 0;
pub const GHOSTTY_CLIPBOARD_LOCATION_SELECTION: GhosttyClipboardLocation = 1;
pub const GHOSTTY_CLIPBOARD_LOCATION_PRIMARY: GhosttyClipboardLocation = 2;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyClipboardContent {
    pub mime: GhosttyString,
    pub data: GhosttyString,
}

pub type GhosttyClipboardWriteResult = c_int;
pub const GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS: GhosttyClipboardWriteResult = 0;
pub const GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED: GhosttyClipboardWriteResult = 1;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyClipboardWriteReply {
    pub size: usize,
    pub result: GhosttyClipboardWriteResult,
    pub remember: bool,
}

pub type GhosttyClipboardWriteReplyFn = unsafe extern "C" fn(
    write: *const GhosttyClipboardWrite,
    reply: *const GhosttyClipboardWriteReply,
);

#[repr(C)]
pub struct GhosttyClipboardWrite {
    pub size: usize,
    pub location: GhosttyClipboardLocation,
    pub contents: *const GhosttyClipboardContent,
    pub contents_len: usize,
    pub name: GhosttyString,
    pub granted: bool,
    pub can_remember: bool,
    pub ctx: *const c_void,
    pub reply: Option<GhosttyClipboardWriteReplyFn>,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalDesktopNotification {
    pub size: usize,
    pub title: GhosttyString,
    pub body: GhosttyString,
}

pub type GhosttyTerminalProgressState = c_int;
pub const GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE: GhosttyTerminalProgressState = 0;
pub const GHOSTTY_TERMINAL_PROGRESS_STATE_SET: GhosttyTerminalProgressState = 1;
pub const GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR: GhosttyTerminalProgressState = 2;
pub const GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE: GhosttyTerminalProgressState = 3;
pub const GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE: GhosttyTerminalProgressState = 4;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalProgressReport {
    pub size: usize,
    pub state: GhosttyTerminalProgressState,
    pub progress: i8,
}

pub type GhosttyProgramStatusState = c_int;
pub const GHOSTTY_PROGRAM_STATUS_STATE_IDLE: GhosttyProgramStatusState = 0;
pub const GHOSTTY_PROGRAM_STATUS_STATE_WORKING: GhosttyProgramStatusState = 1;
pub const GHOSTTY_PROGRAM_STATUS_STATE_DONE: GhosttyProgramStatusState = 2;
pub const GHOSTTY_PROGRAM_STATUS_STATE_BLOCKED: GhosttyProgramStatusState = 3;
pub const GHOSTTY_PROGRAM_STATUS_STATE_ERROR: GhosttyProgramStatusState = 4;
pub const GHOSTTY_PROGRAM_STATUS_STATE_CLEAR: GhosttyProgramStatusState = 5;

pub type GhosttyProgramStatusKind = c_int;
pub const GHOSTTY_PROGRAM_STATUS_KIND_NONE: GhosttyProgramStatusKind = 0;
pub const GHOSTTY_PROGRAM_STATUS_KIND_PERMISSION: GhosttyProgramStatusKind = 1;
pub const GHOSTTY_PROGRAM_STATUS_KIND_QUESTION: GhosttyProgramStatusKind = 2;
pub const GHOSTTY_PROGRAM_STATUS_KIND_AUTH: GhosttyProgramStatusKind = 3;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalProgramStatus {
    pub size: usize,
    pub state: GhosttyProgramStatusState,
    pub kind: GhosttyProgramStatusKind,
    pub progress: i8,
    pub id: GhosttyString,
    pub app: GhosttyString,
    pub title: GhosttyString,
    pub message: GhosttyString,
}

// Terminal callback signatures
pub type GhosttyTerminalBellFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void);
pub type GhosttyTerminalTitleChangedFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void);
pub type GhosttyTerminalPwdChangedFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void);
pub type GhosttyTerminalRenderHoldFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void, bool);
pub type GhosttyTerminalColorSchemeFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *mut GhosttyColorScheme) -> bool;
pub type GhosttyTerminalUnknownSequenceFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyTerminalUnknownSequence);
pub type GhosttyTerminalClipboardWriteFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyClipboardWrite);
pub type GhosttyTerminalDesktopNotificationFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyTerminalDesktopNotification);
pub type GhosttyTerminalProgressReportFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyTerminalProgressReport);
pub type GhosttyTerminalProgramStatusFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyTerminalProgramStatus);

// Terminal options
pub type GhosttyTerminalOption = c_int;
pub const GHOSTTY_TERMINAL_OPT_USERDATA: GhosttyTerminalOption = 0;
pub const GHOSTTY_TERMINAL_OPT_WRITE_PTY: GhosttyTerminalOption = 1;
pub const GHOSTTY_TERMINAL_OPT_BELL: GhosttyTerminalOption = 2;
pub const GHOSTTY_TERMINAL_OPT_TITLE_CHANGED: GhosttyTerminalOption = 5;
pub const GHOSTTY_TERMINAL_OPT_COLOR_SCHEME: GhosttyTerminalOption = 7;
pub const GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND: GhosttyTerminalOption = 11;
pub const GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND: GhosttyTerminalOption = 12;
pub const GHOSTTY_TERMINAL_OPT_COLOR_CURSOR: GhosttyTerminalOption = 13;
pub const GHOSTTY_TERMINAL_OPT_COLOR_PALETTE: GhosttyTerminalOption = 14;
pub const GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT: GhosttyTerminalOption = 15;
pub const GHOSTTY_TERMINAL_OPT_SELECTION: GhosttyTerminalOption = 21;
pub const GHOSTTY_TERMINAL_OPT_PWD_CHANGED: GhosttyTerminalOption = 25;
pub const GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE: GhosttyTerminalOption = 26;
pub const GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES: GhosttyTerminalOption = 27;
pub const GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES: GhosttyTerminalOption = 28;
pub const GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION: GhosttyTerminalOption = 29;
pub const GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT: GhosttyTerminalOption = 30;
pub const GHOSTTY_TERMINAL_OPT_MODE: GhosttyTerminalOption = 34;
pub const GHOSTTY_TERMINAL_OPT_UNKNOWN_SEQUENCE: GhosttyTerminalOption = 35;
pub const GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES: GhosttyTerminalOption = 36;
pub const GHOSTTY_TERMINAL_OPT_RENDER_HOLD: GhosttyTerminalOption = 41;
pub const GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS: GhosttyTerminalOption = 46;

// Terminal data keys
pub type GhosttyTerminalData = c_int;
pub const GHOSTTY_TERMINAL_DATA_COLS: GhosttyTerminalData = 1;
pub const GHOSTTY_TERMINAL_DATA_ROWS: GhosttyTerminalData = 2;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_X: GhosttyTerminalData = 3;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_Y: GhosttyTerminalData = 4;
pub const GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN: GhosttyTerminalData = 6;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE: GhosttyTerminalData = 7;
pub const GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS: GhosttyTerminalData = 8;
pub const GHOSTTY_TERMINAL_DATA_SCROLLBAR: GhosttyTerminalData = 9;
pub const GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING: GhosttyTerminalData = 11;
pub const GHOSTTY_TERMINAL_DATA_TITLE: GhosttyTerminalData = 12;
pub const GHOSTTY_TERMINAL_DATA_PWD: GhosttyTerminalData = 13;
pub const GHOSTTY_TERMINAL_DATA_TOTAL_ROWS: GhosttyTerminalData = 14;
pub const GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS: GhosttyTerminalData = 15;
pub const GHOSTTY_TERMINAL_DATA_WIDTH_PX: GhosttyTerminalData = 16;
pub const GHOSTTY_TERMINAL_DATA_HEIGHT_PX: GhosttyTerminalData = 17;
pub const GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS: GhosttyTerminalData = 30;
pub const GHOSTTY_TERMINAL_DATA_SELECTION: GhosttyTerminalData = 31;
pub const GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE: GhosttyTerminalData = 32;
pub const GHOSTTY_TERMINAL_DATA_MODE: GhosttyTerminalData = 37;
pub const GHOSTTY_TERMINAL_DATA_MOUSE_SHAPE: GhosttyTerminalData = 41;

// ---------------------------------------------------------------------------
// Render State (render.h)
// ---------------------------------------------------------------------------

pub type GhosttyRenderStateDirty = c_int;
pub const GHOSTTY_RENDER_STATE_DIRTY_FALSE: GhosttyRenderStateDirty = 0;
pub const GHOSTTY_RENDER_STATE_DIRTY_PARTIAL: GhosttyRenderStateDirty = 1;
pub const GHOSTTY_RENDER_STATE_DIRTY_FULL: GhosttyRenderStateDirty = 2;

pub type GhosttyRenderStateCursorVisualStyle = c_int;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR: GhosttyRenderStateCursorVisualStyle = 0;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK: GhosttyRenderStateCursorVisualStyle = 1;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE: GhosttyRenderStateCursorVisualStyle =
    2;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW:
    GhosttyRenderStateCursorVisualStyle = 3;

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct GhosttyRenderStateOverscan {
    pub above: u16,
    pub below: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct GhosttyRenderStateRowId {
    pub bits: [u64; 2],
}

pub type GhosttyRenderStateData = c_int;
pub const GHOSTTY_RENDER_STATE_DATA_COLS: GhosttyRenderStateData = 1;
pub const GHOSTTY_RENDER_STATE_DATA_ROWS: GhosttyRenderStateData = 2;
pub const GHOSTTY_RENDER_STATE_DATA_DIRTY: GhosttyRenderStateData = 3;
pub const GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR: GhosttyRenderStateData = 4;
pub const GHOSTTY_RENDER_STATE_DATA_CURSOR: GhosttyRenderStateData = 18;
pub const GHOSTTY_RENDER_STATE_DATA_COLORS: GhosttyRenderStateData = 19;
pub const GHOSTTY_RENDER_STATE_DATA_OVERSCAN: GhosttyRenderStateData = 20;
pub const GHOSTTY_RENDER_STATE_DATA_IMAGE_ITERATOR: GhosttyRenderStateData = 22;
pub const GHOSTTY_RENDER_STATE_DATA_IMAGE_PLACEMENTS_GENERATION: GhosttyRenderStateData = 23;

pub type GhosttyRenderStateOption = c_int;
pub const GHOSTTY_RENDER_STATE_OPTION_DIRTY: GhosttyRenderStateOption = 0;
pub const GHOSTTY_RENDER_STATE_OPTION_OVERSCAN: GhosttyRenderStateOption = 1;

pub type GhosttyImageProtocol = c_int;
pub const GHOSTTY_IMAGE_PROTOCOL_KITTY: GhosttyImageProtocol = 0;

pub type GhosttyRenderStateImageLayer = c_int;
pub const GHOSTTY_RENDER_STATE_IMAGE_LAYER_BELOW_BACKGROUND: GhosttyRenderStateImageLayer = 0;
pub const GHOSTTY_RENDER_STATE_IMAGE_LAYER_BELOW_TEXT: GhosttyRenderStateImageLayer = 1;
pub const GHOSTTY_RENDER_STATE_IMAGE_LAYER_ABOVE_TEXT: GhosttyRenderStateImageLayer = 2;

pub type GhosttyRenderStateKittyPlacementKind = c_int;
pub const GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_VIRTUAL: GhosttyRenderStateKittyPlacementKind =
    0;
pub const GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_RELATIVE: GhosttyRenderStateKittyPlacementKind =
    1;
pub const GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_FIXED: GhosttyRenderStateKittyPlacementKind = 2;

pub type GhosttyRenderStateImageData = c_int;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_PROTOCOL: GhosttyRenderStateImageData = 1;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_ID: GhosttyRenderStateImageData = 2;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_IMAGE_GENERATION: GhosttyRenderStateImageData = 3;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_LAYER: GhosttyRenderStateImageData = 4;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_Z: GhosttyRenderStateImageData = 5;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_COL: GhosttyRenderStateImageData = 6;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_VIEWPORT_ROW: GhosttyRenderStateImageData = 7;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_GRID_COLS: GhosttyRenderStateImageData = 8;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_GRID_ROWS: GhosttyRenderStateImageData = 9;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_X: GhosttyRenderStateImageData = 10;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_CELL_OFFSET_Y: GhosttyRenderStateImageData = 11;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_WIDTH: GhosttyRenderStateImageData = 12;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_PIXEL_HEIGHT: GhosttyRenderStateImageData = 13;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_X: GhosttyRenderStateImageData = 14;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_Y: GhosttyRenderStateImageData = 15;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_WIDTH: GhosttyRenderStateImageData = 16;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_SOURCE_HEIGHT: GhosttyRenderStateImageData = 17;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_KITTY_PLACEMENT_ID: GhosttyRenderStateImageData = 18;
pub const GHOSTTY_RENDER_STATE_IMAGE_DATA_KITTY_KIND: GhosttyRenderStateImageData = 19;

pub type GhosttyRenderStateRowData = c_int;
pub const GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY: GhosttyRenderStateRowData = 1;
pub const GHOSTTY_RENDER_STATE_ROW_DATA_RAW: GhosttyRenderStateRowData = 2;
pub const GHOSTTY_RENDER_STATE_ROW_DATA_CELLS: GhosttyRenderStateRowData = 3;
pub const GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION: GhosttyRenderStateRowData = 4;
pub const GHOSTTY_RENDER_STATE_ROW_DATA_CELLS_RAW: GhosttyRenderStateRowData = 5;
pub const GHOSTTY_RENDER_STATE_ROW_DATA_VIEWPORT_Y: GhosttyRenderStateRowData = 6;
pub const GHOSTTY_RENDER_STATE_ROW_DATA_ID: GhosttyRenderStateRowData = 7;

pub type GhosttyRenderStateRowCellsData = c_int;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW: GhosttyRenderStateRowCellsData = 1;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE: GhosttyRenderStateRowCellsData = 2;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN: GhosttyRenderStateRowCellsData = 3;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR: GhosttyRenderStateRowCellsData = 5;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR: GhosttyRenderStateRowCellsData = 6;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED: GhosttyRenderStateRowCellsData = 7;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING: GhosttyRenderStateRowCellsData = 8;
pub const GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8: GhosttyRenderStateRowCellsData = 9;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyRenderStateRowSelection {
    pub size: usize,
    pub start_x: u16,
    pub end_x: u16,
}

impl Default for GhosttyRenderStateRowSelection {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            start_x: 0,
            end_x: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyRenderStateCursor {
    pub size: usize,
    pub viewport_has_value: bool,
    pub viewport_x: u16,
    pub viewport_y: u16,
    pub wide_tail: bool,
    pub visible: bool,
    pub blinking: bool,
    pub password_input: bool,
    pub visual_style: GhosttyRenderStateCursorVisualStyle,
}

impl Default for GhosttyRenderStateCursor {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            viewport_has_value: false,
            viewport_x: 0,
            viewport_y: 0,
            wide_tail: false,
            visible: false,
            blinking: false,
            password_input: false,
            visual_style: GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK,
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyRenderStateColors {
    pub size: usize,
    pub background: GhosttyColorRgb,
    pub foreground: GhosttyColorRgb,
    pub cursor: GhosttyColorRgb,
    pub cursor_has_value: bool,
    pub palette: [GhosttyColorRgb; 256],
}

impl Default for GhosttyRenderStateColors {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            background: GhosttyColorRgb::default(),
            foreground: GhosttyColorRgb::default(),
            cursor: GhosttyColorRgb::default(),
            cursor_has_value: false,
            palette: [GhosttyColorRgb::default(); 256],
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot Decoder (snapshot.h)
// ---------------------------------------------------------------------------

pub type GhosttySnapshotDecoderOption = c_int;
pub const GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES: GhosttySnapshotDecoderOption = 0;
pub const GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION: GhosttySnapshotDecoderOption = 1;
pub const GHOSTTY_SNAPSHOT_DECODER_OPT_COMPRESS_HISTORY: GhosttySnapshotDecoderOption = 2;
pub const GHOSTTY_SNAPSHOT_DECODER_OPT_KITTY_IMAGE_STORAGE_LIMIT: GhosttySnapshotDecoderOption = 3;

pub type GhosttySnapshotDecoderProgressKind = c_int;
pub const GHOSTTY_SNAPSHOT_DECODER_PROGRESS_KIND_PAGE: GhosttySnapshotDecoderProgressKind = 0;
pub const GHOSTTY_SNAPSHOT_DECODER_PROGRESS_KIND_IMAGE: GhosttySnapshotDecoderProgressKind = 1;

pub type GhosttySnapshotDecoderData = c_int;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET: GhosttySnapshotDecoderData = 2;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_HISTORY_ROWS_PRIMARY: GhosttySnapshotDecoderData = 3;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_HISTORY_ROWS_ALTERNATE: GhosttySnapshotDecoderData = 4;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_SCREEN: GhosttySnapshotDecoderData = 5;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_ROWS: GhosttySnapshotDecoderData = 6;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_REMAINING: GhosttySnapshotDecoderData = 7;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_KIND: GhosttySnapshotDecoderData = 11;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_IMAGE_ID: GhosttySnapshotDecoderData = 12;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_IMAGE_FRAME: GhosttySnapshotDecoderData = 13;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_IMAGE_BYTES: GhosttySnapshotDecoderData = 14;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_KITTY_IMAGES_PRIMARY: GhosttySnapshotDecoderData = 15;
pub const GHOSTTY_SNAPSHOT_DECODER_DATA_KITTY_PENDING_BYTES_PRIMARY: GhosttySnapshotDecoderData =
    17;

// ---------------------------------------------------------------------------
// Kitty Graphics (kitty_graphics.h)
// ---------------------------------------------------------------------------

pub type GhosttyKittyGraphicsData = c_int;
pub const GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR: GhosttyKittyGraphicsData = 1;
pub const GHOSTTY_KITTY_GRAPHICS_DATA_GENERATION: GhosttyKittyGraphicsData = 2;

pub type GhosttyKittyImageFormat = c_int;
pub const GHOSTTY_KITTY_IMAGE_FORMAT_RGB: GhosttyKittyImageFormat = 0;
pub const GHOSTTY_KITTY_IMAGE_FORMAT_RGBA: GhosttyKittyImageFormat = 1;
pub const GHOSTTY_KITTY_IMAGE_FORMAT_PNG: GhosttyKittyImageFormat = 2;
pub const GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA: GhosttyKittyImageFormat = 3;
pub const GHOSTTY_KITTY_IMAGE_FORMAT_GRAY: GhosttyKittyImageFormat = 4;

pub type GhosttyKittyGraphicsImageData = c_int;
pub const GHOSTTY_KITTY_IMAGE_DATA_ID: GhosttyKittyGraphicsImageData = 1;
pub const GHOSTTY_KITTY_IMAGE_DATA_NUMBER: GhosttyKittyGraphicsImageData = 2;
pub const GHOSTTY_KITTY_IMAGE_DATA_WIDTH: GhosttyKittyGraphicsImageData = 3;
pub const GHOSTTY_KITTY_IMAGE_DATA_HEIGHT: GhosttyKittyGraphicsImageData = 4;
pub const GHOSTTY_KITTY_IMAGE_DATA_FORMAT: GhosttyKittyGraphicsImageData = 5;
pub const GHOSTTY_KITTY_IMAGE_DATA_DATA_PTR: GhosttyKittyGraphicsImageData = 7;
pub const GHOSTTY_KITTY_IMAGE_DATA_DATA_LEN: GhosttyKittyGraphicsImageData = 8;
pub const GHOSTTY_KITTY_IMAGE_DATA_GENERATION: GhosttyKittyGraphicsImageData = 9;

// ---------------------------------------------------------------------------
// Key & Mouse & Focus & Paste (key/*.h, mouse/*.h, focus.h, paste.h)
// ---------------------------------------------------------------------------

pub type GhosttyKeyAction = c_int;
pub const GHOSTTY_KEY_ACTION_RELEASE: GhosttyKeyAction = 0;
pub const GHOSTTY_KEY_ACTION_PRESS: GhosttyKeyAction = 1;
pub const GHOSTTY_KEY_ACTION_REPEAT: GhosttyKeyAction = 2;

pub type GhosttyMods = u16;
pub const GHOSTTY_MODS_SHIFT: GhosttyMods = 1 << 0;
pub const GHOSTTY_MODS_CTRL: GhosttyMods = 1 << 1;
pub const GHOSTTY_MODS_ALT: GhosttyMods = 1 << 2;
pub const GHOSTTY_MODS_SUPER: GhosttyMods = 1 << 3;
pub const GHOSTTY_MODS_CAPS_LOCK: GhosttyMods = 1 << 4;
pub const GHOSTTY_MODS_NUM_LOCK: GhosttyMods = 1 << 5;
pub const GHOSTTY_MODS_SHIFT_SIDE: GhosttyMods = 1 << 6;
pub const GHOSTTY_MODS_CTRL_SIDE: GhosttyMods = 1 << 7;
pub const GHOSTTY_MODS_ALT_SIDE: GhosttyMods = 1 << 8;
pub const GHOSTTY_MODS_SUPER_SIDE: GhosttyMods = 1 << 9;

pub type GhosttyKey = c_int;

pub type GhosttyKittyKeyFlags = u8;
pub const GHOSTTY_KITTY_KEY_DISABLED: GhosttyKittyKeyFlags = 0;
pub const GHOSTTY_KITTY_KEY_DISAMBIGUATE: GhosttyKittyKeyFlags = 1 << 0;
pub const GHOSTTY_KITTY_KEY_REPORT_EVENTS: GhosttyKittyKeyFlags = 1 << 1;
pub const GHOSTTY_KITTY_KEY_REPORT_ALTERNATES: GhosttyKittyKeyFlags = 1 << 2;
pub const GHOSTTY_KITTY_KEY_REPORT_ALL: GhosttyKittyKeyFlags = 1 << 3;
pub const GHOSTTY_KITTY_KEY_REPORT_ASSOCIATED: GhosttyKittyKeyFlags = 1 << 4;

pub type GhosttyOptionAsAlt = c_int;
pub const GHOSTTY_OPTION_AS_ALT_FALSE: GhosttyOptionAsAlt = 0;
pub const GHOSTTY_OPTION_AS_ALT_TRUE: GhosttyOptionAsAlt = 1;
pub const GHOSTTY_OPTION_AS_ALT_LEFT: GhosttyOptionAsAlt = 2;
pub const GHOSTTY_OPTION_AS_ALT_RIGHT: GhosttyOptionAsAlt = 3;

pub type GhosttyKeyEncoderOption = c_int;
pub const GHOSTTY_KEY_ENCODER_OPT_CURSOR_KEY_APPLICATION: GhosttyKeyEncoderOption = 0;
pub const GHOSTTY_KEY_ENCODER_OPT_KEYPAD_KEY_APPLICATION: GhosttyKeyEncoderOption = 1;
pub const GHOSTTY_KEY_ENCODER_OPT_IGNORE_KEYPAD_WITH_NUMLOCK: GhosttyKeyEncoderOption = 2;
pub const GHOSTTY_KEY_ENCODER_OPT_ALT_ESC_PREFIX: GhosttyKeyEncoderOption = 3;
pub const GHOSTTY_KEY_ENCODER_OPT_MODIFY_OTHER_KEYS_STATE_2: GhosttyKeyEncoderOption = 4;
pub const GHOSTTY_KEY_ENCODER_OPT_KITTY_FLAGS: GhosttyKeyEncoderOption = 5;
pub const GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT: GhosttyKeyEncoderOption = 6;
pub const GHOSTTY_KEY_ENCODER_OPT_BACKARROW_KEY_MODE: GhosttyKeyEncoderOption = 7;

pub type GhosttyMouseAction = c_int;
pub const GHOSTTY_MOUSE_ACTION_PRESS: GhosttyMouseAction = 0;
pub const GHOSTTY_MOUSE_ACTION_RELEASE: GhosttyMouseAction = 1;
pub const GHOSTTY_MOUSE_ACTION_MOTION: GhosttyMouseAction = 2;

pub type GhosttyMouseButton = c_int;
pub const GHOSTTY_MOUSE_BUTTON_UNKNOWN: GhosttyMouseButton = 0;
pub const GHOSTTY_MOUSE_BUTTON_LEFT: GhosttyMouseButton = 1;
pub const GHOSTTY_MOUSE_BUTTON_RIGHT: GhosttyMouseButton = 2;
pub const GHOSTTY_MOUSE_BUTTON_MIDDLE: GhosttyMouseButton = 3;
pub const GHOSTTY_MOUSE_BUTTON_FOUR: GhosttyMouseButton = 4;
pub const GHOSTTY_MOUSE_BUTTON_FIVE: GhosttyMouseButton = 5;
pub const GHOSTTY_MOUSE_BUTTON_SIX: GhosttyMouseButton = 6;
pub const GHOSTTY_MOUSE_BUTTON_SEVEN: GhosttyMouseButton = 7;
pub const GHOSTTY_MOUSE_BUTTON_EIGHT: GhosttyMouseButton = 8;
pub const GHOSTTY_MOUSE_BUTTON_NINE: GhosttyMouseButton = 9;
pub const GHOSTTY_MOUSE_BUTTON_TEN: GhosttyMouseButton = 10;
pub const GHOSTTY_MOUSE_BUTTON_ELEVEN: GhosttyMouseButton = 11;

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct GhosttyMousePosition {
    pub x: f32,
    pub y: f32,
}

pub type GhosttyMouseTrackingMode = c_int;
pub const GHOSTTY_MOUSE_TRACKING_NONE: GhosttyMouseTrackingMode = 0;
pub const GHOSTTY_MOUSE_TRACKING_X10: GhosttyMouseTrackingMode = 1;
pub const GHOSTTY_MOUSE_TRACKING_NORMAL: GhosttyMouseTrackingMode = 2;
pub const GHOSTTY_MOUSE_TRACKING_BUTTON: GhosttyMouseTrackingMode = 3;
pub const GHOSTTY_MOUSE_TRACKING_ANY: GhosttyMouseTrackingMode = 4;

pub type GhosttyMouseFormat = c_int;
pub const GHOSTTY_MOUSE_FORMAT_X10: GhosttyMouseFormat = 0;
pub const GHOSTTY_MOUSE_FORMAT_UTF8: GhosttyMouseFormat = 1;
pub const GHOSTTY_MOUSE_FORMAT_SGR: GhosttyMouseFormat = 2;
pub const GHOSTTY_MOUSE_FORMAT_URXVT: GhosttyMouseFormat = 3;
pub const GHOSTTY_MOUSE_FORMAT_SGR_PIXELS: GhosttyMouseFormat = 4;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyMouseEncoderSize {
    pub size: usize,
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub padding_top: u32,
    pub padding_bottom: u32,
    pub padding_right: u32,
    pub padding_left: u32,
}

impl Default for GhosttyMouseEncoderSize {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            screen_width: 0,
            screen_height: 0,
            cell_width: 1,
            cell_height: 1,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        }
    }
}

pub type GhosttyMouseEncoderOption = c_int;
pub const GHOSTTY_MOUSE_ENCODER_OPT_EVENT: GhosttyMouseEncoderOption = 0;
pub const GHOSTTY_MOUSE_ENCODER_OPT_FORMAT: GhosttyMouseEncoderOption = 1;
pub const GHOSTTY_MOUSE_ENCODER_OPT_SIZE: GhosttyMouseEncoderOption = 2;
pub const GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED: GhosttyMouseEncoderOption = 3;
pub const GHOSTTY_MOUSE_ENCODER_OPT_TRACK_LAST_CELL: GhosttyMouseEncoderOption = 4;

pub type GhosttyMouseShape = c_int;

pub type GhosttyFocusEvent = c_int;
pub const GHOSTTY_FOCUS_GAINED: GhosttyFocusEvent = 0;
pub const GHOSTTY_FOCUS_LOST: GhosttyFocusEvent = 1;

// ---------------------------------------------------------------------------
// Selection & Search (selection.h, search.h)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttySelection {
    pub size: usize,
    pub start: GhosttyGridRef,
    pub end: GhosttyGridRef,
    pub rectangle: bool,
}

impl Default for GhosttySelection {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            start: GhosttyGridRef::default(),
            end: GhosttyGridRef::default(),
            rectangle: false,
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttySelectionBuffer {
    pub ptr: *mut GhosttySelection,
    pub cap: usize,
    pub len: usize,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalSelectWordOptions {
    pub size: usize,
    pub ref_: GhosttyGridRef,
    pub boundary_codepoints: *const u32,
    pub boundary_codepoints_len: usize,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalSelectLineOptions {
    pub size: usize,
    pub ref_: GhosttyGridRef,
    pub whitespace: *const u32,
    pub whitespace_len: usize,
    pub semantic_prompt_boundary: bool,
}

pub type GhosttyFormatter = *mut c_void;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyFormatterScreenExtra {
    pub size: usize,
    pub cursor: bool,
    pub style: bool,
    pub hyperlink: bool,
    pub protection: bool,
    pub kitty_keyboard: bool,
    pub charsets: bool,
}

impl Default for GhosttyFormatterScreenExtra {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            cursor: false,
            style: false,
            hyperlink: false,
            protection: false,
            kitty_keyboard: false,
            charsets: false,
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyFormatterTerminalExtra {
    pub size: usize,
    pub palette: bool,
    pub modes: bool,
    pub scrolling_region: bool,
    pub tabstops: bool,
    pub pwd: bool,
    pub keyboard: bool,
    pub screen: GhosttyFormatterScreenExtra,
}

impl Default for GhosttyFormatterTerminalExtra {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            palette: false,
            modes: false,
            scrolling_region: false,
            tabstops: false,
            pwd: false,
            keyboard: false,
            screen: GhosttyFormatterScreenExtra::default(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyFormatterTerminalOptions {
    pub size: usize,
    pub emit: GhosttyFormatterFormat,
    pub unwrap: bool,
    pub trim: bool,
    pub extra: GhosttyFormatterTerminalExtra,
    pub selection: *const GhosttySelection,
}

impl Default for GhosttyFormatterTerminalOptions {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>(),
            emit: GHOSTTY_FORMATTER_FORMAT_PLAIN,
            unwrap: false,
            trim: false,
            extra: GhosttyFormatterTerminalExtra::default(),
            selection: std::ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttyTerminalSelectionFormatOptions {
    pub size: usize,
    pub emit: GhosttyFormatterFormat,
    pub unwrap: bool,
    pub trim: bool,
    pub selection: *const GhosttySelection,
}

pub type GhosttySelectionGestureBehavior = c_int;
pub const GHOSTTY_SELECTION_GESTURE_BEHAVIOR_CELL: GhosttySelectionGestureBehavior = 0;
pub const GHOSTTY_SELECTION_GESTURE_BEHAVIOR_WORD: GhosttySelectionGestureBehavior = 1;
pub const GHOSTTY_SELECTION_GESTURE_BEHAVIOR_LINE: GhosttySelectionGestureBehavior = 2;
pub const GHOSTTY_SELECTION_GESTURE_BEHAVIOR_OUTPUT: GhosttySelectionGestureBehavior = 3;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttySelectionGestureBehaviors {
    pub single_click: GhosttySelectionGestureBehavior,
    pub double_click: GhosttySelectionGestureBehavior,
    pub triple_click: GhosttySelectionGestureBehavior,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct GhosttySelectionGestureGeometry {
    pub columns: u32,
    pub cell_width: u32,
    pub padding_left: u32,
    pub screen_height: u32,
}

pub type GhosttySelectionGestureAutoscroll = c_int;
pub const GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_NONE: GhosttySelectionGestureAutoscroll = 0;
pub const GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_UP: GhosttySelectionGestureAutoscroll = 1;
pub const GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_DOWN: GhosttySelectionGestureAutoscroll = 2;

pub type GhosttySelectionGestureData = c_int;
pub const GHOSTTY_SELECTION_GESTURE_DATA_CLICK_COUNT: GhosttySelectionGestureData = 0;
pub const GHOSTTY_SELECTION_GESTURE_DATA_DRAGGED: GhosttySelectionGestureData = 1;
pub const GHOSTTY_SELECTION_GESTURE_DATA_AUTOSCROLL: GhosttySelectionGestureData = 2;
pub const GHOSTTY_SELECTION_GESTURE_DATA_BEHAVIOR: GhosttySelectionGestureData = 3;
pub const GHOSTTY_SELECTION_GESTURE_DATA_ANCHOR: GhosttySelectionGestureData = 4;

pub type GhosttySelectionGestureEventType = c_int;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS: GhosttySelectionGestureEventType = 0;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE: GhosttySelectionGestureEventType = 1;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG: GhosttySelectionGestureEventType = 2;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_AUTOSCROLL_TICK: GhosttySelectionGestureEventType =
    3;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DEEP_PRESS: GhosttySelectionGestureEventType = 4;

pub type GhosttySelectionGestureEventOption = c_int;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF: GhosttySelectionGestureEventOption = 0;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION: GhosttySelectionGestureEventOption = 1;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE: GhosttySelectionGestureEventOption =
    2;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS: GhosttySelectionGestureEventOption = 3;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS:
    GhosttySelectionGestureEventOption = 4;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_WORD_BOUNDARY_CODEPOINTS:
    GhosttySelectionGestureEventOption = 5;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_BEHAVIORS: GhosttySelectionGestureEventOption = 6;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_RECTANGLE: GhosttySelectionGestureEventOption = 7;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY: GhosttySelectionGestureEventOption = 8;
pub const GHOSTTY_SELECTION_GESTURE_EVENT_OPT_VIEWPORT: GhosttySelectionGestureEventOption = 9;

pub type GhosttySearchStatus = c_int;
pub const GHOSTTY_SEARCH_STATUS_RUNNING: GhosttySearchStatus = 0;
pub const GHOSTTY_SEARCH_STATUS_FEED_REQUIRED: GhosttySearchStatus = 1;
pub const GHOSTTY_SEARCH_STATUS_COMPLETE: GhosttySearchStatus = 2;

pub type GhosttySearchScroll = c_int;
pub const GHOSTTY_SEARCH_SCROLL_IF_NEEDED: GhosttySearchScroll = 0;
pub const GHOSTTY_SEARCH_SCROLL_NONE: GhosttySearchScroll = 1;

pub type GhosttySearchData = c_int;
pub const GHOSTTY_SEARCH_DATA_STATUS: GhosttySearchData = 0;
pub const GHOSTTY_SEARCH_DATA_NEEDLE: GhosttySearchData = 1;
pub const GHOSTTY_SEARCH_DATA_TOTAL_MATCHES: GhosttySearchData = 2;
pub const GHOSTTY_SEARCH_DATA_SELECTED_INDEX: GhosttySearchData = 3;
pub const GHOSTTY_SEARCH_DATA_SELECTED_MATCH: GhosttySearchData = 4;
pub const GHOSTTY_SEARCH_DATA_MATCHES: GhosttySearchData = 5;
pub const GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES: GhosttySearchData = 6;
pub const GHOSTTY_SEARCH_DATA_SELECT_SCROLL: GhosttySearchData = 7;

pub type GhosttySearchOption = c_int;
pub const GHOSTTY_SEARCH_OPT_NEEDLE: GhosttySearchOption = 0;
pub const GHOSTTY_SEARCH_OPT_SELECT_NEXT: GhosttySearchOption = 1;
pub const GHOSTTY_SEARCH_OPT_SELECT_PREV: GhosttySearchOption = 2;
pub const GHOSTTY_SEARCH_OPT_SELECT_SCROLL: GhosttySearchOption = 3;

// ---------------------------------------------------------------------------
// Packed GhosttyCell decoding (verified against ghostty_type_json() ABI manifest)
// ---------------------------------------------------------------------------

/// Decoded fields from a packed 64-bit `GhosttyCell`.
///
/// Exact bit layout verified against `/usr/lib/libghostty-vt.so` (`ghostty_type_json()`):
/// - bits `[0..=1]`  : `content_tag` (`0=CODEPOINT`, `1=CODEPOINT_GRAPHEME`, `2=BG_COLOR_PALETTE`, `3=BG_COLOR_RGB`)
/// - bits `[2..=25]` : 24-bit content union (u21 codepoint, u8 palette index, or RGB bytes `r=[2..9] g=[10..17] b=[18..25]`)
/// - bits `[26..=41]`: `style_id` (`u16`)
/// - bits `[42..=43]`: `wide` (`0=NARROW`, `1=WIDE`, `2=SPACER_TAIL`, `3=SPACER_HEAD`)
/// - bit  `44`       : `protected` (`bool`)
/// - bit  `45`       : `hyperlink` (`bool`)
/// - bits `[46..=47]`: `semantic_content` (`0=OUTPUT`, `1=INPUT`, `2=PROMPT`)
#[inline]
pub const fn cell_content_tag(raw: GhosttyCell) -> GhosttyCellContentTag {
    (raw & 0b11) as GhosttyCellContentTag
}

#[inline]
pub const fn cell_codepoint(raw: GhosttyCell) -> u32 {
    ((raw >> 2) & 0x1F_FFFF) as u32
}

#[inline]
pub const fn cell_bg_palette(raw: GhosttyCell) -> u8 {
    ((raw >> 2) & 0xFF) as u8
}

#[inline]
pub const fn cell_bg_rgb(raw: GhosttyCell) -> GhosttyColorRgb {
    GhosttyColorRgb {
        r: ((raw >> 2) & 0xFF) as u8,
        g: ((raw >> 10) & 0xFF) as u8,
        b: ((raw >> 18) & 0xFF) as u8,
    }
}

#[inline]
pub const fn cell_style_id(raw: GhosttyCell) -> GhosttyStyleId {
    ((raw >> 26) & 0xFFFF) as GhosttyStyleId
}

#[inline]
pub const fn cell_wide(raw: GhosttyCell) -> GhosttyCellWide {
    ((raw >> 42) & 0b11) as GhosttyCellWide
}

#[inline]
pub const fn cell_protected(raw: GhosttyCell) -> bool {
    ((raw >> 44) & 1) != 0
}

#[inline]
pub const fn cell_has_hyperlink(raw: GhosttyCell) -> bool {
    ((raw >> 45) & 1) != 0
}

#[inline]
pub const fn cell_semantic_content(raw: GhosttyCell) -> GhosttyCellSemanticContent {
    ((raw >> 46) & 0b11) as GhosttyCellSemanticContent
}

// ---------------------------------------------------------------------------
// Extern C functions
// ---------------------------------------------------------------------------

unsafe extern "C" {
    // Build info & ABI manifest
    pub fn ghostty_build_info(data: c_int, out: *mut c_void) -> GhosttyResult;
    pub fn ghostty_type_json() -> *const c_char;

    // Allocator & system callbacks
    pub fn ghostty_alloc(allocator: *const GhosttyAllocator, len: usize) -> *mut u8;
    pub fn ghostty_free(allocator: *const GhosttyAllocator, ptr: *mut u8, len: usize);
    pub fn ghostty_sys_set(option: GhosttySysOption, value: *const c_void) -> GhosttyResult;

    // Color helpers
    pub fn ghostty_color_palette_default(out: *mut GhosttyColorRgb);
    pub fn ghostty_color_parse(
        value: *const c_char,
        len: usize,
        out: *mut GhosttyColorRgb,
    ) -> GhosttyResult;
    pub fn ghostty_color_parse_palette_entry(
        value: *const c_char,
        len: usize,
        out_index: *mut u8,
        out_rgb: *mut GhosttyColorRgb,
    ) -> GhosttyResult;
    pub fn ghostty_color_scheme_report_encode(
        scheme: GhosttyColorScheme,
        buf: *mut c_char,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;

    // Terminal lifecycle & operations
    pub fn ghostty_terminal_new(
        allocator: *const GhosttyAllocator,
        terminal: *mut GhosttyTerminal,
        cols: u16,
        rows: u16,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_free(terminal: GhosttyTerminal);
    pub fn ghostty_terminal_reset(terminal: GhosttyTerminal);
    pub fn ghostty_terminal_resize(
        terminal: GhosttyTerminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_set(
        terminal: GhosttyTerminal,
        option: GhosttyTerminalOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_get(
        terminal: GhosttyTerminal,
        data: GhosttyTerminalData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);
    pub fn ghostty_terminal_scroll_viewport(
        terminal: GhosttyTerminal,
        behavior: GhosttyTerminalScrollViewport,
    );
    pub fn ghostty_terminal_compress(
        terminal: GhosttyTerminal,
        mode: GhosttyTerminalCompressionMode,
        out_result: *mut GhosttyTerminalCompressionResult,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_grid_ref(
        terminal: GhosttyTerminal,
        point: GhosttyPoint,
        out_ref: *mut GhosttyGridRef,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_point_from_grid_ref(
        terminal: GhosttyTerminal,
        ref_: *const GhosttyGridRef,
        tag: GhosttyPointTag,
        out: *mut GhosttyPointCoordinate,
    ) -> GhosttyResult;

    // Grid ref
    pub fn ghostty_grid_ref_hyperlink_uri(
        ref_: *const GhosttyGridRef,
        buf: *mut u8,
        buf_len: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;

    // Formatter
    pub fn ghostty_formatter_terminal_new(
        allocator: *const GhosttyAllocator,
        formatter: *mut GhosttyFormatter,
        terminal: GhosttyTerminal,
        options: GhosttyFormatterTerminalOptions,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_format_buf(
        formatter: GhosttyFormatter,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_free(formatter: GhosttyFormatter);

    // Render state
    pub fn ghostty_render_state_new(
        allocator: *const GhosttyAllocator,
        state: *mut GhosttyRenderState,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_free(state: GhosttyRenderState);
    pub fn ghostty_render_state_begin_update(
        state: GhosttyRenderState,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_end_update(state: GhosttyRenderState) -> GhosttyResult;
    pub fn ghostty_render_state_clean(state: GhosttyRenderState) -> GhosttyResult;
    pub fn ghostty_render_state_get(
        state: GhosttyRenderState,
        data: GhosttyRenderStateData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_set(
        state: GhosttyRenderState,
        option: GhosttyRenderStateOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_iterator_new(
        allocator: *const GhosttyAllocator,
        out_iterator: *mut GhosttyRenderStateRowIterator,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_iterator_free(iterator: GhosttyRenderStateRowIterator);
    pub fn ghostty_render_state_row_iterator_next(iterator: GhosttyRenderStateRowIterator) -> bool;
    pub fn ghostty_render_state_row_iterator_next_dirty(
        iterator: GhosttyRenderStateRowIterator,
        out_y: *mut u16,
    ) -> bool;
    pub fn ghostty_render_state_row_get(
        iterator: GhosttyRenderStateRowIterator,
        data: GhosttyRenderStateRowData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_cells_new(
        allocator: *const GhosttyAllocator,
        out_cells: *mut GhosttyRenderStateRowCells,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_cells_free(cells: GhosttyRenderStateRowCells);
    pub fn ghostty_render_state_row_cells_select(
        cells: GhosttyRenderStateRowCells,
        x: u16,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_cells_get(
        cells: GhosttyRenderStateRowCells,
        data: GhosttyRenderStateRowCellsData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_image_iterator_new(
        allocator: *const GhosttyAllocator,
        out_iterator: *mut GhosttyRenderStateImageIterator,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_image_iterator_free(iterator: GhosttyRenderStateImageIterator);
    pub fn ghostty_render_state_image_iterator_next(
        iterator: GhosttyRenderStateImageIterator,
    ) -> bool;
    pub fn ghostty_render_state_image_get(
        iterator: GhosttyRenderStateImageIterator,
        data: GhosttyRenderStateImageData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_image_get_multi(
        iterator: GhosttyRenderStateImageIterator,
        count: usize,
        keys: *const GhosttyRenderStateImageData,
        values: *mut *mut c_void,
        out_written: *mut usize,
    ) -> GhosttyResult;

    // Snapshot encoder/decoder
    pub fn ghostty_snapshot_encode_buf(
        terminal: GhosttyTerminal,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_new(
        allocator: *const GhosttyAllocator,
        decoder: *mut GhosttySnapshotDecoder,
        reader: GhosttyReader,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_new_buf(
        allocator: *const GhosttyAllocator,
        decoder: *mut GhosttySnapshotDecoder,
        ptr: *const u8,
        len: usize,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_free(decoder: GhosttySnapshotDecoder);
    pub fn ghostty_snapshot_decoder_set(
        decoder: GhosttySnapshotDecoder,
        option: GhosttySnapshotDecoderOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_get(
        decoder: GhosttySnapshotDecoder,
        data: GhosttySnapshotDecoderData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_ready(
        decoder: GhosttySnapshotDecoder,
        terminal: *mut GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_next(decoder: GhosttySnapshotDecoder) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_decode(
        decoder: GhosttySnapshotDecoder,
        terminal: *mut GhosttyTerminal,
    ) -> GhosttyResult;

    // Kitty graphics storage
    pub fn ghostty_kitty_graphics_get(
        graphics: GhosttyKittyGraphics,
        data: GhosttyKittyGraphicsData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_kitty_graphics_animation_tick(
        graphics: GhosttyKittyGraphics,
        now_ms: u64,
        next_ms: *mut u64,
    ) -> GhosttyResult;
    pub fn ghostty_kitty_graphics_image(
        graphics: GhosttyKittyGraphics,
        image_id: u32,
    ) -> GhosttyKittyGraphicsImage;
    pub fn ghostty_kitty_graphics_image_get(
        image: GhosttyKittyGraphicsImage,
        data: GhosttyKittyGraphicsImageData,
        out: *mut c_void,
    ) -> GhosttyResult;

    // Key event & encoder
    pub fn ghostty_key_event_new(
        allocator: *const GhosttyAllocator,
        event: *mut GhosttyKeyEvent,
    ) -> GhosttyResult;
    pub fn ghostty_key_event_free(event: GhosttyKeyEvent);
    pub fn ghostty_key_event_set_action(event: GhosttyKeyEvent, action: GhosttyKeyAction);
    pub fn ghostty_key_event_set_key(event: GhosttyKeyEvent, key: GhosttyKey);
    pub fn ghostty_key_event_set_mods(event: GhosttyKeyEvent, mods: GhosttyMods);
    pub fn ghostty_key_event_set_consumed_mods(event: GhosttyKeyEvent, consumed_mods: GhosttyMods);
    pub fn ghostty_key_event_set_composing(event: GhosttyKeyEvent, composing: bool);
    pub fn ghostty_key_event_set_utf8(event: GhosttyKeyEvent, utf8: *const c_char, len: usize);
    pub fn ghostty_key_event_set_unshifted_codepoint(event: GhosttyKeyEvent, codepoint: u32);

    pub fn ghostty_key_encoder_new(
        allocator: *const GhosttyAllocator,
        encoder: *mut GhosttyKeyEncoder,
    ) -> GhosttyResult;
    pub fn ghostty_key_encoder_free(encoder: GhosttyKeyEncoder);
    pub fn ghostty_key_encoder_setopt(
        encoder: GhosttyKeyEncoder,
        option: GhosttyKeyEncoderOption,
        value: *const c_void,
    );
    pub fn ghostty_key_encoder_setopt_from_terminal(
        encoder: GhosttyKeyEncoder,
        terminal: GhosttyTerminal,
    );
    pub fn ghostty_key_encoder_encode(
        encoder: GhosttyKeyEncoder,
        event: GhosttyKeyEvent,
        out_buf: *mut c_char,
        out_buf_size: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;

    // Mouse event & encoder
    pub fn ghostty_mouse_event_new(
        allocator: *const GhosttyAllocator,
        event: *mut GhosttyMouseEvent,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_event_free(event: GhosttyMouseEvent);
    pub fn ghostty_mouse_event_set_action(event: GhosttyMouseEvent, action: GhosttyMouseAction);
    pub fn ghostty_mouse_event_set_button(event: GhosttyMouseEvent, button: GhosttyMouseButton);
    pub fn ghostty_mouse_event_clear_button(event: GhosttyMouseEvent);
    pub fn ghostty_mouse_event_set_mods(event: GhosttyMouseEvent, mods: GhosttyMods);
    pub fn ghostty_mouse_event_set_position(
        event: GhosttyMouseEvent,
        position: GhosttyMousePosition,
    );

    pub fn ghostty_mouse_encoder_new(
        allocator: *const GhosttyAllocator,
        encoder: *mut GhosttyMouseEncoder,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_encoder_free(encoder: GhosttyMouseEncoder);
    pub fn ghostty_mouse_encoder_setopt(
        encoder: GhosttyMouseEncoder,
        option: GhosttyMouseEncoderOption,
        value: *const c_void,
    );
    pub fn ghostty_mouse_encoder_setopt_from_terminal(
        encoder: GhosttyMouseEncoder,
        terminal: GhosttyTerminal,
    );
    pub fn ghostty_mouse_encoder_reset(encoder: GhosttyMouseEncoder);
    pub fn ghostty_mouse_encoder_encode(
        encoder: GhosttyMouseEncoder,
        event: GhosttyMouseEvent,
        out_buf: *mut c_char,
        out_buf_size: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;

    // Paste & Focus
    pub fn ghostty_paste_is_safe(data: *const c_char, len: usize) -> bool;
    pub fn ghostty_paste_encode(
        data: *mut c_char,
        data_len: usize,
        bracketed: bool,
        buf: *mut c_char,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_focus_encode(
        event: GhosttyFocusEvent,
        buf: *mut c_char,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;

    // Selection & selection gestures
    pub fn ghostty_selection_gesture_new(
        allocator: *const GhosttyAllocator,
        out_gesture: *mut GhosttySelectionGesture,
    ) -> GhosttyResult;
    pub fn ghostty_selection_gesture_free(
        gesture: GhosttySelectionGesture,
        terminal: GhosttyTerminal,
    );
    pub fn ghostty_selection_gesture_reset(
        gesture: GhosttySelectionGesture,
        terminal: GhosttyTerminal,
    );
    pub fn ghostty_selection_gesture_get(
        gesture: GhosttySelectionGesture,
        terminal: GhosttyTerminal,
        data: GhosttySelectionGestureData,
        value: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_selection_gesture_event_new(
        allocator: *const GhosttyAllocator,
        out_event: *mut GhosttySelectionGestureEvent,
        event_type: GhosttySelectionGestureEventType,
    ) -> GhosttyResult;
    pub fn ghostty_selection_gesture_event_free(event: GhosttySelectionGestureEvent);
    pub fn ghostty_selection_gesture_event_set(
        event: GhosttySelectionGestureEvent,
        option: GhosttySelectionGestureEventOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_selection_gesture_event(
        gesture: GhosttySelectionGesture,
        terminal: GhosttyTerminal,
        event: GhosttySelectionGestureEvent,
        out_selection: *mut GhosttySelection,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_select_word(
        terminal: GhosttyTerminal,
        options: *const GhosttyTerminalSelectWordOptions,
        out_selection: *mut GhosttySelection,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_select_line(
        terminal: GhosttyTerminal,
        options: *const GhosttyTerminalSelectLineOptions,
        out_selection: *mut GhosttySelection,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_select_all(
        terminal: GhosttyTerminal,
        out_selection: *mut GhosttySelection,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_selection_format_buf(
        terminal: GhosttyTerminal,
        options: GhosttyTerminalSelectionFormatOptions,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;

    // Search
    pub fn ghostty_search_new(
        allocator: *const GhosttyAllocator,
        out_search: *mut GhosttySearch,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_search_free(search: GhosttySearch);
    pub fn ghostty_search_tick(
        search: GhosttySearch,
        out_status: *mut GhosttySearchStatus,
    ) -> GhosttyResult;
    pub fn ghostty_search_feed(search: GhosttySearch) -> GhosttyResult;
    pub fn ghostty_search_run(search: GhosttySearch) -> GhosttyResult;
    pub fn ghostty_search_set(
        search: GhosttySearch,
        option: GhosttySearchOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_search_get(
        search: GhosttySearch,
        data: GhosttySearchData,
        value: *mut c_void,
    ) -> GhosttyResult;
}
