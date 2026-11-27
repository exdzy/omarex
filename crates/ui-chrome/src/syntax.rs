//! Zero-dependency stateful multi-line / multi-token syntax highlighter for the viewer pane,
//! mapped directly to the active Omarchy [`Theme`] palette (`terminal.ansi` + [`UiTokens`]).

use omarchy_theme::{Rgba, Theme, UiTokens};

use crate::draw::TextSpan;

/// Supported languages for multi-token syntax highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxLanguage {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    C,
    Cpp,
    Json,
    Toml,
    Yaml,
    Shell,
    Sql,
    Diff,
    Markdown,
    Plain,
}

impl SyntaxLanguage {
    /// Resolves a file extension or Markdown fenced code language tag (case-insensitive).
    #[must_use]
    pub fn from_hint(hint: &str) -> Self {
        let norm = hint.trim().to_ascii_lowercase();
        let norm = norm.split([',', ' ', '{']).next().unwrap_or(norm.as_str());
        match norm {
            "rs" | "rust" => Self::Rust,
            "ts" | "tsx" | "typescript" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" | "javascript" => Self::JavaScript,
            "py" | "pyi" | "python" | "python3" => Self::Python,
            "go" | "golang" => Self::Go,
            "c" | "h" => Self::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "c++" => Self::Cpp,
            "json" | "jsonc" | "jsonl" | "ndjson" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "sh" | "bash" | "zsh" | "fish" | "shell" | "console" | "dockerfile" | "makefile"
            | "mk" | "nix" | "ini" | "conf" | "cfg" | "env" => Self::Shell,
            "sql" | "postgres" | "sqlite" => Self::Sql,
            "diff" | "patch" => Self::Diff,
            "md" | "mdx" | "markdown" => Self::Markdown,
            // Common C-like/system languages mapped to nearest rich grammar
            "wgsl" | "glsl" | "vert" | "frag" | "zig" | "proto" => Self::Rust,
            "java" | "kt" | "kts" | "swift" | "css" | "scss" | "less" | "graphql" => {
                Self::TypeScript
            }
            "rb" | "ruby" | "lua" | "php" => Self::Python,
            _ => Self::Plain,
        }
    }
}

/// Semantic token class emitted by the syntax lexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Plain,
    Keyword,
    ControlFlow,
    Type,
    String,
    Number,
    Constant,
    Comment,
    Attribute,
    Macro,
    Function,
    Property,
    Operator,
    Punctuation,
    DiffAdded,
    DiffRemoved,
    DiffHunk,
}

/// Single byte-range token within a tab-expanded source line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxToken {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// Persistent multi-line lexer state carried across consecutive source lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LexLineState {
    pub block_comment_depth: u8,
    pub triple_quote: Option<u8>,
    pub raw_string_hashes: Option<usize>,
}

/// Expands `\t` tabs to 4 ASCII spaces so monospace column layout, selection, and token byte
/// offsets match 1:1 with zero drift.
#[must_use]
pub fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_owned();
    }
    let mut out = String::with_capacity(line.len() + 8);
    for ch in line.chars() {
        if ch == '\t' {
            out.push_str("    ");
        } else {
            out.push(ch);
        }
    }
    out
}

/// Tokenizes all lines in a document, propagating multi-line comments, Python triple-quoted
/// strings, and Rust raw strings cleanly. Token byte ranges refer to [`expand_tabs(line)`].
#[must_use]
pub fn tokenize_lines(lines: &[String], lang: SyntaxLanguage) -> Vec<Vec<SyntaxToken>> {
    let mut state = LexLineState::default();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let expanded = expand_tabs(line);
        let tokens = tokenize_single_line(&expanded, lang, &mut state);
        out.push(tokens);
    }
    out
}

