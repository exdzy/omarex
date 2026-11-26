//! Persistent user configuration stored atomically at `~/.config/omarex/settings.json`.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use omarchy_theme::MaterialStyle;
use serde::{Deserialize, Serialize};

/// Sidebar / tab-strip docking position (all 4 Rex toolbar layouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarPosition {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

/// Sidebar / toolbar reveal behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarBehavior {
    #[default]
    Always,
    OnHover,
    Hidden,
}

/// Pane chrome spacing density.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutDensity {
    /// Inset rounded pane cards (`8px` gap/padding, synced with Hyprland `decoration:rounding`).
    #[default]
    Comfortable,
    /// Edge-to-edge zero-padding panes (`0px` radius/inset).
    Compact,
}

/// Persistent pane header visibility mode (`always`, `on_hover`, `hidden`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedPaneHeaderMode {
    #[default]
    Always,
    OnHover,
    Hidden,
}

/// Window surface material mode.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WindowMaterial {
    /// Fully opaque surfaces (default on Omarchy for direct scanout & crisp contrast).
    #[default]
    Classic,
    /// ARGB translucent surfaces for Hyprland Kawase blur (`o.transparent_window("omarex")`).
    Translucent {
        #[serde(default = "default_translucent_alpha")]
        alpha: f32,
    },
}

fn default_translucent_alpha() -> f32 {
    0.90
}

impl WindowMaterial {
    pub fn to_theme_material(self) -> MaterialStyle {
        match self {
            Self::Classic => MaterialStyle::Classic,
            Self::Translucent { alpha } => MaterialStyle::Translucent {
                alpha: alpha.clamp(0.5, 1.0),
            },
        }
    }
}

/// Saved remote Rex server entry (in addition to local default socket & server `hosts.list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHost {
    pub id: String,
    pub label: String,
    pub endpoint: String,
    #[serde(default)]
    pub auto_connect: bool,
}

/// Persistent user settings for `omarex`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSettings {
    /// Toolbar / sidebar dock edge (`left` default for `tr3x`).
    pub sidebar_position: SidebarPosition,
    /// Sidebar visibility mode (`always`, `on_hover`, `hidden`).
    pub sidebar_behavior: SidebarBehavior,
    /// Sidebar width in logical pixels (clamped `180..=380`).
    pub sidebar_width_px: f32,
    /// Pane spacing density (`comfortable` vs `compact`).
    pub density: LayoutDensity,
    /// Pane header visibility (`always`, `on_hover`, `hidden`).
    pub pane_header_mode: SavedPaneHeaderMode,
    /// Highlight the focused pane's card border with the active theme accent.
    pub highlight_active_border: bool,
    /// Subtly dim unfocused panes in split layouts.
    pub fade_inactive_panes: bool,
    /// Adapt corner rounding to Hyprland's live `decoration:rounding`.
    pub adaptive_rounding: bool,
    /// Surface material (`classic` vs `translucent`).
    pub material: WindowMaterial,
    /// Terminal font family (defaults to Omarchy's `JetBrainsMono Nerd Font`).
    pub font_family: String,
    /// Terminal font size in pt (clamped `7.0..=36.0`).
    pub font_size_pt: f32,
    /// Maximum number of recently visited sessions kept warm in the LRU stream cache.
    pub warm_session_cache_limit: usize,
    /// Auto-alias `tr3x.lua`'s `ctrl+cmd+*` chords to Hyprland-conflict-free `Ctrl+Alt+*`.
    pub remap_ctrl_cmd_to_ctrl_alt: bool,
    /// Manually saved remote hosts (`https://*.ts.net`, `ssh://...`, `host:port`).
    pub saved_hosts: Vec<SavedHost>,
    /// Session IDs collapsed in the sidebar (`"<host_id>/<session_id>"`).
    pub collapsed_sessions: BTreeSet<String>,
    /// Session IDs with desktop/bell notifications muted (`"<host_id>/<session_id>"`).
    pub muted_sessions: BTreeSet<String>,
    /// Block IDs with desktop/bell notifications muted (`"<host_id>/<block_id>"`).
    pub muted_blocks: BTreeSet<String>,
    /// Last active `(host_id, session_id)` to pass as `preferred_session_id` on startup.
    pub last_host_id: Option<String>,
    pub last_session_id: Option<String>,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            sidebar_position: SidebarPosition::Left,
            sidebar_behavior: SidebarBehavior::Always,
            sidebar_width_px: 248.0,
            density: LayoutDensity::Comfortable,
            pane_header_mode: SavedPaneHeaderMode::Always,
            highlight_active_border: true,
            fade_inactive_panes: false,
            adaptive_rounding: true,
            material: WindowMaterial::Classic,
            font_family: "JetBrainsMono Nerd Font".to_owned(),
            font_size_pt: 11.0,
            warm_session_cache_limit: 4,
            remap_ctrl_cmd_to_ctrl_alt: true,
            saved_hosts: Vec::new(),
            collapsed_sessions: BTreeSet::new(),
            muted_sessions: BTreeSet::new(),
            muted_blocks: BTreeSet::new(),
            last_host_id: None,
            last_session_id: None,
        }
    }
}

