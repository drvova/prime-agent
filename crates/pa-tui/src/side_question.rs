//! The side-question pane: the `/btw` conversation mounted above the
//! prompt dock. Turns render in the popup surface; the first turn keeps
//! its `/btw` header, follow-ups render as user-message bubbles, and local
//! notices render as complete turns that never reach the daemon.

use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::{str_width, wrap_text};

/// One pane turn (a streamed event, or a client-local notice).
#[derive(Debug, Clone, PartialEq)]
pub struct SideQuestionTurn {
    pub id: String,
    pub question: String,
    pub answer: String,
    /// `running` | `complete` | `cancelled` | `error`.
    pub status: String,
    pub error_message: Option<String>,
    /// A client-local notice: rendered like a turn, never sent to the daemon.
    pub local: bool,
}

/// Whether the turn can seed a follow-up (local notices never join).
#[must_use]
pub fn turn_seeds_follow_up(turn: &SideQuestionTurn) -> bool {
    !turn.local && !turn.answer.is_empty()
}

/// A pane-mounted bash run: the pane renders the same bordered card the main thread mounts, at the
/// pane width. The `!` variant seeds follow-up side questions.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneBash {
    pub command: String,
    /// The raw accumulated output chunks.
    pub output: String,
    pub running: bool,
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
    pub error_message: Option<String>,
    /// The `!!` variant: the pane card's border renders dim.
    pub excluded: bool,
}

impl PaneBash {
    /// A running pane-mounted run for one command.
    #[must_use]
    pub fn new_running(command: &str, excluded: bool) -> Self {
        Self {
            command: command.to_string(),
            output: String::new(),
            running: true,
            exit_code: None,
            cancelled: false,
            truncated: false,
            full_output_path: None,
            error_message: None,
            excluded,
        }
    }

    /// The card the pane renders: the rows come from the shared card renderer.
    #[must_use]
    pub fn execution_card(&self) -> crate::bash_card::BashExecutionCard {
        let mut card =
            crate::bash_card::BashExecutionCard::new_running("", &self.command, self.excluded);
        card.append_output(&self.output);
        if let Some(message) = &self.error_message {
            card.set_failed(message);
        } else if !self.running {
            card.set_complete(
                self.exit_code,
                self.cancelled,
                self.truncated,
                self.full_output_path.clone(),
            );
        }
        card
    }
}

/// The pane: the turns in order, a bash run mounted after them, the invisible follow-up seeds a
/// finished bash run contributed, and the expansion flag the detail cycle toggles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SideQuestionPane {
    pub turns: Vec<SideQuestionTurn>,
    pub bash: Option<PaneBash>,
    /// Follow-up seeds that never render in the pane: the raw `!input` and the
    /// formatted output.
    pub extra_seeds: Vec<(String, String)>,
    pub expanded: bool,
}

/// The pane's horizontal padding (TS `paddingX = max(2, editorPaddingX)`;
/// this surface's editor padding is the default two columns).
const PADDING_X: usize = 2;

impl SideQuestionPane {
    /// The answered turns the follow-up seeds its context with, plus the
    /// pane-mounted bash runs the `!` variant contributed.
    #[must_use]
    pub fn seed_turns(&self) -> Vec<(String, String)> {
        self.turns
            .iter()
            .filter(|turn| turn_seeds_follow_up(turn))
            .map(|turn| (turn.question.clone(), turn.answer.clone()))
            .chain(self.extra_seeds.iter().cloned())
            .collect()
    }

    /// Whether any turn or bash run is still running (a completed notice can
    /// sit below a running turn).
    #[must_use]
    pub fn running(&self) -> bool {
        self.turns.iter().any(|turn| turn.status == "running")
            || self.bash.as_ref().is_some_and(|bash| bash.running)
    }

    /// Upsert a streamed event into its turn.
    pub fn upsert(&mut self, turn: SideQuestionTurn) {
        match self
            .turns
            .iter_mut()
            .find(|existing| existing.id == turn.id)
        {
            Some(existing) => *existing = turn,
            None => self.turns.push(turn),
        }
    }

    /// The running turn the escape key cancels (the latest the pane tracks).
    #[must_use]
    pub fn active_turn(&self) -> Option<&SideQuestionTurn> {
        self.turns
            .iter()
            .rev()
            .find(|turn| turn.status == "running" && !turn.local)
    }

