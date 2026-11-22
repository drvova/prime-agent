//! The `usage` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel `usage.get()` adoption for one session (v2), emitted at session end beside
/// `agent session ended` (which sits near the backend's 128-key cap) only when the session read
/// its usage. A count only: never a cost, a token number, a provider, or a quota.
pub(in crate::catalog) const AGENT_USAGE_READS: EventRule = EventRule {
    name: "agent usage reads",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("usage_read_count", required(count())),
    ],
};
