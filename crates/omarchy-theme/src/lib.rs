//! Live Omarchy theme integration, zero-polling Hyprland IPC rounding watcher,
//! and native D-Bus desktop notifications for `omarex`.

pub mod colors;
pub mod hyprland;
pub mod notify;
pub mod watcher;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::sync::mpsc;

pub use colors::{
    AppearanceMode, MaterialStyle, RexSetThemeArgs, Rgb, Rgba, TerminalColors, ThemePalette,
    UiTokens,
};
pub use hyprland::{
    HyprlandWatcherHandle, hyprland_socket_dir, parse_rounding_json,
    query_current_rounding_or_zero, query_rounding, query_rounding_sync,
    spawn_hyprland_rounding_watcher, spawn_hyprland_rounding_watcher_in,
};
pub use notify::{
    DEFAULT_RATE_LIMIT, DesktopNotifier, MuteFilter, NotificationRequest, NotificationTarget,
    NotificationUrgency, sanitize_notification_text,
};
pub use watcher::{DEFAULT_DEBOUNCE, ThemeWatcherHandle, WatcherConfig, spawn_theme_watcher};

/// Errors produced by `omarchy-theme`.
#[derive(Debug, Error)]
pub enum ThemeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("theme parse error: {0}")]
    Parse(String),
    #[error("filesystem watcher error: {0}")]
    Watch(String),
    #[error("Hyprland IPC error: {0}")]
    Hyprland(String),
    #[error("D-Bus notification error: {0}")]
    Dbus(String),
}

/// Live events pushed from zero-polling Omarchy and Hyprland watchers.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "ThemeEvent is low-frequency and keeps the public ThemeEvent::PaletteChanged(ThemePalette) signature"
)]
pub enum ThemeEvent {
    /// Emitted when `~/.local/state/omarchy/current/theme/colors.toml` changes.
    PaletteChanged(ThemePalette),
    /// Emitted when Hyprland's `decoration:rounding` changes after `configreloaded`.
    RoundingChanged(f32),
}

/// Combined live theme state consumed by `gpu-renderer`, `ui-chrome`, and `omarex`.
#[derive(Debug, Clone)]
pub struct Theme {
    pub is_dark: bool,
    pub background: Rgb,
    pub foreground: Rgb,
    pub accent: Rgb,
    pub corner_radius: u32,
    pub rounding: f32,
    pub material: MaterialStyle,
    pub palette: ThemePalette,
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_parts(ThemePalette::default_dark(), 0.0, MaterialStyle::Classic)
    }
}

impl Theme {
    /// Builds a [`Theme`] from a parsed [`ThemePalette`], Hyprland rounding, and material style.
    pub fn from_parts(palette: ThemePalette, rounding: f32, material: MaterialStyle) -> Self {
        let clamped_rounding = rounding.max(0.0);
        Self {
            is_dark: palette.mode.is_dark(),
            background: palette.terminal.background,
            foreground: palette.terminal.foreground,
            accent: palette.ui.accent_primary.rgb(),
            corner_radius: clamped_rounding.round() as u32,
            rounding: clamped_rounding,
            material,
            palette,
        }
    }

    /// Loads the current Omarchy palette and Hyprland `decoration:rounding` with clean fallbacks.
    pub fn load_live() -> Self {
        let palette = ThemePalette::load_omarchy_or_default();
        let rounding = query_current_rounding_or_zero();
        Self::from_parts(palette, rounding, MaterialStyle::Classic)
    }

    /// Returns derived UI tokens adjusted for the current [`MaterialStyle`].
    pub fn effective_ui(&self) -> UiTokens {
        self.palette.ui.with_material(self.material)
    }

    /// Applies an incoming [`ThemeEvent`] in place, returning `true` if state changed.
    pub fn apply_event(&mut self, event: ThemeEvent) -> bool {
        match event {
            ThemeEvent::PaletteChanged(palette) => {
                if self.palette == palette {
                    return false;
                }
                self.is_dark = palette.mode.is_dark();
                self.background = palette.terminal.background;
                self.foreground = palette.terminal.foreground;
                self.accent = palette.ui.accent_primary.rgb();
                self.palette = palette;
                true
            }
            ThemeEvent::RoundingChanged(rounding) => {
                let next = rounding.max(0.0);
                if (self.rounding - next).abs() <= f32::EPSILON {
                    return false;
                }
                self.rounding = next;
                self.corner_radius = next.round() as u32;
                true
            }
        }
    }
}

