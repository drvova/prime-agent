//! The `fs` capability owns seven events: the search calls, the anchored read/edit/write calls,
//! the re-reads of seen files, the checks after edits, how `edit` found the text it replaced, the
//! symbol index queries, and the neighbour prefetch of a read.

use super::super::{count, duration, optional, required, uuid, EventRule};

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
/// the edits in them, patches rejected for stale anchors and the stale anchors, failed edits, and
/// whole-file writes (`fs.write`: all, those that created the file, and failed ones).
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
        ("fs_write_count", optional(count())),
        ("fs_write_created_count", optional(count())),
        ("fs_write_failed_count", optional(count())),
    ],
};

/// Diff-only re-reads for one session (v2), emitted at session end beside `agent session ended`
/// only when the session re-read a file it had seen: whole-file re-reads shown as one
/// "unchanged" line, as a diff against the seen version, or whole (the seen version left the
/// context, or the diff was no smaller). Counts only: never a path or file content.
pub(in crate::catalog) const AGENT_FS_REREADS: EventRule = EventRule {
    name: "agent fs rereads",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("fs_reread_unchanged_count", required(count())),
        ("fs_reread_diff_count", required(count())),
        ("fs_reread_full_count", required(count())),
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

/// Kernel `fs.symbol` adoption (v2), emitted at session end beside `agent session ended` (which sits
/// near the backend's 128-key cap) only when the session asked a query: queries answered from the
/// project's symbol index, those answered before its first full pass (partial), and the definitions
/// and references returned. Counts only: never a name or a path.
pub(in crate::catalog) const AGENT_SYMBOL_INDEX: EventRule = EventRule {
    name: "agent symbol index",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("symbol_query_count", required(count())),
        ("symbol_query_partial_count", required(count())),
        ("symbol_definition_count", required(count())),
        ("symbol_reference_count", required(count())),
    ],
};

/// Kernel `fs.read` neighbour prefetch (v2), emitted at session end beside `agent session ended`
/// (which sits near the backend's 128-key cap) only when a read prefetched: reads that paged their
/// likely next reads (test file, imports, callers) out to the spill store, the handles issued, those
/// the model went on to use (a `ctx-*` read or an `fs-read` of the file) and those left unused, and
/// reads the learned share left without handles. Counts only: never a path.
pub(in crate::catalog) const AGENT_PREFETCH: EventRule = EventRule {
    name: "agent prefetch",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("prefetch_read_count", required(count())),
        ("prefetch_issued_count", required(count())),
        ("prefetch_used_count", required(count())),
        ("prefetch_unused_count", required(count())),
        ("prefetch_skipped_count", required(count())),
    ],
};

/// How the kernel's `(fs-edit path (hash 'old old-str 'new new-str))` found the text it replaced (v2), emitted at
/// session end beside `agent session ended` (which sits near the backend's 128-key cap) only when
/// an edit matched or missed: edits applied by each step of its normalization ladder (exact,
/// trailing whitespace ignored, indentation ignored, Unicode punctuation folded, literal escapes
/// unescaped), and edits refused for an `old-str` carrying `fs-read` anchor prefixes, failed as
/// ambiguous (the first step to find it found it more than once), failed as not found, or failed
/// as not found while `new-str` is in the file once (the edit looks already applied). Counts only: never a path, a string, or file content.
pub(in crate::catalog) const AGENT_EDIT_MATCHES: EventRule = EventRule {
    name: "agent edit matches",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("edit_exact_count", required(count())),
        ("edit_trailing_whitespace_count", required(count())),
        ("edit_indentation_count", required(count())),
        ("edit_unicode_punctuation_count", required(count())),
        ("edit_unescaped_count", required(count())),
        ("edit_anchor_prefix_refused_count", required(count())),
        ("edit_ambiguous_count", required(count())),
        ("edit_not_found_count", required(count())),
        ("edit_already_applied_count", required(count())),
    ],
};
