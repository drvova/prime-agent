//! The `ctx` capability: spills into the session's spill store and the `ctx.*` reads.

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