    /// Render the pane: blank surfaced row, the turns, and the dim hint row, every row painted with
    /// the popup background and padded to the full width.
    #[must_use]
    pub fn render(
        &self,
        theme: &Theme,
        frame: usize,
        expanded: bool,
        kb: &crate::keybindings::KeybindingsManager,
        width: usize,
    ) -> Vec<crate::Line> {
        let cancel_hint = kb.key_text("tui.select.cancel");
        let bg = theme.bg_style(ThemeBg::ToolPanelBg);
        let user_text = theme.fg_style(ThemeColor::UserMessageText);
        let accent = theme.fg_style(ThemeColor::Accent);
        let error = theme.fg_style(ThemeColor::Error);
        let blank = || vec![crate::Span::styled(" ".repeat(width.max(1)), bg)];
        let surface = |line: crate::Line| -> crate::Line {
            let used: usize = line.iter().map(|span| str_width(&span.content)).sum();
            let mut line = line;
            for span in &mut line {
                span.style = span.style.patch(bg);
            }
            line.push(crate::Span::styled(
                " ".repeat(width.saturating_sub(used)),
                bg,
            ));
            line
        };
        let mut rows: Vec<crate::Line> = vec![blank()];
        for (index, turn) in self.turns.iter().enumerate() {
            if index > 0 {
                // Follow-ups and notices render as standard user-message bubbles.
                rows.extend(render_bubble(&turn.question, theme, width));
            } else {
                // The question wraps under its own first column (a hanging indent past the
                // accent `/btw` label), so a long question never runs back to the pane's edge.
                let label = "/btw  ";
                let lead = PADDING_X + str_width(label);
                for (row, segments) in wrap_text(&turn.question, width.saturating_sub(lead).max(1))
                    .into_iter()
                    .enumerate()
                {
                    let mut line: crate::Line =
                        vec![crate::Span::styled(" ".repeat(PADDING_X), bg)];
                    if row == 0 {
                        line.push(crate::Span::styled("/btw".to_string(), accent));
                        line.push(crate::Span::styled("  ".to_string(), bg));
                    } else {
                        line.push(crate::Span::styled(" ".repeat(str_width(label)), bg));
                    }
                    line.extend(segments.into_iter().map(|span| crate::Span {
                        content: span.content,
                        style: user_text,
                    }));
                    rows.push(surface(line));
                }
            }
            rows.push(blank());
            let mut style = crate::markdown::MarkdownStyle::from_theme(theme);
            // The plain text renders in the user-message color, not the markdown
            // body color.
            style.body = theme.fg_style(ThemeColor::UserMessageText);
            let content_width = width.saturating_sub(PADDING_X).max(1);
            let mut rendered = if turn.answer.is_empty() {
                Vec::new()
            } else {
                crate::markdown::render_markdown(&turn.answer, content_width, &style)
            };
            if let Some(message) = &turn.error_message {
                // The error row is a single-paddingX row; the `padded` prefix below
                // supplies the pad.
                rendered.push(vec![crate::Span::styled(message.clone(), error)]);
            }
            if rendered.is_empty() {
                // The placeholder rows are single-paddingX rows too.
                let text = match turn.status.as_str() {
                    "cancelled" => "Cancelled".to_string(),
                    "complete" => "No response".to_string(),
                    _ => "Thinking…".to_string(),
                };
                rendered.push(vec![crate::Span::styled(text, user_text)]);
            }
            for line in rendered {
                let padded: crate::Line =
                    std::iter::once(crate::Span::styled(" ".repeat(PADDING_X), bg))
                        .chain(line)
                        .collect();
                for wrapped in wrap_row(&padded, width) {
                    rows.push(surface(wrapped));
                }
            }
            rows.push(blank());
        }
        // A pane-mounted bash run: one blank before and after, the card's own leading spacer
        // excluded (the pane adds the blank itself, matching the component's `Spacer(1)` row).
        if let Some(bash) = &self.bash {
            rows.push(blank());
            let card = bash.execution_card();
            for row in crate::bash_card::render_bash_execution(
                &card,
                frame,
                expanded,
                &cancel_hint,
                theme,
                width,
            ) {
                rows.push(surface(row));
            }
            rows.push(blank());
        }
        // The editor stays live under the pane: Enter sends a follow-up while no turn runs, and
        // the editor's clear key cancels a running turn and returns to the session.
        let mut footer = crate::menu_chrome::Footer::new();
        if !self.running() {
            footer = footer.bound(
                kb,
                &["tui.input.submit"],
                "follow up",
                crate::menu_chrome::HintRank::Useful,
            );
        }
        let footer = footer.bound(
            kb,
            &["app.input.clear"],
            "close",
            crate::menu_chrome::HintRank::Core,
        );
        rows.push(surface(footer.line(theme, width)));
        rows.push(blank());
        rows
    }
}

