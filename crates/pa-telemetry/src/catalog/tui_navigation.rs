//! The interactive client's standalone event for finding a place in a long transcript.
//! Standalone because `tui exit` sits at the backend's 128-key cap.

use super::{enum_rule, required, EventRule};

/// A transcript navigation action was used (v2), the first time in a run for each action: a
/// jump to the previous or next user prompt, or the follow key pressed while the follow pill
/// counted new entries. Never prompt text, a row count, or a key.
pub(super) const TUI_TRANSCRIPT_NAVIGATION: EventRule = EventRule {
    name: "tui transcript navigation",
    since: 2,
    properties: &[(
        "action",
        required(enum_rule(
            &["previous_prompt", "next_prompt", "follow_new_output"],
            "previous_prompt",
        )),
    )],
};
