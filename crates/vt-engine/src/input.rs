//! Safe wrappers for `libghostty-vt` key, mouse, paste, focus, and color-scheme encoders.
//!
//! Uses W3C physical scancodes (`PhysicalKey`), logical UTF-8 text, unshifted Unicode
//! codepoints, consumed modifiers (critical for AltGr on German/international layouts),
//! and Kitty Keyboard Protocol flags.

use std::ffi::{c_char, c_void};
use std::ptr;

use crate::{ColorScheme, Terminal, VtError, ffi};

// ---------------------------------------------------------------------------
// Modifiers & Kitty keyboard flags
// ---------------------------------------------------------------------------

/// Keyboard modifier bitmask matching `GhosttyMods` (`key/event.h`).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Hash)]
pub struct Modifiers(pub u16);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(ffi::GHOSTTY_MODS_SHIFT);
    pub const CTRL: Self = Self(ffi::GHOSTTY_MODS_CTRL);
    pub const ALT: Self = Self(ffi::GHOSTTY_MODS_ALT);
    pub const SUPER: Self = Self(ffi::GHOSTTY_MODS_SUPER);
    pub const CAPS_LOCK: Self = Self(ffi::GHOSTTY_MODS_CAPS_LOCK);
    pub const NUM_LOCK: Self = Self(ffi::GHOSTTY_MODS_NUM_LOCK);
    pub const SHIFT_RIGHT: Self = Self(ffi::GHOSTTY_MODS_SHIFT | ffi::GHOSTTY_MODS_SHIFT_SIDE);
    pub const CTRL_RIGHT: Self = Self(ffi::GHOSTTY_MODS_CTRL | ffi::GHOSTTY_MODS_CTRL_SIDE);
    pub const ALT_RIGHT: Self = Self(ffi::GHOSTTY_MODS_ALT | ffi::GHOSTTY_MODS_ALT_SIDE);
    pub const SUPER_RIGHT: Self = Self(ffi::GHOSTTY_MODS_SUPER | ffi::GHOSTTY_MODS_SUPER_SIDE);

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[inline]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Kitty keyboard protocol flags (`key/encoder.h`).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Hash)]
pub struct KittyKeyboardFlags(pub u8);

impl KittyKeyboardFlags {
    pub const DISABLED: Self = Self(ffi::GHOSTTY_KITTY_KEY_DISABLED);
    pub const DISAMBIGUATE: Self = Self(ffi::GHOSTTY_KITTY_KEY_DISAMBIGUATE);
    pub const REPORT_EVENTS: Self = Self(ffi::GHOSTTY_KITTY_KEY_REPORT_EVENTS);
    pub const REPORT_ALTERNATES: Self = Self(ffi::GHOSTTY_KITTY_KEY_REPORT_ALTERNATES);
    pub const REPORT_ALL: Self = Self(ffi::GHOSTTY_KITTY_KEY_REPORT_ALL);
    pub const REPORT_ASSOCIATED: Self = Self(ffi::GHOSTTY_KITTY_KEY_REPORT_ASSOCIATED);
    pub const ALL: Self = Self(
        ffi::GHOSTTY_KITTY_KEY_DISAMBIGUATE
            | ffi::GHOSTTY_KITTY_KEY_REPORT_EVENTS
            | ffi::GHOSTTY_KITTY_KEY_REPORT_ALTERNATES
            | ffi::GHOSTTY_KITTY_KEY_REPORT_ALL
            | ffi::GHOSTTY_KITTY_KEY_REPORT_ASSOCIATED,
    );

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl std::ops::BitOr for KittyKeyboardFlags {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Key event action matching `GhosttyKeyAction`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(i32)]
pub enum KeyAction {
    Release = ffi::GHOSTTY_KEY_ACTION_RELEASE,
    Press = ffi::GHOSTTY_KEY_ACTION_PRESS,
    Repeat = ffi::GHOSTTY_KEY_ACTION_REPEAT,
}

/// Option-as-Alt behavior (`GhosttyOptionAsAlt`).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(i32)]
pub enum OptionAsAlt {
    False = ffi::GHOSTTY_OPTION_AS_ALT_FALSE,
    True = ffi::GHOSTTY_OPTION_AS_ALT_TRUE,
    Left = ffi::GHOSTTY_OPTION_AS_ALT_LEFT,
    Right = ffi::GHOSTTY_OPTION_AS_ALT_RIGHT,
}

/// Layout-independent physical key codes matching `GhosttyKey` (`key/event.h`) 1:1.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum PhysicalKey {
    Unidentified = 0,

