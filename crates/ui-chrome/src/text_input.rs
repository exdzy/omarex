//! Reusable UTF-8-safe single-line text input state machine for palettes, modals, filters, and rename fields.

use std::ops::Range;

/// Active IME composition (preedit) string displayed inline at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImePreedit {
    pub text: String,
    /// Optional highlighted byte range within `text`.
    pub cursor_range: Option<Range<usize>>,
}

/// Self-contained single-line text editing state with selection, word motion, history, and IME preedit.
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    text: String,
    /// Caret byte offset within `text` (always on a UTF-8 char boundary).
    cursor: usize,
    /// Selection anchor byte offset when a selection is active.
    anchor: Option<usize>,
    placeholder: String,
    preedit: Option<ImePreedit>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    draft_before_history: String,
}

impl TextInput {
    /// Creates an empty text input with the given placeholder hint.
    #[must_use]
    pub fn new(placeholder: impl Into<String>) -> Self {
        Self {
            placeholder: placeholder.into(),
            ..Self::default()
        }
    }

    /// Creates a text input pre-populated with `value` and cursor at the end.
    #[must_use]
    pub fn with_value(placeholder: impl Into<String>, value: impl Into<String>) -> Self {
        let text = value.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            placeholder: placeholder.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.preedit.as_ref().is_none_or(|p| p.text.is_empty())
    }

