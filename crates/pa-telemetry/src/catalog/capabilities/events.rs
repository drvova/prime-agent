//! The `events` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel event bus (`event.*`) adoption for one session (v2), emitted at session end beside
/// `agent session ended` (which sits near the backend's 128-key cap) only when the session used
/// it: sources defined (all kinds, then the timer, folder-watch and webhook ones), events published and the subscriptions they matched, subscriptions made
/// and cancelled, and refused calls. Counts only: never a source name, a payload, or a prompt.
pub(in crate::catalog) const AGENT_KERNEL_EVENTS: EventRule = EventRule {
    name: "agent kernel events",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("event_source_define_count", required(count())),
        ("event_source_define_timer_count", required(count())),
        ("event_source_define_watch_count", required(count())),
        ("event_source_define_webhook_count", required(count())),
        ("event_publish_count", required(count())),
        ("event_publish_matched_count", required(count())),
        ("event_subscribe_count", required(count())),
        ("event_unsubscribe_count", required(count())),
        ("event_request_failed_count", required(count())),
    ],
};
