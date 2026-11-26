//! The family's `agent_message.*`: the parent, the siblings and the children
//! of one session, each message published on its receiver's inbox
//! ([`super::inbox`]) as today's `agent_message` row, and the late receipts
//! of fire-and-forget sends.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use pa_core::session::manager::format_iso_now;
use pa_core::session_engine::agent_messaging::{
    create_agent_observe_message_preview, create_agent_session_message_id,
    create_agent_session_message_prompt, create_agent_session_message_row, wait_for_arrival,
    AgentFamilyMember, AgentFamilyRelationship, AgentFamilyStatus, AgentMessageController,
    AgentMessageDeliveryStatus, AgentMessagePromptPayload, AgentMessageReceipt,
    AgentMessageSendInput, AgentMessageWaitOutcome, AgentMessageWaitRequest, AgentObserveActivity,
    AgentObserveController, AgentObserveListResult, AgentObserveRecentMessagesResult,
    AgentObserveSummary, AgentSessionMessageRowPayload,
};
use serde_json::{json, Value};

use pa_types::event_log::{NewEvent, Topic, Trust, MAX_HOPS};

use super::inbox::InboxMessage;
use super::{lock, now_ms, Delivery, Node, Subagents};

/// The family roster and delivery over one node's place in the tree.
pub(crate) struct FamilyMessages(pub(crate) Arc<Node>);

