//! One event shared by two capabilities: `resource_reload` owns its `repl_reload_*` counts,
//! `settings` its `repl_settings_get_count`.

use super::super::{count, required, uuid, EventRule};

/// Kernel `rlm.reload()` and `settings.get()` adoption for one session (v2), emitted at session
/// end beside `agent session ended` (which sits near the backend's 128-key cap) only when the
/// session reloaded or read its settings: reloads, the skills they added, and settings reads.
/// Counts only: never a skill name, a path, or a setting value.
pub(in crate::catalog) const AGENT_REPL_RELOAD: EventRule = EventRule {
    name: "agent repl reload",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("repl_reload_count", required(count())),
        ("repl_reload_skills_added_count", required(count())),
        ("repl_settings_get_count", required(count())),
    ],
};
