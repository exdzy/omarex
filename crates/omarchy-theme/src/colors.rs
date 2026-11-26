//! Omarchy v4 Aether `colors.toml` parser, WCAG contrast utilities, derived Rex/`tr3x`
//! semantic UI color tokens, and Rex `com.superlogical.terminal.set_theme` payload builder.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ThemeError;

/// 8-bit sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `#RRGGBB` or `RRGGBB` (case-insensitive, surrounding whitespace ignored).
    pub fn from_hex(input: &str) -> Result<Self, ThemeError> {
        let s = input.trim().strip_prefix('#').unwrap_or(input.trim());
        if s.len() != 6 {
            return Err(ThemeError::Parse(format!("invalid hex color `{input}`")));
        }
        let r = u8::from_str_radix(&s[0..2], 16)
            .map_err(|_| ThemeError::Parse(format!("invalid hex red in `{input}`")))?;
        let g = u8::from_str_radix(&s[2..4], 16)
            .map_err(|_| ThemeError::Parse(format!("invalid hex green in `{input}`")))?;
        let b = u8::from_str_radix(&s[4..6], 16)
            .map_err(|_| ThemeError::Parse(format!("invalid hex blue in `{input}`")))?;
        Ok(Self { r, g, b })
    }

    /// Formats as lowercase `#rrggbb`.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Converts to opaque [`Rgba`].
    pub const fn opaque(self) -> Rgba {
        Rgba {
            r: self.r,
            g: self.g,
            b: self.b,
            a: 255,
        }
    }

    /// Converts to [`Rgba`] with an explicit byte alpha.
    pub const fn with_alpha(self, a: u8) -> Rgba {
        Rgba {
            r: self.r,
            g: self.g,
            b: self.b,
            a,
        }
    }

    /// Converts to [`Rgba`] with a normalized alpha in `0.0..=1.0`.
    pub fn with_alpha_f32(self, alpha: f32) -> Rgba {
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        self.with_alpha(a)
    }

    /// Linearly blends `self` toward `other` by `t` in `0.0..=1.0`.
    pub fn blend(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let inv = 1.0 - t;
        Self {
            r: (f32::from(self.r) * inv + f32::from(other.r) * t).round() as u8,
            g: (f32::from(self.g) * inv + f32::from(other.g) * t).round() as u8,
            b: (f32::from(self.b) * inv + f32::from(other.b) * t).round() as u8,
        }
    }

    /// Lightens toward white by `amount` in `0.0..=1.0`.
    pub fn lighten(self, amount: f32) -> Self {
        self.blend(Self::new(255, 255, 255), amount)
    }

    /// Darkens toward black by `amount` in `0.0..=1.0`.
    pub fn darken(self, amount: f32) -> Self {
        self.blend(Self::new(0, 0, 0), amount)
    }

    /// WCAG 2.1 relative luminance in `0.0..=1.0`.
    pub fn relative_luminance(self) -> f64 {
        fn linear(channel: u8) -> f64 {
            let v = f64::from(channel) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG 2.1 contrast ratio (`1.0..=21.0`) between two colors.
    pub fn wcag_contrast(self, other: Self) -> f64 {
        let la = self.relative_luminance();
        let lb = other.relative_luminance();
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Returns black or white, whichever yields higher WCAG contrast on `self`.
    pub fn readable_foreground(self) -> Self {
        let black = Self::new(0, 0, 0);
        let white = Self::new(255, 255, 255);
        if self.wcag_contrast(black) >= self.wcag_contrast(white) {
            black
        } else {
            white
        }
    }

    /// Formats an OSC 11 background color reply (`ESC ] 11 ; rgb:RRRR/GGGG/BBBB ESC \`).
    pub fn to_osc11_reply(self) -> String {
        format!(
            "\x1b]11;rgb:{0:02x}{0:02x}/{1:02x}{1:02x}/{2:02x}{2:02x}\x1b\\",
            self.r, self.g, self.b
        )
    }
}

/// 8-bit sRGBA color for UI chrome and GPU shaders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(self) -> Rgb {
        Rgb {
            r: self.r,
            g: self.g,
            b: self.b,
        }
    }

    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    pub fn with_alpha_f32(self, alpha: f32) -> Self {
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        Self { a, ..self }
    }

    /// Normalized `[r, g, b, a]` in `0.0..=1.0` for `wgpu` uniforms/instances.
    pub fn to_f32_array(self) -> [f32; 4] {
        [
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            f32::from(self.a) / 255.0,
        ]
    }
}

