//! The `fs` capability owns three events: the search calls, the anchored read/edit calls, and
//! the checks after edits.

use super::super::{count, duration, required, uuid, EventRule};

/// Kernel native file search (`fs.grep`/`fs.glob`/`fs.find`) adoption for one session (v2),
/// emitted at session end beside `agent session ended` (which sits near the backend's 128-key cap)
/// only when the session searched: calls by operation, failures, capped calls, results returned,
/// and the slowest call. Counts only: never a pattern, a path, or a match.
pub(in crate::catalog) const AGENT_FS_SEARCH: EventRule = EventRule {
    name: "agent fs search",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("fs_grep_count", required(count())),
        ("fs_glob_count", required(count())),
        ("fs_find_count", required(count())),
        ("fs_search_failed_count", required(count())),
        ("fs_search_truncated_count", required(count())),
        ("fs_search_result_count", required(count())),
        ("fs_search_max_ms", required(duration())),
    ],
};

/// Kernel anchored (hashline) file reads and edits (`fs.read`/`fs.edit`) adoption for one session
/// (v2), emitted at session end beside `agent session ended` (which sits near the backend's 128-key
/// cap) only when the session used them: reads and lines shown, failed reads, patches applied and
/// the edits in them, patches rejected for stale anchors and the stale anchors, and failed edits.
/// Counts only: never a path, an anchor, or file content.
pub(in crate::catalog) const AGENT_FS_EDIT: EventRule = EventRule {
    name: "agent fs edit",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("fs_read_count", required(count())),
        ("fs_read_line_count", required(count())),
        ("fs_read_failed_count", required(count())),
        ("fs_edit_count", required(count())),
        ("fs_edit_op_count", required(count())),
        ("fs_edit_rejected_count", required(count())),
        ("fs_edit_stale_anchor_count", required(count())),
        ("fs_edit_failed_count", required(count())),
    ],
};

/// Edit-then-check (v2), emitted at session end beside `agent session ended` (which sits near the
/// backend's 128-key cap) only when an edit was checked: checks run after an edit (`fs.edit` or the
/// `edit` skill), those that found diagnostics, those that outlived the edit's wait (their
/// diagnostics arrive as a notice), those dropped because their learned cost exceeds their benefit,
/// and those that could not run. Counts only: never a path, a command, or output.
pub(in crate::catalog) const AGENT_EDIT_CHECKS: EventRule = EventRule {
    name: "agent edit checks",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("edit_check_run_count", required(count())),
        ("edit_check_found_count", required(count())),
        ("edit_check_late_count", required(count())),
        ("edit_check_skipped_count", required(count())),
        ("edit_check_failed_count", required(count())),
    ],
};