/// Tokenizes one tab-expanded line and updates `state` for the next line.
pub fn tokenize_single_line(
    line: &str,
    lang: SyntaxLanguage,
    state: &mut LexLineState,
) -> Vec<SyntaxToken> {
    if line.is_empty() {
        return Vec::new();
    }
    if lang == SyntaxLanguage::Plain {
        return vec![SyntaxToken {
            start: 0,
            end: line.len(),
            kind: TokenKind::Plain,
        }];
    }
    if lang == SyntaxLanguage::Diff {
        return tokenize_diff_line(line);
    }
    if lang == SyntaxLanguage::Toml {
        return tokenize_toml_line(line);
    }
    if lang == SyntaxLanguage::Yaml {
        return tokenize_yaml_line(line);
    }

    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0usize;

    // 1. Continue open multi-line construct from previous line.
    if state.block_comment_depth > 0 {
        let start = 0;
        let allow_nested = lang == SyntaxLanguage::Rust;
        while i < len {
            if allow_nested && bytes[i..].starts_with(b"/*") {
                state.block_comment_depth = state.block_comment_depth.saturating_add(1);
                i += 2;
            } else if bytes[i..].starts_with(b"*/") {
                state.block_comment_depth -= 1;
                i += 2;
                if state.block_comment_depth == 0 {
                    break;
                }
            } else {
                i += char_len_at(line, i);
            }
        }
        push_token(&mut tokens, start, i, TokenKind::Comment);
    } else if let Some(q) = state.triple_quote {
        let delim = [q, q, q];
        let start = 0;
        while i < len {
            if bytes[i] == b'\\' && i + 1 < len {
                i += 1 + char_len_at(line, i + 1);
            } else if bytes[i..].starts_with(&delim) {
                i += 3;
                state.triple_quote = None;
                break;
            } else {
                i += char_len_at(line, i);
            }
        }
        push_token(&mut tokens, start, i, TokenKind::String);
    } else if let Some(hashes) = state.raw_string_hashes {
        let start = 0;
        while i < len {
            if bytes[i] == b'"' && count_hashes(&bytes[i + 1..]) >= hashes {
                i += 1 + hashes;
                state.raw_string_hashes = None;
                break;
            }
            i += char_len_at(line, i);
        }
        push_token(&mut tokens, start, i, TokenKind::String);
    }

    let mut after_member_dot = false;

    while i < len {
        let b = bytes[i];

        // Whitespace
        if b.is_ascii_whitespace() {
            let start = i;
            while i < len && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            push_token(&mut tokens, start, i, TokenKind::Plain);
            continue;
        }

        // Line comments
        if (supports_slash_comments(lang) && bytes[i..].starts_with(b"//"))
            || (supports_hash_comments(lang) && b == b'#')
            || (lang == SyntaxLanguage::Sql && bytes[i..].starts_with(b"--"))
        {
            push_token(&mut tokens, i, len, TokenKind::Comment);
            break;
        }

        // Block comments `/* ... */`
        if supports_block_comments(lang) && bytes[i..].starts_with(b"/*") {
            let start = i;
            i += 2;
            state.block_comment_depth = 1;
            let allow_nested = lang == SyntaxLanguage::Rust;
            while i < len {
                if allow_nested && bytes[i..].starts_with(b"/*") {
                    state.block_comment_depth = state.block_comment_depth.saturating_add(1);
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    state.block_comment_depth -= 1;
                    i += 2;
                    if state.block_comment_depth == 0 {
                        break;
                    }
                } else {
                    i += char_len_at(line, i);
                }
            }
            push_token(&mut tokens, start, i, TokenKind::Comment);
            after_member_dot = false;
            continue;
        }

        // Rust attributes `#[...]` or `#![...]`
        if lang == SyntaxLanguage::Rust
            && (bytes[i..].starts_with(b"#[") || bytes[i..].starts_with(b"#!["))
        {
            let start = i;
            i += if bytes[i + 1] == b'!' { 3 } else { 2 };
            let mut depth = 1usize;
            while i < len && depth > 0 {
                match bytes[i] {
                    b'"' => {
                        i = scan_quoted(line, i, b'"');
                    }
                    b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b']' => {
                        depth -= 1;
                        i += 1;
                    }
                    _ => i += char_len_at(line, i),
                }
            }
            push_token(&mut tokens, start, i, TokenKind::Attribute);
            after_member_dot = false;
            continue;
        }

        // C/C++ preprocessor `#include`, `#define`, etc.
        if matches!(lang, SyntaxLanguage::C | SyntaxLanguage::Cpp) && b == b'#' {
            let start = i;
            i += 1;
            while i < len && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            push_token(&mut tokens, start, i, TokenKind::Attribute);
            after_member_dot = false;
            continue;
        }

        // Python / TS decorators `@ident`
        if matches!(
            lang,
            SyntaxLanguage::Python | SyntaxLanguage::TypeScript | SyntaxLanguage::JavaScript
        ) && b == b'@'
        {
            let start = i;
            i += 1;
            while i < len
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
            {
                i += 1;
            }
            push_token(&mut tokens, start, i, TokenKind::Attribute);
            after_member_dot = false;
            continue;
        }

        // Python triple-quoted strings `"""..."""` / `'''...'''`
        if lang == SyntaxLanguage::Python
            && (bytes[i..].starts_with(b"\"\"\"") || bytes[i..].starts_with(b"'''"))
        {
            let q = b;
            let delim = [q, q, q];
            let start = i;
            i += 3;
            state.triple_quote = Some(q);
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1 + char_len_at(line, i + 1);
                } else if bytes[i..].starts_with(&delim) {
                    i += 3;
                    state.triple_quote = None;
                    break;
                } else {
                    i += char_len_at(line, i);
                }
            }
            push_token(&mut tokens, start, i, TokenKind::String);
            after_member_dot = false;
            continue;
        }

        // Rust raw strings `r#"..."#` / `br#"..."#` / `r"..."`
        if lang == SyntaxLanguage::Rust
            && let Some((raw_end, open_hashes)) = match_rust_raw_string_start(bytes, i)
        {
            let start = i;
            i = raw_end;
            state.raw_string_hashes = Some(open_hashes);
            while i < len {
                if bytes[i] == b'"' && count_hashes(&bytes[i + 1..]) >= open_hashes {
                    i += 1 + open_hashes;
                    state.raw_string_hashes = None;
                    break;
                }
                i += char_len_at(line, i);
            }
            push_token(&mut tokens, start, i, TokenKind::String);
            after_member_dot = false;
            continue;
        }

        // Double-quoted strings (plus JSON property key lookahead)
        if b == b'"' {
            let start = i;
            let end = scan_quoted(line, i, b'"');
            i = end;
            let kind = if lang == SyntaxLanguage::Json && next_non_ws_is(bytes, i, b':') {
                TokenKind::Property
            } else {
                TokenKind::String
            };
            push_token(&mut tokens, start, i, kind);
            after_member_dot = false;
            continue;
        }

        // Single-quoted strings / Rust char literals vs lifetimes (`'a`, `'static`)
        if b == b'\'' {
            if lang == SyntaxLanguage::Rust {
                if let Some(char_end) = scan_rust_char_literal(line, i) {
                    push_token(&mut tokens, i, char_end, TokenKind::String);
                    i = char_end;
                    after_member_dot = false;
                    continue;
                }
                if i + 1 < len && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_') {
                    let start = i;
                    i += 1;
                    while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                        i += 1;
                    }
                    push_token(&mut tokens, start, i, TokenKind::Type);
                    after_member_dot = false;
                    continue;
                }
            }
            let start = i;
            let end = scan_quoted(line, i, b'\'');
            push_token(&mut tokens, start, end, TokenKind::String);
            i = end;
            after_member_dot = false;
            continue;
        }

        // JS/TS/Shell/SQL backtick strings
        if b == b'`'
            && matches!(
                lang,
                SyntaxLanguage::JavaScript
                    | SyntaxLanguage::TypeScript
                    | SyntaxLanguage::Shell
                    | SyntaxLanguage::Go
                    | SyntaxLanguage::Sql
            )
        {
            let start = i;
            let end = scan_quoted(line, i, b'`');
            push_token(&mut tokens, start, end, TokenKind::String);
            i = end;
            after_member_dot = false;
            continue;
        }

        // Numbers (hex, bin, oct, float, exponent, suffixes)
        if b.is_ascii_digit()
            || (b == b'.' && i + 1 < len && bytes[i + 1].is_ascii_digit() && !after_member_dot)
        {
            let start = i;
            i = scan_number(bytes, i);
            push_token(&mut tokens, start, i, TokenKind::Number);
            after_member_dot = false;
            continue;
        }

        // Identifiers, keywords, types, macros, functions, properties, constants
        if is_ident_start(line, i) {
            let start = i;
            i += char_len_at(line, i);
            while i < len && is_ident_continue(line, i) {
                i += char_len_at(line, i);
            }
            let ident = &line[start..i];
            let kind = classify_ident(
                ident,
                lang,
                bytes,
                i,
                after_member_dot,
                &mut i, // advances if macro `!` is consumed
            );
            push_token(&mut tokens, start, i, kind);
            after_member_dot = false;
            continue;
        }

        // Multi-char operators
        if let Some(op_len) = match_multi_char_op(&bytes[i..]) {
            push_token(&mut tokens, i, i + op_len, TokenKind::Operator);
            after_member_dot = &bytes[i..i + op_len] == b"->";
            i += op_len;
            continue;
        }

        // Single-char operators vs punctuation
        let ch_len = char_len_at(line, i);
        if b == b'.' {
            push_token(&mut tokens, i, i + 1, TokenKind::Punctuation);
            after_member_dot = true;
        } else if matches!(
            b,
            b'+' | b'-'
                | b'*'
                | b'/'
                | b'%'
                | b'='
                | b'<'
                | b'>'
                | b'!'
                | b'&'
                | b'|'
                | b'^'
                | b'~'
                | b'?'
        ) {
            push_token(&mut tokens, i, i + ch_len, TokenKind::Operator);
            after_member_dot = false;
        } else if matches!(
            b,
            b'(' | b')' | b'{' | b'}' | b'[' | b']' | b',' | b';' | b':'
        ) {
            push_token(&mut tokens, i, i + ch_len, TokenKind::Punctuation);
            after_member_dot = false;
        } else {
            push_token(&mut tokens, i, i + ch_len, TokenKind::Plain);
            after_member_dot = false;
        }
        i += ch_len;
    }

    tokens
}