/// Light or dark color scheme (drives alpha polarity, Rex `set_theme.scheme`, and Mode 2031).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppearanceMode {
    Dark,
    Light,
}

impl AppearanceMode {
    /// Wire value for Rex `com.superlogical.terminal.set_theme` (`"dark"` or `"light"`).
    pub const fn as_rex_scheme(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// DEC Private Mode 2031 (`CSI ? 997 ; 1 n` = dark, `CSI ? 997 ; 2 n` = light).
    pub const fn mode_2031_reply(self) -> &'static [u8] {
        match self {
            Self::Dark => b"\x1b[?997;1n",
            Self::Light => b"\x1b[?997;2n",
        }
    }

    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}

/// Window surface opacity mode matching `docs/gui-spec.md §6.2`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum MaterialStyle {
    /// Fully opaque surfaces (default on Omarchy for crisp contrast and VRR direct scanout).
    #[default]
    Classic,
    /// ARGB translucent surfaces for Hyprland Kawase blur (`o.transparent_window("omarex")`).
    Translucent {
        /// Opacity in `0.5..=1.0` (recommended `0.88..=0.94`, default `0.90`).
        alpha: f32,
    },
}

impl MaterialStyle {
    pub const fn translucent_default() -> Self {
        Self::Translucent { alpha: 0.90 }
    }
}

/// Derived semantic UI tokens matching `docs/gui-spec.md §6.1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiTokens {
    pub surface_frame: Rgba,
    pub surface_sidebar: Rgba,
    pub surface_pane: Rgba,
    pub surface_pane_header: Rgba,
    pub surface_modal: Rgba,
    pub surface_tab_active: Rgba,
    pub surface_row_active: Rgba,
    pub surface_row_hover: Rgba,
    pub surface_row_selected: Rgba,
    pub surface_icon_inactive: Rgba,
    pub surface_scrim: Rgba,

    pub fg_primary: Rgba,
    pub fg_secondary: Rgba,
    pub fg_muted: Rgba,
    pub accent_primary: Rgba,
    pub accent_foreground: Rgba,

    pub border_hairline: Rgba,
    pub border_subtle: Rgba,
    pub border_pane: Rgba,
    pub border_modal: Rgba,
    pub border_active: Rgba,
    pub border_inactive: Rgba,

    pub status_blocked: Rgba,
    pub status_working: Rgba,
    pub status_error: Rgba,
    pub status_done: Rgba,
    pub status_idle: Rgba,

    pub scrollbar_thumb: Rgba,
    pub scrollbar_thumb_hover: Rgba,
    pub shadow_color: Rgba,
}

impl UiTokens {
    /// Returns a copy of these tokens adjusted for [`MaterialStyle`].
    pub fn with_material(&self, material: MaterialStyle) -> Self {
        let mut out = self.clone();
        if let MaterialStyle::Translucent { alpha } = material {
            let clamped = alpha.clamp(0.5, 1.0);
            let frame_alpha = (clamped - 0.04).clamp(0.45, 1.0);
            out.surface_frame = out.surface_frame.with_alpha_f32(frame_alpha);
            out.surface_sidebar = out.surface_sidebar.with_alpha_f32(frame_alpha);
            out.surface_pane = out.surface_pane.with_alpha_f32(clamped);
            out.surface_pane_header = out.surface_pane_header.with_alpha_f32(clamped);
        }
        out
    }
}

