//! The event log's vocabulary: everything that happens is an event on a
//! topic, subscriptions turn matching events into tasks, and a task runs in
//! its session's lane.
//!
//! - [`topic`]: topics, MQTT-style filters (`+`, `#`), the matching trie,
//!   and which topics are live only (fanned out, never stored).
//! - [`subscription`]: who listens, under which condition, and what a match
//!   does (a [`Route`]); the [`Task`] a route queues.
//! - [`agent`]: the payloads of a session's stored topics: what
//!   `agent/<id>/input` carries in and `agent/<id>/done` reports out.
//!
//! An event's trust says where it came from. An event caused by another can
//! only be as trusted as its cause, and a chain of causes is bounded by
//! [`MAX_HOPS`], so subscriptions that publish cannot loop forever.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod agent;
pub mod subscription;
pub mod topic;

pub use agent::{AgentDone, AgentInput, TaskFailure};

pub use subscription::{
    AgentTarget, Predicate, Priority, Route, Scope, Subscription, Task, TaskState,
};
pub use topic::{Topic, TopicError, TopicFilter, TopicTrie};

/// The longest chain of causes an event may end: the event caused by the
/// eighth event of a chain is refused.
pub const MAX_HOPS: u8 = 8;

/// An event's id, unique in the log.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub String);

/// Where an event came from, which bounds what a task it starts may do.
///
/// `Owner` is the user and their own sessions and sources; `Untrusted` is
/// anything carrying outside content (a webhook body, a fetched page) and
/// everything such an event causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Owner,
    Untrusted,
}

impl Trust {
    /// The lower of two trusts: what an event caused by one source of each
    /// may carry.
    #[must_use]
    pub fn lowest(self, other: Trust) -> Trust {
        self.max(other)
    }
}

/// The event that caused another, as the caused event records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cause {
    pub id: EventId,
    pub trust: Trust,
    /// The cause's own place in its chain: 0 for an event nothing caused.
    pub hops: u8,
}

/// An event before the log has given it an id, a sequence number, and a time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewEvent {
    pub topic: Topic,
    /// What published it: a source's name, a session id, `runtime`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub payload: Value,
    pub trust: Trust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    /// Where an answer to this event is published, when one is expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<Topic>,
}

/// An event caused by another would end a chain longer than [`MAX_HOPS`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("event {cause:?} is the end of a chain of {MAX_HOPS} causes; what it causes is dropped")]
pub struct HopLimit {
    pub cause: EventId,
}

/// An event in the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: EventId,
    /// The log's order: each stored event's is one more than the last's,
    /// from 1. A live-only event ([`Topic::is_live_only`]) is never stored
    /// and has 0.
    pub seq: u64,
    /// Unix milliseconds.
    pub ts: u64,
    #[serde(flatten)]
    pub event: NewEvent,
}

impl Event {
    /// Its place in its chain of causes: 0 when nothing caused it.
    #[must_use]
    pub fn hops(&self) -> u8 {
        self.event.cause.as_ref().map_or(0, |cause| cause.hops + 1)
    }

    /// A new event this one causes, on `topic` from `source`, carrying at most
    /// this event's trust.
    ///
    /// # Errors
    ///
    /// This event already ends a chain of [`MAX_HOPS`] causes.
    pub fn cause(
        &self,
        topic: Topic,
        source: impl Into<String>,
        payload: Value,
        trust: Trust,
    ) -> Result<NewEvent, HopLimit> {
        let hops = self.hops();
        if hops + 1 >= MAX_HOPS {
            return Err(HopLimit {
                cause: self.id.clone(),
            });
        }
        Ok(NewEvent {
            topic,
            source: source.into(),
            session: None,
            payload,
            trust: trust.lowest(self.event.trust),
            cause: Some(Cause {
                id: self.id.clone(),
                trust: self.event.trust,
                hops,
            }),
            reply: None,
        })
    }
}

#[cfg(test)]
mod tests;
