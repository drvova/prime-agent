//! Agent family traffic: one display-only entry per accepted agent-message delivery, for the
//! user's Traffic pane. Entries never enter a model's context and never touch a transcript: the
//! receiving worker reports them to the supervisor, which keeps a bounded in-memory ring per root
//! tree and pushes new entries to the clients that subscribed.

use serde::{Deserialize, Serialize};

use crate::JsonMap;

/// The longest preview one entry carries.
pub const AGENT_TRAFFIC_PREVIEW_MAX_CHARS: usize = 240;
/// The entries the supervisor keeps per root tree (oldest drop first).
pub const AGENT_TRAFFIC_RING_CAPACITY: usize = 200;

/// One accepted agent-message delivery, as the receiving worker saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTrafficEntry {
    /// The receiving session's root tree (filled by the supervisor from the worker descriptor).
    #[serde(default)]
    pub root_active_session_id: String,
    /// The sender's display name and live id (a client-origin sender has no id).
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_active_session_id: Option<String>,
    /// The receiver's display name and live id.
    pub to: String,
    pub to_active_session_id: String,
    /// The sender's relationship to the receiver (`parent`, `sibling`, `child`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationship: Option<String>,
    /// `direct` or `broadcast`.
    pub kind: String,
    /// The receipt's delivery status (`delivered`, `queued`, ...).
    pub outcome: String,
    /// The body's length in characters.
    pub chars: u64,
    /// A sanitized one-line preview of at most [`AGENT_TRAFFIC_PREVIEW_MAX_CHARS`] characters.
    pub preview: String,
    pub timestamp_ms: u64,
    #[serde(flatten)]
    pub rest: JsonMap,
}

/// A message body as a display preview: control characters dropped, whitespace runs collapsed to
/// one space, and at most [`AGENT_TRAFFIC_PREVIEW_MAX_CHARS`] characters (an overlong body ends
/// in `…`).
#[must_use]
pub fn sanitize_agent_traffic_preview(body: &str) -> String {
    let cleaned: String = body
        .chars()
        .map(|character| {
            if character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .filter(|character| !character.is_control())
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= AGENT_TRAFFIC_PREVIEW_MAX_CHARS {
        return collapsed;
    }
    let mut preview: String = collapsed
        .chars()
        .take(AGENT_TRAFFIC_PREVIEW_MAX_CHARS - 1)
        .collect();
    preview.push('…');
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preview_is_one_clean_line_within_the_cap() {
        assert_eq!(
            sanitize_agent_traffic_preview("  hello\n\tthere\u{1b}[31m  you "),
            "hello there[31m you"
        );
        assert_eq!(
            sanitize_agent_traffic_preview(&"z".repeat(300)),
            format!("{}…", "z".repeat(239))
        );
    }
}