fn tokenize_diff_line(line: &str) -> Vec<SyntaxToken> {
    let kind = if line.starts_with("@@") {
        TokenKind::DiffHunk
    } else if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
        TokenKind::Keyword
    } else if line.starts_with('+') {
        TokenKind::DiffAdded
    } else if line.starts_with('-') {
        TokenKind::DiffRemoved
    } else {
        TokenKind::Plain
    };
    vec![SyntaxToken {
        start: 0,
        end: line.len(),
        kind,
    }]
}

fn tokenize_toml_line(line: &str) -> Vec<SyntaxToken> {
    let trimmed = line.trim_start();
    let ws = line.len() - trimmed.len();
    let mut tokens = Vec::new();
    if ws > 0 {
        push_token(&mut tokens, 0, ws, TokenKind::Plain);
    }
    if trimmed.starts_with('#') {
        push_token(&mut tokens, ws, line.len(), TokenKind::Comment);
        return tokens;
    }
    if trimmed.starts_with('[') {
        if let Some(comment_pos) = find_unquoted_hash(line, ws) {
            push_token(&mut tokens, ws, comment_pos, TokenKind::Attribute);
            push_token(&mut tokens, comment_pos, line.len(), TokenKind::Comment);
        } else {
            push_token(&mut tokens, ws, line.len(), TokenKind::Attribute);
        }
        return tokens;
    }
    if let Some(eq_rel) = trimmed.find('=') {
        let eq_pos = ws + eq_rel;
        push_token(&mut tokens, ws, eq_pos, TokenKind::Property);
        push_token(&mut tokens, eq_pos, eq_pos + 1, TokenKind::Operator);
        let mut st = LexLineState::default();
        let rhs_tokens = tokenize_single_line(&line[eq_pos + 1..], SyntaxLanguage::Json, &mut st);
        for t in rhs_tokens {
            push_token(
                &mut tokens,
                eq_pos + 1 + t.start,
                eq_pos + 1 + t.end,
                t.kind,
            );
        }
        return tokens;
    }
    let mut st = LexLineState::default();
    tokenize_single_line(line, SyntaxLanguage::Shell, &mut st)
}

