//! Rex `client.keymap`, named key-mode, sequence/which-key, and Omarchy Hyprland
//! conflict-free keybinding resolver.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::BridgeError;

/// Keyboard modifier mask with Linux `cmd` <-> `super` equivalence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        shift: false,
        alt: false,
        super_key: false,
    };

    pub const CTRL: Self = Self {
        ctrl: true,
        shift: false,
        alt: false,
        super_key: false,
    };

    pub const SHIFT: Self = Self {
        ctrl: false,
        shift: true,
        alt: false,
        super_key: false,
    };

    pub const ALT: Self = Self {
        ctrl: false,
        shift: false,
        alt: true,
        super_key: false,
    };

    pub const CTRL_SHIFT: Self = Self {
        ctrl: true,
        shift: true,
        alt: false,
        super_key: false,
    };

    pub const CTRL_ALT: Self = Self {
        ctrl: true,
        shift: false,
        alt: true,
        super_key: false,
    };

    pub const CTRL_ALT_SHIFT: Self = Self {
        ctrl: true,
        shift: true,
        alt: true,
        super_key: false,
    };

    pub const CTRL_SUPER: Self = Self {
        ctrl: true,
        shift: false,
        alt: false,
        super_key: true,
    };

    /// Returns `true` when this chord uses `ctrl+cmd` / `ctrl+super` (without `alt`),
    /// which Omarchy Hyprland steals globally and `omarex` auto-aliases to `Ctrl+Alt`.
    pub const fn is_ctrl_cmd_without_alt(self) -> bool {
        self.ctrl && self.super_key && !self.alt
    }

    /// Converts `ctrl+super(+shift)` into conflict-free `ctrl+alt(+shift)`.
    pub const fn alias_ctrl_cmd_to_ctrl_alt(self) -> Self {
        Self {
            ctrl: self.ctrl,
            shift: self.shift,
            alt: true,
            super_key: false,
        }
    }

    /// Converts to `rex_proto::types::KeyModifiers` for wire `client.key` / `block.event` payloads.
    pub const fn to_proto_modifiers(self) -> rex_proto::types::KeyModifiers {
        rex_proto::types::KeyModifiers {
            shift: self.shift,
            ctrl: self.ctrl,
            alt: self.alt,
            super_key: self.super_key,
            caps_lock: false,
            num_lock: false,
            shift_side: false,
            ctrl_side: false,
            alt_side: false,
            super_side: false,
        }
    }

    /// Formats modifier names as a `+`-joined canonical Rex rev 24 wire string
    /// (`shift+ctrl+alt+super`, or `""` when empty) for `client.key` / `block.event`.
    pub fn to_rex_wire_mods(self) -> String {
        self.to_proto_modifiers().to_rex_string()
    }
}

/// Canonical single keypress chord (e.g., `Ctrl+Alt+I`, `Ctrl+Shift+P`, `Alt+1`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyChord {
    pub mods: Modifiers,
    pub key: String,
}

impl KeyChord {
    pub fn new(mods: Modifiers, key: impl AsRef<str>) -> Self {
        Self {
            mods,
            key: canonicalize_key_name(key.as_ref()),
        }
    }

