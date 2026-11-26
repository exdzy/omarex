//! High-speed `nucleo-matcher` fuzzy search & highlight engine for Command Palette,
//! Switch Session, Go to Anything, and `rex.ui.pick` (up to 2,000 items in sub-millisecond time).

use std::cmp::Ordering;
use std::ops::Range;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::icons::IconId;

/// Uniform filterable item for Command Palette, Switch Session, Go to Anything, and `rex.ui.pick`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub category: Option<String>,
    pub keywords: Option<String>,
    pub detail: Option<String>,
    pub icon: Option<IconId>,
    /// Optional shortcut badge displayed at the far right (e.g., `"Ctrl+Shift+D"`).
    pub shortcut: Option<String>,
    /// Multi-select tick state (used by `rex.ui.pick` when `multiple = true`).
    pub selected: bool,
    /// Optional MRU tie-breaker bonus (added to score when query is non-empty).
    pub mru_boost: u32,
}

impl PaletteItem {
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            category: None,
            keywords: None,
            detail: None,
            icon: None,
            shortcut: None,
            selected: false,
            mru_boost: 0,
        }
    }
}

/// Ranked fuzzy match referencing the source slice index plus matched character positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredMatch {
    /// Index into the input `&[PaletteItem]` slice.
    pub index: usize,
    /// Composite relevance score (higher is better; `0` when query is empty).
    pub score: u32,
    /// Sorted, deduplicated Unicode char indices matched in `item.title`.
    pub title_indices: Vec<u32>,
    /// Sorted, deduplicated Unicode char indices matched in `item.subtitle`.
    pub subtitle_indices: Vec<u32>,
}

/// Reusable fuzzy filter engine keeping `nucleo_matcher::Matcher`'s internal slab allocated across keystrokes.
pub struct PaletteMatcher {
    matcher: Matcher,
    pattern: Pattern,
    utf32_buf: Vec<char>,
    indices_buf: Vec<u32>,
    combined_buf: String,
}

