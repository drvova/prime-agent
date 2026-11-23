//! Finding your place in a long transcript: the prompt row pinned over a paused window's first
//! row, the follow pill over its last, and the jumps between user prompts. All three read the
//! geometry the window already holds; none adds a row to the layout.

use super::AgentView;
use crate::chat::ChatEntry;
use crate::menu_chrome::{Footer, ELLIPSIS, SEPARATOR};
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::{str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};

/// The prompt mark that leads the pinned row.
const PROMPT_MARK: &str = "\u{276f} ";
/// The fewest prompt columns the pinned row keeps before it gives any to the jump hints.
const PINNED_PROMPT_MIN_COLUMNS: usize = 24;
/// The blank columns between the pinned prompt and its jump hints.
const PINNED_HINT_GAP: usize = 2;

/// Which way a prompt jump moves the paused window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptJump {
    Previous,
    Next,
}

/// Hand one navigation action's report to the telemetry seam and return at once: the key path
/// never waits on the delivery.
pub(crate) fn report_used(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    action: &'static str,
) {
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry.transcript_navigated(action).await;
        });
    }
}

impl AgentView {
    /// Move the window to the nearest user prompt that starts above (`Previous`) or below
    /// (`Next`) its first row and pause there; `Next` past the last prompt returns to the bottom
    /// and follows. Whether the window moved: there is no earlier prompt among the loaded rows,
    /// or the window already follows the tail.
    pub(crate) fn jump_to_prompt(&mut self, jump: PromptJump) -> bool {
        if self.layout_width == 0 {
            return false;
        }
        let was_following = self.following;
        self.resolve_sparse_geometry();
        let layout = self.layout_pass(self.layout_width);
        let max_scroll = layout.total.saturating_sub(self.window_rows);
        let top = if was_following {
            max_scroll
        } else {
            self.scroll_top.min(max_scroll)
        };
        let mut prompts = self
            .chat
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry, ChatEntry::User { .. }))
            .map(|(index, _)| layout.entry_start(index));
        let target = match jump {
            PromptJump::Previous => prompts.take_while(|start| *start < top).last(),
            PromptJump::Next => prompts.find(|start| *start > top),
        };
        match (jump, target) {
            // A prompt the bottom window already shows cannot reach the first row: the jump
            // ends at the bottom instead.
            (PromptJump::Previous | PromptJump::Next, Some(start)) if start < max_scroll => {
                if was_following {
                    self.unseen_entries = 0;
                }
                self.sparse_window = None;
                self.scroll_top = start;
                self.following = false;
                true
            }
            (PromptJump::Next, Some(_) | None) if !was_following => {
                self.scroll_to_bottom();
                true
            }
            (PromptJump::Previous | PromptJump::Next, Some(_) | None) => false,
        }
    }

    /// The prompt a paused window reads under: the last user prompt that starts above the
    /// window's first row. `None` while that prompt's own rows hold the first row, or no prompt
    /// precedes it.
    pub(super) fn pinned_prompt(&self) -> Option<&str> {
        let (top, _) = self.reader_position()?;
        self.chat
            .get(..=top)?
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| match entry {
                ChatEntry::User { text, .. } => Some((index, text.as_str())),
                ChatEntry::Status { .. }
                | ChatEntry::RunSummary(_)
                | ChatEntry::Assistant(_)
                | ChatEntry::SlashCommand { .. }
                | ChatEntry::AgentMessage(_)
                | ChatEntry::BackgroundCompletion(_)
                | ChatEntry::CustomPanel(_)
                | ChatEntry::Tool(_)
                | ChatEntry::BashExecution(_)
                | ChatEntry::SkillInvocation(_)
                | ChatEntry::InjectedPrompt(_)
                | ChatEntry::RefinementOutcome(_)
                | ChatEntry::CompactionSummary { .. } => None,
            })
            .filter(|(index, _)| *index < top)
            .map(|(_, text)| text)
    }
}

/// The pinned prompt row: the prompt's first line on the user-message band, cut with the shared
/// mark, and the jump hints flush right in the columns the prompt leaves (the rightmost hint
/// drops first). Exactly `width` columns.
pub(super) fn pinned_prompt_row(prompt: &str, jumps: &Footer, theme: &Theme, width: usize) -> Line {
    let band = theme.bg_style(ThemeBg::UserMessageBg);
    let first_line: String = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let mark_width = str_width(PROMPT_MARK);
    // The prompt comes first: the hints get what its text, up to three quarters of the row,
    // leaves.
    let prompt_columns = str_width(&first_line)
        .min(width * 3 / 4)
        .max(PINNED_PROMPT_MIN_COLUMNS);
    let hints = jumps.fitted(width.saturating_sub(mark_width + prompt_columns + PINNED_HINT_GAP));
    let mut trailing: Line = Vec::new();
    for (index, hint) in hints.iter().enumerate() {
        if index > 0 {
            trailing.push(Span::styled(
                SEPARATOR,
                band.patch(theme.fg_style(ThemeColor::Dim)),
            ));
        }
        trailing.push(Span::styled(
            hint.key.clone(),
            band.patch(theme.fg_style(ThemeColor::Muted)),
        ));
        trailing.push(Span::styled(
            format!(" {}", hint.verb),
            band.patch(theme.fg_style(ThemeColor::Dim)),
        ));
    }
    let trailing_width: usize = trailing.iter().map(|span| str_width(&span.content)).sum();
    let reserved = if trailing.is_empty() {
        0
    } else {
        trailing_width + PINNED_HINT_GAP
    };
    let text = truncate_to_width(
        &first_line,
        width.saturating_sub(mark_width + reserved),
        ELLIPSIS,
    );
    let used = mark_width + str_width(&text) + trailing_width;
    let mut row: Line = vec![
        Span::styled(PROMPT_MARK, band.patch(theme.fg_style(ThemeColor::Accent))),
        Span::styled(
            text,
            band.patch(theme.fg_style(ThemeColor::UserMessageText)),
        ),
        Span::styled(" ".repeat(width.saturating_sub(used)), band),
    ];
    row.extend(trailing);
    // A frame narrower than the mark keeps what fits.
    truncate_line(&row, width, "")
}

/// The follow pill's text: what arrived since the window paused and the key that returns to the
/// bottom, shortened until it fits `width`. `None` when nothing fits or there is nothing to say
/// (no new entry and no bound key).
pub(super) fn follow_pill_label(unseen: usize, key: Option<&str>, width: usize) -> Option<String> {
    let new = (unseen > 0).then(|| format!("{unseen} new"));
    let candidates = match (new, key) {
        (Some(new), Some(key)) => vec![
            format!(" \u{2193} {new}{SEPARATOR}{key} to follow "),
            format!(" \u{2193} {new}{SEPARATOR}{key} "),
            format!(" \u{2193} {new} "),
        ],
        (Some(new), None) => vec![format!(" \u{2193} {new} ")],
        (None, Some(key)) => vec![
            format!(" \u{2193} {key} to follow "),
            format!(" \u{2193} {key} "),
        ],
        (None, None) => Vec::new(),
    };
    candidates
        .into_iter()
        .find(|label| str_width(label) <= width)
}

#[cfg(test)]
#[path = "navigation_tests.rs"]
mod tests;
