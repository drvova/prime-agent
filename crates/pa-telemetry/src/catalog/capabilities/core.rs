//! The `core` capability (the `rlm` package root): `rlm.abort`.

use super::super::{count, required, uuid, EventRule};

/// Kernel `rlm.abort` (stop a child's current turn, keep the child) adoption for one session
/// (v2), emitted at session end beside `agent session ended` (which sits at the backend's 128-key
/// cap) only when the session aborted a child: aborts that stopped a turn, aborts of an idle
/// child, and failed calls. Counts only: never a child id or name.
pub(in crate::catalog) const AGENT_RLM_CHILD_ABORT: EventRule = EventRule {
    name: "agent rlm child abort",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("rlm_abort_aborted_count", required(count())),
        ("rlm_abort_idle_count", required(count())),
        ("rlm_abort_failed_count", required(count())),
    ],
};
