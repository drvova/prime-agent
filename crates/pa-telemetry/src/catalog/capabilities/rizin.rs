//! The `rizin` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel `rz-*` (the installed Rizin reverse-engineering program, driven over a pipe) adoption
/// for one session (v2), emitted at session end beside `agent session ended` (which sits near the
/// backend's 128-key cap) only when the session used it: sessions opened without and with the
/// debugger, text and JSON commands answered, commands interrupted by a time limit or a stopped
/// cell, rizins that died, and opens refused because rizin is not installed. Counts only: never a
/// target, a path, a command, or output.
pub(in crate::catalog) const AGENT_REPL_RIZIN: EventRule = EventRule {
    name: "agent repl rizin",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("rizin_open_count", required(count())),
        ("rizin_debug_open_count", required(count())),
        ("rizin_command_count", required(count())),
        ("rizin_json_command_count", required(count())),
        ("rizin_interrupted_count", required(count())),
        ("rizin_died_count", required(count())),
        ("rizin_not_installed_count", required(count())),
    ],
};