    // Writing System Keys (W3C § 3.1.1)
    Backquote,
    Backslash,
    BracketLeft,
    BracketRight,
    Comma,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Equal,
    IntlBackslash,
    IntlRo,
    IntlYen,
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    Minus,
    Period,
    Quote,
    Semicolon,
    Slash,

    // Functional Keys (W3C § 3.1.2)
    AltLeft,
    AltRight,
    Backspace,
    CapsLock,
    ContextMenu,
    ControlLeft,
    ControlRight,
    Enter,
    MetaLeft,
    MetaRight,
    ShiftLeft,
    ShiftRight,
    Space,
    Tab,
    Convert,
    KanaMode,
    NonConvert,

    // Control Pad Section (W3C § 3.2)
    Delete,
    End,
    Help,
    Home,
    Insert,
    PageDown,
    PageUp,

    // Arrow Pad Section (W3C § 3.3)
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,

    // Numpad Section (W3C § 3.4)
    NumLock,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadBackspace,
    NumpadClear,
    NumpadClearEntry,
    NumpadComma,
    NumpadDecimal,
    NumpadDivide,
    NumpadEnter,
    NumpadEqual,
    NumpadMemoryAdd,
    NumpadMemoryClear,
    NumpadMemoryRecall,
    NumpadMemoryStore,
    NumpadMemorySubtract,
    NumpadMultiply,
    NumpadParenLeft,
    NumpadParenRight,
    NumpadSubtract,
    NumpadSeparator,
    NumpadUp,
    NumpadDown,
    NumpadRight,
    NumpadLeft,
    NumpadBegin,
    NumpadHome,
    NumpadEnd,
    NumpadInsert,
    NumpadDelete,
    NumpadPageUp,
    NumpadPageDown,

    // Function Section (W3C § 3.5)
    Escape,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,
    F25,
    Fn,
    FnLock,
    PrintScreen,
    ScrollLock,
    Pause,

    // Media Keys (W3C § 3.6)
    BrowserBack,
    BrowserFavorites,
    BrowserForward,
    BrowserHome,
    BrowserRefresh,
    BrowserSearch,
    BrowserStop,
    Eject,
    LaunchApp1,
    LaunchApp2,
    LaunchMail,
    MediaPlayPause,
    MediaSelect,
    MediaStop,
    MediaTrackNext,
    MediaTrackPrevious,
    Power,
    Sleep,
    AudioVolumeDown,
    AudioVolumeMute,
    AudioVolumeUp,
    WakeUp,

    // Legacy, Non-standard, and Special Keys (W3C § 3.7)
    Copy,
    Cut,
    Paste,
}

/// Platform-normalized key event input for [`KeyEncoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEventInput<'a> {
    pub action: KeyAction,
    pub key: PhysicalKey,
    pub mods: Modifiers,
    /// Modifiers consumed by the keyboard layout to produce `utf8` (e.g. `Modifiers::ALT` for
    /// AltGr `@` on German QWERTZ, or `Modifiers::SHIFT` for uppercase/symbols).
    pub consumed_mods: Modifiers,
    /// Unmodified UTF-8 text from the layout (omit C0 controls `0x00..=0x1F` / `0x7F`).
    pub utf8: Option<&'a str>,
    /// Unicode scalar value produced by this physical key without Shift/Ctrl modifiers.
    pub unshifted_codepoint: Option<char>,
    pub composing: bool,
}

/// Safe RAII wrapper around `GhosttyKeyEncoder` + reusable `GhosttyKeyEvent`.
pub struct KeyEncoder {
    encoder: ffi::GhosttyKeyEncoder,
    event: ffi::GhosttyKeyEvent,
}

// SAFETY: `KeyEncoder` owns both opaque C handles exclusively; no shared interior state.
unsafe impl Send for KeyEncoder {}

