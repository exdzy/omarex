//! Direct wire-protocol bridge between `rex_proto::types::*` (Rex API rev 24)
//! and `tr3x_bridge`'s stateful keymap, `rex.ui` prompt, perform/proxy, and
//! `T3 Code` sidebar tab-card models.

use std::collections::HashMap;
use std::time::Instant;

use rex_proto::types as proto;

use crate::ActionRegistry;
use crate::keymap::{
    ClientKeymapResponse, KeyChord, KeyMode, KeymapConfigMeta, KeymapEngine, WireActionDescriptor,
    WireKeyBinding,
};
use crate::perform::{
    ClientActionItem, ClientActionResponse, ClientCallRequest, ClientCommand, ClientInfo,
    ClientPerformRequest, ClientPrincipal, ClientRequester, ProxyCallPlan, dispatch_queued_actions,
    handle_perform_request, plan_remote_call,
};
use crate::prompts::{
    ClientReplyPayload, PromptEndedEvent, PromptIcon, PromptItem, PromptManager, PromptOpenOutcome,
    PromptRequest, PromptSpec,
};
use crate::t3_session::{
    BlockedKind, ProgramState, ProgramStatusRecord, ProviderKind, T3BlockTag, TabCardMetadata,
    build_tab_card, is_t3_session, parse_t3_block_tag, rollup_records,
};

// ============================================================================
// 1. Keymap & Action Catalog (`client.keymap`, `keymap_changed`, `client.key`)
// ============================================================================

/// Default layer name registered in `client.keymap.defaults`.
/// Per Rex API rev 24, layer names MUST be non-empty and contain no colons.
pub const OMAREX_DEFAULT_KEYMAP_LAYER: &str = "default";

/// Builds a strongly-typed Rex API rev 24 `client.keymap` registration request
/// from [`ActionRegistry`], wrapping built-in bindings in a valid [`proto::KeymapLayer`].
pub fn build_proto_keymap_request(registry: &ActionRegistry) -> proto::ClientKeymapRequest {
    build_proto_keymap_request_with_overrides(registry, None)
}

/// Builds a Rex API rev 24 `client.keymap` request including optional user override layers.
pub fn build_proto_keymap_request_with_overrides(
    registry: &ActionRegistry,
    overrides: Option<Vec<proto::KeymapLayer>>,
) -> proto::ClientKeymapRequest {
    let actions: Vec<proto::ActionDefinition> = registry
        .builtins()
        .iter()
        .map(|entry| proto::ActionDefinition {
            name: entry.name.clone(),
            kind: Some(entry.kind.as_wire().to_owned()),
            title: Some(entry.title.clone()),
            description: None,
            category: Some(entry.category.clone()),
            keywords: (!entry.keywords.is_empty()).then(|| entry.keywords.clone()),
            args: None,
            repeats: None,
            source: None,
        })
        .collect();

    let default_bindings: Vec<proto::KeyBinding> = registry
        .keymap()
        .wire_default_bindings()
        .into_iter()
        .map(|b| proto::KeyBinding {
            keys: b.keys,
            action: b.action,
            args: b.args,
            source: None,
            mode: b.mode,
        })
        .collect();

    let defaults = vec![proto::KeymapLayer {
        name: OMAREX_DEFAULT_KEYMAP_LAYER.to_owned(),
        bindings: Some(default_bindings),
        removals: None,
    }];

    proto::ClientKeymapRequest {
        actions: Some(actions),
        server_actions: Some("all".to_owned()),
        defaults: Some(defaults),
        overrides,
    }
}

/// Converts a `rex_proto::types::ClientKeymapResponse` into `tr3x_bridge`'s internal representation.
pub fn keymap_response_from_proto(resp: &proto::ClientKeymapResponse) -> ClientKeymapResponse {
    let bindings = resp
        .bindings
        .iter()
        .map(|b| WireKeyBinding {
            keys: b.keys.clone(),
            action: b.action.clone(),
            args: b.args.clone(),
            mode: b.mode.clone(),
            source: b.source.clone(),
        })
        .collect();

    let actions = resp
        .actions
        .iter()
        .map(|a| WireActionDescriptor {
            name: a.name.clone(),
            kind: a.kind.clone().unwrap_or_else(|| "client".to_owned()),
            title: a.title.clone().unwrap_or_default(),
            category: a.category.clone().unwrap_or_default(),
            keywords: a.keywords.clone().unwrap_or_default(),
        })
        .collect();

    let modes = resp
        .modes
        .iter()
        .map(|m| KeyMode {
            name: m.name.clone(),
            exclusive: m.exclusive,
            blocked: m.blocked.clone(),
        })
        .collect();

    let diagnostics = resp
        .diagnostics
        .iter()
        .filter_map(|d| serde_json::to_value(d).ok())
        .collect();

    let config = KeymapConfigMeta {
        path: resp.config.path.clone(),
        exists: resp.config.exists,
        generation: resp.config.generation,
        instance_id: resp.config.instance_id.clone(),
        hash: resp.config.hash.clone(),
    };

    ClientKeymapResponse {
        bindings,
        actions,
        modes,
        diagnostics,
        config,
    }
}

/// Applies a wire [`proto::ClientKeymapResponse`] to both the [`KeymapEngine`] and action catalog.
pub fn apply_proto_keymap(registry: &mut ActionRegistry, resp: &proto::ClientKeymapResponse) {
    let converted = keymap_response_from_proto(resp);
    registry.apply_server_keymap(&converted);
}

/// Applies a wire [`proto::ClientKeymapResponse`] directly to a [`KeymapEngine`].
pub fn apply_proto_keymap_to_engine(engine: &mut KeymapEngine, resp: &proto::ClientKeymapResponse) {
    let converted = keymap_response_from_proto(resp);
    engine.apply_server_keymap(&converted);
}

/// Evaluates an incoming `ServerEvent::KeymapChanged { generation, hash }` against the
/// currently loaded keymap state to decide whether `client.keymap` must be re-requested.
pub fn keymap_changed_needs_refresh(
    registry: &ActionRegistry,
    generation: u64,
    hash: &str,
) -> bool {
    if !hash.is_empty() {
        registry.needs_keymap_refresh(hash)
    } else {
        generation != registry.keymap().last_generation()
    }
}

/// Converts a canonical [`KeyChord`] key name to its Rex physical W3C `snake_case` key code.
pub fn chord_key_to_rex_code(canonical_key: &str) -> String {
    match canonical_key {
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphabetic() => {
            format!("key_{}", k.to_ascii_lowercase())
        }
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_digit() => format!("digit_{k}"),
        "[" => "bracket_left".to_owned(),
        "]" => "bracket_right".to_owned(),
        "=" => "equal".to_owned(),
        "-" => "minus".to_owned(),
        "," => "comma".to_owned(),
        "." => "period".to_owned(),
        "/" => "slash".to_owned(),
        "\\" => "backslash".to_owned(),
        ";" => "semicolon".to_owned(),
        "'" => "quote".to_owned(),
        "`" => "backquote".to_owned(),
        "left" => "arrow_left".to_owned(),
        "right" => "arrow_right".to_owned(),
        "up" => "arrow_up".to_owned(),
        "down" => "arrow_down".to_owned(),
        other => other.to_owned(),
    }
}

