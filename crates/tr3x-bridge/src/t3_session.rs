//! First-class `tr3x` awareness: `T3 Code` session detection, `t3:<env8>/<thread>` tag
//! parsing, provider detection, OSC 7501 status rollup & sanitization, 2-line tab card
//! metadata extraction, and `/pr` floating drawer geometry.

use serde::{Deserialize, Serialize};

/// Parsed `tr3x` block tag (`t3:<env8>/<thread>` or `t3#<hash>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum T3BlockTag {
    EnvThread {
        env_prefix: String,
        thread_id: String,
    },
    Hashed {
        hash: String,
    },
}

/// Returns `true` for merged (`"T3 Code"`) or per-machine (`"T3 Code · <host>"`) `tr3x` sessions.
pub fn is_t3_session(label: &str) -> bool {
    let trimmed = label.trim();
    trimmed == "T3 Code" || trimmed.starts_with("T3 Code · ")
}

/// Extracts the machine suffix from `"T3 Code · <machine>"`, or `None` for merged `"T3 Code"`.
pub fn t3_session_machine(label: &str) -> Option<&str> {
    label
        .trim()
        .strip_prefix("T3 Code · ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Parses a `tr3x` block label (`t3:<env8>/<thread>` or `t3#<hash>`).
pub fn parse_t3_block_tag(label: &str) -> Option<T3BlockTag> {
    let s = label.trim();
    if let Some(rest) = s.strip_prefix("t3:") {
        let (env, thread) = rest.split_once('/')?;
        if !env.is_empty() && !thread.is_empty() {
            return Some(T3BlockTag::EnvThread {
                env_prefix: env.to_owned(),
                thread_id: thread.to_owned(),
            });
        }
        return None;
    }
    if let Some(hash) = s.strip_prefix("t3#")
        && !hash.is_empty()
    {
        return Some(T3BlockTag::Hashed {
            hash: hash.to_owned(),
        });
    }
    None
}

/// Coding agent / program provider mapped to `assets/icons/*.svg` and brand accents (`gui-spec.md §2.5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    Claude,
    OpenAi,
    Gemini,
    Copilot,
    OpenCode,
    Kimi,
    Hermes,
    Cursor,
    Pi,
    Grok,
    Fx,
    Muse,
    Antigravity,
    T3Code,
    Shell,
    Other,
}

impl ProviderKind {
    /// Detects provider from OSC 7501 `app` field first, then foreground process name/argv0.
    pub fn detect(osc_app: Option<&str>, process_name: Option<&str>) -> Self {
        if let Some(app) = osc_app
            && let Some(kind) = classify_provider_token(app)
        {
            return kind;
        }
        if let Some(proc_name) = process_name
            && let Some(kind) = classify_provider_token(proc_name)
        {
            return kind;
        }
        Self::Other
    }

    /// Icon asset name matching `assets/icons/<name>.svg`.
    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::Gemini | Self::Antigravity => "gemini",
            Self::Copilot => "copilot",
            Self::OpenCode => "opencode",
            Self::Kimi => "kimi",
            Self::Hermes => "hermes",
            Self::T3Code => "t3",
            Self::Cursor | Self::Pi | Self::Grok | Self::Fx | Self::Muse => "t3",
            Self::Shell | Self::Other => "terminal",
        }
    }

    /// Brand accent hex for active color mode (`docs/gui-spec.md §2.5`).
    pub const fn brand_hex(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("#d96b43"),
            Self::OpenAi => Some("#10a37f"),
            Self::Gemini | Self::Antigravity => Some("#4e8df5"),
            Self::Copilot => Some("#6e40c9"),
            Self::OpenCode => Some("#f5f5f5"),
            Self::Kimi => Some("#3b82f6"),
            Self::Hermes => Some("#e28203"),
            Self::T3Code | Self::Pi => Some("#e28203"),
            Self::Cursor | Self::Grok | Self::Fx | Self::Muse => Some("#ffffff"),
            Self::Shell | Self::Other => None,
        }
    }
}

fn classify_provider_token(raw: &str) -> Option<ProviderKind> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lower.contains("claude") || lower.contains("anthropic") {
        Some(ProviderKind::Claude)
    } else if lower.contains("codex") || lower.contains("openai") || lower.contains("chatgpt") {
        Some(ProviderKind::OpenAi)
    } else if lower.contains("antigravity") || lower == "agy" || lower.ends_with("/agy") {
        Some(ProviderKind::Antigravity)
    } else if lower.contains("gemini") {
        Some(ProviderKind::Gemini)
    } else if lower.contains("copilot") {
        Some(ProviderKind::Copilot)
    } else if lower.contains("opencode") {
        Some(ProviderKind::OpenCode)
    } else if lower.contains("kimi") {
        Some(ProviderKind::Kimi)
    } else if lower.contains("hermes") {
        Some(ProviderKind::Hermes)
    } else if lower.contains("cursor") {
        Some(ProviderKind::Cursor)
    } else if lower == "pi" || lower.ends_with("/pi") {
        Some(ProviderKind::Pi)
    } else if lower.contains("grok") {
        Some(ProviderKind::Grok)
    } else if lower == "fx" || lower.ends_with("/fx") {
        Some(ProviderKind::Fx)
    } else if lower.contains("muse") {
        Some(ProviderKind::Muse)
    } else if lower.contains("t3code") || lower.contains("tr3x") || lower == "t3" {
        Some(ProviderKind::T3Code)
    } else if matches!(
        lower.rsplit('/').next().unwrap_or(&lower),
        "zsh" | "bash" | "fish" | "nu" | "sh" | "dash" | "tcsh"
    ) {
        Some(ProviderKind::Shell)
    } else {
        None
    }
}

