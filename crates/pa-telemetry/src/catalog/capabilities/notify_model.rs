//! One event shared by two capabilities: `notify` owns its `notify_*` counts, `model` (for
//! `model.set_effort`) its `effort_*` counts.

use super::super::{count, required, uuid, EventRule};

/// Kernel `notify(...)` and `model.set_effort(...)` adoption for one session (v2), emitted at
/// session end beside `agent session ended` (which sits near the backend's 128-key cap) only when
/// the session pinged or switched effort: pings, pings nobody was attached to see, and effort
/// switches applied and refused. Counts only: never a message or a level.
pub(in crate::catalog) const AGENT_REPL_NOTIFY: EventRule = EventRule {
    name: "agent repl notify",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("notify_count", required(count())),
        ("notify_undelivered_count", required(count())),
        ("effort_set_count", required(count())),
        ("effort_refused_count", required(count())),
    ],
};