/// Builds a wire [`proto::KeyInputEvent`] from a [`KeyChord`] using canonical `+`-joined
/// Rex rev 24 modifiers (`shift+ctrl+alt+super`).
pub fn key_event_from_chord(
    chord: &KeyChord,
    text: impl Into<String>,
    unshifted_codepoint: i32,
    action: proto::KeyAction,
) -> proto::KeyInputEvent {
    proto::KeyInputEvent {
        key: chord_key_to_rex_code(&chord.key),
        mods: chord.mods.to_rex_wire_mods(),
        consumed_mods: String::new(),
        text: text.into(),
        unshifted_codepoint,
        composing: false,
        action,
    }
}

/// Converts a `KeyResolution::ForwardToServer { prefix, trigger }` outcome into a
/// ready-to-send [`proto::ClientKeyRequest`].
pub fn build_client_key_request(
    session_id: impl Into<String>,
    block_id: Option<String>,
    prefix: &[KeyChord],
    trigger: &KeyChord,
) -> proto::ClientKeyRequest {
    let prefix_events = if prefix.is_empty() {
        None
    } else {
        Some(
            prefix
                .iter()
                .map(|c| {
                    let cp = c
                        .key
                        .chars()
                        .next()
                        .filter(|_| c.key.len() == 1)
                        .map_or(0, |ch| ch as i32);
                    key_event_from_chord(c, "", cp, proto::KeyAction::Press)
                })
                .collect(),
        )
    };
    let trigger_cp = trigger
        .key
        .chars()
        .next()
        .filter(|_| trigger.key.len() == 1)
        .map_or(0, |ch| ch as i32);
    proto::ClientKeyRequest {
        session_id: session_id.into(),
        key: key_event_from_chord(trigger, "", trigger_cp, proto::KeyAction::Press),
        block_id,
        prefix: prefix_events,
    }
}

// ============================================================================
// 2. Native `rex.ui` Prompts (`client_prompt_requested` / `client.reply`)
// ============================================================================

impl From<&proto::PromptIcon> for PromptIcon {
    fn from(icon: &proto::PromptIcon) -> Self {
        match icon {
            proto::PromptIcon::Document => Self::Document,
            proto::PromptIcon::Folder => Self::Folder,
            proto::PromptIcon::Branch => Self::Branch,
            proto::PromptIcon::Terminal => Self::Terminal,
            proto::PromptIcon::Code => Self::Code,
            proto::PromptIcon::Server => Self::Server,
            proto::PromptIcon::Database => Self::Database,
            proto::PromptIcon::Package => Self::Package,
            proto::PromptIcon::Globe => Self::Globe,
            proto::PromptIcon::Link => Self::Link,
            proto::PromptIcon::Person => Self::Person,
            proto::PromptIcon::Tag => Self::Tag,
            proto::PromptIcon::Clock => Self::Clock,
            proto::PromptIcon::Star => Self::Star,
            proto::PromptIcon::Gear => Self::Gear,
            proto::PromptIcon::Plus => Self::Plus,
            proto::PromptIcon::Unknown(other) => Self::Unknown(other.clone()),
        }
    }
}

impl From<&proto::PromptItem> for PromptItem {
    fn from(item: &proto::PromptItem) -> Self {
        Self {
            id: item.id.clone(),
            title: item.title.clone(),
            subtitle: item.subtitle.clone(),
            icon: item.icon.as_ref().map(|ic| ic.as_str().to_owned()),
            detail: item.detail.clone(),
            selected: item.selected,
        }
    }
}

impl From<&proto::PromptSpec> for PromptSpec {
    fn from(spec: &proto::PromptSpec) -> Self {
        let kind = match &spec.kind {
            proto::PromptKind::Input => "input".to_owned(),
            proto::PromptKind::Pick => "pick".to_owned(),
            proto::PromptKind::Unknown(other) => other.clone(),
        };
        let items =
            (!spec.items.is_empty()).then(|| spec.items.iter().map(PromptItem::from).collect());
        Self {
            kind,
            title: spec.title.clone(),
            placeholder: spec.placeholder.clone(),
            value: spec.value.clone(),
            confirm: spec.confirm.clone(),
            multiple: spec.multiple,
            allow_other: spec.allow_other,
            items,
        }
    }
}

impl From<&proto::ClientPromptRequestedEvent> for PromptRequest {
    fn from(ev: &proto::ClientPromptRequestedEvent) -> Self {
        Self {
            call_id: ev.call_id.clone(),
            action: ev.action.clone(),
            prompt: PromptSpec::from(&ev.prompt),
            timeout_ms: ev.timeout_ms,
        }
    }
}

impl From<&proto::ClientPromptEndedEvent> for PromptEndedEvent {
    fn from(ev: &proto::ClientPromptEndedEvent) -> Self {
        Self {
            call_id: ev.call_id.clone(),
            reason: ev.reason.clone(),
        }
    }
}

impl From<ClientReplyPayload> for proto::ClientReplyRequest {
    fn from(reply: ClientReplyPayload) -> Self {
        Self {
            call_id: reply.call_id,
            result: reply.result,
            error: reply.error.map(|e| proto::ReplyError {
                code: e.code,
                message: e.message,
            }),
        }
    }
}

impl From<&ClientReplyPayload> for proto::ClientReplyRequest {
    fn from(reply: &ClientReplyPayload) -> Self {
        Self {
            call_id: reply.call_id.clone(),
            result: reply.result.clone(),
            error: reply.error.as_ref().map(|e| proto::ReplyError {
                code: e.code.clone(),
                message: e.message.clone(),
            }),
        }
    }
}

/// Outcome of routing a wire [`proto::ClientPromptRequestedEvent`] into [`PromptManager`].
#[derive(Debug, Clone, PartialEq)]
pub enum ProtoPromptOutcome {
    /// Modal prompt opened and is awaiting user interaction or `client_prompt_ended`.
    Opened,
    /// Prompt could not be opened (`busy` or `not_supported`); send this unordered `client.reply`.
    ImmediateReply(proto::ClientReplyRequest),
}

/// Converts a [`ClientReplyPayload`] into a wire unordered [`proto::ClientReplyRequest`].
pub fn reply_to_proto(reply: ClientReplyPayload) -> proto::ClientReplyRequest {
    proto::ClientReplyRequest::from(reply)
}

/// Feeds a wire [`proto::ClientPromptRequestedEvent`] directly into [`PromptManager`].
pub fn on_proto_prompt_requested(
    manager: &mut PromptManager,
    event: &proto::ClientPromptRequestedEvent,
    now: Instant,
) -> ProtoPromptOutcome {
    match manager.on_prompt_requested(PromptRequest::from(event), now) {
        PromptOpenOutcome::Opened => ProtoPromptOutcome::Opened,
        PromptOpenOutcome::ImmediateReply(reply) => {
            ProtoPromptOutcome::ImmediateReply(reply.into())
        }
    }
}

/// Feeds a wire [`proto::ClientPromptEndedEvent`] directly into [`PromptManager`].
/// Returns `true` if the active modal matched and was closed (no `client.reply` should be sent).
pub fn on_proto_prompt_ended(
    manager: &mut PromptManager,
    event: &proto::ClientPromptEndedEvent,
) -> bool {
    manager.on_prompt_ended(&PromptEndedEvent::from(event))
}

