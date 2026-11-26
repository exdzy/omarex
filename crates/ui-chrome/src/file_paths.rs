//! Zero-regex, zero-allocation ASCII-fast-path file path & `line:col` detector for terminal hover.
//!
//! Replaces `orex`'s expensive per-mouseover `QRegularExpression` scanning with a single-pass
//! byte-slice scanner over the hovered wrapped line.

use std::ops::Range;

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
    let (path_slice, line, column, loc_len) = split_path_and_location(path_with_loc);
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
}