impl UserSettings {
    /// Default config file path: `~/.config/omarex/settings.json`.
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join("omarex")
            .join("settings.json")
    }

    /// Clamps numeric settings to safe runtime bounds.
    pub fn sanitize(&mut self) {
        self.sidebar_width_px = self.sidebar_width_px.clamp(180.0, 380.0);
        self.font_size_pt = self.font_size_pt.clamp(7.0, 36.0);
        self.warm_session_cache_limit = self.warm_session_cache_limit.clamp(1, 16);
        if let WindowMaterial::Translucent { alpha } = &mut self.material {
            *alpha = alpha.clamp(0.5, 1.0);
        }
        if self.font_family.trim().is_empty() {
            self.font_family = "JetBrainsMono Nerd Font".to_owned();
        }
    }

    /// Loads settings from `path`, falling back cleanly to defaults if the file does not exist.
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let mut settings: Self = serde_json::from_str(&raw)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                settings.sanitize();
                Ok(settings)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Loads from `~/.config/omarex/settings.json`, logging and returning defaults on parse failure.
    pub fn load_or_default() -> Self {
        let path = Self::default_path();
        match Self::load_from(&path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "using default settings");
                Self::default()
            }
        }
    }

    /// Atomically persists settings via temp-file + `sync_all()` + `rename()` so crashes or
    /// concurrent reads never observe a truncated JSON file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut sanitized = self.clone();
        sanitized.sanitize();

        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("settings path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("settings.json");
        let tmp_path = parent.join(format!(".{file_name}.{}.{nanos}.tmp", std::process::id()));

        let payload = serde_json::to_vec_pretty(&sanitized)?;
        {
            let mut f = std::fs::File::create(&tmp_path)
                .with_context(|| format!("failed to create {}", tmp_path.display()))?;
            f.write_all(&payload)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }

        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                tmp_path.display()
            )
        })?;
        Ok(())
    }

    /// Persist sidebar collapse state for `(host_id, session_id)`.
    pub fn set_session_collapsed(&mut self, host_id: &str, session_id: &str, collapsed: bool) {
        let key = format!("{host_id}/{session_id}");
        if collapsed {
            self.collapsed_sessions.insert(key);
        } else {
            self.collapsed_sessions.remove(&key);
        }
    }

    /// Toggle notification mute for `(host_id, session_id)`. Returns `true` if now muted.
    pub fn toggle_mute_session(&mut self, host_id: &str, session_id: &str) -> bool {
        let key = format!("{host_id}/{session_id}");
        if !self.muted_sessions.remove(&key) {
            self.muted_sessions.insert(key);
            true
        } else {
            false
        }
    }

    /// Toggle notification mute for `(host_id, block_id)`. Returns `true` if now muted.
    pub fn toggle_mute_block(&mut self, host_id: &str, block_id: &str) -> bool {
        let key = format!("{host_id}/{block_id}");
        if !self.muted_blocks.remove(&key) {
            self.muted_blocks.insert(key);
            true
        } else {
            false
        }
    }

    pub fn is_muted(&self, host_id: &str, session_id: &str, block_id: &str) -> bool {
        self.muted_sessions
            .contains(&format!("{host_id}/{session_id}"))
            || self.muted_blocks.contains(&format!("{host_id}/{block_id}"))
    }

    pub fn increase_font(&mut self) -> f32 {
        self.font_size_pt = (self.font_size_pt + 1.0).clamp(7.0, 36.0);
        self.font_size_pt
    }

    pub fn decrease_font(&mut self) -> f32 {
        self.font_size_pt = (self.font_size_pt - 1.0).clamp(7.0, 36.0);
        self.font_size_pt
    }

    pub fn reset_font(&mut self) -> f32 {
        self.font_size_pt = 11.0;
        self.font_size_pt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join(format!("omarex-cfg-{tag}-{}-{nanos}", std::process::id()))
            .join("settings.json")
    }

    #[test]
    fn atomic_save_and_load_round_trip_and_sanitizes() {
        let path = temp_settings_path("roundtrip");
        let missing = UserSettings::load_from(&path).expect("missing file yields defaults");
        assert_eq!(missing, UserSettings::default());
        assert_eq!(missing.pane_header_mode, SavedPaneHeaderMode::Always);

        let mut cfg = UserSettings {
            sidebar_position: SidebarPosition::Right,
            sidebar_behavior: SidebarBehavior::OnHover,
            sidebar_width_px: 999.0, // clamped to 380.0
            density: LayoutDensity::Compact,
            pane_header_mode: SavedPaneHeaderMode::OnHover,
            highlight_active_border: false,
            fade_inactive_panes: true,
            adaptive_rounding: false,
            material: WindowMaterial::Translucent { alpha: 0.92 },
            font_size_pt: 13.5,
            saved_hosts: vec![SavedHost {
                id: "host:1".into(),
                label: "studio".into(),
                endpoint: "studio.tail1234.ts.net".into(),
                auto_connect: true,
            }],
            ..UserSettings::default()
        };
        cfg.set_session_collapsed("local", "session:t3", true);
        assert!(cfg.toggle_mute_session("local", "session:1"));
        assert!(cfg.is_muted("local", "session:1", "block:any"));
        assert!(cfg.toggle_mute_block("local", "block:2"));
        assert!(cfg.is_muted("local", "session:other", "block:2"));

        cfg.save_to(&path).expect("atomic save succeeds");

        // Ensure no stray .tmp files were left behind.
        let parent = path.parent().unwrap();
        let entries: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["settings.json"]);

        let loaded = UserSettings::load_from(&path).expect("load saved settings");
        assert_eq!(loaded.sidebar_position, SidebarPosition::Right);
        assert_eq!(loaded.sidebar_behavior, SidebarBehavior::OnHover);
        assert!((loaded.sidebar_width_px - 380.0).abs() <= f32::EPSILON);
        assert_eq!(loaded.density, LayoutDensity::Compact);
        assert_eq!(loaded.pane_header_mode, SavedPaneHeaderMode::OnHover);
        assert!(!loaded.highlight_active_border);
        assert!(loaded.fade_inactive_panes);
        assert!(!loaded.adaptive_rounding);
        assert!(loaded.collapsed_sessions.contains("local/session:t3"));
        assert_eq!(loaded.material, WindowMaterial::Translucent { alpha: 0.92 });
        assert_eq!(loaded.saved_hosts.len(), 1);
        assert!(loaded.is_muted("local", "session:1", "block:99"));

        let _ = std::fs::remove_dir_all(parent);
    }
}
