//! The inline answer prompt for a kernel `ask(...)` question (`question_open`): it replaces the
//! composer while the question waits, rendered in the shared menu grammar — the question as the
//! title, the choices as menu rows (`( )`/`(•)` for one choice, `[ ]`/`[x]` for several) or a
//! bordered text field, and the key footer. Up/Down move, Space toggles a multi-choice row,
//! Enter answers, Esc declines; every key resolves through the keybinding registry. Ctrl+C is not
//! the prompt's: it passes through to the app, which interrupts the turn (and so withdraws the
//! question).

use pa_types::question::{Question, QuestionKind, QuestionReply};

use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{Footer, HintRank, CHROME_INDENT};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::width::wrap_text;
use crate::{Line, Span};

/// What one key did to the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuestionAction {
    /// Send this reply for the open question.
    Reply(QuestionReply),
    /// The key moved the selection or edited the text.
    Handled,
    /// Not the prompt's key: the app handles it (Ctrl+C interrupts the turn).
    PassThrough,
}

/// One open question's answer state.
#[derive(Debug, Clone)]
pub(crate) struct QuestionPrompt {
    pub(crate) question: Question,
    selected: usize,
    checked: Vec<bool>,
    input: SearchInput,
}

impl QuestionPrompt {
    /// Open the prompt on the question's default: the default choice selected (or checked), the
    /// default text prefilled.
    #[must_use]
    pub(crate) fn new(question: Question) -> Self {
        let checked: Vec<bool> = question
            .choices
            .iter()
            .map(|choice| question.default.contains(choice))
            .collect();
        let selected = checked.iter().position(|checked| *checked).unwrap_or(0);
        let mut input = SearchInput::new();
        if question.kind == QuestionKind::Text {
            if let Some(default) = question.default.first() {
                input.prefill(default);
            }
        }
        Self {
            question,
            selected,
            checked,
            input,
        }
    }

    /// One bracketed paste: the text field takes it.
    pub(crate) fn paste(&mut self, text: &str) {
        if self.question.kind == QuestionKind::Text {
            self.input.paste(text);
        }
    }

    /// One key id.
    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> QuestionAction {
        if kb.matches(key, "app.clear") || kb.matches(key, "app.interrupt") {
            return QuestionAction::PassThrough;
        }
        if kb.matches(key, "tui.select.cancel") {
            return QuestionAction::Reply(QuestionReply::Declined);
        }
        if kb.matches(key, "tui.select.confirm") {
            let answer = match self.question.kind {
                QuestionKind::Text => {
                    let text = self.input.value().trim();
                    if text.is_empty() {
                        return QuestionAction::Handled;
                    }
                    vec![text.to_string()]
                }
                QuestionKind::Single => self
                    .question
                    .choices
                    .get(self.selected)
                    .cloned()
                    .into_iter()
                    .collect(),
                QuestionKind::Multi => self
                    .question
                    .choices
                    .iter()
                    .zip(&self.checked)
                    .filter(|(_, checked)| **checked)
                    .map(|(choice, _)| choice.clone())
                    .collect(),
            };
            return QuestionAction::Reply(QuestionReply::Answered { answer });
        }
        match self.question.kind {
            QuestionKind::Text => self.input.handle_key(key, kb),
            QuestionKind::Single | QuestionKind::Multi => {
                let count = self.question.choices.len();
                if kb.matches(key, "tui.select.up") {
                    self.selected = (self.selected + count - 1) % count.max(1);
                } else if kb.matches(key, "tui.select.down") {
                    self.selected = (self.selected + 1) % count.max(1);
                } else if self.question.kind == QuestionKind::Multi
                    && kb.matches(key, "app.question.toggle")
                {
                    if let Some(checked) = self.checked.get_mut(self.selected) {
                        *checked = !*checked;
                    }
                } else if key.starts_with("ctrl+") || key.starts_with("alt+") {
                    // A chord the choice list does not use stays the app's (Ctrl+O, the
                    // viewport keys); plain keys stop here so nothing types into the hidden
                    // composer.
                    return QuestionAction::PassThrough;
                }
            }
        }
        QuestionAction::Handled
    }

