//! The interactive client's standalone events for finding its shortcuts and mouse gestures.
//! Standalone because `tui exit` sits at the backend's 128-key cap.

use super::{enum_rule, required, EventRule};

/// The `?` shortcut overview opened on an empty prompt (v2), the first time in a run. Never a
/// key or any prompt text.
pub(super) const TUI_SHORTCUT_OVERLAY_OPENED: EventRule = EventRule {
    name: "tui shortcut overlay opened",
    since: 2,
    properties: &[],
};

/// A mouse selection in the transcript was copied (v2), the first time in a run for each kind:
/// a drag, a double click's word, or a triple click's row. Never the copied text or its length.
pub(super) const TUI_MOUSE_SELECTION: EventRule = EventRule {
    name: "tui mouse selection",
    since: 2,
    properties: &[(
        "kind",
        required(enum_rule(&["drag", "word", "line"], "drag")),
    )],
};
