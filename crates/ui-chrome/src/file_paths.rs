//! Zero-regex, zero-allocation ASCII-fast-path file path & `line:col` detector for terminal hover.
//!
//! Replaces `orex`'s expensive per-mouseover `QRegularExpression` scanning with a single-pass
//! byte-slice scanner over the hovered wrapped line.

use std::ops::Range;
use std::path::{Path, PathBuf};

/// A file path (with optional line & column) detected under a hovered terminal column/byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedFilePath<'a> {
    /// Clean path slice (quotes, enclosing brackets, and trailing `:line:col` stripped).
    pub path: &'a str,
    /// 1-based line number if present (e.g., `:42` or `(42, 7)`).
    pub line: Option<u32>,
    /// 1-based column number if present (e.g., `:42:7` or `(42, 7)`).
    pub column: Option<u32>,
    /// Byte span in the source `line_text` covering the clickable link (path + optional `:line:col`).
    pub byte_span: Range<usize>,
    /// Unicode character index span in `line_text` (matching terminal column offsets for ASCII/BMP).
    pub char_span: Range<usize>,
}

/// Detects a clickable file path around `hover_char_col` (0-based char index) in a wrapped terminal line.
///
/// Returns `None` for plain words, web URLs (`http://`, `https://`, `ssh://`, etc.), or tokens
/// without path structure. Performs zero heap allocations.
#[must_use]
pub fn detect_at_char_col(line_text: &str, hover_char_col: usize) -> Option<DetectedFilePath<'_>> {
    let hover_byte = char_col_to_byte(line_text, hover_char_col)?;
    detect_at_byte_offset(line_text, hover_byte)
}

/// Detects a clickable file path around `hover_byte` (0-based UTF-8 byte offset) in `line_text`.
///
/// Supports:
/// - Quoted paths containing spaces (`'/path/with spaces/clip.mp4'`, `"/tmp/my shot.png"`)
/// - `file://` URIs (`file:///home/user/shot.png`, `file://localhost/tmp/video.mp4`)
/// - Absolute `/...`, home `~/...`, relative `./...`, `../...`, and repo-relative `dir/file.ext`
/// - Trailing `:line`, `:line:col`, and `(line,col)` compiler/editor locations
/// - `key=path` flag assignments (`--config=/etc/foo.conf`, `file=./a.log:12`)
#[must_use]
pub fn detect_at_byte_offset(line_text: &str, hover_byte: usize) -> Option<DetectedFilePath<'_>> {
    let bytes = line_text.as_bytes();
    if hover_byte >= bytes.len() {
        return None;
    }

    // 1. First check if `hover_byte` lies inside a quoted path span (supports spaces in filenames,
    //    such as shell-escaped dropped images/videos `'/home/user/My Videos/clip.mp4'`).
    if let Some(hit) = detect_quoted_path_at_byte(line_text, hover_byte) {
        return Some(hit);
    }

    if is_hard_delimiter(bytes[hover_byte]) {
        return None;
    }

    // Expand outward to whitespace / hard shell delimiters.
    let mut start = hover_byte;
    while start > 0 && !is_hard_delimiter(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = hover_byte;
    while end < bytes.len() && !is_hard_delimiter(bytes[end]) {
        end += 1;
    }

    // Clamp to valid UTF-8 boundaries in case hover_byte landed mid-codepoint.
    while start > 0 && !line_text.is_char_boundary(start) {
        start -= 1;
    }
    while end < bytes.len() && !line_text.is_char_boundary(end) {
        end += 1;
    }

    let (trimmed_start, trimmed_end) = strip_wrappers(line_text, start, end);
    if trimmed_start >= trimmed_end || hover_byte < trimmed_start || hover_byte >= trimmed_end {
        return None;
    }

    let token = &line_text[trimmed_start..trimmed_end];
    if let Some(hit) = detect_file_uri_token(line_text, trimmed_start, trimmed_end, hover_byte) {
        return Some(hit);
    }
    if looks_like_url(token) {
        return None;
    }

    // Handle `key=path` style tokens (e.g. `file=src/main.rs:10`, `--config=/etc/foo.conf`).
    let (token_start, token) = if let Some(eq_pos) = token.find('=') {
        let rhs_start = trimmed_start + eq_pos + 1;
        if hover_byte >= rhs_start && rhs_start < trimmed_end {
            let (s, e) = strip_wrappers(line_text, rhs_start, trimmed_end);
            if s < e && hover_byte >= s && hover_byte < e {
                (s, &line_text[s..e])
            } else {
                (trimmed_start, token)
            }
        } else {
            (trimmed_start, token)
        }
    } else {
        (trimmed_start, token)
    };
    let token_end = token_start + token.len();

    if let Some(hit) = detect_file_uri_token(line_text, token_start, token_end, hover_byte) {
        return Some(hit);
    }
    if looks_like_url(token) {
        return None;
    }

    let (path_slice, line, column, link_len) = split_path_and_location(token);
    if !is_plausible_path(path_slice) {
        return None;
    }

    let final_end = (token_start + link_len).min(token_end);
    if hover_byte < token_start || hover_byte >= final_end {
        return None;
    }

    let char_start = line_text[..token_start].chars().count();
    let char_len = line_text[token_start..final_end].chars().count();
    Some(DetectedFilePath {
        path: path_slice,
        line,
        column,
        byte_span: token_start..final_end,
        char_span: char_start..(char_start + char_len),
    })
}

