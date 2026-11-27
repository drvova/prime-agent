//! The agents view's controls of one session's node (8d-4b), with pa-core's
//! rules and reply shapes (`session_engine::agent_controls`), as the old
//! worker applies them:
//!
//! - **Agent-message ingest.** A paused node's inbox refuses every delivery
//!   (`Agent messaging is paused`, [`super::inbox`]); a pause and a clear
//!   take the agent messages waiting on its agent out of both lanes (client
//!   prompts stay), under the node's delivery gate, so no delivery lands
//!   between the read and the take. A wake task queued for a taken row then
//!   runs over nothing and ends.
//! - **The depth bound.** Its status reads the node's bound and where it
//!   comes from; a chat override fixes it.
//! - **The delete.** Only a settled child is deleted (with the kernel's
//!   `rlm.delete_subagent` teardown); one with live work answers `running`,
//!   and a handle deleted before answers deleted again.

use std::sync::Arc;

use pa_agent::types::AgentMessage;
use pa_core::session_engine::agent_controls::{
    agent_message_safety_status, cleared_agent_messages, rlm_max_depth_status,
    SubagentDeleteOutcome,
};
use pa_core::session_engine::agent_messaging::AGENT_MESSAGE_CUSTOM_TYPE;
use pa_core::session_engine::rlm_host::RlmSubagentHost as _;
use serde_json::Value;

use super::host::SessionSubagents;
use super::{child_is_unsettled, lock, Node, Subagents};

impl Subagents {
    /// The node of session `id`, root or subagent.
    fn controlled(&self, id: &str) -> anyhow::Result<Arc<Node>> {
        self.node(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown active session: {id}"))
    }

    /// The ids of the sessions whose ingest the selector-less controls
    /// reach: every running one, by id.
    pub(crate) fn ingesting_sessions(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock(&self.nodes)
            .iter()
            .filter(|(_, node)| !node.is_disposed() && node.engine().is_some())
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// `agent_messages_status` of session `id`.
    ///
    /// # Errors
    ///
    /// The runtime runs no session `id`.
    pub(crate) fn agent_message_status(&self, id: &str) -> anyhow::Result<Value> {
        let node = self.controlled(id)?;
        Ok(agent_message_safety_status(node.agent_messages.paused()))
    }

    /// `agent_messages_pause` (`paused`) or `_resume` of session `id`: a
    /// pause also takes its waiting agent messages out. Its status after.
    ///
    /// # Errors
    ///
    /// The runtime runs no session `id`.
    pub(crate) fn pause_agent_messages(&self, id: &str, paused: bool) -> anyhow::Result<Value> {
        let node = self.controlled(id)?;
        node.agent_messages.set(paused);
        if paused {
            take_agent_messages(&node);
        }
        Ok(agent_message_safety_status(paused))
    }

    /// `agent_messages_clear` of session `id`: the agent messages waiting on
    /// its agent are taken out, and answered by lane.
    ///
    /// # Errors
    ///
    /// The runtime runs no session `id`.
    pub(crate) fn clear_agent_messages(&self, id: &str) -> anyhow::Result<Value> {
        let node = self.controlled(id)?;
        let (steering, follow_up) = take_agent_messages(&node);
        Ok(cleared_agent_messages(&steering, &follow_up))
    }

    /// `get_rlm_max_depth_status` of session `id`.
    ///
    /// # Errors
    ///
    /// The runtime runs no session `id`.
    pub(crate) fn rlm_max_depth_status(&self, id: &str) -> anyhow::Result<Value> {
        let node = self.controlled(id)?;
        let source = *lock(&node.depth_source);
        Ok(rlm_max_depth_status(node.delegation.max_depth(), source))
    }

    /// Fixes session `id`'s bound at `max_depth`, a chat override.
    ///
    /// # Errors
    ///
    /// The runtime runs no session `id`.
    pub(crate) fn fix_rlm_max_depth(&self, id: &str, max_depth: u32) -> anyhow::Result<()> {
        let node = self.controlled(id)?;
        node.delegation.fix(max_depth);
        *lock(&node.depth_source) = "chat";
        Ok(())
    }

    /// `delete_rlm_subagent`: deletes session `id`'s settled direct child
    /// `child` (its handle).
    ///
    /// # Errors
    ///
    /// The child's teardown failed.
    pub(crate) async fn delete_child(
        &self,
        id: &str,
        child: &str,
    ) -> anyhow::Result<SubagentDeleteOutcome> {
        let Some(node) = self.node(id) else {
            return Ok(SubagentDeleteOutcome::NotFound);
        };
        let found = node
            .children
            .lock()
            .await
            .iter()
            .find(|held| held.rlm_child_id == child)
            .cloned();
        let Some(found) = found else {
            return Ok(if lock(&node.deleted_children).contains(child) {
                SubagentDeleteOutcome::Deleted
            } else {
                SubagentDeleteOutcome::NotFound
            });
        };
        if child_is_unsettled(&found).await {
            return Ok(SubagentDeleteOutcome::Running);
        }
        SessionSubagents(node)
            .delete_subagent(child.to_string())
            .await?;
        Ok(SubagentDeleteOutcome::Deleted)
    }
}

/// Takes the agent messages waiting on `node`'s agent out of both lanes:
/// each one's prompt, by lane, in queue order.
fn take_agent_messages(node: &Node) -> (Vec<String>, Vec<String>) {
    let Some(engine) = node.engine() else {
        return (Vec::new(), Vec::new());
    };
    let agent = engine.session.agent();
    let taken = {
        let _delivery_guard = lock(&node.delivery_gate);
        let steering: Vec<AgentMessage> = agent.steering_batches().into_iter().flatten().collect();
        let taken = agent.remove_queued_messages(is_agent_message);
        // A row the turn read meanwhile is in neither: each taken row was in
        // one lane, and the delivery gate kept new rows out.
        taken
            .into_iter()
            .map(|message| (steering.contains(&message), message))
            .collect::<Vec<_>>()
    };
    if taken.is_empty() {
        return (Vec::new(), Vec::new());
    }
    node.stream.queue_changed();
    node.activity.notify_waiters();
    let prompt = |message: &AgentMessage| match message {
        AgentMessage::Custom(custom) => custom.payload["content"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        AgentMessage::Standard(_) => String::new(),
    };
    let (steering, follow_up): (Vec<_>, Vec<_>) =
        taken.iter().partition(|(in_steering, _)| *in_steering);
    (
        steering
            .iter()
            .map(|(_, message)| prompt(message))
            .collect(),
        follow_up
            .iter()
            .map(|(_, message)| prompt(message))
            .collect(),
    )
}

/// Whether `message` is an agent message's row.
fn is_agent_message(message: &AgentMessage) -> bool {
    match message {
        AgentMessage::Custom(custom) => {
            custom.payload.get("customType").and_then(Value::as_str)
                == Some(AGENT_MESSAGE_CUSTOM_TYPE)
        }
        AgentMessage::Standard(_) => false,
    }
}