/// Wrap one rendered row to the width, keeping the bg style on the tail: the markdown renderer
/// wraps its own lines, this re-wraps when the terminal is narrower than the rendered content.
fn wrap_row(line: &crate::Line, width: usize) -> Vec<crate::Line> {
    let used: usize = line.iter().map(|span| str_width(&span.content)).sum();
    if used <= width || width == 0 {
        return vec![line.clone()];
    }
    let plain: String = line.iter().map(|span| span.content.as_str()).collect();
    let wrapped = wrap_text(&plain, width);
    let style = line
        .iter()
        .map(|span| span.style)
        .reduce(ratatui::style::Style::patch)
        .unwrap_or_default();
    wrapped
        .into_iter()
        .map(|segments| {
            segments
                .into_iter()
                .map(|span| crate::Span {
                    content: span.content,
                    style,
                })
                .collect()
        })
        .collect()
}

/// The follow-up bubble: blank surface row, wrapped question rows, blank
/// surface row, every row padded to the full width on the block background.
fn render_bubble(text: &str, theme: &Theme, width: usize) -> Vec<crate::Line> {
    let bg = theme.bg_style(ThemeBg::UserMessageBg);
    let text_style = theme.fg_style(ThemeColor::UserMessageText);
    let content_width = width.saturating_sub(PADDING_X * 2).max(1);
    let mut rows: Vec<crate::Line> = vec![vec![crate::Span::styled(" ".repeat(width.max(1)), bg)]];
    let wrapped = wrap_text(text, content_width);
    if wrapped.is_empty() {
        rows.push(vec![crate::Span::styled(" ".repeat(width.max(1)), bg)]);
    }
    for line in wrapped {
        let mut row: crate::Line = vec![crate::Span::styled(" ".repeat(PADDING_X), bg)];
        let mut segments = line;
        for span in &mut segments {
            span.style = span.style.patch(text_style);
        }
        row.extend(segments);
        let used: usize = row.iter().map(|span| str_width(&span.content)).sum();
        row.push(crate::Span::styled(
            " ".repeat(width.saturating_sub(used)),
            bg,
        ));
        rows.push(row);
    }
    rows.push(vec![crate::Span::styled(" ".repeat(width.max(1)), bg)]);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, status: &str, answer: &str) -> SideQuestionTurn {
        SideQuestionTurn {
            id: id.to_string(),
            question: format!("question {id}"),
            answer: answer.to_string(),
            status: status.to_string(),
            error_message: None,
            local: false,
        }
    }

    #[test]
    fn local_notices_never_seed_follow_ups() {
        let mut pane = SideQuestionPane::default();
        pane.upsert(turn("a", "complete", "answered"));
        pane.turns.push(SideQuestionTurn {
            id: "side-notice-1".to_string(),
            question: "/tree".to_string(),
            answer: "Slash commands are not available...".to_string(),
            status: "complete".to_string(),
            error_message: None,
            local: true,
        });
        assert_eq!(
            pane.seed_turns(),
            vec![("question a".into(), "answered".into())]
        );
    }

    #[test]
    fn a_pane_bash_run_keeps_the_hint_cancelled_and_seeds_follow_ups() {
        let mut pane = SideQuestionPane::default();
        pane.upsert(turn("a", "complete", "done"));
        assert!(!pane.running());
        // A running pane bash run owns the hint's cancel affordance.
        let mut bash = PaneBash::new_running("echo pane", true);
        bash.output.push_str("hi\n");
        pane.bash = Some(bash);
        assert!(pane.running());
        assert!(pane.active_turn().is_none());
        // The finished run renders its rows and seeds the follow-up list
        // without joining the pane's turns.
        let mut bash = pane.bash.take().unwrap();
        bash.running = false;
        bash.exit_code = Some(0);
        pane.bash = Some(bash);
        assert!(!pane.running());
        pane.extra_seeds
            .push(("!echo pane".into(), "```\nhi\n```".into()));
        assert_eq!(
            pane.seed_turns(),
            vec![
                ("question a".into(), "done".into()),
                ("!echo pane".into(), "```\nhi\n```".into()),
            ]
        );
    }

    /// A question longer than the pane wraps under its own first column, past the `/btw` label,
    /// instead of running back to the pane's edge.
    #[test]
    fn a_long_question_wraps_under_its_first_column() {
        let theme = crate::theme::Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
        let pane = SideQuestionPane {
            turns: vec![SideQuestionTurn {
                question: "what does the parser do with an unterminated string".to_string(),
                ..turn("a", "complete", "")
            }],
            ..SideQuestionPane::default()
        };
        let rows: Vec<String> = pane
            .render(
                &theme,
                0,
                false,
                &crate::keybindings::KeybindingsManager::new(),
                30,
            )
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .take(4)
            .collect();
        assert_eq!(
            rows,
            [
                "",
                "  /btw  what does the parser",
                "        do with an",
                "        unterminated string",
            ]
        );
    }

    #[test]
    fn a_pane_bash_run_renders_header_output_and_status() {
        let theme = crate::theme::Theme::builtin("prime", crate::theme::ColorMode::Color256);
        let mut pane = SideQuestionPane::default();
        pane.upsert(turn("a", "complete", "the answer"));
        let mut bash = PaneBash::new_running("echo hi", true);
        bash.output = "hi\n".to_string();
        pane.bash = Some(bash);
        let rows = pane.render(
            &theme,
            0,
            false,
            &crate::keybindings::KeybindingsManager::new(),
            80,
        );
        let text =
            |line: &crate::Line| -> String { line.iter().map(|s| s.content.as_str()).collect() };
        let joined: Vec<String> = rows.iter().map(&text).collect();
        assert!(
            joined.iter().any(|row| row.contains("$ echo hi")),
            "the bash header rendered: {joined:?}"
        );
        assert!(
            joined.iter().any(|row| row.contains("hi")),
            "the streamed output rendered: {joined:?}"
        );
        assert!(
            joined
                .iter()
                .any(|row| row.contains("Running... (Esc/Ctrl+C to cancel)")),
            "the running loader rendered: {joined:?}"
        );
        // A settled failing run shows its exit status instead.
        pane.bash.as_mut().unwrap().running = false;
        pane.bash.as_mut().unwrap().exit_code = Some(3);
        let joined: Vec<String> = pane
            .render(
                &theme,
                0,
                false,
                &crate::keybindings::KeybindingsManager::new(),
                80,
            )
            .iter()
            .map(&text)
            .collect();
        assert!(
            joined.iter().any(|row| row.contains("(exit 3)")),
            "the exit status rendered: {joined:?}"
        );
    }

    #[test]
    fn running_hint_follows_any_running_turn() {
        let mut pane = SideQuestionPane::default();
        pane.upsert(turn("a", "complete", "done"));
        assert!(!pane.running());
        pane.upsert(turn("b", "running", ""));
        assert!(pane.running());
        assert_eq!(pane.active_turn().unwrap().id, "b");
    }

    #[test]
    fn render_places_the_btw_header_then_answer_then_hint() {
        let theme = crate::theme::Theme::builtin("prime", crate::theme::ColorMode::Color256);
        let mut pane = SideQuestionPane::default();
        pane.upsert(turn("a", "complete", "the answer"));
        let rows = pane.render(
            &theme,
            0,
            false,
            &crate::keybindings::KeybindingsManager::new(),
            80,
        );
        let text =
            |line: &crate::Line| -> String { line.iter().map(|s| s.content.as_str()).collect() };
        let joined: Vec<String> = rows.iter().map(&text).collect();
        let first = joined
            .iter()
            .find(|row| row.contains("/btw"))
            .expect("the /btw header row");
        assert!(first.contains("question a"));
        assert!(joined.iter().any(|row| row.contains("the answer")));
        assert!(joined
            .iter()
            .any(|row| row.contains("Enter follow up · Esc close")));
        // A running turn swaps the hint.
        pane.upsert(turn("b", "running", ""));
        let rows = pane.render(
            &theme,
            0,
            false,
            &crate::keybindings::KeybindingsManager::new(),
            80,
        );
        let joined: Vec<String> = rows.iter().map(&text).collect();
        assert!(joined.iter().any(|row| row.trim_end() == "  Esc close"));
        // The cancelled placeholder shows when no answer streamed.
        pane.upsert(SideQuestionTurn {
            status: "cancelled".to_string(),
            answer: String::new(),
            ..turn("b", "cancelled", "")
        });
        let rows = pane.render(
            &theme,
            0,
            false,
            &crate::keybindings::KeybindingsManager::new(),
            80,
        );
        let joined: Vec<String> = rows.iter().map(&text).collect();
        assert!(joined.iter().any(|row| row.contains("Cancelled")));
    }
}
