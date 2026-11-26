//! The inbox: an agent message one session sends another is published on
//! `agent/<to>/inbox` (an [`InboxMessage`]), and the runtime's inbox
//! subscription ([`super::INBOX_SUBSCRIPTION`]) delivers it
//! ([`Subagents::deliver_inbox`]) the way the daemon's receiving worker
//! does:
//!
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
use pa_types::event_log::Event;
use serde_json::{json, Value};

use super::{deliver_row, lock, wake_on_delivery, Subagents};

/// What `agent/<to>/inbox` carries: `{id, from, row}`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InboxMessage {
    /// The message's id (`agentmsg_…`): its receipt's.
    pub(crate) id: String,
    /// The sending session's id: whose wake budget a wake spends.
    pub(crate) from: String,
    /// The `agent_message` row the receiver takes.
    pub(crate) row: Value,
    /// A direct message, or one of a broadcast's (which ends no wait).
    pub(crate) kind: AgentMessageKind,
}

impl InboxMessage {
    /// The event payload.
    pub(crate) fn payload(&self) -> Value {
        let mut payload = json!({ "id": self.id, "from": self.from, "row": self.row });
        if let Some(kind) = self.kind.to_wire() {
            payload["messageKind"] = json!(kind);
        }
        payload
    }

    /// The message a payload carries, when it carries one.
    fn read(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        Some(InboxMessage {
            id: text("id")?,
            from: text("from")?,
            row: payload.get("row")?.clone(),
            kind: AgentMessageKind::from_wire(payload.get("messageKind").and_then(Value::as_str)),
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
            let agent = engine.session.agent();
            assert_agent_message_queue_capacity(
                agent.steering_previews().len() + agent.follow_up_previews().len(),
                DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
            )?;
            // Only a delivery that wakes an idle receiver spends the
            // sender's budget; one in a turn queues it unbudgeted.
            let queued = agent.state().await.is_streaming;
            if !queued {
                if let Err(retry_after_ms) = receiver.wakes.admit(&message.from, Instant::now()) {
                    return Ok(AgentMessageDeliveryStatus::WakeRefused { retry_after_ms });
                }
            }
            let details = &message.row["details"];
            let arrival = AgentMessageArrival {
                message_id: message.id.clone(),
                sender_ids: vec![message.from.clone()],
                sender_name: details["from"]["sessionName"]
                    .as_str()
                    .unwrap_or(&message.from)
                    .to_string(),
                preview: create_agent_message_preview(
                    details["message"].as_str().unwrap_or_default(),
                ),
                kind: message.kind,
            };
            deliver_row(&receiver, message.row)?;
            // The receiving side of the receiver's waits hears every
            // delivery it took.
            receiver.waits.deliver(arrival);
            wake_on_delivery(&receiver, event.event.trust).await;
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

/// Where a sender hears how its message went, by the message's id.
pub(crate) type InboxAnswers = Mutex<
    HashMap<String, tokio::sync::oneshot::Sender<anyhow::Result<AgentMessageDeliveryStatus>>>,
>;
