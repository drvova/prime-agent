//! The interactive client's standalone event for user keybindings that override defaults.
//! Standalone because `tui exit` sits at the backend's 128-key cap.

use super::{count, required, EventRule};

/// The user's `keybindings.json` took default keys from other actions (v2), reported once at a
/// run's start when it did: `takeovers` counts the default keys taken. Never a key, an action,
/// or any of the file's text.
pub(super) const TUI_KEYBINDING_TAKEOVERS: EventRule = EventRule {
    name: "tui keybinding takeovers",
    since: 2,
    properties: &[("takeovers", required(count()))],
};