/// Reserved built-in theme name that tracks live `~/.local/state/omarchy/current/theme/`.
pub const OMARCHY_LIVE_THEME_NAME: &str = "omarchy-live";
/// Built-in dark fallback theme name.
pub const OMARCHY_DEFAULT_DARK_NAME: &str = "omarchy-default-dark";
/// Built-in light fallback theme name.
pub const OMARCHY_DEFAULT_LIGHT_NAME: &str = "omarchy-default-light";

/// Metadata for a discovered Omarchy or built-in theme entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeDescriptor {
    pub name: String,
    pub colors_path: Option<PathBuf>,
    pub mode: AppearanceMode,
    pub is_user_override: bool,
    pub is_builtin: bool,
}

/// Lists all available themes across `~/.config/omarchy/themes/`, `/usr/share/omarchy/themes/`,
/// live Omarchy state, and built-in presets. Directories lacking `colors.toml` are skipped.
pub fn list_available_themes() -> Vec<ThemeDescriptor> {
    let user_dir = ThemePalette::default_user_themes_dir();
    let system_dir = ThemePalette::default_system_themes_dir();
    let current_dir = ThemePalette::default_omarchy_dir();
    discover_themes_in(
        user_dir.as_deref(),
        Some(system_dir.as_path()),
        current_dir.as_deref(),
    )
}

/// Alias matching design docs for [`list_available_themes`].
pub fn discover_themes() -> Vec<ThemeDescriptor> {
    list_available_themes()
}

/// Discovers themes in explicit user, system, and current-state directories (unit-test friendly).
pub fn discover_themes_in(
    user_dir: Option<&Path>,
    system_dir: Option<&Path>,
    current_dir: Option<&Path>,
) -> Vec<ThemeDescriptor> {
    let live_palette = current_dir
        .and_then(|d| ThemePalette::load_from_omarchy_dir(d).ok())
        .unwrap_or_else(ThemePalette::default_dark);

    let mut out = vec![ThemeDescriptor {
        name: OMARCHY_LIVE_THEME_NAME.to_owned(),
        colors_path: current_dir.map(|d| d.join("theme/colors.toml")),
        mode: live_palette.mode,
        is_user_override: false,
        is_builtin: true,
    }];

    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(OMARCHY_LIVE_THEME_NAME.to_owned());

    let mut discovered = Vec::new();
    for (dir_opt, is_user) in [(user_dir, true), (system_dir, false)] {
        let Some(dir) = dir_opt else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if !entry_path.is_dir() {
                continue;
            }
            let Some(name) = entry_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            if seen.contains(&name) {
                continue;
            }
            let colors_path = entry_path.join("colors.toml");
            if !colors_path.is_file() {
                continue;
            }
            let Ok(pal) = ThemePalette::load_from_path(&colors_path) else {
                continue;
            };
            seen.insert(name.clone());
            discovered.push(ThemeDescriptor {
                name,
                colors_path: Some(colors_path),
                mode: pal.mode,
                is_user_override: is_user,
                is_builtin: false,
            });
        }
    }

    discovered.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    out.extend(discovered);

    if !seen.contains(OMARCHY_DEFAULT_DARK_NAME) {
        out.push(ThemeDescriptor {
            name: OMARCHY_DEFAULT_DARK_NAME.to_owned(),
            colors_path: None,
            mode: AppearanceMode::Dark,
            is_user_override: false,
            is_builtin: true,
        });
    }
    if !seen.contains(OMARCHY_DEFAULT_LIGHT_NAME) {
        out.push(ThemeDescriptor {
            name: OMARCHY_DEFAULT_LIGHT_NAME.to_owned(),
            colors_path: None,
            mode: AppearanceMode::Light,
            is_user_override: false,
            is_builtin: true,
        });
    }
    out
}

