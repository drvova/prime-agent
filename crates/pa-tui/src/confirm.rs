//! The yes/no confirm selector: the shared menu grammar over the title,
//! the message as its description lines, and a small option list that
//! answers the pending question.

use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{title_row, EscVerb, Footer, HintRank, CHROME_INDENT};
use crate::menu_panel::menu_row;
use crate::theme::{Theme, ThemeColor};
use crate::width::wrap_text;
use crate::Line;

/// One answer to the pending question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Enter on an option: its label.
    Select(String),
    /// Escape or ctrl+c: the question was declined.
    Cancel,
    /// Navigation only.
    None,
    /// A printable key no option starts with: the panel has no search field to type into, so it
    /// closes as declined and the key goes to the composer instead of being swallowed.
    TypeThrough(String),
}

/// A pending confirm. Owns the editor dock while open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmPanel {
    title: String,
    message: Vec<String>,
    options: Vec<String>,
    selected: usize,
}

impl ConfirmPanel {
    /// The Yes/No confirm of a pending question (`title`, `message`). The selection opens on
    /// `No`: a stray Enter never runs the change the question guards.
    pub fn yes_no(title: &str, message: &str) -> Self {
        ConfirmPanel {
            title: title.to_string(),
            message: message
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
            options: vec!["Yes".to_string(), "No".to_string()],
            selected: 1,
        }
    }

    /// One key id: up/down move, confirm selects, escape and ctrl+c
    /// cancel, and a printable key no option starts with types through.
    pub fn handle_key(&mut self, kb: &KeybindingsManager, id: &str) -> ConfirmAction {
        if id == "ctrl+c" {
            return ConfirmAction::Cancel;
        }
        if kb.matches(id, "tui.select.cancel") {
            return ConfirmAction::Cancel;
        }
        if kb.matches(id, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
            return ConfirmAction::None;
        }
        if kb.matches(id, "tui.select.down") {
            self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
            return ConfirmAction::None;
        }
        if kb.matches(id, "tui.select.confirm") {
            return ConfirmAction::Select(
                self.options.get(self.selected).cloned().unwrap_or_default(),
            );
        }
        let mut chars = id.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            let starts_an_option = self.options.iter().any(|option| {
                option
                    .chars()
                    .next()
                    .is_some_and(|first| first.to_lowercase().eq(ch.to_lowercase()))
            });
            if !ch.is_control() && !ch.is_whitespace() && !starts_an_option {
                return ConfirmAction::TypeThrough(id.to_string());
            }
        }
        ConfirmAction::None
    }

    /// The pane's rendered rows.
    #[must_use]
    pub fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Vec::new());
        lines.push(title_row(theme, width, &self.title));
        // The question wraps: a confirm the user cannot read to its end cannot be answered.
        let muted = theme.fg_style(ThemeColor::Muted);
        for line in &self.message {
            for wrapped in wrap_text(line, width.saturating_sub(CHROME_INDENT.len())) {
                let mut row = vec![crate::Span::raw(CHROME_INDENT)];
                row.extend(
                    wrapped
                        .into_iter()
                        .map(|span| crate::Span::styled(span.content, muted)),
                );
                lines.push(row);
            }
        }
        lines.push(Vec::new());
        for (index, option) in self.options.iter().enumerate() {
            lines.push(menu_row(
                theme,
                width,
                vec![crate::Span::raw(option.clone())],
                &[],
                index == self.selected,
            ));
        }
        lines.push(Vec::new());
        lines.push(
            Footer::new()
                .navigate(kb)
                .bound(kb, &["tui.select.confirm"], "select", HintRank::Core)
                .esc(kb, EscVerb::Close)
                .line(theme, width),
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

    /// A question longer than the pane wraps under the title: every word of what the user is
    /// asked to confirm stays readable, at the chrome indent.
    #[test]
    fn a_long_question_wraps_instead_of_being_cut() {
        let panel = ConfirmPanel::yes_no(
            "Import session",
            "Replace the current session with the imported one?",
        );
        let theme = Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
        let rows: Vec<String> = panel
            .render(&theme, 30, &kb())
            .iter()
            .map(|line| line.iter().map(|span| span.content.as_str()).collect())
            .collect();
        assert_eq!(
            rows[1..4],
            [
                "  Import session",
                "  Replace the current session",
                "  with the imported one?",
            ]
            .map(str::to_string)
        );
    }

    /// The confirm opens on `No`: Enter straight away declines.
    #[test]
    fn yes_no_defaults_to_no() {
        let mut panel = ConfirmPanel::yes_no("Import session", "Replace current session?");
        assert_eq!(
            panel.handle_key(&kb(), "enter"),
            ConfirmAction::Select("No".to_string())
        );
    }

    #[test]
    fn yes_no_moves_and_selects() {
        let mut panel = ConfirmPanel::yes_no("Import session", "Replace current session?");
        assert_eq!(panel.handle_key(&kb(), "up"), ConfirmAction::None);
        assert_eq!(
            panel.handle_key(&kb(), "enter"),
            ConfirmAction::Select("Yes".to_string())
        );
    }

    /// A letter that starts no option falls through to the composer; one that starts an option
    /// stays with the panel.
    #[test]
    fn a_letter_no_option_starts_with_types_through() {
        let mut panel = ConfirmPanel::yes_no("Import session", "Replace current session?");
        assert_eq!(
            [
                panel.handle_key(&kb(), "h"),
                panel.handle_key(&kb(), "Y"),
                panel.handle_key(&kb(), "n"),
            ],
            [
                ConfirmAction::TypeThrough("h".to_string()),
                ConfirmAction::None,
                ConfirmAction::None,
            ]
        );
    }

    #[test]
    fn escape_and_ctrl_c_cancel() {
        let mut panel = ConfirmPanel::yes_no("t", "m");
        assert_eq!(panel.handle_key(&kb(), "escape"), ConfirmAction::Cancel);
        assert_eq!(panel.handle_key(&kb(), "ctrl+c"), ConfirmAction::Cancel);
    }

    #[test]
    fn the_message_lines_render() {
        let theme = crate::theme::Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
        let panel = ConfirmPanel::yes_no(
            "Session cwd not found",
            "cwd from session file does not exist\n/old/path\n\ncontinue in current cwd\n/new/path",
        );
        let rows = panel.render(&theme, 80, &kb());
        let text = rows
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(text.iter().any(|row| row.contains("Session cwd not found")));
        assert!(text
            .iter()
            .any(|row| row.contains("continue in current cwd")));
        assert!(text.iter().any(|row| row.starts_with("  Yes")));
        assert!(text.iter().any(|row| row.starts_with("› No")));
        assert!(text
            .iter()
            .any(|row| row.contains("↑/↓ navigate · Enter select · Esc close")));
    }
}
