//! First-class `tr3x` and Rex Lua action, prompt (`rex.ui`), perform, and keymap bridge.

pub mod keymap;
pub mod perform;
pub mod prompts;
pub mod proto_bridge;
pub mod t3_session;

use std::collections::HashMap;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use keymap::{
    ClientKeymapResponse, KeyBinding, KeyChord, KeyMode, KeyResolution, KeymapConfigMeta,
    KeymapEngine, KeymapOptions, Modifiers, WhichKeyHint, WireActionDescriptor, WireKeyBinding,
    canonicalize_key_name, omarex_default_bindings,
};
pub use perform::{
    CLIENT_CALL_PROXY_TIMEOUT, ClientActionItem, ClientActionResponse, ClientCallRequest,
    ClientCommand, ClientInfo, ClientPerformRequest, ClientPrincipal, ClientRequester,
    PaneDirection, ProxyCallPlan, SplitDirection, authorize_principal, dispatch_queued_actions,
    handle_perform_request, parse_client_command, plan_remote_call,
};
pub use prompts::{
    ClientReplyError, ClientReplyPayload, FilteredPromptRow, PromptEndedEvent, PromptIcon,
    PromptItem, PromptKind, PromptManager, PromptOpenOutcome, PromptRequest, PromptSession,
    PromptSpec,
};
pub use proto_bridge::{
    BlockCardContext, OMAREX_DEFAULT_KEYMAP_LAYER, ProtoPromptOutcome, WindowTabCard,
    apply_proto_keymap, apply_proto_keymap_to_engine, build_client_key_request,
    build_proto_keymap_request, build_proto_keymap_request_with_overrides,
    build_session_tab_card_metadata, build_session_tab_cards, build_window_tab_card,
    chord_key_to_rex_code, dispatch_proto_queued_actions, extract_project_name_from_pwd,
    friendly_terminal_title, handle_proto_perform_request, is_placeholder_window_label,
    key_event_from_chord, keymap_changed_needs_refresh, keymap_response_from_proto,
    on_proto_prompt_ended, on_proto_prompt_requested, plan_proto_remote_call,
    program_status_from_proto, program_statuses_from_response, proxy_reply_error, proxy_reply_ok,
    reply_to_proto,
};
pub use t3_session::{
    BlockedKind, FloatingRect, PR_DRAWER_WIDE_COLS, PriorityDot, ProgramState, ProgramStatusRecord,
    ProviderKind, RolledUpStatus, T3BlockTag, TabCardMetadata, WorkingIndicator, build_tab_card,
    format_relative_age, is_pr_layer, is_t3_session, parse_t3_block_tag, pr_drawer_rect,
    rollup_records, sanitize_status_text, t3_session_machine,
};

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("unknown action: {0}")]
    UnknownAction(String),
    #[error("invalid key spec: {0}")]
    InvalidKeySpec(String),
    #[error("protocol error: {0}")]
    Proto(#[from] rex_proto::ProtoError),
}

/// Origin of an action inside [`ActionRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Client,
    Lua,
    Block,
}

impl ActionKind {
    pub fn from_wire(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "lua" => Self::Lua,
            "block" => Self::Block,
            _ => Self::Client,
        }
    }

    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Lua => "lua",
            Self::Block => "block",
        }
    }
}

/// Unified command palette action descriptor combining built-in client actions,
/// server Lua actions (`init.lua` / `tr3x.lua`), and terminal block actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionEntry {
    pub name: String,
    pub kind: ActionKind,
    pub title: String,
    pub category: String,
    pub keywords: String,
    pub shortcut_hint: Option<String>,
    pub is_tr3x: bool,
}

/// Fuzzy search match returned by [`ActionRegistry::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionMatch {
    pub entry: ActionEntry,
    pub score: u32,
    pub title_indices: Vec<u32>,
}

/// Canonical list of all 35 `tr3x` Lua actions verified in `docs/protocol-spec.md §6.5`.
pub const TR3X_ACTION_NAMES: &[&str] = &[
    "t3_settle",
    "t3_reopen_settled",
    "t3_pin",
    "t3_unpin",
    "t3_rename",
    "t3_retitle",
    "t3_fork",
    "t3_archive",
    "t3_delete",
    "t3_stop",
    "t3_force_stop",
    "t3_model",
    "t3_snooze",
    "t3_snooze_hour",
    "t3_snooze_3h",
    "t3_snooze_tomorrow",
    "t3_inbox",
    "t3_approve",
    "t3_approve_always",
    "t3_decline",
    "t3_answer",
    "t3_answer_next",
    "t3_parent",
    "t3_child",
    "t3_new",
    "t3_project",
    "t3_find",
    "t3_open",
    "t3_pr",
    "t3_pr_create",
    "t3_fit",
    "t3_pair",
    "t3_update",
    "t3_sessions_per_machine",
    "t3_sessions_merged",
];

/// Unified action catalog and keymap coordinator for `omarex`.
#[derive(Debug, Clone)]
pub struct ActionRegistry {
    builtins: Vec<ActionEntry>,
    actions: Vec<ActionEntry>,
    by_name: HashMap<String, usize>,
    keymap: KeymapEngine,
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self::new(KeymapOptions::default())
    }
}

