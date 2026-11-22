//! The `ctx` capability owns two events: spills into the session's spill store with the `ctx-*`
//! reads, and the surprise-weighted previews of paged-out output.

use super::super::{count, enum_rule, required, uuid, EventRule};

/// Paged-out output for one session (v2), emitted at session end beside `agent session ended`
/// (which sits near the backend's 128-key cap) only when the session spilled or read paged-out
/// output: spills (cut cell output and thinned results written to the session's spill store), their
/// size as a bucket, spills the store could not keep, evictions, and the kernel `ctx.*` reads.
/// Counts and a size bucket only: never a handle, a pattern, or any output text.
pub(in crate::catalog) const AGENT_CONTEXT_PAGING: EventRule = EventRule {
    name: "agent context paging",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("spill_count", required(count())),
        (
            "spill_bytes_bucket",
            required(enum_rule(
                &[
                    "none", "lt_1mb", "lt_16mb", "lt_64mb", "lt_256mb", "ge_256mb",
                ],
                "none",
            )),
        ),
        ("spill_not_stored_count", required(count())),
        ("spill_evicted_count", required(count())),
        ("ctx_page_count", required(count())),
        ("ctx_grep_count", required(count())),
        ("ctx_tail_count", required(count())),
        ("ctx_info_count", required(count())),
    ],
};

/// Surprise-weighted previews (v2), emitted at session end beside `agent session ended` (which sits
/// near the backend's 128-key cap) only when the session built one: previews of spilled cell
/// output and thinned tool results, the lines they kept, dropped, folded as repeats, and folded as
/// learned noise, the noise patterns the session learned, and the `ctx-*` reads that followed
/// (fewer means the previews chose well). Counts only: never a line, a pattern, or output.
pub(in crate::catalog) const AGENT_PREVIEWS: EventRule = EventRule {
    name: "agent previews",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("preview_count", required(count())),
        ("preview_kept_line_count", required(count())),
        ("preview_dropped_line_count", required(count())),
        ("preview_repeat_collapsed_count", required(count())),
        ("preview_noise_line_count", required(count())),
        ("preview_noise_pattern_learned_count", required(count())),
        ("preview_followup_read_count", required(count())),
    ],
};