impl Default for PaletteMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PaletteMatcher {
    /// Allocates a matcher configured with default smart-case fuzzy scoring.
    #[must_use]
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            pattern: Pattern::default(),
            utf32_buf: Vec::with_capacity(256),
            indices_buf: Vec::with_capacity(64),
            combined_buf: String::with_capacity(512),
        }
    }

    /// Filters and ranks `items` against `query`.
    ///
    /// When `query` is empty/whitespace-only, preserves exact input order (required by `rex.ui.pick`)
    /// with `score = 0` and empty highlight indices.
    pub fn filter(&mut self, query: &str, items: &[PaletteItem]) -> Vec<ScoredMatch> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return (0..items.len())
                .map(|index| ScoredMatch {
                    index,
                    score: 0,
                    title_indices: Vec::new(),
                    subtitle_indices: Vec::new(),
                })
                .collect();
        }

        self.pattern
            .reparse(trimmed, CaseMatching::Smart, Normalization::Smart);
        if self.pattern.atoms.is_empty() {
            return (0..items.len())
                .map(|index| ScoredMatch {
                    index,
                    score: 0,
                    title_indices: Vec::new(),
                    subtitle_indices: Vec::new(),
                })
                .collect();
        }

        let lower_query = trimmed.to_ascii_lowercase();
        let mut matches = Vec::with_capacity(items.len().min(128));

        for (index, item) in items.iter().enumerate() {
            if let Some(scored) = self.score_item(index, item, &lower_query) {
                matches.push(scored);
            }
        }

        matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index)));
        matches
    }

    fn score_item(
        &mut self,
        index: usize,
        item: &PaletteItem,
        lower_query: &str,
    ) -> Option<ScoredMatch> {
        // 1. Score primary title (weighted 4x + exact/prefix bonuses).
        self.indices_buf.clear();
        let title_haystack = Utf32Str::new(&item.title, &mut self.utf32_buf);
        let title_raw =
            self.pattern
                .indices(title_haystack, &mut self.matcher, &mut self.indices_buf);
        let mut title_indices = if title_raw.is_some() {
            dedup_indices(&mut self.indices_buf)
        } else {
            Vec::new()
        };

        // 2. Score subtitle (weighted 2x).
        let mut subtitle_indices = Vec::new();
        let subtitle_raw = if let Some(sub) = item.subtitle.as_deref() {
            self.indices_buf.clear();
            let hay = Utf32Str::new(sub, &mut self.utf32_buf);
            let s = self
                .pattern
                .indices(hay, &mut self.matcher, &mut self.indices_buf);
            if s.is_some() {
                subtitle_indices = dedup_indices(&mut self.indices_buf);
            }
            s
        } else {
            None
        };

        // 3. Score category, keywords, detail, and id (weighted 1x).
        let aux_raw = [
            item.category.as_deref(),
            item.keywords.as_deref(),
            item.detail.as_deref(),
            Some(item.id.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter_map(|field| {
            let hay = Utf32Str::new(field, &mut self.utf32_buf);
            self.pattern.score(hay, &mut self.matcher)
        })
        .max();

        let mut composite = title_raw
            .map(|s| s.saturating_mul(4))
            .max(subtitle_raw.map(|s| s.saturating_mul(2)))
            .max(aux_raw);

        // 4. Cross-field multi-atom matching & per-atom title boost (e.g. `"t3 settle"`
        // matching category `"T3 Code"` + title `"Settle Thread"`).
        if title_raw.is_none() && self.pattern.atoms.len() > 1 {
            if composite.is_none() {
                self.combined_buf.clear();
                self.combined_buf.push_str(&item.title);
                for field in [
                    item.subtitle.as_deref(),
                    item.category.as_deref(),
                    item.keywords.as_deref(),
                    item.detail.as_deref(),
                    Some(item.id.as_str()),
                ]
                .into_iter()
                .flatten()
                {
                    if !field.is_empty() {
                        self.combined_buf.push(' ');
                        self.combined_buf.push_str(field);
                    }
                }
                let combined_hay = Utf32Str::new(&self.combined_buf, &mut self.utf32_buf);
                composite = self.pattern.score(combined_hay, &mut self.matcher);
            }

            if let Some(base) = composite {
                let (title_atom_score, atom_title_indices) =
                    self.score_and_collect_atoms(&item.title);
                title_indices = atom_title_indices;
                if subtitle_raw.is_none()
                    && let Some(sub) = item.subtitle.as_deref()
                {
                    let (_, atom_sub_indices) = self.score_and_collect_atoms(sub);
                    subtitle_indices = atom_sub_indices;
                }
                composite = Some(base.saturating_add(title_atom_score.saturating_mul(3)));
            }
        }

        let mut score = composite?;

        // Exact & prefix bonuses on title for crisp command-palette ergonomics.
        if title_raw.is_some() {
            let lower_title = item.title.to_ascii_lowercase();
            if lower_title == lower_query {
                score = score.saturating_add(10_000);
            } else if lower_title.starts_with(lower_query) {
                score = score.saturating_add(2_500);
            }
        }
        score = score.saturating_add(item.mru_boost);

        Some(ScoredMatch {
            index,
            score,
            title_indices,
            subtitle_indices,
        })
    }

    fn score_and_collect_atoms(&mut self, field: &str) -> (u32, Vec<u32>) {
        self.indices_buf.clear();
        let hay = Utf32Str::new(field, &mut self.utf32_buf);
        let mut total = 0u32;
        for atom in &self.pattern.atoms {
            if let Some(s) = atom.indices(hay, &mut self.matcher, &mut self.indices_buf) {
                total = total.saturating_add(u32::from(s));
            }
        }
        (total, dedup_indices(&mut self.indices_buf))
    }
}

fn dedup_indices(buf: &mut Vec<u32>) -> Vec<u32> {
    buf.sort_unstable();
    buf.dedup();
    buf.clone()
}

/// Converts sorted Unicode char match indices into contiguous `(byte_range, is_highlighted)` spans
/// for GPU text shaping.
#[must_use]
pub fn highlight_spans(text: &str, char_indices: &[u32]) -> Vec<(Range<usize>, bool)> {
    if text.is_empty() {
        return Vec::new();
    }
    if char_indices.is_empty() {
        return vec![(0..text.len(), false)];
    }

    let mut spans: Vec<(Range<usize>, bool)> = Vec::new();
    let mut hi_cursor = 0usize;

    for (char_idx, (byte_idx, ch)) in text.char_indices().enumerate() {
        while hi_cursor < char_indices.len() && (char_indices[hi_cursor] as usize) < char_idx {
            hi_cursor += 1;
        }
        let is_hi =
            hi_cursor < char_indices.len() && (char_indices[hi_cursor] as usize) == char_idx;
        let byte_end = byte_idx + ch.len_utf8();

        match spans.last_mut() {
            Some((range, prev_hi)) if *prev_hi == is_hi => range.end = byte_end,
            _ => spans.push((byte_idx..byte_end, is_hi)),
        }
    }

    // Sanity invariant: spans partition `0..text.len()`.
    debug_assert_eq!(
        spans.last().map(|(r, _)| r.end).cmp(&Some(text.len())),
        Ordering::Equal
    );
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn empty_query_preserves_exact_server_order() {
        let mut matcher = PaletteMatcher::new();
        let items = vec![
            PaletteItem::new("z", "Zebra"),
            PaletteItem::new("a", "Alpha"),
            PaletteItem::new("m", "Middle"),
        ];
        let res = matcher.filter("   ", &items);
        let ids: Vec<&str> = res.iter().map(|m| items[m.index].id.as_str()).collect();
        assert_eq!(ids, vec!["z", "a", "m"]);
        assert!(
            res.iter()
                .all(|m| m.score == 0 && m.title_indices.is_empty())
        );
    }

    #[test]
    fn ranks_title_prefix_and_cross_field_queries() {
        let mut matcher = PaletteMatcher::new();
        let mut settle = PaletteItem::new("t3_settle", "Settle Thread");
        settle.category = Some("T3 Code".into());
        settle.keywords = Some("done close archive inbox".into());

        let mut split = PaletteItem::new("pane.split.right", "Split Pane Right");
        split.category = Some("Pane".into());

        let mut find = PaletteItem::new("t3_find", "Find Thread…");
        find.category = Some("T3 Code".into());
        find.subtitle = Some("Search every open, settled, and archived thread".into());

        let items = vec![split, find, settle];

        // Cross-field query: category `"t3"` + title `"settle"`
        let res = matcher.filter("t3 settle", &items);
        assert_eq!(items[res[0].index].id, "t3_settle");
        assert!(!res[0].title_indices.is_empty());

        // Highlight spans convert cleanly to byte runs
        let spans = highlight_spans(&items[res[0].index].title, &res[0].title_indices);
        let reconstructed: String = spans
            .iter()
            .map(|(r, _)| &items[res[0].index].title[r.clone()])
            .collect();
        assert_eq!(reconstructed, "Settle Thread");
        assert!(spans.iter().any(|(_, hi)| *hi));
    }

    #[test]
    fn scores_2000_rex_ui_pick_items_quickly() {
        let mut matcher = PaletteMatcher::new();
        let items: Vec<PaletteItem> = (0..2_000)
            .map(|i| {
                let mut item = PaletteItem::new(
                    format!("thread:{i}"),
                    if i == 1_337 {
                        "Fix Wayland Kitty Graphics Paste".to_owned()
                    } else {
                        format!("Thread item number {i} build logs")
                    },
                );
                item.subtitle = Some(format!("project-{} · Done", i % 12));
                item.icon = Some(IconId::Terminal);
                item
            })
            .collect();

        let start = Instant::now();
        let res = matcher.filter("kitty paste", &items);
        let elapsed = start.elapsed();

        assert!(!res.is_empty());
        assert_eq!(items[res[0].index].id, "thread:1337");
        // Generous upper bound for CI/debug builds while verifying fast linear scaling.
        assert!(
            elapsed.as_millis() < 250,
            "filtering 2,000 items took {elapsed:?}"
        );
    }
}
