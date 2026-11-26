//! Native `rex.ui` (`client_prompt_requested` / `client_prompt_ended`) prompt state
//! machine producing unordered Rex API rev 24 `client.reply` payloads.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};

/// All 16 official `rex.ui.pick` icon names (`docs/protocol-spec.md §6.3`),
/// plus graceful handling for unknown future names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PromptIcon {
    Document,
    Folder,
    Branch,
    Terminal,
    Code,
    Server,
    Database,
    Package,
    Globe,
    Link,
    Person,
    Tag,
    Clock,
    Star,
    Gear,
    Plus,
    Unknown(String),
}

impl PromptIcon {
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "document" => Self::Document,
            "folder" => Self::Folder,
            "branch" => Self::Branch,
            "terminal" => Self::Terminal,
            "code" => Self::Code,
            "server" => Self::Server,
            "database" => Self::Database,
            "package" => Self::Package,
            "globe" => Self::Globe,
            "link" => Self::Link,
            "person" => Self::Person,
            "tag" => Self::Tag,
            "clock" => Self::Clock,
            "star" => Self::Star,
            "gear" => Self::Gear,
            "plus" => Self::Plus,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Document => "document",
            Self::Folder => "folder",
            Self::Branch => "branch",
            Self::Terminal => "terminal",
            Self::Code => "code",
            Self::Server => "server",
            Self::Database => "database",
            Self::Package => "package",
            Self::Globe => "globe",
            Self::Link => "link",
            Self::Person => "person",
            Self::Tag => "tag",
            Self::Clock => "clock",
            Self::Star => "star",
            Self::Gear => "gear",
            Self::Plus => "plus",
            Self::Unknown(s) => s.as_str(),
        }
    }

    /// Returns `true` if this is one of the 16 official Rex UI icons.
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

/// Wire item inside `client_prompt_requested.prompt.items`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptItem {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub selected: bool,
}

impl PromptItem {
    /// Display title (falls back to `id` per Rex spec when `title` is empty).
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        }
    }

    pub fn parsed_icon(&self) -> Option<PromptIcon> {
        self.icon.as_deref().map(PromptIcon::from_name)
    }
}

/// Wire `prompt` object inside `client_prompt_requested`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSpec {
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub allow_other: bool,
    #[serde(default)]
    pub items: Option<Vec<PromptItem>>,
}

/// Wire `client_prompt_requested` server event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequest {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    pub prompt: PromptSpec,
    pub timeout_ms: u64,
}

/// Wire `client_prompt_ended` server event payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptEndedEvent {
    pub call_id: String,
    pub reason: String,
}

/// Wire error body for `client.reply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientReplyError {
    pub code: String,
    pub message: String,
}

/// Exact Rex API rev 24 `client.reply` payload.
///
/// **CRITICAL:** `client.reply` is strictly **unordered** on the Rex control connection
/// so interactive `rex.ui` prompts and `client_call_requested` proxies never deadlock
/// behind an in-flight ordered `client.action` RPC (`docs/protocol-spec.md §3.2`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientReplyPayload {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ClientReplyError>,
}

impl ClientReplyPayload {
    /// Always `true` — callers must dispatch `client.reply` on the unordered lane.
    pub const UNORDERED: bool = true;

    pub const fn is_unordered(&self) -> bool {
        Self::UNORDERED
    }

