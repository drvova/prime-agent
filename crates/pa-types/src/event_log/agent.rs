//! The payloads of a session's stored topics.
//!
//! - `agent/<id>/input`: an [`AgentInput`], the text a turn starts from and
//!   its place in the session's lane.
//! - `agent/<id>/done`: an [`AgentDone`], how a task of the session ended.
//!
//! A turn's live stream (`agent/<id>/stream`) carries each agent event in
//! the session-event wire shape the headless json mode prints.

use serde::{Deserialize, Serialize};

use super::{Priority, TaskState};

/// What a client publishes on `agent/<id>/input`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInput {
    /// The prompt the turn starts from.
    pub text: String,
    /// Where the task goes in the session's lane (`steer`, `follow_up`);
    /// absent, the route's priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
}

/// What a session publishes on `agent/<id>/done` when one of its tasks ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDone {
    /// The task's id.
    pub task: String,
    /// `done`, `failed` or `cancelled`.
    pub outcome: TaskState,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