impl AgentMessageController for FamilyMessages {
    async fn family(&self) -> anyhow::Result<Vec<AgentFamilyMember>> {
        let node = &self.0;
        let mut members = Vec::new();
        let mut siblings = Vec::new();
        if let Some(parent) = node.parent_node() {
            members.push(AgentFamilyMember {
                relationship: AgentFamilyRelationship::Parent,
                id: parent.session_id.clone(),
                name: parent.session_name().filter(|name| !name.is_empty()),
                aliases: Vec::new(),
            });
            for sibling in parent.children.lock().await.clone() {
                if Arc::ptr_eq(&sibling.node, node) {
                    continue;
                }
                siblings.push(AgentFamilyMember {
                    relationship: AgentFamilyRelationship::Sibling,
                    id: sibling.node.session_id.clone(),
                    name: sibling.node.session_name(),
                    aliases: vec![sibling.rlm_child_id.clone()],
                });
            }
        }
        siblings.sort_by(|left, right| left.member_name().cmp(right.member_name()));
        let mut children = Vec::new();
        for child in node.children.lock().await.clone() {
            children.push(AgentFamilyMember {
                relationship: AgentFamilyRelationship::Child,
                id: child.node.session_id.clone(),
                name: child.node.session_name(),
                aliases: vec![child.rlm_child_id.clone()],
            });
        }
        children.sort_by(|left, right| left.member_name().cmp(right.member_name()));
        members.extend(siblings);
        members.extend(children);
        Ok(members)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the target lookup, the reply marks and the delivery read linearly"
    )]
    async fn send_agent_message(
        &self,
        input: AgentMessageSendInput,
    ) -> anyhow::Result<AgentMessageReceipt> {
        let node = &self.0;
        if input.target == node.session_id {
            anyhow::bail!("Agent messaging cannot target the sending session");
        }
        // The sender's relationship to the target: the parent (the sender
        // is its child), a sibling, or one of the sender's own children (the
        // sender is their parent).
        let mut target: Option<(Arc<Node>, AgentFamilyRelationship)> = None;
        if let Some(parent) = node.parent_node() {
            if parent.session_id == input.target {
                target = Some((Arc::clone(&parent), AgentFamilyRelationship::Child));
            }
            if target.is_none() {
                for sibling in parent.children.lock().await.iter() {
                    if !Arc::ptr_eq(&sibling.node, node) && sibling.node.session_id == input.target
                    {
                        target =
                            Some((Arc::clone(&sibling.node), AgentFamilyRelationship::Sibling));
                        break;
                    }
                }
            }
        }
        if target.is_none() {
            for child in node.children.lock().await.iter() {
                if child.node.session_id == input.target {
                    target = Some((Arc::clone(&child.node), AgentFamilyRelationship::Parent));
                    break;
                }
            }
        }
        let Some((target_node, relationship)) = target else {
            anyhow::bail!("No family member matches \"{}\"", input.target);
        };
        // Marked before the delivery, so a fast child's reply cannot race the
        // marking: a reply to the parent withholds the no-reply notice; a
        // fresh task from the parent means silence this round earns it.
        match relationship {
            AgentFamilyRelationship::Child => {
                for child in target_node.children.lock().await.iter() {
                    if Arc::ptr_eq(&child.node, node) {
                        child.state.lock().await.replied_since_task = true;
                    }
                }
            }
            AgentFamilyRelationship::Parent => {
                for child in node.children.lock().await.iter() {
                    if Arc::ptr_eq(&child.node, &target_node) {
                        child.state.lock().await.replied_since_task = false;
                    }
                }
            }
            AgentFamilyRelationship::Sibling => {}
        }
        let prompt = create_agent_session_message_prompt(&AgentMessagePromptPayload {
            message: input.message.clone(),
            sender_name: node
                .session_name()
                .unwrap_or_else(|| node.session_id.clone()),
            from_relationship: Some(relationship),
        });
        let id = create_agent_session_message_id();
        let row = create_agent_session_message_row(&AgentSessionMessageRowPayload {
            id: &id,
            prompt: &prompt,
            message: &input.message,
            from: &node.endpoint(),
            from_relationship: Some(relationship),
            target: Some(&target_node.endpoint()),
            timestamp: now_ms(),
        });
        // One delivery path: the receiver's inbox ([`Subagents::post`]).
        let tree = node
            .tree
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the runtime is not running"))?;
        let status = tree
            .post(
                &id,
                NewEvent {
                    topic: Topic::new(format!("agent/{}/inbox", target_node.session_id))?,
                    source: node.session_id.clone(),
                    session: Some(node.session_id.clone()),
                    payload: InboxMessage {
                        id: id.clone(),
                        from: Some(node.session_id.clone()),
                        row,
                        kind: input.kind,
                        delivery: Delivery::Steer,
                    }
                    .payload(),
                    trust: node.trust.current(),
                    cause: node.trust.cause(),
                    reply: None,
                },
            )
            .await?;
        let (delivered_at, queued_at) = match status {
            AgentMessageDeliveryStatus::Delivered => (Some(format_iso_now()), None),
            AgentMessageDeliveryStatus::Queued => (None, Some(format_iso_now())),
            AgentMessageDeliveryStatus::WakeRefused { .. } => (None, None),
        };
        Ok(AgentMessageReceipt {
            id,
            target: target_node.session_id.clone(),
            target_session_id: Some(target_node.session_id.clone()),
            target_session_name: target_node.session_name().filter(|name| !name.is_empty()),
            target_runtime_kind: None,
            message: input.message.clone(),
            delivery_status: status,
            delivery_mode: Some("steer"),
            receiver_role: input.receiver_role,
            delivered_at,
            queued_at,
        })
    }

    /// The runtime daemon's wait: parked on this session's receiving side
    /// (which its inbox deliveries report to), a cycle refused at once from
    /// the wait edges of every session the runtime runs. A headless run
    /// keeps the refusal.
    async fn wait_for_agent_message(
        &self,
        request: AgentMessageWaitRequest,
    ) -> anyhow::Result<AgentMessageWaitOutcome> {
        let node = &self.0;
        let tree = node
            .tree
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the runtime is not running"))?;
        anyhow::ensure!(
            tree.daemon().is_some(),
            "agent_message.wait needs a daemon session"
        );
        wait_for_arrival(&node.waits, request, &[node.session_id.as_str()], async {
            Ok(tree.wait_roster())
        })
        .await
    }
}

/// Who sends a message the runtime daemon delivers for a client
/// (`session.send`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MessageSender {
    /// The client itself, by its name: the old daemon's client-origin
    /// sender (`{clientId}`), whose messages spend no wake budget.
    Client(String),
    /// An open session, by its id (`prime-agent send --from`).
    Session(String),
}