impl ActionRegistry {
    pub fn new(options: KeymapOptions) -> Self {
        let builtins = builtin_client_actions();
        let keymap = KeymapEngine::new(options);
        let mut reg = Self {
            builtins,
            actions: Vec::new(),
            by_name: HashMap::new(),
            keymap,
        };
        reg.rebuild_actions(&[]);
        reg
    }

    pub fn keymap(&self) -> &KeymapEngine {
        &self.keymap
    }

    pub fn keymap_mut(&mut self) -> &mut KeymapEngine {
        &mut self.keymap
    }

    pub fn builtins(&self) -> &[ActionEntry] {
        &self.builtins
    }

    /// Returns `true` when an incoming `keymap_changed` hash requires re-calling `client.keymap`.
    pub fn needs_keymap_refresh(&self, hash: &str) -> bool {
        self.keymap.needs_refresh(hash)
    }

    /// Builds the typed Rex API rev 24 `client.keymap` registration request (using `KeymapLayer`).
    pub fn build_proto_keymap_request(&self) -> rex_proto::types::ClientKeymapRequest {
        proto_bridge::build_proto_keymap_request(self)
    }

    /// Builds the exact `client.keymap` RPC registration payload for Rex API rev 24
    /// (`docs/protocol-spec.md §6.1`) with `defaults` wrapped in a `KeymapLayer`.
    pub fn build_client_keymap_request(&self) -> serde_json::Value {
        serde_json::to_value(self.build_proto_keymap_request()).unwrap_or_default()
    }

    /// Merges a wire `rex_proto::types::ClientKeymapResponse` into both the keymap engine and catalog.
    pub fn apply_proto_keymap(&mut self, response: &rex_proto::types::ClientKeymapResponse) {
        proto_bridge::apply_proto_keymap(self, response);
    }

    /// Merges a server `client.keymap` response into both the keymap engine and action catalog.
    pub fn apply_server_keymap(&mut self, response: &ClientKeymapResponse) {
        self.keymap.apply_server_keymap(response);
        self.rebuild_actions(&response.actions);
    }

    pub fn actions(&self) -> &[ActionEntry] {
        &self.actions
    }

    pub fn get(&self, name: &str) -> Option<&ActionEntry> {
        self.by_name.get(name).map(|&i| &self.actions[i])
    }

    /// Fuzzy-searches all registered actions across `title`, `category`, `name`, and `keywords`.
    pub fn search(&self, query: &str) -> Vec<ActionMatch> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return self
                .actions
                .iter()
                .cloned()
                .map(|entry| ActionMatch {
                    entry,
                    score: 0,
                    title_indices: Vec::new(),
                })
                .collect();
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(trimmed, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut title_buf = Vec::new();
        let mut matches = Vec::new();

        for entry in &self.actions {
            let combined = format!(
                "{} {} {} {}",
                entry.title, entry.category, entry.name, entry.keywords
            );
            buf.clear();
            if let Some(score) = pattern.score(Utf32Str::new(&combined, &mut buf), &mut matcher) {
                let mut indices = Vec::new();
                title_buf.clear();
                let _ = pattern.indices(
                    Utf32Str::new(&entry.title, &mut title_buf),
                    &mut matcher,
                    &mut indices,
                );
                matches.push(ActionMatch {
                    entry: entry.clone(),
                    score,
                    title_indices: indices,
                });
            }
        }

        matches.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.entry.title.cmp(&b.entry.title))
        });
        matches
    }

    fn rebuild_actions(&mut self, server_actions: &[WireActionDescriptor]) {
        self.actions.clear();
        self.by_name.clear();

        let mut push_or_update = |mut entry: ActionEntry| {
            entry.shortcut_hint = self.keymap.display_shortcut_for_action(&entry.name);
            if let Some(&idx) = self.by_name.get(&entry.name) {
                self.actions[idx] = entry;
            } else {
                let idx = self.actions.len();
                self.by_name.insert(entry.name.clone(), idx);
                self.actions.push(entry);
            }
        };

        for b in &self.builtins {
            push_or_update(b.clone());
        }

        for w in server_actions {
            let kind = ActionKind::from_wire(&w.kind);
            // Per Rex API rev 24, block methods (no `title`) are bindable but omitted from menus.
            if kind == ActionKind::Block && w.title.trim().is_empty() {
                continue;
            }
            let is_tr3x = w.name.starts_with("t3_") || w.category.eq_ignore_ascii_case("T3 Code");
            let title = if w.title.is_empty() {
                w.name.clone()
            } else {
                w.title.clone()
            };
            let category = if w.category.is_empty() {
                if is_tr3x {
                    "T3 Code".to_owned()
                } else {
                    "Actions".to_owned()
                }
            } else {
                w.category.clone()
            };
            push_or_update(ActionEntry {
                name: w.name.clone(),
                kind,
                title,
                category,
                keywords: w.keywords.clone(),
                shortcut_hint: None,
                is_tr3x,
            });
        }
    }
}

