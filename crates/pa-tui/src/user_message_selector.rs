//! The `/fork` user-message selector, rendered through the shared menu
//! grammar.

use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{empty_state_row, title_row, EscVerb, Footer, HintRank, CHROME_INDENT};
use crate::menu_panel::{menu_row, scroll_row, search_field_lines};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};

/// One forkable user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessageItem {
    pub id: String,
    pub text: String,
}

/// What a key press did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMessageSelectorAction {
    /// Enter on a message: fork from its entry id.
    Select(String),
    /// Escape: close.
    Cancel,
    None,
}

const MAX_VISIBLE: usize = 10;

/// The `/fork` selector state: the messages, the search filter over them, and the selection
/// within the filtered view.
pub struct UserMessageSelector {
    messages: Vec<UserMessageItem>,
    search: SearchInput,
    /// The message indices the query keeps, oldest first.
    filtered: Vec<usize>,
    /// The selection's position in `filtered`.
    selected: usize,
}

impl UserMessageSelector {
    /// Build over the `get_user_messages_for_forking` response; the latest
    /// message is preselected (TS `initialSelectedId` fallback).
    #[must_use]
    pub fn new(messages: Vec<UserMessageItem>) -> Self {
        let filtered: Vec<usize> = (0..messages.len()).collect();
        let selected = filtered.len().saturating_sub(1);
        UserMessageSelector {
            messages,
            search: SearchInput::new(),
            filtered,
            selected,
        }
    }

    /// True when there is nothing to fork from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The search query.
    #[must_use]
    pub fn query(&self) -> &str {
        self.search.value()
    }

    /// Up/Down move within the filtered list, Enter forks from the selection, Esc clears a
    /// non-empty query before it closes; every other key edits the query.
    pub fn handle_key(&mut self, kb: &KeybindingsManager, id: &str) -> UserMessageSelectorAction {
        if kb.matches(id, "tui.select.cancel") {
            if self.search.value().is_empty() {
                return UserMessageSelectorAction::Cancel;
            }
            self.search.prefill("");
            self.refilter();
            return UserMessageSelectorAction::None;
        }
        let count = self.filtered.len();
        if kb.matches(id, "tui.select.up") {
            if count > 0 {
                self.selected = (self.selected + count - 1) % count;
            }
            UserMessageSelectorAction::None
        } else if kb.matches(id, "tui.select.down") {
            if count > 0 {
                self.selected = (self.selected + 1) % count;
            }
            UserMessageSelectorAction::None
        } else if kb.matches(id, "tui.select.confirm") {
            self.filtered
                .get(self.selected)
                .map_or(UserMessageSelectorAction::None, |&index| {
                    UserMessageSelectorAction::Select(self.messages[index].id.clone())
                })
        } else {
            let previous = self.search.value().to_string();
            self.search.handle_key(id, kb);
            if self.search.value() != previous {
                self.refilter();
            }
            UserMessageSelectorAction::None
        }
    }

    /// Keep the messages containing every query word (case-insensitive), in session order; the
    /// selection lands on the newest match.
    fn refilter(&mut self) {
        let words: Vec<String> = self
            .search
            .value()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.filtered = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                let text = message.text.to_lowercase();
                words.iter().all(|word| text.contains(word.as_str()))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.filtered.len().saturating_sub(1);
    }

    /// The full pane: title and description rows, the search field, then the shared menu list
    /// (one row per message, the scroll row), the Enter line, and the footer.
    #[must_use]
    pub fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let mut lines: Vec<Line> = vec![
            title_row(theme, width, "Fork"),
            crate::width::truncate_line(
                &vec![
                    Span::raw(CHROME_INDENT),
                    theme.fg_span(
                        ThemeColor::Muted,
                        "Copy the active path up to a message into a new session",
                    ),
                ],
                width,
                crate::menu_chrome::ELLIPSIS,
            ),
        ];
        lines.extend(search_field_lines(
            theme,
            width,
            self.search.value(),
            self.search.cursor(),
            /*focused*/ true,
            "Search messages",
        ));
        if self.filtered.is_empty() {
            lines.push(empty_state_row(
                theme,
                width,
                kb,
                self.search.value(),
                "No user messages found",
            ));
        } else {
            let count = self.filtered.len();
            let start = self
                .selected
                .saturating_sub(MAX_VISIBLE / 2)
                .min(count.saturating_sub(MAX_VISIBLE));
            let end = (start + MAX_VISIBLE).min(count);
            for position in start..end {
                let normalized = self.messages[self.filtered[position]]
                    .text
                    .replace('\n', " ")
                    .trim()
                    .to_string();
                lines.push(menu_row(
                    theme,
                    width,
                    vec![Span::raw(normalized)],
                    &[],
                    position == self.selected,
                ));
            }
            if start > 0 || end < count {
                lines.push(scroll_row(theme, width, self.selected + 1, count));
            }
        }
        lines.push(Vec::new());
        lines.extend(
            Footer::new()
                .enter(
                    kb,
                    (!self.filtered.is_empty()).then_some("forks from this message"),
                )
                .navigate(kb)
                .bound(kb, &["tui.select.confirm"], "fork", HintRank::Core)
                .esc(kb, EscVerb::Close)
                .rows(theme, width),
        );
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    fn theme() -> Theme {
        Theme::builtin("prime", crate::theme::ColorMode::TrueColor)
    }

