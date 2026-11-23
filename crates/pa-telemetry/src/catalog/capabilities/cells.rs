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

/// Kernel calls refused before the function ran (v2), emitted at session end beside `agent
/// session ended` (which sits near the backend's 128-key cap) only when a call was refused: calls
/// that named an option the function does not declare, gave an option twice, gave an option no
/// value, passed too few or too many arguments, or passed an MCP tool an argument its schema does
/// not declare. Counts only: never a function, an option's or argument's name, or a value.
pub(in crate::catalog) const AGENT_KERNEL_CALL_REFUSALS: EventRule = EventRule {
    name: "agent kernel call refusals",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("call_refused_unknown_option_count", required(count())),
        ("call_refused_repeated_option_count", required(count())),
        ("call_refused_option_without_value_count", required(count())),
        ("call_refused_arity_count", required(count())),
        ("call_refused_tool_argument_count", required(count())),
    ],
};

/// `wait-until` waits inside the turn (v2), emitted at session end beside `agent session ended`
/// (which sits near the backend's 128-key cap) only when a cell waited: waits started, and how
/// many ended with their condition met or at their timeout (the rest were stopped with their
/// cell). Counts only: never a condition, a command, a label, a value, or a duration.
pub(in crate::catalog) const AGENT_KERNEL_WAITS: EventRule = EventRule {
    name: "agent kernel waits",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("wait_until_count", required(count())),
        ("wait_until_met_count", required(count())),
        ("wait_until_timed_out_count", required(count())),
    ],
};