/// Loads a theme by name from `~/.config/omarchy/themes/`, `/usr/share/omarchy/themes/`,
/// live Omarchy state (`"omarchy-live"`), or built-in fallbacks.
pub fn load_theme_by_name(name: &str) -> Result<ThemePalette, ThemeError> {
    let user_dir = ThemePalette::default_user_themes_dir();
    let system_dir = ThemePalette::default_system_themes_dir();
    let current_dir = ThemePalette::default_omarchy_dir();
    load_theme_by_name_in(
        name,
        user_dir.as_deref(),
        Some(system_dir.as_path()),
        current_dir.as_deref(),
    )
}

/// Loads a theme by name from explicit user, system, and current-state directories.
pub fn load_theme_by_name_in(
    name: &str,
    user_dir: Option<&Path>,
    system_dir: Option<&Path>,
    current_dir: Option<&Path>,
) -> Result<ThemePalette, ThemeError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed == "."
        || trimmed == ".."
    {
        return Err(ThemeError::Parse(format!("invalid theme name `{name}`")));
    }

    match trimmed {
        OMARCHY_LIVE_THEME_NAME => {
            return Ok(current_dir
                .and_then(|d| ThemePalette::load_from_omarchy_dir(d).ok())
                .unwrap_or_else(ThemePalette::default_dark));
        }
        OMARCHY_DEFAULT_DARK_NAME => return Ok(ThemePalette::default_dark()),
        OMARCHY_DEFAULT_LIGHT_NAME => return Ok(ThemePalette::default_light()),
        _ => {}
    }

    let mut last_parse_err: Option<ThemeError> = None;
    for dir_opt in [user_dir, system_dir] {
        let Some(dir) = dir_opt else {
            continue;
        };
        let candidate = dir.join(trimmed).join("colors.toml");
        if candidate.is_file() {
            match ThemePalette::load_from_path(&candidate) {
                Ok(mut pal) => {
                    if pal.source_name.is_none() {
                        pal.source_name = Some(trimmed.to_owned());
                    }
                    return Ok(pal);
                }
                Err(err) => {
                    tracing::warn!(
                        path = %candidate.display(),
                        error = %err,
                        "skipping invalid omarchy theme colors.toml"
                    );
                    last_parse_err = Some(err);
                }
            }
        }
    }

    Err(last_parse_err.unwrap_or_else(|| {
        ThemeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("theme `{trimmed}` not found"),
        ))
    }))
}

/// Holds both zero-polling background watcher handles alive until dropped.
pub struct ThemeRuntimeHandles {
    pub theme_watcher: Option<ThemeWatcherHandle>,
    pub hyprland_watcher: Option<HyprlandWatcherHandle>,
}