    #[must_use]
    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.placeholder = placeholder.into();
    }

    /// Caret byte offset within `text()`.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Active selection byte range `[start..end)` if non-empty.
    #[must_use]
    pub fn selected_range(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            None
        } else if anchor < self.cursor {
            Some(anchor..self.cursor)
        } else {
            Some(self.cursor..anchor)
        }
    }

    /// Active selected substring if non-empty.
    #[must_use]
    pub fn selected_text(&self) -> Option<&str> {
        let range = self.selected_range()?;
        Some(&self.text[range])
    }

    /// Replaces the entire text and places the cursor at the end.
    pub fn set_text(&mut self, value: impl Into<String>) {
        self.text = value.into();
        self.cursor = self.text.len();
        self.anchor = None;
        self.preedit = None;
        self.history_cursor = None;
    }

    /// Clears text, selection, and preedit.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = None;
        self.preedit = None;
        self.history_cursor = None;
    }

    /// Selects the entire committed text.
    pub fn select_all(&mut self) {
        if self.text.is_empty() {
            self.anchor = None;
            self.cursor = 0;
        } else {
            self.anchor = Some(0);
            self.cursor = self.text.len();
        }
    }

    /// Clears selection without moving the caret.
    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Inserts a single character at the caret (replacing any active selection).
    pub fn insert_char(&mut self, ch: char) {
        if matches!(ch, '\r' | '\n') {
            return;
        }
        let mut buf = [0u8; 4];
        self.insert_str(ch.encode_utf8(&mut buf));
    }

    /// Inserts text at the caret (replacing any active selection and collapsing newlines to spaces).
    pub fn insert_str(&mut self, input: &str) {
        self.preedit = None;
        self.delete_selected_internal();
        if input.is_empty() {
            return;
        }
        if !input.contains(['\r', '\n']) {
            self.text.insert_str(self.cursor, input);
            self.cursor += input.len();
        } else {
            let cleaned: String = input
                .chars()
                .filter(|&c| c != '\r')
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect();
            self.text.insert_str(self.cursor, &cleaned);
            self.cursor += cleaned.len();
        }
        self.history_cursor = None;
    }

    /// Pastes clipboard text into the single-line input.
    pub fn paste(&mut self, clipboard: &str) {
        self.insert_str(clipboard);
    }

    /// Copies the currently selected text (if any).
    #[must_use]
    pub fn copy(&self) -> Option<String> {
        self.selected_text().map(str::to_owned)
    }

    /// Cuts the currently selected text (if any) and returns it.
    pub fn cut(&mut self) -> Option<String> {
        let range = self.selected_range()?;
        let out = self.text[range.clone()].to_owned();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = None;
        self.preedit = None;
        self.history_cursor = None;
        Some(out)
    }

    /// Deletes selection or the character before the caret.
    pub fn backspace(&mut self) -> bool {
        self.preedit = None;
        if self.delete_selected_internal() {
            self.history_cursor = None;
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.history_cursor = None;
        true
    }

    /// Deletes selection or the character after the caret.
    pub fn delete_forward(&mut self) -> bool {
        self.preedit = None;
        if self.delete_selected_internal() {
            self.history_cursor = None;
            return true;
        }
        if self.cursor >= self.text.len() {
            return false;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        self.history_cursor = None;
        true
    }

    /// Deletes the previous word (`Ctrl+Backspace` / `Ctrl+W`).
    pub fn delete_word_backward(&mut self) -> bool {
        self.preedit = None;
        if self.delete_selected_internal() {
            self.history_cursor = None;
            return true;
        }
        let start = prev_word_boundary(&self.text, self.cursor);
        if start == self.cursor {
            return false;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.history_cursor = None;
        true
    }

    /// Deletes the next word (`Ctrl+Delete` / `Alt+D`).
    pub fn delete_word_forward(&mut self) -> bool {
        self.preedit = None;
        if self.delete_selected_internal() {
            self.history_cursor = None;
            return true;
        }
        let end = next_word_boundary(&self.text, self.cursor);
        if end == self.cursor {
            return false;
        }
        self.text.replace_range(self.cursor..end, "");
        self.history_cursor = None;
        true
    }

    /// Deletes from the start of the input to the caret (`Ctrl+U`).
    pub fn delete_to_line_start(&mut self) -> bool {
        self.preedit = None;
        self.anchor = None;
        if self.cursor == 0 {
            return false;
        }
        self.text.replace_range(0..self.cursor, "");
        self.cursor = 0;
        self.history_cursor = None;
        true
    }

    /// Deletes from the caret to the end of the input (`Ctrl+K`).
    pub fn delete_to_line_end(&mut self) -> bool {
        self.preedit = None;
        self.anchor = None;
        if self.cursor >= self.text.len() {
            return false;
        }
        self.text.truncate(self.cursor);
        self.history_cursor = None;
        true
    }

    pub fn move_left(&mut self, extend_selection: bool) {
        self.preedit = None;
        if !extend_selection && let Some(range) = self.selected_range() {
            self.cursor = range.start;
            self.anchor = None;
            return;
        }
        self.prepare_motion(extend_selection);
        self.cursor = prev_char_boundary(&self.text, self.cursor);
        self.collapse_empty_selection();
    }

    pub fn move_right(&mut self, extend_selection: bool) {
        self.preedit = None;
        if !extend_selection && let Some(range) = self.selected_range() {
            self.cursor = range.end;
            self.anchor = None;
            return;
        }
        self.prepare_motion(extend_selection);
        self.cursor = next_char_boundary(&self.text, self.cursor);
        self.collapse_empty_selection();
    }

    pub fn move_word_left(&mut self, extend_selection: bool) {
        self.preedit = None;
        self.prepare_motion(extend_selection);
        self.cursor = prev_word_boundary(&self.text, self.cursor);
        self.collapse_empty_selection();
    }

    pub fn move_word_right(&mut self, extend_selection: bool) {
        self.preedit = None;
        self.prepare_motion(extend_selection);
        self.cursor = next_word_boundary(&self.text, self.cursor);
        self.collapse_empty_selection();
    }

    pub fn move_home(&mut self, extend_selection: bool) {
        self.preedit = None;
        self.prepare_motion(extend_selection);
        self.cursor = 0;
        self.collapse_empty_selection();
    }

    pub fn move_end(&mut self, extend_selection: bool) {
        self.preedit = None;
        self.prepare_motion(extend_selection);
        self.cursor = self.text.len();
        self.collapse_empty_selection();
    }

    /// Moves the caret to the closest valid UTF-8 boundary at or before `byte_index`.
    pub fn set_cursor_byte(&mut self, byte_index: usize, extend_selection: bool) {
        self.preedit = None;
        self.prepare_motion(extend_selection);
        let clamped = byte_index.min(self.text.len());
        self.cursor = floor_char_boundary(&self.text, clamped);
        self.collapse_empty_selection();
    }

    /// Updates IME preedit text (empty string clears active preedit).
    pub fn set_preedit(&mut self, text: String, cursor_range: Option<Range<usize>>) {
        if text.is_empty() {
            self.preedit = None;
        } else {
            let clamped = cursor_range.map(|r| {
                let start = floor_char_boundary(&text, r.start.min(text.len()));
                let end = floor_char_boundary(&text, r.end.min(text.len())).max(start);
                start..end
            });
            self.preedit = Some(ImePreedit {
                text,
                cursor_range: clamped,
            });
        }
    }

    /// Commits IME text (replacing any preedit and active selection).
    pub fn commit_preedit(&mut self, committed: &str) {
        self.preedit = None;
        self.insert_str(committed);
    }

    pub fn clear_preedit(&mut self) {
        self.preedit = None;
    }

    #[must_use]
    pub fn preedit(&self) -> Option<&ImePreedit> {
        self.preedit.as_ref()
    }

    /// Returns the combined visual string (committed text with active IME preedit spliced in)
    /// and the visual caret byte offset.
    #[must_use]
    pub fn display_text(&self) -> (String, usize) {
        let Some(preedit) = &self.preedit else {
            return (self.text.clone(), self.cursor);
        };
        let mut combined = String::with_capacity(self.text.len() + preedit.text.len());
        combined.push_str(&self.text[..self.cursor]);
        combined.push_str(&preedit.text);
        combined.push_str(&self.text[self.cursor..]);
        let preedit_caret = preedit
            .cursor_range
            .as_ref()
            .map_or(preedit.text.len(), |r| r.end);
        (combined, self.cursor + preedit_caret)
    }

    /// Records a submitted value into input history (deduplicating consecutive identical entries).
    pub fn push_history(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.is_empty() {
            return;
        }
        if self.history.last().is_some_and(|last| last == &entry) {
            self.history_cursor = None;
            return;
        }
        self.history.push(entry);
        self.history_cursor = None;
    }

    /// Navigates backward into input history, preserving any unsent draft.
    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let next_idx = match self.history_cursor {
            None => {
                self.draft_before_history = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next_idx);
        self.text = self.history[next_idx].clone();
        self.cursor = self.text.len();
        self.anchor = None;
        self.preedit = None;
        true
    }

    /// Navigates forward through input history, restoring the saved draft at the end.
    pub fn history_next(&mut self) -> bool {
        let Some(idx) = self.history_cursor else {
            return false;
        };
        if idx + 1 < self.history.len() {
            let next_idx = idx + 1;
            self.history_cursor = Some(next_idx);
            self.text = self.history[next_idx].clone();
        } else {
            self.history_cursor = None;
            self.text = std::mem::take(&mut self.draft_before_history);
        }
        self.cursor = self.text.len();
        self.anchor = None;
        self.preedit = None;
        true
    }

    /// Applies an autocomplete replacement over `replace_range` (clamped to char boundaries)
    /// and places the caret directly after the inserted completion.
    pub fn apply_completion(&mut self, replacement: &str, replace_range: Range<usize>) {
        self.preedit = None;
        self.anchor = None;
        let start = floor_char_boundary(&self.text, replace_range.start.min(self.text.len()));
        let end =
            floor_char_boundary(&self.text, replace_range.end.min(self.text.len())).max(start);
        self.text.replace_range(start..end, replacement);
        self.cursor = start + replacement.len();
        self.history_cursor = None;
    }

    fn delete_selected_internal(&mut self) -> bool {
        let Some(range) = self.selected_range() else {
            return false;
        };
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = None;
        true
    }

    fn prepare_motion(&mut self, extend_selection: bool) {
        if extend_selection {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
    }

    fn collapse_empty_selection(&mut self) {
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
    }
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    s[..idx]
        .char_indices()
        .next_back()
        .map_or(0, |(pos, _)| pos)
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let ch = s[idx..].chars().next().expect("idx < s.len()");
    idx + ch.len_utf8()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn prev_word_boundary(s: &str, idx: usize) -> usize {
    let mut pos = idx;
    // Skip trailing whitespace / punctuation before the word.
    while pos > 0 {
        let prev = prev_char_boundary(s, pos);
        let ch = s[prev..pos].chars().next().unwrap();
        if is_word_char(ch) {
            break;
        }
        pos = prev;
    }
    // Consume word characters.
    while pos > 0 {
        let prev = prev_char_boundary(s, pos);
        let ch = s[prev..pos].chars().next().unwrap();
        if !is_word_char(ch) {
            break;
        }
        pos = prev;
    }
    pos
}

fn next_word_boundary(s: &str, idx: usize) -> usize {
    let mut pos = idx;
    // Skip leading non-word chars.
    while pos < s.len() {
        let next = next_char_boundary(s, pos);
        let ch = s[pos..next].chars().next().unwrap();
        if is_word_char(ch) {
            break;
        }
        pos = next;
    }
    // Consume word chars.
    while pos < s.len() {
        let next = next_char_boundary(s, pos);
        let ch = s[pos..next].chars().next().unwrap();
        if !is_word_char(ch) {
            break;
        }
        pos = next;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::TextInput;

    #[test]
    fn utf8_editing_and_selection_never_panic() {
        let mut input = TextInput::new("Filter threads");
        input.insert_str("Übung 🚀 café");
        assert_eq!(input.text(), "Übung 🚀 café");
        assert_eq!(input.cursor(), input.text().len());

        input.move_word_left(true);
        assert_eq!(input.selected_text(), Some("café"));
        assert_eq!(input.cut().as_deref(), Some("café"));
        assert_eq!(input.text(), "Übung 🚀 ");

        input.backspace();
        input.backspace();
        assert_eq!(input.text(), "Übung ");
        input.move_home(false);
        input.move_right(true);
        assert_eq!(input.selected_text(), Some("Ü"));
        input.insert_str("Ae");
        assert_eq!(input.text(), "Aebung ");
    }

    #[test]
    fn word_and_line_deletions_work() {
        let mut input = TextInput::with_value("Cmd", "git switch -c feat/omarex");
        input.delete_word_backward();
        assert_eq!(input.text(), "git switch -c feat/");
        input.move_home(false);
        input.move_word_right(false);
        assert_eq!(input.cursor(), 3);
        input.delete_word_forward();
        assert_eq!(input.text(), "git -c feat/");
        input.delete_to_line_end();
        assert_eq!(input.text(), "git");
        input.insert_str(" status");
        input.delete_to_line_start();
        assert_eq!(input.text(), "");
    }

    #[test]
    fn paste_strips_newlines_and_replaces_selection() {
        let mut input = TextInput::with_value("", "hello world");
        input.select_all();
        input.paste("line1\r\nline2\n");
        assert_eq!(input.text(), "line1 line2 ");
    }

    #[test]
    fn ime_preedit_and_commit_display_cleanly() {
        let mut input = TextInput::with_value("", "ab");
        input.move_left(false);
        input.set_preedit("かん".into(), Some(0..3));
        let (display, caret) = input.display_text();
        assert_eq!(display, "aかんb");
        assert_eq!(caret, 1 + 3);

        input.commit_preedit("漢");
        assert_eq!(input.text(), "a漢b");
        assert!(input.preedit().is_none());
    }

    #[test]
    fn history_and_completion_preserve_draft() {
        let mut input = TextInput::new("");
        input.push_history("first");
        input.push_history("second");
        input.push_history("second"); // deduped
        input.insert_str("draft");

        assert!(input.history_prev());
        assert_eq!(input.text(), "second");
        assert!(input.history_prev());
        assert_eq!(input.text(), "first");
        assert!(!input.history_prev());

        assert!(input.history_next());
        assert_eq!(input.text(), "second");
        assert!(input.history_next());
        assert_eq!(input.text(), "draft");
        assert!(!input.history_next());

        input.set_text("/sn");
        input.apply_completion("/snooze ", 0..3);
        assert_eq!(input.text(), "/snooze ");
        assert_eq!(input.cursor(), 8);
    }
}