fn tokenize_yaml_line(line: &str) -> Vec<SyntaxToken> {
    let trimmed = line.trim_start();
    let ws = line.len() - trimmed.len();
    let mut tokens = Vec::new();
    if ws > 0 {
        push_token(&mut tokens, 0, ws, TokenKind::Plain);
    }
    if trimmed.starts_with('#') {
        push_token(&mut tokens, ws, line.len(), TokenKind::Comment);
        return tokens;
    }
    if trimmed == "---" || trimmed == "..." {
        push_token(&mut tokens, ws, line.len(), TokenKind::Punctuation);
        return tokens;
    }
    let key_start = if let Some(after_dash) = trimmed.strip_prefix("- ") {
        push_token(&mut tokens, ws, ws + 2, TokenKind::Punctuation);
        line.len() - after_dash.len()
    } else {
        ws
    };
    if let Some(colon_rel) = line[key_start..].find(':') {
        let colon_pos = key_start + colon_rel;
        let after_colon = line.as_bytes().get(colon_pos + 1).copied();
        if after_colon.is_none() || after_colon.is_some_and(|b| b.is_ascii_whitespace()) {
            push_token(&mut tokens, key_start, colon_pos, TokenKind::Property);
            push_token(
                &mut tokens,
                colon_pos,
                colon_pos + 1,
                TokenKind::Punctuation,
            );
            let mut st = LexLineState::default();
            for t in tokenize_single_line(&line[colon_pos + 1..], SyntaxLanguage::Shell, &mut st) {
                push_token(
                    &mut tokens,
                    colon_pos + 1 + t.start,
                    colon_pos + 1 + t.end,
                    t.kind,
                );
            }
            return tokens;
        }
    }
    let mut st = LexLineState::default();
    tokenize_single_line(line, SyntaxLanguage::Shell, &mut st)
}

fn find_unquoted_hash(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = start;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 1 + char_len_at(line, i + 1);
                continue;
            }
            if b == q {
                quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if b == b'#' {
            return Some(i);
        }
        i += char_len_at(line, i);
    }
    None
}

fn push_token(tokens: &mut Vec<SyntaxToken>, start: usize, end: usize, kind: TokenKind) {
    if end <= start {
        return;
    }
    if let Some(last) = tokens.last_mut()
        && last.kind == kind
        && last.end == start
    {
        last.end = end;
        return;
    }
    tokens.push(SyntaxToken { start, end, kind });
}

fn supports_slash_comments(lang: SyntaxLanguage) -> bool {
    matches!(
        lang,
        SyntaxLanguage::Rust
            | SyntaxLanguage::TypeScript
            | SyntaxLanguage::JavaScript
            | SyntaxLanguage::Go
            | SyntaxLanguage::C
            | SyntaxLanguage::Cpp
            | SyntaxLanguage::Json
    )
}

