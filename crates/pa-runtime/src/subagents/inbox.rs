//! The inbox: an agent message one session sends another is published on
//! `agent/<to>/inbox` (an [`InboxMessage`]), and the runtime's inbox
//! subscription ([`super::INBOX_SUBSCRIPTION`]) delivers it
//! ([`Subagents::deliver_inbox`]) the way the daemon's receiving worker
//! does:
//!
//! - refused while the receiver's agent messaging is paused (`Agent
//!   messaging is paused`, the agents view's pause);
//! - refused while the receiver holds
//!   [`DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION`] undelivered rows;
//! - a receiver in a turn takes it queued, and no wake is spent;
//! - an idle receiver is woken by it, which spends one of the sender's
//!   [`DEFAULT_AGENT_MESSAGE_WAKE_BUDGET`] wakes of that receiver in the
//!   trailing [`DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS`]; past the budget it is
//!   not delivered, and the sender's receipt says `wake_refused` with when
//!   to retry.
//!
//! A delivered message is today's `agent_message` row, steered onto the
//! receiver; a settled child is given its next task over it, and a root in
//! the runtime daemon is woken for it. The sender
//! reads the outcome once its publish returns: the route has run by then.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pa_core::session_engine::agent_messaging::{
    assert_agent_message_queue_capacity, create_agent_message_preview, AgentMessageArrival,
    AgentMessageDeliveryStatus, AgentMessageKind, DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
    DEFAULT_AGENT_MESSAGE_WAKE_BUDGET, DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS,
};
use pa_types::agent_traffic::{sanitize_agent_traffic_preview, AgentTrafficEntry};
use pa_types::event_log::{Event, NewEvent, Topic, Trust};
use serde_json::{json, Value};

use super::{deliver_row, lock, now_ms, wake_on_delivery, Delivery, Node, Subagents};

/// The runtime daemon's agent traffic feed: one live-only event per
/// accepted agent-message delivery, its payload the old `agent_traffic`
/// frame's `entry` (pa-types `AgentTrafficEntry`), display only: nothing
/// stores it, and it reaches no model's context.
pub(crate) const TRAFFIC_TOPIC: &str = "runtime/traffic";

/// What `agent/<to>/inbox` carries: `{id, from, row}`, with
/// `"deliveryMode": "follow_up"` for a follow-up; a client's message
/// (`session.send`) has no `from`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InboxMessage {
    /// The message's id (`agentmsg_…`): its receipt's.
    pub(crate) id: String,
    /// The sending session's id: whose wake budget a wake spends. `None`
    /// for a client's message, which spends none (only sessions can answer
    /// each other without end).
    pub(crate) from: Option<String>,
    /// The `agent_message` row the receiver takes.
    pub(crate) row: Value,
    /// A direct message, or one of a broadcast's (which ends no wait).
    pub(crate) kind: AgentMessageKind,
    /// Steered, or (`follow_up`) queued behind a receiver's running turn; an
    /// idle receiver is woken for either.
    pub(crate) delivery: Delivery,
}

impl InboxMessage {
    /// The event payload.
    pub(crate) fn payload(&self) -> Value {
        let mut payload = json!({ "id": self.id, "row": self.row });
        if let Some(from) = &self.from {
            payload["from"] = json!(from);
        }
        if let Some(kind) = self.kind.to_wire() {
            payload["messageKind"] = json!(kind);
        }
        match self.delivery {
            Delivery::Steer => {}
            Delivery::FollowUp => payload["deliveryMode"] = json!("follow_up"),
        }
        payload
    }

    /// The message a payload carries, when it carries one.
    fn read(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        Some(InboxMessage {
            id: text("id")?,
            from: text("from"),
            row: payload.get("row")?.clone(),
            kind: AgentMessageKind::from_wire(payload.get("messageKind").and_then(Value::as_str)),
            delivery: if text("deliveryMode").as_deref() == Some("follow_up") {
                Delivery::FollowUp
            } else {
                Delivery::Steer
            },
        })
    }
}

/// One receiver's wakes by each sender in the trailing window, oldest first.
#[derive(Default)]
pub(crate) struct WakeBudget(Mutex<HashMap<String, VecDeque<Instant>>>);

