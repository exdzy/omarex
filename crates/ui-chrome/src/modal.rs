//! Unified centered modals (`460–620px`) driven by `FocusTarget::Modal` + `PaletteMatcher` +
//! `TextInput`, including Command Palette, Switch Session, Go to Anything, Activity Picker,
//! live Settings panel, Rename / Add Host, Keyboard Shortcuts Help, native `rex.ui` `input` &
//! `pick` overlay (`PromptSession` with timeout bar & multi-select), Which-Key chord/mode popup,
//! inline terminal Find bar, and 6-anchor Toast notifications.

use std::time::Instant;

use omarchy_theme::{MaterialStyle, Rgba, Theme, UiTokens};
use tr3x_bridge::keymap::WhichKeyHint;
use tr3x_bridge::prompts::{ClientReplyPayload, PromptKind, PromptSession};

use crate::draw::{
    FontKind, FontWeight, HitTarget, ModalKind, SettingChange, SettingsCategory, ShadowSpec,
    TextAlign, UiAction, UiDrawList, UiInteraction, cap_radius, z_layer,
};
use crate::icons::IconId;
use crate::layout::{
    ChromeLayoutConfig, PaneDensity, PaneHeaderMode, RectF, ToolbarBehavior, ToolbarPosition,
};
use crate::palette::{PaletteItem, PaletteMatcher, ScoredMatch, highlight_spans};
use crate::text_input::TextInput;

/// Normalized key events understood by modals and overlay inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Enter,
    CtrlEnter,
    Tab,
    Space,
    Escape,
    Backspace,
    Delete,
    WordBackspace,
    Left { extend: bool },
    Right { extend: bool },
    Home { extend: bool },
    End { extend: bool },
    SelectAll,
    Char(char),
    Paste(String),
}

/// Result of handling a keyboard event inside [`ModalState`].
#[derive(Debug, Clone, PartialEq)]
pub enum ModalKeyOutcome {
    Consumed,
    Action(UiAction),
    ReplyPrompt(ClientReplyPayload),
    Closed,
}

/// Pre-encoded square ISO/IEC 18004 QR code module matrix (`true` = dark module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    pub size: usize,
    pub modules: Vec<bool>,
}

impl QrMatrix {
    /// Encodes `text` into a QR code matrix using Medium error correction (matching Rex CLI).
    #[must_use]
    pub fn encode_text(text: &str) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        let qr = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium).ok()?;
        let size = usize::try_from(qr.size()).ok()?;
        let mut modules = Vec::with_capacity(size * size);
        for y in 0..size {
            for x in 0..size {
                modules.push(qr.get_module(x as i32, y as i32));
            }
        }
        Some(Self { size, modules })
    }

    #[must_use]
    pub fn get(&self, x: usize, y: usize) -> bool {
        self.modules
            .get(y * self.size + x)
            .copied()
            .unwrap_or(false)
    }
}

/// Live Tailscale pairing state of the local Rex server for Rex iOS QR onboarding.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TailscalePairState {
    #[default]
    ServerDisconnected,
    Unsupported,
    Disabled {
        error: Option<String>,
    },
    Starting,
    NeedsLogin {
        auth_url: Option<String>,
    },
    NeedsMachineApproval,
    HttpsPending {
        dns_name: Option<String>,
    },
    Ready {
        url: String,
    },
}

/// Snapshot of local Rex server Tailscale / iOS QR pairing status displayed in Settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IosPairingSnapshot {
    pub state: TailscalePairState,
    pub tailnet_name: Option<String>,
    pub account_login: Option<String>,
    pub dns_name: Option<String>,
    pub allow_tailnet_members: bool,
    pub busy: bool,
    pub last_error: Option<String>,
    pub qr_modules: Option<QrMatrix>,
}

impl IosPairingSnapshot {
    /// Convenience constructor for a ready-to-scan Tailscale HTTPS pairing state.
    #[must_use]
    pub fn ready(
        url: impl Into<String>,
        tailnet_name: Option<String>,
        account_login: Option<String>,
        dns_name: Option<String>,
        allow_tailnet_members: bool,
    ) -> Self {
        let url = url.into();
        let qr_modules = QrMatrix::encode_text(&url);
        Self {
            state: TailscalePairState::Ready { url },
            tailnet_name,
            account_login,
            dns_name,
            allow_tailnet_members,
            busy: false,
            last_error: None,
            qr_modules,
        }
    }
}

/// Connection & lifecycle state of the local Rex server shown in Settings → Server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LocalServerState {
    Running,
    Reconnecting {
        reason: String,
    },
    #[default]
    Stopped,
    Error(String),
}

/// Snapshot of local Rex server & OSC 7577 Program Resume status displayed in Settings → Server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerSettingsSnapshot {
    pub state: LocalServerState,
    pub version: Option<String>,
    pub api_revision: Option<u32>,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub session_count: u64,
    pub endpoint: String,
    pub resumable_trusted_count: usize,
    pub resumable_untrusted_count: usize,
    pub busy_action: Option<String>,
    pub last_error: Option<String>,
    /// Two-step confirmation guard before stopping the local Rex server.
    pub confirm_stop: bool,
}

/// Interactive settings snapshot displayed inside the Settings modal (`Ctrl+Shift+,`).
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsViewSnapshot {
    pub toolbar_position: ToolbarPosition,
    pub toolbar_behavior: ToolbarBehavior,
    pub density: PaneDensity,
    pub pane_header_mode: PaneHeaderMode,
    pub material: MaterialStyle,
    pub highlight_active_border: bool,
    pub fade_inactive_panes: bool,
    pub adaptive_rounding: bool,
    pub program_color_header: bool,
    pub theme_name: Option<String>,
    pub font_family: String,
    pub font_size_pt: f32,
    pub chime_enabled: bool,
    pub desktop_notifications: bool,
    pub server: ServerSettingsSnapshot,
    pub ios_pairing: IosPairingSnapshot,
}

impl SettingsViewSnapshot {
    #[must_use]
    pub fn from_config(
        cfg: &ChromeLayoutConfig,
        material: MaterialStyle,
        highlight_active_border: bool,
        fade_inactive_panes: bool,
        program_color_header: bool,
    ) -> Self {
        Self {
            toolbar_position: cfg.toolbar_position,
            toolbar_behavior: cfg.toolbar_behavior,
            density: cfg.density,
            pane_header_mode: cfg.pane_header_mode,
            material,
            highlight_active_border,
            fade_inactive_panes,
            adaptive_rounding: cfg.hyprland_rounding.is_some(),
            program_color_header,
            theme_name: None,
            font_family: "JetBrainsMono Nerd Font".to_owned(),
            font_size_pt: 13.0,
            chime_enabled: true,
            desktop_notifications: true,
            server: ServerSettingsSnapshot::default(),
            ios_pairing: IosPairingSnapshot::default(),
        }
    }

    #[must_use]
    pub fn with_extras(
        mut self,
        theme_name: Option<String>,
        font_family: impl Into<String>,
        font_size_pt: f32,
        chime_enabled: bool,
        desktop_notifications: bool,
    ) -> Self {
        self.theme_name = theme_name;
        self.font_family = font_family.into();
        self.font_size_pt = font_size_pt;
        self.chime_enabled = chime_enabled;
        self.desktop_notifications = desktop_notifications;
        self
    }

    #[must_use]
    pub fn with_server_status(mut self, server: ServerSettingsSnapshot) -> Self {
        self.server = server;
        self
    }

    #[must_use]
    pub fn with_ios_pairing(mut self, ios_pairing: IosPairingSnapshot) -> Self {
        self.ios_pairing = ios_pairing;
        self
    }
}

/// Purpose of the File/Dir Picker modal (`InsertPath` vs `GoToDirectory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePickerPurpose {
    InsertPath,
    GoToDirectory,
}

/// Entry displayed inside `ModalContent::FilePicker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePickerEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
}

/// Host + Tailscale + stream health row shown in the Diagnostics HUD modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsHostRow {
    pub label: String,
    pub status: String,
    pub tailscale_summary: String,
    pub dropped_events: u64,
    pub active_streams: usize,
}

/// Telemetry snapshot displayed inside the Rich Diagnostics & Telemetry HUD (`Ctrl+Alt+,`).
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticsSnapshot {
    pub app_version: String,
    pub vt_version: String,
    pub gpu_adapter: String,
    pub gpu_backend: String,
    pub frame_times_us: Vec<u32>,
    pub avg_frame_ms: f32,
    pub p95_frame_ms: f32,
    pub max_frame_ms: f32,
    pub last_draw_calls: u32,
    pub last_quads: u32,
    pub last_glyphs: u32,
    pub hosts: Vec<DiagnosticsHostRow>,
    pub keymap_warnings: Vec<String>,
    pub active_ports: Vec<String>,
    pub active_tunnels: Vec<String>,
    /// Redacted OSC 7577 Program Resume record summaries (never includes command argv).
    pub resume_records: Vec<String>,
}

/// Active modal content backing `FocusTarget::Modal(ModalKind)`.
#[derive(Debug, Clone)]
pub enum ModalContent {
    List {
        kind: ModalKind,
        title: String,
        shortcut_badge: Option<String>,
        input: TextInput,
        items: Vec<PaletteItem>,
        matches: Vec<ScoredMatch>,
        selected_row: usize,
        scroll_row: usize,
    },
    FilePicker {
        purpose: FilePickerPurpose,
        current_dir: String,
        remote_host: Option<String>,
        loading: bool,
        error: Option<String>,
        input: TextInput,
        entries: Vec<FilePickerEntry>,
        filtered_indices: Vec<usize>,
        selected_row: usize,
        scroll_row: usize,
    },
    ThemePicker {
        input: TextInput,
        items: Vec<PaletteItem>,
        matches: Vec<ScoredMatch>,
        selected_row: usize,
        scroll_row: usize,
    },
    Diagnostics {
        snapshot: DiagnosticsSnapshot,
        scroll_row: usize,
    },
    RexPrompt {
        session: PromptSession,
        input: TextInput,
        scroll_row: usize,
    },
    Settings {
        snapshot: SettingsViewSnapshot,
        category: SettingsCategory,
        selected_row: usize,
    },
    Rename {
        target_id: String,
        title: String,
        input: TextInput,
    },
    AddHost {
        input: TextInput,
    },
    ShortcutsHelp {
        scroll_row: usize,
    },
}

/// Unified modal controller handling filtering, selection, keyboard navigation, and rendering.
pub struct ModalState {
    pub content: ModalContent,
    matcher: PaletteMatcher,
}

impl ModalState {
    /// Opens a fuzzy-searchable list modal (Command Palette, Switch Session, Go to Anything, Activity Picker).
    #[must_use]
    pub fn open_list(
        kind: ModalKind,
        title: impl Into<String>,
        placeholder: impl Into<String>,
        shortcut_badge: Option<String>,
        items: Vec<PaletteItem>,
    ) -> Self {
        let mut matcher = PaletteMatcher::new();
        let matches = matcher.filter("", &items);
        Self {
            content: ModalContent::List {
                kind,
                title: title.into(),
                shortcut_badge,
                input: TextInput::new(placeholder),
                items,
                matches,
                selected_row: 0,
                scroll_row: 0,
            },
            matcher,
        }
    }