fn supports_block_comments(lang: SyntaxLanguage) -> bool {
    matches!(
        lang,
        SyntaxLanguage::Rust
            | SyntaxLanguage::TypeScript
            | SyntaxLanguage::JavaScript
            | SyntaxLanguage::Go
            | SyntaxLanguage::C
            | SyntaxLanguage::Cpp
            | SyntaxLanguage::Sql
            | SyntaxLanguage::Json
    )
}

fn supports_hash_comments(lang: SyntaxLanguage) -> bool {
    matches!(
        lang,
        SyntaxLanguage::Python
            | SyntaxLanguage::Shell
            | SyntaxLanguage::Toml
            | SyntaxLanguage::Yaml
    )
}

fn char_len_at(s: &str, byte_idx: usize) -> usize {
    s[byte_idx..].chars().next().map_or(1, char::len_utf8)
}

fn is_ident_start(s: &str, idx: usize) -> bool {
    let b = s.as_bytes()[idx];
    b.is_ascii_alphabetic()
        || b == b'_'
        || (!b.is_ascii() && s[idx..].chars().next().is_some_and(char::is_alphabetic))
}

fn is_ident_continue(s: &str, idx: usize) -> bool {
    let b = s.as_bytes()[idx];
    b.is_ascii_alphanumeric()
        || b == b'_'
        || (!b.is_ascii() && s[idx..].chars().next().is_some_and(char::is_alphanumeric))
}

fn count_hashes(slice: &[u8]) -> usize {
    slice.iter().take_while(|&&b| b == b'#').count()
}

fn match_rust_raw_string_start(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut pos = i;
    if bytes.get(pos) == Some(&b'b') || bytes.get(pos) == Some(&b'c') {
        pos += 1;
    }
    if bytes.get(pos) != Some(&b'r') {
        return None;
    }
    pos += 1;
    let hashes = count_hashes(&bytes[pos..]);
    pos += hashes;
    if bytes.get(pos) == Some(&b'"') {
        Some((pos + 1, hashes))
    } else {
        None
    }
}

fn scan_quoted(line: &str, start: usize, quote: u8) -> usize {
    let bytes = line.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1 + char_len_at(line, i + 1);
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += char_len_at(line, i);
    }
    bytes.len()
}

fn scan_rust_char_literal(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = start + 1;
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'\\' {
        i += 1;
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'x' {
            i += 1;
            let end = (i + 2).min(bytes.len());
            while i < end && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
        } else if bytes[i] == b'u' && bytes.get(i + 1) == Some(&b'{') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'}' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
        } else {
            i += char_len_at(line, i);
        }
    } else {
        let ch_len = char_len_at(line, i);
        i += ch_len;
    }
    (bytes.get(i) == Some(&b'\'')).then_some(i + 1)
}

fn scan_number(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    if bytes.get(i) == Some(&b'0') && i + 1 < bytes.len() {
        match bytes[i + 1] {
            b'x' | b'X' => {
                i += 2;
                while i < bytes.len() && (bytes[i].is_ascii_hexdigit() || bytes[i] == b'_') {
                    i += 1;
                }
                return scan_num_suffix(bytes, i);
            }
            b'b' | b'B' => {
                i += 2;
                while i < bytes.len() && (matches!(bytes[i], b'0' | b'1' | b'_')) {
                    i += 1;
                }
                return scan_num_suffix(bytes, i);
            }
            b'o' | b'O' => {
                i += 2;
                while i < bytes.len() && (matches!(bytes[i], b'0'..=b'7' | b'_')) {
                    i += 1;
                }
                return scan_num_suffix(bytes, i);
            }
            _ => {}
        }
    }
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
            i += 1;
        }
    }
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        let mut exp_i = i + 1;
        if matches!(bytes.get(exp_i), Some(b'+' | b'-')) {
            exp_i += 1;
        }
        if bytes.get(exp_i).is_some_and(u8::is_ascii_digit) {
            i = exp_i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                i += 1;
            }
        }
    }
    scan_num_suffix(bytes, i)
}

fn scan_num_suffix(bytes: &[u8], mut i: usize) -> usize {
    if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
    }
    i
}

fn next_non_ws_is(bytes: &[u8], mut i: usize, target: u8) -> bool {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    bytes.get(i) == Some(&target)
}

fn match_multi_char_op(slice: &[u8]) -> Option<usize> {
    for op in [
        &b"==="[..],
        b"!==",
        b"..=",
        b"<<=",
        b">>=",
        b"->",
        b"=>",
        b"::",
        b"|>",
        b"<=",
        b">=",
        b"==",
        b"!=",
        b"&&",
        b"||",
        b"<<",
        b">>",
        b"..",
        b"+=",
        b"-=",
        b"*=",
        b"/=",
        b"%=",
        b"&=",
        b"|=",
        b"^=",
        b"??",
        b"?.",
    ] {
        if slice.starts_with(op) {
            return Some(op.len());
        }
    }
    None
}

