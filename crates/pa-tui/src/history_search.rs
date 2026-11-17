//! The prompt-history search (`app.history.search`, default ctrl+r; omp's `History` overlay in
//! the inline-menu style every picker here shares): the bordered search field over `›`-marker
//! rows of this session's past prompts, newest first. Every query word must appear in a row (case
//! folded); the matched words render in accent, and each row's age sits flush right (`now`,
//! `32m`, `5h`, `3d`). Enter puts the picked prompt in the composer; Esc closes with the draft
//! untouched.

use crate::editor::HistoryEntry;
use crate::keybindings::{format_key_text, KeybindingsManager};
use crate::menu_panel::{
    hint_row, menu_row, no_match_row, scroll_row, scrub_controls, search_field_lines, MenuSegment,
};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};

/// The search field's placeholder.
const SEARCH_PLACEHOLDER: &str = "Search prompt history";
/// How many rows the list shows before it scrolls (omp's `MAX_VISIBLE`).
const VISIBLE_ROWS: usize = 10;

/// One key press while the search is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistorySearchAction {
    /// Enter on a row: the caller puts this prompt in the composer.
    Select(String),
    /// Esc or Ctrl+C: close, the draft untouched.
    Cancel,
    /// Navigation or query editing only.
    None,
}

/// The open search: the query, the history it was opened over (newest first), and the
/// filtered selection.
#[derive(Debug, Clone)]
pub(crate) struct HistorySearch {
    search: SearchInput,
    entries: Vec<HistoryEntry>,
    /// Indices into `entries` that match the query, newest first.
    filtered: Vec<usize>,
    selected: usize,
}

/// The query's words, case folded; an empty query matches everything.
fn query_tokens(query: &str) -> Vec<Vec<char>> {
    query
        .split_whitespace()
        .map(|word| {
            word.chars()
                .map(|ch| ch.to_lowercase().next().unwrap_or(ch))
                .collect()
        })
        .collect()
}

/// Which characters of `text` the query words cover (case folded), or `None` when a word does
/// not appear at all — the filter and the row highlight read the same marks.
fn token_marks(text: &[char], tokens: &[Vec<char>]) -> Option<Vec<bool>> {
    let folded: Vec<char> = text
        .iter()
        .map(|ch| ch.to_lowercase().next().unwrap_or(*ch))
        .collect();
    let mut marks = vec![false; text.len()];
    for token in tokens {
        let mut found = false;
        for start in 0..folded.len().saturating_sub(token.len() - 1) {
            if folded[start..start + token.len()] == token[..] {
                marks[start..start + token.len()].fill(true);
                found = true;
            }
        }
        if !found {
            return None;
        }
    }
    Some(marks)
}

/// A prompt as one row: its lines joined by spaces, control characters scrubbed.
fn row_chars(text: &str) -> Vec<char> {
    scrub_controls(&text.split_whitespace().collect::<Vec<_>>().join(" "))
        .chars()
        .collect()
}

impl HistorySearch {
    /// Open over the editor's history (newest first), every entry listed.
    #[must_use]
    pub(crate) fn new(entries: &[HistoryEntry]) -> Self {
        let mut search = HistorySearch {
            search: SearchInput::new(),
            entries: entries.to_vec(),
            filtered: Vec::new(),
            selected: 0,
        };
        search.refilter();
        search
    }

