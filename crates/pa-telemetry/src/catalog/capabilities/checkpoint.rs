//! The `checkpoint` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel workspace checkpoints (`checkpoint.create/list/restore`) adoption for one session (v2),
/// emitted at session end beside `agent session ended` (which sits at the backend's 128-key cap)
/// only when the session used them: checkpoints taken, listings, and restores by outcome
/// (restored, declined or unanswered, refused by the off-by-default setting) plus failed calls.
/// Counts only: never a label, an id, or a path.
pub(in crate::catalog) const AGENT_REPL_CHECKPOINT: EventRule = EventRule {
    name: "agent repl checkpoint",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("checkpoint_created_count", required(count())),
        ("checkpoint_list_count", required(count())),
        ("checkpoint_restored_count", required(count())),
        ("checkpoint_restore_declined_count", required(count())),
        ("checkpoint_restore_disabled_count", required(count())),
        ("checkpoint_failed_count", required(count())),
    ],
};