fn classify_ident(
    ident: &str,
    lang: SyntaxLanguage,
    bytes: &[u8],
    end_idx: usize,
    after_member_dot: bool,
    cursor_i: &mut usize,
) -> TokenKind {
    // Rust macro invocation `foo!`
    if lang == SyntaxLanguage::Rust
        && bytes.get(end_idx) == Some(&b'!')
        && bytes.get(end_idx + 1) != Some(&b'=')
    {
        *cursor_i = end_idx + 1;
        return TokenKind::Macro;
    }

    if is_control_flow(ident, lang) {
        return TokenKind::ControlFlow;
    }
    if is_keyword(ident, lang) {
        return TokenKind::Keyword;
    }
    if is_constant(ident, lang) {
        return TokenKind::Constant;
    }
    if is_builtin_type(ident, lang) {
        return TokenKind::Type;
    }
    if next_non_ws_is(bytes, end_idx, b'(') {
        return TokenKind::Function;
    }
    if after_member_dot {
        return TokenKind::Property;
    }
    if matches!(
        lang,
        SyntaxLanguage::TypeScript | SyntaxLanguage::JavaScript | SyntaxLanguage::Rust
    ) && next_non_ws_is(bytes, end_idx, b':')
        && bytes.get(end_idx + 1) != Some(&b':')
    {
        return TokenKind::Property;
    }
    if ident.len() >= 2
        && ident
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && ident.chars().any(|c| c.is_ascii_uppercase())
    {
        return TokenKind::Constant;
    }
    if ident.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && ident.chars().any(|c| c.is_ascii_lowercase())
    {
        return TokenKind::Type;
    }
    TokenKind::Plain
}

fn is_control_flow(w: &str, lang: SyntaxLanguage) -> bool {
    match lang {
        SyntaxLanguage::Sql => {
            let u = w.to_ascii_uppercase();
            matches!(u.as_str(), "CASE" | "WHEN" | "THEN" | "ELSE" | "END" | "IF")
        }
        _ => matches!(
            w,
            "if" | "else"
                | "elif"
                | "match"
                | "switch"
                | "case"
                | "default"
                | "for"
                | "while"
                | "loop"
                | "return"
                | "break"
                | "continue"
                | "yield"
                | "await"
                | "try"
                | "catch"
                | "finally"
                | "throw"
                | "raise"
                | "except"
                | "pass"
                | "with"
                | "defer"
                | "select"
                | "go"
                | "do"
                | "fi"
                | "done"
                | "esac"
                | "then"
        ),
    }
}

fn is_keyword(w: &str, lang: SyntaxLanguage) -> bool {
    match lang {
        SyntaxLanguage::Rust => matches!(
            w,
            "fn" | "pub"
                | "let"
                | "mut"
                | "const"
                | "static"
                | "struct"
                | "enum"
                | "impl"
                | "trait"
                | "type"
                | "where"
                | "mod"
                | "use"
                | "crate"
                | "as"
                | "in"
                | "ref"
                | "dyn"
                | "async"
                | "unsafe"
                | "extern"
                | "move"
                | "super"
        ),
        SyntaxLanguage::TypeScript | SyntaxLanguage::JavaScript => matches!(
            w,
            "function"
                | "const"
                | "let"
                | "var"
                | "class"
                | "interface"
                | "type"
                | "enum"
                | "implements"
                | "extends"
                | "export"
                | "import"
                | "from"
                | "as"
                | "async"
                | "new"
                | "typeof"
                | "keyof"
                | "readonly"
                | "declare"
                | "namespace"
                | "public"
                | "private"
                | "protected"
                | "static"
                | "in"
                | "of"
                | "instanceof"
                | "delete"
                | "void"
        ),
        SyntaxLanguage::Python => matches!(
            w,
            "def"
                | "class"
                | "import"
                | "from"
                | "as"
                | "async"
                | "lambda"
                | "global"
                | "nonlocal"
                | "in"
                | "is"
                | "not"
                | "and"
                | "or"
                | "del"
                | "assert"
        ),
        SyntaxLanguage::Go => matches!(
            w,
            "func"
                | "package"
                | "import"
                | "type"
                | "struct"
                | "interface"
                | "var"
                | "const"
                | "map"
                | "chan"
                | "range"
                | "fallthrough"
                | "goto"
        ),
        SyntaxLanguage::C | SyntaxLanguage::Cpp => matches!(
            w,
            "struct"
                | "enum"
                | "union"
                | "typedef"
                | "const"
                | "static"
                | "extern"
                | "inline"
                | "volatile"
                | "sizeof"
                | "class"
                | "namespace"
                | "template"
                | "typename"
                | "using"
                | "public"
                | "private"
                | "protected"
                | "virtual"
                | "override"
                | "final"
                | "new"
                | "delete"
                | "constexpr"
                | "noexcept"
                | "auto"
        ),
        SyntaxLanguage::Shell => matches!(
            w,
            "function"
                | "export"
                | "local"
                | "readonly"
                | "declare"
                | "unset"
                | "source"
                | "in"
                | "FROM"
                | "RUN"
                | "CMD"
                | "COPY"
                | "ADD"
                | "ENV"
                | "WORKDIR"
                | "EXPOSE"
                | "ENTRYPOINT"
        ),
        SyntaxLanguage::Sql => {
            let u = w.to_ascii_uppercase();
            matches!(
                u.as_str(),
                "SELECT"
                    | "FROM"
                    | "WHERE"
                    | "INSERT"
                    | "INTO"
                    | "VALUES"
                    | "UPDATE"
                    | "SET"
                    | "DELETE"
                    | "CREATE"
                    | "ALTER"
                    | "DROP"
                    | "TABLE"
                    | "INDEX"
                    | "VIEW"
                    | "JOIN"
                    | "LEFT"
                    | "RIGHT"
                    | "INNER"
                    | "OUTER"
                    | "ON"
                    | "AS"
                    | "AND"
                    | "OR"
                    | "NOT"
                    | "IN"
                    | "IS"
                    | "GROUP"
                    | "BY"
                    | "ORDER"
                    | "ASC"
                    | "DESC"
                    | "LIMIT"
                    | "OFFSET"
                    | "HAVING"
                    | "DISTINCT"
                    | "PRIMARY"
                    | "KEY"
                    | "REFERENCES"
            )
        }
        _ => false,
    }
}

