//! Embedded vector icon registry for agent providers, `rex.ui.pick` items, and UI chrome.

/// Typed identifier for every bundled 24x24 tint-ready SVG icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconId {
    // Agent & provider marks
    Claude,
    OpenAi,
    Gemini,
    Copilot,
    OpenCode,
    Kimi,
    Hermes,
    T3,
    Terminal,

    // `rex.ui.pick` icons (plus `Terminal` above = all 16 official names)
    Document,
    Folder,
    Branch,
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

    // Pane & chrome controls
    SplitRight,
    SplitDown,
    Float,
    Dock,
    Zoom,
    Close,
    Bell,
    BellOff,
    Search,
    Sidebar,
    Activity,
    ChevronRight,
    ChevronDown,
    Check,
    Warning,
    Error,
}

impl IconId {
    /// All 40 bundled icons in deterministic order (for GPU atlas preloading).
    pub const ALL: &'static [IconId] = &[
        Self::Claude,
        Self::OpenAi,
        Self::Gemini,
        Self::Copilot,
        Self::OpenCode,
        Self::Kimi,
        Self::Hermes,
        Self::T3,
        Self::Terminal,
        Self::Document,
        Self::Folder,
        Self::Branch,
        Self::Code,
        Self::Server,
        Self::Database,
        Self::Package,
        Self::Globe,
        Self::Link,
        Self::Person,
        Self::Tag,
        Self::Clock,
        Self::Star,
        Self::Gear,
        Self::Plus,
        Self::SplitRight,
        Self::SplitDown,
        Self::Float,
        Self::Dock,
        Self::Zoom,
        Self::Close,
        Self::Bell,
        Self::BellOff,
        Self::Search,
        Self::Sidebar,
        Self::Activity,
        Self::ChevronRight,
        Self::ChevronDown,
        Self::Check,
        Self::Warning,
        Self::Error,
    ];

    /// Canonical kebab-case identifier matching the SVG file stem under `assets/icons/`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::Copilot => "copilot",
            Self::OpenCode => "opencode",
            Self::Kimi => "kimi",
            Self::Hermes => "hermes",
            Self::T3 => "t3",
            Self::Terminal => "terminal",
            Self::Document => "document",
            Self::Folder => "folder",
            Self::Branch => "branch",
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
            Self::SplitRight => "split-right",
            Self::SplitDown => "split-down",
            Self::Float => "float",
            Self::Dock => "dock",
            Self::Zoom => "zoom",
            Self::Close => "close",
            Self::Bell => "bell",
            Self::BellOff => "bell-off",
            Self::Search => "search",
            Self::Sidebar => "sidebar",
            Self::Activity => "activity",
            Self::ChevronRight => "chevron-right",
            Self::ChevronDown => "chevron-down",
            Self::Check => "check",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// Raw embedded UTF-8 SVG bytes (24x24 pure `#ffffff` geometry ready for `resvg` tinting).
    #[must_use]
    pub const fn svg_bytes(self) -> &'static [u8] {
        match self {
            Self::Claude => include_bytes!("../../../assets/icons/claude.svg"),
            Self::OpenAi => include_bytes!("../../../assets/icons/openai.svg"),
            Self::Gemini => include_bytes!("../../../assets/icons/gemini.svg"),
            Self::Copilot => include_bytes!("../../../assets/icons/copilot.svg"),
            Self::OpenCode => include_bytes!("../../../assets/icons/opencode.svg"),
            Self::Kimi => include_bytes!("../../../assets/icons/kimi.svg"),
            Self::Hermes => include_bytes!("../../../assets/icons/hermes.svg"),
            Self::T3 => include_bytes!("../../../assets/icons/t3.svg"),
            Self::Terminal => include_bytes!("../../../assets/icons/terminal.svg"),
            Self::Document => include_bytes!("../../../assets/icons/document.svg"),
            Self::Folder => include_bytes!("../../../assets/icons/folder.svg"),
            Self::Branch => include_bytes!("../../../assets/icons/branch.svg"),
            Self::Code => include_bytes!("../../../assets/icons/code.svg"),
            Self::Server => include_bytes!("../../../assets/icons/server.svg"),
            Self::Database => include_bytes!("../../../assets/icons/database.svg"),
            Self::Package => include_bytes!("../../../assets/icons/package.svg"),
            Self::Globe => include_bytes!("../../../assets/icons/globe.svg"),
            Self::Link => include_bytes!("../../../assets/icons/link.svg"),
            Self::Person => include_bytes!("../../../assets/icons/person.svg"),
            Self::Tag => include_bytes!("../../../assets/icons/tag.svg"),
            Self::Clock => include_bytes!("../../../assets/icons/clock.svg"),
            Self::Star => include_bytes!("../../../assets/icons/star.svg"),
            Self::Gear => include_bytes!("../../../assets/icons/gear.svg"),
            Self::Plus => include_bytes!("../../../assets/icons/plus.svg"),
            Self::SplitRight => include_bytes!("../../../assets/icons/split-right.svg"),
            Self::SplitDown => include_bytes!("../../../assets/icons/split-down.svg"),
            Self::Float => include_bytes!("../../../assets/icons/float.svg"),
            Self::Dock => include_bytes!("../../../assets/icons/dock.svg"),
            Self::Zoom => include_bytes!("../../../assets/icons/zoom.svg"),
            Self::Close => include_bytes!("../../../assets/icons/close.svg"),
            Self::Bell => include_bytes!("../../../assets/icons/bell.svg"),
            Self::BellOff => include_bytes!("../../../assets/icons/bell-off.svg"),
            Self::Search => include_bytes!("../../../assets/icons/search.svg"),
            Self::Sidebar => include_bytes!("../../../assets/icons/sidebar.svg"),
            Self::Activity => include_bytes!("../../../assets/icons/activity.svg"),
            Self::ChevronRight => include_bytes!("../../../assets/icons/chevron-right.svg"),
            Self::ChevronDown => include_bytes!("../../../assets/icons/chevron-down.svg"),
            Self::Check => include_bytes!("../../../assets/icons/check.svg"),
            Self::Warning => include_bytes!("../../../assets/icons/warning.svg"),
            Self::Error => include_bytes!("../../../assets/icons/error.svg"),
        }
    }

    /// Look up one of the 16 official `rex.ui.pick` icon names.
    ///
    /// Unknown names return `None` so callers omit the icon per Rex's Custom UI spec.
    #[must_use]
    pub fn from_rex_ui_name(name: &str) -> Option<Self> {
        let normalized = name.trim();
        if normalized.eq_ignore_ascii_case("document") {
            Some(Self::Document)
        } else if normalized.eq_ignore_ascii_case("folder") {
            Some(Self::Folder)
        } else if normalized.eq_ignore_ascii_case("branch") {
            Some(Self::Branch)
        } else if normalized.eq_ignore_ascii_case("terminal") {
            Some(Self::Terminal)
        } else if normalized.eq_ignore_ascii_case("code") {
            Some(Self::Code)
        } else if normalized.eq_ignore_ascii_case("server") {
            Some(Self::Server)
        } else if normalized.eq_ignore_ascii_case("database") {
            Some(Self::Database)
        } else if normalized.eq_ignore_ascii_case("package") {
            Some(Self::Package)
        } else if normalized.eq_ignore_ascii_case("globe") {
            Some(Self::Globe)
        } else if normalized.eq_ignore_ascii_case("link") {
            Some(Self::Link)
        } else if normalized.eq_ignore_ascii_case("person") {
            Some(Self::Person)
        } else if normalized.eq_ignore_ascii_case("tag") {
            Some(Self::Tag)
        } else if normalized.eq_ignore_ascii_case("clock") {
            Some(Self::Clock)
        } else if normalized.eq_ignore_ascii_case("star") {
            Some(Self::Star)
        } else if normalized.eq_ignore_ascii_case("gear") {
            Some(Self::Gear)
        } else if normalized.eq_ignore_ascii_case("plus") {
            Some(Self::Plus)
        } else {
            None
        }
    }

    /// Map an OSC 7501 `app` identifier, T3 provider driver, or shell name to its brand icon.
    #[must_use]
    pub fn from_provider(name: &str) -> Option<Self> {
        let lower = name.trim().to_ascii_lowercase();
        match lower.as_str() {
            "claude" | "claude-code" | "claude_code" | "claudeagent" | "claude_agent"
            | "claude-agent" | "anthropic" => Some(Self::Claude),
            "openai" | "codex" | "codex-cli" | "openai-codex" | "gpt" | "chatgpt" => {
                Some(Self::OpenAi)
            }
            "gemini" | "gemini-cli" | "gemini-agent" | "google" | "antigravity"
            | "antigravity-acp" | "google-antigravity" | "agy" => Some(Self::Gemini),
            "copilot" | "github-copilot" | "gh-copilot" => Some(Self::Copilot),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "kimi" | "kimi-code" | "moonshot" => Some(Self::Kimi),
            "hermes" | "hermes-agent" => Some(Self::Hermes),
            "t3" | "t3code" | "t3-code" | "t3_code" | "tr3x" => Some(Self::T3),
            "terminal" | "shell" | "zsh" | "bash" | "fish" | "sh" | "nu" | "nushell" => {
                Some(Self::Terminal)
            }
            _ => None,
        }
    }

    /// Resolve an optional provider/app name to a brand icon, falling back to `Terminal`.
    #[must_use]
    pub fn for_provider_or_terminal(name: Option<&str>) -> Self {
        name.and_then(Self::from_provider).unwrap_or(Self::Terminal)
    }

    /// Optional brand tile accent color `[r, g, b]` when active tab icons render in color.
    #[must_use]
    pub const fn brand_rgb(self) -> Option<[u8; 3]> {
        match self {
            Self::Claude => Some([0xD9, 0x6B, 0x43]),
            Self::OpenAi => Some([0x10, 0xA3, 0x7F]),
            Self::Gemini => Some([0x5B, 0x8A, 0xF5]),
            Self::Copilot => Some([0x6E, 0x40, 0xC9]),
            Self::OpenCode => Some([0x3A, 0x3A, 0x42]),
            Self::Kimi => Some([0x3B, 0x82, 0xF6]),
            Self::Hermes => Some([0xE2, 0x82, 0x03]),
            Self::T3 => Some([0xE2, 0x82, 0x03]),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IconId;
    use std::collections::HashSet;

    #[test]
    fn all_icons_are_embedded_valid_svgs() {
        assert_eq!(IconId::ALL.len(), 40);
        let mut names = HashSet::new();
        for &id in IconId::ALL {
            assert!(
                names.insert(id.name()),
                "duplicate icon name: {}",
                id.name()
            );
            let svg = std::str::from_utf8(id.svg_bytes()).expect("SVG must be valid UTF-8");
            assert!(svg.contains("<svg"), "{} missing <svg", id.name());
            assert!(
                svg.contains("viewBox=\"0 0 24 24\""),
                "{} missing 24x24 viewBox",
                id.name()
            );
            assert!(svg.contains("</svg>"), "{} missing </svg>", id.name());
        }
    }

    #[test]
    fn rex_ui_pick_covers_all_16_official_names() {
        let official = [
            "document", "folder", "branch", "terminal", "code", "server", "database", "package",
            "globe", "link", "person", "tag", "clock", "star", "gear", "plus",
        ];
        for name in official {
            assert!(
                IconId::from_rex_ui_name(name).is_some(),
                "missing official rex.ui icon {name}"
            );
        }
        assert_eq!(IconId::from_rex_ui_name("  Folder "), Some(IconId::Folder));
        assert_eq!(IconId::from_rex_ui_name("unknown-icon"), None);
    }

    #[test]
    fn provider_aliases_resolve_cleanly() {
        assert_eq!(IconId::from_provider("claude-code"), Some(IconId::Claude));
        assert_eq!(IconId::from_provider("claudeAgent"), Some(IconId::Claude));
        assert_eq!(IconId::from_provider("codex"), Some(IconId::OpenAi));
        assert_eq!(IconId::from_provider("OpenAI"), Some(IconId::OpenAi));
        assert_eq!(IconId::from_provider("gemini"), Some(IconId::Gemini));
        assert_eq!(IconId::from_provider("antigravity"), Some(IconId::Gemini));
        assert_eq!(
            IconId::from_provider("antigravity-acp"),
            Some(IconId::Gemini)
        );
        assert_eq!(IconId::from_provider("copilot"), Some(IconId::Copilot));
        assert_eq!(IconId::from_provider("opencode"), Some(IconId::OpenCode));
        assert_eq!(IconId::from_provider("kimi"), Some(IconId::Kimi));
        assert_eq!(IconId::from_provider("hermes"), Some(IconId::Hermes));
        assert_eq!(IconId::from_provider("t3code"), Some(IconId::T3));
        assert_eq!(IconId::from_provider("zsh"), Some(IconId::Terminal));
        assert_eq!(
            IconId::for_provider_or_terminal(Some("mystery")),
            IconId::Terminal
        );
        assert_eq!(IconId::for_provider_or_terminal(None), IconId::Terminal);
    }
}