    /// Parses a single chord such as `"ctrl+cmd+i"`, `"Ctrl+Shift+P"`, `"shift+insert"`, `"alt+1"`.
    pub fn parse(spec: &str) -> Result<Self, BridgeError> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err(BridgeError::InvalidKeySpec("empty key chord".to_owned()));
        }

        // Handle literal "+" key at the end (e.g. "ctrl++" or "+").
        let mut mods = Modifiers::NONE;
        let parts: Vec<&str> = if trimmed == "+" {
            vec!["+"]
        } else if let Some(prefix) = trimmed.strip_suffix("++") {
            let mut p: Vec<&str> = prefix.split('+').filter(|s| !s.is_empty()).collect();
            p.push("+");
            p
        } else {
            trimmed.split('+').collect()
        };

        if parts.is_empty() {
            return Err(BridgeError::InvalidKeySpec(spec.to_owned()));
        }

        let (mod_parts, key_part) = parts.split_at(parts.len() - 1);
        for m in mod_parts {
            match m.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods.ctrl = true,
                "shift" => mods.shift = true,
                "alt" | "opt" | "option" => mods.alt = true,
                "cmd" | "super" | "meta" | "win" | "command" => mods.super_key = true,
                "" => continue,
                other => {
                    return Err(BridgeError::InvalidKeySpec(format!(
                        "unknown modifier `{other}` in `{spec}`"
                    )));
                }
            }
        }

        let key = canonicalize_key_name(key_part[0]);
        if key.is_empty() {
            return Err(BridgeError::InvalidKeySpec(format!(
                "missing key in `{spec}`"
            )));
        }

        Ok(Self { mods, key })
    }

    /// Parses a Rex key binding string supporting optional `mode/` prefix and `>` sequences:
    /// e.g. `"ctrl+cmd+i"`, `"ctrl+b>c"`, `"resize/left"`.
    pub fn parse_spec(spec: &str) -> Result<(Option<String>, Vec<Self>), BridgeError> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Err(BridgeError::InvalidKeySpec("empty key spec".to_owned()));
        }

        // Distinguish `mode/key` from a chord containing literal `/` (like `ctrl+alt+/`).
        let (mode, rest) = if let Some((lhs, rhs)) = trimmed.split_once('/')
            && !lhs.is_empty()
            && !rhs.is_empty()
            && !lhs.contains('+')
            && !lhs.contains('>')
            && lhs
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            (Some(lhs.to_ascii_lowercase()), rhs)
        } else {
            (None, trimmed)
        };

        let mut seq = Vec::new();
        for seg in rest.split('>') {
            let s = seg.trim();
            if s.is_empty() {
                return Err(BridgeError::InvalidKeySpec(format!(
                    "empty segment in key sequence `{spec}`"
                )));
            }
            seq.push(Self::parse(s)?);
        }
        Ok((mode, seq))
    }

    /// Formats chord in Rex canonical lowercase notation (e.g. `ctrl+alt+i`).
    pub fn to_rex_string(&self) -> String {
        let mut parts = Vec::with_capacity(5);
        if self.mods.ctrl {
            parts.push("ctrl");
        }
        if self.mods.alt {
            parts.push("alt");
        }
        if self.mods.shift {
            parts.push("shift");
        }
        if self.mods.super_key {
            parts.push("super");
        }
        parts.push(self.key.as_str());
        parts.join("+")
    }

    /// Formats chord for Linux/Omarchy UI display (e.g. `Ctrl+Shift+P`, `Alt+1`, `Shift+Insert`).
    pub fn to_display_string(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(5);
        if self.mods.ctrl {
            parts.push("Ctrl".to_owned());
        }
        if self.mods.alt {
            parts.push("Alt".to_owned());
        }
        if self.mods.shift {
            parts.push("Shift".to_owned());
        }
        if self.mods.super_key {
            parts.push("Super".to_owned());
        }
        parts.push(display_key_label(&self.key));
        parts.join("+")
    }
}

/// Normalizes physical/logical key names across Rex (`key_i`, `bracket_left`),
/// Lua (`ctrl+cmd+[`, `return`), and Linux/winit conventions.
pub fn canonicalize_key_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_ascii_lowercase();
    let stripped = lower
        .strip_prefix("key_")
        .or_else(|| lower.strip_prefix("digit_"))
        .unwrap_or(&lower);

    match stripped {
        "return" | "enter" => "enter".to_owned(),
        "esc" | "escape" => "escape".to_owned(),
        "tab" => "tab".to_owned(),
        "backtab" => "tab".to_owned(),
        "backspace" | "bs" => "backspace".to_owned(),
        "space" | " " => "space".to_owned(),
        "insert" | "ins" => "insert".to_owned(),
        "delete" | "del" => "delete".to_owned(),
        "home" => "home".to_owned(),
        "end" => "end".to_owned(),
        "pageup" | "page_up" | "pgup" => "page_up".to_owned(),
        "pagedown" | "page_down" | "pgdn" => "page_down".to_owned(),
        "left" | "arrow_left" | "arrowleft" => "left".to_owned(),
        "right" | "arrow_right" | "arrowright" => "right".to_owned(),
        "up" | "arrow_up" | "arrowup" => "up".to_owned(),
        "down" | "arrow_down" | "arrowdown" => "down".to_owned(),
        "bracket_left" | "bracketleft" | "[" => "[".to_owned(),
        "bracket_right" | "bracketright" | "]" => "]".to_owned(),
        "equal" | "equals" | "plus" | "=" | "+" => "=".to_owned(),
        "minus" | "hyphen" | "-" => "-".to_owned(),
        "comma" | "," => ",".to_owned(),
        "period" | "dot" | "." => ".".to_owned(),
        "slash" | "/" => "/".to_owned(),
        "backslash" | "\\" => "\\".to_owned(),
        "semicolon" | ";" => ";".to_owned(),
        "quote" | "'" => "'".to_owned(),
        "backquote" | "grave" | "`" => "`".to_owned(),
        other => other.to_owned(),
    }
}