fn is_constant(w: &str, lang: SyntaxLanguage) -> bool {
    if lang == SyntaxLanguage::Sql {
        let u = w.to_ascii_uppercase();
        return matches!(u.as_str(), "TRUE" | "FALSE" | "NULL");
    }
    matches!(
        w,
        "true"
            | "false"
            | "True"
            | "False"
            | "null"
            | "nil"
            | "None"
            | "Some"
            | "Ok"
            | "Err"
            | "self"
            | "Self"
            | "this"
            | "undefined"
            | "NaN"
            | "Infinity"
            | "nullptr"
    )
}

fn is_builtin_type(w: &str, lang: SyntaxLanguage) -> bool {
    match lang {
        SyntaxLanguage::Rust => matches!(
            w,
            "bool"
                | "char"
                | "str"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "f32"
                | "f64"
                | "String"
                | "Vec"
                | "Option"
                | "Result"
                | "Box"
                | "Arc"
                | "Rc"
                | "HashMap"
                | "HashSet"
                | "BTreeMap"
                | "Path"
                | "PathBuf"
        ),
        SyntaxLanguage::TypeScript | SyntaxLanguage::JavaScript => matches!(
            w,
            "string"
                | "number"
                | "boolean"
                | "any"
                | "unknown"
                | "never"
                | "object"
                | "symbol"
                | "bigint"
                | "Promise"
                | "Array"
                | "Record"
                | "Map"
                | "Set"
        ),
        SyntaxLanguage::Python => matches!(
            w,
            "int" | "float" | "str" | "bool" | "bytes" | "list" | "dict" | "tuple" | "set" | "Any"
        ),
        SyntaxLanguage::Go => matches!(
            w,
            "bool"
                | "byte"
                | "rune"
                | "string"
                | "int"
                | "int8"
                | "int16"
                | "int32"
                | "int64"
                | "uint"
                | "uint8"
                | "uint16"
                | "uint32"
                | "uint64"
                | "uintptr"
                | "float32"
                | "float64"
                | "error"
                | "any"
        ),
        SyntaxLanguage::C | SyntaxLanguage::Cpp => matches!(
            w,
            "void"
                | "bool"
                | "char"
                | "short"
                | "int"
                | "long"
                | "float"
                | "double"
                | "signed"
                | "unsigned"
                | "size_t"
                | "ssize_t"
                | "uint8_t"
                | "uint16_t"
                | "uint32_t"
                | "uint64_t"
                | "int8_t"
                | "int16_t"
                | "int32_t"
                | "int64_t"
                | "std"
                | "string"
                | "vector"
        ),
        SyntaxLanguage::Sql => {
            let u = w.to_ascii_uppercase();
            matches!(
                u.as_str(),
                "INT"
                    | "INTEGER"
                    | "BIGINT"
                    | "TEXT"
                    | "VARCHAR"
                    | "BOOLEAN"
                    | "REAL"
                    | "BLOB"
                    | "TIMESTAMP"
            )
        }
        _ => false,
    }
}

