//! The `core` capability owns three events: aborting a child's turn, the adaptive delegation
//! depth bound, and collects that met a child waiting on its parent.

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

/// The adaptive delegation depth bound for one session (v2), emitted at session end beside `agent
/// session ended` (which sits near the backend's 128-key cap) only when the bound moved or a
/// brief followed it: raises after children came back checked and passing, drops after failed
/// or timed-out children, and brief-mode `better` runs scaled to the remaining bound. Counts
/// only: never a child id, a name, or a brief.
pub(in crate::catalog) const AGENT_RLM_DELEGATION_DEPTH: EventRule = EventRule {
    name: "agent rlm delegation depth",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("rlm_depth_raised_count", required(count())),
        ("rlm_depth_lowered_count", required(count())),
        ("rlm_depth_scaled_brief_count", required(count())),
    ],
};

/// `rlm.collect` rows that reported a child waiting on the collecting session (v2), emitted at
/// session end beside `agent session ended` (which sits near the backend's 128-key cap) only when
/// a collect returned such a row: the child was parked in an `agent_message.wait` for its
/// parent's message, so the collect did not wait for it. Counts only: never a child id or name.
pub(in crate::catalog) const AGENT_RLM_COLLECT_WAITS: EventRule = EventRule {
    name: "agent rlm collect waits",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("rlm_collect_waiting_on_you_count", required(count())),
    ],
};