// ============================================================================
// 3. Perform & Queued Client Actions (`client_perform_requested`, `client.action`)
// ============================================================================

impl From<&proto::Principal> for ClientPrincipal {
    fn from(p: &proto::Principal) -> Self {
        Self {
            kind: p.kind.clone(),
            transport: p.transport.clone(),
            display: p.display.clone(),
            remote_addr: p.remote_addr.clone(),
            uid: p.uid,
            gid: p.gid,
            pid: p.pid,
            sid: p.sid.clone(),
            user_id: p.user_id.clone(),
            login_name: p.login_name.clone(),
            node: p.node.clone(),
            tags: p.tags.clone(),
            path: p.path.clone(),
        }
    }
}

impl From<&proto::ClientInfo> for ClientInfo {
    fn from(info: &proto::ClientInfo) -> Self {
        Self {
            kind: info.kind.clone(),
            platform: info.platform.clone(),
            name: info.name.clone(),
            version: info.version.clone(),
            label: info.label.clone(),
        }
    }
}

impl From<&proto::RequesterInfo> for ClientRequester {
    fn from(req: &proto::RequesterInfo) -> Self {
        Self {
            client_id: req.client_id.clone(),
            info: req.info.as_ref().map(ClientInfo::from),
            principal: ClientPrincipal::from(&req.principal),
        }
    }
}

impl From<&proto::ClientPerformRequestedEvent> for ClientPerformRequest {
    fn from(ev: &proto::ClientPerformRequestedEvent) -> Self {
        Self {
            call_id: ev.call_id.clone(),
            action: ev.action.clone(),
            args: ev.args.clone(),
            origin: ev.origin.clone(),
            session_id: ev.session_id.clone(),
            block_id: ev.block_id.clone(),
            requester: ClientRequester::from(&ev.requester),
        }
    }
}

impl From<&proto::ClientActionResponse> for ClientActionResponse {
    fn from(resp: &proto::ClientActionResponse) -> Self {
        Self {
            result: resp.result.clone(),
            client_actions: resp
                .client_actions
                .iter()
                .map(|a| ClientActionItem {
                    action: a.action.clone(),
                    args: a.args.clone(),
                })
                .collect(),
            ordered_after: resp.ordered_after.clone(),
        }
    }
}

/// Authorizes and dispatches a wire [`proto::ClientPerformRequestedEvent`], returning the
/// parsed [`ClientCommand`] (if authorized and supported) alongside the unordered
/// [`proto::ClientReplyRequest`] to send back over `/api/control`.
pub fn handle_proto_perform_request(
    event: &proto::ClientPerformRequestedEvent,
    local_uid: Option<u32>,
) -> (Option<ClientCommand>, proto::ClientReplyRequest) {
    let req = ClientPerformRequest::from(event);
    let (cmd, reply) = handle_perform_request(&req, local_uid);
    (cmd, reply.into())
}

/// Converts inline `client_actions` from a wire [`proto::ClientActionResponse`] into
/// ordered native [`ClientCommand`]s (used by `tr3x` `rex.client.queue("session.select", ...)`).
pub fn dispatch_proto_queued_actions(resp: &proto::ClientActionResponse) -> Vec<ClientCommand> {
    let converted = ClientActionResponse::from(resp);
    dispatch_queued_actions(&converted)
}

// ============================================================================
// 4. Multi-Server Action Proxy (`client_call_requested`)
// ============================================================================

impl From<&proto::ClientCallRequestedEvent> for ClientCallRequest {
    fn from(ev: &proto::ClientCallRequestedEvent) -> Self {
        Self {
            call_id: ev.call_id.clone(),
            server: ev.server.clone(),
            method: ev.method.clone(),
            args: ev.args.clone(),
        }
    }
}

/// Validates a wire [`proto::ClientCallRequestedEvent`] against connected server IDs/names.
/// Returns a [`ProxyCallPlan`] (10s timeout) on success, or an immediate unordered
/// [`proto::ClientReplyRequest`] error on validation failure.
pub fn plan_proto_remote_call(
    event: &proto::ClientCallRequestedEvent,
    connected_servers: &[&str],
) -> Result<ProxyCallPlan, proto::ClientReplyRequest> {
    plan_remote_call(ClientCallRequest::from(event), connected_servers).map_err(Into::into)
}

/// Builds a wire unordered [`proto::ClientReplyRequest`] for a succeeded [`ProxyCallPlan`].
pub fn proxy_reply_ok(
    plan: &ProxyCallPlan,
    result: serde_json::Value,
) -> proto::ClientReplyRequest {
    plan.reply_ok(result).into()
}

/// Builds a wire unordered [`proto::ClientReplyRequest`] for a failed/timed-out [`ProxyCallPlan`].
pub fn proxy_reply_error(
    plan: &ProxyCallPlan,
    code: &str,
    message: impl Into<String>,
) -> proto::ClientReplyRequest {
    plan.reply_error(code, message).into()
}

// ============================================================================
// 5. SessionView / WindowView / BlockView + OSC 7501 -> Ordered TabCardMetadata
// ============================================================================

/// Converts a single wire [`proto::ProgramStatusRecord`] into `tr3x_bridge`'s
/// [`ProgramStatusRecord`], preserving `effective_app` and treating unknown `kind` values
/// as `None` per Rex API rev 24.
pub fn program_status_from_proto(
    rec: &proto::ProgramStatusRecord,
    unseen: bool,
) -> ProgramStatusRecord {
    ProgramStatusRecord {
        id: rec.id.clone().unwrap_or_default(),
        state: ProgramState::parse(&rec.state),
        kind: rec.kind.as_deref().and_then(BlockedKind::parse_opt),
        progress: rec.progress,
        app: rec.app.clone(),
        effective_app: rec.effective_app.clone(),
        title: rec.title.clone(),
        msg: rec.msg.clone(),
        unseen,
    }
}

impl From<&proto::ProgramStatusRecord> for ProgramStatusRecord {
    fn from(rec: &proto::ProgramStatusRecord) -> Self {
        program_status_from_proto(rec, false)
    }
}

/// Converts a full [`proto::TerminalProgramStatusResponse`] into internal OSC 7501 records.
pub fn program_statuses_from_response(
    resp: &proto::TerminalProgramStatusResponse,
    unseen: bool,
) -> Vec<ProgramStatusRecord> {
    resp.records
        .iter()
        .map(|r| program_status_from_proto(r, unseen))
        .collect()
}

/// Optional live per-block enrichment data (OSC 7501 status records, foreground process,
/// working directory/project name, and last-activity age) used when deriving sidebar cards
/// from a [`proto::SessionView`].
#[derive(Debug, Clone, Default)]
pub struct BlockCardContext {
    pub program_statuses: Vec<ProgramStatusRecord>,
    pub process_name: Option<String>,
    pub project_or_cwd: Option<String>,
    pub age_secs: Option<u64>,
    /// Terminal title the program set via OSC 0/2 (e.g. `claude`'s task name or bash's
    /// `user@host:~/dir`). Used to name windows that still carry the server's placeholder label.
    pub title: Option<String>,
}

