//! The inline answer panel for a kernel `ask(...)` question (`question_open`): it replaces the
//! composer while the question waits, rendered in the shared menu grammar — the question as the
//! title, the choices as numbered menu rows (`( )`/`(•)` for one choice, `[ ]`/`[x]` for
//! several), an `Other…` row when the question takes a typed answer (`allowOther`), or a
//! bordered text field for a free-text question, then the live timeout countdown and the key
//! footer.
//!
//! Keys (every one resolves through the keybinding registry, except the literal digit row):
//! Up/Down move; a digit picks its row (one choice: answers at once; several: toggles it);
//! `app.question.toggle` (Space) toggles a multi-choice row; Enter answers (on `Other…` it opens
//! the text field, where Enter answers the typed text and Esc goes back to the choices); Esc
//! declines. Ctrl+C is not the panel's: it passes through to the app, which interrupts the turn
//! (and so withdraws the question). A click on a row acts like its digit, except that a click
//! on a single-choice row selects it (Enter answers).

use pa_types::question::{Question, QuestionKind, QuestionReply};

use crate::keybindings::KeybindingsManager;
use crate::menu_chrome::{Footer, HintRank, CHROME_INDENT};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::width::wrap_text;
use crate::{Line, Span};

/// The `Other…` row's label.
const OTHER_LABEL: &str = "Other\u{2026}";
/// The typed-answer field's placeholder.
const TYPE_PLACEHOLDER: &str = "Type your answer";

/// What one key did to the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuestionAction {
    /// Send this reply for the open question.
    Reply(QuestionReply),
    /// The key moved the selection or edited the text.
    Handled,
    /// Not the panel's key: the app handles it (Ctrl+C interrupts the turn).
    PassThrough,
}

/// Whether the keys drive the choice rows or the text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Choosing,
    Typing,
}

/// One open question's answer state.
#[derive(Debug, Clone)]
pub(crate) struct QuestionPrompt {
    pub(crate) question: Question,
    /// The selected row: a choice index, or `choices.len()` for the `Other…` row.
    selected: usize,
    checked: Vec<bool>,
    input: SearchInput,
    entry: Entry,
}