/// Terminal color palette passed to `libghostty-vt` and Rex `set_theme`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalColors {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub selection_bg: Rgb,
    pub selection_fg: Rgb,
    pub ansi: [Rgb; 16],
}

/// Exact payload for Rex `com.superlogical.terminal.set_theme` (`args` object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RexSetThemeArgs {
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    pub palette: Vec<String>,
    pub scheme: String,
}

/// Complete resolved theme palette combining Omarchy source colors, derived UI tokens,
/// and terminal palette.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemePalette {
    pub mode: AppearanceMode,
    pub source_name: Option<String>,
    pub ui: UiTokens,
    pub terminal: TerminalColors,
}

impl ThemePalette {
    /// Omarchy dark fallback palette (matches Omarchy v4 Aether dark theme defaults).
    pub fn default_dark() -> Self {
        let raw = RawOmarchyColors::default_dark();
        Self::from_raw(raw, Some("omarchy-default-dark".to_owned()))
    }

    /// Light fallback palette (matches Rex *Alpine Milk* / Omarchy light theme parity).
    pub fn default_light() -> Self {
        let raw = RawOmarchyColors::default_light();
        Self::from_raw(raw, Some("omarchy-default-light".to_owned()))
    }

    /// Parses an Omarchy v4 `colors.toml` string (also tolerates legacy `color0..color15` keys).
    pub fn from_toml_str(content: &str) -> Result<Self, ThemeError> {
        Self::from_toml_str_with_name(content, None)
    }