/// Convenience startup helper launching both the Omarchy `inotify` theme watcher and
/// Hyprland `.socket2.sock` rounding listener onto a single [`ThemeEvent`] channel.
pub fn start_live_watchers(
    material: MaterialStyle,
) -> (Theme, mpsc::Receiver<ThemeEvent>, ThemeRuntimeHandles) {
    let (tx, rx) = mpsc::channel(32);

    let (palette, theme_watcher) = match WatcherConfig::default_omarchy()
        .and_then(|cfg| spawn_theme_watcher(cfg, tx.clone()))
    {
        Ok((pal, handle)) => (pal, Some(handle)),
        Err(err) => {
            tracing::warn!(error = %err, "omarchy theme watcher unavailable; using default theme");
            (ThemePalette::load_omarchy_or_default(), None)
        }
    };

    let (rounding, hyprland_watcher) = spawn_hyprland_rounding_watcher(tx);
    let theme = Theme::from_parts(palette, rounding, material);

    (
        theme,
        rx,
        ThemeRuntimeHandles {
            theme_watcher,
            hyprland_watcher,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        // Keep socket paths short enough for Linux's 108-byte sockaddr_un SUN_LEN limit.
        let base = if std::path::Path::new("/tmp").is_dir() {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!("omx-{prefix}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn parses_dark_omarchy_v4_colors_and_builds_rex_theme() {
        let toml = r##"
# Generated by Aether for Omarchy v4.
mode = "dark"
accent = "#e28203"
selection = "#2a2a2a"
selection_foreground = "#121212"
muted = "#9e9e9e"
background = "#121212"
dark_background = "#0e0e0e"
darker_background = "#090909"
lighter_background = "#2a2a2a"
foreground = "#ffffff"
dark_foreground = "#bfbfbf"
light_foreground = "#ffffff"
bright_foreground = "#ffffff"
red = "#ef3827"
yellow = "#bca674"
orange = "#f15647"
green = "#b79d62"
cyan = "#b39456"
blue = "#e28203"
magenta = "#db860b"
bright_red = "#ff5940"
bright_yellow = "#d0ba89"
bright_green = "#cbb176"
bright_cyan = "#c8a96a"
bright_blue = "#ff980f"
bright_magenta = "#fd9d12"
"##;
        let pal = ThemePalette::from_toml_str(toml).expect("parse dark colors.toml");
        assert_eq!(pal.mode, AppearanceMode::Dark);
        assert_eq!(pal.mode_2031_reply(), b"\x1b[?997;1n");
        assert_eq!(pal.terminal.background, Rgb::new(0x12, 0x12, 0x12));
        assert_eq!(pal.terminal.foreground, Rgb::new(0xff, 0xff, 0xff));
        assert_eq!(pal.ui.surface_frame.rgb(), Rgb::new(0x0e, 0x0e, 0x0e));
        assert_eq!(pal.ui.accent_primary.rgb(), Rgb::new(0xe2, 0x82, 0x03));

        // Low-contrast #121212 on #2a2a2a is automatically promoted to readable foreground.
        assert!(
            pal.terminal
                .selection_fg
                .wcag_contrast(pal.terminal.selection_bg)
                >= 3.0
        );

        // Verify Rex set_theme payload + ANSI 16 palette + Mode 2031 scheme.
        let args = pal.to_rex_set_theme_args();
        assert_eq!(args.background, "#121212");
        assert_eq!(args.foreground, "#ffffff");
        assert_eq!(args.scheme, "dark");
        assert_eq!(args.palette.len(), 16);
        assert_eq!(args.palette[0], "0=#121212");
        assert_eq!(args.palette[1], "1=#ef3827");
        assert_eq!(args.palette[4], "4=#e28203");
        assert_eq!(args.palette[15], "15=#ffffff");

        let json = pal.to_rex_set_theme_json();
        assert_eq!(json["scheme"], "dark");
        assert_eq!(json["background"], "#121212");

        // Translucent mode lowers frame/pane alpha without mutating original tokens.
        let translucent = pal.ui.with_material(MaterialStyle::translucent_default());
        assert!(translucent.surface_pane.a < 255);
        assert!(translucent.surface_frame.a <= translucent.surface_pane.a);
    }

    #[test]
    fn parses_light_theme_and_auto_detects_light_mode() {
        let toml = r##"
background = "#f8f9fb"
foreground = "#1c1f26"
accent = "#1e66f5"
red = "#d20f39"
green = "#40a02b"
"##;
        let pal = ThemePalette::from_toml_str(toml).expect("parse light colors.toml");
        assert_eq!(pal.mode, AppearanceMode::Light);
        assert_eq!(pal.mode_2031_reply(), b"\x1b[?997;2n");
        assert_eq!(pal.to_rex_set_theme_args().scheme, "light");
        // Light mode hover & borders use dark-ink alphas (r=0, g=0, b=0).
        assert_eq!(pal.ui.surface_row_hover.rgb(), Rgb::new(0, 0, 0));
        assert_eq!(pal.ui.border_hairline.rgb(), Rgb::new(0, 0, 0));
    }

    #[test]
    fn unreadable_low_contrast_ansi_blue_and_status_working_are_lifted() {
        let toml = r##"
mode = "dark"
accent = "#7675d2"
selection = "#2a2a2a"
selection_foreground = "#121212"
muted = "#9e9e9e"
background = "#121212"
dark_background = "#0e0e0e"
darker_background = "#090909"
lighter_background = "#2a2a2a"
foreground = "#ffffff"
red = "#d2594b"
yellow = "#efcf76"
orange = "#d97266"
green = "#f5d970"
cyan = "#e4c06b"
blue = "#0d003d"
magenta = "#b963be"
bright_red = "#ff7966"
bright_yellow = "#f7d988"
bright_green = "#fce283"
bright_cyan = "#f0d188"
bright_blue = "#120046"
bright_magenta = "#e680f3"
"##;
        let pal = ThemePalette::from_toml_str(toml).expect("parse theme");
        let bg = pal.terminal.background;
        // Near-invisible #0d003d / #120046 on #121212 must be lifted to readable indigo-blue.
        assert!(
            pal.terminal.ansi[4].wcag_contrast(bg) >= 3.8,
            "ansi[4] blue should be readable on #121212, got {:?} (contrast {:.2})",
            pal.terminal.ansi[4],
            pal.terminal.ansi[4].wcag_contrast(bg)
        );
        assert!(
            pal.terminal.ansi[12].wcag_contrast(bg) >= 4.5,
            "ansi[12] bright_blue should be readable on #121212, got {:?} (contrast {:.2})",
            pal.terminal.ansi[12],
            pal.terminal.ansi[12].wcag_contrast(bg)
        );
        // Preserve dominant blue hue.
        assert!(pal.terminal.ansi[4].b > pal.terminal.ansi[4].r);
        assert!(pal.terminal.ansi[4].b > pal.terminal.ansi[4].g);
        // Sidebar working dot must also remain legible on dark_background (#0e0e0e).
        assert!(
            pal.ui
                .status_working
                .rgb()
                .wcag_contrast(pal.ui.surface_sidebar.rgb())
                >= 3.8
        );
        // Already-readable colors must not be altered.
        assert_eq!(pal.terminal.ansi[1], Rgb::new(0xd2, 0x59, 0x4b));
        assert_eq!(pal.terminal.ansi[2], Rgb::new(0xf5, 0xd9, 0x70));
        assert_eq!(pal.terminal.ansi[0], Rgb::new(0x12, 0x12, 0x12));
    }

    #[test]
    fn wcag_contrast_and_osc11_formatting() {
        let black = Rgb::new(0, 0, 0);
        let white = Rgb::new(255, 255, 255);
        assert!((black.wcag_contrast(white) - 21.0).abs() < 0.01);
        assert_eq!(black.readable_foreground(), white);
        assert_eq!(white.readable_foreground(), black);

        let bg = Rgb::new(0x12, 0x34, 0xab);
        assert_eq!(bg.to_osc11_reply(), "\x1b]11;rgb:1212/3434/abab\x1b\\");
    }

    #[test]
    fn parses_live_local_omarchy_colors_toml_if_present() {
        if let Some(dir) = ThemePalette::default_omarchy_dir()
            && dir.join("theme/colors.toml").is_file()
        {
            let pal = ThemePalette::load_from_omarchy_dir(&dir)
                .expect("live Omarchy colors.toml should parse cleanly");
            let args = pal.to_rex_set_theme_args();
            assert_eq!(args.palette.len(), 16);
            assert!(args.scheme == "dark" || args.scheme == "light");
        }
    }

    #[test]
    fn smoke_queries_live_hyprland_rounding_if_running() {
        let rounding = query_current_rounding_or_zero();
        assert!(rounding >= 0.0);
        if let Some(dir) = hyprland_socket_dir() {
            let direct = query_rounding_sync(&dir).expect("live Hyprland socket query");
            assert!((direct - rounding).abs() <= f32::EPSILON);
        }
    }

    #[tokio::test]
    async fn watcher_survives_atomic_symlink_swap() {
        let root = make_temp_dir("symlink");
        let theme_a = root.join("theme-a");
        let theme_b = root.join("theme-b");
        let current = root.join("current");
        std::fs::create_dir_all(&theme_a).expect("mkdir theme-a");
        std::fs::create_dir_all(&theme_b).expect("mkdir theme-b");
        std::fs::create_dir_all(&current).expect("mkdir current");

        std::fs::write(
            theme_a.join("colors.toml"),
            "mode = \"dark\"\nbackground = \"#111111\"\nforeground = \"#eeeeee\"\naccent = \"#ff0000\"\n",
        )
        .expect("write theme-a");
        std::fs::write(
            theme_b.join("colors.toml"),
            "mode = \"light\"\nbackground = \"#fafafa\"\nforeground = \"#111111\"\naccent = \"#0055ff\"\n",
        )
        .expect("write theme-b");

        let theme_link = current.join("theme");
        symlink(&theme_a, &theme_link).expect("initial symlink");

        let (tx, mut rx) = mpsc::channel(8);
        let mut cfg = WatcherConfig::new(current.clone());
        cfg.debounce = Duration::from_millis(30);
        let (initial, _handle) = spawn_theme_watcher(cfg, tx).expect("spawn watcher");
        assert_eq!(initial.terminal.background, Rgb::new(0x11, 0x11, 0x11));
        assert_eq!(initial.mode, AppearanceMode::Dark);

        // Perform an atomic symlink swap (symlink temp -> rename over `current/theme`).
        let tmp_link = current.join("theme.next");
        symlink(&theme_b, &tmp_link).expect("create temp symlink");
        std::fs::rename(&tmp_link, &theme_link).expect("atomic rename symlink");

        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("watcher should emit within 3s")
            .expect("channel open");
        match ev {
            ThemeEvent::PaletteChanged(pal) => {
                assert_eq!(pal.mode, AppearanceMode::Light);
                assert_eq!(pal.terminal.background, Rgb::new(0xfa, 0xfa, 0xfa));
                assert_eq!(pal.ui.accent_primary.rgb(), Rgb::new(0x00, 0x55, 0xff));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Verify watches re-armed on the newly linked directory by mutating theme-b/colors.toml.
        std::fs::write(
            theme_b.join("colors.toml"),
            "mode = \"dark\"\nbackground = \"#222233\"\nforeground = \"#ffffff\"\naccent = \"#33cc99\"\n",
        )
        .expect("overwrite theme-b colors");

        let ev2 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("re-armed watcher should emit on second edit")
            .expect("channel open");
        match ev2 {
            ThemeEvent::PaletteChanged(pal) => {
                assert_eq!(pal.terminal.background, Rgb::new(0x22, 0x22, 0x33));
                assert_eq!(pal.ui.accent_primary.rgb(), Rgb::new(0x33, 0xcc, 0x99));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn hyprland_mock_ipc_reloads_rounding_on_configreloaded() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::UnixListener;

        let dir = make_temp_dir("hypr-mock");
        let cmd_sock = dir.join(".socket.sock");
        let ev_sock = dir.join(".socket2.sock");

        let cmd_listener = UnixListener::bind(&cmd_sock).expect("bind mock .socket.sock");
        let ev_listener = UnixListener::bind(&ev_sock).expect("bind mock .socket2.sock");

        // Mock command server responds with rounding=10.
        let cmd_task = tokio::spawn(async move {
            if let Ok((mut s, _)) = cmd_listener.accept().await {
                let mut req = [0u8; 64];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut req).await;
                let _ = s
                    .write_all(br#"{"option": "decoration:rounding", "int": 10, "set": true}"#)
                    .await;
            }
        });

        let (tx, mut rx) = mpsc::channel(4);
        let _handle = spawn_hyprland_rounding_watcher_in(dir.clone(), 0.0, tx);

        // Accept socket2 connection and push configreloaded>> event.
        let (mut ev_stream, _) = tokio::time::timeout(Duration::from_secs(2), ev_listener.accept())
            .await
            .expect("watcher should connect to socket2")
            .expect("accept socket2");
        ev_stream
            .write_all(b"workspace>>2\nconfigreloaded>>\n")
            .await
            .expect("send configreloaded");

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("rounding update expected")
            .expect("channel open");
        assert_eq!(event, ThemeEvent::RoundingChanged(10.0));

        let _ = cmd_task.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notification_mute_rate_limit_and_sanitization() {
        let mut filter = MuteFilter::new(Duration::from_secs(3));
        let target = NotificationTarget {
            host: "local".to_owned(),
            session_id: "sess-1".to_owned(),
            block_id: "blk-1".to_owned(),
        };
        let t0 = Instant::now();
        assert!(filter.allow_and_record(&target, t0));
        // Cooldown suppresses duplicate within 3s.
        assert!(!filter.allow_and_record(&target, t0 + Duration::from_secs(1)));
        // Allowed again after 3s.
        assert!(filter.allow_and_record(&target, t0 + Duration::from_secs(4)));

        // Session mute blocks all its panes.
        assert!(filter.toggle_session("sess-1"));
        assert!(!filter.allow_and_record(&target, t0 + Duration::from_secs(10)));
        assert!(!filter.toggle_session("sess-1"));

        // Block mute blocks only that pane.
        filter.mute_block("blk-1");
        assert!(!filter.allow_and_record(&target, t0 + Duration::from_secs(15)));
        filter.unmute_block("blk-1");
        assert!(filter.allow_and_record(&target, t0 + Duration::from_secs(15)));

        // Sanitization strips BiDi overrides and control chars.
        let dirty = "Needs\x07 you\u{202E}spoof\nnow\t!";
        assert_eq!(sanitize_notification_text(dirty), "Needs youspoof now !");
    }

    #[test]
    fn discover_themes_skips_missing_colors_toml_and_prefers_user_override() {
        let root = make_temp_dir("themes");
        let user_dir = root.join("user");
        let sys_dir = root.join("sys");
        let cur_dir = root.join("current");
        std::fs::create_dir_all(user_dir.join("catppuccin")).unwrap();
        std::fs::create_dir_all(sys_dir.join("catppuccin")).unwrap();
        std::fs::create_dir_all(sys_dir.join("osaka-jade")).unwrap();
        std::fs::create_dir_all(sys_dir.join("alpine-light")).unwrap();
        std::fs::create_dir_all(cur_dir.join("theme")).unwrap();

        // User override uses light mode, system uses dark -> user must win.
        std::fs::write(
            user_dir.join("catppuccin/colors.toml"),
            "background = \"#f5f5f5\"\nforeground = \"#111111\"\n",
        )
        .unwrap();
        std::fs::write(
            sys_dir.join("catppuccin/colors.toml"),
            "background = \"#111111\"\nforeground = \"#eeeeee\"\n",
        )
        .unwrap();
        std::fs::write(
            sys_dir.join("alpine-light/colors.toml"),
            "background = \"#ffffff\"\nforeground = \"#000000\"\n",
        )
        .unwrap();
        std::fs::write(
            cur_dir.join("theme/colors.toml"),
            "background = \"#121212\"\nforeground = \"#dddddd\"\n",
        )
        .unwrap();
        // Note: sys_dir/osaka-jade intentionally has NO colors.toml!

        let list = discover_themes_in(Some(&user_dir), Some(&sys_dir), Some(&cur_dir));
        let names: Vec<&str> = list.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"omarchy-live"));
        assert!(names.contains(&"catppuccin"));
        assert!(names.contains(&"alpine-light"));
        assert!(!names.contains(&"osaka-jade"));

        let cat = list.iter().find(|t| t.name == "catppuccin").unwrap();
        assert!(cat.is_user_override);
        assert_eq!(cat.mode, AppearanceMode::Light);

        let loaded = load_theme_by_name_in(
            "catppuccin",
            Some(&user_dir),
            Some(&sys_dir),
            Some(&cur_dir),
        )
        .unwrap();
        assert_eq!(loaded.mode, AppearanceMode::Light);

        assert!(
            load_theme_by_name_in(
                "osaka-jade",
                Some(&user_dir),
                Some(&sys_dir),
                Some(&cur_dir),
            )
            .is_err()
        );
        assert!(
            load_theme_by_name_in("../escape", Some(&user_dir), Some(&sys_dir), Some(&cur_dir),)
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_theme_by_name_falls_back_to_system_when_user_override_corrupt() {
        let root = make_temp_dir("corrupt-fallback");
        let user_dir = root.join("user");
        let sys_dir = root.join("sys");
        std::fs::create_dir_all(user_dir.join("nord")).unwrap();
        std::fs::create_dir_all(sys_dir.join("nord")).unwrap();
        std::fs::create_dir_all(user_dir.join("only-broken")).unwrap();

        std::fs::write(user_dir.join("nord/colors.toml"), "NOT VALID TOML = [[[").unwrap();
        std::fs::write(
            sys_dir.join("nord/colors.toml"),
            "background = \"#2e3440\"\nforeground = \"#d8dee9\"\n",
        )
        .unwrap();
        std::fs::write(
            user_dir.join("only-broken/colors.toml"),
            "NOT VALID TOML = [[[",
        )
        .unwrap();

        let pal = load_theme_by_name_in("nord", Some(&user_dir), Some(&sys_dir), None)
            .expect("should fall back to valid system nord theme");
        assert_eq!(pal.source_name.as_deref(), Some("nord"));
        assert_eq!(pal.mode, AppearanceMode::Dark);

        assert!(
            load_theme_by_name_in("only-broken", Some(&user_dir), Some(&sys_dir), None).is_err(),
            "must still return Err when no valid candidate exists anywhere"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