    fn messages() -> Vec<UserMessageItem> {
        (0..12)
            .map(|index| UserMessageItem {
                id: format!("entry-{index}"),
                text: format!("message {index}"),
            })
            .collect()
    }

    fn row_text(line: &Line) -> String {
        line.iter().map(|span| span.content.as_str()).collect()
    }

    #[test]
    fn escape_cancels_and_enter_selects() {
        let mut selector = UserMessageSelector::new(messages());
        assert_eq!(
            selector.handle_key(&kb(), "escape"),
            UserMessageSelectorAction::Cancel
        );
        assert_eq!(
            selector.handle_key(&kb(), "enter"),
            UserMessageSelectorAction::Select("entry-11".to_string())
        );
    }

    #[test]
    fn navigation_wraps_around_the_list() {
        let mut selector = UserMessageSelector::new(messages());
        for _ in 0..12 {
            selector.handle_key(&kb(), "up");
        }
        assert_eq!(
            selector.handle_key(&kb(), "enter"),
            UserMessageSelectorAction::Select("entry-11".to_string())
        );
        selector.handle_key(&kb(), "down");
        assert_eq!(
            selector.handle_key(&kb(), "enter"),
            UserMessageSelectorAction::Select("entry-0".to_string())
        );
    }

    #[test]
    fn the_pane_renders_through_the_shared_menu_grammar() {
        let selector = UserMessageSelector::new(messages());
        let lines = selector.render(&theme(), 80, &kb());
        let rendered: Vec<String> = lines.iter().map(row_text).collect();
        assert!(
            rendered
                .iter()
                .any(|row| row.starts_with("\u{203a} message 11")),
            "the preselected latest message carries the menu marker:\n{rendered:?}"
        );
        assert!(
            rendered.iter().any(|row| row.contains("(12/12)")),
            "the shared scroll row replaces the per-row metadata:\n{rendered:?}"
        );
        assert!(!rendered.iter().any(|row| row.contains("Message 1 of")));
        assert!(rendered
            .iter()
            .any(|row| row.contains("\u{2191}/\u{2193} navigate · Enter fork · Esc close")));
    }

    #[test]
    fn the_hint_omits_unbound_actions() {
        let mut bindings = crate::keybindings::KeybindingsConfig::new();
        bindings.insert("tui.select.cancel".to_string(), Vec::new());
        let kb = KeybindingsManager::with_user_bindings(bindings);
        let selector = UserMessageSelector::new(messages());
        let lines = selector.render(&theme(), 80, &kb);
        let rendered: Vec<String> = lines.iter().map(row_text).collect();
        let hint = rendered
            .iter()
            .find(|row| row.contains("navigate"))
            .expect("the hint row renders");
        assert!(
            hint.contains("Enter fork"),
            "the bound action stays: {hint}"
        );
        assert!(!hint.contains("close"), "the unbound action drops: {hint}");
    }

    /// Typing filters the messages (the newest match selected), a query that hides every message
    /// shows the no-match state, and Esc clears the query before it closes.
    #[test]
    fn typing_filters_and_esc_clears_before_closing() {
        let mut selector = UserMessageSelector::new(messages());
        for key in ["1", "0"] {
            selector.handle_key(&kb(), key);
        }
        assert_eq!(
            selector.handle_key(&kb(), "enter"),
            UserMessageSelectorAction::Select("entry-10".to_string())
        );
        for key in ["z", "z"] {
            selector.handle_key(&kb(), key);
        }
        let rendered: Vec<String> = selector
            .render(&theme(), 80, &kb())
            .iter()
            .map(row_text)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|row| row == "  no match for \u{201c}10zz\u{201d} \u{b7} esc clears"),
            "{rendered:?}"
        );
        assert_eq!(
            selector.handle_key(&kb(), "escape"),
            UserMessageSelectorAction::None
        );
        assert_eq!(selector.query(), "");
        assert_eq!(
            selector.handle_key(&kb(), "escape"),
            UserMessageSelectorAction::Cancel
        );
    }

    /// The Enter line names what Enter does for the selected message.
    #[test]
    fn the_enter_line_names_the_fork() {
        let rendered: Vec<String> = UserMessageSelector::new(messages())
            .render(&theme(), 80, &kb())
            .iter()
            .map(row_text)
            .collect();
        let enter = rendered
            .iter()
            .position(|row| row == "  enter forks from this message")
            .expect("the Enter line");
        assert!(rendered[enter + 1].contains("Enter fork"), "{rendered:?}");
    }

    #[test]
    fn an_empty_session_renders_the_shared_no_match_row() {
        let selector = UserMessageSelector::new(Vec::new());
        let lines = selector.render(&theme(), 80, &kb());
        let rendered: Vec<String> = lines.iter().map(row_text).collect();
        assert!(rendered
            .iter()
            .any(|row| row.contains("No user messages found")));
    }
}
