//! The `worktree` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel `worktree` adoption for one session (v2), emitted at session end beside
/// `agent session ended` (which sits near the backend's 128-key cap) only when the session used a
/// worktree: created, diffed, merged into the checkout, committed to a branch, merges that found
/// conflicts, removed, and failed operations. Counts only: never a name, a path, or a diff.
pub(in crate::catalog) const AGENT_WORKTREES: EventRule = EventRule {
    name: "agent worktrees",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("worktree_created_count", required(count())),
        ("worktree_diff_count", required(count())),
        ("worktree_merged_count", required(count())),
        ("worktree_branched_count", required(count())),
        ("worktree_conflict_count", required(count())),
        ("worktree_removed_count", required(count())),
        ("worktree_failed_count", required(count())),
    ],
};