/// Detects an `http://` or `https://` URL around `hover_char_col` (0-based char index) in a
/// terminal line, stripping surrounding brackets and trailing sentence punctuation.
#[must_use]
pub fn detect_url_at_char_col(line_text: &str, hover_char_col: usize) -> Option<&str> {
    let hover_byte = char_col_to_byte(line_text, hover_char_col)?;
    let bytes = line_text.as_bytes();
    if hover_byte >= bytes.len() || is_hard_delimiter(bytes[hover_byte]) {
        return None;
    }

    let mut start = hover_byte;
    while start > 0 && !is_hard_delimiter(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = hover_byte;
    while end < bytes.len() && !is_hard_delimiter(bytes[end]) {
        end += 1;
    }
    while start > 0 && !line_text.is_char_boundary(start) {
        start -= 1;
    }
    while end < bytes.len() && !line_text.is_char_boundary(end) {
        end += 1;
    }

    let (s, e) = strip_wrappers(line_text, start, end);
    if s >= e || hover_byte < s || hover_byte >= e {
        return None;
    }
    let tok = &line_text[s..e];
    let lower = tok.get(..8).unwrap_or(tok);
    if lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.eq_ignore_ascii_case("https://")
        || tok
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
    {
        let host_part = tok.split_once("://").map(|(_, rest)| rest).unwrap_or("");
        if !host_part.is_empty() && !host_part.starts_with('/') {
            return Some(tok);
        }
    }
    None
}

fn detect_quoted_path_at_byte(line_text: &str, hover_byte: usize) -> Option<DetectedFilePath<'_>> {
    let bytes = line_text.as_bytes();
    if matches!(bytes[hover_byte], b'\'' | b'"' | b'`' | b'\r' | b'\n') {
        return None;
    }

    // Scan left for an unmatched quote delimiter on this line.
    for quote in *b"'\"`" {
        let mut open = None;
        let mut i = hover_byte;
        while i > 0 {
            i -= 1;
            if bytes[i] == b'\n' || bytes[i] == b'\r' {
                break;
            }
            if bytes[i] == quote {
                open = Some(i);
                break;
            }
        }
        let Some(open_idx) = open else {
            continue;
        };
        let mut close = None;
        let mut j = hover_byte + 1;
        while j < bytes.len() {
            if bytes[j] == b'\n' || bytes[j] == b'\r' {
                break;
            }
            if bytes[j] == quote {
                close = Some(j);
                break;
            }
            j += 1;
        }
        let Some(close_idx) = close else {
            continue;
        };
        let inner_start = open_idx + 1;
        let inner_end = close_idx;
        if inner_start >= inner_end
            || !line_text.is_char_boundary(inner_start)
            || !line_text.is_char_boundary(inner_end)
        {
            continue;
        }
        let inner = &line_text[inner_start..inner_end];
        if !inner.contains(' ') {
            // Let standard unquoted path detection handle non-space quoted tokens so `:line:col`
            // outside or inside quotes behaves identically.
            continue;
        }
        if let Some(hit) = detect_file_uri_token(line_text, inner_start, inner_end, hover_byte) {
            return Some(hit);
        }
        if looks_like_url(inner) {
            continue;
        }
        let (path_slice, line, column, link_len) = split_path_and_location(inner);
        if (path_slice.starts_with('/')
            || path_slice.starts_with("~/")
            || path_slice.starts_with("./")
            || path_slice.starts_with("../"))
            && is_plausible_path(path_slice)
        {
            let final_end = (inner_start + link_len).min(inner_end);
            if hover_byte >= inner_start && hover_byte < final_end {
                let char_start = line_text[..inner_start].chars().count();
                let char_len = line_text[inner_start..final_end].chars().count();
                return Some(DetectedFilePath {
                    path: path_slice,
                    line,
                    column,
                    byte_span: inner_start..final_end,
                    char_span: char_start..(char_start + char_len),
                });
            }
        }
    }
    None
}

fn detect_file_uri_token(
    line_text: &str,
    token_start: usize,
    token_end: usize,
    hover_byte: usize,
) -> Option<DetectedFilePath<'_>> {
    let token = &line_text[token_start..token_end];
    let rest = token.strip_prefix("file://")?;
    // Strip authority (empty for `file:///...` or hostname for `file://localhost/...`).
    let slash_pos = rest.find('/')?;
    let path_with_loc = &rest[slash_pos..];
    let (path_slice, line, _end_line, column, loc_len) =
        split_path_and_location_with_hash(path_with_loc);
    if !is_plausible_path(path_slice) {
        return None;
    }
    let final_end = (token_start + "file://".len() + slash_pos + loc_len).min(token_end);
    if hover_byte < token_start || hover_byte >= final_end {
        return None;
    }
    let char_start = line_text[..token_start].chars().count();
    let char_len = line_text[token_start..final_end].chars().count();
    Some(DetectedFilePath {
        path: path_slice,
        line,
        column,
        byte_span: token_start..final_end,
        char_span: char_start..(char_start + char_len),
    })
}

#[inline]
fn is_hard_delimiter(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t'
            | b'\r'
            | b'\n'
            | b'"'
            | b'\''
            | b'`'
            | b'<'
            | b'>'
            | b'|'
            | b';'
            | b'{'
            | b'}'
    )
}

fn strip_wrappers(line_text: &str, mut start: usize, mut end: usize) -> (usize, usize) {
    let bytes = line_text.as_bytes();
    // Trim leading opening wrappers (e.g., `(src/lib.rs:5)` or `[src/lib.rs]`).
    while start < end && matches!(bytes[start], b'(' | b'[' | b',' | b':') {
        start += 1;
    }
    // Trim trailing punctuation unless it closes a `(line,col)` suffix.
    while end > start {
        let last = bytes[end - 1];
        if last == b')' && has_paren_location_suffix(&line_text[start..end]) {
            break;
        }
        if matches!(last, b')' | b']' | b',' | b'.' | b':' | b'!' | b'?') {
            end -= 1;
        } else {
            break;
        }
    }
    (start, end)
}