impl KeyEncoder {
    pub fn new() -> Result<Self, VtError> {
        let mut encoder: ffi::GhosttyKeyEncoder = ptr::null_mut();
        // SAFETY: Passing NULL allocator uses libghostty's default allocator.
        let rc = unsafe { ffi::ghostty_key_encoder_new(ptr::null(), &raw mut encoder) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let mut event: ffi::GhosttyKeyEvent = ptr::null_mut();
        // SAFETY: Passing NULL allocator uses libghostty's default allocator.
        let rc = unsafe { ffi::ghostty_key_event_new(ptr::null(), &raw mut event) };
        if rc != ffi::GHOSTTY_SUCCESS {
            // SAFETY: `encoder` was allocated above and has not been freed.
            unsafe { ffi::ghostty_key_encoder_free(encoder) };
            return Err(VtError::Ffi(rc));
        }

        Ok(Self { encoder, event })
    }

    /// Synchronizes cursor-key, keypad, alt-esc, modifyOtherKeys, and Kitty flags from `terminal`.
    pub fn sync_from_terminal(&mut self, terminal: &Terminal) {
        let guard = terminal.inner.lock();
        // SAFETY: Both handles are valid and `terminal` is locked for the duration of this call.
        unsafe {
            ffi::ghostty_key_encoder_setopt_from_terminal(self.encoder, guard.raw);
        }
    }

    pub fn set_kitty_flags(&mut self, flags: KittyKeyboardFlags) {
        let raw = flags.0;
        // SAFETY: `raw` is a `GhosttyKittyKeyFlags` (`u8`) matching `OPT_KITTY_FLAGS`.
        unsafe {
            ffi::ghostty_key_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_KEY_ENCODER_OPT_KITTY_FLAGS,
                (&raw const raw).cast::<c_void>(),
            );
        }
    }

    pub fn set_cursor_key_application(&mut self, enabled: bool) {
        // SAFETY: `enabled` is a `bool` matching `OPT_CURSOR_KEY_APPLICATION`.
        unsafe {
            ffi::ghostty_key_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_KEY_ENCODER_OPT_CURSOR_KEY_APPLICATION,
                (&raw const enabled).cast::<c_void>(),
            );
        }
    }

    pub fn set_keypad_key_application(&mut self, enabled: bool) {
        // SAFETY: `enabled` is a `bool` matching `OPT_KEYPAD_KEY_APPLICATION`.
        unsafe {
            ffi::ghostty_key_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_KEY_ENCODER_OPT_KEYPAD_KEY_APPLICATION,
                (&raw const enabled).cast::<c_void>(),
            );
        }
    }

    pub fn set_alt_esc_prefix(&mut self, enabled: bool) {
        // SAFETY: `enabled` is a `bool` matching `OPT_ALT_ESC_PREFIX`.
        unsafe {
            ffi::ghostty_key_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_KEY_ENCODER_OPT_ALT_ESC_PREFIX,
                (&raw const enabled).cast::<c_void>(),
            );
        }
    }

    pub fn set_modify_other_keys(&mut self, enabled: bool) {
        // SAFETY: `enabled` is a `bool` matching `OPT_MODIFY_OTHER_KEYS_STATE_2`.
        unsafe {
            ffi::ghostty_key_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_KEY_ENCODER_OPT_MODIFY_OTHER_KEYS_STATE_2,
                (&raw const enabled).cast::<c_void>(),
            );
        }
    }

    pub fn set_backarrow_key_mode(&mut self, enabled: bool) {
        // SAFETY: `enabled` is a `bool` matching `OPT_BACKARROW_KEY_MODE`.
        unsafe {
            ffi::ghostty_key_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_KEY_ENCODER_OPT_BACKARROW_KEY_MODE,
                (&raw const enabled).cast::<c_void>(),
            );
        }
    }

    pub fn set_option_as_alt(&mut self, mode: OptionAsAlt) {
        let raw = mode as ffi::GhosttyOptionAsAlt;
        // SAFETY: `raw` is a `GhosttyOptionAsAlt` matching `OPT_MACOS_OPTION_AS_ALT`.
        unsafe {
            ffi::ghostty_key_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT,
                (&raw const raw).cast::<c_void>(),
            );
        }
    }

    /// Encodes `input` into VT escape bytes, appending them to `out`.
    pub fn encode(&mut self, input: &KeyEventInput<'_>, out: &mut Vec<u8>) -> Result<(), VtError> {
        let filtered_utf8 = input
            .utf8
            .filter(|s| !s.is_empty() && !s.bytes().all(|b| b < 0x20 || b == 0x7F));

        // SAFETY: `self.event` and `self.encoder` are valid handles; `utf8` slice lives across `encode`.
        unsafe {
            ffi::ghostty_key_event_set_action(self.event, input.action as ffi::GhosttyKeyAction);
            ffi::ghostty_key_event_set_key(self.event, input.key as ffi::GhosttyKey);
            ffi::ghostty_key_event_set_mods(self.event, input.mods.0);
            ffi::ghostty_key_event_set_consumed_mods(self.event, input.consumed_mods.0);
            ffi::ghostty_key_event_set_composing(self.event, input.composing);
            ffi::ghostty_key_event_set_unshifted_codepoint(
                self.event,
                input.unshifted_codepoint.map_or(0, |c| c as u32),
            );
            match filtered_utf8 {
                Some(s) => ffi::ghostty_key_event_set_utf8(
                    self.event,
                    s.as_ptr().cast::<c_char>(),
                    s.len(),
                ),
                None => ffi::ghostty_key_event_set_utf8(self.event, ptr::null(), 0),
            }

            let mut stack_buf = [0u8; 128];
            let mut written: usize = 0;
            let rc = ffi::ghostty_key_encoder_encode(
                self.encoder,
                self.event,
                stack_buf.as_mut_ptr().cast::<c_char>(),
                stack_buf.len(),
                &raw mut written,
            );
            // Clear borrowed utf8 pointer before returning so event never holds a dangling ref.
            let res = match rc {
                ffi::GHOSTTY_SUCCESS => {
                    out.extend_from_slice(&stack_buf[..written]);
                    Ok(())
                }
                ffi::GHOSTTY_OUT_OF_SPACE => {
                    let start = out.len();
                    out.resize(start + written, 0);
                    let mut actual: usize = 0;
                    let rc2 = ffi::ghostty_key_encoder_encode(
                        self.encoder,
                        self.event,
                        out[start..].as_mut_ptr().cast::<c_char>(),
                        written,
                        &raw mut actual,
                    );
                    if rc2 != ffi::GHOSTTY_SUCCESS {
                        out.truncate(start);
                        Err(VtError::Ffi(rc2))
                    } else {
                        out.truncate(start + actual);
                        Ok(())
                    }
                }
                other => Err(VtError::Ffi(other)),
            };
            ffi::ghostty_key_event_set_utf8(self.event, ptr::null(), 0);
            res
        }
    }
}