fn display_key_label(key: &str) -> String {
    match key {
        "enter" => "Enter".to_owned(),
        "escape" => "Esc".to_owned(),
        "tab" => "Tab".to_owned(),
        "backspace" => "Backspace".to_owned(),
        "space" => "Space".to_owned(),
        "insert" => "Insert".to_owned(),
        "delete" => "Delete".to_owned(),
        "home" => "Home".to_owned(),
        "end" => "End".to_owned(),
        "page_up" => "PageUp".to_owned(),
        "page_down" => "PageDown".to_owned(),
        "left" => "Left".to_owned(),
        "right" => "Right".to_owned(),
        "up" => "Up".to_owned(),
        "down" => "Down".to_owned(),
        other if other.len() == 1 => other.to_ascii_uppercase(),
        other if other.starts_with('f') && other[1..].chars().all(|c| c.is_ascii_digit()) => {
            other.to_ascii_uppercase()
        }
        other => other.to_owned(),
    }
}

/// Named Rex key mode returned by `client.keymap` (API rev 24).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMode {
    pub name: String,
    #[serde(default)]
    pub exclusive: bool,
    /// Specific key specs explicitly blocked in this mode (Go sends `null` or `[]` when empty).
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub blocked: Vec<String>,
}

impl KeyMode {
    /// Returns `true` when `candidate` matches a key/sequence listed in `self.blocked`.
    pub fn blocks_sequence(&self, candidate: &[KeyChord]) -> bool {
        if self.blocked.is_empty() || candidate.is_empty() {
            return false;
        }
        self.blocked.iter().any(|spec| {
            KeyChord::parse_spec(spec)
                .map(|(_, seq)| seq.as_slice() == candidate)
                .unwrap_or(false)
        })
    }
}

/// Wire entry inside `client.keymap` response `bindings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireKeyBinding {
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub keys: Vec<String>,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Wire entry inside `client.keymap` response `actions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireActionDescriptor {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub keywords: String,
}

/// Server config metadata returned by `client.keymap`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeymapConfigMeta {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub exists: bool,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub hash: String,
}

/// Full `client.keymap` RPC response payload (Rex API rev 24).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ClientKeymapResponse {
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub bindings: Vec<WireKeyBinding>,
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub actions: Vec<WireActionDescriptor>,
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub modes: Vec<KeyMode>,
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub diagnostics: Vec<serde_json::Value>,
    #[serde(default)]
    pub config: KeymapConfigMeta,
}

/// Normalized internal binding stored inside [`KeymapEngine`].
#[derive(Debug, Clone, PartialEq)]
pub struct KeyBinding {
    pub keys: Vec<KeyChord>,
    pub mode: Option<String>,
    pub action: String,
    pub args: Option<serde_json::Value>,
    pub source: Option<String>,
    pub aliased_from_ctrl_cmd: bool,
}

/// Hint row for the which-key sequence/mode overlay (`docs/gui-spec.md §3.4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhichKeyHint {
    pub next_key: KeyChord,
    pub display_key: String,
    pub action: Option<String>,
    pub is_prefix: bool,
}