    /// Opens a native `rex.ui` `input` or `pick` overlay backed by `tr3x_bridge::prompts::PromptSession`.
    #[must_use]
    pub fn open_rex_prompt(session: PromptSession) -> Self {
        let mut input = TextInput::with_value(&session.placeholder, &session.input_text);
        if session.kind == PromptKind::Input && !session.input_text.is_empty() {
            input.select_all();
        }
        Self {
            content: ModalContent::RexPrompt {
                session,
                input,
                scroll_row: 0,
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the live Settings panel (`Ctrl+Shift+,`).
    #[must_use]
    pub fn open_settings(snapshot: SettingsViewSnapshot) -> Self {
        Self::open_settings_category(snapshot, SettingsCategory::Workspace)
    }

    /// Opens the live Settings panel directly on `category` (e.g. `SettingsCategory::ConnectIos`).
    #[must_use]
    pub fn open_settings_category(
        snapshot: SettingsViewSnapshot,
        category: SettingsCategory,
    ) -> Self {
        Self {
            content: ModalContent::Settings {
                snapshot,
                category,
                selected_row: 0,
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the remote-capable File/Directory Picker (`InsertPath` / `GoToDirectory`).
    #[must_use]
    pub fn open_file_picker(
        purpose: FilePickerPurpose,
        current_dir: impl Into<String>,
        remote_host: Option<String>,
        entries: Vec<FilePickerEntry>,
        loading: bool,
    ) -> Self {
        let filtered_indices: Vec<usize> = (0..entries.len()).collect();
        let placeholder = match purpose {
            FilePickerPurpose::InsertPath => "Filter files or dirs · Tab descend · Enter insert",
            FilePickerPurpose::GoToDirectory => "Filter directories · Tab descend · Enter cd",
        };
        Self {
            content: ModalContent::FilePicker {
                purpose,
                current_dir: current_dir.into(),
                remote_host,
                loading,
                error: None,
                input: TextInput::new(placeholder),
                entries,
                filtered_indices,
                selected_row: 0,
                scroll_row: 0,
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Updates entries inside an open `FilePicker` modal (e.g., on remote `list_dir` response).
    pub fn set_file_picker_entries(
        &mut self,
        current_dir: impl Into<String>,
        entries: Vec<FilePickerEntry>,
        error: Option<String>,
    ) {
        if let ModalContent::FilePicker {
            current_dir: dir,
            loading,
            error: err_slot,
            input,
            entries: list,
            filtered_indices,
            selected_row,
            scroll_row,
            ..
        } = &mut self.content
        {
            let next_dir = current_dir.into();
            if error.is_none() && *dir != next_dir {
                input.clear();
            }
            *dir = next_dir;
            *loading = false;
            *err_slot = error;
            *list = entries;
            *filtered_indices = filter_file_picker_entries(input.text(), list);
            *selected_row = 0;
            *scroll_row = 0;
        }
    }

    /// Surfaces an error inside an open `FilePicker` modal while preserving the prior
    /// `current_dir` and `entries`.
    pub fn set_file_picker_error(&mut self, error: impl Into<String>) {
        if let ModalContent::FilePicker {
            loading,
            error: err_slot,
            ..
        } = &mut self.content
        {
            *loading = false;
            *err_slot = Some(error.into());
        }
    }

    /// Opens the live-preview Theme Switcher modal (`ChangeTheme`).
    #[must_use]
    pub fn open_theme_picker(items: Vec<PaletteItem>, active_name: Option<&str>) -> Self {
        let mut matcher = PaletteMatcher::new();
        let matches = matcher.filter("", &items);
        let selected_row = active_name
            .and_then(|active| matches.iter().position(|m| items[m.index].id == active))
            .unwrap_or(0);
        let scroll_row = selected_row.saturating_sub(4);
        Self {
            content: ModalContent::ThemePicker {
                input: TextInput::new("Filter themes… (↑/↓ live preview, Enter apply, Esc revert)"),
                items,
                matches,
                selected_row,
                scroll_row,
            },
            matcher,
        }
    }

    /// Opens the Rich Diagnostics & Telemetry HUD (`Ctrl+Alt+,`).
    #[must_use]
    pub fn open_diagnostics(snapshot: DiagnosticsSnapshot) -> Self {
        Self {
            content: ModalContent::Diagnostics {
                snapshot,
                scroll_row: 0,
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the inline Rename modal pre-filled with `current_label`.
    #[must_use]
    pub fn open_rename(
        target_id: impl Into<String>,
        title: impl Into<String>,
        current_label: impl Into<String>,
    ) -> Self {
        let mut input = TextInput::with_value("New label…", current_label);
        input.select_all();
        Self {
            content: ModalContent::Rename {
                target_id: target_id.into(),
                title: title.into(),
                input,
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the Add Remote Host modal (`Name = https://...`, `https://`, `ssh://`, or `unix://`).
    #[must_use]
    pub fn open_add_host() -> Self {
        Self {
            content: ModalContent::AddHost {
                input: TextInput::new("Name = https://box.tailnet.ts.net or ssh://user@host"),
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the Omarchy conflict-free Keyboard Shortcuts Help modal (`?`).
    #[must_use]
    pub fn open_shortcuts_help() -> Self {
        Self {
            content: ModalContent::ShortcutsHelp { scroll_row: 0 },
            matcher: PaletteMatcher::new(),
        }
    }

    #[must_use]
    pub fn kind(&self) -> ModalKind {
        match &self.content {
            ModalContent::List { kind, .. } => *kind,
            ModalContent::FilePicker { .. } => ModalKind::FilePicker,
            ModalContent::ThemePicker { .. } => ModalKind::ThemePicker,
            ModalContent::Diagnostics { .. } => ModalKind::Diagnostics,
            ModalContent::RexPrompt { .. } => ModalKind::RexPrompt,
            ModalContent::Settings { .. } => ModalKind::Settings,
            ModalContent::Rename { .. } => ModalKind::Rename,
            ModalContent::AddHost { .. } => ModalKind::AddHost,
            ModalContent::ShortcutsHelp { .. } => ModalKind::ShortcutsHelp,
        }
    }

    /// Processes a keyboard event while the modal holds `FocusTarget::Modal`.
    pub fn handle_key(&mut self, key: ModalKey) -> ModalKeyOutcome {
        match &mut self.content {
            ModalContent::List {
                input,
                items,
                matches,
                selected_row,
                scroll_row,
                ..
            } => match key {
                ModalKey::Escape => ModalKeyOutcome::Closed,
                ModalKey::Up => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + matches.len() - 1) % matches.len();
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + 1) % matches.len();
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageUp => {
                    *selected_row = selected_row.saturating_sub(6);
                    ensure_row_visible(*selected_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageDown => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + 6).min(matches.len() - 1);
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Enter | ModalKey::CtrlEnter => {
                    if let Some(m) = matches.get(*selected_row) {
                        let id = items[m.index].id.clone();
                        ModalKeyOutcome::Action(UiAction::RunCommand { action_id: id })
                    } else {
                        ModalKeyOutcome::Closed
                    }
                }
                other => {
                    if apply_text_key(input, other) {
                        *matches = self.matcher.filter(input.text(), items);
                        *selected_row = 0;
                        *scroll_row = 0;
                    }
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::FilePicker {
                purpose: _,
                current_dir,
                input,
                entries,
                filtered_indices,
                selected_row,
                scroll_row,
                ..
            } => match key {
                ModalKey::Escape => ModalKeyOutcome::Closed,
                ModalKey::Up => {
                    if !filtered_indices.is_empty() {
                        *selected_row =
                            (*selected_row + filtered_indices.len() - 1) % filtered_indices.len();
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down => {
                    if !filtered_indices.is_empty() {
                        *selected_row = (*selected_row + 1) % filtered_indices.len();
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageUp => {
                    *selected_row = selected_row.saturating_sub(6);
                    ensure_row_visible(*selected_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageDown => {
                    if !filtered_indices.is_empty() {
                        *selected_row = (*selected_row + 6).min(filtered_indices.len() - 1);
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Tab => {
                    if let Some(&entry_idx) = filtered_indices.get(*selected_row)
                        && let Some(entry) = entries.get(entry_idx)
                        && entry.is_dir
                    {
                        return ModalKeyOutcome::Action(UiAction::FilePickerDescend {
                            path: entry.path.clone(),
                        });
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Backspace if input.text().is_empty() => {
                    ModalKeyOutcome::Action(UiAction::FilePickerParent)
                }
                ModalKey::Enter | ModalKey::CtrlEnter => {
                    let chosen = filtered_indices
                        .get(*selected_row)
                        .and_then(|&idx| entries.get(idx))
                        .map(|e| e.path.clone())
                        .unwrap_or_else(|| current_dir.clone());
                    ModalKeyOutcome::Action(UiAction::FilePickerAccept { path: chosen })
                }
                other => {
                    if apply_text_key(input, other) {
                        *filtered_indices = filter_file_picker_entries(input.text(), entries);
                        *selected_row = 0;
                        *scroll_row = 0;
                    }
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::ThemePicker {
                input,
                items,
                matches,
                selected_row,
                scroll_row,
            } => match key {
                ModalKey::Escape => ModalKeyOutcome::Action(UiAction::ThemePickerCancel),
                ModalKey::Up => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + matches.len() - 1) % matches.len();
                        ensure_row_visible(*selected_row, scroll_row, 8);
                        if let Some(m) = matches.get(*selected_row) {
                            return ModalKeyOutcome::Action(UiAction::ThemePickerPreview {
                                name: items[m.index].id.clone(),
                            });
                        }
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + 1) % matches.len();
                        ensure_row_visible(*selected_row, scroll_row, 8);
                        if let Some(m) = matches.get(*selected_row) {
                            return ModalKeyOutcome::Action(UiAction::ThemePickerPreview {
                                name: items[m.index].id.clone(),
                            });
                        }
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageUp => {
                    *selected_row = selected_row.saturating_sub(6);
                    ensure_row_visible(*selected_row, scroll_row, 8);
                    if let Some(m) = matches.get(*selected_row) {
                        return ModalKeyOutcome::Action(UiAction::ThemePickerPreview {
                            name: items[m.index].id.clone(),
                        });
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageDown => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + 6).min(matches.len() - 1);
                        ensure_row_visible(*selected_row, scroll_row, 8);
                        if let Some(m) = matches.get(*selected_row) {
                            return ModalKeyOutcome::Action(UiAction::ThemePickerPreview {
                                name: items[m.index].id.clone(),
                            });
                        }
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Enter | ModalKey::CtrlEnter => {
                    if let Some(m) = matches.get(*selected_row) {
                        ModalKeyOutcome::Action(UiAction::ThemePickerCommit {
                            name: items[m.index].id.clone(),
                        })
                    } else {
                        ModalKeyOutcome::Action(UiAction::ThemePickerCancel)
                    }
                }
                other => {
                    if apply_text_key(input, other) {
                        *matches = self.matcher.filter(input.text(), items);
                        *selected_row = 0;
                        *scroll_row = 0;
                        if let Some(m) = matches.first() {
                            return ModalKeyOutcome::Action(UiAction::ThemePickerPreview {
                                name: items[m.index].id.clone(),
                            });
                        }
                    }
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::Diagnostics { scroll_row, .. } => match key {
                ModalKey::Escape | ModalKey::Enter => ModalKeyOutcome::Closed,
                ModalKey::Up => {
                    *scroll_row = scroll_row.saturating_sub(1);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down => {
                    *scroll_row = scroll_row.saturating_add(1);
                    ModalKeyOutcome::Consumed
                }
                _ => ModalKeyOutcome::Consumed,
            },
            ModalContent::RexPrompt {
                session,
                input,
                scroll_row,
            } => match key {
                ModalKey::Escape => ModalKeyOutcome::ReplyPrompt(session.cancel()),
                ModalKey::Up if session.kind == PromptKind::Pick => {
                    session.move_focus(-1);
                    ensure_row_visible(session.focused_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down if session.kind == PromptKind::Pick => {
                    session.move_focus(1);
                    ensure_row_visible(session.focused_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageUp if session.kind == PromptKind::Pick => {
                    session.move_focus(-6);
                    ensure_row_visible(session.focused_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageDown if session.kind == PromptKind::Pick => {
                    session.move_focus(6);
                    ensure_row_visible(session.focused_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Space if session.multiple => {
                    session.toggle_focused_selection();
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Enter => {
                    if let Some(reply) = session.submit_enter() {
                        ModalKeyOutcome::ReplyPrompt(reply)
                    } else {
                        ModalKeyOutcome::Consumed
                    }
                }
                ModalKey::CtrlEnter => {
                    if let Some(reply) = session.submit_confirm() {
                        ModalKeyOutcome::ReplyPrompt(reply)
                    } else {
                        ModalKeyOutcome::Consumed
                    }
                }
                other => {
                    if apply_text_key(input, other) {
                        session.set_input_text(input.text());
                        *scroll_row = 0;
                    }
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::Settings {
                snapshot,
                category,
                selected_row,
            } => {
                let row_count = settings_category_row_count(*category, snapshot).max(1);
                if *selected_row >= row_count {
                    *selected_row = row_count - 1;
                }
                match key {
                    ModalKey::Escape => {
                        if snapshot.server.confirm_stop {
                            snapshot.server.confirm_stop = false;
                            ModalKeyOutcome::Consumed
                        } else {
                            ModalKeyOutcome::Closed
                        }
                    }
                    ModalKey::Left { .. } => {
                        snapshot.server.confirm_stop = false;
                        *category = prev_settings_category(*category);
                        *selected_row = 0;
                        ModalKeyOutcome::Consumed
                    }
                    ModalKey::Right { .. } | ModalKey::Tab => {
                        snapshot.server.confirm_stop = false;
                        *category = next_settings_category(*category);
                        *selected_row = 0;
                        ModalKeyOutcome::Consumed
                    }
                    ModalKey::Up => {
                        snapshot.server.confirm_stop = false;
                        *selected_row = (*selected_row + row_count - 1) % row_count;
                        ModalKeyOutcome::Consumed
                    }
                    ModalKey::Down => {
                        snapshot.server.confirm_stop = false;
                        *selected_row = (*selected_row + 1) % row_count;
                        ModalKeyOutcome::Consumed
                    }
                    ModalKey::Enter | ModalKey::Space => {
                        if let Some(change) = cycle_setting_row(snapshot, *category, *selected_row)
                        {
                            dispatch_setting_change(snapshot, change)
                        } else {
                            ModalKeyOutcome::Consumed
                        }
                    }
                    _ => ModalKeyOutcome::Consumed,
                }
            }
            ModalContent::Rename {
                target_id, input, ..
            } => match key {
                ModalKey::Escape => ModalKeyOutcome::Closed,
                ModalKey::Enter | ModalKey::CtrlEnter => {
                    let trimmed = input.text().trim();
                    if trimmed.is_empty() {
                        ModalKeyOutcome::Closed
                    } else {
                        ModalKeyOutcome::Action(UiAction::SubmitRename {
                            target_id: target_id.clone(),
                            new_label: trimmed.to_owned(),
                        })
                    }
                }
                other => {
                    apply_text_key(input, other);
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::AddHost { input } => match key {
                ModalKey::Escape => ModalKeyOutcome::Closed,
                ModalKey::Enter | ModalKey::CtrlEnter => {
                    let endpoint = input.text().trim();
                    if endpoint.is_empty() {
                        ModalKeyOutcome::Closed
                    } else {
                        ModalKeyOutcome::Action(UiAction::SubmitAddHost {
                            endpoint: endpoint.to_owned(),
                        })
                    }
                }
                other => {
                    apply_text_key(input, other);
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::ShortcutsHelp { scroll_row } => match key {
                ModalKey::Escape | ModalKey::Enter => ModalKeyOutcome::Closed,
                ModalKey::Up => {
                    *scroll_row = scroll_row.saturating_sub(1);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down => {
                    *scroll_row = (*scroll_row + 1).min(SHORTCUT_HELP_ROWS.len().saturating_sub(1));
                    ModalKeyOutcome::Consumed
                }
                _ => ModalKeyOutcome::Consumed,
            },
        }
    }

    /// Activates a clicked row index inside the modal.
    pub fn activate_row(&mut self, row_index: usize) -> ModalKeyOutcome {
        match &mut self.content {
            ModalContent::List {
                items,
                matches,
                selected_row,
                ..
            } => {
                if let Some(m) = matches.get(row_index) {
                    *selected_row = row_index;
                    ModalKeyOutcome::Action(UiAction::RunCommand {
                        action_id: items[m.index].id.clone(),
                    })
                } else {
                    ModalKeyOutcome::Consumed
                }
            }
            ModalContent::FilePicker {
                entries,
                filtered_indices,
                selected_row,
                ..
            } => {
                if let Some(&entry_idx) = filtered_indices.get(row_index)
                    && let Some(entry) = entries.get(entry_idx)
                {
                    *selected_row = row_index;
                    if entry.is_dir {
                        ModalKeyOutcome::Action(UiAction::FilePickerDescend {
                            path: entry.path.clone(),
                        })
                    } else {
                        ModalKeyOutcome::Action(UiAction::FilePickerAccept {
                            path: entry.path.clone(),
                        })
                    }
                } else {
                    ModalKeyOutcome::Consumed
                }
            }
            ModalContent::ThemePicker {
                items,
                matches,
                selected_row,
                ..
            } => {
                if let Some(m) = matches.get(row_index) {
                    *selected_row = row_index;
                    ModalKeyOutcome::Action(UiAction::ThemePickerCommit {
                        name: items[m.index].id.clone(),
                    })
                } else {
                    ModalKeyOutcome::Consumed
                }
            }
            ModalContent::RexPrompt { session, .. } => {
                session.focused_row = row_index;
                if let Some(reply) = session.submit_enter() {
                    ModalKeyOutcome::ReplyPrompt(reply)
                } else {
                    ModalKeyOutcome::Consumed
                }
            }
            ModalContent::Settings {
                snapshot,
                category,
                selected_row,
            } => {
                *selected_row = row_index;
                if let Some(change) = cycle_setting_row(snapshot, *category, row_index) {
                    dispatch_setting_change(snapshot, change)
                } else {
                    ModalKeyOutcome::Consumed
                }
            }
            _ => ModalKeyOutcome::Consumed,
        }
    }

    /// Toggles a multi-select `rex.ui.pick` checkbox row without submitting the modal.
    pub fn toggle_checkbox_row(&mut self, row_index: usize) -> ModalKeyOutcome {
        if let ModalContent::RexPrompt { session, .. } = &mut self.content
            && session.multiple
        {
            session.focused_row = row_index;
            session.toggle_focused_selection();
            return ModalKeyOutcome::Consumed;
        }
        self.activate_row(row_index)
    }

    /// Activates a Settings row by its stable key (called when clicking `HitTarget::SettingsToggle`).
    pub fn activate_setting_key(&mut self, key: &str) -> ModalKeyOutcome {
        if let ModalContent::Settings {
            snapshot,
            category,
            selected_row,
        } = &mut self.content
        {
            let Some((cat, row_idx, change)) = setting_change_for_key(snapshot, key) else {
                return ModalKeyOutcome::Consumed;
            };
            *category = cat;
            *selected_row = row_idx;
            return dispatch_setting_change(snapshot, change);
        }
        ModalKeyOutcome::Consumed
    }

    /// Switches the active Settings category tab.
    pub fn select_settings_category(&mut self, next: SettingsCategory) -> bool {
        if let ModalContent::Settings {
            snapshot,
            category,
            selected_row,
        } = &mut self.content
        {
            let changed = *category != next;
            snapshot.server.confirm_stop = false;
            *category = next;
            *selected_row = 0;
            return changed;
        }
        false
    }

    /// Scrolls a scrollable modal list by `delta_rows` (`> 0` scrolls down, `< 0` scrolls up).
    pub fn scroll_by(&mut self, delta_rows: isize) -> bool {
        match &mut self.content {
            ModalContent::List {
                matches,
                scroll_row,
                ..
            }
            | ModalContent::ThemePicker {
                matches,
                scroll_row,
                ..
            } => {
                let max_scroll = matches.len().saturating_sub(8);
                let next =
                    (*scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
                let changed = next != *scroll_row;
                *scroll_row = next;
                changed
            }
            ModalContent::FilePicker {
                filtered_indices,
                scroll_row,
                ..
            } => {
                let max_scroll = filtered_indices.len().saturating_sub(8);
                let next =
                    (*scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
                let changed = next != *scroll_row;
                *scroll_row = next;
                changed
            }
            ModalContent::Diagnostics { scroll_row, .. } => {
                let max_scroll = 12usize;
                let next =
                    (*scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
                let changed = next != *scroll_row;
                *scroll_row = next;
                changed
            }
            ModalContent::RexPrompt {
                session,
                scroll_row,
                ..
            } => {
                let max_scroll = session.visible_row_count().saturating_sub(8);
                let next =
                    (*scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
                let changed = next != *scroll_row;
                *scroll_row = next;
                changed
            }
            ModalContent::ShortcutsHelp { scroll_row } => {
                let max_scroll = SHORTCUT_HELP_ROWS.len().saturating_sub(10);
                let next =
                    (*scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
                let changed = next != *scroll_row;
                *scroll_row = next;
                changed
            }
            _ => false,
        }
    }
}

fn dispatch_setting_change(
    snapshot: &mut SettingsViewSnapshot,
    change: SettingChange,
) -> ModalKeyOutcome {
    if change == SettingChange::ServerStop {
        if !snapshot.server.confirm_stop {
            snapshot.server.confirm_stop = true;
            return ModalKeyOutcome::Consumed;
        }
        snapshot.server.confirm_stop = false;
    } else {
        snapshot.server.confirm_stop = false;
    }
    apply_setting_to_snapshot(snapshot, &change);
    ModalKeyOutcome::Action(UiAction::UpdateSetting(change))
}

fn apply_setting_to_snapshot(snapshot: &mut SettingsViewSnapshot, change: &SettingChange) {
    match *change {
        SettingChange::ToolbarPosition(pos) => snapshot.toolbar_position = pos,
        SettingChange::ToolbarBehavior(beh) => snapshot.toolbar_behavior = beh,
        SettingChange::PaneDensity(den) => snapshot.density = den,
        SettingChange::PaneHeaderMode(mode) => snapshot.pane_header_mode = mode,
        SettingChange::ToggleTranslucentMaterial => {
            snapshot.material = match snapshot.material {
                MaterialStyle::Classic => MaterialStyle::Translucent { alpha: 0.90 },
                MaterialStyle::Translucent { .. } => MaterialStyle::Classic,
            };
        }
        SettingChange::ToggleActivePaneBorder => {
            snapshot.highlight_active_border = !snapshot.highlight_active_border;
        }
        SettingChange::ToggleFadeInactivePanes => {
            snapshot.fade_inactive_panes = !snapshot.fade_inactive_panes;
        }
        SettingChange::ToggleAdaptiveRounding => {
            snapshot.adaptive_rounding = !snapshot.adaptive_rounding;
        }
        SettingChange::ToggleProgramColorHeader => {
            snapshot.program_color_header = !snapshot.program_color_header;
        }
        SettingChange::ToggleChimeEnabled => {
            snapshot.chime_enabled = !snapshot.chime_enabled;
        }
        SettingChange::ToggleDesktopNotifications => {
            snapshot.desktop_notifications = !snapshot.desktop_notifications;
        }
        SettingChange::FontSizePt(pt) => {
            snapshot.font_size_pt = pt;
        }
        SettingChange::OpenThemePicker | SettingChange::IosCopyUrl => {}
        SettingChange::IosToggleTailscale
        | SettingChange::IosToggleAllowTailnetMembers
        | SettingChange::IosRefreshStatus => {
            snapshot.ios_pairing.busy = true;
            snapshot.ios_pairing.last_error = None;
        }
        SettingChange::IosOpenLoginUrl => {
            if matches!(
                snapshot.ios_pairing.state,
                TailscalePairState::NeedsLogin { auth_url: None }
            ) {
                snapshot.ios_pairing.busy = true;
                snapshot.ios_pairing.last_error = None;
            }
        }
        SettingChange::ServerStart => {
            snapshot.server.busy_action = Some("Starting…".to_owned());
            snapshot.server.last_error = None;
        }
        SettingChange::ServerStop => {
            snapshot.server.busy_action = Some("Stopping…".to_owned());
            snapshot.server.last_error = None;
        }
        SettingChange::ServerRestart => {
            snapshot.server.busy_action = Some("Restarting…".to_owned());
            snapshot.server.last_error = None;
        }
        SettingChange::ServerReloadConfig => {
            snapshot.server.busy_action = Some("Reloading…".to_owned());
            snapshot.server.last_error = None;
        }
        SettingChange::ServerRefreshStatus => {
            snapshot.server.busy_action = Some("Refreshing…".to_owned());
            snapshot.server.last_error = None;
        }
        SettingChange::ServerReconnect => {
            snapshot.server.busy_action = Some("Connecting…".to_owned());
            snapshot.server.last_error = None;
        }
    }
}

fn filter_file_picker_entries(query: &str, entries: &[FilePickerEntry]) -> Vec<usize> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return (0..entries.len()).collect();
    }
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.name.to_ascii_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

const SETTINGS_CATEGORIES: [(SettingsCategory, &str); 6] = [
    (SettingsCategory::Workspace, "Workspace"),
    (SettingsCategory::Appearance, "Appearance"),
    (SettingsCategory::Terminal, "Terminal"),
    (SettingsCategory::AudioAlerts, "Audio & Alerts"),
    (SettingsCategory::Server, "Server"),
    (SettingsCategory::ConnectIos, "Connect iOS"),
];

fn next_settings_category(cat: SettingsCategory) -> SettingsCategory {
    match cat {
        SettingsCategory::Workspace => SettingsCategory::Appearance,
        SettingsCategory::Appearance => SettingsCategory::Terminal,
        SettingsCategory::Terminal => SettingsCategory::AudioAlerts,
        SettingsCategory::AudioAlerts => SettingsCategory::Server,
        SettingsCategory::Server => SettingsCategory::ConnectIos,
        SettingsCategory::ConnectIos => SettingsCategory::Workspace,
    }
}

fn prev_settings_category(cat: SettingsCategory) -> SettingsCategory {
    match cat {
        SettingsCategory::Workspace => SettingsCategory::ConnectIos,
        SettingsCategory::Appearance => SettingsCategory::Workspace,
        SettingsCategory::Terminal => SettingsCategory::Appearance,
        SettingsCategory::AudioAlerts => SettingsCategory::Terminal,
        SettingsCategory::Server => SettingsCategory::AudioAlerts,
        SettingsCategory::ConnectIos => SettingsCategory::Server,
    }
}

fn server_action_rows(srv: &ServerSettingsSnapshot) -> Vec<(&'static str, &'static str, String)> {
    let busy = srv.busy_action.as_deref();
    match &srv.state {
        LocalServerState::Running => vec![
            (
                "server_restart",
                "Restart Rex Server (Resumes OSC 7577 Programs)",
                busy.unwrap_or("Restart").to_owned(),
            ),
            (
                "server_stop",
                if srv.confirm_stop {
                    "Confirm Stop Local Rex Server"
                } else {
                    "Stop Local Rex Server"
                },
                if let Some(b) = busy {
                    b.to_owned()
                } else if srv.confirm_stop {
                    "Confirm Stop".to_owned()
                } else {
                    "Stop Server".to_owned()
                },
            ),
            (
                "server_reload_config",
                "Reload Rex Config (init.lua & Resume Rules)",
                busy.unwrap_or("Reload").to_owned(),
            ),
            (
                "server_refresh_status",
                "Local Server Status",
                busy.unwrap_or("Refresh").to_owned(),
            ),
        ],
        LocalServerState::Reconnecting { .. } => vec![
            (
                "server_start",
                "Start Local Rex Server",
                busy.unwrap_or("Start Server").to_owned(),
            ),
            (
                "server_reconnect",
                "Reconnect Local Socket",
                busy.unwrap_or("Reconnect").to_owned(),
            ),
        ],
        LocalServerState::Stopped | LocalServerState::Error(_) => vec![
            (
                "server_start",
                "Start Local Rex Server",
                busy.unwrap_or("Start Server").to_owned(),
            ),
            (
                "server_reconnect",
                "Reconnect Local Socket",
                busy.unwrap_or("Reconnect").to_owned(),
            ),
        ],
    }
}

fn ios_action_rows(ios: &IosPairingSnapshot) -> Vec<(&'static str, &'static str, String)> {
    match &ios.state {
        TailscalePairState::Ready { .. } => vec![
            ("ios_copy_url", "Tailscale Pair URL", "Copy URL".to_owned()),
            (
                "ios_allow_tailnet_members",
                "Allow Tailnet Members",
                if ios.busy {
                    "Updating…".to_owned()
                } else if ios.allow_tailnet_members {
                    "On (Tailnet)".to_owned()
                } else {
                    "Owner Only".to_owned()
                },
            ),
            (
                "ios_toggle_tailscale",
                "Tailscale Remote Access",
                if ios.busy {
                    "Working…".to_owned()
                } else {
                    "On · Turn Off".to_owned()
                },
            ),
        ],
        TailscalePairState::Disabled { .. } => vec![
            (
                "ios_toggle_tailscale",
                "Tailscale Remote Access",
                if ios.busy {
                    "Starting…".to_owned()
                } else {
                    "Enable Tailscale".to_owned()
                },
            ),
            (
                "ios_refresh_status",
                "Server Tailscale Status",
                if ios.busy {
                    "Checking…".to_owned()
                } else {
                    "Refresh".to_owned()
                },
            ),
        ],
        TailscalePairState::NeedsLogin { auth_url } => {
            let mut rows = vec![(
                "ios_open_login_url",
                "Tailscale Sign-In",
                if ios.busy {
                    "Requesting…".to_owned()
                } else if auth_url.is_some() {
                    "Open Sign-In".to_owned()
                } else {
                    "Get Sign-In Link".to_owned()
                },
            )];
            if auth_url.is_some() {
                rows.push(("ios_copy_url", "Sign-In URL", "Copy Link".to_owned()));
            } else {
                rows.push((
                    "ios_refresh_status",
                    "Server Tailscale Status",
                    if ios.busy {
                        "Checking…".to_owned()
                    } else {
                        "Refresh".to_owned()
                    },
                ));
            }
            rows.push((
                "ios_toggle_tailscale",
                "Tailscale Remote Access",
                if ios.busy {
                    "Working…".to_owned()
                } else {
                    "Turn Off".to_owned()
                },
            ));
            rows
        }
        TailscalePairState::Starting
        | TailscalePairState::NeedsMachineApproval
        | TailscalePairState::HttpsPending { .. } => vec![
            (
                "ios_refresh_status",
                "Server Tailscale Status",
                if ios.busy {
                    "Checking…".to_owned()
                } else {
                    "Refresh".to_owned()
                },
            ),
            (
                "ios_toggle_tailscale",
                "Tailscale Remote Access",
                if ios.busy {
                    "Working…".to_owned()
                } else {
                    "Turn Off".to_owned()
                },
            ),
        ],
        TailscalePairState::Unsupported | TailscalePairState::ServerDisconnected => vec![(
            "ios_refresh_status",
            "Server Tailscale Status",
            if ios.busy {
                "Checking…".to_owned()
            } else {
                "Refresh".to_owned()
            },
        )],
    }
}

fn settings_category_row_count(cat: SettingsCategory, snapshot: &SettingsViewSnapshot) -> usize {
    match cat {
        SettingsCategory::Workspace => 4,
        SettingsCategory::Appearance => 6,
        SettingsCategory::Terminal => 2,
        SettingsCategory::AudioAlerts => 2,
        SettingsCategory::Server => server_action_rows(&snapshot.server).len(),
        SettingsCategory::ConnectIos => ios_action_rows(&snapshot.ios_pairing).len(),
    }
}

fn setting_change_for_key(
    s: &SettingsViewSnapshot,
    key: &str,
) -> Option<(SettingsCategory, usize, SettingChange)> {
    match key {
        "toolbar_position" => Some((
            SettingsCategory::Workspace,
            0,
            SettingChange::ToolbarPosition(match s.toolbar_position {
                ToolbarPosition::Left => ToolbarPosition::Right,
                ToolbarPosition::Right => ToolbarPosition::Top,
                ToolbarPosition::Top => ToolbarPosition::Bottom,
                ToolbarPosition::Bottom => ToolbarPosition::Left,
            }),
        )),
        "toolbar_behavior" => Some((
            SettingsCategory::Workspace,
            1,
            SettingChange::ToolbarBehavior(match s.toolbar_behavior {
                ToolbarBehavior::Always => ToolbarBehavior::OnHover,
                ToolbarBehavior::OnHover => ToolbarBehavior::Hidden,
                ToolbarBehavior::Hidden => ToolbarBehavior::Always,
            }),
        )),
        "pane_density" => Some((
            SettingsCategory::Workspace,
            2,
            SettingChange::PaneDensity(match s.density {
                PaneDensity::Comfortable => PaneDensity::Compact,
                PaneDensity::Compact => PaneDensity::Comfortable,
            }),
        )),
        "pane_headers" => Some((
            SettingsCategory::Workspace,
            3,
            SettingChange::PaneHeaderMode(match s.pane_header_mode {
                PaneHeaderMode::OnHover => PaneHeaderMode::Always,
                PaneHeaderMode::Always => PaneHeaderMode::Hidden,
                PaneHeaderMode::Hidden => PaneHeaderMode::OnHover,
            }),
        )),
        "theme_name" => Some((
            SettingsCategory::Appearance,
            0,
            SettingChange::OpenThemePicker,
        )),
        "material" => Some((
            SettingsCategory::Appearance,
            1,
            SettingChange::ToggleTranslucentMaterial,
        )),
        "active_border" => Some((
            SettingsCategory::Appearance,
            2,
            SettingChange::ToggleActivePaneBorder,
        )),
        "fade_inactive" => Some((
            SettingsCategory::Appearance,
            3,
            SettingChange::ToggleFadeInactivePanes,
        )),
        "adaptive_rounding" => Some((
            SettingsCategory::Appearance,
            4,
            SettingChange::ToggleAdaptiveRounding,
        )),
        "program_color_header" => Some((
            SettingsCategory::Appearance,
            5,
            SettingChange::ToggleProgramColorHeader,
        )),
        "font_size" => {
            const PRESETS: [f32; 8] = [9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 16.0, 18.0];
            let next = PRESETS
                .iter()
                .copied()
                .find(|&pt| pt > s.font_size_pt + 0.05)
                .unwrap_or(PRESETS[0]);
            Some((
                SettingsCategory::Terminal,
                1,
                SettingChange::FontSizePt(next),
            ))
        }
        "chime_enabled" => Some((
            SettingsCategory::AudioAlerts,
            0,
            SettingChange::ToggleChimeEnabled,
        )),
        "desktop_notifications" => Some((
            SettingsCategory::AudioAlerts,
            1,
            SettingChange::ToggleDesktopNotifications,
        )),
        "ios_copy_url" => {
            let row_idx = ios_action_rows(&s.ios_pairing)
                .iter()
                .position(|(k, _, _)| *k == "ios_copy_url")
                .unwrap_or(0);
            Some((
                SettingsCategory::ConnectIos,
                row_idx,
                SettingChange::IosCopyUrl,
            ))
        }
        "ios_allow_tailnet_members" => {
            let row_idx = ios_action_rows(&s.ios_pairing)
                .iter()
                .position(|(k, _, _)| *k == "ios_allow_tailnet_members")
                .unwrap_or(1);
            Some((
                SettingsCategory::ConnectIos,
                row_idx,
                SettingChange::IosToggleAllowTailnetMembers,
            ))
        }
        "ios_toggle_tailscale" => {
            let row_idx = ios_action_rows(&s.ios_pairing)
                .iter()
                .position(|(k, _, _)| *k == "ios_toggle_tailscale")
                .unwrap_or(0);
            Some((
                SettingsCategory::ConnectIos,
                row_idx,
                SettingChange::IosToggleTailscale,
            ))
        }
        "ios_open_login_url" => {
            let row_idx = ios_action_rows(&s.ios_pairing)
                .iter()
                .position(|(k, _, _)| *k == "ios_open_login_url")
                .unwrap_or(0);
            Some((
                SettingsCategory::ConnectIos,
                row_idx,
                SettingChange::IosOpenLoginUrl,
            ))
        }
        "ios_refresh_status" => {
            let row_idx = ios_action_rows(&s.ios_pairing)
                .iter()
                .position(|(k, _, _)| *k == "ios_refresh_status")
                .unwrap_or(0);
            Some((
                SettingsCategory::ConnectIos,
                row_idx,
                SettingChange::IosRefreshStatus,
            ))
        }
        "server_start" => {
            let row_idx = server_action_rows(&s.server)
                .iter()
                .position(|(k, _, _)| *k == "server_start")
                .unwrap_or(0);
            Some((
                SettingsCategory::Server,
                row_idx,
                SettingChange::ServerStart,
            ))
        }
        "server_stop" => {
            let row_idx = server_action_rows(&s.server)
                .iter()
                .position(|(k, _, _)| *k == "server_stop")
                .unwrap_or(1);
            Some((SettingsCategory::Server, row_idx, SettingChange::ServerStop))
        }
        "server_restart" => {
            let row_idx = server_action_rows(&s.server)
                .iter()
                .position(|(k, _, _)| *k == "server_restart")
                .unwrap_or(0);
            Some((
                SettingsCategory::Server,
                row_idx,
                SettingChange::ServerRestart,
            ))
        }
        "server_reload_config" => {
            let row_idx = server_action_rows(&s.server)
                .iter()
                .position(|(k, _, _)| *k == "server_reload_config")
                .unwrap_or(2);
            Some((
                SettingsCategory::Server,
                row_idx,
                SettingChange::ServerReloadConfig,
            ))
        }
        "server_refresh_status" => {
            let row_idx = server_action_rows(&s.server)
                .iter()
                .position(|(k, _, _)| *k == "server_refresh_status")
                .unwrap_or(3);
            Some((
                SettingsCategory::Server,
                row_idx,
                SettingChange::ServerRefreshStatus,
            ))
        }
        "server_reconnect" => {
            let row_idx = server_action_rows(&s.server)
                .iter()
                .position(|(k, _, _)| *k == "server_reconnect")
                .unwrap_or(1);
            Some((
                SettingsCategory::Server,
                row_idx,
                SettingChange::ServerReconnect,
            ))
        }
        _ => None,
    }
}

fn ensure_row_visible(row: usize, scroll_row: &mut usize, visible_rows: usize) {
    if row < *scroll_row {
        *scroll_row = row;
    } else if row >= *scroll_row + visible_rows {
        *scroll_row = row + 1 - visible_rows;
    }
}

fn apply_text_key(input: &mut TextInput, key: ModalKey) -> bool {
    match key {
        ModalKey::Space => {
            input.insert_char(' ');
            true
        }
        ModalKey::Char(c) => {
            input.insert_char(c);
            true
        }
        ModalKey::Paste(s) => {
            input.paste(&s);
            true
        }
        ModalKey::Backspace => input.backspace(),
        ModalKey::Delete => input.delete_forward(),
        ModalKey::WordBackspace => input.delete_word_backward(),
        ModalKey::Left { extend } => {
            input.move_left(extend);
            false
        }
        ModalKey::Right { extend } => {
            input.move_right(extend);
            false
        }
        ModalKey::Home { extend } => {
            input.move_home(extend);
            false
        }
        ModalKey::End { extend } => {
            input.move_end(extend);
            false
        }
        ModalKey::SelectAll => {
            input.select_all();
            false
        }
        _ => false,
    }
}

fn cycle_setting_row(
    s: &SettingsViewSnapshot,
    category: SettingsCategory,
    row: usize,
) -> Option<SettingChange> {
    if category == SettingsCategory::Server {
        let rows = server_action_rows(&s.server);
        let (key, _, _) = rows.get(row)?;
        return setting_change_for_key(s, key).map(|(_, _, ch)| ch);
    }
    if category == SettingsCategory::ConnectIos {
        let rows = ios_action_rows(&s.ios_pairing);
        let (key, _, _) = rows.get(row)?;
        return setting_change_for_key(s, key).map(|(_, _, ch)| ch);
    }
    let key = match (category, row) {
        (SettingsCategory::Workspace, 0) => "toolbar_position",
        (SettingsCategory::Workspace, 1) => "toolbar_behavior",
        (SettingsCategory::Workspace, 2) => "pane_density",
        (SettingsCategory::Workspace, 3) => "pane_headers",
        (SettingsCategory::Appearance, 0) => "theme_name",
        (SettingsCategory::Appearance, 1) => "material",
        (SettingsCategory::Appearance, 2) => "active_border",
        (SettingsCategory::Appearance, 3) => "fade_inactive",
        (SettingsCategory::Appearance, 4) => "adaptive_rounding",
        (SettingsCategory::Appearance, 5) => "program_color_header",
        (SettingsCategory::Terminal, 1) => "font_size",
        (SettingsCategory::AudioAlerts, 0) => "chime_enabled",
        (SettingsCategory::AudioAlerts, 1) => "desktop_notifications",
        _ => return None,
    };
    setting_change_for_key(s, key).map(|(_, _, ch)| ch)
}

/// Renders the centered modal (`460–620px`) over a dimming backdrop scrim.
pub fn render_modal(
    draw_list: &mut UiDrawList,
    viewport: RectF,
    modal: &ModalState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    now: Instant,
) {
    if viewport.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();

    // Backdrop scrim
    draw_list.push_rect(viewport, tokens.surface_scrim, None, 0.0, 0.0, None);
    draw_list.push_hit(viewport, HitTarget::ModalBackdrop, z_layer::MODAL_BACKDROP);

    let modal_w = (viewport.w * 0.52)
        .clamp(460.0, 620.0)
        .min((viewport.w - 32.0).max(200.0));
    let max_h = (viewport.h * 0.65).clamp(160.0, 460.0);
    let top_y =
        (viewport.y + viewport.h * 0.16).min((viewport.bottom() - 180.0).max(viewport.y + 16.0));
    let left_x = viewport.x + (viewport.w - modal_w) * 0.5;
    let card_r = cap_radius(12.0, theme.rounding);

    let shadow = ShadowSpec {
        offset_x: 0.0,
        offset_y: 20.0,
        blur_radius: 56.0,
        spread: 0.0,
        color: tokens.shadow_color,
    };

    match &modal.content {
        ModalContent::List {
            title,
            shortcut_badge,
            input,
            items,
            matches,
            selected_row,
            scroll_row,
            ..
        } => {
            let visible_count = matches.len().min(8);
            let list_h = if matches.is_empty() {
                54.0
            } else {
                visible_count as f32 * 44.0 + 12.0
            };
            let card_h = (46.0 + list_h).min(max_h);
            let card = RectF::new(
                left_x.round(),
                top_y.round(),
                modal_w.round(),
                card_h.round(),
            );

            draw_list.push_rect(
                card,
                tokens.surface_modal,
                Some(tokens.border_modal),
                1.0,
                card_r,
                Some(shadow),
            );
            render_modal_search_header(
                draw_list,
                RectF::new(card.x, card.y, card.w, 46.0),
                input,
                shortcut_badge.as_deref().unwrap_or(title.as_str()),
                &tokens,
                theme.rounding,
            );

            let list_rect = RectF::new(card.x, card.y + 46.0, card.w, (card.h - 46.0).max(0.0));
            draw_list.push_clip(list_rect);

            if matches.is_empty() {
                draw_list.push_text(
                    RectF::new(
                        list_rect.x + 16.0,
                        list_rect.y + 16.0,
                        (list_rect.w - 32.0).max(0.0),
                        20.0,
                    ),
                    "No matching items",
                    tokens.fg_muted,
                    FontKind::Sans,
                    12.5,
                    FontWeight::Regular,
                    TextAlign::Center,
                    true,
                );
            } else {
                let mut y = list_rect.y + 6.0;
                let row_r = cap_radius(7.0, theme.rounding);
                for (vis_idx, m) in matches.iter().enumerate().skip(*scroll_row).take(8) {
                    let item = &items[m.index];
                    let row_rect =
                        RectF::new(list_rect.x + 6.0, y, (list_rect.w - 12.0).max(0.0), 42.0);
                    let row_target = HitTarget::ModalRow { index: vis_idx };
                    let is_sel = vis_idx == *selected_row;
                    let is_hov = interaction.is_hovered(&row_target);
                    if is_sel {
                        draw_list.push_rect(
                            row_rect,
                            tokens.surface_row_selected,
                            Some(tokens.border_subtle),
                            1.0,
                            row_r,
                            None,
                        );
                    } else if is_hov {
                        draw_list.push_rect(
                            row_rect,
                            tokens.surface_row_hover,
                            Some(tokens.border_hairline),
                            1.0,
                            row_r,
                            None,
                        );
                    }

                    let mut tx = row_rect.x + 10.0;
                    if let Some(icon) = item.icon {
                        draw_list.push_icon(
                            RectF::new(tx, row_rect.y + 13.0, 15.0, 15.0),
                            icon,
                            if is_sel {
                                tokens.accent_primary
                            } else {
                                tokens.fg_secondary
                            },
                        );
                        tx += 24.0;
                    }

                    let right_text = item.shortcut.as_deref().or(item.detail.as_deref());
                    let right_w = right_text
                        .map_or(0.0, |t| (t.chars().count() as f32 * 6.8 + 16.0).min(130.0));
                    let text_w = (row_rect.right() - tx - right_w - 10.0).max(0.0);

                    let title_y = if item.subtitle.is_some() {
                        row_rect.y + 5.0
                    } else {
                        row_rect.y + 12.0
                    };
                    let hi_ranges: Vec<_> = highlight_spans(&item.title, &m.title_indices)
                        .into_iter()
                        .filter_map(|(r, hi)| hi.then_some(r))
                        .collect();
                    draw_list.push_highlighted_text(
                        RectF::new(tx, title_y, text_w, 17.0),
                        &item.title,
                        if is_sel {
                            tokens.fg_primary
                        } else {
                            tokens.fg_secondary
                        },
                        FontKind::Sans,
                        13.0,
                        FontWeight::Medium,
                        TextAlign::Left,
                        true,
                        hi_ranges,
                        tokens.accent_primary,
                    );

                    if let Some(sub) = &item.subtitle {
                        let sub_hi: Vec<_> = highlight_spans(sub, &m.subtitle_indices)
                            .into_iter()
                            .filter_map(|(r, hi)| hi.then_some(r))
                            .collect();
                        draw_list.push_highlighted_text(
                            RectF::new(tx, row_rect.y + 22.0, text_w, 15.0),
                            sub,
                            tokens.fg_muted,
                            FontKind::Sans,
                            11.5,
                            FontWeight::Regular,
                            TextAlign::Left,
                            true,
                            sub_hi,
                            tokens.accent_primary,
                        );
                    }

                    if let Some(badge) = right_text {
                        let badge_rect = RectF::new(
                            (row_rect.right() - right_w - 4.0).max(tx),
                            row_rect.y + 11.0,
                            right_w,
                            20.0,
                        );
                        if item.shortcut.is_some() {
                            draw_list.push_rect(
                                badge_rect,
                                tokens.surface_pane,
                                Some(tokens.border_hairline),
                                1.0,
                                cap_radius(5.0, theme.rounding),
                                None,
                            );
                        }
                        draw_list.push_text(
                            badge_rect,
                            badge,
                            tokens.fg_muted,
                            FontKind::Mono,
                            10.5,
                            FontWeight::Regular,
                            if item.shortcut.is_some() {
                                TextAlign::Center
                            } else {
                                TextAlign::Right
                            },
                            true,
                        );
                    }

                    draw_list.push_hit(
                        row_rect,
                        HitTarget::ModalRow { index: vis_idx },
                        z_layer::MODAL + 1,
                    );
                    y += 44.0;
                }
                render_modal_scrollbar(
                    draw_list,
                    list_rect,
                    *scroll_row,
                    8,
                    matches.len(),
                    &tokens,
                );
            }
            draw_list.pop_clip();
        }
        ModalContent::RexPrompt {
            session,
            input,
            scroll_row,
        } => {
            render_rex_prompt_modal(
                draw_list,
                left_x,
                top_y,
                modal_w,
                max_h,
                card_r,
                shadow,
                session,
                input,
                *scroll_row,
                interaction,
                &tokens,
                theme.rounding,
                now,
            );
        }
        ModalContent::FilePicker {
            purpose,
            current_dir,
            remote_host,
            loading,
            error,
            input,
            entries,
            filtered_indices,
            selected_row,
            scroll_row,
        } => {
            render_file_picker_modal(
                draw_list,
                left_x,
                top_y,
                modal_w,
                max_h,
                card_r,
                shadow,
                *purpose,
                current_dir,
                remote_host.as_deref(),
                *loading,
                error.as_deref(),
                input,
                entries,
                filtered_indices,
                *selected_row,
                *scroll_row,
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::ThemePicker {
            input,
            items,
            matches,
            selected_row,
            scroll_row,
        } => {
            render_theme_picker_modal(
                draw_list,
                left_x,
                top_y,
                modal_w,
                max_h,
                card_r,
                shadow,
                input,
                items,
                matches,
                *selected_row,
                *scroll_row,
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::Diagnostics {
            snapshot,
            scroll_row,
        } => {
            render_diagnostics_modal(
                draw_list,
                left_x,
                top_y,
                (viewport.w * 0.64)
                    .clamp(520.0, 740.0)
                    .min((viewport.w - 32.0).max(240.0)),
                (viewport.h * 0.78).clamp(320.0, 560.0),
                viewport,
                card_r,
                shadow,
                snapshot,
                *scroll_row,
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::Settings {
            snapshot,
            category,
            selected_row,
        } => {
            render_settings_modal(
                draw_list,
                left_x,
                top_y,
                modal_w,
                viewport,
                card_r,
                shadow,
                snapshot,
                *category,
                *selected_row,
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::Rename { title, input, .. } => {
            render_single_input_modal(
                draw_list,
                left_x,
                top_y,
                modal_w.min(480.0),
                card_r,
                shadow,
                title,
                input,
                "Rename",
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::AddHost { input } => {
            render_single_input_modal(
                draw_list,
                left_x,
                top_y,
                modal_w.min(520.0),
                card_r,
                shadow,
                "Add Remote Rex Host",
                input,
                "Connect",
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::ShortcutsHelp { scroll_row } => {
            render_shortcuts_help_modal(
                draw_list,
                left_x,
                top_y,
                modal_w,
                max_h,
                card_r,
                shadow,
                *scroll_row,
                &tokens,
                theme.rounding,
            );
        }
    }
}

fn render_modal_scrollbar(
    draw_list: &mut UiDrawList,
    list_rect: RectF,
    scroll_row: usize,
    visible_rows: usize,
    total_rows: usize,
    tokens: &UiTokens,
) {
    if total_rows <= visible_rows || list_rect.h < 24.0 {
        return;
    }
    let track_h = (list_rect.h - 12.0).max(16.0);
    let min_thumb = 20.0_f32.min(track_h);
    let thumb_h = ((visible_rows as f32 / total_rows as f32) * track_h).clamp(min_thumb, track_h);
    let max_scroll = total_rows.saturating_sub(visible_rows).max(1) as f32;
    let frac = (scroll_row as f32 / max_scroll).clamp(0.0, 1.0);
    let thumb_y = list_rect.y + 6.0 + frac * (track_h - thumb_h);
    draw_list.push_rect(
        RectF::new(
            (list_rect.right() - 5.0).max(list_rect.x),
            thumb_y,
            3.0,
            thumb_h,
        ),
        tokens.fg_muted.with_alpha_f32(0.38),
        None,
        0.0,
        1.5,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_rex_prompt_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    max_h: f32,
    card_r: f32,
    shadow: ShadowSpec,
    session: &PromptSession,
    input: &TextInput,
    scroll_row: usize,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    now: Instant,
) {
    let header_h = 74.0; // timeout bar (2px) + title breadcrumb (26px) + search/input bar (46px)
    let footer_h = if session.multiple { 42.0 } else { 0.0 };
    let body_h = match session.kind {
        PromptKind::Input => 48.0,
        PromptKind::Pick => {
            let rows = session.visible_row_count().min(8);
            if rows == 0 {
                52.0
            } else {
                rows as f32 * 44.0 + 12.0
            }
        }
    };
    let card_h = (header_h + body_h + footer_h).min(max_h);
    let card = RectF::new(
        left_x.round(),
        top_y.round(),
        modal_w.round(),
        card_h.round(),
    );

    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    // Timeout countdown progress bar along top edge of `rex.ui` modal
    let remaining_frac = if session.timeout_ms > 0 {
        (session.remaining_ms(now) as f32 / session.timeout_ms as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let bar_color = if remaining_frac <= 0.15 {
        tokens.status_error
    } else if remaining_frac <= 0.30 {
        tokens.status_blocked
    } else {
        tokens.accent_primary
    };
    draw_list.push_progress(
        RectF::new(card.x + 1.0, card.y + 1.0, (card.w - 2.0).max(0.0), 2.5),
        remaining_frac,
        bar_color,
        tokens.border_hairline,
        0.0,
    );

    // Title breadcrumb row (`26px`)
    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 8.0, (card.w - 72.0).max(0.0), 18.0),
        &session.title,
        tokens.accent_primary,
        FontKind::Sans,
        11.5,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    draw_list.push_text(
        RectF::new(card.right() - 56.0, card.y + 8.0, 42.0, 18.0),
        "Esc",
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );

    // Search / text input row (`46px`)
    render_modal_search_header(
        draw_list,
        RectF::new(card.x, card.y + 28.0, card.w, 46.0),
        input,
        if session.kind == PromptKind::Input {
            "↵"
        } else {
            &session.confirm_label
        },
        tokens,
        theme_rounding,
    );

    let body_rect = RectF::new(
        card.x,
        card.y + header_h,
        card.w,
        (card.h - header_h - footer_h).max(0.0),
    );
    draw_list.push_clip(body_rect);
    let row_r = cap_radius(7.0, theme_rounding);

    match session.kind {
        PromptKind::Input => {
            let submit_rect = RectF::new(
                body_rect.x + 6.0,
                body_rect.y + 6.0,
                (body_rect.w - 12.0).max(0.0),
                36.0,
            );
            draw_list.push_rect(
                submit_rect,
                tokens.surface_row_selected,
                Some(tokens.border_subtle),
                1.0,
                row_r,
                None,
            );
            let label = if session.confirm_label == "OK" && !session.input_text.is_empty() {
                format!("Use “{}”", session.input_text)
            } else {
                session.confirm_label.clone()
            };
            draw_list.push_text(
                RectF::new(
                    submit_rect.x + 12.0,
                    submit_rect.y + 9.0,
                    (submit_rect.w - 50.0).max(0.0),
                    18.0,
                ),
                label,
                tokens.fg_primary,
                FontKind::Sans,
                13.0,
                FontWeight::Medium,
                TextAlign::Left,
                true,
            );
            draw_list.push_hit(
                submit_rect,
                HitTarget::ModalConfirmButton,
                z_layer::MODAL + 1,
            );
        }
        PromptKind::Pick => {
            let rows = session.filtered_rows();
            let mut y = body_rect.y + 6.0;
            let mut rendered = 0usize;

            for (vis_idx, row) in rows.iter().enumerate().skip(scroll_row).take(8) {
                let item = &session.items[row.original_index];
                let row_rect =
                    RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 42.0);
                let is_sel = vis_idx == session.focused_row;
                let is_hov = interaction.is_hovered(&HitTarget::ModalRow { index: vis_idx })
                    || interaction.is_hovered(&HitTarget::ModalCheckbox { index: vis_idx });
                if is_sel {
                    draw_list.push_rect(
                        row_rect,
                        tokens.surface_row_selected,
                        Some(tokens.border_subtle),
                        1.0,
                        row_r,
                        None,
                    );
                } else if is_hov {
                    draw_list.push_rect(
                        row_rect,
                        tokens.surface_row_hover,
                        Some(tokens.border_hairline),
                        1.0,
                        row_r,
                        None,
                    );
                }

                let mut cx = row_rect.x + 10.0;
                if session.multiple {
                    let cb = RectF::new(cx, row_rect.y + 13.0, 16.0, 16.0);
                    let cb_fill = if row.checked {
                        tokens.accent_primary
                    } else {
                        tokens.surface_pane
                    };
                    draw_list.push_rect(
                        cb,
                        cb_fill,
                        Some(if row.checked {
                            tokens.accent_primary
                        } else {
                            tokens.border_subtle
                        }),
                        1.0,
                        cap_radius(4.0, theme_rounding),
                        None,
                    );
                    if row.checked {
                        draw_list.push_icon(
                            cb.inset_uniform(2.0),
                            IconId::Check,
                            tokens.accent_foreground,
                        );
                    }
                    draw_list.push_hit(
                        cb,
                        HitTarget::ModalCheckbox { index: vis_idx },
                        z_layer::MODAL + 2,
                    );
                    cx += 24.0;
                }

                if let Some(icon) = item.icon.as_deref().and_then(IconId::from_rex_ui_name) {
                    draw_list.push_icon(
                        RectF::new(cx, row_rect.y + 13.0, 15.0, 15.0),
                        icon,
                        if is_sel {
                            tokens.accent_primary
                        } else {
                            tokens.fg_secondary
                        },
                    );
                    cx += 24.0;
                }

                let detail_w = item
                    .detail
                    .as_deref()
                    .map_or(0.0, |d| (d.chars().count() as f32 * 6.8 + 10.0).min(110.0));
                let text_w = (row_rect.right() - cx - detail_w - 8.0).max(0.0);
                let title_y = if item.subtitle.is_some() {
                    row_rect.y + 5.0
                } else {
                    row_rect.y + 12.0
                };
                draw_list.push_text(
                    RectF::new(cx, title_y, text_w, 17.0),
                    item.display_title(),
                    if is_sel {
                        tokens.fg_primary
                    } else {
                        tokens.fg_secondary
                    },
                    FontKind::Sans,
                    13.0,
                    FontWeight::Medium,
                    TextAlign::Left,
                    true,
                );
                if let Some(sub) = &item.subtitle {
                    draw_list.push_text(
                        RectF::new(cx, row_rect.y + 22.0, text_w, 15.0),
                        sub,
                        tokens.fg_muted,
                        FontKind::Sans,
                        11.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        true,
                    );
                }
                if let Some(det) = &item.detail {
                    draw_list.push_text(
                        RectF::new(
                            (row_rect.right() - detail_w - 6.0).max(cx),
                            row_rect.y + 13.0,
                            detail_w,
                            16.0,
                        ),
                        det,
                        tokens.fg_muted,
                        FontKind::Mono,
                        11.0,
                        FontWeight::Regular,
                        TextAlign::Right,
                        true,
                    );
                }
                draw_list.push_hit(
                    row_rect,
                    HitTarget::ModalRow { index: vis_idx },
                    z_layer::MODAL + 1,
                );
                y += 44.0;
                rendered += 1;
            }

            // Dedicated `allow_other` row for custom typed text (e.g., `Snooze Thread…` `"45m"`).
            if session.has_other_row() && rendered < 8 {
                let other_idx = rows.len();
                let is_sel = session.focused_row == other_idx;
                let row_rect =
                    RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 42.0);
                if is_sel {
                    draw_list.push_rect(
                        row_rect,
                        tokens.surface_row_selected,
                        Some(tokens.border_subtle),
                        1.0,
                        row_r,
                        None,
                    );
                } else if interaction.is_hovered(&HitTarget::ModalOtherRow) {
                    draw_list.push_rect(
                        row_rect,
                        tokens.surface_row_hover,
                        Some(tokens.border_hairline),
                        1.0,
                        row_r,
                        None,
                    );
                }
                draw_list.push_icon(
                    RectF::new(row_rect.x + 10.0, row_rect.y + 13.0, 15.0, 15.0),
                    IconId::Plus,
                    tokens.accent_primary,
                );
                draw_list.push_text(
                    RectF::new(
                        row_rect.x + 34.0,
                        row_rect.y + 12.0,
                        (row_rect.w - 44.0).max(0.0),
                        17.0,
                    ),
                    format!("{} “{}”", session.confirm_label, session.input_text),
                    tokens.fg_primary,
                    FontKind::Sans,
                    12.5,
                    FontWeight::Medium,
                    TextAlign::Left,
                    true,
                );
                draw_list.push_hit(row_rect, HitTarget::ModalOtherRow, z_layer::MODAL + 1);
            }
            render_modal_scrollbar(
                draw_list,
                body_rect,
                scroll_row,
                8,
                session.visible_row_count(),
                tokens,
            );
        }
    }
    draw_list.pop_clip();

    // Multi-select footer with ticked count + `Confirm (Ctrl+Enter)` primary pill
    if session.multiple {
        let footer = RectF::new(
            card.x,
            (card.bottom() - footer_h).max(card.y),
            card.w,
            footer_h,
        );
        draw_list.push_rect(
            RectF::new(footer.x, footer.y, footer.w, 1.0),
            tokens.border_hairline,
            None,
            0.0,
            0.0,
            None,
        );
        draw_list.push_text(
            RectF::new(footer.x + 14.0, footer.y + 12.0, 160.0, 18.0),
            format!("{} selected · Space toggles", session.checked_ids.len()),
            tokens.fg_muted,
            FontKind::Sans,
            11.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        let confirm_text = format!("{} (Ctrl+Enter)", session.confirm_label);
        let btn_w = ((confirm_text.chars().count() as f32) * 6.8 + 24.0).clamp(120.0, 210.0);
        let btn = RectF::new(
            (footer.right() - btn_w - 10.0).max(footer.x),
            footer.y + 7.0,
            btn_w,
            28.0,
        );
        draw_list.push_rect(
            btn,
            tokens.accent_primary,
            None,
            0.0,
            cap_radius(6.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            btn,
            confirm_text,
            tokens.accent_foreground,
            FontKind::Sans,
            11.5,
            FontWeight::SemiBold,
            TextAlign::Center,
            false,
        );
        draw_list.push_hit(btn, HitTarget::ModalConfirmButton, z_layer::MODAL + 2);
    }
}

fn render_modal_search_header(
    draw_list: &mut UiDrawList,
    header: RectF,
    input: &TextInput,
    badge: &str,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    draw_list.push_icon(
        RectF::new(header.x + 14.0, header.y + 15.0, 15.0, 15.0),
        IconId::Search,
        tokens.fg_muted,
    );
    let badge_w = ((badge.chars().count() as f32) * 6.6 + 14.0).clamp(32.0, 112.0);
    let badge_rect = RectF::new(
        (header.right() - badge_w - 12.0).max(header.x),
        header.y + 12.0,
        badge_w,
        22.0,
    );
    draw_list.push_rect(
        badge_rect,
        tokens.surface_pane,
        Some(tokens.border_subtle),
        1.0,
        cap_radius(5.0, theme_rounding),
        None,
    );
    draw_list.push_text(
        badge_rect,
        badge,
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Center,
        true,
    );

    let text_rect = RectF::new(
        header.x + 38.0,
        header.y + 13.0,
        (badge_rect.x - header.x - 46.0).max(0.0),
        20.0,
    );
    let (display, color) = if input.is_empty() {
        (input.placeholder().to_owned(), tokens.fg_muted)
    } else {
        (input.display_text().0, tokens.fg_primary)
    };
    draw_list.push_text(
        text_rect,
        display,
        color,
        FontKind::Sans,
        14.0,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    let caret_x = text_rect.x
        + (input.display_text().0.chars().count() as f32 * 7.6).min((text_rect.w - 2.0).max(0.0));
    draw_list.push_rect(
        RectF::new(caret_x, text_rect.y + 2.0, 1.5, 16.0),
        tokens.accent_primary,
        None,
        0.0,
        0.75,
        None,
    );

    draw_list.push_rect(
        RectF::new(
            header.x,
            (header.bottom() - 1.0).max(header.y),
            header.w,
            1.0,
        ),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );
    draw_list.push_hit(header, HitTarget::ModalInput, z_layer::MODAL + 1);
}

#[allow(clippy::too_many_arguments)]
fn render_file_picker_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    max_h: f32,
    card_r: f32,
    shadow: ShadowSpec,
    purpose: FilePickerPurpose,
    current_dir: &str,
    remote_host: Option<&str>,
    loading: bool,
    error: Option<&str>,
    input: &TextInput,
    entries: &[FilePickerEntry],
    filtered_indices: &[usize],
    selected_row: usize,
    scroll_row: usize,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let header_h = 74.0;
    let footer_h = 28.0;
    let visible_count = filtered_indices.len().min(8);
    let body_h = if loading || filtered_indices.is_empty() {
        56.0
    } else if error.is_some() {
        visible_count as f32 * 38.0 + 36.0
    } else {
        visible_count as f32 * 38.0 + 12.0
    };
    let card_h = (header_h + body_h + footer_h).min(max_h);
    let card = RectF::new(
        left_x.round(),
        top_y.round(),
        modal_w.round(),
        card_h.round(),
    );
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    let title = match purpose {
        FilePickerPurpose::InsertPath => "Insert Path",
        FilePickerPurpose::GoToDirectory => "Go to Directory",
    };
    let host_tag = remote_host.map_or_else(String::new, |h| format!("[{h}] "));
    let crumb = format!("{title} · {host_tag}{current_dir}");
    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 8.0, (card.w - 28.0).max(0.0), 18.0),
        crumb,
        tokens.accent_primary,
        FontKind::Mono,
        11.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    render_modal_search_header(
        draw_list,
        RectF::new(card.x, card.y + 28.0, card.w, 46.0),
        input,
        match purpose {
            FilePickerPurpose::InsertPath => "Ins",
            FilePickerPurpose::GoToDirectory => "cd",
        },
        tokens,
        theme_rounding,
    );

    let list_rect = RectF::new(
        card.x,
        card.y + header_h,
        card.w,
        (card.h - header_h - footer_h).max(0.0),
    );
    draw_list.push_clip(list_rect);
    if loading {
        draw_list.push_text(
            RectF::new(
                list_rect.x + 16.0,
                list_rect.y + 16.0,
                (list_rect.w - 32.0).max(0.0),
                20.0,
            ),
            "Loading directory…",
            tokens.fg_muted,
            FontKind::Sans,
            12.5,
            FontWeight::Regular,
            TextAlign::Center,
            true,
        );
    } else if let Some(err) = error
        && filtered_indices.is_empty()
    {
        draw_list.push_text(
            RectF::new(
                list_rect.x + 16.0,
                list_rect.y + 16.0,
                (list_rect.w - 32.0).max(0.0),
                20.0,
            ),
            err,
            tokens.status_error,
            FontKind::Sans,
            12.0,
            FontWeight::Regular,
            TextAlign::Center,
            true,
        );
    } else if filtered_indices.is_empty() {
        draw_list.push_text(
            RectF::new(
                list_rect.x + 16.0,
                list_rect.y + 16.0,
                (list_rect.w - 32.0).max(0.0),
                20.0,
            ),
            "Empty directory (Enter selects current dir)",
            tokens.fg_muted,
            FontKind::Sans,
            12.0,
            FontWeight::Regular,
            TextAlign::Center,
            true,
        );
    } else {
        let mut y = list_rect.y + 6.0;
        if let Some(err) = error {
            draw_list.push_text(
                RectF::new(list_rect.x + 14.0, y, (list_rect.w - 28.0).max(0.0), 18.0),
                err,
                tokens.status_error,
                FontKind::Sans,
                11.5,
                FontWeight::Medium,
                TextAlign::Left,
                true,
            );
            y += 24.0;
        }
        let row_r = cap_radius(6.0, theme_rounding);
        for (vis_idx, &entry_idx) in filtered_indices.iter().enumerate().skip(scroll_row).take(8) {
            let Some(entry) = entries.get(entry_idx) else {
                continue;
            };
            let row_rect = RectF::new(list_rect.x + 6.0, y, (list_rect.w - 12.0).max(0.0), 36.0);
            let row_target = HitTarget::ModalRow { index: vis_idx };
            let is_sel = vis_idx == selected_row;
            let is_hov = interaction.is_hovered(&row_target);
            if is_sel {
                draw_list.push_rect(
                    row_rect,
                    tokens.surface_row_selected,
                    Some(tokens.border_subtle),
                    1.0,
                    row_r,
                    None,
                );
            } else if is_hov {
                draw_list.push_rect(
                    row_rect,
                    tokens.surface_row_hover,
                    Some(tokens.border_hairline),
                    1.0,
                    row_r,
                    None,
                );
            }
            let icon = if entry.is_dir {
                IconId::Folder
            } else {
                IconId::Document
            };
            draw_list.push_icon(
                RectF::new(row_rect.x + 10.0, row_rect.y + 10.0, 15.0, 15.0),
                icon,
                if entry.is_dir {
                    tokens.accent_primary
                } else {
                    tokens.fg_muted
                },
            );
            let tag = if entry.is_symlink && entry.is_dir {
                "link → dir"
            } else if entry.is_symlink {
                "symlink"
            } else if entry.is_dir {
                "dir"
            } else {
                "file"
            };
            draw_list.push_text(
                RectF::new(
                    row_rect.x + 32.0,
                    row_rect.y + 9.0,
                    (row_rect.w - 110.0).max(0.0),
                    18.0,
                ),
                &entry.name,
                if is_sel {
                    tokens.fg_primary
                } else {
                    tokens.fg_secondary
                },
                FontKind::Mono,
                12.5,
                FontWeight::Medium,
                TextAlign::Left,
                true,
            );
            draw_list.push_text(
                RectF::new(
                    (row_rect.right() - 78.0).max(row_rect.x),
                    row_rect.y + 10.0,
                    68.0,
                    16.0,
                ),
                tag,
                tokens.fg_muted,
                FontKind::Mono,
                10.0,
                FontWeight::Regular,
                TextAlign::Right,
                false,
            );
            draw_list.push_hit(row_rect, row_target, z_layer::MODAL + 1);
            y += 38.0;
        }
        render_modal_scrollbar(
            draw_list,
            list_rect,
            scroll_row,
            8,
            filtered_indices.len(),
            tokens,
        );
    }
    draw_list.pop_clip();

    draw_list.push_rect(
        RectF::new(card.x, card.bottom() - footer_h, card.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );
    draw_list.push_text(
        RectF::new(
            card.x + 12.0,
            card.bottom() - footer_h + 6.0,
            (card.w - 24.0).max(0.0),
            16.0,
        ),
        "Tab descend · Backspace parent · Enter select · Esc cancel",
        tokens.fg_muted,
        FontKind::Mono,
        10.0,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_theme_picker_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    max_h: f32,
    card_r: f32,
    shadow: ShadowSpec,
    input: &TextInput,
    items: &[PaletteItem],
    matches: &[ScoredMatch],
    selected_row: usize,
    scroll_row: usize,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let visible_count = matches.len().min(8);
    let list_h = if matches.is_empty() {
        54.0
    } else {
        visible_count as f32 * 44.0 + 12.0
    };
    let card_h = (46.0 + list_h).min(max_h);
    let card = RectF::new(
        left_x.round(),
        top_y.round(),
        modal_w.round(),
        card_h.round(),
    );
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );
    render_modal_search_header(
        draw_list,
        RectF::new(card.x, card.y, card.w, 46.0),
        input,
        "Theme",
        tokens,
        theme_rounding,
    );

    let list_rect = RectF::new(card.x, card.y + 46.0, card.w, (card.h - 46.0).max(0.0));
    draw_list.push_clip(list_rect);
    if matches.is_empty() {
        draw_list.push_text(
            RectF::new(
                list_rect.x + 16.0,
                list_rect.y + 16.0,
                (list_rect.w - 32.0).max(0.0),
                20.0,
            ),
            "No matching themes",
            tokens.fg_muted,
            FontKind::Sans,
            12.5,
            FontWeight::Regular,
            TextAlign::Center,
            true,
        );
    } else {
        let mut y = list_rect.y + 6.0;
        let row_r = cap_radius(7.0, theme_rounding);
        for (vis_idx, m) in matches.iter().enumerate().skip(scroll_row).take(8) {
            let item = &items[m.index];
            let row_rect = RectF::new(list_rect.x + 6.0, y, (list_rect.w - 12.0).max(0.0), 42.0);
            let row_target = HitTarget::ModalRow { index: vis_idx };
            let is_sel = vis_idx == selected_row;
            let is_hov = interaction.is_hovered(&row_target);
            if is_sel {
                draw_list.push_rect(
                    row_rect,
                    tokens.surface_row_selected,
                    Some(tokens.border_subtle),
                    1.0,
                    row_r,
                    None,
                );
            } else if is_hov {
                draw_list.push_rect(
                    row_rect,
                    tokens.surface_row_hover,
                    Some(tokens.border_hairline),
                    1.0,
                    row_r,
                    None,
                );
            }

            let tx = row_rect.x + 12.0;
            let right_w = item
                .detail
                .as_deref()
                .map_or(0.0, |t| (t.chars().count() as f32 * 6.8 + 16.0).min(120.0));
            let text_w = (row_rect.right() - tx - right_w - 10.0).max(0.0);
            let title_y = if item.subtitle.is_some() {
                row_rect.y + 5.0
            } else {
                row_rect.y + 12.0
            };
            draw_list.push_text(
                RectF::new(tx, title_y, text_w, 17.0),
                &item.title,
                if is_sel {
                    tokens.fg_primary
                } else {
                    tokens.fg_secondary
                },
                FontKind::Sans,
                13.0,
                FontWeight::Medium,
                TextAlign::Left,
                true,
            );
            if let Some(sub) = &item.subtitle {
                draw_list.push_text(
                    RectF::new(tx, row_rect.y + 22.0, text_w, 15.0),
                    sub,
                    tokens.fg_muted,
                    FontKind::Sans,
                    11.0,
                    FontWeight::Regular,
                    TextAlign::Left,
                    true,
                );
            }
            if let Some(badge) = &item.detail {
                draw_list.push_text(
                    RectF::new(
                        (row_rect.right() - right_w - 8.0).max(tx),
                        row_rect.y + 12.0,
                        right_w,
                        18.0,
                    ),
                    badge,
                    tokens.accent_primary,
                    FontKind::Mono,
                    10.5,
                    FontWeight::Medium,
                    TextAlign::Right,
                    false,
                );
            }
            draw_list.push_hit(row_rect, row_target, z_layer::MODAL + 1);
            y += 44.0;
        }
        render_modal_scrollbar(draw_list, list_rect, scroll_row, 8, matches.len(), tokens);
    }
    draw_list.pop_clip();
}

#[allow(clippy::too_many_arguments)]
fn render_diagnostics_modal(
    draw_list: &mut UiDrawList,
    _left_x: f32,
    _top_y: f32,
    modal_w: f32,
    card_h: f32,
    viewport: RectF,
    card_r: f32,
    shadow: ShadowSpec,
    snap: &DiagnosticsSnapshot,
    scroll_row: usize,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let left_x = viewport.x + (viewport.w - modal_w) * 0.5;
    let top_y = (viewport.y + (viewport.h - card_h) * 0.5).max(viewport.y + 16.0);
    let card = RectF::new(
        left_x.round(),
        top_y.round(),
        modal_w.round(),
        card_h.round(),
    );
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    // Header
    draw_list.push_icon(
        RectF::new(card.x + 14.0, card.y + 14.0, 15.0, 15.0),
        IconId::Activity,
        tokens.accent_primary,
    );
    draw_list.push_text(
        RectF::new(
            card.x + 36.0,
            card.y + 12.0,
            (card.w - 220.0).max(0.0),
            20.0,
        ),
        "Diagnostics & Telemetry HUD",
        tokens.fg_primary,
        FontKind::Sans,
        14.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    draw_list.push_text(
        RectF::new(
            (card.right() - 180.0).max(card.x),
            card.y + 14.0,
            166.0,
            16.0,
        ),
        format!("{} · {}", snap.gpu_backend, snap.app_version),
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        true,
    );
    draw_list.push_rect(
        RectF::new(card.x, card.y + 44.0, card.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let footer_h = 44.0;
    let body_rect = RectF::new(
        card.x + 14.0,
        card.y + 52.0,
        (card.w - 28.0).max(0.0),
        (card.h - 52.0 - footer_h).max(0.0),
    );
    draw_list.push_clip(body_rect);

    let mut y = body_rect.y - (scroll_row as f32 * 20.0);

    // 1. GPU Frame-time graph
    draw_list.push_text(
        RectF::new(body_rect.x, y, body_rect.w, 16.0),
        format!(
            "GPU: {}   avg {:.2}ms · p95 {:.2}ms · max {:.2}ms   draws={} quads={} glyphs={}",
            snap.gpu_adapter,
            snap.avg_frame_ms,
            snap.p95_frame_ms,
            snap.max_frame_ms,
            snap.last_draw_calls,
            snap.last_quads,
            snap.last_glyphs
        ),
        tokens.fg_secondary,
        FontKind::Mono,
        10.5,
        FontWeight::Medium,
        TextAlign::Left,
        true,
    );
    y += 20.0;

    let graph_h = 68.0;
    let graph_rect = RectF::new(body_rect.x, y, body_rect.w, graph_h);
    draw_list.push_rect(
        graph_rect,
        tokens.surface_pane,
        Some(tokens.border_hairline),
        1.0,
        cap_radius(6.0, theme_rounding),
        None,
    );
    let max_us = snap
        .frame_times_us
        .iter()
        .copied()
        .max()
        .unwrap_or(16_667)
        .max(16_667) as f32;
    let samples = if snap.frame_times_us.is_empty() {
        vec![0u32; 1]
    } else {
        snap.frame_times_us.clone()
    };
    let bar_area_w = (graph_rect.w - 12.0).max(12.0);
    let bar_count = samples.len().max(1);
    let step_w = bar_area_w / (bar_count as f32);
    let bar_w = (step_w - 1.0).max(1.5);
    for (idx, &us) in samples.iter().enumerate() {
        let frac = (us as f32 / max_us).clamp(0.04, 1.0);
        let bh = (graph_h - 10.0) * frac;
        let bx = graph_rect.x + 6.0 + idx as f32 * step_w;
        let by = graph_rect.bottom() - 5.0 - bh;
        let bar_color = if us <= 8_000 {
            tokens.status_done
        } else if us <= 16_667 {
            tokens.status_working
        } else {
            tokens.status_error
        };
        draw_list.push_rect(
            RectF::new(bx, by, bar_w, bh),
            bar_color,
            None,
            0.0,
            1.0,
            None,
        );
    }
    y += graph_h + 14.0;

    // 2. Hosts, Tailscale & Stream Health
    draw_list.push_text(
        RectF::new(body_rect.x, y, body_rect.w, 16.0),
        format!("Hosts, Tailscale & Stream Health (VT {})", snap.vt_version),
        tokens.accent_primary,
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    y += 20.0;
    if snap.hosts.is_empty() {
        draw_list.push_text(
            RectF::new(body_rect.x + 8.0, y, body_rect.w - 8.0, 16.0),
            "No configured Rex hosts",
            tokens.fg_muted,
            FontKind::Mono,
            10.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        y += 18.0;
    } else {
        for h in &snap.hosts {
            draw_list.push_text(
                RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 16.0),
                format!(
                    "• {} [{}] · TS: {} · streams={} dropped={}",
                    h.label, h.status, h.tailscale_summary, h.active_streams, h.dropped_events
                ),
                tokens.fg_secondary,
                FontKind::Mono,
                10.5,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );
            y += 18.0;
        }
    }
    y += 8.0;

    // 3. Keymap warnings
    draw_list.push_text(
        RectF::new(body_rect.x, y, body_rect.w, 16.0),
        format!("Keymap Diagnostics ({})", snap.keymap_warnings.len()),
        if snap.keymap_warnings.is_empty() {
            tokens.status_done
        } else {
            tokens.status_blocked
        },
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    y += 18.0;
    if snap.keymap_warnings.is_empty() {
        draw_list.push_text(
            RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 16.0),
            "All keymap bindings clean",
            tokens.fg_muted,
            FontKind::Mono,
            10.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        y += 18.0;
    } else {
        for w in snap.keymap_warnings.iter().take(4) {
            draw_list.push_text(
                RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 16.0),
                format!("! {w}"),
                tokens.status_blocked,
                FontKind::Mono,
                10.5,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );
            y += 18.0;
        }
    }
    y += 8.0;

    // 4. Listening Ports & Active Tunnels
    draw_list.push_text(
        RectF::new(body_rect.x, y, body_rect.w, 16.0),
        format!(
            "Listening Ports ({}) & Tunnels ({})",
            snap.active_ports.len(),
            snap.active_tunnels.len()
        ),
        tokens.accent_primary,
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    y += 18.0;
    if snap.active_ports.is_empty() && snap.active_tunnels.is_empty() {
        draw_list.push_text(
            RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 16.0),
            "None active",
            tokens.fg_muted,
            FontKind::Mono,
            10.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        y += 18.0;
    } else {
        for line in snap
            .active_ports
            .iter()
            .chain(snap.active_tunnels.iter())
            .take(5)
        {
            draw_list.push_text(
                RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 16.0),
                format!("• {line}"),
                tokens.fg_secondary,
                FontKind::Mono,
                10.5,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );
            y += 17.0;
        }
    }
    y += 8.0;

    // 5. Program Resume (OSC 7577)
    draw_list.push_text(
        RectF::new(body_rect.x, y, body_rect.w, 16.0),
        format!("Program Resume (OSC 7577) ({})", snap.resume_records.len()),
        tokens.accent_primary,
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    y += 18.0;
    if snap.resume_records.is_empty() {
        draw_list.push_text(
            RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 16.0),
            "No active OSC 7577 resume records",
            tokens.fg_muted,
            FontKind::Mono,
            10.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
    } else {
        for line in snap.resume_records.iter().take(6) {
            draw_list.push_text(
                RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 16.0),
                format!("↻ {line}"),
                tokens.fg_secondary,
                FontKind::Mono,
                10.5,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );
            y += 17.0;
        }
    }

    draw_list.pop_clip();

    // Footer with Copy Debug Bundle button
    let footer = RectF::new(card.x, card.bottom() - footer_h, card.w, footer_h);
    draw_list.push_rect(
        RectF::new(footer.x, footer.y, footer.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );
    draw_list.push_text(
        RectF::new(footer.x + 14.0, footer.y + 13.0, 220.0, 16.0),
        "Esc / Enter to close",
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    let btn = RectF::new(
        (footer.right() - 164.0).max(footer.x),
        footer.y + 8.0,
        150.0,
        28.0,
    );
    let hovered = interaction.is_hovered(&HitTarget::DiagnosticsCopyBundle);
    draw_list.push_rect(
        btn,
        if hovered {
            tokens.accent_primary.with_alpha_f32(0.90)
        } else {
            tokens.accent_primary
        },
        None,
        0.0,
        cap_radius(6.0, theme_rounding),
        None,
    );
    draw_list.push_text(
        btn,
        "Copy Debug Bundle",
        tokens.accent_foreground,
        FontKind::Sans,
        11.5,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(btn, HitTarget::DiagnosticsCopyBundle, z_layer::MODAL + 1);
}

#[allow(clippy::too_many_arguments)]
fn render_settings_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    viewport: RectF,
    card_r: f32,
    shadow: ShadowSpec,
    s: &SettingsViewSnapshot,
    category: SettingsCategory,
    selected_row: usize,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let rows: Vec<(&str, &str, String)> = match category {
        SettingsCategory::Workspace => vec![
            (
                "toolbar_position",
                "Toolbar Layout",
                format!("{:?}", s.toolbar_position),
            ),
            (
                "toolbar_behavior",
                "Toolbar Behavior",
                format!("{:?}", s.toolbar_behavior),
            ),
            ("pane_density", "Pane Density", format!("{:?}", s.density)),
            (
                "pane_headers",
                "Pane Headers",
                format!("{:?}", s.pane_header_mode),
            ),
        ],
        SettingsCategory::Appearance => vec![
            (
                "theme_name",
                "Theme",
                s.theme_name
                    .clone()
                    .unwrap_or_else(|| "omarchy-live".to_owned()),
            ),
            (
                "material",
                "Surface Material",
                match s.material {
                    MaterialStyle::Classic => "Classic (Opaque)".to_owned(),
                    MaterialStyle::Translucent { alpha } => {
                        format!("Translucent ({:.0}%)", alpha * 100.0)
                    }
                },
            ),
            (
                "active_border",
                "Highlight Active Pane Border",
                if s.highlight_active_border {
                    "On"
                } else {
                    "Off"
                }
                .to_owned(),
            ),
            (
                "fade_inactive",
                "Fade Inactive Panes",
                if s.fade_inactive_panes { "On" } else { "Off" }.to_owned(),
            ),
            (
                "adaptive_rounding",
                "Follow Hyprland Rounding",
                if s.adaptive_rounding { "On" } else { "Off" }.to_owned(),
            ),
            (
                "program_color_header",
                "Program-Color Pane Header",
                if s.program_color_header { "On" } else { "Off" }.to_owned(),
            ),
        ],
        SettingsCategory::Terminal => vec![
            (
                "font_family",
                "Font Family (settings.json)",
                s.font_family.clone(),
            ),
            (
                "font_size",
                "Font Size (click/Enter step)",
                format!("{:.1} pt", s.font_size_pt),
            ),
        ],
        SettingsCategory::AudioAlerts => vec![
            (
                "chime_enabled",
                "PipeWire Micro-Chimes (OSC 7501)",
                if s.chime_enabled { "On" } else { "Off" }.to_owned(),
            ),
            (
                "desktop_notifications",
                "Desktop Notifications",
                if s.desktop_notifications { "On" } else { "Off" }.to_owned(),
            ),
        ],
        SettingsCategory::Server => server_action_rows(&s.server),
        SettingsCategory::ConnectIos => ios_action_rows(&s.ios_pairing),
    };

    let tab_bar_h = 34.0;
    let wide_tab = matches!(
        category,
        SettingsCategory::Server | SettingsCategory::ConnectIos
    );
    let effective_w = if wide_tab {
        (viewport.w * 0.58)
            .clamp(540.0, 620.0)
            .min((viewport.w - 32.0).max(240.0))
    } else {
        modal_w.max(520.0).min((viewport.w - 32.0).max(240.0))
    };
    let hero_h = match category {
        SettingsCategory::Server => {
            let has_banner = s.server.last_error.is_some() || s.server.confirm_stop;
            116.0 + if has_banner { 26.0 } else { 0.0 }
        }
        SettingsCategory::ConnectIos => {
            let has_err = s.ios_pairing.last_error.is_some()
                || matches!(
                    s.ios_pairing.state,
                    TailscalePairState::Disabled { error: Some(_) }
                );
            if matches!(s.ios_pairing.state, TailscalePairState::Ready { .. }) {
                220.0 + if has_err { 24.0 } else { 0.0 }
            } else {
                138.0 + if has_err { 24.0 } else { 0.0 }
            }
        }
        _ => 0.0,
    };
    let card_h = 46.0 + tab_bar_h + hero_h + rows.len().max(1) as f32 * 38.0 + 16.0;
    let effective_x = viewport.x + (viewport.w - effective_w) * 0.5;
    let effective_y = top_y.min((viewport.bottom() - card_h - 16.0).max(viewport.y + 16.0));
    let _ = left_x;
    let card = RectF::new(
        effective_x.round(),
        effective_y.round(),
        effective_w.round(),
        card_h.round(),
    );
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    draw_list.push_icon(
        RectF::new(card.x + 14.0, card.y + 15.0, 15.0, 15.0),
        IconId::Gear,
        tokens.accent_primary,
    );
    draw_list.push_text(
        RectF::new(
            card.x + 36.0,
            card.y + 13.0,
            (card.w - 200.0).max(0.0),
            20.0,
        ),
        "Settings",
        tokens.fg_primary,
        FontKind::Sans,
        14.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    draw_list.push_text(
        RectF::new(card.right() - 170.0, card.y + 15.0, 156.0, 16.0),
        if wide_tab {
            "←/→ tab · Enter run"
        } else {
            "←/→ tab · Enter cycle"
        },
        tokens.fg_muted,
        FontKind::Mono,
        10.0,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );
    draw_list.push_rect(
        RectF::new(card.x, card.y + 45.0, card.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    // Category tabs
    let tab_w = ((card.w - 16.0) / SETTINGS_CATEGORIES.len() as f32).max(56.0);
    for (idx, &(cat, title)) in SETTINGS_CATEGORIES.iter().enumerate() {
        let tab_rect = RectF::new(
            card.x + 8.0 + idx as f32 * tab_w,
            card.y + 50.0,
            (tab_w - 4.0).max(40.0),
            24.0,
        );
        let tab_hit = HitTarget::SettingsCategoryTab(cat);
        let active = cat == category;
        let hovered = interaction.is_hovered(&tab_hit);
        if active {
            draw_list.push_rect(
                tab_rect,
                tokens.accent_primary.with_alpha_f32(0.18),
                Some(tokens.accent_primary.with_alpha_f32(0.65)),
                1.0,
                cap_radius(6.0, theme_rounding),
                None,
            );
        } else if hovered {
            draw_list.push_rect(
                tab_rect,
                tokens.surface_row_hover,
                Some(tokens.border_hairline),
                1.0,
                cap_radius(6.0, theme_rounding),
                None,
            );
        }
        draw_list.push_text(
            tab_rect,
            title,
            if active {
                tokens.accent_primary
            } else {
                tokens.fg_secondary
            },
            FontKind::Sans,
            11.0,
            if active {
                FontWeight::SemiBold
            } else {
                FontWeight::Regular
            },
            TextAlign::Center,
            true,
        );
        draw_list.push_hit(tab_rect, tab_hit, z_layer::MODAL + 1);
    }

    let mut y = card.y + 46.0 + tab_bar_h + 6.0;
    if category == SettingsCategory::Server {
        let hero_rect = RectF::new(
            card.x + 10.0,
            y,
            (card.w - 20.0).max(0.0),
            (hero_h - 10.0).max(0.0),
        );
        render_server_settings_hero(draw_list, hero_rect, &s.server, tokens, theme_rounding);
        y += hero_h;
    } else if category == SettingsCategory::ConnectIos {
        let hero_rect = RectF::new(
            card.x + 10.0,
            y,
            (card.w - 20.0).max(0.0),
            (hero_h - 10.0).max(0.0),
        );
        render_ios_pairing_hero(
            draw_list,
            hero_rect,
            &s.ios_pairing,
            interaction,
            tokens,
            theme_rounding,
        );
        y += hero_h;
    }

    let row_r = cap_radius(7.0, theme_rounding);
    for (idx, (key, label, value)) in rows.into_iter().enumerate() {
        let row_rect = RectF::new(card.x + 8.0, y, (card.w - 16.0).max(0.0), 34.0);
        let toggle_target = HitTarget::SettingsToggle {
            key: key.to_owned(),
        };
        let is_hovered = interaction.is_hovered(&toggle_target)
            || interaction.is_hovered(&HitTarget::ModalRow { index: idx });
        let is_pressed = interaction.is_pressed(&toggle_target);
        let is_danger_confirm = key == "server_stop" && s.server.confirm_stop;
        if idx == selected_row || is_pressed {
            draw_list.push_rect(
                row_rect,
                tokens.surface_row_selected,
                Some(tokens.border_subtle),
                1.0,
                row_r,
                None,
            );
        } else if is_hovered {
            draw_list.push_rect(
                row_rect,
                tokens.surface_row_hover,
                Some(tokens.border_hairline),
                1.0,
                row_r,
                None,
            );
        }
        draw_list.push_text(
            RectF::new(
                row_rect.x + 10.0,
                row_rect.y + 8.0,
                (row_rect.w - 166.0).max(0.0),
                18.0,
            ),
            label,
            if is_danger_confirm {
                tokens.status_error
            } else {
                tokens.fg_primary
            },
            FontKind::Sans,
            12.5,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );
        let pill = RectF::new(
            (row_rect.right() - 150.0).max(row_rect.x),
            row_rect.y + 6.0,
            140.0,
            22.0,
        );
        draw_list.push_rect(
            pill,
            if is_danger_confirm {
                tokens.status_error.with_alpha_f32(0.18)
            } else {
                tokens.surface_pane
            },
            Some(if is_danger_confirm {
                tokens.status_error
            } else if is_hovered || idx == selected_row {
                tokens.accent_primary.with_alpha_f32(0.50)
            } else {
                tokens.border_hairline
            }),
            1.0,
            cap_radius(5.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            pill,
            value,
            if is_danger_confirm {
                tokens.status_error
            } else {
                tokens.accent_primary
            },
            FontKind::Mono,
            10.5,
            FontWeight::Medium,
            TextAlign::Center,
            true,
        );
        draw_list.push_hit(row_rect, toggle_target, z_layer::MODAL + 1);
        draw_list.push_hit(row_rect, HitTarget::ModalRow { index: idx }, z_layer::MODAL);
        y += 38.0;
    }
}

fn render_server_settings_hero(
    draw_list: &mut UiDrawList,
    hero: RectF,
    srv: &ServerSettingsSnapshot,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    draw_list.push_rect(
        hero,
        tokens.surface_pane,
        Some(tokens.border_hairline),
        1.0,
        cap_radius(8.0, theme_rounding),
        None,
    );

    let (dot_color, headline) = match &srv.state {
        LocalServerState::Running => (tokens.status_done, "Local Rex Server · Running".to_owned()),
        LocalServerState::Reconnecting { reason } => (
            tokens.status_blocked,
            if reason.is_empty() {
                "Local Rex Server · Reconnecting…".to_owned()
            } else {
                format!("Local Rex Server · Reconnecting ({reason})")
            },
        ),
        LocalServerState::Stopped => (tokens.fg_muted, "Local Rex Server · Stopped".to_owned()),
        LocalServerState::Error(err) => (
            tokens.status_error,
            format!("Local Rex Server · Error ({err})"),
        ),
    };

    let x = hero.x + 14.0;
    let w = (hero.w - 28.0).max(0.0);
    let mut y = hero.y + 12.0;

    draw_list.push_rect(
        RectF::new(x, y + 5.0, 8.0, 8.0),
        dot_color,
        None,
        0.0,
        4.0,
        None,
    );
    draw_list.push_text(
        RectF::new(x + 14.0, y, (w - 14.0).max(0.0), 18.0),
        headline,
        tokens.fg_primary,
        FontKind::Sans,
        13.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    y += 22.0;

    let mut meta_parts = Vec::new();
    if let Some(ver) = &srv.version {
        meta_parts.push(format!("rex {ver}"));
    }
    if let Some(rev) = srv.api_revision {
        meta_parts.push(format!("API rev {rev}"));
    }
    if let Some(pid) = srv.pid {
        meta_parts.push(format!("PID {pid}"));
    }
    if matches!(srv.state, LocalServerState::Running) {
        meta_parts.push(format!(
            "{} {}",
            srv.session_count,
            if srv.session_count == 1 {
                "session"
            } else {
                "sessions"
            }
        ));
    }
    let meta_line = if meta_parts.is_empty() {
        "Daemon inactive — start the server to restore saved sessions".to_owned()
    } else {
        meta_parts.join(" · ")
    };
    draw_list.push_text(
        RectF::new(x, y, w, 16.0),
        meta_line,
        tokens.fg_secondary,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    y += 18.0;

    let endpoint_line = if srv.endpoint.is_empty() {
        "Endpoint: default local Unix socket".to_owned()
    } else {
        format!("Endpoint: {}", srv.endpoint)
    };
    draw_list.push_text(
        RectF::new(x, y, w, 16.0),
        endpoint_line,
        tokens.fg_muted,
        FontKind::Mono,
        10.0,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    y += 18.0;

    let resume_summary = if srv.resumable_trusted_count > 0 || srv.resumable_untrusted_count > 0 {
        format!(
            "Program Resume (OSC 7577): {} trusted · {} untrusted",
            srv.resumable_trusted_count, srv.resumable_untrusted_count
        )
    } else {
        "Program Resume (OSC 7577): ready · no active resumable programs".to_owned()
    };
    draw_list.push_text(
        RectF::new(x, y, w, 16.0),
        resume_summary,
        if srv.resumable_trusted_count > 0 {
            tokens.status_done
        } else if srv.resumable_untrusted_count > 0 {
            tokens.status_blocked
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        11.0,
        FontWeight::Medium,
        TextAlign::Left,
        true,
    );
    y += 22.0;

    if srv.confirm_stop {
        let banner = RectF::new(x, y, w, 22.0);
        draw_list.push_rect(
            banner,
            tokens.status_blocked.with_alpha_f32(0.16),
            Some(tokens.status_blocked.with_alpha_f32(0.55)),
            1.0,
            cap_radius(5.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            RectF::new(
                banner.x + 8.0,
                banner.y + 3.0,
                (banner.w - 16.0).max(0.0),
                16.0,
            ),
            "Stopping ends running processes; trusted OSC 7577 programs resume on next start.",
            tokens.status_blocked,
            FontKind::Sans,
            10.5,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );
    } else if let Some(err) = &srv.last_error {
        let banner = RectF::new(x, y, w, 22.0);
        draw_list.push_rect(
            banner,
            tokens.status_error.with_alpha_f32(0.14),
            Some(tokens.status_error.with_alpha_f32(0.50)),
            1.0,
            cap_radius(5.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            RectF::new(
                banner.x + 8.0,
                banner.y + 3.0,
                (banner.w - 16.0).max(0.0),
                16.0,
            ),
            err,
            tokens.status_error,
            FontKind::Mono,
            10.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
    }
}

/// Draws an ISO/IEC 18004 QR code inside `slot` on a high-contrast `#FFFFFF` quiet-zone card
/// using integer-aligned horizontal run-length merged `#000000` module rects (zero blur).
fn render_qr_matrix(
    draw_list: &mut UiDrawList,
    slot: RectF,
    qr: &QrMatrix,
    border_color: Rgba,
    theme_rounding: f32,
) {
    if qr.size == 0 || slot.w < 40.0 || slot.h < 40.0 {
        return;
    }
    const QUIET_MODULES: usize = 4;
    let total_modules = qr.size + QUIET_MODULES * 2;
    let outer_pad = 4.0_f32;
    let usable = (slot.w.min(slot.h) - outer_pad * 2.0).max(total_modules as f32);
    let mod_px = (usable / total_modules as f32).floor().max(2.0);
    let grid_side = mod_px * total_modules as f32;
    let card_side = grid_side + outer_pad * 2.0;

    let card_x = (slot.x + (slot.w - card_side) * 0.5).floor();
    let card_y = (slot.y + (slot.h - card_side) * 0.5).floor();
    let qr_card = RectF::new(card_x, card_y, card_side, card_side);

    // Pure white quiet-zone container for guaranteed contrast on dark & light themes.
    draw_list.push_rect(
        qr_card,
        Rgba::new(255, 255, 255, 255),
        Some(border_color),
        1.0,
        cap_radius(8.0, theme_rounding),
        None,
    );

    let origin_x = card_x + outer_pad + QUIET_MODULES as f32 * mod_px;
    let origin_y = card_y + outer_pad + QUIET_MODULES as f32 * mod_px;
    let dark = Rgba::new(0, 0, 0, 255);

    for row in 0..qr.size {
        let mut col = 0usize;
        while col < qr.size {
            if !qr.get(col, row) {
                col += 1;
                continue;
            }
            let start = col;
            while col < qr.size && qr.get(col, row) {
                col += 1;
            }
            let span_w = (col - start) as f32 * mod_px;
            let rect = RectF::new(
                origin_x + start as f32 * mod_px,
                origin_y + row as f32 * mod_px,
                span_w,
                mod_px,
            );
            draw_list.push_rect(rect, dark, None, 0.0, 0.0, None);
        }
    }
}

fn render_ios_pairing_hero(
    draw_list: &mut UiDrawList,
    hero: RectF,
    ios: &IosPairingSnapshot,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    draw_list.push_rect(
        hero,
        tokens.surface_pane,
        Some(tokens.border_hairline),
        1.0,
        cap_radius(8.0, theme_rounding),
        None,
    );

    let err_banner = ios.last_error.as_deref().or(match &ios.state {
        TailscalePairState::Disabled { error: Some(e) } => Some(e.as_str()),
        _ => None,
    });
    let content_h = if err_banner.is_some() {
        (hero.h - 26.0).max(0.0)
    } else {
        hero.h
    };

    match &ios.state {
        TailscalePairState::Ready { url } => {
            let qr_col_w = 198.0_f32.min((hero.w * 0.42).max(150.0));
            let qr_slot = RectF::new(
                hero.x + 10.0,
                hero.y + 8.0,
                qr_col_w,
                (content_h - 16.0).max(0.0),
            );
            if let Some(qr) = &ios.qr_modules {
                render_qr_matrix(draw_list, qr_slot, qr, tokens.border_subtle, theme_rounding);
            }

            let info_x = qr_slot.right() + 14.0;
            let info_w = (hero.right() - info_x - 12.0).max(0.0);
            let mut iy = hero.y + 14.0;

            // Live status indicator
            draw_list.push_rect(
                RectF::new(info_x, iy + 5.0, 8.0, 8.0),
                tokens.status_done,
                None,
                0.0,
                4.0,
                None,
            );
            draw_list.push_text(
                RectF::new(info_x + 14.0, iy, (info_w - 14.0).max(0.0), 18.0),
                "Ready to Pair · Rex for iOS",
                tokens.fg_primary,
                FontKind::Sans,
                13.0,
                FontWeight::SemiBold,
                TextAlign::Left,
                true,
            );
            iy += 24.0;

            let steps = [
                "1. Open Rex on iPhone or iPad → Connect to Linux",
                "2. Tap \"Pair via QR Code\" and scan this code",
                "3. Keep your iOS device on the same Tailscale tailnet",
            ];
            for step in steps {
                draw_list.push_text(
                    RectF::new(info_x, iy, info_w, 16.0),
                    step,
                    tokens.fg_secondary,
                    FontKind::Sans,
                    11.0,
                    FontWeight::Regular,
                    TextAlign::Left,
                    true,
                );
                iy += 18.0;
            }
            iy += 6.0;

            // Clickable Tailscale HTTPS URL box
            let copy_hit = HitTarget::SettingsToggle {
                key: "ios_copy_url".to_owned(),
            };
            let url_hovered = interaction.is_hovered(&copy_hit);
            let url_box = RectF::new(info_x, iy, info_w, 28.0);
            draw_list.push_rect(
                url_box,
                if url_hovered {
                    tokens.surface_row_hover
                } else {
                    tokens.surface_modal
                },
                Some(if url_hovered {
                    tokens.accent_primary
                } else {
                    tokens.border_subtle
                }),
                1.0,
                cap_radius(6.0, theme_rounding),
                None,
            );
            draw_list.push_icon(
                RectF::new(url_box.x + 8.0, url_box.y + 7.0, 14.0, 14.0),
                IconId::Link,
                tokens.accent_primary,
            );
            draw_list.push_text(
                RectF::new(
                    url_box.x + 26.0,
                    url_box.y + 6.0,
                    (url_box.w - 34.0).max(0.0),
                    16.0,
                ),
                url,
                tokens.accent_primary,
                FontKind::Mono,
                10.5,
                FontWeight::Medium,
                TextAlign::Left,
                true,
            );
            draw_list.push_hit(url_box, copy_hit, z_layer::MODAL + 2);
            iy += 34.0;

            // Optional Tailnet / account line
            let meta = match (ios.tailnet_name.as_deref(), ios.account_login.as_deref()) {
                (Some(tn), Some(acc)) if tn != acc => format!("Tailnet: {tn} ({acc})"),
                (Some(tn), _) => format!("Tailnet: {tn}"),
                (None, Some(acc)) => format!("Account: {acc}"),
                (None, None) => "Equivalent to: rex server tailscale pair".to_owned(),
            };
            draw_list.push_text(
                RectF::new(info_x, iy, info_w, 16.0),
                meta,
                tokens.fg_muted,
                FontKind::Mono,
                10.0,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );
        }
        other => {
            let (icon, accent, title, line1, line2, cta) = match other {
                TailscalePairState::Disabled { .. } => (
                    IconId::Globe,
                    tokens.accent_primary,
                    "Tailscale Remote Access is Off",
                    "Turn on Rex's built-in Tailscale node to generate a QR code",
                    "that Rex on iOS can scan directly over your private tailnet.",
                    Some((
                        "ios_toggle_tailscale",
                        if ios.busy {
                            "Starting Tailscale…"
                        } else {
                            "Enable Tailscale"
                        },
                    )),
                ),
                TailscalePairState::NeedsLogin { auth_url } => (
                    IconId::Link,
                    tokens.status_blocked,
                    "Sign In to Tailscale",
                    "Authenticate this Linux server with your Tailscale account.",
                    "Once signed in, the iOS QR pairing code will appear automatically.",
                    Some((
                        "ios_open_login_url",
                        if ios.busy {
                            "Requesting Link…"
                        } else if auth_url.is_some() {
                            "Open Tailscale Sign-In"
                        } else {
                            "Get Sign-In Link"
                        },
                    )),
                ),
                TailscalePairState::Starting => (
                    IconId::Globe,
                    tokens.status_working,
                    "Starting Tailscale…",
                    "Connecting the local Rex server to your Tailscale tailnet.",
                    "The QR code will appear automatically once HTTPS is ready.",
                    None,
                ),
                TailscalePairState::NeedsMachineApproval => (
                    IconId::Warning,
                    tokens.status_blocked,
                    "Waiting for Tailnet Approval",
                    "Approve this device in your Tailscale admin console.",
                    "The QR code will appear automatically once approved.",
                    None,
                ),
                TailscalePairState::HttpsPending { dns_name } => (
                    IconId::Globe,
                    tokens.status_working,
                    "Provisioning Tailscale HTTPS…",
                    if dns_name.is_some() {
                        "Tailscale is online; waiting for its HTTPS address to become ready."
                    } else {
                        "Waiting for Tailscale MagicDNS & HTTPS certificates to activate."
                    },
                    "Ensure HTTPS Certificates are enabled in Tailscale DNS settings.",
                    None,
                ),
                TailscalePairState::Unsupported => (
                    IconId::Server,
                    tokens.fg_muted,
                    "Built-in Tailscale Unavailable",
                    "The connected Rex server does not expose built-in Tailscale.",
                    "Update Rex or configure Tailscale on the local Rex daemon.",
                    None,
                ),
                TailscalePairState::ServerDisconnected => (
                    IconId::Server,
                    tokens.fg_muted,
                    "Local Rex Server Not Connected",
                    "Connect to the local Rex daemon to display your iOS QR code.",
                    "Rex iOS pairs with the local server's Tailscale HTTPS address.",
                    None,
                ),
                TailscalePairState::Ready { .. } => unreachable!(),
            };

            draw_list.push_icon(
                RectF::new(hero.x + 16.0, hero.y + 16.0, 18.0, 18.0),
                icon,
                accent,
            );
            draw_list.push_text(
                RectF::new(hero.x + 42.0, hero.y + 14.0, (hero.w - 56.0).max(0.0), 20.0),
                title,
                tokens.fg_primary,
                FontKind::Sans,
                13.5,
                FontWeight::SemiBold,
                TextAlign::Left,
                true,
            );
            draw_list.push_text(
                RectF::new(hero.x + 16.0, hero.y + 42.0, (hero.w - 32.0).max(0.0), 16.0),
                line1,
                tokens.fg_secondary,
                FontKind::Sans,
                11.5,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );
            draw_list.push_text(
                RectF::new(hero.x + 16.0, hero.y + 60.0, (hero.w - 32.0).max(0.0), 16.0),
                line2,
                tokens.fg_muted,
                FontKind::Sans,
                11.0,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );

            if let Some((cta_key, cta_label)) = cta {
                let cta_target = HitTarget::SettingsToggle {
                    key: cta_key.to_owned(),
                };
                let btn_w = ((cta_label.chars().count() as f32) * 7.0 + 28.0).clamp(140.0, 220.0);
                let btn = RectF::new(hero.x + 16.0, hero.y + 86.0, btn_w, 28.0);
                let hovered = interaction.is_hovered(&cta_target);
                draw_list.push_rect(
                    btn,
                    if hovered {
                        tokens.accent_primary.with_alpha_f32(0.88)
                    } else {
                        tokens.accent_primary
                    },
                    None,
                    0.0,
                    cap_radius(6.0, theme_rounding),
                    None,
                );
                draw_list.push_text(
                    btn,
                    cta_label,
                    tokens.accent_foreground,
                    FontKind::Sans,
                    11.5,
                    FontWeight::SemiBold,
                    TextAlign::Center,
                    true,
                );
                draw_list.push_hit(btn, cta_target, z_layer::MODAL + 2);
            } else if let TailscalePairState::HttpsPending {
                dns_name: Some(dns),
            } = other
            {
                draw_list.push_text(
                    RectF::new(hero.x + 16.0, hero.y + 88.0, (hero.w - 32.0).max(0.0), 16.0),
                    format!("Node: {dns}"),
                    tokens.fg_muted,
                    FontKind::Mono,
                    10.5,
                    FontWeight::Regular,
                    TextAlign::Left,
                    true,
                );
            }
        }
    }

    if let Some(err) = err_banner {
        let err_y = (hero.bottom() - 22.0).max(hero.y);
        draw_list.push_text(
            RectF::new(hero.x + 12.0, err_y, (hero.w - 24.0).max(0.0), 16.0),
            format!("Error: {err}"),
            tokens.status_error,
            FontKind::Mono,
            10.5,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_single_input_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    card_r: f32,
    shadow: ShadowSpec,
    title: &str,
    input: &TextInput,
    confirm_label: &str,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let card = RectF::new(left_x.round(), top_y.round(), modal_w.round(), 134.0);
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 12.0, (card.w - 28.0).max(0.0), 18.0),
        title,
        tokens.fg_primary,
        FontKind::Sans,
        13.5,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );

    let field = RectF::new(card.x + 14.0, card.y + 40.0, (card.w - 28.0).max(0.0), 34.0);
    draw_list.push_rect(
        field,
        tokens.surface_pane,
        Some(tokens.border_active),
        1.0,
        cap_radius(7.0, theme_rounding),
        None,
    );
    let (display, color) = if input.is_empty() {
        (input.placeholder().to_owned(), tokens.fg_muted)
    } else {
        (input.display_text().0, tokens.fg_primary)
    };
    draw_list.push_text(
        RectF::new(
            field.x + 10.0,
            field.y + 8.0,
            (field.w - 20.0).max(0.0),
            18.0,
        ),
        display,
        color,
        FontKind::Mono,
        12.5,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    draw_list.push_hit(field, HitTarget::ModalInput, z_layer::MODAL + 1);

    let ok_btn = RectF::new(
        (card.right() - 102.0).max(card.x),
        card.y + 88.0,
        88.0,
        30.0,
    );
    let cancel_btn = RectF::new((ok_btn.x - 86.0).max(card.x), card.y + 88.0, 78.0, 30.0);
    let btn_r = cap_radius(6.0, theme_rounding);

    let cancel_fill = if interaction.is_pressed(&HitTarget::ModalCancelButton) {
        tokens.surface_row_selected
    } else if interaction.is_hovered(&HitTarget::ModalCancelButton) {
        tokens.surface_row_active
    } else {
        tokens.surface_row_hover
    };
    draw_list.push_rect(
        cancel_btn,
        cancel_fill,
        Some(if interaction.is_hovered(&HitTarget::ModalCancelButton) {
            tokens.border_subtle
        } else {
            tokens.border_hairline
        }),
        1.0,
        btn_r,
        None,
    );
    draw_list.push_text(
        cancel_btn,
        "Cancel",
        if interaction.is_hovered(&HitTarget::ModalCancelButton) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::Medium,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(cancel_btn, HitTarget::ModalCancelButton, z_layer::MODAL + 1);

    let ok_fill = if interaction.is_pressed(&HitTarget::ModalConfirmButton) {
        tokens.accent_primary.with_alpha_f32(0.80)
    } else if interaction.is_hovered(&HitTarget::ModalConfirmButton) {
        tokens.accent_primary.with_alpha_f32(0.92)
    } else {
        tokens.accent_primary
    };
    draw_list.push_rect(ok_btn, ok_fill, None, 0.0, btn_r, None);
    draw_list.push_text(
        ok_btn,
        confirm_label,
        tokens.accent_foreground,
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(ok_btn, HitTarget::ModalConfirmButton, z_layer::MODAL + 1);
}

const SHORTCUT_HELP_ROWS: &[(&str, &str)] = &[
    ("Command Palette", "Ctrl+Shift+P"),
    ("Switch Session", "Ctrl+Shift+K"),
    ("Go to Anything", "Ctrl+Alt+P"),
    ("Activity Filter", "Ctrl+Shift+."),
    ("Toggle Sidebar", "Ctrl+Shift+B"),
    ("Toggle Toolbar Layout", "Ctrl+Shift+S"),
    ("New Terminal", "Ctrl+Shift+T"),
    ("New Terminal Session", "Ctrl+Shift+N"),
    ("Close Pane / Tab", "Ctrl+Shift+W / Ctrl+Alt+W"),
    ("Jump to Tab 1..9", "Alt+1 .. Alt+9"),
    ("Split Right / Down", "Ctrl+Shift+D / E"),
    ("Zoom / Float Pane", "Ctrl+Shift+Z / Ctrl+Alt+Enter"),
    ("Copy / Paste (Omarchy)", "Ctrl+Shift+C/V · Ctrl/Shift+Ins"),
    ("Find in Terminal", "Ctrl+Shift+F"),
    ("Settings", "Ctrl+Shift+,"),
    ("T3: Needs You Inbox", "Ctrl+Alt+I"),
    ("T3: New Thread / Project", "Ctrl+Alt+N / O"),
    ("T3: Answer… / Fit Device", "Ctrl+Alt+A / T"),
    ("T3: Parent / Subagent", "Ctrl+Alt+[ / ]"),
];

#[allow(clippy::too_many_arguments)]
fn render_shortcuts_help_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    max_h: f32,
    card_r: f32,
    shadow: ShadowSpec,
    scroll_row: usize,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let card_h = (46.0_f32 + 10.0 * 30.0 + 16.0).min(max_h);
    let card = RectF::new(
        left_x.round(),
        top_y.round(),
        modal_w.round(),
        card_h.round(),
    );
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 13.0, (card.w - 80.0).max(0.0), 20.0),
        "Keyboard Shortcuts (Omarchy Conflict-Free)",
        tokens.fg_primary,
        FontKind::Sans,
        13.5,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    draw_list.push_text(
        RectF::new(card.right() - 56.0, card.y + 14.0, 42.0, 18.0),
        "Esc",
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );
    draw_list.push_rect(
        RectF::new(card.x, card.y + 45.0, card.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let list = RectF::new(card.x, card.y + 46.0, card.w, (card.h - 46.0).max(0.0));
    draw_list.push_clip(list);
    let mut y = list.y + 8.0;
    for (label, chord) in SHORTCUT_HELP_ROWS.iter().skip(scroll_row).take(10) {
        let row = RectF::new(list.x + 14.0, y, (list.w - 28.0).max(0.0), 26.0);
        draw_list.push_text(
            RectF::new(row.x, row.y + 4.0, (row.w - 170.0).max(0.0), 18.0),
            *label,
            tokens.fg_secondary,
            FontKind::Sans,
            12.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        let pill = RectF::new((row.right() - 166.0).max(row.x), row.y + 3.0, 166.0, 20.0);
        draw_list.push_rect(
            pill,
            tokens.surface_pane,
            Some(tokens.border_hairline),
            1.0,
            cap_radius(5.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            pill,
            *chord,
            tokens.accent_primary,
            FontKind::Mono,
            10.5,
            FontWeight::Medium,
            TextAlign::Center,
            true,
        );
        y += 30.0;
    }
    render_modal_scrollbar(
        draw_list,
        list,
        scroll_row,
        10,
        SHORTCUT_HELP_ROWS.len(),
        tokens,
    );
    draw_list.pop_clip();
}

/// Active key-mode or multi-key chord prefix displayed in the bottom-right which-key popup.
#[derive(Debug, Clone)]
pub struct WhichKeyState {
    pub header: String,
    pub exclusive: bool,
    pub hints: Vec<WhichKeyHint>,
}

/// Renders the bottom-right which-key chord/mode hint bar (`terminal.keyboard.sequence_hints`).
pub fn render_which_key(
    draw_list: &mut UiDrawList,
    workspace: RectF,
    state: &WhichKeyState,
    theme: &Theme,
) {
    if workspace.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();
    let visible_hints = state.hints.len().min(8);
    let cols = if visible_hints > 4 { 2 } else { 1 };
    let rows = visible_hints.div_ceil(cols).max(1);
    let w = (if cols == 2 { 390.0_f32 } else { 230.0_f32 }).min((workspace.w - 28.0).max(120.0));
    let h = 34.0 + rows as f32 * 24.0 + 8.0;
    let card = RectF::new(
        (workspace.right() - w - 14.0).max(workspace.x + 8.0),
        (workspace.bottom() - h - 14.0).max(workspace.y + 8.0),
        w,
        h,
    );

    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_active),
        1.0,
        cap_radius(8.0, theme.rounding),
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 8.0,
            blur_radius: 24.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );

    let title = if state.exclusive {
        format!("{} · exclusive · Esc exits", state.header)
    } else {
        format!("{} · Esc cancels", state.header)
    };
    draw_list.push_text(
        RectF::new(card.x + 12.0, card.y + 8.0, (card.w - 24.0).max(0.0), 18.0),
        title,
        tokens.accent_primary,
        FontKind::Mono,
        11.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );

    let col_w = ((card.w - 24.0) / cols as f32).max(40.0);
    for (idx, hint) in state.hints.iter().take(8).enumerate() {
        let col = idx % cols;
        let row = idx / cols;
        let hx = card.x + 12.0 + col as f32 * col_w;
        let hy = card.y + 32.0 + row as f32 * 24.0;
        let label = match (&hint.action, hint.is_prefix) {
            (Some(act), true) => format!("{act} …"),
            (Some(act), false) => act.clone(),
            (None, true) => "+prefix".to_owned(),
            (None, false) => String::new(),
        };
        draw_list.push_text(
            RectF::new(hx, hy, 56.0, 18.0),
            &hint.display_key,
            tokens.fg_primary,
            FontKind::Mono,
            11.0,
            FontWeight::Bold,
            TextAlign::Left,
            true,
        );
        draw_list.push_text(
            RectF::new(hx + 60.0, hy, (col_w - 64.0).max(0.0), 18.0),
            label,
            tokens.fg_muted,
            FontKind::Sans,
            11.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
    }
}

/// Inline terminal Find bar state (`Ctrl+Shift+F`).
#[derive(Debug, Clone)]
pub struct FindBarState {
    pub open: bool,
    pub input: TextInput,
    pub case_sensitive: bool,
    pub regex: bool,
    pub current_match: usize,
    pub total_matches: usize,
}

impl Default for FindBarState {
    fn default() -> Self {
        Self {
            open: false,
            input: TextInput::new("Find in terminal…"),
            case_sensitive: false,
            regex: false,
            current_match: 0,
            total_matches: 0,
        }
    }
}

/// Renders the inline terminal Find bar (`336×34px`) anchored top-right inside the focused pane.
pub fn render_find_bar(
    draw_list: &mut UiDrawList,
    pane_rect: RectF,
    state: &FindBarState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
) {
    if !state.open || pane_rect.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();
    let w = 336.0_f32.min((pane_rect.w - 20.0).max(160.0));
    let bar = RectF::new(
        (pane_rect.right() - w - 10.0).max(pane_rect.x + 6.0),
        pane_rect.y + 10.0,
        w,
        34.0,
    );
    draw_list.push_rect(
        bar,
        tokens.surface_modal,
        Some(tokens.border_active),
        1.0,
        cap_radius(8.0, theme.rounding),
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 8.0,
            blur_radius: 20.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );

    let close_btn = RectF::new(bar.right() - 26.0, bar.y + 6.0, 22.0, 22.0);
    let next_btn = RectF::new(close_btn.x - 22.0, bar.y + 6.0, 20.0, 22.0);
    let prev_btn = RectF::new(next_btn.x - 22.0, bar.y + 6.0, 20.0, 22.0);
    let regex_btn = RectF::new(prev_btn.x - 26.0, bar.y + 6.0, 24.0, 22.0);
    let case_btn = RectF::new(regex_btn.x - 26.0, bar.y + 6.0, 24.0, 22.0);
    let count_rect = RectF::new(case_btn.x - 46.0, bar.y + 8.0, 42.0, 18.0);
    let input_rect = RectF::new(
        bar.x + 8.0,
        bar.y + 7.0,
        (count_rect.x - bar.x - 12.0).max(20.0),
        20.0,
    );

    let (display, color) = if state.input.is_empty() {
        (state.input.placeholder().to_owned(), tokens.fg_muted)
    } else {
        (state.input.text().to_owned(), tokens.fg_primary)
    };
    draw_list.push_text(
        input_rect,
        display,
        color,
        FontKind::Mono,
        12.0,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    let caret_x = input_rect.x
        + (state.input.text().chars().count() as f32 * 7.2).min((input_rect.w - 2.0).max(0.0));
    draw_list.push_rect(
        RectF::new(caret_x, input_rect.y + 3.0, 1.5, 14.0),
        tokens.accent_primary,
        None,
        0.0,
        0.75,
        None,
    );
    draw_list.push_hit(input_rect, HitTarget::FindBarInput, z_layer::FIND_BAR + 1);

    let count_label = if state.total_matches == 0 {
        "0/0".to_owned()
    } else {
        format!(
            "{}/{}",
            state.current_match.min(state.total_matches),
            state.total_matches
        )
    };
    draw_list.push_text(
        count_rect,
        count_label,
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );

    let toggle_r = cap_radius(4.0, theme.rounding);
    if state.case_sensitive {
        draw_list.push_rect(case_btn, tokens.accent_primary, None, 0.0, toggle_r, None);
    } else if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::FindBarCaseToggle, false, &tokens)
    {
        draw_list.push_rect(case_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        case_btn,
        "Aa",
        if state.case_sensitive {
            tokens.accent_foreground
        } else if interaction.is_hovered(&HitTarget::FindBarCaseToggle) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Mono,
        10.5,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(
        case_btn,
        HitTarget::FindBarCaseToggle,
        z_layer::FIND_BAR + 1,
    );

    if state.regex {
        draw_list.push_rect(regex_btn, tokens.accent_primary, None, 0.0, toggle_r, None);
    } else if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::FindBarRegexToggle, false, &tokens)
    {
        draw_list.push_rect(regex_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        regex_btn,
        ".*",
        if state.regex {
            tokens.accent_foreground
        } else if interaction.is_hovered(&HitTarget::FindBarRegexToggle) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Mono,
        10.5,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(
        regex_btn,
        HitTarget::FindBarRegexToggle,
        z_layer::FIND_BAR + 1,
    );

    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::FindBarPrev, false, &tokens)
    {
        draw_list.push_rect(prev_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        prev_btn,
        "↑",
        if interaction.is_hovered(&HitTarget::FindBarPrev) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(prev_btn, HitTarget::FindBarPrev, z_layer::FIND_BAR + 1);

    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::FindBarNext, false, &tokens)
    {
        draw_list.push_rect(next_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        next_btn,
        "↓",
        if interaction.is_hovered(&HitTarget::FindBarNext) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(next_btn, HitTarget::FindBarNext, z_layer::FIND_BAR + 1);

    let (close_bg, close_tint) =
        interaction.danger_button_style(&HitTarget::FindBarClose, tokens.fg_secondary, &tokens);
    if let Some((fill, border)) = close_bg {
        draw_list.push_rect(close_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_icon(close_btn.inset_uniform(4.0), IconId::Close, close_tint);
    draw_list.push_hit(close_btn, HitTarget::FindBarClose, z_layer::FIND_BAR + 1);
}

/// 6-position anchor grid for in-app floating notifications (`notifications.overview.position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastAnchor {
    TopLeft,
    TopCenter,
    #[default]
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastSeverity {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastItem {
    pub id: u64,
    pub severity: ToastSeverity,
    pub title: String,
    pub body: String,
}

/// Renders stacked in-app toast banners at any of the 6 Rex anchor positions.
pub fn render_toasts(
    draw_list: &mut UiDrawList,
    workspace: RectF,
    anchor: ToastAnchor,
    toasts: &[ToastItem],
    interaction: UiInteraction<'_>,
    theme: &Theme,
) {
    if toasts.is_empty() || workspace.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();
    const TOAST_W: f32 = 330.0;
    const TOAST_H: f32 = 58.0;
    const INSET: f32 = 14.0;
    const GAP: f32 = 8.0;

    let w = TOAST_W.min((workspace.w - INSET * 2.0).max(140.0));
    let is_bottom = matches!(
        anchor,
        ToastAnchor::BottomLeft | ToastAnchor::BottomCenter | ToastAnchor::BottomRight
    );

    for (idx, toast) in toasts.iter().take(4).enumerate() {
        let x = match anchor {
            ToastAnchor::TopLeft | ToastAnchor::BottomLeft => workspace.x + INSET,
            ToastAnchor::TopCenter | ToastAnchor::BottomCenter => {
                workspace.x + (workspace.w - w) * 0.5
            }
            ToastAnchor::TopRight | ToastAnchor::BottomRight => {
                (workspace.right() - INSET - w).max(workspace.x)
            }
        };
        let stack = idx as f32 * (TOAST_H + GAP);
        let y = if is_bottom {
            (workspace.bottom() - INSET - TOAST_H - stack).max(workspace.y)
        } else {
            workspace.y + INSET + stack
        };
        let card = RectF::new(x.round(), y.round(), w.round(), TOAST_H);

        let (accent, icon) = match toast.severity {
            ToastSeverity::Info => (tokens.accent_primary, IconId::Activity),
            ToastSeverity::Success => (tokens.status_done, IconId::Check),
            ToastSeverity::Warning => (tokens.status_blocked, IconId::Warning),
            ToastSeverity::Error => (tokens.status_error, IconId::Error),
        };

        draw_list.push_rect(
            card,
            tokens.surface_modal,
            Some(accent),
            1.0,
            cap_radius(10.0, theme.rounding),
            Some(ShadowSpec {
                offset_x: 0.0,
                offset_y: 8.0,
                blur_radius: 24.0,
                spread: 0.0,
                color: tokens.shadow_color,
            }),
        );
        draw_list.push_icon(
            RectF::new(card.x + 12.0, card.y + 12.0, 15.0, 15.0),
            icon,
            accent,
        );
        draw_list.push_text(
            RectF::new(card.x + 34.0, card.y + 10.0, (card.w - 64.0).max(0.0), 17.0),
            &toast.title,
            tokens.fg_primary,
            FontKind::Sans,
            12.5,
            FontWeight::SemiBold,
            TextAlign::Left,
            true,
        );
        draw_list.push_text(
            RectF::new(card.x + 34.0, card.y + 29.0, (card.w - 48.0).max(0.0), 18.0),
            &toast.body,
            tokens.fg_muted,
            FontKind::Sans,
            11.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );

        let dismiss = RectF::new(card.right() - 26.0, card.y + 8.0, 18.0, 18.0);
        let dismiss_target = HitTarget::ToastDismiss { id: toast.id };
        let (dismiss_bg, dismiss_tint) =
            interaction.danger_button_style(&dismiss_target, tokens.fg_muted, &tokens);
        if let Some((fill, border)) = dismiss_bg {
            draw_list.push_rect(
                dismiss,
                fill,
                border,
                1.0,
                cap_radius(4.0, theme.rounding),
                None,
            );
        }
        draw_list.push_icon(dismiss.inset_uniform(3.0), IconId::Close, dismiss_tint);
        draw_list.push_hit(dismiss, dismiss_target, z_layer::TOAST + 2);
        draw_list.push_hit(
            card,
            HitTarget::ToastBody { id: toast.id },
            z_layer::TOAST + 1,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_gap02_modal_scrollbar_clamps_safely_on_short_list_rect() {
        let mut dl = UiDrawList::new();
        let tokens = Theme::default().effective_ui();
        // list_rect.h = 28.0 -> >= 24.0 guard, track_h = 16.0 < 20.0.
        render_modal_scrollbar(
            &mut dl,
            RectF::new(0.0, 0.0, 400.0, 28.0),
            2,
            8,
            30,
            &tokens,
        );
        dl.validate_invariants()
            .expect("short modal scrollbar draw list must be valid");
        assert_eq!(dl.primitives.len(), 1);
        match &dl.primitives[0] {
            crate::draw::UiPrimitive::RoundedRect { rect, .. } => {
                assert!(rect.h > 0.0 && rect.h <= 16.0 + 0.01);
            }
            other => panic!("expected RoundedRect primitive, got {other:?}"),
        }
    }
}