/// Returns `true` for the placeholder label the Rex server assigns to windows created without
/// a label: `Window <last 8 hex digits of the window ID>` (e.g. `Window e340e9f2` for
/// `window:01a217b3-08c9-7d90-a0db-92f2e340e9f2`). Such labels say nothing to the user, so
/// clients should derive a name from the window's content instead, as the Rex app does.
pub fn is_placeholder_window_label(label: &str, window_id: Option<&str>) -> bool {
    let Some(suffix) = label.trim().strip_prefix("Window ") else {
        return false;
    };
    if suffix.len() != 8 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    window_id.is_none_or(|id| {
        id.to_ascii_lowercase()
            .ends_with(&suffix.to_ascii_lowercase())
    })
}

fn is_shell_name(name: &str) -> bool {
    matches!(
        name.trim().trim_start_matches('-'),
        "bash" | "zsh" | "fish" | "sh" | "dash" | "nu" | "elvish" | "xonsh"
    )
}

/// Shortens a terminal title for display in a tab or pane header.
///
/// Shell prompts commonly set the title to `user@host:path` (bash's default `PROMPT_COMMAND`),
/// which is mostly noise in a sidebar; it becomes the last path component (`~` stays `~`).
/// Any other title (such as an agent's task name) is returned trimmed and unchanged.
pub fn friendly_terminal_title(title: &str) -> String {
    let title = title.trim();
    if let Some((user_host, path)) = title.split_once(':')
        && let Some((user, host)) = user_host.split_once('@')
        && !user.is_empty()
        && !host.is_empty()
        && !user_host.contains(char::is_whitespace)
    {
        let path = path.trim().trim_end_matches('/');
        if path.is_empty() {
            return "/".to_owned();
        }
        return path.rsplit('/').next().unwrap_or(path).to_owned();
    }
    title.to_owned()
}

impl BlockCardContext {
    /// Replaces OSC 7501 statuses from a `com.superlogical.terminal.program_status` response.
    pub fn set_proto_statuses(
        &mut self,
        resp: &proto::TerminalProgramStatusResponse,
        unseen: bool,
    ) {
        self.program_statuses = program_statuses_from_response(resp, unseen);
    }

    /// Populates `process_name` and `project_or_cwd` from `com.superlogical.terminal.process`.
    pub fn apply_proto_process(&mut self, resp: &proto::TerminalProcessResponse) {
        let proc_info = resp.foreground.as_ref().or(resp.child.as_ref());
        if let Some(info) = proc_info {
            if !info.name.trim().is_empty() {
                self.process_name = Some(info.name.clone());
            }
            if let Some(cwd) = info.cwd.as_deref()
                && let Some(proj) = extract_project_name_from_pwd(cwd)
            {
                self.project_or_cwd = Some(proj);
            }
        }
    }

    /// Populates `project_or_cwd` from `com.superlogical.terminal.pwd` (supports `file://` OSC 7).
    pub fn apply_proto_pwd(&mut self, resp: &proto::TerminalPwdResponse) {
        if let Some(pwd) = resp.pwd.as_deref()
            && let Some(proj) = extract_project_name_from_pwd(pwd)
        {
            self.project_or_cwd = Some(proj);
        }
        if self.process_name.is_none()
            && let Some(owner) = resp.owner.as_ref()
            && !owner.name.trim().is_empty()
        {
            self.process_name = Some(owner.name.clone());
        }
    }
}