/// Result of resolving a keypress against [`KeymapEngine`].
#[derive(Debug, Clone, PartialEq)]
pub enum KeyResolution {
    /// Matched a concrete action (local client action or server Lua/block action).
    Action {
        action: String,
        args: Option<serde_json::Value>,
        source: Option<String>,
    },
    /// Special Rex action `client.key.forward`: forward prefix + trigger to `client.key`.
    ForwardToServer {
        prefix: Vec<KeyChord>,
        trigger: KeyChord,
    },
    /// Matched the prefix of one or more multi-key sequences; show which-key hints.
    PendingSequence {
        prefix: Vec<KeyChord>,
        hints: Vec<WhichKeyHint>,
    },
    /// Entered a named key mode; display mode pill and hints.
    ModeEntered {
        mode: String,
        hints: Vec<WhichKeyHint>,
    },
    /// Exited current key mode (or cancelled a pending sequence).
    ModeExited,
    /// Unbound key — pass directly through to the terminal PTY / `client.key`.
    PassThrough,
    /// Active key mode is `exclusive`/`blocked` and swallowed this unbound key.
    Blocked,
}

/// Behavioral options for [`KeymapEngine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeymapOptions {
    /// Automatically alias `ctrl+cmd+<key>` bindings from `tr3x.lua`/`init.lua` to
    /// Hyprland-conflict-free `Ctrl+Alt+<key>` unless explicitly overridden (`gui-spec.md §5.2 E`).
    pub remap_ctrl_cmd_to_ctrl_alt: bool,
}

impl Default for KeymapOptions {
    fn default() -> Self {
        Self {
            remap_ctrl_cmd_to_ctrl_alt: true,
        }
    }
}

/// Stateful keybinding, sequence, and key-mode engine.
#[derive(Debug, Clone)]
pub struct KeymapEngine {
    options: KeymapOptions,
    defaults: Vec<KeyBinding>,
    server_bindings: Vec<KeyBinding>,
    user_overrides: Vec<KeyBinding>,
    effective: Vec<KeyBinding>,
    modes: HashMap<String, KeyMode>,
    mode_stack: Vec<String>,
    pending_sequence: Vec<KeyChord>,
    last_config_hash: Option<String>,
    last_generation: u64,
}

impl Default for KeymapEngine {
    fn default() -> Self {
        Self::new(KeymapOptions::default())
    }
}

impl KeymapEngine {
    /// Creates an engine preloaded with Omarchy conflict-free default bindings.
    pub fn new(options: KeymapOptions) -> Self {
        let defaults = omarex_default_bindings();
        let mut engine = Self {
            options,
            defaults,
            server_bindings: Vec::new(),
            user_overrides: Vec::new(),
            effective: Vec::new(),
            modes: HashMap::new(),
            mode_stack: Vec::new(),
            pending_sequence: Vec::new(),
            last_config_hash: None,
            last_generation: 0,
        };
        engine.rebuild_effective();
        engine
    }

    pub fn options(&self) -> KeymapOptions {
        self.options
    }

    pub fn set_options(&mut self, options: KeymapOptions) {
        self.options = options;
        self.rebuild_effective();
    }

    /// Returns `true` when an incoming `keymap_changed` event carries a new config hash.
    pub fn needs_refresh(&self, hash: &str) -> bool {
        self.last_config_hash.as_deref() != Some(hash)
    }

    pub fn last_generation(&self) -> u64 {
        self.last_generation
    }

    /// Active key mode name (top of mode stack), if any.
    pub fn active_mode(&self) -> Option<&str> {
        self.mode_stack.last().map(String::as_str)
    }

    /// Pending multi-key sequence chords, if any.
    pub fn pending_sequence(&self) -> &[KeyChord] {
        &self.pending_sequence
    }

    /// Replaces user-level overrides (highest priority per Rex spec) and rebuilds table.
    pub fn set_user_overrides(&mut self, overrides: Vec<KeyBinding>) {
        self.user_overrides = overrides;
        self.rebuild_effective();
    }

    /// Exports default bindings in Rex wire format for `client.keymap` registration.
    pub fn wire_default_bindings(&self) -> Vec<WireKeyBinding> {
        self.defaults
            .iter()
            .map(|b| WireKeyBinding {
                keys: b.keys.iter().map(KeyChord::to_rex_string).collect(),
                action: b.action.clone(),
                args: b.args.clone(),
                mode: b.mode.clone(),
                source: b.source.clone(),
            })
            .collect()
    }

