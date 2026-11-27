//! The event wire between clients (the terminal UI, the CLI, ACP) and the
//! runtime: a client subscribes to topics on the event log, publishes its
//! input as events, and asks the runtime questions and for changes.
//!
//! Frames are JSON objects tagged by `type`, carried by the private-frame
//! codec ([`crate::daemon::framing`]). A subscription first replays stored
//! events from where the client asks ([`Since`]), says where the replay
//! ended ([`ServerFrame::Backfilled`]), and then streams live events, so a
//! client that reconnects from its last sequence number misses nothing. A
//! live-only event (`Topic::is_live_only`: a turn's stream and tool deltas,
//! the agents view's roster and traffic feeds) is never stored: it reaches a
//! subscription only live, with `seq` 0, and is not a sequence number to
//! resume from.
//! The typed queries and calls arrive with the runtime that answers them;
//! until then a query or call names its operation and carries its arguments.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event_log::{Event, Topic, TopicFilter};

/// The wire's version: a client and a runtime that differ refuse each other
/// at `hello` with a "restart the client" error.
pub const WIRE2_VERSION: u32 = 1;

/// Where a subscription's replay starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum Since {
    /// No replay: live events only.
    Now,
    /// Every stored event after this sequence number.
    After { seq: u64 },
    /// The last `count` stored matching events.
    Tail { count: u32 },
}

/// What a client sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ClientFrame {
    Hello {
        version: u32,
        /// The client's name and version, for the runtime's log.
        client: String,
    },
    Subscribe {
        /// The client's name for this subscription; events carry it back.
        sub_id: String,
        filter: TopicFilter,
        since: Since,
    },
    Unsubscribe {
        sub_id: String,
    },
    /// Publish one event with the client's (owner) trust: a prompt is an
    /// event on `agent/<id>/input`.
    Publish {
        id: String,
        topic: Topic,
        payload: Value,
    },
    /// Read something the log does not stream: a transcript page, the
    /// session list.
    Query {
        id: String,
        name: String,
        #[serde(default)]
        args: Value,
    },
    /// Change something that is not an event: create, rename or delete a
    /// session, change its model.
    Call {
        id: String,
        method: String,
        #[serde(default)]
        args: Value,
    },
}

/// What the runtime sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ServerFrame {
    Welcome {
        version: u32,
        runtime_version: String,
        /// The last stored event's sequence number.
        last_seq: u64,
    },
    /// One event for one of the client's subscriptions.
    Event { sub_id: String, event: Event },
    /// The subscription's replay is done; live events follow.
    Backfilled { sub_id: String, through_seq: u64 },
    /// The answer to a publish, query or call with the same `id`.
    Reply {
        id: String,
        #[serde(flatten)]
        outcome: Outcome,
    },
    /// The runtime is closing the connection.
    Closing { reason: String },
}

/// How a publish, query or call ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Ok {
        result: Value,
    },
    Error {
        message: String,
        /// What a client can act on beyond the message, when the error
        /// carries it: the old daemon's `errorInfo` (a
        /// `pa_types::daemon::DaemonErrorInfo`), such as the missing working
        /// directory a session import offers to replace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        info: Option<Value>,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ClientFrame, Outcome, ServerFrame, Since};

    /// The frames' JSON is the wire contract between separately built
    /// clients and runtimes.
    #[test]
    fn frames_have_their_wire_form() {
        let subscribe: ClientFrame = serde_json::from_value(json!({
            "type": "subscribe",
            "subId": "chat",
            "filter": "agent/3f2a/#",
            "since": { "from": "after", "seq": 41 },
        }))
        .unwrap();
        assert_eq!(
            (
                subscribe,
                serde_json::to_value(ServerFrame::Reply {
                    id: "q1".to_string(),
                    outcome: Outcome::Error {
                        message: "no such session".to_string(),
                        info: None,
                    },
                })
                .unwrap(),
            ),
            (
                ClientFrame::Subscribe {
                    sub_id: "chat".to_string(),
                    filter: crate::event_log::TopicFilter::new("agent/3f2a/#").unwrap(),
                    since: Since::After { seq: 41 },
                },
                json!({ "type": "reply", "id": "q1", "status": "error", "message": "no such session" }),
            )
        );
    }
}