impl WakeBudget {
    /// Spends one of `sender`'s wakes at `now`, or answers the milliseconds
    /// until its oldest counted wake leaves the window.
    fn admit(&self, sender: &str, now: Instant) -> Result<(), u64> {
        let window = Duration::from_millis(DEFAULT_AGENT_MESSAGE_WAKE_WINDOW_MS);
        let mut wakes = lock(&self.0);
        // Every sender's aged-out wakes go, so the map holds live windows only.
        wakes.retain(|_, times| {
            while times
                .front()
                .is_some_and(|oldest| now.saturating_duration_since(*oldest) >= window)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        let times = wakes.entry(sender.to_string()).or_default();
        if times.len() >= DEFAULT_AGENT_MESSAGE_WAKE_BUDGET {
            let oldest = times.front().copied().unwrap_or(now);
            let retry_after = window.saturating_sub(now.saturating_duration_since(oldest));
            return Err(u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX));
        }
        times.push_back(now);
        Ok(())
    }

    /// How many wakes `sender` has spent in the window.
    #[cfg(test)]
    pub(crate) fn spent(&self, sender: &str) -> usize {
        lock(&self.0).get(sender).map_or(0, VecDeque::len)
    }
}

impl Subagents {
    /// Delivers the agent message `event` (on `agent/<to>/inbox`) to its
    /// receiver, and answers its sender with how it went: delivered,
    /// queued behind the receiver's turn, refused by the wake budget, or the
    /// error that refused it.
    pub(crate) async fn deliver_inbox(&self, event: &Event) {
        let Some(message) = InboxMessage::read(&event.event.payload) else {
            tracing::error!("an agent message on {} is malformed", event.event.topic);
            return;
        };
        let answer = lock(&self.answers).remove(&message.id);
        let delivered = async {
            let receiver = event
                .event
                .topic
                .levels()
                .nth(1)
                .and_then(|id| self.node(id))
                .filter(|receiver| !receiver.is_disposed())
                .ok_or_else(|| anyhow::anyhow!("the target session is not running"))?;
            let engine = receiver
                .engine()
                .ok_or_else(|| anyhow::anyhow!("the target session is not running"))?;
            // The agents view's pause refuses every delivery.
            receiver.agent_messages.admit()?;
            let agent = engine.session.agent();
            assert_agent_message_queue_capacity(
                agent.steering_previews().len() + agent.follow_up_previews().len(),
                DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
            )?;
            // Only a delivery that wakes an idle receiver spends the
            // sender's budget; one in a turn queues it unbudgeted.
            let queued = agent.state().await.is_streaming;
            if !queued {
                if let Some(from) = &message.from {
                    if let Err(retry_after_ms) = receiver.wakes.admit(from, Instant::now()) {
                        return Ok(AgentMessageDeliveryStatus::WakeRefused { retry_after_ms });
                    }
                }
            }
            let details = &message.row["details"];
            let arrival = AgentMessageArrival {
                message_id: message.id.clone(),
                sender_ids: message.from.iter().cloned().collect(),
                sender_name: details["from"]["sessionName"]
                    .as_str()
                    .or(message.from.as_deref())
                    .or_else(|| details["from"]["clientId"].as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                preview: create_agent_message_preview(
                    details["message"].as_str().unwrap_or_default(),
                ),
                kind: message.kind,
            };
            // A follow-up waits behind a running turn; an idle receiver runs
            // either at once.
            let delivery = if queued {
                message.delivery
            } else {
                Delivery::Steer
            };
            let traffic = self.traffic_entry(&receiver, &message, queued);
            deliver_row(&receiver, message.row.clone(), delivery)?;
            // The receiving side of the receiver's waits hears every
            // delivery it took.
            receiver.waits.deliver(arrival);
            wake_on_delivery(&receiver, event.event.trust).await;
            // Reported once it landed, never on the delivery's own path.
            if let Some(entry) = traffic {
                self.publish_traffic(&receiver, &entry);
            }
            anyhow::Ok(if queued {
                AgentMessageDeliveryStatus::Queued
            } else {
                AgentMessageDeliveryStatus::Delivered
            })
        }
        .await;
        match answer {
            Some(answer) => {
                // A sender that stopped waiting has nothing to hear.
                let _ = answer.send(delivered);
            }
            None => {
                if let Err(error) = delivered {
                    tracing::warn!("agent message {} was not delivered: {error:#}", message.id);
                }
            }
        }
    }
}

impl Subagents {
    /// The traffic pane's entry for `message` landing on `receiver`
    /// (`queued` behind its turn, else delivered), as the old daemon's
    /// receiving worker reports it: the sender's and the receiver's names
    /// and ids, the sender's relationship to the receiver, the kind, the
    /// outcome, the body's length and its preview. Only the runtime daemon
    /// reports traffic: `None` in a headless run.
    fn traffic_entry(
        &self,
        receiver: &Node,
        message: &InboxMessage,
        queued: bool,
    ) -> Option<AgentTrafficEntry> {
        self.daemon()?;
        let details = &message.row["details"];
        let from = &details["from"];
        let body = details["message"].as_str().unwrap_or_default();
        // The old label precedence: name, durable id, live id, client.
        let from_name = ["sessionName", "sessionId", "activeSessionId", "clientId"]
            .iter()
            .find_map(|key| from[*key].as_str())
            .unwrap_or("unknown")
            .to_string();
        let relationship = details["fromRelationship"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                // Two sessions under one parent, or two roots, are siblings;
                // a client is no relative.
                let sender = self.node(message.from.as_deref()?)?;
                let parent =
                    |node: &Node| node.parent_node().map(|parent| parent.session_id.clone());
                (parent(&sender) == parent(receiver)).then(|| "sibling".to_string())
            });
        Some(AgentTrafficEntry {
            root_active_session_id: receiver.root_id(),
            from: from_name,
            from_active_session_id: message.from.clone(),
            to: receiver
                .session_name()
                .unwrap_or_else(|| receiver.session_id.clone()),
            to_active_session_id: receiver.session_id.clone(),
            relationship,
            kind: match message.kind {
                AgentMessageKind::Direct => "direct",
                AgentMessageKind::Broadcast => "broadcast",
            }
            .to_string(),
            outcome: if queued { "queued" } else { "delivered" }.to_string(),
            chars: u64::try_from(body.chars().count()).unwrap_or(u64::MAX),
            preview: sanitize_agent_traffic_preview(body),
            timestamp_ms: now_ms(),
            rest: serde_json::Map::default(),
        })
    }

    /// Publishes `entry` on [`TRAFFIC_TOPIC`], live only, fire and forget.
    fn publish_traffic(&self, receiver: &Node, entry: &AgentTrafficEntry) {
        let Ok(payload) = serde_json::to_value(entry) else {
            return;
        };
        self.outbox.send(NewEvent {
            topic: Topic::new(TRAFFIC_TOPIC).expect("a topic"),
            source: receiver.session_id.clone(),
            session: Some(receiver.session_id.clone()),
            payload,
            trust: Trust::Owner,
            cause: None,
            reply: None,
        });
    }
}

/// Where a sender hears how its message went, by the message's id.
pub(crate) type InboxAnswers = Mutex<
    HashMap<String, tokio::sync::oneshot::Sender<anyhow::Result<AgentMessageDeliveryStatus>>>,
>;
