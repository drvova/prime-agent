//! The `decision` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel `rlm.decide(...)` adoption for one session (v2), emitted at session end beside `agent
/// session ended` only when the session asked for a decision: decisions made, decisions refused
/// at the per-cell bound, and failed decision calls. Counts only: never a question, an option,
/// or a choice.
pub(in crate::catalog) const AGENT_REPL_DECIDE: EventRule = EventRule {
    name: "agent repl decide",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("decide_count", required(count())),
        ("decide_limited_count", required(count())),
        ("decide_failed_count", required(count())),
    ],
};
