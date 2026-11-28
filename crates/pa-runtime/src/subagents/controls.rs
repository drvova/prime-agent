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
//! - **A queue clear.** `session.clear_queue` takes the agent messages too,
//!   and bounces each to the session that sent it (a client's has no session
//!   to bounce to) with pa-core's `cleared_agent_message_bounce_text`, as
//!   the old worker's `clear_queue` does: the sender otherwise never learns
//!   its message will not be delivered.
//! - **The depth bound.** Its status reads the node's bound and where it
//!   comes from; a chat override fixes it.
//! - **A heartbeat's queued fire.** A change to a heartbeat (all but a
//!   resume) withdraws its `heartbeat_prompt` row waiting on the session's
//!   agent, as the old daemon dropped its `heartbeat:<id>` queue item.
//! - **The delete.** Only a settled child is deleted (with the kernel's
//!   `rlm.delete_subagent` teardown); one with live work answers `running`,
//!   and a handle deleted before answers deleted again.
//! - **A subagent's stop** (`session.stop` of a child, the old `kill` of its
//!   session). A plain stop ends the child, running or not, and lets it go
//!   from its family; its ledger edge stays, so it lists as a passive child
//!   and its parent opened again finds it. A stop that deletes
//!   (`rlmLedgerDelete`, the old supervisor's marker) writes the child's
//!   deletion boundary before its teardown, as the kernel's delete does; one
//!   aimed at a child no session runs (stopped before, or a retry of a delete)
//!   is the ledger's alone, and a retry writes no second boundary.

use std::sync::Arc;

use pa_agent::types::AgentMessage;
use pa_core::session_engine::agent_controls::{
    agent_message_safety_status, cleared_agent_messages, rlm_max_depth_status,
    SubagentDeleteOutcome,
};
use pa_core::session_engine::agent_messaging::{
    cleared_agent_message_bounce_text, AGENT_MESSAGE_CUSTOM_TYPE,
};
use pa_core::session_engine::messages::HEARTBEAT_PROMPT_CUSTOM_TYPE;
use pa_core::session_engine::rlm_host::RlmSubagentHost as _;
use pa_core::session_engine::rlm_ledger::RlmLedgerDeleteReason;
use serde_json::Value;

use super::host::{delete_child, retire_child, SessionSubagents};
use super::{child_is_unsettled, lock, Delivery, MessageSender, Node, Subagents};

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

    /// The agent half of session `id`'s queue clear (see the module
    /// documentation): its waiting agent messages are taken out and each
    /// bounced to its sender; their prompts, by lane. A bounce that fails is
    /// logged, never retried.
    ///
    /// # Errors
    ///
    /// The runtime runs no session `id`.
    pub(crate) async fn clear_and_bounce_agent_messages(
        &self,
        id: &str,
    ) -> anyhow::Result<(Vec<String>, Vec<String>)> {
        let node = self.controlled(id)?;
        let (steering, follow_up) = take_rows(&node, is_agent_message);
        // The cleared queue is this session's own: its name, else its id,
        // names the receiver.
        let receiver = node
            .session_name()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.to_string());
        for row in steering.iter().chain(&follow_up) {
            let AgentMessage::Custom(custom) = row else {
                continue;
            };
            let details = &custom.payload["details"];
            let Some(sender) = details["from"]["activeSessionId"]
                .as_str()
                .filter(|sender| !sender.is_empty() && self.node(sender).is_some())
            else {
                continue;
            };
            let text = cleared_agent_message_bounce_text(
                &receiver,
                details["message"].as_str().unwrap_or_default(),
            );
            if let Err(error) = self
                .deliver_message(
                    sender,
                    text,
                    MessageSender::Session(id.to_string()),
                    Delivery::Steer,
                )
                .await
            {
                tracing::error!("a cleared agent message's bounce to {sender} failed: {error:#}");
            }
        }
        Ok((
            steering.iter().map(row_prompt).collect(),
            follow_up.iter().map(row_prompt).collect(),
        ))
    }

    /// Withdraws heartbeat `job`'s fire waiting on session `id`'s agent (its
    /// `heartbeat_prompt` row): the old daemon's `heartbeat:<id>` queue key.
    /// The empty input queued to run it then runs over nothing. Nothing to
    /// withdraw for a session the runtime does not run.
    pub(crate) fn withdraw_heartbeat_fire(&self, id: &str, job: &str) {
        let Some(node) = self.node(id) else {
            return;
        };
        let _ = take_rows(&node, |message| match message {
            AgentMessage::Custom(custom) => {
                custom.payload.get("customType").and_then(Value::as_str)
                    == Some(HEARTBEAT_PROMPT_CUSTOM_TYPE)
                    && custom
                        .payload
                        .pointer("/details/jobId")
                        .and_then(Value::as_str)
                        == Some(job)
            }
            AgentMessage::Standard(_) => false,
        });
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

/// What a subagent's stop does (see the module documentation).
pub(crate) enum ChildStop {
    /// The child ends; its ledger edge stays.
    Stop,
    /// The child is deleted for `reason`; `child_id`, when given, is the
    /// handle the delete names.
    Delete {
        reason: RlmLedgerDeleteReason,
        child_id: Option<String>,
    },
}

/// What a subagent's stop found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChildStopped {
    /// The child's session ran and was ended.
    Session,
    /// No session ran it: the delete's boundary is the ledger's.
    Ledger,
    /// The runtime runs no subagent by that id.
    Unknown,
}

