//! The family's `agent_message.*`: the parent, the siblings and the children
//! of one session, each message published on its receiver's inbox
//! ([`super::inbox`]) as today's `agent_message` row, and the late receipts
//! of fire-and-forget sends.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use pa_core::session::manager::format_iso_now;
use pa_core::session_engine::agent_messaging::{
    create_agent_session_message_id, create_agent_session_message_prompt,
    create_agent_session_message_row, AgentFamilyMember, AgentFamilyRelationship,
    AgentMessageController, AgentMessageDeliveryStatus, AgentMessagePromptPayload,
    AgentMessageReceipt, AgentMessageSendInput, AgentSessionMessageRowPayload,
};

use pa_types::event_log::{NewEvent, Topic, MAX_HOPS};

use super::inbox::InboxMessage;
use super::{lock, now_ms, Node};

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
        // One delivery path: the receiver's inbox, whose subscription
        // steers the row (a streaming receiver takes it at its next turn
        // boundary, its drive runs it when idle, and a settled child starts
        // its next task over it) under the wake budget. Its outcome is here
        // once the publish returns.
        let tree = node
            .tree
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the runtime is not running"))?;
        let (answer, mut answered) = tokio::sync::oneshot::channel();
        lock(&tree.answers).insert(id.clone(), answer);
        let published = tree
            .outbox
            .publish(NewEvent {
                topic: Topic::new(format!("agent/{}/inbox", target_node.session_id))?,
                source: node.session_id.clone(),
                session: Some(node.session_id.clone()),
                payload: InboxMessage {
                    id: id.clone(),
                    from: node.session_id.clone(),
                    row,
                }
                .payload(),
                trust: node.trust.current(),
                cause: node.trust.cause(),
                reply: None,
            })
            .await;
        lock(&tree.answers).remove(&id);
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
        let status = status?;
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
