//! The `fs` capability owns two events: the search calls and the anchored read/edit calls.

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