impl Drop for KeyEncoder {
    fn drop(&mut self) {
        // SAFETY: Both handles were created by `*_new` and are freed exactly once.
        unsafe {
            ffi::ghostty_key_event_free(self.event);
            ffi::ghostty_key_encoder_free(self.encoder);
        }
    }
}

// ---------------------------------------------------------------------------
// Mouse encoder
// ---------------------------------------------------------------------------

/// Terminal mouse tracking mode (`GhosttyMouseTrackingMode`).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum MouseTrackingMode {
    #[default]
    None = ffi::GHOSTTY_MOUSE_TRACKING_NONE,
    X10 = ffi::GHOSTTY_MOUSE_TRACKING_X10,
    Normal = ffi::GHOSTTY_MOUSE_TRACKING_NORMAL,
    Button = ffi::GHOSTTY_MOUSE_TRACKING_BUTTON,
    Any = ffi::GHOSTTY_MOUSE_TRACKING_ANY,
}

/// Terminal mouse wire format (`GhosttyMouseFormat`).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum MouseFormat {
    #[default]
    X10 = ffi::GHOSTTY_MOUSE_FORMAT_X10,
    Utf8 = ffi::GHOSTTY_MOUSE_FORMAT_UTF8,
    Sgr = ffi::GHOSTTY_MOUSE_FORMAT_SGR,
    Urxvt = ffi::GHOSTTY_MOUSE_FORMAT_URXVT,
    SgrPixels = ffi::GHOSTTY_MOUSE_FORMAT_SGR_PIXELS,
}