impl Subagents {
    /// Publishes `event` (an [`InboxMessage`] with id `id` on its receiver's
    /// `agent/<to>/inbox`) and answers how its delivery went. The inbox's
    /// subscription delivers the row (a streaming receiver takes it at its
    /// next turn boundary, its drive runs it when idle, and a settled child
    /// starts its next task over it) under the wake budget, and the outcome
    /// is here once the publish returns.
    async fn post(&self, id: &str, event: NewEvent) -> anyhow::Result<AgentMessageDeliveryStatus> {
        let (answer, mut answered) = tokio::sync::oneshot::channel();
        lock(&self.answers).insert(id.to_string(), answer);
        let published = self.outbox.publish(event).await;
        lock(&self.answers).remove(id);
        let published = published?;
        let Ok(status) = answered.try_recv() else {
            let dropped = published
                .caused
                .iter()
                .any(|event| event.event.topic.as_str() == "system/loop_dropped");
            if dropped {
                anyhow::bail!(
                    "Agent message not delivered: it ends a chain of {MAX_HOPS} events that caused each other"
                );
            }
            anyhow::bail!("Agent message not delivered: no inbox took it");
        };
        status
    }

    /// Delivers `message` to open session `target` as an agent message from
    /// `sender`, steered or (`delivery`) queued behind its running turn, the
    /// way the old daemon's `send_message` does, and answers the old
    /// daemon's receipt: `{id, source, message, deliveryMode,
    /// deliveryStatus, deliveredAt | queuedAt | retryAfterMs, target, from}`.
    ///
    /// # Errors
    ///
    /// The message is empty or too long, a session is not open (`Unknown
    /// active session: <id>`), the sender is the target, or the inbox did not
    /// take it.
    pub(crate) async fn send_message(
        &self,
        target: &str,
        message: &str,
        sender: MessageSender,
        delivery: Delivery,
    ) -> anyhow::Result<Value> {
        let message =
            pa_core::session_engine::agent_messaging::normalize_agent_session_message(message)?;
        let target_node = self.built_node(target).await?;
        let (from, endpoint, name, relationship) = match &sender {
            MessageSender::Client(client) => {
                (None, json!({ "clientId": client }), client.clone(), None)
            }
            MessageSender::Session(id) => {
                anyhow::ensure!(
                    id != target,
                    "Agent messaging cannot target the sending session"
                );
                let node = self
                    .node(id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown active session: {id}"))?;
                // A child writing to its parent is labelled as one.
                let relationship = node
                    .parent_node()
                    .filter(|parent| parent.session_id == target)
                    .map(|_| AgentFamilyRelationship::Child);
                let endpoint = node.named_endpoint().await;
                let name = endpoint["sessionName"]
                    .as_str()
                    .map_or_else(|| id.clone(), str::to_string);
                (Some(id.clone()), endpoint, name, relationship)
            }
        };
        // The event's source: the sending session, or the client.
        let source = from.clone().unwrap_or_else(|| name.clone());
        let prompt = create_agent_session_message_prompt(&AgentMessagePromptPayload {
            message: message.clone(),
            sender_name: name,
            from_relationship: relationship,
        });
        let id = create_agent_session_message_id();
        let target_endpoint = target_node.named_endpoint().await;
        let row = create_agent_session_message_row(&AgentSessionMessageRowPayload {
            id: &id,
            prompt: &prompt,
            message: &message,
            from: &endpoint,
            from_relationship: relationship,
            target: Some(&target_endpoint),
            timestamp: now_ms(),
        });
        let status = self
            .post(
                &id,
                NewEvent {
                    topic: Topic::new(format!("agent/{target}/inbox"))?,
                    source,
                    session: Some(from.clone().unwrap_or_else(|| target.to_string())),
                    payload: InboxMessage {
                        id: id.clone(),
                        from,
                        row,
                        kind: pa_core::session_engine::agent_messaging::AgentMessageKind::Direct,
                        delivery,
                    }
                    .payload(),
                    trust: Trust::Owner,
                    cause: None,
                    reply: None,
                },
            )
            .await?;
        let mut receipt = json!({
            "id": id,
            "source": pa_core::session_engine::agent_messaging::AGENT_MESSAGE_SOURCE,
            "message": message,
            "deliveryMode": match delivery {
                Delivery::Steer => "steer",
                Delivery::FollowUp => "follow_up",
            },
            "deliveryStatus": status.as_str(),
            "target": target_endpoint,
            "from": endpoint,
        });
        match status {
            AgentMessageDeliveryStatus::Delivered => {
                receipt["deliveredAt"] = json!(format_iso_now());
            }
            AgentMessageDeliveryStatus::Queued => {
                receipt["queuedAt"] = json!(format_iso_now());
            }
            AgentMessageDeliveryStatus::WakeRefused { retry_after_ms } => {
                receipt["retryAfterMs"] = json!(retry_after_ms);
            }
        }
        Ok(receipt)
    }
}

/// The runtime daemon's `agent_observe.*` over one node's nuclear family:
/// itself, its parent, its siblings and its children, each a session the
/// runtime runs.
pub(crate) struct FamilyObserve(pub(crate) Arc<Node>);

impl FamilyObserve {
    /// The family's summaries, the node's own first.
    async fn family(&self) -> Vec<AgentObserveSummary> {
        let node = &self.0;
        let mut members = vec![(Arc::clone(node), None, None)];
        if let Some(parent) = node.parent_node() {
            members.push((
                Arc::clone(&parent),
                Some(AgentFamilyRelationship::Parent),
                None,
            ));
            for sibling in parent.children.lock().await.iter() {
                if !Arc::ptr_eq(&sibling.node, node) {
                    members.push((
                        Arc::clone(&sibling.node),
                        Some(AgentFamilyRelationship::Sibling),
                        Some(sibling.rlm_child_id.clone()),
                    ));
                }
            }
        }
        for child in node.children.lock().await.iter() {
            members.push((
                Arc::clone(&child.node),
                Some(AgentFamilyRelationship::Child),
                Some(child.rlm_child_id.clone()),
            ));
        }
        // The node's own child id, when it is one.
        let own_child_id = match node.parent_node() {
            Some(parent) => parent
                .children
                .lock()
                .await
                .iter()
                .find(|child| Arc::ptr_eq(&child.node, node))
                .map(|child| child.rlm_child_id.clone()),
            None => None,
        };
        let mut summaries = Vec::with_capacity(members.len());
        for (member, relationship, child_id) in members {
            let child_id = if relationship.is_none() {
                own_child_id.clone()
            } else {
                child_id
            };
            summaries.push(summary(&member, relationship, child_id).await);
        }
        summaries
    }