/// OSC 7501 program state ordered by UI attention urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgramState {
    Blocked,
    Error,
    Waiting,
    Working,
    Done,
    Idle,
}

impl ProgramState {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "blocked" => Self::Blocked,
            "error" | "failed" => Self::Error,
            "waiting" => Self::Waiting,
            "working" => Self::Working,
            "done" => Self::Done,
            _ => Self::Idle,
        }
    }

    /// Strict rollup priority:
    /// `blocked` (5) > `error` (4) > `waiting` (3) > `working` (2) > `done` (1) > `idle` (0).
    /// When `unseen_done_boost` is set (for attention dots), unseen `done` ranks above `working`.
    pub const fn priority_rank(self, unseen: bool) -> u8 {
        match self {
            Self::Blocked => 6,
            Self::Error => 5,
            Self::Waiting => 4,
            Self::Done if unseen => 3,
            Self::Working => 2,
            Self::Done => 1,
            Self::Idle => 0,
        }
    }

    /// Short human-readable label for 2-line sidebar subtitles (`pulse · Needs you`).
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Blocked => "Needs you",
            Self::Error => "Error",
            Self::Waiting => "Waiting",
            Self::Working => "Working",
            Self::Done => "Done",
            Self::Idle => "Idle",
        }
    }
}

/// Optional `kind` qualifier on OSC 7501 `blocked` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
    Other,
}

impl BlockedKind {
    pub fn parse(raw: &str) -> Self {
        Self::parse_opt(raw).unwrap_or(Self::Other)
    }

    /// Parses known Rex rev 24 OSC 7501 `kind` values; unknown values return `None`
    /// per the Rex specification ("treat a value you don't know as if it were left out").
    pub fn parse_opt(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "permission" => Some(Self::Permission),
            "question" => Some(Self::Question),
            "auth" => Some(Self::Auth),
            _ => None,
        }
    }
}

/// Single OSC 7501 program status record reported for a block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramStatusRecord {
    #[serde(default)]
    pub id: String,
    pub state: ProgramState,
    #[serde(default)]
    pub kind: Option<BlockedKind>,
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub app: Option<String>,
    /// Inherited program app from the nearest parent OSC 7501 record when `app` is unset.
    #[serde(default)]
    pub effective_app: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub unseen: bool,
}

impl ProgramStatusRecord {
    /// Returns the effective app name (`effective_app` if populated, else `app`).
    pub fn resolved_app(&self) -> Option<&str> {
        self.effective_app
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.app.as_deref().filter(|s| !s.trim().is_empty()))
    }
}

/// Aggregated OSC 7501 status for a pane, tab, or session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledUpStatus {
    pub state: ProgramState,
    pub kind: Option<BlockedKind>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
    pub unseen: bool,
}

/// Sanitizes untrusted OSC 7501 `title`/`msg` fields per `docs/gui-spec.md §2.4`:
/// removes ASCII control chars and Unicode BiDi override/isolate characters
/// (`U+202A..=U+202E`, `U+2066..=U+2069`).
pub fn sanitize_status_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let code = ch as u32;
        if (0x202A..=0x202E).contains(&code) || (0x2066..=0x2069).contains(&code) {
            continue;
        }
        if ch.is_control() {
            continue;
        }
        out.push(ch);
    }
    out.trim().to_owned()
}

/// Rolls up up to 256 OSC 7501 records into the highest-priority status for chrome display.
pub fn rollup_records(records: &[ProgramStatusRecord]) -> Option<RolledUpStatus> {
    let best = records
        .iter()
        .max_by_key(|r| r.state.priority_rank(r.unseen))?;
    Some(RolledUpStatus {
        state: best.state,
        kind: best.kind,
        progress: best.progress.map(|p| p.min(100)),
        app: best.resolved_app().map(ToOwned::to_owned),
        title: best
            .title
            .as_deref()
            .map(sanitize_status_text)
            .filter(|s| !s.is_empty()),
        msg: best
            .msg
            .as_deref()
            .map(sanitize_status_text)
            .filter(|s| !s.is_empty()),
        unseen: best.unseen,
    })
}

/// Attention dot style overlaid on the top-right corner of the `26×26px` sidebar icon tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityDot {
    None,
    Blocked,
    Error,
    DoneUnseen,
}

/// Right-hand working indicator on a sidebar tab card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingIndicator {
    None,
    PulseDots,
    ProgressPie(u8),
}