/// Maps a [`TokenKind`] to `(color, bold, italic)` derived from the active Omarchy [`Theme`]
/// (`terminal.ansi` + [`UiTokens`]), ensuring high WCAG readability in both dark and light modes.
#[must_use]
pub fn token_style(kind: TokenKind, theme: &Theme, tokens: &UiTokens) -> (Rgba, bool, bool) {
    let ansi = &theme.palette.terminal.ansi;
    let (raw_color, bold, italic) = match kind {
        TokenKind::Keyword => (ansi[5].opaque(), true, false),
        TokenKind::ControlFlow => (ansi[13].opaque(), true, false),
        TokenKind::Type => (ansi[3].opaque(), false, false),
        TokenKind::String => (ansi[2].opaque(), false, false),
        TokenKind::Number => (ansi[9].opaque(), false, false),
        TokenKind::Constant => (ansi[11].opaque(), false, false),
        TokenKind::Comment => (tokens.fg_muted, false, true),
        TokenKind::Attribute => (ansi[13].opaque(), false, false),
        TokenKind::Macro => (ansi[12].opaque(), true, false),
        TokenKind::Function => (ansi[4].opaque(), false, false),
        TokenKind::Property => (ansi[6].opaque(), false, false),
        TokenKind::Operator => (ansi[14].opaque(), false, false),
        TokenKind::Punctuation => (tokens.fg_secondary, false, false),
        TokenKind::DiffAdded => (ansi[2].opaque(), false, false),
        TokenKind::DiffRemoved => (ansi[1].opaque(), false, false),
        TokenKind::DiffHunk => (tokens.accent_primary, true, false),
        TokenKind::Plain => (tokens.fg_primary, false, false),
    };

    if kind == TokenKind::Comment || kind == TokenKind::Punctuation || kind == TokenKind::Plain {
        return (raw_color, bold, italic);
    }

    let bg = tokens.surface_pane.rgb();
    let adjusted = if raw_color.rgb().wcag_contrast(bg) < 2.2 {
        raw_color
            .rgb()
            .blend(tokens.fg_primary.rgb(), 0.45)
            .opaque()
    } else {
        raw_color
    };
    (adjusted, bold, italic)
}

/// Converts a byte slice `[slice_start..slice_end]` of a tab-expanded line and its
/// [`SyntaxToken`]s into merged [`TextSpan`]s ready for [`crate::draw::UiPrimitive::RichText`].
#[must_use]
pub fn slice_tokens_to_spans(
    line_expanded: &str,
    line_tokens: &[SyntaxToken],
    slice_start: usize,
    slice_end: usize,
    theme: &Theme,
    ui_tokens: &UiTokens,
) -> Vec<TextSpan> {
    let s = slice_start.min(line_expanded.len());
    let e = slice_end.min(line_expanded.len());
    if e <= s || !line_expanded.is_char_boundary(s) || !line_expanded.is_char_boundary(e) {
        return Vec::new();
    }

    let mut spans: Vec<TextSpan> = Vec::new();
    let mut push_piece = |piece: &str, color: Rgba, bold: bool, italic: bool| {
        if piece.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut()
            && last.color == color
            && last.bold == bold
            && last.italic == italic
        {
            last.text.push_str(piece);
        } else {
            spans.push(TextSpan::styled(piece, color, bold, italic));
        }
    };

    if line_tokens.is_empty() {
        let (col, bold, italic) = token_style(TokenKind::Plain, theme, ui_tokens);
        push_piece(&line_expanded[s..e], col, bold, italic);
        return spans;
    }

    let mut cursor = s;
    for tok in line_tokens {
        if tok.end <= s {
            continue;
        }
        if tok.start >= e {
            break;
        }
        let ts = tok.start.max(s);
        let te = tok.end.min(e);
        if ts > cursor
            && line_expanded.is_char_boundary(cursor)
            && line_expanded.is_char_boundary(ts)
        {
            let (col, bold, italic) = token_style(TokenKind::Plain, theme, ui_tokens);
            push_piece(&line_expanded[cursor..ts], col, bold, italic);
        }
        if te > ts && line_expanded.is_char_boundary(ts) && line_expanded.is_char_boundary(te) {
            let (col, bold, italic) = token_style(tok.kind, theme, ui_tokens);
            push_piece(&line_expanded[ts..te], col, bold, italic);
            cursor = te;
        }
    }
    if cursor < e && line_expanded.is_char_boundary(cursor) {
        let (col, bold, italic) = token_style(TokenKind::Plain, theme, ui_tokens);
        push_piece(&line_expanded[cursor..e], col, bold, italic);
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_and_find_unquoted_hash_handle_escaped_multibyte_utf8() {
        let header = "[section.\"\\€\"] # trailing comment";
        let hash_pos = find_unquoted_hash(header, 0).expect("unquoted hash found");
        assert_eq!(&header[hash_pos..], "# trailing comment");
        assert_eq!(find_unquoted_hash("[\"\\🦀#inside\"]", 0), None);

        let lines = vec![header.to_owned()];
        let tok_lines = tokenize_lines(&lines, SyntaxLanguage::Toml);
        assert_eq!(tok_lines.len(), 1);
        assert!(
            tok_lines[0].iter().any(|t| t.kind == TokenKind::Comment),
            "expected TOML section header comment token after escaped euro"
        );
    }
}