    /// The prompt's rows at `width`.
    #[must_use]
    pub(crate) fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let mut lines: Vec<Line> = vec![Vec::new()];
        let text_width = width.saturating_sub(CHROME_INDENT.len() + 2).max(1);
        for (index, row) in wrap_text(&self.question.text, text_width)
            .into_iter()
            .enumerate()
        {
            let mut line = vec![
                Span::raw(CHROME_INDENT),
                theme.fg_span(ThemeColor::Accent, if index == 0 { "? " } else { "  " }),
            ];
            line.extend(
                row.into_iter()
                    .map(|span| theme.fg_span(ThemeColor::Text, span.content)),
            );
            lines.push(line);
        }
        lines.push(Vec::new());
        match self.question.kind {
            QuestionKind::Text => lines.extend(crate::menu_panel::search_field_lines(
                theme,
                width,
                self.input.value(),
                self.input.cursor(),
                true,
                "Type your answer",
            )),
            QuestionKind::Single | QuestionKind::Multi => {
                for (index, choice) in self.question.choices.iter().enumerate() {
                    // A multi-choice row shows its checkbox; a single-choice row its radio,
                    // filled at the selection (Enter answers the selected row).
                    let on = match self.question.kind {
                        QuestionKind::Multi => self.checked.get(index).copied().unwrap_or(false),
                        QuestionKind::Single | QuestionKind::Text => index == self.selected,
                    };
                    let mark = match (self.question.kind, on) {
                        (QuestionKind::Multi, true) => "[x]",
                        (QuestionKind::Multi, false) => "[ ]",
                        (QuestionKind::Single | QuestionKind::Text, true) => "(\u{2022})",
                        (QuestionKind::Single | QuestionKind::Text, false) => "( )",
                    };
                    let primary: Line = vec![
                        theme.fg_span(
                            if on {
                                ThemeColor::Success
                            } else {
                                ThemeColor::Dim
                            },
                            mark,
                        ),
                        Span::raw(" "),
                        Span::raw(choice.clone()),
                    ];
                    lines.push(crate::menu_panel::menu_row(
                        theme,
                        width,
                        primary,
                        &[],
                        index == self.selected,
                    ));
                }
            }
        }
        if let Some(seconds) = self.question.timeout_seconds {
            let fallback = if self.question.default.is_empty() {
                "no answer".to_string()
            } else {
                self.question.default.join(", ")
            };
            lines.push(crate::width::truncate_line(
                &vec![
                    Span::raw(CHROME_INDENT),
                    theme.fg_span(
                        ThemeColor::Dim,
                        format!("After {seconds}s the agent continues with: {fallback}"),
                    ),
                ],
                width,
                "\u{2026}",
            ));
        }
        lines.push(Vec::new());
        let footer = match self.question.kind {
            QuestionKind::Text => Footer::new(),
            QuestionKind::Single => Footer::new().bound(
                kb,
                &["tui.select.up", "tui.select.down"],
                "choose",
                HintRank::Useful,
            ),
            QuestionKind::Multi => Footer::new()
                .bound(
                    kb,
                    &["tui.select.up", "tui.select.down"],
                    "move",
                    HintRank::Useful,
                )
                .bound(kb, &["app.question.toggle"], "toggle", HintRank::Core),
        };
        lines.extend(
            footer
                .bound(kb, &["tui.select.confirm"], "answer", HintRank::Core)
                .bound(kb, &["tui.select.cancel"], "decline", HintRank::Core)
                .rows(theme, width),
        );
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorMode;

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    fn question(kind: QuestionKind, choices: &[&str], default: &[&str]) -> Question {
        Question {
            id: "q1".to_string(),
            text: "Which store should the session cache use?".to_string(),
            kind,
            choices: choices.iter().map(ToString::to_string).collect(),
            default: default.iter().map(ToString::to_string).collect(),
            timeout_seconds: None,
        }
    }

    fn rows(prompt: &QuestionPrompt, width: usize) -> Vec<String> {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        prompt
            .render(&theme, width, &kb())
            .iter()
            .map(|line| line.iter().map(|span| span.content.as_str()).collect())
            .collect()
    }