    /// The family member `target` names: its session id, name or child id.
    async fn find(&self, target: &str) -> anyhow::Result<Option<AgentObserveSummary>> {
        let mut matches: Vec<AgentObserveSummary> = self
            .family()
            .await
            .into_iter()
            .filter(|summary| {
                summary.session_id == target
                    || summary.session_name.as_deref() == Some(target)
                    || summary.rlm_child_id.as_deref() == Some(target)
            })
            .collect();
        if matches.len() > 1 {
            anyhow::bail!("Ambiguous active session \"{target}\"");
        }
        Ok(matches.pop())
    }
}

/// One member's observe summary: what its engine says now.
async fn summary(
    node: &Arc<Node>,
    relationship: Option<AgentFamilyRelationship>,
    rlm_child_id: Option<String>,
) -> AgentObserveSummary {
    let engine = node.engine();
    let (is_streaming, queued_count, message_count) = match &engine {
        Some(engine) => {
            let agent = engine.session.agent();
            let streaming = agent.state().await.is_streaming;
            let queued = agent.steering_previews().len() + agent.follow_up_previews().len();
            let messages = engine
                .session
                .shared_persistence()
                .lock()
                .await
                .active_context()
                .messages
                .len();
            (
                streaming,
                queued,
                Some(u64::try_from(messages).unwrap_or(u64::MAX)),
            )
        }
        None => (false, 0, None),
    };
    let has_engine = engine.is_some();
    let status = if !has_engine {
        AgentFamilyStatus::Inactive
    } else if is_streaming {
        AgentFamilyStatus::Running
    } else {
        AgentFamilyStatus::Idle
    };
    let parent = node.parent_node();
    let session_file = lock(&node.adopted).session_file.clone();
    AgentObserveSummary {
        active_session_id: has_engine.then(|| node.session_id.clone()),
        session_id: node.session_id.clone(),
        session_name: node.session_name().filter(|name| !name.is_empty()),
        relationship,
        runtime_kind: Some(
            if node.is_subagent() {
                "subagent"
            } else {
                "top-level"
            }
            .to_string(),
        ),
        cwd: Some(node.cwd.to_string_lossy().into_owned()),
        status,
        activity: has_engine.then_some(if is_streaming {
            AgentObserveActivity::Model
        } else {
            AgentObserveActivity::Idle
        }),
        is_current: relationship.is_none(),
        is_streaming,
        is_compacting: false,
        attached_clients: 0,
        message_count,
        queued_count,
        is_session_active: is_streaming,
        parent_active_session_id: parent.as_ref().map(|parent| parent.session_id.clone()),
        parent_session_id: parent.as_ref().map(|parent| parent.session_id.clone()),
        rlm_child_id,
        first_message: None,
        session_file,
    }
}

impl AgentObserveController for FamilyObserve {
    async fn list_agents(&self) -> anyhow::Result<AgentObserveListResult> {
        let mut family = self.family().await;
        let current = family.remove(0);
        Ok(AgentObserveListResult {
            current,
            agents: family,
        })
    }