fn looks_like_url(token: &str) -> bool {
    let Some(scheme_end) = token.find("://") else {
        return false;
    };
    scheme_end > 0
        && token.as_bytes()[..scheme_end]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// Checks for `(123)` or `(123,45)` / `(123, 45)` style location suffix.
fn has_paren_location_suffix(token: &str) -> bool {
    parse_paren_location(token).is_some()
}

fn parse_paren_location(token: &str) -> Option<(&str, u32, Option<u32>)> {
    let stripped = token.strip_suffix(')')?;
    let open = stripped.rfind('(')?;
    let inner = &stripped[open + 1..];
    let path = &stripped[..open];
    if path.is_empty() || inner.is_empty() {
        return None;
    }
    if let Some((l, c)) = inner.split_once(',') {
        let line = parse_positive_u32(l.trim())?;
        let col = parse_positive_u32(c.trim())?;
        Some((path, line, Some(col)))
    } else {
        let line = parse_positive_u32(inner.trim())?;
        Some((path, line, None))
    }
}

/// Splits `path:line:col`, `path:line`, or `path(line,col)` cleanly without regex.
fn split_path_and_location(token: &str) -> (&str, Option<u32>, Option<u32>, usize) {
    if let Some((path, line, col)) = parse_paren_location(token) {
        return (path, Some(line), col, token.len());
    }

    // Parse trailing `:digits` (up to two segments: `:line:col` or `:line`).
    let mut base = token;
    let mut trailing_nums: [Option<u32>; 2] = [None, None];
    let mut count = 0usize;

    while count < 2 {
        let Some(colon) = base.rfind(':') else {
            break;
        };
        let suffix = &base[colon + 1..];
        let Some(num) = parse_positive_u32(suffix) else {
            break;
        };
        if colon == 0 {
            break;
        }
        trailing_nums[count] = Some(num);
        count += 1;
        base = &base[..colon];
    }

    match count {
        2 => (base, trailing_nums[1], trailing_nums[0], token.len()),
        1 => (base, trailing_nums[0], None, token.len()),
        _ => (token, None, None, token.len()),
    }
}

fn parse_positive_u32(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v = s.parse::<u32>().ok()?;
    (v > 0).then_some(v)
}

/// Validates that `path` looks like a real filesystem path rather than a ratio, date, or word.
fn is_plausible_path(path: &str) -> bool {
    if path.len() < 2 {
        return false;
    }
    let bytes = path.as_bytes();

    // Must contain a `/` separator (absolute `/...`, home `~/...`, relative `./...`, `../...`,
    // or repo-relative `dir/file.ext`).
    let Some(slash_idx) = bytes.iter().position(|&b| b == b'/') else {
        return false;
    };
    if path.bytes().all(|b| b == b'/' || b == b'.') {
        return false;
    }

    // Reject obvious dates/fractions like `12/31/2026` or `3/4` where every segment is purely digits.
    if path
        .split('/')
        .filter(|seg| !seg.is_empty())
        .all(|seg| seg.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }

    // Explicit prefixes `/`, `~/`, `./`, `../` are always valid path roots if non-trivial.
    if path.starts_with('/')
        || path.starts_with("~/")
        || path.starts_with("./")
        || path.starts_with("../")
    {
        return path.len() > slash_idx + 1 || path == "/";
    }

    // Bare relative path (`crates/ui-chrome/src/lib.rs`, `docs/gui-spec.md`):
    // require a valid first segment character and a non-empty final component.
    let first = bytes[0];
    if !(first.is_ascii_alphanumeric() || matches!(first, b'_' | b'.' | b'@')) {
        return false;
    }
    !path.ends_with('/')
}

fn char_col_to_byte(s: &str, col: usize) -> Option<usize> {
    s.char_indices().nth(col).map(|(idx, _)| idx)
}

/// Decoded filesystem path with optional line/end-line/column location parsed from `:line:col`,
/// `(line,col)`, `#L12`, `#L12-L20`, or `#L12C5` suffixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFilePath {
    pub path: String,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub column: Option<u32>,
}

/// Coarse semantic kind of a detected link or filesystem target (for badges, tooltips & routing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    TextCode,
    Markdown,
    Image,
    Video,
    Audio,
    Pdf,
    ThreadLink,
    ExternalUrl,
    OtherFile,
}

/// Classified terminal link / file / media target ready for viewer or external dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTarget {
    TextCode {
        path: String,
        line: Option<u32>,
        end_line: Option<u32>,
        col: Option<u32>,
    },
    Markdown {
        path: String,
        line: Option<u32>,
        end_line: Option<u32>,
        col: Option<u32>,
    },
    Image {
        path: String,
    },
    Video {
        path: String,
    },
    Audio {
        path: String,
    },
    Pdf {
        path: String,
        page: Option<u32>,
    },
    ThreadLink {
        uri: String,
    },
    ExternalUrl {
        url: String,
    },
    OtherFile {
        path: String,
    },
}

impl ResolvedTarget {
    #[must_use]
    pub const fn kind(&self) -> TargetKind {
        match self {
            Self::TextCode { .. } => TargetKind::TextCode,
            Self::Markdown { .. } => TargetKind::Markdown,
            Self::Image { .. } => TargetKind::Image,
            Self::Video { .. } => TargetKind::Video,
            Self::Audio { .. } => TargetKind::Audio,
            Self::Pdf { .. } => TargetKind::Pdf,
            Self::ThreadLink { .. } => TargetKind::ThreadLink,
            Self::ExternalUrl { .. } => TargetKind::ExternalUrl,
            Self::OtherFile { .. } => TargetKind::OtherFile,
        }
    }

    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::TextCode { path, .. }
            | Self::Markdown { path, .. }
            | Self::Image { path }
            | Self::Video { path }
            | Self::Audio { path }
            | Self::Pdf { path, .. }
            | Self::OtherFile { path } => Some(path.as_str()),
            Self::ThreadLink { .. } | Self::ExternalUrl { .. } => None,
        }
    }

    #[must_use]
    pub const fn line(&self) -> Option<u32> {
        match self {
            Self::TextCode { line, .. } | Self::Markdown { line, .. } => *line,
            Self::Pdf { page, .. } => *page,
            _ => None,
        }
    }

    #[must_use]
    pub const fn end_line(&self) -> Option<u32> {
        match self {
            Self::TextCode { end_line, .. } | Self::Markdown { end_line, .. } => *end_line,
            _ => None,
        }
    }

    #[must_use]
    pub const fn col(&self) -> Option<u32> {
        match self {
            Self::TextCode { col, .. } | Self::Markdown { col, .. } => *col,
            _ => None,
        }
    }

    /// Returns `true` when this target can be displayed inside OmaRex's built-in viewer pane.
    #[must_use]
    pub const fn is_in_app_viewable(&self) -> bool {
        matches!(
            self,
            Self::TextCode { .. }
                | Self::Markdown { .. }
                | Self::Image { .. }
                | Self::Video { .. }
                | Self::Audio { .. }
                | Self::Pdf { .. }
        )
    }
}