/// Built-in native client actions registered in `client.keymap`.
pub fn builtin_client_actions() -> Vec<ActionEntry> {
    let raw: &[(&str, &str, &str, &str)] = &[
        (
            "session.select",
            "Select Session / Window / Block",
            "Navigation",
            "switch jump focus",
        ),
        (
            "session.switch",
            "Switch Session…",
            "Session",
            "change workspace",
        ),
        ("session.new", "New Session", "Session", "create workspace"),
        (
            "session.new.on_host",
            "New Session on Host…",
            "Session",
            "remote tailscale ssh",
        ),
        (
            "session.refresh",
            "Refresh Sessions",
            "Session",
            "reload sync",
        ),
        (
            "session.undo_close",
            "Undo Close",
            "Session",
            "reopen restore",
        ),
        (
            "client.palette.toggle",
            "Command Palette…",
            "View",
            "commands actions search",
        ),
        (
            "pane.go_to",
            "Go to Anything…",
            "Navigation",
            "jump search tab pane",
        ),
        (
            "client.sidebar.toggle",
            "Toggle Sidebar",
            "View",
            "sessions panel hide show",
        ),
        (
            "client.tab_style.toggle",
            "Toggle Toolbar Layout",
            "View",
            "left right top bottom tabs",
        ),
        (
            "client.sidebar.activity.toggle",
            "Toggle Activity Filter",
            "View",
            "agents blocked working",
        ),
        (
            "client.sidebar.filter.focus",
            "Focus Tab Filter",
            "View",
            "search sidebar",
        ),
        (
            "client.toolbar.reveal",
            "Reveal Toolbar",
            "View",
            "show tabs",
        ),
        (
            "client.pane_header.reveal",
            "Reveal Pane Header",
            "View",
            "show header",
        ),
        (
            "client.notice.dismiss",
            "Dismiss Notice",
            "View",
            "toast notification clear",
        ),
        (
            "client.navigate.back",
            "Navigate Back",
            "Navigation",
            "history previous",
        ),
        (
            "client.navigate.forward",
            "Navigate Forward",
            "Navigation",
            "history next",
        ),
        (
            "client.settings",
            "Settings…",
            "Settings",
            "preferences config",
        ),
        (
            "client.diagnostics",
            "Rex Diagnostics",
            "System",
            "debug info version gpu",
        ),
        (
            "client.config.reload",
            "Reload Rex Config",
            "System",
            "init.lua keymap",
        ),
        (
            "client.theme.change",
            "Change Theme…",
            "View",
            "omarchy dark light colors",
        ),
        ("client.window.new", "New Window", "Session", "window"),
        ("client.tab.new", "New Tab", "Tab", "window create"),
        ("client.tab.close", "Close Tab", "Tab", "window"),
        ("client.tab.next", "Next Tab", "Tab", "window"),
        ("client.tab.previous", "Previous Tab", "Tab", "window"),
        ("client.tab.goto", "Go to Tab", "Tab", "window index"),
        (
            "client.tab.move.backward",
            "Move Tab Backward",
            "Tab",
            "reorder",
        ),
        (
            "client.tab.move.forward",
            "Move Tab Forward",
            "Tab",
            "reorder",
        ),
        (
            "client.tab.unread",
            "Mark Tab as Unread",
            "Tab",
            "badge dot",
        ),
        (
            "client.tab.mute",
            "Mute / Unmute Tab Notifications",
            "Tab",
            "bell silence",
        ),
        ("client.close", "Close Pane or Tab", "Pane", "close"),
        ("pane.split", "Split Pane", "Pane", "divide"),
        ("pane.split.right", "Split Pane Right", "Pane", "horizontal"),
        ("pane.split.down", "Split Pane Down", "Pane", "vertical"),
        ("pane.split.left", "Split Pane Left", "Pane", "horizontal"),
        ("pane.split.up", "Split Pane Up", "Pane", "vertical"),
        ("pane.split.auto", "Split Pane Best Fit", "Pane", "auto"),
        ("pane.close", "Close Pane", "Pane", "kill"),
        ("pane.zoom", "Toggle Pane Zoom", "Pane", "maximize focus"),
        ("pane.float", "Float / Dock Pane", "Pane", "floating layer"),
        (
            "pane.minimize_floating",
            "Minimize Floating Pane",
            "Pane",
            "capsule hide",
        ),
        (
            "pane.minimize_all_floating",
            "Minimize / Restore All Floating",
            "Pane",
            "capsules",
        ),
        (
            "pane.balance",
            "Balance Split Panes",
            "Pane",
            "equalize resize",
        ),
        ("pane.focus", "Focus Pane", "Navigation", "direction"),
        ("pane.focus.left", "Focus Pane Left", "Navigation", "split"),
        (
            "pane.focus.right",
            "Focus Pane Right",
            "Navigation",
            "split",
        ),
        ("pane.focus.up", "Focus Pane Up", "Navigation", "split"),
        ("pane.focus.down", "Focus Pane Down", "Navigation", "split"),
        ("pane.focus_next", "Focus Next Pane", "Navigation", "cycle"),
        (
            "pane.focus_previous",
            "Focus Previous Pane",
            "Navigation",
            "cycle",
        ),
        ("pane.move.left", "Move Pane Left", "Pane", "swap"),
        ("pane.move.right", "Move Pane Right", "Pane", "swap"),
        ("pane.move.up", "Move Pane Up", "Pane", "swap"),
        ("pane.move.down", "Move Pane Down", "Pane", "swap"),
        ("pane.copy", "Copy Selection", "Edit", "clipboard"),
        ("pane.paste", "Paste", "Edit", "clipboard image"),
        (
            "pane.copy_mode.enter",
            "Enter Copy Mode",
            "Edit",
            "vim scrollback yank",
        ),
        (
            "pane.go_to_directory",
            "Go to Directory…",
            "Navigation",
            "cd folder",
        ),
        ("pane.insert_path", "Insert Path…", "Edit", "file browser"),
        (
            "client.find.open",
            "Find in Pane…",
            "Edit",
            "search scrollback",
        ),
        ("client.find.next", "Find Next Match", "Edit", "search"),
        (
            "client.find.previous",
            "Find Previous Match",
            "Edit",
            "search",
        ),
        (
            "client.prompt.previous",
            "Jump to Previous Prompt",
            "Navigation",
            "osc 133",
        ),
        (
            "client.prompt.next",
            "Jump to Next Prompt",
            "Navigation",
            "osc 133",
        ),
        (
            "client.font.increase",
            "Increase Font Size",
            "View",
            "zoom text bigger",
        ),
        (
            "client.font.decrease",
            "Decrease Font Size",
            "View",
            "zoom text smaller",
        ),
        (
            "client.font.reset",
            "Reset Font Size",
            "View",
            "default text",
        ),
        (
            "client.quick_look",
            "Quick Look Preview",
            "View",
            "file image link",
        ),
        ("client.mode.enter", "Enter Key Mode", "Keymap", "mode"),
        ("client.mode.exit", "Exit Key Mode", "Keymap", "mode"),
        (
            "client.mode.exit_all",
            "Exit All Key Modes",
            "Keymap",
            "mode",
        ),
    ];

    raw.iter()
        .map(|(name, title, cat, kw)| ActionEntry {
            name: (*name).to_owned(),
            kind: ActionKind::Client,
            title: (*title).to_owned(),
            category: (*cat).to_owned(),
            keywords: (*kw).to_owned(),
            shortcut_hint: None,
            is_tr3x: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn parses_rex_key_chords_sequences_and_modes() {
        let c = KeyChord::parse("ctrl+cmd+i").expect("parse ctrl+cmd+i");
        assert_eq!(c.mods, Modifiers::CTRL_SUPER);
        assert_eq!(c.key, "i");

        let (mode, seq) = KeyChord::parse_spec("ctrl+b>c").expect("parse sequence");
        assert_eq!(mode, None);
        assert_eq!(seq.len(), 2);
        assert_eq!(seq[0], KeyChord::new(Modifiers::CTRL, "b"));
        assert_eq!(seq[1], KeyChord::new(Modifiers::NONE, "c"));

        let (mode2, seq2) = KeyChord::parse_spec("resize/left").expect("parse mode key");
        assert_eq!(mode2.as_deref(), Some("resize"));
        assert_eq!(seq2, vec![KeyChord::new(Modifiers::NONE, "left")]);

        // Literal slash with modifiers must NOT be misinterpreted as a mode prefix.
        let (mode3, seq3) = KeyChord::parse_spec("ctrl+alt+/").expect("parse ctrl+alt+/");
        assert_eq!(mode3, None);
        assert_eq!(seq3, vec![KeyChord::new(Modifiers::CTRL_ALT, "/")]);
    }

    #[test]
    fn keymap_auto_aliases_tr3x_ctrl_cmd_to_ctrl_alt_and_preserves_passthrough() {
        let mut engine = KeymapEngine::default();
        let tr3x_response = ClientKeymapResponse {
            bindings: vec![
                WireKeyBinding {
                    keys: vec!["ctrl+cmd+i".to_owned()],
                    action: "t3_inbox".to_owned(),
                    args: None,
                    mode: None,
                    source: Some("tr3x.lua:430".to_owned()),
                },
                WireKeyBinding {
                    keys: vec!["ctrl+cmd+a".to_owned()],
                    action: "t3_answer".to_owned(),
                    args: None,
                    mode: None,
                    source: Some("tr3x.lua:433".to_owned()),
                },
                WireKeyBinding {
                    keys: vec!["ctrl+cmd+[".to_owned()],
                    action: "t3_parent".to_owned(),
                    args: None,
                    mode: None,
                    source: Some("tr3x.lua:434".to_owned()),
                },
                WireKeyBinding {
                    keys: vec!["ctrl+cmd+t".to_owned()],
                    action: "t3_fit".to_owned(),
                    args: None,
                    mode: None,
                    source: Some("tr3x.lua:480".to_owned()),
                },
            ],
            actions: vec![],
            modes: vec![],
            diagnostics: vec![],
            config: KeymapConfigMeta {
                hash: "sha256-abc".to_owned(),
                generation: 2,
                ..Default::default()
            },
        };
        engine.apply_server_keymap(&tr3x_response);
        assert!(!engine.needs_refresh("sha256-abc"));
        assert!(engine.needs_refresh("sha256-def"));

        // Hyprland-conflict-free Ctrl+Alt+I triggers t3_inbox.
        assert_eq!(
            engine.resolve_key(KeyChord::new(Modifiers::CTRL_ALT, "i")),
            KeyResolution::Action {
                action: "t3_inbox".to_owned(),
                args: None,
                source: Some("tr3x.lua:430".to_owned()),
            }
        );
        // Super+Ctrl+I still works too if delivered by compositor.
        assert_eq!(
            engine.resolve_key(KeyChord::new(Modifiers::CTRL_SUPER, "i")),
            KeyResolution::Action {
                action: "t3_inbox".to_owned(),
                args: None,
                source: Some("tr3x.lua:430".to_owned()),
            }
        );
        // Ctrl+Alt+[ triggers t3_parent, Ctrl+Alt+T triggers t3_fit.
        assert!(matches!(
            engine.resolve_key(KeyChord::new(Modifiers::CTRL_ALT, "[")),
            KeyResolution::Action { action, .. } if action == "t3_parent"
        ));
        assert!(matches!(
            engine.resolve_key(KeyChord::new(Modifiers::CTRL_ALT, "t")),
            KeyResolution::Action { action, .. } if action == "t3_fit"
        ));

        // Omarchy Super+C / Super+V translations (Ctrl+Insert / Shift+Insert).
        assert!(matches!(
            engine.resolve_key(KeyChord::new(Modifiers::CTRL, "insert")),
            KeyResolution::Action { action, .. } if action == "pane.copy"
        ));
        assert!(matches!(
            engine.resolve_key(KeyChord::new(Modifiers::SHIFT, "insert")),
            KeyResolution::Action { action, .. } if action == "pane.paste"
        ));

        // Tabs Alt+1..9.
        assert_eq!(
            engine.resolve_key(KeyChord::new(Modifiers::ALT, "3")),
            KeyResolution::Action {
                action: "client.tab.goto".to_owned(),
                args: Some(serde_json::json!({ "index": 3 })),
                source: Some("omarex:default".to_owned()),
            }
        );

        // Standard terminal keys (Ctrl+C, Ctrl+D, Shift+Enter, plain 'a') pass straight through!
        assert_eq!(
            engine.resolve_key(KeyChord::new(Modifiers::CTRL, "c")),
            KeyResolution::PassThrough
        );
        assert_eq!(
            engine.resolve_key(KeyChord::new(Modifiers::SHIFT, "enter")),
            KeyResolution::PassThrough
        );
        assert_eq!(
            engine.resolve_key(KeyChord::new(Modifiers::NONE, "a")),
            KeyResolution::PassThrough
        );
    }

    #[test]
    fn keymap_handles_sequences_which_key_and_exclusive_modes() {
        let mut engine = KeymapEngine::default();
        let resp = ClientKeymapResponse {
            bindings: vec![
                WireKeyBinding {
                    keys: vec!["ctrl+b>c".to_owned()],
                    action: "client.tab.new".to_owned(),
                    args: None,
                    mode: None,
                    source: Some("init.lua:10".to_owned()),
                },
                WireKeyBinding {
                    keys: vec!["ctrl+b>f".to_owned()],
                    action: "client.key.forward".to_owned(),
                    args: None,
                    mode: None,
                    source: Some("init.lua:11".to_owned()),
                },
                WireKeyBinding {
                    keys: vec!["ctrl+alt+r".to_owned()],
                    action: "client.mode.enter".to_owned(),
                    args: Some(serde_json::json!({ "mode": "resize" })),
                    mode: None,
                    source: Some("init.lua:12".to_owned()),
                },
                WireKeyBinding {
                    keys: vec!["resize/left".to_owned()],
                    action: "pane.resize".to_owned(),
                    args: Some(serde_json::json!({ "direction": "left" })),
                    mode: None,
                    source: Some("init.lua:13".to_owned()),
                },
            ],
            actions: vec![],
            modes: vec![KeyMode {
                name: "resize".to_owned(),
                exclusive: true,
                blocked: vec![],
            }],
            diagnostics: vec![],
            config: KeymapConfigMeta::default(),
        };
        engine.apply_server_keymap(&resp);

        // Sequence prefix returns which-key hints.
        let pending = engine.resolve_key(KeyChord::new(Modifiers::CTRL, "b"));
        match pending {
            KeyResolution::PendingSequence { prefix, hints } => {
                assert_eq!(prefix.len(), 1);
                assert_eq!(hints.len(), 2);
            }
            other => panic!("expected PendingSequence, got {other:?}"),
        }
        // Completing with `f` emits ForwardToServer with prefix!
        let fwd = engine.resolve_key(KeyChord::new(Modifiers::NONE, "f"));
        assert!(matches!(
            fwd,
            KeyResolution::ForwardToServer { prefix, trigger }
                if prefix.len() == 1 && trigger.key == "f"
        ));

        // Entering exclusive mode blocks unbound keys and exits on Esc.
        let entered = engine.resolve_key(KeyChord::new(Modifiers::CTRL_ALT, "r"));
        assert!(matches!(entered, KeyResolution::ModeEntered { mode, .. } if mode == "resize"));
        assert_eq!(engine.active_mode(), Some("resize"));
        assert_eq!(
            engine.resolve_key(KeyChord::new(Modifiers::NONE, "x")),
            KeyResolution::Blocked
        );
        assert!(matches!(
            engine.resolve_key(KeyChord::new(Modifiers::NONE, "left")),
            KeyResolution::Action { action, .. } if action == "pane.resize"
        ));
        assert_eq!(
            engine.resolve_key(KeyChord::new(Modifiers::NONE, "escape")),
            KeyResolution::ModeExited
        );
        assert_eq!(engine.active_mode(), None);
    }

    #[test]
    fn rex_ui_prompt_input_pick_multiple_allow_other_and_dismissal() {
        let mut mgr = PromptManager::new();
        let t0 = Instant::now();

        // 1. Input prompt preserves exact text & produces unordered client.reply.
        let req_in = PromptRequest {
            call_id: "p-1".to_owned(),
            action: Some("t3_rename".to_owned()),
            timeout_ms: 30_000,
            prompt: PromptSpec {
                kind: "input".to_owned(),
                title: "Rename Thread".to_owned(),
                placeholder: Some("Title".to_owned()),
                value: Some("Old".to_owned()),
                confirm: Some("Rename".to_owned()),
                multiple: false,
                allow_other: false,
                items: None,
            },
        };
        assert_eq!(
            mgr.on_prompt_requested(req_in, t0),
            PromptOpenOutcome::Opened
        );

        // Second concurrent prompt gets immediate `busy` reply.
        let req_busy = PromptRequest {
            call_id: "p-2".to_owned(),
            action: None,
            timeout_ms: 10_000,
            prompt: PromptSpec {
                kind: "input".to_owned(),
                title: "Busy".to_owned(),
                placeholder: None,
                value: None,
                confirm: None,
                multiple: false,
                allow_other: false,
                items: None,
            },
        };
        assert!(matches!(
            mgr.on_prompt_requested(req_busy, t0),
            PromptOpenOutcome::ImmediateReply(r) if r.error.as_ref().is_some_and(|e| e.code == "busy")
        ));

        mgr.active_mut()
            .expect("active")
            .set_input_text("  New Title  ");
        let reply = mgr.submit_enter().expect("input reply");
        assert!(reply.is_unordered());
        assert_eq!(reply.call_id, "p-1");
        assert_eq!(
            reply.result,
            Some(serde_json::json!({ "text": "  New Title  " }))
        );
        assert!(!mgr.is_open());

        // 2. Pick with allow_other (e.g. t3_snooze custom "45m").
        let req_pick = PromptRequest {
            call_id: "p-3".to_owned(),
            action: Some("t3_snooze".to_owned()),
            timeout_ms: 60_000,
            prompt: PromptSpec {
                kind: "pick".to_owned(),
                title: "Snooze Thread".to_owned(),
                placeholder: Some("Or type 45m".to_owned()),
                value: None,
                confirm: Some("Snooze".to_owned()),
                multiple: false,
                allow_other: true,
                items: Some(vec![
                    PromptItem {
                        id: "1h".to_owned(),
                        title: "1 hour".to_owned(),
                        subtitle: None,
                        icon: Some("clock".to_owned()),
                        detail: None,
                        selected: false,
                    },
                    PromptItem {
                        id: "tomorrow".to_owned(),
                        title: "Tomorrow".to_owned(),
                        subtitle: None,
                        icon: Some("clock".to_owned()),
                        detail: Some("9am".to_owned()),
                        selected: false,
                    },
                ]),
            },
        };
        assert_eq!(
            mgr.on_prompt_requested(req_pick, t0),
            PromptOpenOutcome::Opened
        );
        let active = mgr.active_mut().expect("pick active");
        assert_eq!(active.items[0].parsed_icon(), Some(PromptIcon::Clock));
        active.set_input_text("45m");
        assert_eq!(active.filtered_rows().len(), 0);
        assert!(active.has_other_row());
        let reply = mgr.submit_enter().expect("other text reply");
        assert_eq!(reply.result, Some(serde_json::json!({ "text": "45m" })));

        // 3. Multi-select pick with Space/Enter toggle and Ctrl+Enter confirm preserving order.
        let req_multi = PromptRequest {
            call_id: "p-4".to_owned(),
            action: Some("t3_answer".to_owned()),
            timeout_ms: 10_000,
            prompt: PromptSpec {
                kind: "pick".to_owned(),
                title: "Pick options".to_owned(),
                placeholder: None,
                value: None,
                confirm: Some("Answer".to_owned()),
                multiple: true,
                allow_other: false,
                items: Some(vec![
                    PromptItem {
                        id: "a".to_owned(),
                        title: "Alpha".to_owned(),
                        subtitle: None,
                        icon: None,
                        detail: None,
                        selected: true,
                    },
                    PromptItem {
                        id: "b".to_owned(),
                        title: "Beta".to_owned(),
                        subtitle: None,
                        icon: None,
                        detail: None,
                        selected: false,
                    },
                ]),
            },
        };
        assert_eq!(
            mgr.on_prompt_requested(req_multi, t0),
            PromptOpenOutcome::Opened
        );
        mgr.active_mut().expect("multi").move_focus(1);
        assert!(mgr.submit_enter().is_none()); // toggles Beta without closing
        let multi_reply = mgr.submit_confirm().expect("Ctrl+Enter confirms");
        assert_eq!(
            multi_reply.result,
            Some(serde_json::json!({ "ids": ["a", "b"] }))
        );

        // 4. Multi-client dismissal via client_prompt_ended closes without replying.
        let req_end = PromptRequest {
            call_id: "p-5".to_owned(),
            action: None,
            timeout_ms: 5_000,
            prompt: PromptSpec {
                kind: "input".to_owned(),
                title: "Race".to_owned(),
                placeholder: None,
                value: None,
                confirm: None,
                multiple: false,
                allow_other: false,
                items: None,
            },
        };
        assert_eq!(
            mgr.on_prompt_requested(req_end, t0),
            PromptOpenOutcome::Opened
        );
        assert!(mgr.on_prompt_ended(&PromptEndedEvent {
            call_id: "p-5".to_owned(),
            reason: "answered".to_owned(),
        }));
        assert!(!mgr.is_open());

        // 5. Unknown prompt kind immediately replies `not_supported`, and timeout auto-closes.
        let req_bad = PromptRequest {
            call_id: "p-6".to_owned(),
            action: None,
            timeout_ms: 1_000,
            prompt: PromptSpec {
                kind: "future_widget".to_owned(),
                title: "Future".to_owned(),
                placeholder: None,
                value: None,
                confirm: None,
                multiple: false,
                allow_other: false,
                items: None,
            },
        };
        assert!(matches!(
            mgr.on_prompt_requested(req_bad, t0),
            PromptOpenOutcome::ImmediateReply(r)
                if r.error.as_ref().is_some_and(|e| e.code == "not_supported")
        ));

        let req_to = PromptRequest {
            call_id: "p-7".to_owned(),
            action: None,
            timeout_ms: 500,
            prompt: PromptSpec {
                kind: "input".to_owned(),
                title: "Timeout".to_owned(),
                placeholder: None,
                value: None,
                confirm: None,
                multiple: false,
                allow_other: false,
                items: None,
            },
        };
        assert_eq!(
            mgr.on_prompt_requested(req_to, t0),
            PromptOpenOutcome::Opened
        );
        assert!(!mgr.check_timeout(t0 + Duration::from_millis(200)));
        assert!(mgr.check_timeout(t0 + Duration::from_millis(600)));
        assert!(!mgr.is_open());
    }

    #[test]
    fn perform_and_queued_client_actions_and_proxy_plan() {
        // Inline client.action queued response (tr3x rex.client.queue("session.select", ...)).
        let resp = ClientActionResponse {
            result: None,
            client_actions: vec![ClientActionItem {
                action: "session.select".to_owned(),
                args: Some(serde_json::json!({
                    "session_id": "session:1",
                    "window_id": "window:2",
                    "block_id": "block:3",
                })),
            }],
            ordered_after: Some("key-42".to_owned()),
        };
        let queued = dispatch_queued_actions(&resp);
        assert_eq!(
            queued,
            vec![ClientCommand::SelectSession {
                session_id: Some("session:1".to_owned()),
                window_id: Some("window:2".to_owned()),
                block_id: Some("block:3".to_owned()),
            }]
        );

        // Authorized peer perform request.
        let req = ClientPerformRequest {
            call_id: "perf-1".to_owned(),
            action: "pane.paste".to_owned(),
            args: None,
            origin: Some("cli".to_owned()),
            session_id: Some("session:1".to_owned()),
            block_id: Some("block:9".to_owned()),
            requester: ClientRequester {
                client_id: "client:abc".to_owned(),
                info: None,
                principal: ClientPrincipal {
                    kind: "peer".to_owned(),
                    transport: "unix".to_owned(),
                    uid: Some(1000),
                    ..Default::default()
                },
            },
        };
        let (cmd, reply) = handle_perform_request(&req, Some(1000));
        assert_eq!(
            cmd,
            Some(ClientCommand::Paste {
                session_id: Some("session:1".to_owned()),
                block_id: Some("block:9".to_owned()),
            })
        );
        assert!(reply.error.is_none());

        // Anonymous requester is refused regardless of requester.info.
        let mut bad_req = req.clone();
        bad_req.requester.principal.kind = "anonymous".to_owned();
        let (bad_cmd, bad_reply) = handle_perform_request(&bad_req, Some(1000));
        assert!(bad_cmd.is_none());
        assert_eq!(
            bad_reply.error.as_ref().map(|e| e.code.as_str()),
            Some("refused")
        );

        // Multi-server client_call_requested proxy plan.
        let call = ClientCallRequest {
            call_id: "call-1".to_owned(),
            server: "mac-studio".to_owned(),
            method: "session.list".to_owned(),
            args: None,
        };
        let plan = plan_remote_call(call.clone(), &["local", "mac-studio"]).expect("valid plan");
        assert_eq!(plan.timeout, CLIENT_CALL_PROXY_TIMEOUT);
        assert!(plan_remote_call(call, &["local"]).is_err());
    }

    #[test]
    fn t3_session_tags_providers_status_rollup_tab_card_and_pr_drawer() {
        assert!(is_t3_session("T3 Code"));
        assert!(is_t3_session("T3 Code · MacBook"));
        assert!(!is_t3_session("work"));
        assert_eq!(t3_session_machine("T3 Code · MacBook"), Some("MacBook"));
        assert_eq!(t3_session_machine("T3 Code"), None);

        assert_eq!(
            parse_t3_block_tag("t3:a1b2c3d4/thread-99"),
            Some(T3BlockTag::EnvThread {
                env_prefix: "a1b2c3d4".to_owned(),
                thread_id: "thread-99".to_owned(),
            })
        );
        assert_eq!(
            parse_t3_block_tag("t3#deadbeef"),
            Some(T3BlockTag::Hashed {
                hash: "deadbeef".to_owned(),
            })
        );
        assert_eq!(parse_t3_block_tag("zsh"), None);

        assert_eq!(
            ProviderKind::detect(Some("claude-code"), None),
            ProviderKind::Claude
        );
        assert_eq!(
            ProviderKind::detect(None, Some("codex")),
            ProviderKind::OpenAi
        );
        assert_eq!(
            ProviderKind::detect(Some("gemini"), None),
            ProviderKind::Gemini
        );
        assert_eq!(
            ProviderKind::detect(Some("hermes"), None),
            ProviderKind::Hermes
        );
        assert_eq!(ProviderKind::Claude.icon_name(), "claude");

        // Rollup priority: blocked beats working & error, sanitizing BiDi/control chars.
        let records = vec![
            ProgramStatusRecord {
                id: String::new(),
                state: ProgramState::Working,
                kind: None,
                progress: Some(40),
                app: Some("claude".to_owned()),
                effective_app: None,
                title: None,
                msg: Some("Working".to_owned()),
                unseen: false,
            },
            ProgramStatusRecord {
                id: "child".to_owned(),
                state: ProgramState::Blocked,
                kind: Some(BlockedKind::Permission),
                progress: None,
                app: Some("claude".to_owned()),
                effective_app: None,
                title: Some("Approve\u{202E}run".to_owned()),
                msg: Some("Needs\x07 you".to_owned()),
                unseen: true,
            },
        ];
        let rolled = rollup_records(&records).expect("rollup");
        assert_eq!(rolled.state, ProgramState::Blocked);
        assert_eq!(rolled.kind, Some(BlockedKind::Permission));
        assert_eq!(rolled.msg.as_deref(), Some("Needs you"));

        let card = build_tab_card(
            "↳ Subagent task",
            Some("omarex"),
            None,
            Some(&rolled),
            Some(720),
        );
        assert!(card.is_subagent);
        assert_eq!(card.provider, ProviderKind::Claude);
        assert_eq!(card.priority_dot, PriorityDot::Blocked);
        assert_eq!(card.subtitle, "omarex · Needs you");
        assert_eq!(card.relative_age.as_deref(), Some("12m"));

        // /pr floating drawer geometry.
        assert_eq!(pr_drawer_rect(160).to_array(), [0.45, 0.0, 0.55, 1.0]);
        assert_eq!(pr_drawer_rect(120).to_array(), [0.03, 0.03, 0.94, 0.94]);
        assert!(is_pr_layer("Pull Request"));
    }

    #[test]
    fn action_registry_merges_server_tr3x_actions_and_fuzzy_searches() {
        let mut reg = ActionRegistry::default();
        let req = reg.build_client_keymap_request();
        assert!(req["actions"].as_array().is_some_and(|a| !a.is_empty()));
        assert_eq!(req["defaults"][0]["name"].as_str(), Some("default"));
        assert!(
            req["defaults"][0]["bindings"]
                .as_array()
                .is_some_and(|b| !b.is_empty())
        );

        let server_resp = ClientKeymapResponse {
            bindings: vec![WireKeyBinding {
                keys: vec!["ctrl+cmd+a".to_owned()],
                action: "t3_answer".to_owned(),
                args: None,
                mode: None,
                source: Some("tr3x.lua:433".to_owned()),
            }],
            actions: vec![WireActionDescriptor {
                name: "t3_answer".to_owned(),
                kind: "lua".to_owned(),
                title: "Answer…".to_owned(),
                category: "T3 Code".to_owned(),
                keywords: "t3 approve decline permission".to_owned(),
            }],
            modes: vec![],
            diagnostics: vec![],
            config: KeymapConfigMeta {
                hash: "h1".to_owned(),
                generation: 1,
                ..Default::default()
            },
        };
        reg.apply_server_keymap(&server_resp);

        let entry = reg.get("t3_answer").expect("t3_answer registered");
        assert!(entry.is_tr3x);
        assert_eq!(entry.shortcut_hint.as_deref(), Some("Ctrl+Alt+A"));

        let hits = reg.search("approve");
        assert!(hits.iter().any(|m| m.entry.name == "t3_answer"));
        assert_eq!(TR3X_ACTION_NAMES.len(), 35);
    }
}