    /// Parses an Omarchy v4 `colors.toml` string with an optional theme display name.
    pub fn from_toml_str_with_name(
        content: &str,
        source_name: Option<String>,
    ) -> Result<Self, ThemeError> {
        let table: toml::Table =
            toml::from_str(content).map_err(|e| ThemeError::Parse(e.to_string()))?;

        let mut strings = HashMap::with_capacity(table.len());
        for (k, v) in table {
            if let Some(s) = v.as_str() {
                strings.insert(k, s.to_owned());
            }
        }

        let explicit_mode = strings.get("mode").and_then(|m| {
            let lower = m.trim().to_ascii_lowercase();
            match lower.as_str() {
                "dark" => Some(AppearanceMode::Dark),
                "light" => Some(AppearanceMode::Light),
                _ => None,
            }
        });

        let parsed_bg = parse_opt(&strings, "background")?;
        let mode = explicit_mode.unwrap_or_else(|| match parsed_bg {
            Some(bg) if bg.relative_luminance() > 0.45 => AppearanceMode::Light,
            _ => AppearanceMode::Dark,
        });

        let mut raw = match mode {
            AppearanceMode::Dark => RawOmarchyColors::default_dark(),
            AppearanceMode::Light => RawOmarchyColors::default_light(),
        };
        raw.mode = mode;

        if let Some(c) = parsed_bg {
            raw.background = c;
            raw.dark_background = match mode {
                AppearanceMode::Dark => c.darken(0.22),
                AppearanceMode::Light => c.darken(0.04),
            };
            raw.darker_background = match mode {
                AppearanceMode::Dark => c.darken(0.45),
                AppearanceMode::Light => c.darken(0.08),
            };
            raw.lighter_background = match mode {
                AppearanceMode::Dark => c.lighten(0.11),
                AppearanceMode::Light => c.darken(0.06),
            };
        }
        if let Some(c) = parse_opt(&strings, "foreground")? {
            raw.foreground = c;
            raw.dark_foreground = match mode {
                AppearanceMode::Dark => c.blend(raw.background, 0.26),
                AppearanceMode::Light => c.blend(raw.background, 0.28),
            };
            raw.light_foreground = c;
            raw.bright_foreground = c;
        }
        if let Some(c) = parse_opt(&strings, "accent")? {
            raw.accent = c;
        }
        if let Some(c) =
            parse_opt(&strings, "selection")?.or(parse_opt(&strings, "selection_background")?)
        {
            raw.selection = c;
        }
        if let Some(c) = parse_opt(&strings, "selection_foreground")? {
            raw.selection_foreground = Some(c);
        }
        if let Some(c) = parse_opt(&strings, "muted")? {
            raw.muted = c;
        }
        if let Some(c) = parse_opt(&strings, "dark_background")? {
            raw.dark_background = c;
        }
        if let Some(c) = parse_opt(&strings, "darker_background")? {
            raw.darker_background = c;
        }
        if let Some(c) = parse_opt(&strings, "lighter_background")? {
            raw.lighter_background = c;
        }
        if let Some(c) = parse_opt(&strings, "dark_foreground")? {
            raw.dark_foreground = c;
        }
        if let Some(c) = parse_opt(&strings, "light_foreground")? {
            raw.light_foreground = c;
        }
        if let Some(c) = parse_opt(&strings, "bright_foreground")? {
            raw.bright_foreground = c;
        }
        if let Some(c) = parse_opt(&strings, "cursor")? {
            raw.cursor = Some(c);
        }

        // Named ANSI colors from Omarchy v4 Aether format.
        if let Some(c) = parse_opt(&strings, "red")? {
            raw.red = c;
        }
        if let Some(c) = parse_opt(&strings, "green")? {
            raw.green = c;
        }
        if let Some(c) = parse_opt(&strings, "yellow")? {
            raw.yellow = c;
        }
        if let Some(c) = parse_opt(&strings, "orange")? {
            raw.orange = Some(c);
        }
        if let Some(c) = parse_opt(&strings, "blue")? {
            raw.blue = c;
        }
        if let Some(c) = parse_opt(&strings, "magenta")? {
            raw.magenta = c;
        }
        if let Some(c) = parse_opt(&strings, "cyan")? {
            raw.cyan = c;
        }
        if let Some(c) = parse_opt(&strings, "bright_red")? {
            raw.bright_red = c;
        }
        if let Some(c) = parse_opt(&strings, "bright_green")? {
            raw.bright_green = c;
        }
        if let Some(c) = parse_opt(&strings, "bright_yellow")? {
            raw.bright_yellow = c;
        }
        if let Some(c) = parse_opt(&strings, "bright_blue")? {
            raw.bright_blue = c;
        }
        if let Some(c) = parse_opt(&strings, "bright_magenta")? {
            raw.bright_magenta = c;
        }
        if let Some(c) = parse_opt(&strings, "bright_cyan")? {
            raw.bright_cyan = c;
        }

        // Legacy color0..color15 overrides (when present in older Omarchy themes).
        for (idx, slot) in raw.ansi_overrides.iter_mut().enumerate() {
            let key = format!("color{idx}");
            if let Some(c) = parse_opt(&strings, &key)? {
                *slot = Some(c);
            }
        }

        Ok(Self::from_raw(raw, source_name))
    }