impl QuestionPrompt {
    /// Open the panel on the question's default: the default choice selected (or checked), the
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
        let entry = match question.kind {
            QuestionKind::Text => {
                if let Some(default) = question.default.first() {
                    input.prefill(default);
                }
                Entry::Typing
            }
            QuestionKind::Single | QuestionKind::Multi => Entry::Choosing,
        };
        Self {
            question,
            selected,
            checked,
            input,
            entry,
        }
    }

    /// The selectable rows: the choices, plus `Other…` when the question takes a typed answer.
    fn row_count(&self) -> usize {
        self.question.choices.len() + usize::from(self.has_other_row())
    }

    fn has_other_row(&self) -> bool {
        self.question.allow_other && self.question.kind != QuestionKind::Text
    }

    /// One bracketed paste: the text field takes it.
    pub(crate) fn paste(&mut self, text: &str) {
        if self.entry == Entry::Typing {
            self.input.paste(text);
        }
    }

    /// One key id.
    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> QuestionAction {
        if kb.matches(key, "app.clear") || kb.matches(key, "app.interrupt") {
            return QuestionAction::PassThrough;
        }
        match self.entry {
            Entry::Typing => self.typing_key(key, kb),
            Entry::Choosing => self.choosing_key(key, kb),
        }
    }

    fn typing_key(&mut self, key: &str, kb: &KeybindingsManager) -> QuestionAction {
        let other = self.question.kind != QuestionKind::Text;
        if kb.matches(key, "tui.select.cancel") {
            if other {
                // Back to the choices; the typed text stays for a second try.
                self.entry = Entry::Choosing;
                return QuestionAction::Handled;
            }
            return QuestionAction::Reply(QuestionReply::Declined);
        }
        if kb.matches(key, "tui.select.confirm") {
            let text = self.input.value().trim();
            if text.is_empty() {
                return QuestionAction::Handled;
            }
            let mut answer = match self.question.kind {
                QuestionKind::Multi => self.checked_choices(),
                QuestionKind::Single | QuestionKind::Text => Vec::new(),
            };
            answer.push(text.to_string());
            return QuestionAction::Reply(QuestionReply::Answered { answer });
        }
        self.input.handle_key(key, kb);
        QuestionAction::Handled
    }

    fn choosing_key(&mut self, key: &str, kb: &KeybindingsManager) -> QuestionAction {
        if kb.matches(key, "tui.select.cancel") {
            return QuestionAction::Reply(QuestionReply::Declined);
        }
        let count = self.row_count().max(1);
        if kb.matches(key, "tui.select.confirm") {
            return self.confirm();
        }
        if kb.matches(key, "tui.select.up") {
            self.selected = (self.selected + count - 1) % count;
        } else if kb.matches(key, "tui.select.down") {
            self.selected = (self.selected + 1) % count;
        } else if self.question.kind == QuestionKind::Multi
            && kb.matches(key, "app.question.toggle")
        {
            return self.pick(self.selected, Pick::Toggle);
        } else if let Some(row) = digit_row(key).filter(|row| *row < count) {
            return self.pick(row, Pick::Commit);
        } else if key.starts_with("ctrl+") || key.starts_with("alt+") {
            // A chord the choice list does not use stays the app's (Ctrl+O, the viewport keys,
            // the todo tray toggle); plain keys stop here so nothing types into the hidden
            // composer.
            return QuestionAction::PassThrough;
        }
        QuestionAction::Handled
    }

    /// A click on item row `row` (the click surface's index): a digit's pick, except that a
    /// single-choice row only takes the selection.
    pub(crate) fn click_row(&mut self, row: usize) -> QuestionAction {
        if self.entry != Entry::Choosing || row >= self.row_count() {
            return QuestionAction::Handled;
        }
        self.pick(row, Pick::Toggle)
    }

    /// Select `row` and act on it: `Other…` opens the text field; a multi-choice row toggles;
    /// a single-choice row answers on [`Pick::Commit`] (a digit) and is only selected on
    /// [`Pick::Toggle`] (Space, a click).
    fn pick(&mut self, row: usize, pick: Pick) -> QuestionAction {
        self.selected = row;
        if row == self.question.choices.len() {
            self.entry = Entry::Typing;
            return QuestionAction::Handled;
        }
        match (self.question.kind, pick) {
            (QuestionKind::Multi, Pick::Commit | Pick::Toggle) => {
                if let Some(checked) = self.checked.get_mut(row) {
                    *checked = !*checked;
                }
                QuestionAction::Handled
            }
            (QuestionKind::Single, Pick::Commit) => self.confirm(),
            (QuestionKind::Single | QuestionKind::Text, Pick::Toggle)
            | (QuestionKind::Text, Pick::Commit) => QuestionAction::Handled,
        }
    }

    /// Enter on the choice rows: `Other…` opens the text field; otherwise the answer is the
    /// selected choice (one) or every checked choice (several).
    fn confirm(&mut self) -> QuestionAction {
        if self.selected == self.question.choices.len() && self.has_other_row() {
            self.entry = Entry::Typing;
            return QuestionAction::Handled;
        }
        let answer = match self.question.kind {
            QuestionKind::Multi => self.checked_choices(),
            QuestionKind::Single | QuestionKind::Text => self
                .question
                .choices
                .get(self.selected)
                .cloned()
                .into_iter()
                .collect(),
        };
        QuestionAction::Reply(QuestionReply::Answered { answer })
    }

    fn checked_choices(&self) -> Vec<String> {
        self.question
            .choices
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(choice, _)| choice.clone())
            .collect()
    }

    /// The rows above the first choice row in [`Self::render`] at `width` (the click
    /// surface's chrome rows).
    #[must_use]
    pub(crate) fn chrome_rows(&self, width: usize) -> usize {
        2 + wrap_text(&self.question.text, text_width(width)).len()
    }

    /// The selectable item rows [`Self::render`] draws (none while the text field is open).
    #[must_use]
    pub(crate) fn item_rows(&self) -> usize {
        match self.entry {
            Entry::Choosing => self.row_count(),
            Entry::Typing => 0,
        }
    }

    /// The panel's rows at `width`; `now_ms` (epoch milliseconds) drives the timeout countdown.
    #[must_use]
    pub(crate) fn render(
        &self,
        theme: &Theme,
        width: usize,
        kb: &KeybindingsManager,
        now_ms: u64,
    ) -> Vec<Line> {
        let mut lines: Vec<Line> = vec![Vec::new()];
        for (index, row) in wrap_text(&self.question.text, text_width(width))
            .into_iter()
            .enumerate()
        {
            let mut line = vec![
                Span::raw(CHROME_INDENT),
                theme.fg_span(ThemeColor::Accent, if index == 0 { "? " } else { "  " }),
            ];
            line.extend(
                row.into_iter()
                    .map(|span| theme.bold(theme.fg_span(ThemeColor::Text, span.content))),
            );
            lines.push(line);
        }
        lines.push(Vec::new());
        match self.entry {
            Entry::Choosing => self.render_rows(&mut lines, theme, width),
            Entry::Typing => {
                if self.question.kind != QuestionKind::Text {
                    self.render_rows(&mut lines, theme, width);
                    lines.push(Vec::new());
                }
                lines.extend(crate::menu_panel::search_field_lines(
                    theme,
                    width,
                    self.input.value(),
                    self.input.cursor(),
                    true,
                    TYPE_PLACEHOLDER,
                ));
            }
        }
        if let Some(seconds) = self.question.timeout_seconds {
            let fallback = if self.question.default.is_empty() {
                "no answer".to_string()
            } else {
                self.question.default.join(", ")
            };
            // The countdown runs from the host's deadline; an older host sends none, and the
            // whole timeout shows instead.
            let left = self.question.expires_at_ms.map_or(seconds, |expires| {
                expires.saturating_sub(now_ms).div_ceil(1000)
            });
            lines.push(crate::width::truncate_line(
                &vec![
                    Span::raw(CHROME_INDENT),
                    theme.fg_span(
                        ThemeColor::Dim,
                        format!("Continues with {fallback} in {left}s"),
                    ),
                ],
                width,
                "\u{2026}",
            ));
        }
        lines.push(Vec::new());
        lines.extend(self.footer(kb).rows(theme, width));
        lines
    }

    fn render_rows(&self, lines: &mut Vec<Line>, theme: &Theme, width: usize) {
        let choosing = self.entry == Entry::Choosing;
        for (index, choice) in self.question.choices.iter().enumerate() {
            // A multi-choice row shows its checkbox; a single-choice row its radio, filled at
            // the selection (Enter answers the selected row).
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
                theme.fg_span(ThemeColor::Dim, format!(" {}. ", index + 1)),
                Span::raw(choice.clone()),
            ];
            lines.push(crate::menu_panel::menu_row(
                theme,
                width,
                primary,
                &[],
                choosing && index == self.selected,
            ));
        }
        if self.has_other_row() {
            let index = self.question.choices.len();
            let primary: Line = vec![
                theme.fg_span(ThemeColor::Dim, "   "),
                theme.fg_span(ThemeColor::Dim, format!(" {}. ", index + 1)),
                theme.fg_span(ThemeColor::Muted, OTHER_LABEL),
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

    fn footer(&self, kb: &KeybindingsManager) -> Footer {
        let digits = self.row_count().min(9);
        match (self.entry, self.question.kind) {
            (Entry::Typing, QuestionKind::Text) => Footer::new()
                .bound(kb, &["tui.select.confirm"], "answer", HintRank::Core)
                .bound(kb, &["tui.select.cancel"], "decline", HintRank::Core),
            (Entry::Typing, QuestionKind::Single | QuestionKind::Multi) => Footer::new()
                .bound(kb, &["tui.select.confirm"], "answer", HintRank::Core)
                .bound(kb, &["tui.select.cancel"], "back", HintRank::Core),
            (Entry::Choosing, QuestionKind::Multi) => Footer::new()
                .navigate(kb)
                .keys(
                    kb.get_keys("app.question.toggle")
                        .into_iter()
                        .chain([format!("1-{digits}")]),
                    "toggle",
                    HintRank::Useful,
                )
                .bound(kb, &["tui.select.confirm"], "answer", HintRank::Core)
                .bound(kb, &["tui.select.cancel"], "decline", HintRank::Core),
            (Entry::Choosing, QuestionKind::Single | QuestionKind::Text) => Footer::new()
                .navigate(kb)
                .keys([format!("1-{digits}")], "pick", HintRank::Extra)
                .bound(kb, &["tui.select.confirm"], "answer", HintRank::Core)
                .bound(kb, &["tui.select.cancel"], "decline", HintRank::Core),
        }
    }
}

/// How a row pick acts on a single-choice row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// A digit: answer with the row.
    Commit,
    /// Space or a click: take the selection only.
    Toggle,
}

