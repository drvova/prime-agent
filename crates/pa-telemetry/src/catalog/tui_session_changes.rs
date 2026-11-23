//! The interactive client's standalone event for the session changes panel (`/diff`).
//! Standalone because `tui exit` sits at the backend's 128-key cap.

use super::{count, enum_rule, required, EventRule};

/// A session changes panel action was used (v2), the first time in a run for each action: the
/// panel opened, its scope switched between the turn and the session, a file's diff opened, or a
/// file's diff was attached to the next message. `files` is how many files the open scope listed
/// and `untested` how many of them no test or build ran after. Never a path, a diff, or a
/// command.
pub(super) const TUI_SESSION_CHANGES: EventRule = EventRule {
    name: "tui session changes",
    since: 2,
    properties: &[
        (
            "action",
            required(enum_rule(&["open", "scope", "diff", "attach"], "open")),
        ),
        ("files", required(count())),
        ("untested", required(count())),
    ],
};