    /// Loads `colors.toml` from an explicit file path (and reads sibling `../theme.name` if present).
    pub fn load_from_path(colors_toml: &Path) -> Result<Self, ThemeError> {
        let content = std::fs::read_to_string(colors_toml)?;
        let source_name = colors_toml
            .parent()
            .and_then(|theme_dir| theme_dir.parent())
            .map(|current_dir| current_dir.join("theme.name"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        Self::from_toml_str_with_name(&content, source_name)
    }

    /// Returns the default Omarchy state directory (`~/.local/state/omarchy/current`).
    pub fn default_omarchy_dir() -> Option<std::path::PathBuf> {
        dirs::state_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
            .map(|base| base.join("omarchy/current"))
    }

    /// Loads the active Omarchy theme or falls back cleanly to [`Self::default_dark`].
    pub fn load_omarchy_or_default() -> Self {
        let Some(dir) = Self::default_omarchy_dir() else {
            return Self::default_dark();
        };
        Self::load_from_omarchy_dir(&dir).unwrap_or_else(|_| Self::default_dark())
    }

    /// Loads `<omarchy_current_dir>/theme/colors.toml`.
    pub fn load_from_omarchy_dir(omarchy_current_dir: &Path) -> Result<Self, ThemeError> {
        Self::load_from_path(&omarchy_current_dir.join("theme/colors.toml"))
    }

    /// Builds the typed arguments for Rex `com.superlogical.terminal.set_theme`.
    pub fn to_rex_set_theme_args(&self) -> RexSetThemeArgs {
        let palette = self
            .terminal
            .ansi
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{i}={}", c.to_hex()))
            .collect();
        RexSetThemeArgs {
            foreground: self.terminal.foreground.to_hex(),
            background: self.terminal.background.to_hex(),
            cursor: self.terminal.cursor.to_hex(),
            palette,
            scheme: self.mode.as_rex_scheme().to_owned(),
        }
    }

    /// Serializes the `args` JSON object for Rex `com.superlogical.terminal.set_theme`.
    pub fn to_rex_set_theme_json(&self) -> serde_json::Value {
        serde_json::json!({
            "foreground": self.terminal.foreground.to_hex(),
            "background": self.terminal.background.to_hex(),
            "cursor": self.terminal.cursor.to_hex(),
            "palette": self
                .terminal
                .ansi
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{i}={}", c.to_hex()))
                .collect::<Vec<_>>(),
            "scheme": self.mode.as_rex_scheme(),
        })
    }

    /// Returns the Mode 2031 response bytes for this theme's appearance mode.
    pub const fn mode_2031_reply(&self) -> &'static [u8] {
        self.mode.mode_2031_reply()
    }

    /// Returns the OSC 11 background color reply for this theme.
    pub fn osc11_reply(&self) -> String {
        self.terminal.background.to_osc11_reply()
    }

    fn from_raw(raw: RawOmarchyColors, source_name: Option<String>) -> Self {
        let is_dark = raw.mode.is_dark();

        // Build ANSI 0..15 matching Omarchy's ghostty.conf mapping 1:1.
        let mut ansi = [
            raw.background,
            raw.red,
            raw.green,
            raw.yellow,
            raw.blue,
            raw.magenta,
            raw.cyan,
            raw.foreground,
            raw.muted,
            raw.bright_red,
            raw.bright_green,
            raw.bright_yellow,
            raw.bright_blue,
            raw.bright_magenta,
            raw.bright_cyan,
            raw.bright_foreground,
        ];
        for (idx, ov) in raw.ansi_overrides.iter().enumerate() {
            if let Some(c) = ov {
                ansi[idx] = *c;
            }
        }

        // Ensure readable selection colors (WCAG contrast >= 3.0 between selection_bg and selection_fg).
        let selection_bg = raw.selection;
        let selection_fg = match raw.selection_foreground {
            Some(fg) if fg.wcag_contrast(selection_bg) >= 3.0 => fg,
            _ if raw.foreground.wcag_contrast(selection_bg) >= 3.0 => raw.foreground,
            _ => selection_bg.readable_foreground(),
        };

        let cursor = raw.cursor.unwrap_or(raw.bright_foreground);

        let terminal = TerminalColors {
            background: raw.background,
            foreground: raw.foreground,
            cursor,
            selection_bg,
            selection_fg,
            ansi,
        };

        // Semantic UI tokens directly mirroring docs/gui-spec.md §6.1.
        let surface_frame = raw.dark_background.opaque();
        let surface_sidebar = raw.dark_background.opaque();
        let surface_pane = raw.background.opaque();
        let surface_pane_header = raw.background.blend(raw.dark_background, 0.25).opaque();
        let surface_modal = if is_dark {
            raw.background.lighten(0.04).opaque()
        } else {
            raw.light_foreground.opaque()
        };

        let surface_tab_active = if is_dark {
            raw.background.blend(raw.lighter_background, 0.72).opaque()
        } else {
            raw.background.opaque()
        };
        let surface_row_active = raw.dark_background.blend(raw.foreground, 0.08).opaque();
        let surface_row_hover = if is_dark {
            raw.foreground.with_alpha_f32(0.05)
        } else {
            Rgb::new(0, 0, 0).with_alpha_f32(0.05)
        };
        let surface_row_selected = if raw.selection.wcag_contrast(raw.foreground) >= 3.0 {
            raw.selection.opaque()
        } else {
            raw.background.blend(raw.accent, 0.16).opaque()
        };
        let surface_icon_inactive = if is_dark {
            raw.lighter_background.opaque()
        } else {
            raw.dark_background.darken(0.06).opaque()
        };
        let surface_scrim = if is_dark {
            Rgba::new(0, 0, 0, 89) // ~0.35 alpha
        } else {
            Rgba::new(0, 0, 0, 46) // ~0.18 alpha
        };

        let accent_foreground = raw.accent.readable_foreground().opaque();

        let (border_hairline, border_subtle, border_modal, shadow_color) = if is_dark {
            (
                raw.foreground.with_alpha_f32(0.08),
                raw.foreground.with_alpha_f32(0.14),
                raw.foreground.with_alpha_f32(0.14),
                Rgba::new(0, 0, 0, 140),
            )
        } else {
            (
                Rgb::new(0, 0, 0).with_alpha_f32(0.10),
                Rgb::new(0, 0, 0).with_alpha_f32(0.16),
                Rgb::new(0, 0, 0).with_alpha_f32(0.16),
                Rgba::new(0, 0, 0, 36),
            )
        };

        let status_blocked = raw.orange.unwrap_or(raw.yellow).opaque();
        let status_working = raw.bright_blue.opaque();
        let status_error = raw.red.opaque();
        let status_done = raw.green.opaque();
        let status_idle = raw.muted.opaque();

        let ui = UiTokens {
            surface_frame,
            surface_sidebar,
            surface_pane,
            surface_pane_header,
            surface_modal,
            surface_tab_active,
            surface_row_active,
            surface_row_hover,
            surface_row_selected,
            surface_icon_inactive,
            surface_scrim,
            fg_primary: raw.foreground.opaque(),
            fg_secondary: raw.dark_foreground.opaque(),
            fg_muted: raw.muted.opaque(),
            accent_primary: raw.accent.opaque(),
            accent_foreground,
            border_hairline,
            border_subtle,
            border_pane: border_hairline,
            border_modal,
            border_active: raw.accent.with_alpha_f32(0.75),
            border_inactive: border_hairline,
            status_blocked,
            status_working,
            status_error,
            status_done,
            status_idle,
            scrollbar_thumb: raw.foreground.with_alpha_f32(0.25),
            scrollbar_thumb_hover: raw.foreground.with_alpha_f32(0.48),
            shadow_color,
        };

        Self {
            mode: raw.mode,
            source_name,
            ui,
            terminal,
        }
    }
}

#[derive(Debug, Clone)]
struct RawOmarchyColors {
    mode: AppearanceMode,
    accent: Rgb,
    selection: Rgb,
    selection_foreground: Option<Rgb>,
    muted: Rgb,
    background: Rgb,
    dark_background: Rgb,
    darker_background: Rgb,
    lighter_background: Rgb,
    foreground: Rgb,
    dark_foreground: Rgb,
    light_foreground: Rgb,
    bright_foreground: Rgb,
    cursor: Option<Rgb>,
    red: Rgb,
    yellow: Rgb,
    orange: Option<Rgb>,
    green: Rgb,
    cyan: Rgb,
    blue: Rgb,
    magenta: Rgb,
    bright_red: Rgb,
    bright_yellow: Rgb,
    bright_green: Rgb,
    bright_cyan: Rgb,
    bright_blue: Rgb,
    bright_magenta: Rgb,
    ansi_overrides: [Option<Rgb>; 16],
}

impl RawOmarchyColors {
    fn default_dark() -> Self {
        Self {
            mode: AppearanceMode::Dark,
            accent: Rgb::new(0xe2, 0x82, 0x03),
            selection: Rgb::new(0x2a, 0x2a, 0x2a),
            selection_foreground: Some(Rgb::new(0xff, 0xff, 0xff)),
            muted: Rgb::new(0x9e, 0x9e, 0x9e),
            background: Rgb::new(0x12, 0x12, 0x12),
            dark_background: Rgb::new(0x0e, 0x0e, 0x0e),
            darker_background: Rgb::new(0x09, 0x09, 0x09),
            lighter_background: Rgb::new(0x2a, 0x2a, 0x2a),
            foreground: Rgb::new(0xff, 0xff, 0xff),
            dark_foreground: Rgb::new(0xbf, 0xbf, 0xbf),
            light_foreground: Rgb::new(0xff, 0xff, 0xff),
            bright_foreground: Rgb::new(0xff, 0xff, 0xff),
            cursor: None,
            red: Rgb::new(0xef, 0x38, 0x27),
            yellow: Rgb::new(0xbc, 0xa6, 0x74),
            orange: Some(Rgb::new(0xf1, 0x56, 0x47)),
            green: Rgb::new(0xb7, 0x9d, 0x62),
            cyan: Rgb::new(0xb3, 0x94, 0x56),
            blue: Rgb::new(0xe2, 0x82, 0x03),
            magenta: Rgb::new(0xdb, 0x86, 0x0b),
            bright_red: Rgb::new(0xff, 0x59, 0x40),
            bright_yellow: Rgb::new(0xd0, 0xba, 0x89),
            bright_green: Rgb::new(0xcb, 0xb1, 0x76),
            bright_cyan: Rgb::new(0xc8, 0xa9, 0x6a),
            bright_blue: Rgb::new(0xff, 0x98, 0x0f),
            bright_magenta: Rgb::new(0xfd, 0x9d, 0x12),
            ansi_overrides: [None; 16],
        }
    }