    pub fn ok_value(call_id: impl Into<String>, result: serde_json::Value) -> Self {
        Self {
            call_id: call_id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn input_text(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::ok_value(call_id, serde_json::json!({ "text": text.into() }))
    }

    pub fn picked_ids(call_id: impl Into<String>, ids: Vec<String>) -> Self {
        Self::ok_value(call_id, serde_json::json!({ "ids": ids }))
    }

    pub fn cancelled(call_id: impl Into<String>) -> Self {
        Self::ok_value(call_id, serde_json::json!({ "cancelled": true }))
    }

    pub fn error(
        call_id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            result: None,
            error: Some(ClientReplyError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    pub fn not_supported(call_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::error(call_id, "not_supported", message)
    }

    pub fn busy(call_id: impl Into<String>) -> Self {
        Self::error(call_id, "busy", "another prompt is already open")
    }

    pub fn refused(call_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::error(call_id, "refused", message)
    }
}

/// Supported prompt presentation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Input,
    Pick,
}

/// Filtered pick row for rendering inside the modal palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredPromptRow {
    pub original_index: usize,
    pub score: u32,
    pub checked: bool,
}

/// Active interactive `rex.ui` modal state.
#[derive(Debug, Clone)]
pub struct PromptSession {
    pub call_id: String,
    pub action: Option<String>,
    pub kind: PromptKind,
    pub title: String,
    pub placeholder: String,
    pub confirm_label: String,
    pub multiple: bool,
    pub allow_other: bool,
    pub input_text: String,
    pub items: Vec<PromptItem>,
    pub checked_ids: HashSet<String>,
    pub focused_row: usize,
    pub deadline: Instant,
    pub timeout_ms: u64,
    filtered: Vec<FilteredPromptRow>,
}

impl PromptSession {
    /// Creates a live [`PromptSession`] from a validated `client_prompt_requested`.
    pub fn from_request(req: PromptRequest, now: Instant) -> Result<Self, ClientReplyPayload> {
        let kind = match req.prompt.kind.as_str() {
            "input" => PromptKind::Input,
            "pick" => PromptKind::Pick,
            unknown => {
                return Err(ClientReplyPayload::not_supported(
                    req.call_id,
                    format!("unsupported prompt kind `{unknown}`"),
                ));
            }
        };

        let items = req.prompt.items.unwrap_or_default();
        let multiple = kind == PromptKind::Pick && req.prompt.multiple;
        // Per Rex spec, allow_other applies only to single-select pick.
        let allow_other = kind == PromptKind::Pick && !multiple && req.prompt.allow_other;

        let mut checked_ids = HashSet::new();
        if multiple {
            for item in &items {
                if item.selected {
                    checked_ids.insert(item.id.clone());
                }
            }
        }

        let input_text = req.prompt.value.unwrap_or_default();
        let placeholder = req.prompt.placeholder.unwrap_or_default();
        let confirm_label = req.prompt.confirm.unwrap_or_else(|| "OK".to_owned());
        let deadline = now + Duration::from_millis(req.timeout_ms);

        let mut session = Self {
            call_id: req.call_id,
            action: req.action,
            kind,
            title: req.prompt.title,
            placeholder,
            confirm_label,
            multiple,
            allow_other,
            input_text,
            items,
            checked_ids,
            focused_row: 0,
            deadline,
            timeout_ms: req.timeout_ms,
            filtered: Vec::new(),
        };
        session.refilter();
        Ok(session)
    }

    /// Remaining milliseconds until server prompt timeout.
    pub fn remaining_ms(&self, now: Instant) -> u64 {
        self.deadline
            .checked_duration_since(now)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    /// Updates input/filter text and re-runs fuzzy filtering for `pick` prompts.
    pub fn set_input_text(&mut self, text: impl Into<String>) {
        self.input_text = text.into();
        if self.kind == PromptKind::Pick {
            self.refilter();
            self.focused_row = 0;
        }
    }

    /// Filtered rows for `pick` mode (in exact server order when filter is empty).
    pub fn filtered_rows(&self) -> &[FilteredPromptRow] {
        &self.filtered
    }

    /// Moves focus down/up within visible rows (including optional `allow_other` row).
    pub fn move_focus(&mut self, delta: isize) {
        let count = self.visible_row_count();
        if count == 0 {
            self.focused_row = 0;
            return;
        }
        let next = (self.focused_row as isize + delta).rem_euclid(count as isize) as usize;
        self.focused_row = next;
    }

    /// Returns `true` when the dedicated `allow_other` custom-text row should be displayed.
    pub fn has_other_row(&self) -> bool {
        self.allow_other && !self.input_text.is_empty()
    }

    pub fn visible_row_count(&self) -> usize {
        self.filtered.len() + usize::from(self.has_other_row())
    }

    /// Toggles checkbox selection of the focused item in `multiple` pick mode (`Space`).
    pub fn toggle_focused_selection(&mut self) -> bool {
        if !self.multiple {
            return false;
        }
        let Some(row) = self.filtered.get_mut(self.focused_row) else {
            return false;
        };
        let id = self.items[row.original_index].id.clone();
        if !self.checked_ids.remove(&id) {
            self.checked_ids.insert(id);
            row.checked = true;
        } else {
            row.checked = false;
        }
        true
    }

    /// Handles `Enter` in the modal:
    /// - `input`: submits exact `input_text` (preserving whitespace / empty string).
    /// - `pick` single-select: submits focused item `ids: [id]` or `allow_other` `text`.
    /// - `pick` multi-select: toggles the focused item (returns `None`); use [`Self::submit_multiple`]
    ///   for `Ctrl+Enter` or clicking the Confirm button!
    pub fn submit_enter(&mut self) -> Option<ClientReplyPayload> {
        match self.kind {
            PromptKind::Input => Some(ClientReplyPayload::input_text(
                self.call_id.clone(),
                self.input_text.clone(),
            )),
            PromptKind::Pick if self.multiple => {
                self.toggle_focused_selection();
                None
            }
            PromptKind::Pick => {
                if let Some(row) = self.filtered.get(self.focused_row) {
                    let id = self.items[row.original_index].id.clone();
                    return Some(ClientReplyPayload::picked_ids(
                        self.call_id.clone(),
                        vec![id],
                    ));
                }
                if self.has_other_row() {
                    return Some(ClientReplyPayload::input_text(
                        self.call_id.clone(),
                        self.input_text.clone(),
                    ));
                }
                None
            }
        }
    }

    /// Handles `Ctrl+Enter` / Confirm button for `multiple = true` pick mode (or submits
    /// `input`/single-select). Returns checked IDs in original server `items` order.
    pub fn submit_confirm(&self) -> Option<ClientReplyPayload> {
        match self.kind {
            PromptKind::Input => Some(ClientReplyPayload::input_text(
                self.call_id.clone(),
                self.input_text.clone(),
            )),
            PromptKind::Pick if self.multiple => {
                let ids = self
                    .items
                    .iter()
                    .filter(|item| self.checked_ids.contains(&item.id))
                    .map(|item| item.id.clone())
                    .collect();
                Some(ClientReplyPayload::picked_ids(self.call_id.clone(), ids))
            }
            PromptKind::Pick => {
                if let Some(row) = self.filtered.get(self.focused_row) {
                    let id = self.items[row.original_index].id.clone();
                    return Some(ClientReplyPayload::picked_ids(
                        self.call_id.clone(),
                        vec![id],
                    ));
                }
                if self.has_other_row() {
                    return Some(ClientReplyPayload::input_text(
                        self.call_id.clone(),
                        self.input_text.clone(),
                    ));
                }
                None
            }
        }
    }

    /// Explicitly submits custom typed text when `allow_other == true`.
    pub fn submit_other_text(&self) -> Option<ClientReplyPayload> {
        if self.allow_other {
            Some(ClientReplyPayload::input_text(
                self.call_id.clone(),
                self.input_text.clone(),
            ))
        } else {
            None
        }
    }

    /// Handles `Esc` / backdrop dismiss, returning `{ "cancelled": true }`.
    pub fn cancel(&self) -> ClientReplyPayload {
        ClientReplyPayload::cancelled(self.call_id.clone())
    }

    fn refilter(&mut self) {
        self.filtered.clear();
        if self.kind != PromptKind::Pick {
            return;
        }

        let query = self.input_text.trim();
        if query.is_empty() {
            for (idx, item) in self.items.iter().enumerate() {
                self.filtered.push(FilteredPromptRow {
                    original_index: idx,
                    score: 0,
                    checked: self.checked_ids.contains(&item.id),
                });
            }
            return;
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored = Vec::new();

        for (idx, item) in self.items.iter().enumerate() {
            let mut haystack = String::with_capacity(
                item.display_title().len()
                    + item.subtitle.as_deref().map_or(0, str::len)
                    + item.detail.as_deref().map_or(0, str::len)
                    + item.id.len()
                    + 4,
            );
            haystack.push_str(item.display_title());
            if let Some(sub) = &item.subtitle {
                haystack.push(' ');
                haystack.push_str(sub);
            }
            if let Some(det) = &item.detail {
                haystack.push(' ');
                haystack.push_str(det);
            }
            if !item.title.is_empty() {
                haystack.push(' ');
                haystack.push_str(&item.id);
            }
            buf.clear();
            if let Some(score) = pattern.score(Utf32Str::new(&haystack, &mut buf), &mut matcher) {
                scored.push(FilteredPromptRow {
                    original_index: idx,
                    score,
                    checked: self.checked_ids.contains(&item.id),
                });
            }
        }

        // Highest fuzzy score first; break ties preserving original server order.
        scored.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.original_index.cmp(&b.original_index))
        });
        self.filtered = scored;
    }
}

/// Outcome of handling an incoming `client_prompt_requested` event.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptOpenOutcome {
    /// Prompt opened in the modal UI; wait for user interaction or `client_prompt_ended`.
    Opened,
    /// Prompt could not be opened (`not_supported` or `busy`); send this reply immediately.
    ImmediateReply(ClientReplyPayload),
}