/// High-level link/file hit on a terminal line paired with its exact Unicode `char_span`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedLink {
    pub target: ResolvedTarget,
    pub display_text: String,
    pub char_span: Range<usize>,
}

/// Decodes `%XX` UTF-8 percent-encoding (preserving invalid `%` sequences verbatim).
#[must_use]
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    if !bytes.contains(&b'%') {
        return input.to_owned();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[inline]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parses `#L12`, `#L12-L20`, `#L12-20`, `#L12C5`, `#L12:5`, or `#12` location fragments.
fn parse_hash_location(fragment: &str) -> Option<(u32, Option<u32>, Option<u32>)> {
    let trimmed = fragment.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = trimmed
        .strip_prefix('L')
        .or_else(|| trimmed.strip_prefix('l'))
        .unwrap_or(trimmed);

    // Range form: `L12-L20` or `L12-20`
    if let Some((start_s, end_s)) = body.split_once('-') {
        let start_line = parse_positive_u32(start_s)?;
        let end_clean = end_s
            .strip_prefix('L')
            .or_else(|| end_s.strip_prefix('l'))
            .unwrap_or(end_s);
        let end_line = parse_positive_u32(end_clean)?;
        return Some((start_line, Some(end_line), None));
    }

    // Column form: `L12C5`, `L12c5`, or `L12:5`
    if let Some((line_s, col_s)) = body
        .split_once('C')
        .or_else(|| body.split_once('c'))
        .or_else(|| body.split_once(':'))
    {
        let line = parse_positive_u32(line_s)?;
        let col = parse_positive_u32(col_s)?;
        return Some((line, None, Some(col)));
    }

    let line = parse_positive_u32(body)?;
    Some((line, None, None))
}

fn split_path_and_location_with_hash(
    token: &str,
) -> (&str, Option<u32>, Option<u32>, Option<u32>, usize) {
    if let Some((before_hash, fragment)) = token.rsplit_once('#')
        && !before_hash.is_empty()
        && let Some((line, end_line, col)) = parse_hash_location(fragment)
    {
        return (before_hash, Some(line), end_line, col, token.len());
    }
    let (base, line, col, len) = split_path_and_location(token);
    (base, line, None, col, len)
}

/// Parses a `file://` URI (including `file:///...`, `file://localhost/...`, percent-encoding,
/// `:line:col`, `(line,col)`, and `#L12` / `#L12-L20` / `#L12C5` fragments).
#[must_use]
pub fn parse_file_uri(uri: &str) -> Option<ParsedFilePath> {
    let trimmed = uri.trim();
    let rest = trimmed.strip_prefix("file://").or_else(|| {
        trimmed
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("file://"))
            .map(|_| &trimmed[7..])
    })?;
    let slash_pos = rest.find('/')?;
    let raw_path_loc = &rest[slash_pos..];
    // Strip optional `?query` before location parsing if no `#` came first.
    let (without_hash, hash_frag) = match raw_path_loc.rsplit_once('#') {
        Some((before, frag)) => (before, Some(frag)),
        None => (raw_path_loc, None),
    };
    let clean_path_part = without_hash.split('?').next().unwrap_or(without_hash);

    if let Some(frag) = hash_frag
        && let Some((line, end_line, col)) = parse_hash_location(frag)
    {
        return Some(ParsedFilePath {
            path: percent_decode(clean_path_part),
            line: Some(line),
            end_line,
            column: col,
        });
    }

    let (base, line, col, _) = split_path_and_location(clean_path_part);
    Some(ParsedFilePath {
        path: percent_decode(base),
        line,
        end_line: None,
        column: col,
    })
}

/// Parses a raw path string (or `file://` URI) into a decoded path + optional line/range/column.
#[must_use]
pub fn parse_path_with_location(raw: &str) -> ParsedFilePath {
    if let Some(parsed) = parse_file_uri(raw) {
        return parsed;
    }
    let trimmed = raw.trim();
    let (base, line, end_line, col, _) = split_path_and_location_with_hash(trimmed);
    ParsedFilePath {
        path: base.to_owned(),
        line,
        end_line,
        column: col,
    }
}

/// Classifies a filesystem path by extension or well-known filename into a [`TargetKind`].
#[must_use]
pub fn classify_extension(path: &str) -> TargetKind {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);

    if let Some(ext) = Path::new(file_name).extension().and_then(|s| s.to_str()) {
        return match ext.to_ascii_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" | "ico" | "avif" | "tif"
            | "tiff" => TargetKind::Image,
            "mp4" | "m4v" | "webm" | "mov" | "mkv" | "avi" | "ogv" | "flv" | "wmv" => {
                TargetKind::Video
            }
            "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" => TargetKind::Audio,
            "pdf" => TargetKind::Pdf,
            "md" | "markdown" | "mdx" | "rst" => TargetKind::Markdown,
            "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "pyi" | "go" | "c"
            | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "kt" | "kts" | "swift"
            | "rb" | "php" | "lua" | "sh" | "bash" | "zsh" | "fish" | "toml" | "json" | "jsonc"
            | "jsonl" | "ndjson" | "yaml" | "yml" | "xml" | "html" | "htm" | "css" | "scss"
            | "sass" | "less" | "sql" | "wgsl" | "glsl" | "vert" | "frag" | "zig" | "nix"
            | "txt" | "log" | "csv" | "tsv" | "ini" | "conf" | "cfg" | "env" | "diff" | "patch"
            | "lock" | "cmake" | "mk" | "proto" | "graphql" | "gql" | "vue" | "svelte"
            | "astro" | "tf" | "hcl" | "dockerfile" | "service" | "desktop" | "tex" | "bib" => {
                TargetKind::TextCode
            }
            _ => TargetKind::OtherFile,
        };
    }

    if is_known_extensionless_filename(file_name) {
        TargetKind::TextCode
    } else {
        TargetKind::OtherFile
    }
}