    fn default_light() -> Self {
        Self {
            mode: AppearanceMode::Light,
            accent: Rgb::new(0x1e, 0x66, 0xf5),
            selection: Rgb::new(0xdc, 0xe0, 0xe8),
            selection_foreground: Some(Rgb::new(0x1c, 0x1f, 0x26)),
            muted: Rgb::new(0x6c, 0x6f, 0x85),
            background: Rgb::new(0xf8, 0xf9, 0xfb),
            dark_background: Rgb::new(0xee, 0xf0, 0xf4),
            darker_background: Rgb::new(0xe4, 0xe7, 0xee),
            lighter_background: Rgb::new(0xff, 0xff, 0xff),
            foreground: Rgb::new(0x1c, 0x1f, 0x26),
            dark_foreground: Rgb::new(0x4c, 0x4f, 0x69),
            light_foreground: Rgb::new(0xff, 0xff, 0xff),
            bright_foreground: Rgb::new(0x11, 0x13, 0x18),
            cursor: None,
            red: Rgb::new(0xd2, 0x0f, 0x39),
            yellow: Rgb::new(0xdf, 0x8e, 0x1d),
            orange: Some(Rgb::new(0xfe, 0x64, 0x0b)),
            green: Rgb::new(0x40, 0xa0, 0x2b),
            cyan: Rgb::new(0x17, 0x92, 0x99),
            blue: Rgb::new(0x1e, 0x66, 0xf5),
            magenta: Rgb::new(0x88, 0x39, 0xef),
            bright_red: Rgb::new(0xe6, 0x45, 0x53),
            bright_yellow: Rgb::new(0xe4, 0x93, 0x20),
            bright_green: Rgb::new(0x40, 0xa0, 0x2b),
            bright_cyan: Rgb::new(0x04, 0xa5, 0xe5),
            bright_blue: Rgb::new(0x20, 0x9f, 0xb5),
            bright_magenta: Rgb::new(0xea, 0x76, 0xcb),
            ansi_overrides: [None; 16],
        }
    }
}

fn parse_opt(map: &HashMap<String, String>, key: &str) -> Result<Option<Rgb>, ThemeError> {
    match map.get(key) {
        Some(raw) => Rgb::from_hex(raw).map(Some),
        None => Ok(None),
    }
}