/// Extracts a concise folder/project display name from an OSC 7 `file://host/path` URL
/// or a plain filesystem path.
pub fn extract_project_name_from_pwd(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path_str = if let Some(rest) = trimmed.strip_prefix("file://") {
        // Strip authority/hostname up to the first `/` (e.g. `file://archbox/home/user/projects/omarex`).
        let idx = rest.find('/')?;
        &rest[idx..]
    } else {
        trimmed
    };

    let without_trailing = path_str.trim_end_matches('/');
    if without_trailing.is_empty() {
        return Some("/".to_owned());
    }
    let segment = without_trailing.rsplit('/').next()?;
    let decoded = percent_decode_utf8(segment);
    (!decoded.trim().is_empty()).then_some(decoded)
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Sidebar tab card paired with Rex window/block identity and `tr3x` structural metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowTabCard {
    pub window_id: Option<String>,
    pub focused_block_id: Option<String>,
    pub is_active: bool,
    pub is_t3_window: bool,
    pub t3_tag: Option<T3BlockTag>,
    pub has_floating_layer: bool,
    pub card: TabCardMetadata,
}

/// Derives a [`WindowTabCard`] from a single [`proto::WindowView`] and optional per-block metadata.
pub fn build_window_tab_card(
    window: &proto::WindowView,
    active_window_id: Option<&str>,
    session_is_t3: bool,
    block_contexts: &HashMap<String, BlockCardContext>,
) -> WindowTabCard {
    let is_active = window.window_id.is_some() && window.window_id.as_deref() == active_window_id;
    let has_floating_layer = window
        .layers
        .iter()
        .any(|layer| layer.kind.eq_ignore_ascii_case("floating"));

    // Flatten blocks in server layer order; identify primary block (focused > first tiled > first).
    let mut all_blocks: Vec<&proto::BlockView> = Vec::new();
    let mut first_tiled: Option<&proto::BlockView> = None;
    for layer in &window.layers {
        for block in &layer.blocks {
            if first_tiled.is_none() && layer.kind.eq_ignore_ascii_case("tiled") {
                first_tiled = Some(block);
            }
            all_blocks.push(block);
        }
    }

    let primary_block = window
        .focused_block_id
        .as_deref()
        .and_then(|fid| {
            all_blocks
                .iter()
                .copied()
                .find(|b| b.block_id.as_deref() == Some(fid))
        })
        .or(first_tiled)
        .or_else(|| all_blocks.first().copied());

    // Parse tr3x tag (`t3:<env8>/<thread>` or `t3#<hash>`) from primary block, any block, or window label.
    let t3_tag = primary_block
        .and_then(|b| parse_t3_block_tag(&b.label))
        .or_else(|| all_blocks.iter().find_map(|b| parse_t3_block_tag(&b.label)))
        .or_else(|| parse_t3_block_tag(&window.label));

    let is_t3_window = session_is_t3 || t3_tag.is_some();

    // Aggregate OSC 7501 records across ALL blocks in the window so a blocked split pane surfaces
    // its attention dot on the tab card.
    let mut combined_records: Vec<ProgramStatusRecord> = Vec::new();
    let primary_ctx = primary_block
        .and_then(|b| b.block_id.as_deref())
        .and_then(|id| block_contexts.get(id));

    let mut process_name = primary_ctx.and_then(|c| c.process_name.as_deref());
    let mut project_or_cwd = primary_ctx.and_then(|c| c.project_or_cwd.as_deref());
    let mut age_secs = primary_ctx.and_then(|c| c.age_secs);

    for block in &all_blocks {
        if let Some(id) = block.block_id.as_deref()
            && let Some(ctx) = block_contexts.get(id)
        {
            combined_records.extend(ctx.program_statuses.iter().cloned());
            if process_name.is_none() {
                process_name = ctx.process_name.as_deref();
            }
            if project_or_cwd.is_none() {
                project_or_cwd = ctx.project_or_cwd.as_deref();
            }
            if age_secs.is_none() {
                age_secs = ctx.age_secs;
            }
        }
    }

    let rolled = rollup_records(&combined_records);

    // Name the tab after an explicit window label; for unlabeled windows (empty or the server's
    // `Window <id8>` placeholder) fall back to the primary block's terminal title, then its block
    // label, the foreground program, and the project directory, like the Rex app.
    let terminal_title = primary_ctx
        .and_then(|c| c.title.as_deref())
        .map(friendly_terminal_title)
        .filter(|t| !t.is_empty());
    let raw_title: String = if !window.label.trim().is_empty()
        && !is_placeholder_window_label(&window.label, window.window_id.as_deref())
    {
        window.label.clone()
    } else if let Some(title) = terminal_title {
        title
    } else if let Some(b) = primary_block
        && !b.label.trim().is_empty()
        && b.label.trim() != "shell"
    {
        b.label.clone()
    } else if let Some(name) = process_name.filter(|n| !n.trim().is_empty() && !is_shell_name(n)) {
        name.to_owned()
    } else if let Some(project) = project_or_cwd.filter(|p| !p.trim().is_empty()) {
        project.to_owned()
    } else {
        "Shell".to_owned()
    };

    let mut card = build_tab_card(
        &raw_title,
        project_or_cwd,
        process_name,
        rolled.as_ref(),
        age_secs,
    );

    // Promote unrecognized provider to T3Code inside T3 Code sessions/tagged windows so threads
    // display the T3 icon even before the first OSC 7501 `app` report arrives.
    if is_t3_window && card.provider == ProviderKind::Other {
        card.provider = ProviderKind::T3Code;
    }

    WindowTabCard {
        window_id: window.window_id.clone(),
        focused_block_id: window
            .focused_block_id
            .clone()
            .or_else(|| primary_block.and_then(|b| b.block_id.clone())),
        is_active,
        is_t3_window,
        t3_tag,
        has_floating_layer,
        card,
    }
}

/// Converts every window in a [`proto::SessionView`] into [`WindowTabCard`]s,
/// **strictly preserving server window order** (critical for `tr3x`'s pinned/blocked/working/recency
/// sidebar sorting).
pub fn build_session_tab_cards(
    view: &proto::SessionView,
    block_contexts: &HashMap<String, BlockCardContext>,
) -> Vec<WindowTabCard> {
    let session_is_t3 = is_t3_session(&view.label);
    let active_window_id = view.active_window_id.as_deref();
    view.windows
        .iter()
        .map(|w| build_window_tab_card(w, active_window_id, session_is_t3, block_contexts))
        .collect()
}

/// Convenience projection returning only the ordered [`TabCardMetadata`] list for a [`proto::SessionView`].
pub fn build_session_tab_card_metadata(
    view: &proto::SessionView,
    block_contexts: &HashMap<String, BlockCardContext>,
) -> Vec<TabCardMetadata> {
    build_session_tab_cards(view, block_contexts)
        .into_iter()
        .map(|w| w.card)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{KeyResolution, Modifiers};
    use crate::t3_session::{PriorityDot, WorkingIndicator};

    #[test]
    fn proto_keymap_request_response_round_trip_and_blocked_mode_keys() {
        let mut reg = ActionRegistry::default();
        let req = build_proto_keymap_request(&reg);

        // Rex rev 24 requires `defaults` to be Vec<KeymapLayer> with colon-free layer name.
        let layers = req.defaults.as_ref().expect("default keymap layer");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "default");
        assert!(!layers[0].name.contains(':'));
        assert!(layers[0].bindings.as_ref().is_some_and(|b| !b.is_empty()));
        assert!(req.actions.as_ref().is_some_and(|a| !a.is_empty()));

        // Verify JSON serialization matches exact rev 24 wire schema.
        let req_json = serde_json::to_value(&req).expect("serialize ClientKeymapRequest");
        assert!(req_json["defaults"][0]["name"].is_string());
        assert!(req_json["defaults"][0]["bindings"].is_array());

        // Realistic rev 24 server response (including Go `null` slices and `blocked` key list).
        let raw_resp = r#"{
            "bindings": [
                { "keys": ["ctrl+cmd+i"], "action": "t3_inbox", "source": "tr3x.lua:430" },
                { "keys": ["ctrl+cmd+a"], "action": "t3_answer", "source": "tr3x.lua:433" },
                { "keys": ["ctrl+b", "f"], "action": "client.key.forward", "source": "init.lua:12" },
                { "keys": ["ctrl+alt+m"], "action": "client.mode.enter", "args": { "name": "triage" }, "source": "init.lua:15" },
                { "keys": ["j"], "action": "t3_inbox", "mode": "triage", "source": "init.lua:16" }
            ],
            "diagnostics": null,
            "actions": [
                { "name": "t3_inbox", "kind": "lua", "title": "Go to Thread That Needs You", "category": "T3 Code", "keywords": "inbox blocked", "source": "tr3x.lua:10" },
                { "name": "t3_answer", "kind": "lua", "title": "Answer…", "category": "T3 Code", "keywords": "approve decline", "source": "tr3x.lua:20" },
                { "name": "com.superlogical.terminal.reset", "kind": "block", "title": "Reset Terminal", "category": "Pane" },
                { "name": "com.superlogical.terminal.write", "kind": "block", "args": { "type": "object" } }
            ],
            "modes": [
                { "name": "triage", "exclusive": false, "blocked": ["ctrl+shift+p"] },
                { "name": "empty_mode", "exclusive": false, "blocked": null }
            ],
            "config": {
                "path": "/home/user/.config/rex/init.lua",
                "exists": true,
                "generation": 4,
                "instance_id": "inst-1",
                "hash": "sha256-rev24-keymap"
            }
        }"#;

        let proto_resp: proto::ClientKeymapResponse =
            serde_json::from_str(raw_resp).expect("parse proto ClientKeymapResponse");
        apply_proto_keymap(&mut reg, &proto_resp);

        // Hash gating on keymap_changed.
        assert!(!keymap_changed_needs_refresh(
            &reg,
            4,
            "sha256-rev24-keymap"
        ));
        assert!(keymap_changed_needs_refresh(&reg, 5, "sha256-updated"));

        // tr3x `ctrl+cmd+i` auto-aliases to Hyprland-safe Ctrl+Alt+I.
        assert_eq!(
            reg.get("t3_inbox").and_then(|e| e.shortcut_hint.as_deref()),
            Some("Ctrl+Alt+I")
        );
        // Block actions with title appear in palette, while raw schema-only block methods do NOT.
        assert!(reg.get("com.superlogical.terminal.reset").is_some());
        assert!(reg.get("com.superlogical.terminal.write").is_none());

        // Forwarded sequence (`ctrl+b`, `f`) builds valid rev 24 ClientKeyRequest with `+`-joined mods.
        assert!(matches!(
            reg.keymap_mut()
                .resolve_key(KeyChord::new(Modifiers::CTRL, "b")),
            KeyResolution::PendingSequence { .. }
        ));
        match reg
            .keymap_mut()
            .resolve_key(KeyChord::new(Modifiers::NONE, "f"))
        {
            KeyResolution::ForwardToServer { prefix, trigger } => {
                let key_req = build_client_key_request(
                    "session:1",
                    Some("block:1".to_owned()),
                    &prefix,
                    &trigger,
                );
                assert_eq!(key_req.key.key, "key_f");
                assert_eq!(key_req.key.mods, "");
                let pfx = key_req.prefix.expect("prefix present");
                assert_eq!(pfx.len(), 1);
                assert_eq!(pfx[0].key, "key_b");
                assert_eq!(pfx[0].mods, "ctrl");
            }
            other => panic!("expected ForwardToServer, got {other:?}"),
        }

        // Non-exclusive mode with `blocked: ["ctrl+shift+p"]` blocks that exact global chord
        // while still allowing other global chords (like Ctrl+Shift+B) through!
        assert!(matches!(
            reg.keymap_mut()
                .resolve_key(KeyChord::new(Modifiers::CTRL_ALT, "m")),
            KeyResolution::ModeEntered { mode, .. } if mode == "triage"
        ));
        assert_eq!(
            reg.keymap_mut()
                .resolve_key(KeyChord::new(Modifiers::CTRL_SHIFT, "p")),
            KeyResolution::Blocked
        );
        assert!(matches!(
            reg.keymap_mut()
                .resolve_key(KeyChord::new(Modifiers::CTRL_SHIFT, "b")),
            KeyResolution::Action { action, .. } if action == "client.sidebar.toggle"
        ));
        assert_eq!(
            reg.keymap_mut()
                .resolve_key(KeyChord::new(Modifiers::NONE, "escape")),
            KeyResolution::ModeExited
        );
    }

    #[test]
    fn proto_prompt_pick_allow_other_multiple_and_ended_round_trip() {
        let mut mgr = PromptManager::new();
        let t0 = Instant::now();

        // 1. Single-select pick with `allow_other: true` (e.g. `t3_snooze`).
        let snooze_ev: proto::ClientPromptRequestedEvent = serde_json::from_str(
            r#"{
                "call_id": "prompt-snooze-1",
                "action": "t3_snooze",
                "timeout_ms": 60000,
                "prompt": {
                    "kind": "pick",
                    "title": "Snooze Thread",
                    "placeholder": "Pick or type duration (e.g. 45m)",
                    "confirm": "Snooze",
                    "multiple": false,
                    "allow_other": true,
                    "items": [
                        { "id": "1h", "title": "1 Hour", "icon": "clock", "detail": "in 1h" },
                        { "id": "tomorrow", "title": "Tomorrow", "icon": "branch", "detail": "9am" }
                    ]
                }
            }"#,
        )
        .expect("parse snooze prompt");

        assert_eq!(
            on_proto_prompt_requested(&mut mgr, &snooze_ev, t0),
            ProtoPromptOutcome::Opened
        );
        let active = mgr.active_mut().expect("active prompt");
        assert_eq!(active.items[0].parsed_icon(), Some(PromptIcon::Clock));
        assert_eq!(active.items[1].parsed_icon(), Some(PromptIcon::Branch));
        active.set_input_text("45m");
        assert!(active.has_other_row());

        let reply: proto::ClientReplyRequest =
            reply_to_proto(mgr.submit_enter().expect("custom allow_other submit"));
        assert_eq!(reply.call_id, "prompt-snooze-1");
        assert_eq!(reply.result, Some(serde_json::json!({ "text": "45m" })));
        assert!(reply.error.is_none());

        // 2. Multi-select pick (`multiple: true`) with preselected item + toggle + confirm.
        let multi_ev: proto::ClientPromptRequestedEvent = serde_json::from_str(
            r#"{
                "call_id": "prompt-multi-2",
                "action": "t3_answer",
                "timeout_ms": 30000,
                "prompt": {
                    "kind": "pick",
                    "title": "Select Files",
                    "confirm": "Apply",
                    "multiple": true,
                    "allow_other": false,
                    "items": [
                        { "id": "f1", "title": "Cargo.toml", "icon": "document", "selected": true },
                        { "id": "f2", "title": "src/lib.rs", "icon": "code", "selected": false },
                        { "id": "f3", "title": "README.md", "icon": "unknown_future_icon", "selected": false }
                    ]
                }
            }"#,
        )
        .expect("parse multi prompt");

        assert_eq!(
            on_proto_prompt_requested(&mut mgr, &multi_ev, t0),
            ProtoPromptOutcome::Opened
        );
        mgr.active_mut().expect("multi active").move_focus(2);
        assert!(mgr.submit_enter().is_none()); // Space/Enter toggles f3
        let multi_reply = reply_to_proto(mgr.submit_confirm().expect("Ctrl+Enter confirm"));
        assert_eq!(multi_reply.call_id, "prompt-multi-2");
        assert_eq!(
            multi_reply.result,
            Some(serde_json::json!({ "ids": ["f1", "f3"] }))
        );

        // 3. Unknown future prompt kind -> immediate `not_supported` ClientReplyRequest.
        let unknown_ev: proto::ClientPromptRequestedEvent = serde_json::from_str(
            r#"{
                "call_id": "prompt-unknown-3",
                "timeout_ms": 5000,
                "prompt": { "kind": "color_wheel", "title": "Pick Color", "items": null }
            }"#,
        )
        .expect("parse unknown prompt");
        match on_proto_prompt_requested(&mut mgr, &unknown_ev, t0) {
            ProtoPromptOutcome::ImmediateReply(r) => {
                assert_eq!(r.call_id, "prompt-unknown-3");
                assert_eq!(
                    r.error.as_ref().map(|e| e.code.as_str()),
                    Some("not_supported")
                );
            }
            ProtoPromptOutcome::Opened => panic!("unknown prompt kind must not open"),
        }

        // 4. `client_prompt_ended` closes active modal cleanly without reply.
        assert_eq!(
            on_proto_prompt_requested(&mut mgr, &snooze_ev, t0),
            ProtoPromptOutcome::Opened
        );
        let ended = proto::ClientPromptEndedEvent {
            call_id: "prompt-snooze-1".to_owned(),
            reason: "answered".to_owned(),
        };
        assert!(on_proto_prompt_ended(&mut mgr, &ended));
        assert!(!mgr.is_open());
    }

    #[test]
    fn proto_perform_queued_actions_and_remote_call_proxy() {
        // 1. Inline `client.action` response with `ordered_after: String` & `client_actions`.
        let action_resp: proto::ClientActionResponse = serde_json::from_str(
            r#"{
                "result": null,
                "client_actions": [
                    {
                        "action": "session.select",
                        "args": {
                            "session_id": "session:t3",
                            "window_id": "window:7",
                            "block_id": "block:7"
                        }
                    },
                    {
                        "action": "pane.zoom"
                    }
                ],
                "ordered_after": "key-41"
            }"#,
        )
        .expect("parse ClientActionResponse");
        let cmds = dispatch_proto_queued_actions(&action_resp);
        assert_eq!(
            cmds,
            vec![
                ClientCommand::SelectSession {
                    session_id: Some("session:t3".to_owned()),
                    window_id: Some("window:7".to_owned()),
                    block_id: Some("block:7".to_owned()),
                },
                ClientCommand::ZoomPane,
            ]
        );

        // Also handle Go `null` `client_actions` cleanly.
        let null_resp: proto::ClientActionResponse =
            serde_json::from_str(r#"{"result":{},"client_actions":null,"ordered_after":null}"#)
                .expect("parse null client_actions");
        assert!(dispatch_proto_queued_actions(&null_resp).is_empty());

        // 2. Authorized Unix peer `client_perform_requested`.
        let perform_ev: proto::ClientPerformRequestedEvent = serde_json::from_str(
            r#"{
                "call_id": "perf-42",
                "action": "pane.paste",
                "origin": "cli",
                "session_id": "session:t3",
                "block_id": "block:9",
                "requester": {
                    "client_id": "client:tr3x",
                    "info": { "kind": "cli", "name": "tr3x" },
                    "principal": {
                        "kind": "peer",
                        "transport": "unix",
                        "display": "uid 1000",
                        "uid": 1000,
                        "tags": null,
                        "path": null
                    }
                }
            }"#,
        )
        .expect("parse ClientPerformRequestedEvent");
        let (cmd, reply) = handle_proto_perform_request(&perform_ev, Some(1000));
        assert_eq!(
            cmd,
            Some(ClientCommand::Paste {
                session_id: Some("session:t3".to_owned()),
                block_id: Some("block:9".to_owned()),
            })
        );
        assert_eq!(reply.call_id, "perf-42");
        assert!(reply.error.is_none());

        // Tagged Tailscale machine principal (CI runner) is refused for interactive perform.
        let mut tagged_ts = perform_ev.clone();
        tagged_ts.call_id = "perf-ci".to_owned();
        tagged_ts.requester.principal.kind = "tailscale".to_owned();
        tagged_ts.requester.principal.tags = vec!["tag:ci-runner".to_owned()];
        let (ci_cmd, ci_reply) = handle_proto_perform_request(&tagged_ts, Some(1000));
        assert!(ci_cmd.is_none());
        assert_eq!(
            ci_reply.error.as_ref().map(|e| e.code.as_str()),
            Some("refused")
        );

        // Untagged interactive Tailscale user principal is allowed.
        tagged_ts.call_id = "perf-user".to_owned();
        tagged_ts.requester.principal.tags.clear();
        tagged_ts.requester.principal.user_id = Some("u123".to_owned());
        let (ts_cmd, ts_reply) = handle_proto_perform_request(&tagged_ts, Some(1000));
        assert!(ts_cmd.is_some());
        assert!(ts_reply.error.is_none());

        // 3. Multi-server `client_call_requested` -> ProxyCallPlan -> ClientReplyRequest.
        let call_ev = proto::ClientCallRequestedEvent {
            call_id: "proxy-99".to_owned(),
            server: "mac-studio".to_owned(),
            method: "session.list".to_owned(),
            args: Some(serde_json::json!({})),
        };
        let plan = plan_proto_remote_call(&call_ev, &["local", "mac-studio"]).expect("proxy plan");
        assert_eq!(plan.target_server, "mac-studio");
        assert_eq!(plan.method, "session.list");
        let ok_reply = proxy_reply_ok(&plan, serde_json::json!({ "sessions": [] }));
        assert_eq!(ok_reply.call_id, "proxy-99");
        assert!(ok_reply.error.is_none());

        let missing_err =
            plan_proto_remote_call(&call_ev, &["local"]).expect_err("unconnected server must fail");
        assert_eq!(
            missing_err.error.as_ref().map(|e| e.code.as_str()),
            Some("not_found")
        );
    }

    #[test]
    fn t3_code_23_window_session_view_preserves_exact_server_order_and_statuses() {
        let mut windows = Vec::with_capacity(23);
        let mut contexts: HashMap<String, BlockCardContext> = HashMap::new();

        for i in 0..23usize {
            let win_id = format!("window:{i:02}");
            let blk_id = format!("block:{i:02}");
            let title = if i == 1 {
                "↳ Subagent: audit wire schema".to_owned()
            } else {
                format!("Thread {i:02} — workstream")
            };

            let mut layers = vec![proto::LayerView {
                layer_id: Some(format!("layer:{i:02}")),
                kind: "tiled".to_owned(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: None,
                blocks: vec![proto::BlockView {
                    block_id: Some(blk_id.clone()),
                    label: format!("t3:1baa1939/thread-{i:02}"),
                    creator_name: Some("com.superlogical.terminal".to_owned()),
                    flavor: Some("com.superlogical.terminal.shell".to_owned()),
                    host_id: None,
                    owner: None,
                }],
            }];

            // Attach a floating `/pr` drawer to window 5.
            if i == 5 {
                layers.push(proto::LayerView {
                    layer_id: Some("layer:pr-05".to_owned()),
                    kind: "floating".to_owned(),
                    bounds: [0.45, 0.0, 0.55, 1.0],
                    state: Some("normal".to_owned()),
                    layout: None,
                    blocks: vec![proto::BlockView {
                        block_id: Some("block:pr-05".to_owned()),
                        label: "Pull Request".to_owned(),
                        creator_name: Some("com.superlogical.terminal".to_owned()),
                        flavor: None,
                        host_id: None,
                        owner: None,
                    }],
                });
            }

            windows.push(proto::WindowView {
                window_id: Some(win_id),
                label: title,
                focused_block_id: Some(blk_id.clone()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers,
            });

            let mut ctx = BlockCardContext {
                age_secs: Some((i as u64 + 1) * 60),
                ..Default::default()
            };
            ctx.apply_proto_pwd(&proto::TerminalPwdResponse {
                pwd: Some(format!("file://archbox/home/user/projects/repo-{i:02}")),
                owner: None,
            });

            if i == 0 {
                // Blocked child record inheriting `effective_app = "claude"` from parent!
                ctx.set_proto_statuses(
                    &proto::TerminalProgramStatusResponse {
                        records: vec![proto::ProgramStatusRecord {
                            id: Some("tool/bash".to_owned()),
                            state: "blocked".to_owned(),
                            kind: Some("permission".to_owned()),
                            progress: None,
                            app: None,
                            effective_app: Some("claude".to_owned()),
                            title: Some("Approve command".to_owned()),
                            msg: Some("Needs approval".to_owned()),
                            updated_at: "2026-11-26T09:00:00Z".to_owned(),
                            owner: None,
                        }],
                    },
                    true,
                );
            } else if i == 1 {
                ctx.set_proto_statuses(
                    &proto::TerminalProgramStatusResponse {
                        records: vec![proto::ProgramStatusRecord {
                            id: None,
                            state: "working".to_owned(),
                            kind: None,
                            progress: Some(65),
                            app: Some("codex".to_owned()),
                            effective_app: Some("codex".to_owned()),
                            title: None,
                            msg: Some("Running cargo test".to_owned()),
                            updated_at: "2026-11-26T09:01:00Z".to_owned(),
                            owner: None,
                        }],
                    },
                    false,
                );
            } else if i == 2 {
                ctx.set_proto_statuses(
                    &proto::TerminalProgramStatusResponse {
                        records: vec![proto::ProgramStatusRecord {
                            id: None,
                            state: "done".to_owned(),
                            kind: None,
                            progress: None,
                            app: Some("gemini".to_owned()),
                            effective_app: None,
                            title: None,
                            msg: None,
                            updated_at: "2026-11-26T09:02:00Z".to_owned(),
                            owner: None,
                        }],
                    },
                    true,
                );
            }
            contexts.insert(blk_id, ctx);
        }

        let view = proto::SessionView {
            revision: 412,
            session_id: Some("session:t3-main".to_owned()),
            label: "T3 Code".to_owned(),
            active_window_id: Some("window:05".to_owned()),
            windows,
        };

        let cards = build_session_tab_cards(&view, &contexts);
        assert_eq!(cards.len(), 23);

        // Verify strict server window order preservation across all 23 windows.
        for (i, w) in cards.iter().enumerate() {
            assert_eq!(
                w.window_id.as_deref(),
                Some(format!("window:{i:02}").as_str())
            );
            assert!(w.is_t3_window);
            assert_eq!(
                w.t3_tag,
                Some(T3BlockTag::EnvThread {
                    env_prefix: "1baa1939".to_owned(),
                    thread_id: format!("thread-{i:02}"),
                })
            );
            assert_eq!(w.is_active, i == 5);
            assert_eq!(w.has_floating_layer, i == 5);
        }

        // Window 0: Blocked via inherited `effective_app = "claude"`.
        assert_eq!(cards[0].card.provider, ProviderKind::Claude);
        assert_eq!(cards[0].card.priority_dot, PriorityDot::Blocked);
        assert_eq!(cards[0].card.subtitle, "repo-00 · Needs approval");
        assert_eq!(cards[0].card.relative_age.as_deref(), Some("1m"));

        // Window 1: Subagent (`↳`) working with 65% progress pie & OpenAI/Codex provider.
        assert!(cards[1].card.is_subagent);
        assert_eq!(cards[1].card.provider, ProviderKind::OpenAi);
        assert_eq!(
            cards[1].card.working_indicator,
            WorkingIndicator::ProgressPie(65)
        );

        // Window 2: Unseen Done -> DoneUnseen attention dot & Gemini provider.
        assert_eq!(cards[2].card.provider, ProviderKind::Gemini);
        assert_eq!(cards[2].card.priority_dot, PriorityDot::DoneUnseen);
        assert_eq!(cards[2].card.subtitle, "repo-02 · Done");

        // Window 22: No OSC 7501 yet inside T3 Code -> falls back to T3Code icon, not generic Other.
        assert_eq!(cards[22].card.provider, ProviderKind::T3Code);
        assert_eq!(cards[22].card.subtitle, "repo-22");

        // Convenience metadata projection preserves exact order too.
        let meta = build_session_tab_card_metadata(&view, &contexts);
        assert_eq!(meta.len(), 23);
        assert_eq!(meta[0].title, "Thread 00 — workstream");
        assert_eq!(meta[1].title, "↳ Subagent: audit wire schema");
        assert_eq!(meta[22].title, "Thread 22 — workstream");
    }

    fn single_block_window(window_id: &str, label: &str, block_id: &str) -> proto::WindowView {
        proto::WindowView {
            window_id: Some(window_id.into()),
            label: label.into(),
            focused_block_id: Some(block_id.into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![proto::LayerView {
                layer_id: Some("layer:1".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: None,
                blocks: vec![proto::BlockView {
                    block_id: Some(block_id.into()),
                    label: "shell".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: None,
                    owner: None,
                }],
            }],
        }
    }

    #[test]
    fn placeholder_window_label_detection() {
        let wid = "window:01a217b3-08c9-7d90-a0db-92f2e340e9f2";
        assert!(is_placeholder_window_label("Window e340e9f2", Some(wid)));
        assert!(is_placeholder_window_label("Window e340e9f2", None));
        // Suffix of a different window, user-chosen names, and near misses are real labels.
        assert!(!is_placeholder_window_label("Window 481bac87", Some(wid)));
        assert!(!is_placeholder_window_label("Window build", Some(wid)));
        assert!(!is_placeholder_window_label("Window e340e9f", Some(wid)));
        assert!(!is_placeholder_window_label(
            "Fix Omarex File Drops",
            Some(wid)
        ));
        assert!(!is_placeholder_window_label("", Some(wid)));
    }

    #[test]
    fn friendly_terminal_title_shortens_shell_prompts_only() {
        assert_eq!(friendly_terminal_title("user@omarchy:~"), "~");
        assert_eq!(
            friendly_terminal_title("user@omarchy:~/projects/omarex"),
            "omarex"
        );
        assert_eq!(friendly_terminal_title("user@box:/etc/"), "etc");
        assert_eq!(friendly_terminal_title("root@box:/"), "/");
        assert_eq!(
            friendly_terminal_title("  Laufende Fixes in Oma Rex "),
            "Laufende Fixes in Oma Rex"
        );
        assert_eq!(friendly_terminal_title("nvim: main.rs"), "nvim: main.rs");
        assert_eq!(
            friendly_terminal_title("Mail an bob@example.com: Re"),
            "Mail an bob@example.com: Re"
        );
    }

    #[test]
    fn unlabeled_window_tab_is_named_from_terminal_content() {
        let wid = "window:01a217b3-08c9-7d90-a0db-92f2e340e9f2";
        let bid = "block:01a217b3-08c8-7a10-9208-dfb8060f2845";
        let card_for = |label: &str, ctx: BlockCardContext| {
            let ctxs = HashMap::from([(bid.to_owned(), ctx)]);
            build_window_tab_card(
                &single_block_window(wid, label, bid),
                Some(wid),
                false,
                &ctxs,
            )
            .card
            .title
        };

        // Terminal title wins over the server placeholder.
        let titled = BlockCardContext {
            title: Some("Laufende Fixes in Oma Rex".into()),
            process_name: Some("claude".into()),
            ..Default::default()
        };
        assert_eq!(
            card_for("Window e340e9f2", titled.clone()),
            "Laufende Fixes in Oma Rex"
        );
        assert_eq!(card_for("", titled.clone()), "Laufende Fixes in Oma Rex");
        // An explicit (user or agent) window label always wins.
        assert_eq!(card_for("Build", titled), "Build");

        // Bash prompt title is shortened to the directory.
        let bash = BlockCardContext {
            title: Some("user@omarchy:~/projects/omarex".into()),
            process_name: Some("bash".into()),
            ..Default::default()
        };
        assert_eq!(card_for("Window e340e9f2", bash), "omarex");

        // No title yet: a real program name, else the project directory, never the placeholder.
        let nvim = BlockCardContext {
            process_name: Some("nvim".into()),
            project_or_cwd: Some("omarex".into()),
            ..Default::default()
        };
        assert_eq!(card_for("Window e340e9f2", nvim), "nvim");
        let idle_shell = BlockCardContext {
            process_name: Some("bash".into()),
            project_or_cwd: Some("omarex".into()),
            ..Default::default()
        };
        assert_eq!(card_for("Window e340e9f2", idle_shell), "omarex");
        assert_eq!(
            card_for("Window e340e9f2", BlockCardContext::default()),
            "Shell"
        );
    }
}