/// Coordinates at most one active `rex.ui` modal prompt with multi-client dismissal and timeouts.
#[derive(Debug, Default)]
pub struct PromptManager {
    active: Option<PromptSession>,
}

impl PromptManager {
    pub fn new() -> Self {
        Self { active: None }
    }

    pub fn active(&self) -> Option<&PromptSession> {
        self.active.as_ref()
    }

    pub fn active_mut(&mut self) -> Option<&mut PromptSession> {
        self.active.as_mut()
    }

    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    /// Accepts an incoming `client_prompt_requested` event.
    pub fn on_prompt_requested(&mut self, req: PromptRequest, now: Instant) -> PromptOpenOutcome {
        if self.active.is_some() {
            return PromptOpenOutcome::ImmediateReply(ClientReplyPayload::busy(req.call_id));
        }
        match PromptSession::from_request(req, now) {
            Ok(session) => {
                self.active = Some(session);
                PromptOpenOutcome::Opened
            }
            Err(reply) => PromptOpenOutcome::ImmediateReply(reply),
        }
    }

    /// Handles `client_prompt_ended` (another client answered, cancelled, or timed out).
    /// Closes the modal cleanly **without** sending `client.reply`.
    pub fn on_prompt_ended(&mut self, event: &PromptEndedEvent) -> bool {
        if self
            .active
            .as_ref()
            .is_some_and(|s| s.call_id == event.call_id)
        {
            self.active = None;
            true
        } else {
            false
        }
    }

    /// Auto-dismisses the active prompt if `timeout_ms` has elapsed (no reply needed after server timeout).
    pub fn check_timeout(&mut self, now: Instant) -> bool {
        if self.active.as_ref().is_some_and(|s| s.is_expired(now)) {
            self.active = None;
            true
        } else {
            false
        }
    }

    /// Completes the active prompt on `Enter` (closes modal if a reply is produced).
    pub fn submit_enter(&mut self) -> Option<ClientReplyPayload> {
        let reply = self.active.as_mut()?.submit_enter()?;
        self.active = None;
        Some(reply)
    }

    /// Completes the active prompt on `Ctrl+Enter` / Confirm button.
    pub fn submit_confirm(&mut self) -> Option<ClientReplyPayload> {
        let reply = self.active.as_ref()?.submit_confirm()?;
        self.active = None;
        Some(reply)
    }

    /// Cancels the active prompt (`Esc` / backdrop click) and returns the `cancelled` reply.
    pub fn cancel(&mut self) -> Option<ClientReplyPayload> {
        let session = self.active.take()?;
        Some(session.cancel())
    }
}
