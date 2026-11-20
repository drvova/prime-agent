//! The `better` capability (the kernel `better(...)` and the `/better` command share one event).

use super::super::{count, required, uuid, EventRule};

/// `/better` adoption for one session (v2), emitted at session end beside `agent session ended`
/// (which sits near the backend's 128-key cap) only when a rewrite ran: runs and how each ended
/// (completed, cancelled, failed), and what the user did with an inserted rewrite (sent as is,
/// edited, discarded), the kernel's brief-mode runs and how many needed no fix, and the refine
/// passes verification ran. Counts only: never a prompt, a rewrite, or a path.
pub(in crate::catalog) const AGENT_BETTER_PROMPT: EventRule = EventRule {
    name: "agent better prompt",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("better_run_count", required(count())),
        ("better_completed_count", required(count())),
        ("better_cancelled_count", required(count())),
        ("better_failed_count", required(count())),
        ("better_sent_as_is_count", required(count())),
        ("better_edited_count", required(count())),
        ("better_discarded_count", required(count())),
        ("better_brief_run_count", required(count())),
        ("better_brief_unchanged_count", required(count())),
        ("better_verification_pass_count", required(count())),
    ],
};
