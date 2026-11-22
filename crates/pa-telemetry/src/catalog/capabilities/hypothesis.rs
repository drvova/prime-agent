//! The `hypothesis` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel `rlm.hypotheses(...)` adoption for one session (v2), emitted at session end beside
/// `agent session ended` (which sits near the backend's 128-key cap) only when a run finished:
/// runs, the approaches they tried (one worktree and child each), the approaches that passed the
/// acceptance check, and the runs no approach won. Counts only: never a task, an approach, a
/// check, or a diff.
pub(in crate::catalog) const AGENT_REPL_HYPOTHESES: EventRule = EventRule {
    name: "agent repl hypotheses",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("hypotheses_run_count", required(count())),
        ("hypotheses_approach_count", required(count())),
        ("hypotheses_passed_count", required(count())),
        ("hypotheses_no_winner_count", required(count())),
    ],
};