/// Mouse action (`GhosttyMouseAction`).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(i32)]
pub enum MouseAction {
    Press = ffi::GHOSTTY_MOUSE_ACTION_PRESS,
    Release = ffi::GHOSTTY_MOUSE_ACTION_RELEASE,
    Motion = ffi::GHOSTTY_MOUSE_ACTION_MOTION,
}

/// Mouse button (`GhosttyMouseButton`). Wheel events use `WheelUp` (`Four`), `WheelDown`
/// (`Five`), `WheelLeft` (`Six`), and `WheelRight` (`Seven`) with [`MouseAction::Press`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(i32)]
pub enum MouseButton {
    Unknown = ffi::GHOSTTY_MOUSE_BUTTON_UNKNOWN,
    Left = ffi::GHOSTTY_MOUSE_BUTTON_LEFT,
    Right = ffi::GHOSTTY_MOUSE_BUTTON_RIGHT,
    Middle = ffi::GHOSTTY_MOUSE_BUTTON_MIDDLE,
    WheelUp = ffi::GHOSTTY_MOUSE_BUTTON_FOUR,
    WheelDown = ffi::GHOSTTY_MOUSE_BUTTON_FIVE,
    WheelLeft = ffi::GHOSTTY_MOUSE_BUTTON_SIX,
    WheelRight = ffi::GHOSTTY_MOUSE_BUTTON_SEVEN,
    Eight = ffi::GHOSTTY_MOUSE_BUTTON_EIGHT,
    Nine = ffi::GHOSTTY_MOUSE_BUTTON_NINE,
    Ten = ffi::GHOSTTY_MOUSE_BUTTON_TEN,
    Eleven = ffi::GHOSTTY_MOUSE_BUTTON_ELEVEN,
}

/// Surface/cell pixel geometry required by [`MouseEncoder`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MouseGeometry {
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub padding_top: u32,
    pub padding_bottom: u32,
    pub padding_right: u32,
    pub padding_left: u32,
}

/// Normalized pointer input event for [`MouseEncoder`].
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct MouseEventInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    pub mods: Modifiers,
    pub x: f32,
    pub y: f32,
}

/// Returns `true` if Shift-bypass should override app mouse reporting so the user can
/// perform native text selection / context menus even inside mouse-tracking TUIs.
#[inline]
pub const fn should_bypass_mouse_reporting(mods: Modifiers, mode: MouseTrackingMode) -> bool {
    !matches!(mode, MouseTrackingMode::None) && mods.contains(Modifiers::SHIFT)
}

/// Safe RAII wrapper around `GhosttyMouseEncoder` + reusable `GhosttyMouseEvent`.
pub struct MouseEncoder {
    encoder: ffi::GhosttyMouseEncoder,
    event: ffi::GhosttyMouseEvent,
}

// SAFETY: `MouseEncoder` exclusively owns both opaque C handles.
unsafe impl Send for MouseEncoder {}

