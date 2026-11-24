//! The event bus's wire vocabulary: what a session asks the daemon's event bus
//! (`event_bus` supervisor command) and the records the bus keeps.
//!
//! An event source is a named service with a JSON schema for its payloads,
//! owned by the session that defined it; only that session publishes it. A
//! subscription names a source, an optional filter (payload keys whose values
//! must be equal), and the prompt its session's turn starts from. Publishing
//! validates the payload against the source's schema and delivers one turn to
//! each matching subscription's session.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// How an event reaches a subscribed session: as a follow-up after its
/// running turn (the default), or steering into it at the next boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventDeliveryMode {
    #[default]
    FollowUp,
    Steer,
}

impl EventDeliveryMode {
    /// The `deliveryMode` value a message delivery carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FollowUp => "follow_up",
            Self::Steer => "steer",
        }
    }
}

/// One request to the event bus, tagged by `op`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum EventBusRequest {
    /// Define (or, by its owner, redefine) a source.
    DefineSource {
        name: String,
        #[serde(default)]
        description: String,
        schema: Value,
    },
    /// Remove one of the requester's own sources and the subscriptions to it.
    RemoveSource { name: String },
    /// Publish one event of a source the requester owns.
    Publish { source: String, payload: Value },
    /// Subscribe the requester to a source.
    Subscribe {
        source: String,
        #[serde(default)]
        filter: Map<String, Value>,
        prompt: String,
        #[serde(default)]
        delivery_mode: EventDeliveryMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Cancel one of the requester's subscriptions.
    Unsubscribe { subscription_id: String },
    /// Every source, and the requester's subscriptions.
    List,
}

/// A defined source, as the bus keeps and lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSourceRecord {
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// The owning session's durable id.
    pub owner_session_id: String,
    /// The owner's name when it had one at definition, else its id.
    pub owner_name: String,
    pub defined_at: u64,
    pub published_count: u64,
}

/// A subscription, as the bus keeps and lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSubscriptionRecord {
    pub id: String,
    pub source: String,
    pub filter: Map<String, Value>,
    pub prompt: String,
    pub delivery_mode: EventDeliveryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The subscribed session's durable id.
    pub session_id: String,
    /// The subscribed session's working directory (where a saved session
    /// woken for an event is looked up).
    pub cwd: String,
    pub created_at: u64,
    pub delivered_count: u64,
}
