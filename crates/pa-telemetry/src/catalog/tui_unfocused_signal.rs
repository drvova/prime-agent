//! The interactive client's standalone event for the signal an unfocused terminal gets.
//! Standalone because `tui exit` sits at the backend's 128-key cap.

use super::{enum_rule, required, EventRule};

/// The unfocused terminal was signalled that the agent stopped for the user (v2), the first
/// time in a run for each reason: why it stopped (the run ended, a question opened) and which
/// `terminal.notifyOnTurnEnd` value chose the signal. Never the notification text.
pub(super) const TUI_UNFOCUSED_SIGNAL: EventRule = EventRule {
    name: "tui unfocused signal",
    since: 2,
    properties: &[
        (
            "reason",
            required(enum_rule(&["turn_end", "question"], "turn_end")),
        ),
        (
            "mode",
            required(enum_rule(&["bell", "notification"], "bell")),
        ),
    ],
};