    fn refilter(&mut self) {
        let tokens = query_tokens(self.search.value());
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| token_marks(&row_chars(&entry.text), &tokens).is_some())
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }

    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> HistorySearchAction {
        if key == "ctrl+c" || kb.matches(key, "tui.select.cancel") {
            return HistorySearchAction::Cancel;
        }
        if kb.matches(key, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
            return HistorySearchAction::None;
        }
        if kb.matches(key, "tui.select.down") {
            self.selected = (self.selected + 1).min(self.filtered.len().saturating_sub(1));
            return HistorySearchAction::None;
        }
        if kb.matches(key, "tui.select.confirm") {
            return match self
                .filtered
                .get(self.selected)
                .and_then(|index| self.entries.get(*index))
            {
                Some(entry) => HistorySearchAction::Select(entry.text.clone()),
                None => HistorySearchAction::None,
            };
        }
        let previous = self.search.value().to_string();
        self.search.handle_key(key, kb);
        if self.search.value() != previous {
            self.refilter();
        }
        HistorySearchAction::None
    }

    pub(crate) fn paste(&mut self, text: &str) {
        let previous = self.search.value().to_string();
        self.search.paste(text);
        if self.search.value() != previous {
            self.refilter();
        }
    }

    /// The frame: the search field, the visible window of rows with the matched words in accent
    /// and the age flush right, the scroll position, the empty state, and the key hint. `now`
    /// anchors the ages.
    pub(crate) fn render(
        &self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        now: std::time::SystemTime,
    ) -> Vec<Line> {
        let mut lines = search_field_lines(
            theme,
            width,
            self.search.value(),
            self.search.cursor(),
            true,
            SEARCH_PLACEHOLDER,
        );
        let tokens = query_tokens(self.search.value());
        let start = self
            .selected
            .saturating_sub(VISIBLE_ROWS / 2)
            .min(self.filtered.len().saturating_sub(VISIBLE_ROWS));
        let end = (start + VISIBLE_ROWS).min(self.filtered.len());
        for (index, entry) in self.filtered[start..end]
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .enumerate()
        {
            let chars = row_chars(&entry.text);
            let marks = token_marks(&chars, &tokens).unwrap_or_default();
            // One span per run of matched or unmatched characters.
            let mut primary: Line = Vec::new();
            let mut run = String::new();
            for (at, ch) in chars.iter().enumerate() {
                run.push(*ch);
                let matched = marks.get(at).copied().unwrap_or_default();
                if marks.get(at + 1).copied().unwrap_or_default() != matched
                    || at + 1 == chars.len()
                {
                    let text = std::mem::take(&mut run);
                    primary.push(if matched {
                        theme.fg_span(ThemeColor::Accent, text)
                    } else {
                        Span::raw(text)
                    });
                }
            }
            let secs = now.duration_since(entry.at).unwrap_or_default().as_secs();
            let age = match secs {
                0..60 => "now".to_string(),
                60..3_600 => format!("{}m", secs / 60),
                3_600..86_400 => format!("{}h", secs / 3_600),
                _ => format!("{}d", secs / 86_400),
            };
            lines.push(menu_row(
                theme,
                width,
                primary,
                &[MenuSegment::muted(&age)],
                start + index == self.selected,
            ));
        }
        if start > 0 || end < self.filtered.len() {
            lines.push(scroll_row(
                theme,
                width,
                self.selected + 1,
                self.filtered.len(),
            ));
        }
        if self.filtered.is_empty() {
            let message = if self.entries.is_empty() {
                "No history yet"
            } else {
                "No matching history"
            };
            lines.push(no_match_row(theme, width, message));
        }
        let key = |binding: &str, fallback: &str| {
            kb.first_key(binding)
                .map_or_else(|| fallback.to_string(), |key| format_key_text(&key))
        };
        let hint = format!(
            "{}/{} navigate \u{b7} {} select \u{b7} {} cancel",
            key("tui.select.up", "\u{2191}"),
            key("tui.select.down", "\u{2193}"),
            key("tui.select.confirm", "Enter"),
            key("tui.select.cancel", "Esc"),
        );
        lines.push(hint_row(theme, width, &hint));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorMode;
    use std::time::{Duration, SystemTime};

    fn entry(text: &str, at: SystemTime) -> HistoryEntry {
        HistoryEntry {
            text: text.to_string(),
            at,
        }
    }

    /// The query narrows to the rows holding every word; the selected row's matched words are
    /// accent, and the ages read `now`, minutes, and days.
    #[test]
    fn the_query_filters_highlights_and_ages_the_rows() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 86_400);
        let kb = KeybindingsManager::default();
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        let mut search = HistorySearch::new(&[
            entry("fix the parser tests", now - Duration::from_secs(5)),
            entry("explain the tokenizer", now - Duration::from_secs(32 * 60)),
            entry(
                "Parser\nrefactor plan",
                now - Duration::from_secs(3 * 86_400),
            ),
        ]);
        for key in ["p", "a", "r", "s", "e", "r"] {
            assert_eq!(search.handle_key(key, &kb), HistorySearchAction::None);
        }
        let rows = search.render(&theme, 44, &kb, now);
        let text = |row: &Line| row.iter().map(|span| &*span.content).collect::<String>();
        assert_eq!(
            rows[3..6].iter().map(text).collect::<Vec<_>>(),
            vec![
                "\u{203a} fix the parser tests                   now".to_string(),
                "  Parser refactor plan                    3d".to_string(),
                " \u{2191}/\u{2193} navigate \u{b7} Enter select \u{b7} Esc cancel".to_string(),
            ]
        );
        let accent = theme.fg_style(ThemeColor::Accent);
        let lit: Vec<&str> = rows[3]
            .iter()
            .chain(&rows[4])
            .filter(|span| span.style.fg == accent.fg)
            .map(|span| &*span.content)
            .collect();
        assert_eq!(lit, vec!["parser", "Parser"]);
        assert_eq!(
            search.handle_key("enter", &kb),
            HistorySearchAction::Select("fix the parser tests".to_string())
        );
        let unfiltered = HistorySearch::new(&[entry("x", now - Duration::from_secs(32 * 60))]);
        assert!(text(&unfiltered.render(&theme, 40, &kb, now)[3]).ends_with("32m"));
    }
}