impl MouseEncoder {
    pub fn new() -> Result<Self, VtError> {
        let mut encoder: ffi::GhosttyMouseEncoder = ptr::null_mut();
        // SAFETY: NULL allocator selects default allocator.
        let rc = unsafe { ffi::ghostty_mouse_encoder_new(ptr::null(), &raw mut encoder) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let mut event: ffi::GhosttyMouseEvent = ptr::null_mut();
        // SAFETY: NULL allocator selects default allocator.
        let rc = unsafe { ffi::ghostty_mouse_event_new(ptr::null(), &raw mut event) };
        if rc != ffi::GHOSTTY_SUCCESS {
            // SAFETY: `encoder` was allocated above.
            unsafe { ffi::ghostty_mouse_encoder_free(encoder) };
            return Err(VtError::Ffi(rc));
        }

        Ok(Self { encoder, event })
    }

    /// Syncs mouse tracking mode and output format from `terminal`.
    pub fn sync_from_terminal(&mut self, terminal: &Terminal) {
        let guard = terminal.inner.lock();
        // SAFETY: Both handles are valid while `terminal` is locked.
        unsafe {
            ffi::ghostty_mouse_encoder_setopt_from_terminal(self.encoder, guard.raw);
        }
    }

    pub fn set_tracking_mode(&mut self, mode: MouseTrackingMode) {
        let raw = mode as ffi::GhosttyMouseTrackingMode;
        // SAFETY: `raw` matches `GHOSTTY_MOUSE_ENCODER_OPT_EVENT`.
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_EVENT,
                (&raw const raw).cast::<c_void>(),
            );
        }
    }

    pub fn set_format(&mut self, format: MouseFormat) {
        let raw = format as ffi::GhosttyMouseFormat;
        // SAFETY: `raw` matches `GHOSTTY_MOUSE_ENCODER_OPT_FORMAT`.
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_FORMAT,
                (&raw const raw).cast::<c_void>(),
            );
        }
    }

    pub fn set_geometry(&mut self, geom: MouseGeometry) {
        let raw = ffi::GhosttyMouseEncoderSize {
            size: std::mem::size_of::<ffi::GhosttyMouseEncoderSize>(),
            screen_width: geom.screen_width,
            screen_height: geom.screen_height,
            cell_width: geom.cell_width.max(1),
            cell_height: geom.cell_height.max(1),
            padding_top: geom.padding_top,
            padding_bottom: geom.padding_bottom,
            padding_right: geom.padding_right,
            padding_left: geom.padding_left,
        };
        // SAFETY: `raw` is a properly sized `GhosttyMouseEncoderSize`.
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                (&raw const raw).cast::<c_void>(),
            );
        }
    }

    pub fn set_any_button_pressed(&mut self, pressed: bool) {
        // SAFETY: `pressed` is a `bool` matching `OPT_ANY_BUTTON_PRESSED`.
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                (&raw const pressed).cast::<c_void>(),
            );
        }
    }

    pub fn set_track_last_cell(&mut self, enabled: bool) {
        // SAFETY: `enabled` is a `bool` matching `OPT_TRACK_LAST_CELL`.
        unsafe {
            ffi::ghostty_mouse_encoder_setopt(
                self.encoder,
                ffi::GHOSTTY_MOUSE_ENCODER_OPT_TRACK_LAST_CELL,
                (&raw const enabled).cast::<c_void>(),
            );
        }
    }

    pub fn reset(&mut self) {
        // SAFETY: `self.encoder` is valid.
        unsafe { ffi::ghostty_mouse_encoder_reset(self.encoder) };
    }

    /// Encodes `input` into terminal mouse escape bytes, appending them to `out`.
    pub fn encode(&mut self, input: &MouseEventInput, out: &mut Vec<u8>) -> Result<(), VtError> {
        // SAFETY: `self.event` and `self.encoder` are valid exclusive handles.
        unsafe {
            ffi::ghostty_mouse_event_set_action(
                self.event,
                input.action as ffi::GhosttyMouseAction,
            );
            match input.button {
                Some(btn) => {
                    ffi::ghostty_mouse_event_set_button(self.event, btn as ffi::GhosttyMouseButton)
                }
                None => ffi::ghostty_mouse_event_clear_button(self.event),
            }
            ffi::ghostty_mouse_event_set_mods(self.event, input.mods.0);
            ffi::ghostty_mouse_event_set_position(
                self.event,
                ffi::GhosttyMousePosition {
                    x: input.x,
                    y: input.y,
                },
            );

            let mut stack_buf = [0u8; 64];
            let mut written: usize = 0;
            let rc = ffi::ghostty_mouse_encoder_encode(
                self.encoder,
                self.event,
                stack_buf.as_mut_ptr().cast::<c_char>(),
                stack_buf.len(),
                &raw mut written,
            );
            match rc {
                ffi::GHOSTTY_SUCCESS => {
                    out.extend_from_slice(&stack_buf[..written]);
                    Ok(())
                }
                ffi::GHOSTTY_OUT_OF_SPACE => {
                    let start = out.len();
                    out.resize(start + written, 0);
                    let mut actual: usize = 0;
                    let rc2 = ffi::ghostty_mouse_encoder_encode(
                        self.encoder,
                        self.event,
                        out[start..].as_mut_ptr().cast::<c_char>(),
                        written,
                        &raw mut actual,
                    );
                    if rc2 != ffi::GHOSTTY_SUCCESS {
                        out.truncate(start);
                        Err(VtError::Ffi(rc2))
                    } else {
                        out.truncate(start + actual);
                        Ok(())
                    }
                }
                other => Err(VtError::Ffi(other)),
            }
        }
    }

    /// Convenience wrapper that returns `Ok(false)` without encoding if Shift-bypass is active.
    pub fn encode_with_shift_bypass(
        &mut self,
        input: &MouseEventInput,
        mode: MouseTrackingMode,
        out: &mut Vec<u8>,
    ) -> Result<bool, VtError> {
        if should_bypass_mouse_reporting(input.mods, mode) {
            return Ok(false);
        }
        let before = out.len();
        self.encode(input, out)?;
        Ok(out.len() > before)
    }
}