fn is_known_extensionless_filename(name: &str) -> bool {
    matches!(
        name,
        "Makefile"
            | "GNUmakefile"
            | "Dockerfile"
            | "Containerfile"
            | "PKGBUILD"
            | "LICENSE"
            | "COPYING"
            | "README"
            | "CHANGELOG"
            | "Justfile"
            | "justfile"
            | "Procfile"
            | "Vagrantfile"
            | "Rakefile"
            | "Gemfile"
            | ".gitignore"
            | ".gitattributes"
            | ".dockerignore"
            | ".editorconfig"
            | ".env"
            | ".bashrc"
            | ".zshrc"
    )
}

fn is_known_other_bare_extension(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "pdf"
            | "zip"
            | "tar"
            | "gz"
            | "bz2"
            | "xz"
            | "zst"
            | "7z"
            | "rar"
            | "mp3"
            | "wav"
            | "flac"
            | "ogg"
            | "aac"
            | "m4a"
            | "opus"
            | "wasm"
            | "sqlite"
            | "db"
            | "pkg"
    )
}

/// Classifies a resolved filesystem path + optional location into a [`ResolvedTarget`].
#[must_use]
pub fn classify_path(
    path: &str,
    line: Option<u32>,
    end_line: Option<u32>,
    col: Option<u32>,
) -> ResolvedTarget {
    match classify_extension(path) {
        TargetKind::Image => ResolvedTarget::Image {
            path: path.to_owned(),
        },
        TargetKind::Video => ResolvedTarget::Video {
            path: path.to_owned(),
        },
        TargetKind::Audio => ResolvedTarget::Audio {
            path: path.to_owned(),
        },
        TargetKind::Pdf => ResolvedTarget::Pdf {
            path: path.to_owned(),
            page: line,
        },
        TargetKind::Markdown => ResolvedTarget::Markdown {
            path: path.to_owned(),
            line,
            end_line,
            col,
        },
        TargetKind::TextCode => ResolvedTarget::TextCode {
            path: path.to_owned(),
            line,
            end_line,
            col,
        },
        _ => ResolvedTarget::OtherFile {
            path: path.to_owned(),
        },
    }
}

/// Returns `true` when `uri` is a T3 Code thread deep link (`t3-thread://...` or `t3code://...`).
#[must_use]
pub fn is_thread_uri(uri: &str) -> bool {
    let trimmed = uri.trim();
    trimmed
        .get(..12)
        .is_some_and(|p| p.eq_ignore_ascii_case("t3-thread://"))
        || trimmed
            .get(..9)
            .is_some_and(|p| p.eq_ignore_ascii_case("t3code://"))
}

/// Classifies an arbitrary URI or path (from an OSC 8 hyperlink or plain terminal text) into a
/// [`ResolvedTarget`].
#[must_use]
pub fn classify_uri(uri: &str) -> ResolvedTarget {
    let trimmed = uri.trim();
    if is_thread_uri(trimmed) {
        return ResolvedTarget::ThreadLink {
            uri: trimmed.to_owned(),
        };
    }
    if let Some(parsed) = parse_file_uri(trimmed) {
        return classify_path(&parsed.path, parsed.line, parsed.end_line, parsed.column);
    }
    if trimmed
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        || trimmed
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || trimmed
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("mailto:"))
    {
        return ResolvedTarget::ExternalUrl {
            url: trimmed.to_owned(),
        };
    }
    if looks_like_url(trimmed) {
        return ResolvedTarget::ExternalUrl {
            url: trimmed.to_owned(),
        };
    }
    let parsed = parse_path_with_location(trimmed);
    classify_path(&parsed.path, parsed.line, parsed.end_line, parsed.column)
}

/// Detects a `t3-thread://...` or `t3code://...` deep link around `hover_char_col` in `line_text`.
#[must_use]
pub fn detect_thread_uri_at_char_col(line_text: &str, hover_char_col: usize) -> Option<&str> {
    let (tok, _) = token_around_char_col(line_text, hover_char_col)?;
    is_thread_uri(tok).then_some(tok)
}

/// Detects a plausible bare filename (e.g. `omarex_launch_video_v2.mp4`, `README.md`, `main.rs:42`)
/// without `/` around `hover_char_col`, rejecting prose words, version numbers, and URLs.
#[must_use]
pub fn detect_bare_filename_at_char_col(
    line_text: &str,
    hover_char_col: usize,
) -> Option<DetectedFilePath<'_>> {
    let hover_byte = char_col_to_byte(line_text, hover_char_col)?;
    let (tok, byte_span) = token_around_byte(line_text, hover_byte)?;
    if looks_like_url(tok) || tok.contains('/') {
        return None;
    }

    let (token_start, token) = if let Some(eq_pos) = tok.find('=') {
        let rhs_start = byte_span.start + eq_pos + 1;
        if hover_byte >= rhs_start && rhs_start < byte_span.end {
            let (s, e) = strip_wrappers(line_text, rhs_start, byte_span.end);
            if s < e && hover_byte >= s && hover_byte < e {
                (s, &line_text[s..e])
            } else {
                (byte_span.start, tok)
            }
        } else {
            (byte_span.start, tok)
        }
    } else {
        (byte_span.start, tok)
    };
    let token_end = token_start + token.len();

    let (base, line, _end_line, column, link_len) = split_path_and_location_with_hash(token);
    if !is_plausible_bare_filename(base) {
        return None;
    }

    let final_end = (token_start + link_len).min(token_end);
    if hover_byte < token_start || hover_byte >= final_end {
        return None;
    }

    let char_start = line_text[..token_start].chars().count();
    let char_len = line_text[token_start..final_end].chars().count();
    Some(DetectedFilePath {
        path: base,
        line,
        column,
        byte_span: token_start..final_end,
        char_span: char_start..(char_start + char_len),
    })
}