    /// Merges a live `client.keymap` response from the Rex server.
    pub fn apply_server_keymap(&mut self, response: &ClientKeymapResponse) {
        self.modes.clear();
        for mode in &response.modes {
            self.modes
                .insert(mode.name.to_ascii_lowercase(), mode.clone());
        }

        let mut parsed = Vec::with_capacity(response.bindings.len());
        for wire in &response.bindings {
            match parse_wire_binding(wire) {
                Ok(binding) => parsed.push(binding),
                Err(err) => {
                    tracing::warn!(
                        action = %wire.action,
                        error = %err,
                        "skipping unparseable server keybinding"
                    );
                }
            }
        }

        self.server_bindings = parsed;
        if !response.config.hash.is_empty() {
            self.last_config_hash = Some(response.config.hash.clone());
        }
        self.last_generation = response.config.generation;
        self.rebuild_effective();
    }

    /// Looks up the best human-readable shortcut label for an action (for Command Palette).
    pub fn display_shortcut_for_action(&self, action: &str) -> Option<String> {
        // Prefer conflict-free Ctrl+Alt alias over Ctrl+Super when both exist on Omarchy.
        let mut fallback: Option<String> = None;
        for b in self.effective.iter().rev() {
            if b.action != action || b.mode.is_some() {
                continue;
            }
            let rendered = b
                .keys
                .iter()
                .map(KeyChord::to_display_string)
                .collect::<Vec<_>>()
                .join(" › ");
            if !b.keys.iter().any(|k| k.mods.super_key) {
                return Some(rendered);
            }
            if fallback.is_none() {
                fallback = Some(rendered);
            }
        }
        fallback
    }

    /// Returns all currently active effective bindings.
    pub fn effective_bindings(&self) -> &[KeyBinding] {
        &self.effective
    }

    /// Enters a named key mode and returns its which-key hints.
    pub fn enter_mode(&mut self, mode: &str) -> Vec<WhichKeyHint> {
        let lower = mode.to_ascii_lowercase();
        self.pending_sequence.clear();
        self.mode_stack.push(lower.clone());
        self.hints_for(Some(&lower), &[])
    }

    /// Exits the current key mode.
    pub fn exit_mode(&mut self) -> bool {
        self.pending_sequence.clear();
        self.mode_stack.pop().is_some()
    }

    /// Exits all stacked key modes and cancels any pending sequence.
    pub fn exit_all_modes(&mut self) {
        self.pending_sequence.clear();
        self.mode_stack.clear();
    }

    /// Resolves a keypress chord through active key mode, sequence buffer, and effective map.
    pub fn resolve_key(&mut self, chord: KeyChord) -> KeyResolution {
        // Escape cancels a pending multi-key sequence first, or exits an active mode.
        if chord.mods == Modifiers::NONE && chord.key == "escape" {
            if !self.pending_sequence.is_empty() {
                self.pending_sequence.clear();
                return KeyResolution::ModeExited;
            }
            if self.mode_stack.pop().is_some() {
                return KeyResolution::ModeExited;
            }
        }

        let mut candidate = self.pending_sequence.clone();
        candidate.push(chord.clone());
        let active_mode = self.mode_stack.last().cloned();

        if let Some(res) = self.match_in_scope(active_mode.as_deref(), &candidate, &chord) {
            return res;
        }

        // If a non-exclusive mode didn't match, check global bindings unless the mode is
        // exclusive or explicitly blocks this key sequence.
        if let Some(mode_name) = active_mode.as_deref() {
            let mode_meta = self.modes.get(mode_name);
            let exclusive = mode_meta.map(|m| m.exclusive).unwrap_or(false);
            let blocked_key = mode_meta
                .map(|m| m.blocks_sequence(&candidate))
                .unwrap_or(false);
            if !exclusive && !blocked_key {
                if let Some(res) = self.match_in_scope(None, &candidate, &chord) {
                    return res;
                }
            } else {
                self.pending_sequence.clear();
                return KeyResolution::Blocked;
            }
        }

        self.pending_sequence.clear();
        KeyResolution::PassThrough
    }

