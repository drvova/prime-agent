//! Crash visibility on the wire: what a client is told when a hosted session
//! crashes, recovers or is given up on, and when the daemon process itself
//! died and was restarted by its restart owner.
//!
//! Every field is a primitive about the fault. Nothing here carries prompt,
//! transcript or file content.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One session fault: the `session_crashed` event's payload and the
/// `lastCrash` object on a `list` row (which carries more identifying fields;
/// they are ignored here).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionCrashInfo {
    /// `panic` (contained), `stuck` (a host call outlived its interrupt) or
    /// `daemon_restart` (the daemon died under the session's turn).
    pub kind: String,
    pub message: String,
    /// `file:line:column` of a panic; empty otherwise.
    pub location: String,
    pub thread: String,
    pub at: String,
}

/// The session events the daemon itself sends about a hosted session's
/// health (the session cannot: it is down).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionFaultEvent {
    /// The session went down; the daemon is rebuilding it.
    SessionCrashed(SessionCrashInfo),
    /// The rebuilt session is serving again after `failures` consecutive
    /// failures.
    SessionRecovered {
        #[serde(default)]
        failures: u32,
    },
    /// The daemon gave up rebuilding the session after `failures`
    /// consecutive failures.
    SessionFailed {
        #[serde(default)]
        failures: u32,
    },
}

impl SessionFaultEvent {
    /// The fault a session event carries, when it is one. Every other event
    /// costs one string comparison.
    #[must_use]
    pub fn from_event(event: &Value) -> Option<Self> {
        match event.get("type").and_then(Value::as_str) {
            Some("session_crashed" | "session_recovered" | "session_failed") => {
                Self::deserialize(event).ok()
            }
            Some(_) | None => None,
        }
    }
}

/// What the restart owner did about one daemon death.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonDeathOutcome {
    /// The daemon was started again.
    Restarted,
    /// Too many deaths in a row: the restart owner stopped. The next client
    /// launch starts a daemon again.
    GaveUp,
}

/// One abnormal end of the daemon process, as its restart owner recorded it:
/// a line of the `daemon-deaths.jsonl` log and the `daemonDeath` field of the
/// `daemon_hello` a daemon started after it sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonDeathRecord {
    pub at: String,
    /// The dead daemon's process id; 0 when it could not even be started.
    pub pid: u32,
    /// Its exit code, when it exited on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The signal that killed it, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Why it could not be started, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub uptime_ms: u64,
    /// Sessions the daemon was hosting; absent when they could not be counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_sessions: Option<u64>,
    /// Of those, the sessions whose turn was running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_sessions: Option<u64>,
    /// Deaths in a row without a stable lifetime between them, this one
    /// included.
    pub consecutive: u32,
    pub outcome: DaemonDeathOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_list_rows_last_crash_and_the_crash_event_read_as_the_same_fault() {
        let fault = SessionCrashInfo {
            kind: "panic".to_string(),
            message: "boom".to_string(),
            location: "src/a.rs:1:2".to_string(),
            thread: "session-abc".to_string(),
            at: "2026-01-01T00:00:00.000Z".to_string(),
        };
        let row: SessionCrashInfo = serde_json::from_value(json!({
            "workerId": "w", "sessionFile": "/s.jsonl", "kind": "panic", "message": "boom",
            "location": "src/a.rs:1:2", "thread": "session-abc", "at": "2026-01-01T00:00:00.000Z",
        }))
        .expect("a lastCrash row");
        let event = SessionFaultEvent::from_event(&json!({
            "type": "session_crashed", "kind": "panic", "message": "boom",
            "location": "src/a.rs:1:2", "thread": "session-abc", "at": "2026-01-01T00:00:00.000Z",
        }));
        assert_eq!(
            (
                row,
                event,
                SessionFaultEvent::from_event(&json!({ "type": "turn_end" }))
            ),
            (
                fault.clone(),
                Some(SessionFaultEvent::SessionCrashed(fault)),
                None
            )
        );
    }
}
