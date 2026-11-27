//! Subscriptions and the tasks they queue.
//!
//! A subscription listens on a [`TopicFilter`] within a [`Scope`]. An event
//! that matches the filter and the subscription's `when` [`Predicate`], and
//! that its throttle and debounce let through, runs the subscription's
//! [`Route`]: set a state key, publish another event, make one model call
//! without tools, or queue a [`Task`] that starts an agent turn.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{EventId, Topic, TopicFilter, Trust};

/// Whose events a subscription hears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Scope {
    /// Events of one session only.
    Local { session: String },
    /// Events of one session and the subagents it spawned.
    Agent { session: String },
    /// Every event on the log.
    Global,
}

/// A condition on an event's payload. A pointer is a JSON Pointer
/// (`/env`, `/files/0/path`); the empty pointer is the whole payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Predicate {
    /// The value at `pointer` equals `value`.
    Eq {
        pointer: String,
        value: Value,
    },
    /// The payload has a value at `pointer`.
    Exists {
        pointer: String,
    },
    /// The value at `pointer` equals one of `values`.
    In {
        pointer: String,
        values: Vec<Value>,
    },
    All {
        all: Vec<Predicate>,
    },
    Any {
        any: Vec<Predicate>,
    },
    Not {
        not: Box<Predicate>,
    },
}

impl Predicate {
    /// Whether `payload` satisfies the condition.
    #[must_use]
    pub fn holds(&self, payload: &Value) -> bool {
        match self {
            Predicate::Eq { pointer, value } => payload.pointer(pointer) == Some(value),
            Predicate::Exists { pointer } => payload.pointer(pointer).is_some(),
            Predicate::In { pointer, values } => payload
                .pointer(pointer)
                .is_some_and(|found| values.contains(found)),
            Predicate::All { all } => all.iter().all(|predicate| predicate.holds(payload)),
            Predicate::Any { any } => any.iter().any(|predicate| predicate.holds(payload)),
            Predicate::Not { not } => !not.holds(payload),
        }
    }
}

/// A task's place in its session's lane: lower runs first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Steers the running turn at its next step; runs as a prompt when idle.
    Steer,
    /// The user's own input.
    Prompt,
    /// After the running turn.
    FollowUp,
    /// A subscription's event.
    Event,
    /// Scheduled or housekeeping work.
    Background,
}

/// Which session an agent route's turn runs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentTarget {
    /// An existing session.
    Session { session: String },
    /// The session the matching event was published in (an `agent/<id>/input`
    /// event's `<id>`): one subscription serves every session's input.
    Origin,
    /// A new session, started for the event.
    Spawn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
}

/// What a matching event does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Route {
    /// Store the payload under a state key.
    Set { key: String },
    /// Publish the payload again, on another topic.
    Publish { topic: Topic },
    /// One model call without tools; its answer is published on the event's
    /// `reply` topic.
    Call {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Queue a turn that starts from `prompt` and the event. An empty
    /// `prompt` is the event's own input: its payload is an
    /// [`AgentInput`](super::AgentInput) and the turn starts from its text.
    Agent {
        target: AgentTarget,
        prompt: String,
        priority: Priority,
    },
}

/// A subscription as the log keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    pub scope: Scope,
    pub filter: TopicFilter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Predicate>,
    /// At most one match per window; a match inside it wakes nobody.
    #[serde(default)]
    pub throttle_ms: u64,
    /// A match runs once no other has followed it for this long.
    #[serde(default)]
    pub debounce_ms: u64,
    pub route: Route,
    /// The trust of whoever made it: a subscription made by an untrusted task
    /// queues untrusted tasks.
    pub trust: Trust,
}

/// Where a task is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One unit of a session's lane: a turn to run for an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub session: String,
    pub priority: Priority,
    pub state: TaskState,
    /// The event that queued it.
    pub trigger: EventId,
    pub prompt: String,
    pub trust: Trust,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