impl Subagents {
    /// `session.stop` of subagent `id` (see the module documentation).
    ///
    /// # Errors
    ///
    /// The delete names another child's handle, or its deletion boundary
    /// was not written.
    pub(crate) async fn stop_child(
        &self,
        id: &str,
        stop: ChildStop,
    ) -> anyhow::Result<ChildStopped> {
        let found = self.child_of(id).await;
        let parent = found.as_ref().and_then(|child| child.node.parent_node());
        match (stop, found, parent) {
            (ChildStop::Stop, Some(child), Some(parent)) => {
                retire_child(&parent, &child, "The subagent's session was stopped").await;
                Ok(ChildStopped::Session)
            }
            (ChildStop::Delete { reason, child_id }, Some(child), Some(parent)) => {
                if let Some(named) = child_id.filter(|named| *named != child.rlm_child_id) {
                    anyhow::bail!("session {id} is not RLM subagent {named}");
                }
                delete_child(&parent, &child, reason).await?;
                Ok(ChildStopped::Session)
            }
            (ChildStop::Delete { reason, child_id }, _, _) => match self.daemon() {
                Some(family) => {
                    family
                        .deleted_stopped(id.to_string(), child_id, reason)
                        .await?;
                    Ok(ChildStopped::Ledger)
                }
                None => Ok(ChildStopped::Unknown),
            },
            (ChildStop::Stop, _, _) => Ok(ChildStopped::Unknown),
        }
    }
}

/// Takes the agent messages waiting on `node`'s agent out of both lanes:
/// each one's prompt, by lane, in queue order.
fn take_agent_messages(node: &Node) -> (Vec<String>, Vec<String>) {
    let (steering, follow_up) = take_rows(node, is_agent_message);
    (
        steering.iter().map(row_prompt).collect(),
        follow_up.iter().map(row_prompt).collect(),
    )
}

/// A taken agent message's prompt: its row's content.
fn row_prompt(message: &AgentMessage) -> String {
    match message {
        AgentMessage::Custom(custom) => custom.payload["content"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        AgentMessage::Standard(_) => String::new(),
    }
}

/// Takes the rows `taken` picks out of both lanes of `node`'s agent, under
/// its delivery gate (no row lands between the read and the take): the
/// steering lane's, then the follow-up lane's, each in queue order.
fn take_rows(
    node: &Node,
    taken: impl Fn(&AgentMessage) -> bool,
) -> (Vec<AgentMessage>, Vec<AgentMessage>) {
    let Some(engine) = node.engine() else {
        return (Vec::new(), Vec::new());
    };
    let agent = engine.session.agent();
    let rows = {
        let _delivery_guard = lock(&node.delivery_gate);
        let steering: Vec<AgentMessage> = agent.steering_batches().into_iter().flatten().collect();
        // A row the turn read meanwhile is in neither: each taken row was in
        // one lane, and the delivery gate kept new rows out.
        agent
            .remove_queued_messages(taken)
            .into_iter()
            .map(|message| (steering.contains(&message), message))
            .collect::<Vec<_>>()
    };
    if !rows.is_empty() {
        node.stream.queue_changed();
        node.activity.notify_waiters();
    }
    let (steering, follow_up): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|(in_steering, _)| *in_steering);
    (
        steering.into_iter().map(|(_, message)| message).collect(),
        follow_up.into_iter().map(|(_, message)| message).collect(),
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