impl Drop for MouseEncoder {
    fn drop(&mut self) {
        // SAFETY: Both handles were created in `new` and freed once here.
        unsafe {
            ffi::ghostty_mouse_event_free(self.event);
            ffi::ghostty_mouse_encoder_free(self.encoder);
        }
    }
}

// ---------------------------------------------------------------------------
// Paste, Focus (Mode 1004), and Color Scheme (Mode 2031) encoders
// ---------------------------------------------------------------------------

/// Checks conservatively whether `data` is safe to paste without user confirmation
/// (contains no `\n`, `\r`, or bracketed paste end `\x1b[201~`).
pub fn is_paste_safe(data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    // SAFETY: `data` is a valid non-null byte slice of length `data.len()`.
    unsafe { ffi::ghostty_paste_is_safe(data.as_ptr().cast::<c_char>(), data.len()) }
}

/// Encodes paste data for PTY writing, stripping unsafe control bytes and wrapping with
/// bracketed paste sequences (`\x1b[200~...\x1b[201~`) when `bracketed` is `true`.
pub fn encode_paste(data: &[u8], bracketed: bool) -> Result<Vec<u8>, VtError> {
    let mut mutable = data.to_vec();
    let data_ptr = if mutable.is_empty() {
        ptr::null_mut()
    } else {
        mutable.as_mut_ptr().cast::<c_char>()
    };

    let mut required: usize = 0;
    // SAFETY: Passing NULL `buf` queries exact required capacity in `required`.
    let rc = unsafe {
        ffi::ghostty_paste_encode(
            data_ptr,
            mutable.len(),
            bracketed,
            ptr::null_mut(),
            0,
            &raw mut required,
        )
    };
    if rc != ffi::GHOSTTY_SUCCESS && rc != ffi::GHOSTTY_OUT_OF_SPACE {
        return Err(VtError::Ffi(rc));
    }

    let mut out = vec![0u8; required];
    let mut written: usize = 0;
    // SAFETY: `mutable` and `out` have exact valid lengths.
    let rc2 = unsafe {
        ffi::ghostty_paste_encode(
            data_ptr,
            mutable.len(),
            bracketed,
            out.as_mut_ptr().cast::<c_char>(),
            out.len(),
            &raw mut written,
        )
    };
    if rc2 != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc2));
    }
    out.truncate(written);
    Ok(out)
}

/// Encodes a Mode 1004 focus in (`CSI I`) or focus out (`CSI O`) escape sequence.
pub fn encode_focus(gained: bool) -> Result<Vec<u8>, VtError> {
    let ev = if gained {
        ffi::GHOSTTY_FOCUS_GAINED
    } else {
        ffi::GHOSTTY_FOCUS_LOST
    };
    let mut buf = [0u8; 8];
    let mut written: usize = 0;
    // SAFETY: 8-byte buffer always fits `CSI I` / `CSI O` (3 bytes).
    let rc = unsafe {
        ffi::ghostty_focus_encode(
            ev,
            buf.as_mut_ptr().cast::<c_char>(),
            buf.len(),
            &raw mut written,
        )
    };
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }
    Ok(buf[..written].to_vec())
}

/// Encodes a Mode 2031 color scheme report (`CSI ? 997 ; 1 n` for dark, `2 n` for light).
pub fn encode_color_scheme_report(scheme: ColorScheme) -> Result<Vec<u8>, VtError> {
    let mut buf = [0u8; 16];
    let mut written: usize = 0;
    // SAFETY: 16-byte buffer fits `\x1b[?997;1n` (9 bytes).
    let rc = unsafe {
        ffi::ghostty_color_scheme_report_encode(
            scheme.to_ffi(),
            buf.as_mut_ptr().cast::<c_char>(),
            buf.len(),
            &raw mut written,
        )
    };
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }
    Ok(buf[..written].to_vec())
}
