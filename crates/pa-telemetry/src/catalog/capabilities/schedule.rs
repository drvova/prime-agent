//! The `schedule` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel session-owned scheduled jobs (`schedule.add/list/cancel`) adoption for one session
/// (v2), emitted at session end beside `agent session ended` (which sits at the backend's 128-key
/// cap) only when the session used them: jobs added by kind, listings, cancels, and refused calls.
/// Counts only: never a prompt, a schedule, or a job id.
pub(in crate::catalog) const AGENT_REPL_SCHEDULE: EventRule = EventRule {
    name: "agent repl schedule",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("schedule_once_added_count", required(count())),
        ("schedule_recurring_added_count", required(count())),
        ("schedule_list_count", required(count())),
        ("schedule_cancelled_count", required(count())),
        ("schedule_rejected_count", required(count())),
    ],
};