/// Fully prepared 2-line sidebar tab card metadata (`docs/gui-spec.md §2.2 B` & `§4.1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabCardMetadata {
    pub title: String,
    pub subtitle: String,
    pub is_subagent: bool,
    pub provider: ProviderKind,
    pub state: ProgramState,
    pub priority_dot: PriorityDot,
    pub working_indicator: WorkingIndicator,
    pub relative_age: Option<String>,
}

/// Formats an age in seconds as compact Rex tabular timestamp (`now`, `2m`, `12m`, `1h`, `2d`).
pub fn format_relative_age(age_secs: u64) -> String {
    if age_secs < 60 {
        "now".to_owned()
    } else if age_secs < 3_600 {
        format!("{}m", age_secs / 60)
    } else if age_secs < 86_400 {
        format!("{}h", age_secs / 3_600)
    } else {
        format!("{}d", age_secs / 86_400)
    }
}

/// Builds 2-line sidebar tab card metadata from a Rex window/block snapshot + OSC 7501 status.
pub fn build_tab_card(
    raw_title: &str,
    project_or_cwd: Option<&str>,
    process_name: Option<&str>,
    status: Option<&RolledUpStatus>,
    age_secs: Option<u64>,
) -> TabCardMetadata {
    let clean_title = sanitize_status_text(raw_title);
    let is_subagent = clean_title.starts_with("↳");
    let state = status.map(|s| s.state).unwrap_or(ProgramState::Idle);
    let provider = ProviderKind::detect(status.and_then(|s| s.app.as_deref()), process_name);

    let priority_dot = match status {
        Some(s) if s.state == ProgramState::Blocked => PriorityDot::Blocked,
        Some(s) if s.state == ProgramState::Error => PriorityDot::Error,
        Some(s) if s.state == ProgramState::Done && s.unseen => PriorityDot::DoneUnseen,
        _ => PriorityDot::None,
    };

    let working_indicator = match status {
        Some(s) if s.state == ProgramState::Working => match s.progress {
            Some(pct) => WorkingIndicator::ProgressPie(pct.min(100)),
            None => WorkingIndicator::PulseDots,
        },
        Some(s) if s.state == ProgramState::Blocked => match s.progress {
            Some(pct) => WorkingIndicator::ProgressPie(pct.min(100)),
            None => WorkingIndicator::None,
        },
        _ => WorkingIndicator::None,
    };

    let subtitle = if let Some(msg) = status.and_then(|s| s.msg.as_deref()) {
        if let Some(proj) = project_or_cwd
            .map(sanitize_status_text)
            .filter(|s| !s.is_empty())
            && !msg.contains('·')
        {
            format!("{proj} · {msg}")
        } else {
            msg.to_owned()
        }
    } else {
        let proj = project_or_cwd.map(sanitize_status_text).unwrap_or_default();
        match (proj.is_empty(), state) {
            (false, ProgramState::Idle) => proj,
            (false, st) => format!("{proj} · {}", st.display_label()),
            (true, ProgramState::Idle) => String::new(),
            (true, st) => st.display_label().to_owned(),
        }
    };

    TabCardMetadata {
        title: clean_title,
        subtitle,
        is_subagent,
        provider,
        state,
        priority_dot,
        working_indicator,
        relative_age: age_secs.map(format_relative_age),
    }
}

/// Normalized `[x, y, w, h]` floating layer rectangle (`0.0..=1.0`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatingRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl FloatingRect {
    pub const fn to_array(self) -> [f32; 4] {
        [self.x, self.y, self.w, self.h]
    }
}

/// Column threshold where `tr3x` switches `/pr` from centered overlay to full-height right drawer.
pub const PR_DRAWER_WIDE_COLS: u16 = 140;

/// Computes exact `tr3x` `/pr` floating layer bounds (`docs/gui-spec.md §4.4`):
/// - Wide (`>= 140` cols): right-side drawer `[0.45, 0.0, 0.55, 1.0]`
/// - Narrow (`< 140` cols): centered overlay `[0.03, 0.03, 0.94, 0.94]`
pub const fn pr_drawer_rect(viewport_cols: u16) -> FloatingRect {
    if viewport_cols >= PR_DRAWER_WIDE_COLS {
        FloatingRect {
            x: 0.45,
            y: 0.0,
            w: 0.55,
            h: 1.0,
        }
    } else {
        FloatingRect {
            x: 0.03,
            y: 0.03,
            w: 0.94,
            h: 0.94,
        }
    }
}

/// Returns `true` when a floating layer label represents a `tr3x` Pull Request drawer.
pub fn is_pr_layer(label: &str) -> bool {
    label.trim().eq_ignore_ascii_case("pull request")
}

#[cfg(test)]
mod tests {
    use super::ProviderKind;

    #[test]
    fn antigravity_maps_to_gemini_icon_and_brand_accent() {
        let kind = ProviderKind::detect(Some("antigravity"), Some("tr3x"));
        assert_eq!(kind, ProviderKind::Antigravity);
        assert_eq!(kind.icon_name(), "gemini");
        assert_eq!(kind.brand_hex(), Some("#4e8df5"));
    }
}