    fn match_in_scope(
        &mut self,
        mode: Option<&str>,
        candidate: &[KeyChord],
        trigger: &KeyChord,
    ) -> Option<KeyResolution> {
        let mut exact: Option<&KeyBinding> = None;
        let mut has_longer_prefix = false;

        for binding in &self.effective {
            if binding.mode.as_deref() != mode {
                continue;
            }
            if binding.keys.len() == candidate.len() && binding.keys.as_slice() == candidate {
                exact = Some(binding);
            } else if binding.keys.len() > candidate.len()
                && binding.keys[..candidate.len()] == *candidate
            {
                has_longer_prefix = true;
            }
        }

        if let Some(hit) = exact {
            let action = hit.action.clone();
            let args = hit.args.clone();
            let source = hit.source.clone();
            let prefix = self.pending_sequence.clone();
            self.pending_sequence.clear();

            if action == "client.key.forward" {
                return Some(KeyResolution::ForwardToServer {
                    prefix,
                    trigger: trigger.clone(),
                });
            }
            if action == "client.mode.enter"
                && let Some(target) = args
                    .as_ref()
                    .and_then(|v| v.get("mode").or_else(|| v.get("name")))
                    .and_then(|v| v.as_str())
            {
                let hints = self.enter_mode(target);
                return Some(KeyResolution::ModeEntered {
                    mode: target.to_ascii_lowercase(),
                    hints,
                });
            }
            if action == "client.mode.exit" {
                self.exit_mode();
                return Some(KeyResolution::ModeExited);
            }
            if action == "client.mode.exit_all" {
                self.exit_all_modes();
                return Some(KeyResolution::ModeExited);
            }

            return Some(KeyResolution::Action {
                action,
                args,
                source,
            });
        }

        if has_longer_prefix {
            self.pending_sequence = candidate.to_vec();
            let hints = self.hints_for(mode, candidate);
            return Some(KeyResolution::PendingSequence {
                prefix: candidate.to_vec(),
                hints,
            });
        }

        None
    }

    /// Lists next-key which-key hints for a mode and prefix.
    pub fn hints_for(&self, mode: Option<&str>, prefix: &[KeyChord]) -> Vec<WhichKeyHint> {
        let mut seen = HashSet::new();
        let mut hints = Vec::new();
        for b in &self.effective {
            if b.mode.as_deref() != mode || b.keys.len() <= prefix.len() {
                continue;
            }
            if &b.keys[..prefix.len()] != prefix {
                continue;
            }
            let next = b.keys[prefix.len()].clone();
            if !seen.insert(next.clone()) {
                continue;
            }
            let is_prefix = b.keys.len() > prefix.len() + 1;
            hints.push(WhichKeyHint {
                display_key: next.to_display_string(),
                next_key: next,
                action: (!is_prefix).then(|| b.action.clone()),
                is_prefix,
            });
        }
        hints
    }

    fn rebuild_effective(&mut self) {
        // Priority order (lowest -> highest):
        // 1. Built-in omarex Linux/Omarchy defaults
        // 2. Server init.lua / tr3x.lua bindings
        // 3. Auto-aliased Ctrl+Alt equivalents of server `ctrl+cmd+*` bindings
        //    (only if not explicitly claimed by server init.lua or user overrides)
        // 4. User local overrides
        type BindingKey = (Option<String>, Vec<KeyChord>);

        let mut merged: Vec<KeyBinding> = Vec::new();
        let mut index_by_key: HashMap<BindingKey, usize> = HashMap::new();

        let mut insert_or_replace = |b: KeyBinding| {
            let k = (b.mode.clone(), b.keys.clone());
            if let Some(&idx) = index_by_key.get(&k) {
                merged[idx] = b;
            } else {
                let idx = merged.len();
                index_by_key.insert(k, idx);
                merged.push(b);
            }
        };

        for b in &self.defaults {
            insert_or_replace(b.clone());
        }

        let mut explicit_server_keys: HashSet<BindingKey> = HashSet::new();
        for b in &self.server_bindings {
            explicit_server_keys.insert((b.mode.clone(), b.keys.clone()));
            insert_or_replace(b.clone());
        }

        if self.options.remap_ctrl_cmd_to_ctrl_alt {
            for b in &self.server_bindings {
                if b.keys.len() == 1 && b.keys[0].mods.is_ctrl_cmd_without_alt() {
                    let aliased_chord = KeyChord {
                        mods: b.keys[0].mods.alias_ctrl_cmd_to_ctrl_alt(),
                        key: b.keys[0].key.clone(),
                    };
                    let alias_key = (b.mode.clone(), vec![aliased_chord.clone()]);
                    if !explicit_server_keys.contains(&alias_key) {
                        insert_or_replace(KeyBinding {
                            keys: vec![aliased_chord],
                            mode: b.mode.clone(),
                            action: b.action.clone(),
                            args: b.args.clone(),
                            source: b.source.clone(),
                            aliased_from_ctrl_cmd: true,
                        });
                    }
                }
            }
        }

        for b in &self.user_overrides {
            insert_or_replace(b.clone());
        }

        self.effective = merged;
    }
}