    async fn get_agent(&self, target: &str) -> anyhow::Result<Option<AgentObserveSummary>> {
        self.find(target).await
    }

    async fn recent_messages(
        &self,
        target: &str,
        limit: usize,
        max_chars: usize,
    ) -> anyhow::Result<AgentObserveRecentMessagesResult> {
        let agent = self
            .find(target)
            .await?
            .ok_or_else(|| anyhow::anyhow!("agent {target} is not reachable"))?;
        let member = self
            .0
            .tree
            .upgrade()
            .and_then(|tree| tree.node(&agent.session_id))
            .and_then(|member| member.engine())
            .ok_or_else(|| anyhow::anyhow!("agent {target} is not running"))?;
        let messages = member
            .session
            .shared_persistence()
            .lock()
            .await
            .active_context()
            .messages;
        let start = messages.len().saturating_sub(limit);
        Ok(AgentObserveRecentMessagesResult {
            agent,
            messages: messages[start..]
                .iter()
                .enumerate()
                .map(|(offset, message)| {
                    create_agent_observe_message_preview(message, start + offset, max_chars)
                })
                .collect(),
            limit,
            max_chars,
            truncated: start > 0,
        })
    }

    fn session_messages(
        &self,
        target: &str,
        _start: i64,
        _limit: u64,
        _roles: Vec<String>,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + Send {
        std::future::ready(Err(anyhow::anyhow!(
            "agent_observe.session_messages({target}) is not yet in the runtime daemon; \
             use agent_observe.recent"
        )))
    }
}

/// The late fire-and-forget `agent_message.send` receipt sink: the receipt
/// records through the node's adopted engine, and the node and each of its
/// ancestors count it as pending until it has.
pub(crate) fn late_sent_agent_message_sink(
    node: &Arc<Node>,
) -> pa_core::LateSentAgentMessageHandler {
    let node = Arc::clone(node);
    Arc::new(move |tool_call_id, message| {
        // Counted on the whole ancestor chain before spawning: even a
        // settled child may receive a late callback while the root parks.
        let mut pending_nodes = Vec::new();
        let mut current = Some(Arc::clone(&node));
        while let Some(current_node) = current {
            current_node.pending_receipts.fetch_add(1, Ordering::SeqCst);
            // Admission must wake an already armed zero-count quiescence
            // scan; waiting only for completion leaves a window for a false
            // rest.
            current_node.activity.notify_waiters();
            current = current_node.parent_node();
            pending_nodes.push(current_node);
        }
        let node = Arc::clone(&node);
        let tool_call_id = tool_call_id.to_string();
        tokio::spawn(async move {
            let recorded = match node.engine() {
                Some(engine) => engine
                    .record_late_sent_agent_message(&tool_call_id, message)
                    .await
                    .map_err(|error| format!("{error:#}")),
                None => Err("late receipt arrived without a live session engine".to_string()),
            };
            for pending_node in pending_nodes {
                if let Err(error) = &recorded {
                    *lock(&pending_node.late_receipt_error) = Some(error.clone());
                }
                pending_node.pending_receipts.fetch_sub(1, Ordering::SeqCst);
                pending_node.activity.notify_waiters();
            }
        });
    })
}