fn text_width(width: usize) -> usize {
    width.saturating_sub(CHROME_INDENT.len() + 2).max(1)
}

/// The item row a digit key picks: `1` is the first row, up to `9`.
fn digit_row(key: &str) -> Option<usize> {
    match key.as_bytes() {
        [digit @ b'1'..=b'9'] => Some(usize::from(digit - b'1')),
        _ => None,
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
            allow_other: false,
            expires_at_ms: None,
        }
    }

    fn rows(prompt: &QuestionPrompt, width: usize) -> Vec<String> {
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        prompt
            .render(&theme, width, &kb(), 0)
            .iter()
            .map(|line| line.iter().map(|span| span.content.as_str()).collect())
            .collect()
    }

    fn answered(answer: &[&str]) -> QuestionAction {
        QuestionAction::Reply(QuestionReply::Answered {
            answer: answer.iter().map(ToString::to_string).collect(),
        })
    }

    /// One choice: numbered radios with the default selected; Down moves, Enter answers the
    /// selected row, a digit answers its row at once, Esc declines, and Ctrl+C is left to the app
    /// (it interrupts the turn).
    #[test]
    fn a_single_choice_question_renders_numbered_radios_and_answers_by_key_or_digit() {
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
                pad("\u{203a} (\u{2022}) 1. sqlite"),
                pad("  ( ) 2. redis"),
                String::new(),
                "  \u{2191}/\u{2193} navigate \u{b7} 1-2 pick \u{b7} Enter answer \u{b7} Esc decline"
                    .to_string(),
            ]
        );
        assert_eq!(prompt.handle_key("down", &kb()), QuestionAction::Handled);
        assert_eq!(prompt.handle_key("enter", &kb()), answered(&["redis"]));
        assert_eq!(prompt.handle_key("1", &kb()), answered(&["sqlite"]));
        assert_eq!(prompt.handle_key("3", &kb()), QuestionAction::Handled);
        assert_eq!(
            prompt.handle_key("escape", &kb()),
            QuestionAction::Reply(QuestionReply::Declined)
        );
        assert_eq!(
            prompt.handle_key("ctrl+c", &kb()),
            QuestionAction::PassThrough
        );
        // App chords (Ctrl+O, the todo toggle) stay the app's; plain keys stop here.
        assert_eq!(
            prompt.handle_key("alt+t", &kb()),
            QuestionAction::PassThrough
        );
        assert_eq!(prompt.handle_key("x", &kb()), QuestionAction::Handled);
    }

    /// Several choices: Space or a digit toggles, Enter answers every checked row; the timeout
    /// counts down from the host's deadline.
    #[test]
    fn a_multi_choice_question_toggles_and_counts_down() {
        let mut prompt =
            QuestionPrompt::new(question(QuestionKind::Multi, &["a", "b", "c"], &["b"]));
        prompt.question.timeout_seconds = Some(30);
        prompt.question.expires_at_ms = Some(30_000);
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        let at = |now_ms: u64| -> Vec<String> {
            prompt
                .render(&theme, 72, &kb(), now_ms)
                .iter()
                .map(|line| line.iter().map(|span| span.content.as_str()).collect())
                .collect()
        };
        let pad = |row: &str| format!("{row:<72}");
        assert_eq!(
            at(6_500),
            vec![
                String::new(),
                "  ? Which store should the session cache use?".to_string(),
                String::new(),
                pad("  [ ] 1. a"),
                pad("\u{203a} [x] 2. b"),
                pad("  [ ] 3. c"),
                "  Continues with b in 24s".to_string(),
                String::new(),
                "  \u{2191}/\u{2193} navigate \u{b7} Space/1-3 toggle \u{b7} Enter answer \u{b7} Esc decline"
                    .to_string(),
            ]
        );
        assert_eq!(at(29_001)[6], "  Continues with b in 1s");
        prompt.handle_key("down", &kb());
        prompt.handle_key("space", &kb());
        prompt.handle_key("1", &kb());
        assert_eq!(
            prompt.handle_key("enter", &kb()),
            answered(&["a", "b", "c"])
        );
    }

    /// `Other…`: Enter (or its digit) opens the text field under the choices, Enter answers the
    /// typed text (with the checked choices on a multi question), Esc goes back to the choices.
    #[test]
    fn the_other_row_takes_a_typed_answer() {
        let mut prompt = QuestionPrompt::new(question(QuestionKind::Single, &["yes", "no"], &[]));
        prompt.question.allow_other = true;
        prompt.handle_key("3", &kb());
        let rule = "\u{2500}".repeat(48);
        let pad = |row: &str| format!("{row:<48}");
        assert_eq!(
            rows(&prompt, 48),
            vec![
                String::new(),
                "  ? Which store should the session cache use?".to_string(),
                String::new(),
                pad("  ( ) 1. yes"),
                pad("  ( ) 2. no"),
                pad("\u{203a}     3. Other\u{2026}"),
                String::new(),
                rule.clone(),
                pad(" >  Type your answer"),
                rule,
                String::new(),
                "  Enter answer \u{b7} Esc back".to_string(),
            ]
        );
        assert_eq!(prompt.handle_key("enter", &kb()), QuestionAction::Handled);
        for key in ["l", "a", "t", "e", "r"] {
            prompt.handle_key(key, &kb());
        }
        assert_eq!(prompt.handle_key("escape", &kb()), QuestionAction::Handled);
        assert_eq!(prompt.handle_key("up", &kb()), QuestionAction::Handled);
        assert_eq!(prompt.handle_key("down", &kb()), QuestionAction::Handled);
        assert_eq!(prompt.handle_key("enter", &kb()), QuestionAction::Handled);
        assert_eq!(prompt.handle_key("enter", &kb()), answered(&["later"]));

        let mut multi = QuestionPrompt::new(question(QuestionKind::Multi, &["a", "b"], &["a"]));
        multi.question.allow_other = true;
        assert_eq!(multi.click_row(2), QuestionAction::Handled);
        multi.paste("z");
        assert_eq!(multi.handle_key("enter", &kb()), answered(&["a", "z"]));
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
            answered(&["keep going !"])
        );
        let mut empty = QuestionPrompt::new(question(QuestionKind::Text, &[], &[]));
        assert_eq!(empty.handle_key("enter", &kb()), QuestionAction::Handled);
    }
}
