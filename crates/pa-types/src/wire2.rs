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

/// The arguments of the `session.create` call: which session it opens (by
/// id, by transcript path, or a new one), where, and what its build is
/// chosen from. Every member is optional; a create with none opens a new
/// session in the runtime's own session dir and working directory. Its
/// debug form names the launch environment's and the options' keys, never
/// their values (an API key, a token in the environment).
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreate {
    /// The id of a session to open; with no `path`, its transcript is
    /// `<sessions dir>/<id>.jsonl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The transcript to open (a resumed session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The session's working directory; absent, a resumed session's own
    /// (its header's), else the runtime's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The session keeps nothing and ends with the client that created it
    /// (`--no-session`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
    /// The directory a new session's transcript is made in (`--session-dir`);
    /// absent, the runtime's own session dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    /// The environment of the client that asked (the old daemon's
    /// `launchEnv`, `daemon::launch_env`): the session runs in it over the
    /// runtime's own process environment, minus the daemon-internal vars and
    /// `RLM_DEPTH`. It may carry secrets: the runtime keeps it in memory for
    /// the session's lifetimes in this process only, never in its stores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_env: Option<std::collections::BTreeMap<String, String>>,
    /// What the client chose for the session's build (pa-runtime's
    /// `SessionOptions`): its model, thinking level, prompts, resources,
    /// autonomous run, goal, execution mode, and what it runs on
    /// ([`SessionCredentials`] members), its model scope, telemetry switch
    /// and parent ([`SessionParent`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

impl std::fmt::Debug for SessionCreate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionCreate")
            .field("session", &self.session)
            .field("path", &self.path)
            .field("cwd", &self.cwd)
            .field("ephemeral", &self.ephemeral)
            .field("session_dir", &self.session_dir)
            .field(
                "launch_env",
                &self
                    .launch_env
                    .as_ref()
                    .map(|env| env.keys().collect::<Vec<_>>()),
            )
            .field(
                "options",
                &self
                    .options
                    .as_ref()
                    .and_then(Value::as_object)
                    .map(|options| options.keys().collect::<Vec<_>>()),
            )
            .finish()
    }
}

/// The session a new one is created under (a scoped child: the agents
/// view's new session inside a subtree), as `session.create`'s
/// `options.parent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionParent {
    /// The parent's transcript: the new header names it, and the new session
    /// inherits its credential pin unless it pins its own.
    pub path: String,
    /// How deep the new session is under the tree's root.
    pub rlm_depth: u32,
}

/// An API key a client gives a session (`--api-key`, `session.create`'s
/// `options.apiKey`): its JSON form is the key itself, and it is never
/// printed ([`std::fmt::Debug`] shows no part of it).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        ApiKey(key.into())
    }

    /// The key, for the request that sends it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ApiKey, ClientFrame, Outcome, ServerFrame, SessionCreate, SessionParent, Since};

    /// `session.create`'s arguments have their wire form, and their debug form
    /// shows nothing of a key or the environment's values.
    #[test]
    fn a_create_has_its_wire_form_and_hides_its_key() {
        let create = SessionCreate {
            path: Some("/s/a.jsonl".to_string()),
            ephemeral: false,
            session_dir: Some("/s".to_string()),
            launch_env: Some([("TOKEN".to_string(), "env-secret".to_string())].into()),
            options: Some(json!({
                "apiKey": ApiKey::new("sk-secret"),
                "parent": SessionParent { path: "/s/p.jsonl".to_string(), rlm_depth: 2 },
            })),
            ..SessionCreate::default()
        };
        assert_eq!(
            (
                serde_json::to_value(&create).unwrap(),
                ["sk-secret", "env-secret"].map(|secret| format!("{create:?}").contains(secret)),
                format!("{:?}", ApiKey::new("sk-secret")),
            ),
            (
                json!({
                    "path": "/s/a.jsonl",
                    "sessionDir": "/s",
                    "launchEnv": { "TOKEN": "env-secret" },
                    "options": {
                        "apiKey": "sk-secret",
                        "parent": { "path": "/s/p.jsonl", "rlmDepth": 2 },
                    },
                }),
                [false, false],
                "ApiKey(<redacted>)".to_string(),
            )
        );
    }

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