fn is_plausible_bare_filename(name: &str) -> bool {
    if name.len() < 3 || name.contains('/') || name.contains('\\') || name.contains("://") {
        return false;
    }
    if is_known_extensionless_filename(name) {
        return true;
    }
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() || ext.is_empty() || !stem.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return false;
    }
    classify_extension(name) != TargetKind::OtherFile || is_known_other_bare_extension(ext)
}

/// Scans stitched viewport lines for any full path or `file://` URI whose basename equals
/// `filename`, returning the decoded full path (prefers absolute paths over relative ones).
#[must_use]
pub fn resolve_bare_filename_in_viewport(
    filename: &str,
    viewport_lines: &[String],
) -> Option<String> {
    let target_name = Path::new(filename)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    if target_name.is_empty() {
        return None;
    }

    let mut fallback_relative: Option<String> = None;

    for line in viewport_lines {
        let mut search_from = 0usize;
        while let Some(rel_idx) = line[search_from..].find(target_name) {
            let match_byte = search_from + rel_idx;
            if let Some(hit) = detect_at_byte_offset(line, match_byte) {
                let decoded = if line[hit.byte_span.clone()].starts_with("file://") {
                    percent_decode(hit.path)
                } else {
                    hit.path.to_owned()
                };
                let candidate_name = Path::new(&decoded).file_name().and_then(|s| s.to_str());
                if candidate_name == Some(target_name) {
                    if decoded.starts_with('/') || decoded.starts_with("~/") {
                        return Some(decoded);
                    }
                    if fallback_relative.is_none() {
                        fallback_relative = Some(decoded);
                    }
                }
            }
            search_from = match_byte + target_name.len();
        }
    }

    fallback_relative
}

/// Resolves a bare filename against `cwd` if `cwd.join(filename)` exists on the local filesystem.
#[must_use]
pub fn resolve_bare_filename_in_cwd(filename: &str, cwd: &Path) -> Option<PathBuf> {
    if filename.is_empty() || filename.contains('/') || filename.contains('\\') {
        return None;
    }
    let candidate = cwd.join(filename);
    candidate.exists().then_some(candidate)
}

/// Unified terminal hover/click link detector combining:
/// 1. Full/relative/quoted file paths & `file://` URIs (with percent-decoding & `#L`/`:line:col`)
/// 2. `t3-thread://` & `t3code://` deep links
/// 3. `http://` & `https://` URLs
/// 4. Bare filenames (e.g. `omarex_launch_video_v2.mp4`, `README.md`) resolved via `viewport_lines`
///    first, then `cwd` if provided.
#[must_use]
pub fn detect_link_at_char_col(
    line_text: &str,
    hover_char_col: usize,
    viewport_lines: &[String],
    cwd: Option<&Path>,
) -> Option<DetectedLink> {
    // 1. Explicit path or `file://` URI
    if let Some(hit) = detect_at_char_col(line_text, hover_char_col) {
        let raw_span = &line_text[hit.byte_span.clone()];
        let parsed = if raw_span.starts_with("file://") {
            parse_file_uri(raw_span).unwrap_or(ParsedFilePath {
                path: percent_decode(hit.path),
                line: hit.line,
                end_line: None,
                column: hit.column,
            })
        } else {
            let (_base, line, end_line, col, _) = split_path_and_location_with_hash(raw_span);
            ParsedFilePath {
                path: hit.path.to_owned(),
                line: line.or(hit.line),
                end_line,
                column: col.or(hit.column),
            }
        };
        let target = classify_path(&parsed.path, parsed.line, parsed.end_line, parsed.column);
        return Some(DetectedLink {
            target,
            display_text: raw_span.to_owned(),
            char_span: hit.char_span,
        });
    }

    // 2. `t3-thread://` or `t3code://` deep link
    if let Some((tok, byte_span)) = token_around_char_col(line_text, hover_char_col)
        && is_thread_uri(tok)
    {
        let char_start = line_text[..byte_span.start].chars().count();
        let char_len = tok.chars().count();
        return Some(DetectedLink {
            target: ResolvedTarget::ThreadLink {
                uri: tok.to_owned(),
            },
            display_text: tok.to_owned(),
            char_span: char_start..(char_start + char_len),
        });
    }

    // 3. `http://` or `https://` URL
    if let Some(url) = detect_url_at_char_col(line_text, hover_char_col)
        && let Some((_, byte_span)) = token_around_char_col(line_text, hover_char_col)
    {
        let char_start = line_text[..byte_span.start].chars().count();
        let char_len = url.chars().count();
        return Some(DetectedLink {
            target: ResolvedTarget::ExternalUrl {
                url: url.to_owned(),
            },
            display_text: url.to_owned(),
            char_span: char_start..(char_start + char_len),
        });
    }

    // 4. Bare filename resolved against visible viewport paths or CWD
    if let Some(bare) = detect_bare_filename_at_char_col(line_text, hover_char_col) {
        let resolved_path =
            resolve_bare_filename_in_viewport(bare.path, viewport_lines).or_else(|| {
                cwd.and_then(|dir| resolve_bare_filename_in_cwd(bare.path, dir))
                    .map(|p| p.to_string_lossy().into_owned())
            });
        if let Some(full_path) = resolved_path {
            let raw_span = &line_text[bare.byte_span.clone()];
            let (_, _, end_line, _, _) = split_path_and_location_with_hash(raw_span);
            let target = classify_path(&full_path, bare.line, end_line, bare.column);
            return Some(DetectedLink {
                target,
                display_text: raw_span.to_owned(),
                char_span: bare.char_span,
            });
        }
    }

    None
}

