//! One event shared by two capabilities: `mcp` owns its `kernel_mcp_login_*` counts, `session`
//! (`rlm.session.search`) its `session_search_*` counts.

use super::super::{count, required, uuid, EventRule};

/// Kernel `mcp.login` and past-session recall (`rlm.session.search`, `rlm.session.messages(
/// session=...)`) adoption for one session (v2), emitted at session end beside `agent session
/// ended` (which sits at the backend's 128-key cap) only when the session used them: logins by
/// outcome (logged in, failed, no interactive login in the run), searches, the hits they
/// returned, saved-transcript reads, and failed recall calls. Counts only: never a server, a
/// query, a session, or a snippet.
pub(in crate::catalog) const AGENT_KERNEL_LOGIN_AND_RECALL: EventRule = EventRule {
    name: "agent kernel login and recall",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("kernel_mcp_login_count", required(count())),
        ("kernel_mcp_login_failed_count", required(count())),
        ("kernel_mcp_login_unavailable_count", required(count())),
        ("session_search_count", required(count())),
        ("session_search_hit_count", required(count())),
        ("session_search_read_count", required(count())),
        ("session_search_failed_count", required(count())),
    ],
};