    /// One choice: the default is the selected radio; Down moves, Enter answers the selected row,
    /// Esc declines, and Ctrl+C is left to the app (it interrupts the turn).
    #[test]
    fn a_single_choice_question_renders_radios_and_answers_the_selected_row() {
        let mut prompt = QuestionPrompt::new(question(
            QuestionKind::Single,
            &["sqlite", "redis"],
            &["sqlite"],
        ));
        let pad = |row: &str| format!("{row:<60}");
        assert_eq!(
            rows(&prompt, 60),
            vec![
                String::new(),
                "  ? Which store should the session cache use?".to_string(),
                String::new(),
                pad("\u{203a} (\u{2022}) sqlite"),
                pad("  ( ) redis"),
                String::new(),
                "  \u{2191}/\u{2193} choose \u{b7} Enter answer \u{b7} Esc decline".to_string(),
            ]
        );
        assert_eq!(prompt.handle_key("down", &kb()), QuestionAction::Handled);
        assert_eq!(
            prompt.handle_key("enter", &kb()),
            QuestionAction::Reply(QuestionReply::Answered {
                answer: vec!["redis".to_string()]
            })
        );
        assert_eq!(
            prompt.handle_key("escape", &kb()),
            QuestionAction::Reply(QuestionReply::Declined)
        );
        assert_eq!(
            prompt.handle_key("ctrl+c", &kb()),
            QuestionAction::PassThrough
        );
        // App chords (Ctrl+O cycles the transcript detail) stay the app's; plain keys stop here.
        assert_eq!(
            prompt.handle_key("ctrl+o", &kb()),
            QuestionAction::PassThrough
        );
        assert_eq!(prompt.handle_key("x", &kb()), QuestionAction::Handled);
    }

    /// Several choices: Space toggles the selected row, Enter answers every checked row; the
    /// timeout's fallback is spelled out.
    #[test]
    fn a_multi_choice_question_toggles_with_space() {
        let mut prompt =
            QuestionPrompt::new(question(QuestionKind::Multi, &["a", "b", "c"], &["b"]));
        prompt.question.timeout_seconds = Some(30);
        let pad = |row: &str| format!("{row:<60}");
        assert_eq!(
            rows(&prompt, 60),
            vec![
                String::new(),
                "  ? Which store should the session cache use?".to_string(),
                String::new(),
                pad("  [ ] a"),
                pad("\u{203a} [x] b"),
                pad("  [ ] c"),
                "  After 30s the agent continues with: b".to_string(),
                String::new(),
                "  \u{2191}/\u{2193} move \u{b7} Space toggle \u{b7} Enter answer \u{b7} Esc decline"
                    .to_string(),
            ]
        );
        prompt.handle_key("down", &kb());
        prompt.handle_key("space", &kb());
        assert_eq!(
            prompt.handle_key("enter", &kb()),
            QuestionAction::Reply(QuestionReply::Answered {
                answer: vec!["b".to_string(), "c".to_string()]
            })
        );
    }

    /// Free text: the default prefills the field, typing edits it, Enter answers the text (an
    /// empty field answers nothing).
    #[test]
    fn a_text_question_takes_typed_text() {
        let mut prompt = QuestionPrompt::new(question(QuestionKind::Text, &[], &["keep going"]));
        let rule = "\u{2500}".repeat(40);
        assert_eq!(
            rows(&prompt, 40),
            vec![
                String::new(),
                "  ? Which store should the session cache".to_string(),
                "    use?".to_string(),
                String::new(),
                rule.clone(),
                format!("{:<40}", " > keep going"),
                rule,
                String::new(),
                "  Enter answer \u{b7} Esc decline".to_string(),
            ]
        );
        prompt.handle_key("space", &kb());
        prompt.handle_key("!", &kb());
        assert_eq!(
            prompt.handle_key("enter", &kb()),
            QuestionAction::Reply(QuestionReply::Answered {
                answer: vec!["keep going !".to_string()]
            })
        );
        let mut empty = QuestionPrompt::new(question(QuestionKind::Text, &[], &[]));
        assert_eq!(empty.handle_key("enter", &kb()), QuestionAction::Handled);
    }
}
