//! The `cells` capability.

use super::super::{count, required, uuid, EventRule};

/// Python cells that continued in the background (v2), emitted at session end beside `agent
/// session ended` (which sits near the backend's 128-key cap) only when a cell was armed with a
/// yield window or outlived one: cells backgrounded, how they ended (value, exception,
/// interrupted), and where each armed window came from (learned from the project's recent cells,
/// the model's `timeout`, the user's `pythonCellTimeoutMs`, or the cold 60 s default). Counts only:
/// never code, output, a value, a cell id, or a duration.
pub(in crate::catalog) const AGENT_REPL_BACKGROUND_CELLS: EventRule = EventRule {
    name: "agent repl background cells",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("cell_backgrounded_count", required(count())),
        ("cell_background_ok_count", required(count())),
        ("cell_background_error_count", required(count())),
        ("cell_background_interrupted_count", required(count())),
        ("cell_window_learned_count", required(count())),
        ("cell_window_timeout_count", required(count())),
        ("cell_window_setting_count", required(count())),
        ("cell_window_default_count", required(count())),
    ],
};