fn parse_wire_binding(wire: &WireKeyBinding) -> Result<KeyBinding, BridgeError> {
    let mut mode = wire.mode.as_ref().map(|m| m.to_ascii_lowercase());
    let mut chords = Vec::with_capacity(wire.keys.len());

    for raw in &wire.keys {
        let (parsed_mode, parsed_seq) = KeyChord::parse_spec(raw)?;
        if mode.is_none() {
            mode = parsed_mode;
        }
        chords.extend(parsed_seq);
    }

    if chords.is_empty() {
        return Err(BridgeError::InvalidKeySpec(format!(
            "binding for `{}` had no keys",
            wire.action
        )));
    }

    Ok(KeyBinding {
        keys: chords,
        mode,
        action: wire.action.clone(),
        args: wire.args.clone(),
        source: wire.source.clone(),
        aliased_from_ctrl_cmd: false,
    })
}

/// Complete conflict-free Omarchy/Hyprland default keymap (`docs/gui-spec.md §5.2`).
pub fn omarex_default_bindings() -> Vec<KeyBinding> {
    let mut out = Vec::with_capacity(64);
    let mut add = |mods: Modifiers, key: &str, action: &str, args: Option<serde_json::Value>| {
        out.push(KeyBinding {
            keys: vec![KeyChord::new(mods, key)],
            mode: None,
            action: action.to_owned(),
            args,
            source: Some("omarex:default".to_owned()),
            aliased_from_ctrl_cmd: false,
        });
    };

    // A. Workspace, Palette, Navigation & View
    add(Modifiers::CTRL_SHIFT, "p", "client.palette.toggle", None);
    add(Modifiers::CTRL_ALT, "p", "pane.go_to", None);
    add(Modifiers::CTRL_SHIFT, "k", "session.switch", None);
    add(Modifiers::CTRL_SHIFT, "n", "session.new", None);
    add(Modifiers::CTRL_ALT_SHIFT, "n", "session.new.on_host", None);
    add(Modifiers::CTRL_ALT_SHIFT, "w", "client.window.new", None);
    add(Modifiers::CTRL_SHIFT, ",", "client.settings", None);
    add(Modifiers::CTRL_ALT, ",", "client.diagnostics", None);
    add(Modifiers::CTRL_SHIFT, "b", "client.sidebar.toggle", None);
    add(Modifiers::CTRL_ALT, "/", "client.sidebar.toggle", None);
    add(Modifiers::CTRL_SHIFT, "s", "client.tab_style.toggle", None);
    add(
        Modifiers::CTRL_SHIFT,
        ".",
        "client.sidebar.activity.toggle",
        None,
    );
    add(
        Modifiers::CTRL_ALT,
        "f",
        "client.sidebar.filter.focus",
        None,
    );
    add(
        Modifiers::CTRL_ALT_SHIFT,
        "t",
        "client.toolbar.reveal",
        None,
    );
    add(Modifiers::CTRL_ALT, "h", "client.pane_header.reveal", None);
    add(Modifiers::CTRL_ALT, ".", "client.notice.dismiss", None);
    add(Modifiers::CTRL_SHIFT, "r", "session.refresh", None);
    add(Modifiers::CTRL_ALT_SHIFT, "[", "client.navigate.back", None);
    add(
        Modifiers::CTRL_ALT_SHIFT,
        "]",
        "client.navigate.forward",
        None,
    );

    // B. Tabs (Windows)
    add(Modifiers::CTRL_SHIFT, "t", "client.tab.new", None);
    add(Modifiers::CTRL_SHIFT, "w", "client.close", None);
    add(Modifiers::CTRL_ALT, "w", "client.tab.close", None);
    add(Modifiers::CTRL_ALT_SHIFT, "z", "session.undo_close", None);
    add(Modifiers::CTRL, "tab", "client.tab.next", None);
    add(Modifiers::CTRL_SHIFT, "tab", "client.tab.previous", None);
    add(Modifiers::CTRL, "page_down", "client.tab.next", None);
    add(Modifiers::CTRL, "page_up", "client.tab.previous", None);
    for idx in 1..=9 {
        add(
            Modifiers::ALT,
            &idx.to_string(),
            "client.tab.goto",
            Some(serde_json::json!({ "index": idx })),
        );
    }
    add(
        Modifiers::CTRL_SHIFT,
        "page_up",
        "client.tab.move.backward",
        None,
    );
    add(
        Modifiers::CTRL_SHIFT,
        "page_down",
        "client.tab.move.forward",
        None,
    );
    add(Modifiers::CTRL_ALT, "u", "client.tab.unread", None);

    // C. Panes, Splits, Floating & Zoom
    add(Modifiers::CTRL_SHIFT, "d", "pane.split.right", None);
    add(Modifiers::CTRL_SHIFT, "right", "pane.split.right", None);
    add(Modifiers::CTRL_SHIFT, "e", "pane.split.down", None);
    add(Modifiers::CTRL_SHIFT, "down", "pane.split.down", None);
    add(Modifiers::CTRL_SHIFT, "left", "pane.split.left", None);
    add(Modifiers::CTRL_SHIFT, "up", "pane.split.up", None);
    add(Modifiers::CTRL_SHIFT, "enter", "pane.split.auto", None);
    add(Modifiers::CTRL_ALT, "left", "pane.focus.left", None);
    add(Modifiers::CTRL_ALT, "right", "pane.focus.right", None);
    add(Modifiers::CTRL_ALT, "up", "pane.focus.up", None);
    add(Modifiers::CTRL_ALT, "down", "pane.focus.down", None);
    add(Modifiers::CTRL_SHIFT, "]", "pane.focus_next", None);
    add(Modifiers::CTRL_SHIFT, "[", "pane.focus_previous", None);
    add(Modifiers::CTRL_ALT_SHIFT, "left", "pane.move.left", None);
    add(Modifiers::CTRL_ALT_SHIFT, "right", "pane.move.right", None);
    add(Modifiers::CTRL_ALT_SHIFT, "up", "pane.move.up", None);
    add(Modifiers::CTRL_ALT_SHIFT, "down", "pane.move.down", None);
    add(Modifiers::CTRL_SHIFT, "z", "pane.zoom", None);
    add(Modifiers::CTRL_ALT, "z", "pane.zoom", None);
    add(Modifiers::CTRL_ALT, "enter", "pane.float", None);
    add(Modifiers::CTRL_ALT, "m", "pane.minimize_floating", None);
    add(
        Modifiers::CTRL_ALT_SHIFT,
        "m",
        "pane.minimize_all_floating",
        None,
    );
    add(Modifiers::CTRL_ALT, "g", "pane.go_to_directory", None);
    add(Modifiers::CTRL_ALT_SHIFT, "i", "pane.insert_path", None);

    // D. Clipboard (including Omarchy Super+C -> Ctrl+Insert, Super+V -> Shift+Insert),
    // Find, Copy Mode & Typography
    add(Modifiers::CTRL_SHIFT, "c", "pane.copy", None);
    add(Modifiers::CTRL, "insert", "pane.copy", None);
    add(Modifiers::CTRL_SHIFT, "v", "pane.paste", None);
    add(Modifiers::SHIFT, "insert", "pane.paste", None);
    add(Modifiers::CTRL_SHIFT, "x", "pane.copy_mode.enter", None);
    add(Modifiers::CTRL_SHIFT, "f", "client.find.open", None);
    add(Modifiers::NONE, "f3", "client.find.next", None);
    add(Modifiers::SHIFT, "f3", "client.find.previous", None);
    add(
        Modifiers::CTRL_SHIFT,
        "home",
        "client.prompt.previous",
        None,
    );
    add(Modifiers::CTRL_SHIFT, "end", "client.prompt.next", None);
    add(Modifiers::CTRL, "=", "client.font.increase", None);
    add(Modifiers::CTRL_SHIFT, "=", "client.font.increase", None);
    add(Modifiers::CTRL, "-", "client.font.decrease", None);
    add(Modifiers::CTRL, "0", "client.font.reset", None);
    add(Modifiers::CTRL_SHIFT, "y", "client.quick_look", None);

    out
}
