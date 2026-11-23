//! The interactive client's standalone event for searching a transcript. Standalone because
//! `tui exit` sits at the backend's 128-key cap.

use super::{boolean, count, enum_rule, required, EventRule};

/// A transcript search closed (v2), the first time in a run for each outcome: Enter at a match
/// (`jumped`), Esc with the view put back (`cancelled`), or a close while the query matched
/// nothing (`no_match`); the next/previous presses it took, whether a match inside a collapsed
/// card or folded thinking opened it, and whether older history lived only in the daemon.
/// Never the query, its length, a matched row, or a key.
pub(super) const TUI_TRANSCRIPT_SEARCH: EventRule = EventRule {
    name: "tui transcript search",
    since: 2,
    properties: &[
        (
            "outcome",
            required(enum_rule(&["jumped", "cancelled", "no_match"], "cancelled")),
        ),
        ("steps", required(count())),
        ("revealed", required(boolean())),
        (
            "history",
            required(enum_rule(&["complete", "loaded_only"], "complete")),
        ),
    ],
};
