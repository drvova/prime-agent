//! The interactive client's standalone event for reaching a recently used model.
//! Standalone because `tui exit` sits at the backend's 128-key cap.

use super::{enum_rule, required, EventRule};

/// A recent-model action was used (v2), the first time in a run for each action: the composer's
/// recent-model key switched to the next recent model, or found no other one, or the `/model`
/// picker applied a model from its recent group. Never a model, a provider, or a key.
pub(super) const TUI_MODEL_RECENT_USED: EventRule = EventRule {
    name: "tui model recent used",
    since: 2,
    properties: &[(
        "action",
        required(enum_rule(&["cycle", "cycle_none", "recent_pick"], "cycle")),
    )],
};