fn token_around_char_col(line_text: &str, hover_char_col: usize) -> Option<(&str, Range<usize>)> {
    let hover_byte = char_col_to_byte(line_text, hover_char_col)?;
    token_around_byte(line_text, hover_byte)
}

fn token_around_byte(line_text: &str, hover_byte: usize) -> Option<(&str, Range<usize>)> {
    let bytes = line_text.as_bytes();
    if hover_byte >= bytes.len() || is_hard_delimiter(bytes[hover_byte]) {
        return None;
    }

    let mut start = hover_byte;
    while start > 0 && !is_hard_delimiter(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = hover_byte;
    while end < bytes.len() && !is_hard_delimiter(bytes[end]) {
        end += 1;
    }
    while start > 0 && !line_text.is_char_boundary(start) {
        start -= 1;
    }
    while end < bytes.len() && !line_text.is_char_boundary(end) {
        end += 1;
    }

    let (s, e) = strip_wrappers(line_text, start, end);
    if s >= e || hover_byte < s || hover_byte >= e {
        return None;
    }
    Some((&line_text[s..e], s..e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rust_compiler_and_relative_paths_with_line_col() {
        let line = "  --> crates/ui-chrome/src/layout.rs:142:17";
        let col = line.find("layout.rs").unwrap();
        let hit = detect_at_char_col(line, col).expect("should detect path");
        assert_eq!(hit.path, "crates/ui-chrome/src/layout.rs");
        assert_eq!(hit.line, Some(142));
        assert_eq!(hit.column, Some(17));
        assert_eq!(
            &line[hit.byte_span],
            "crates/ui-chrome/src/layout.rs:142:17"
        );
    }

    #[test]
    fn detects_absolute_home_and_paren_locations() {
        let line = "see (/home/user/projects/omarex/docs/gui-spec.md:88), ~/projects/omarex/Cargo.toml and src/main.rs(24,5).";

        let h1 = detect_at_byte_offset(line, line.find("gui-spec").unwrap()).unwrap();
        assert_eq!(h1.path, "/home/user/projects/omarex/docs/gui-spec.md");
        assert_eq!(h1.line, Some(88));
        assert_eq!(h1.column, None);

        let h2 = detect_at_byte_offset(line, line.find("Cargo.toml").unwrap()).unwrap();
        assert_eq!(h2.path, "~/projects/omarex/Cargo.toml");
        assert_eq!(h2.line, None);

        let h3 = detect_at_byte_offset(line, line.find("main.rs").unwrap()).unwrap();
        assert_eq!(h3.path, "src/main.rs");
        assert_eq!(h3.line, Some(24));
        assert_eq!(h3.column, Some(5));
    }

    #[test]
    fn detects_key_value_path_arguments() {
        let line = "rex do dev_layout file=./logs/app.log:99";
        let hit = detect_at_byte_offset(line, line.find("app.log").unwrap()).unwrap();
        assert_eq!(hit.path, "./logs/app.log");
        assert_eq!(hit.line, Some(99));
        assert_eq!(hit.column, None);
    }

    #[test]
    fn ignores_urls_dates_and_plain_words() {
        let line = "visit https://superlogical.com/rex/docs on 11/26/2026 or 3/4 plainword";
        assert!(detect_at_byte_offset(line, line.find("superlogical").unwrap()).is_none());
        assert!(detect_at_byte_offset(line, line.find("11/26").unwrap()).is_none());
        assert!(detect_at_byte_offset(line, line.find("3/4").unwrap()).is_none());
        assert!(detect_at_byte_offset(line, line.find("plainword").unwrap()).is_none());
    }

    #[test]
    fn handles_unicode_prefix_char_columns_accurately() {
        let line = "✗ Fehler in ./src/über.rs:12:3!";
        let hover_col = line.chars().position(|c| c == 'ü').unwrap();
        let hit = detect_at_char_col(line, hover_col).unwrap();
        assert_eq!(hit.path, "./src/über.rs");
        assert_eq!(hit.line, Some(12));
        assert_eq!(hit.column, Some(3));
        assert_eq!(hit.char_span.start, 12);
    }

    #[test]
    fn detects_quoted_paths_with_spaces_file_uris_and_media_files() {
        let line = "dropped '/home/user/My Videos/demo clip.mp4' and file:///tmp/shot%201.webp";
        let vid_col = line.find("demo").unwrap();
        let hit = detect_at_char_col(line, vid_col).expect("quoted video path with spaces");
        assert_eq!(hit.path, "/home/user/My Videos/demo clip.mp4");
        assert_eq!(hit.line, None);

        let uri_col = line.find("shot").unwrap();
        let hit2 = detect_at_char_col(line, uri_col).expect("file:// URI image path");
        assert_eq!(hit2.path, "/tmp/shot%201.webp");
        assert_eq!(hit2.line, None);
    }

    #[test]
    fn detects_plain_http_and_https_urls() {
        let line = "Check https://workstation-rex.tail000000.ts.net/docs, or (http://127.0.0.1:8080/test).";
        let col1 = line.find("workstation-rex").unwrap();
        assert_eq!(
            detect_url_at_char_col(line, col1),
            Some("https://workstation-rex.tail000000.ts.net/docs")
        );
        let col2 = line.find("127.0.0.1").unwrap();
        assert_eq!(
            detect_url_at_char_col(line, col2),
            Some("http://127.0.0.1:8080/test")
        );
        assert_eq!(detect_url_at_char_col(line, 0), None);
    }

    #[test]
    fn parses_percent_encoded_file_uris_with_hash_and_colon_locations() {
        let p1 = parse_file_uri("file:///home/user/My%20Videos/clip%20v2.mp4").unwrap();
        assert_eq!(p1.path, "/home/user/My Videos/clip v2.mp4");
        assert_eq!(p1.line, None);

        let p2 = parse_file_uri("file://localhost/home/user/src/lib.rs#L12-L20").unwrap();
        assert_eq!(p2.path, "/home/user/src/lib.rs");
        assert_eq!(p2.line, Some(12));
        assert_eq!(p2.end_line, Some(20));
        assert_eq!(p2.column, None);

        let p3 = parse_file_uri("file:///home/user/src/main.rs#L42C7").unwrap();
        assert_eq!(p3.path, "/home/user/src/main.rs");
        assert_eq!(p3.line, Some(42));
        assert_eq!(p3.column, Some(7));

        let p4 = parse_file_uri("file:///home/user/docs/guide.md:88:3").unwrap();
        assert_eq!(p4.path, "/home/user/docs/guide.md");
        assert_eq!(p4.line, Some(88));
        assert_eq!(p4.column, Some(3));

        // Hover scanning also accepts `#L` fragments on `file://` URIs.
        let line = "open file:///home/user/src/lib.rs#L12-L20 right now";
        let col = line.find("lib.rs").unwrap();
        let link = detect_link_at_char_col(line, col, &[], None).expect("file URI with #L");
        assert_eq!(
            link.target,
            ResolvedTarget::TextCode {
                path: "/home/user/src/lib.rs".into(),
                line: Some(12),
                end_line: Some(20),
                col: None,
            }
        );
    }

    #[test]
    fn screenshot_case_bare_filename_resolves_via_stitched_viewport_and_cwd() {
        let stitched_viewport = vec![
            "• Clickable link: omarex_demo_video_v2.mp4".to_owned(),
            "│ /home/user/.cache/demo/2026-11-26-sample-session/omarex_demo_video_v2.mp4"
                .to_owned(),
        ];

        // 1. Hovering bare filename on line 0 resolves to the full stitched path from line 1!
        let line0 = &stitched_viewport[0];
        let col0 = line0
            .chars()
            .take(line0.find("omarex_demo").unwrap())
            .count();
        let link = detect_link_at_char_col(line0, col0, &stitched_viewport, None)
            .expect("bare video filename should resolve via viewport");
        assert_eq!(
            link.target,
            ResolvedTarget::Video {
                path: "/home/user/.cache/demo/2026-11-26-sample-session/omarex_demo_video_v2.mp4"
                    .into()
            }
        );
        assert!(link.target.is_in_app_viewable());
        assert_eq!(link.display_text, "omarex_demo_video_v2.mp4");

        // 2. Hovering bare filename resolved via CWD when no full path is in viewport.
        let tmp = std::env::temp_dir().join(format!("omarex-fp-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let readme = tmp.join("README.md");
        std::fs::write(&readme, "# hello\n").unwrap();

        let cwd_line = "See README.md:5 for details (version v0.1.8 or 28.0 s)";
        let readme_col = cwd_line.find("README.md").unwrap();
        let link_cwd = detect_link_at_char_col(cwd_line, readme_col, &[], Some(&tmp))
            .expect("README.md should resolve against CWD");
        assert_eq!(
            link_cwd.target,
            ResolvedTarget::Markdown {
                path: readme.to_string_lossy().into_owned(),
                line: Some(5),
                end_line: None,
                col: None,
            }
        );

        // Version numbers / decimals must NOT trigger bare-filename detection.
        assert!(
            detect_bare_filename_at_char_col(cwd_line, cwd_line.find("v0.1.8").unwrap()).is_none()
        );
        assert!(
            detect_bare_filename_at_char_col(cwd_line, cwd_line.find("28.0").unwrap()).is_none()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn classifies_thread_links_images_videos_markdown_code_and_other_files() {
        assert_eq!(
            classify_uri("t3-thread://v2/env-1/thread-abc"),
            ResolvedTarget::ThreadLink {
                uri: "t3-thread://v2/env-1/thread-abc".into(),
            }
        );
        assert_eq!(
            classify_uri("t3code://threads/env-1/thread-abc"),
            ResolvedTarget::ThreadLink {
                uri: "t3code://threads/env-1/thread-abc".into(),
            }
        );
        assert_eq!(
            classify_uri("https://github.com/exdzy/omarex/releases"),
            ResolvedTarget::ExternalUrl {
                url: "https://github.com/exdzy/omarex/releases".into(),
            }
        );
        assert_eq!(
            classify_uri("file:///tmp/preview%20shot.webp"),
            ResolvedTarget::Image {
                path: "/tmp/preview shot.webp".into(),
            }
        );
        assert_eq!(
            classify_uri("file:///tmp/demo.mp4"),
            ResolvedTarget::Video {
                path: "/tmp/demo.mp4".into(),
            }
        );
        assert_eq!(
            classify_uri("/home/user/projects/omarex/README.md#L10-L25"),
            ResolvedTarget::Markdown {
                path: "/home/user/projects/omarex/README.md".into(),
                line: Some(10),
                end_line: Some(25),
                col: None,
            }
        );
        assert_eq!(
            classify_uri("/home/user/projects/omarex/crates/vt-engine/src/lib.rs:170:9"),
            ResolvedTarget::TextCode {
                path: "/home/user/projects/omarex/crates/vt-engine/src/lib.rs".into(),
                line: Some(170),
                end_line: None,
                col: Some(9),
            }
        );
        assert_eq!(
            classify_uri("/tmp/archive.tar.gz"),
            ResolvedTarget::OtherFile {
                path: "/tmp/archive.tar.gz".into(),
            }
        );

        let line = "Jump to t3-thread://v2/env-1234/thread-5678 or t3code://threads/e/t";
        let col = line.find("env-1234").unwrap();
        assert_eq!(
            detect_thread_uri_at_char_col(line, col),
            Some("t3-thread://v2/env-1234/thread-5678")
        );
        let link = detect_link_at_char_col(line, col, &[], None).unwrap();
        assert_eq!(
            link.target,
            ResolvedTarget::ThreadLink {
                uri: "t3-thread://v2/env-1234/thread-5678".into(),
            }
        );
    }
}
