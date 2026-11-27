//! The payloads of a session's stored topics.
//!
//! - `agent/<id>/input`: an [`AgentInput`], the text (and images) a turn
//!   starts from and its place in the session's lane.
//! - `agent/<id>/done`: an [`AgentDone`], how a task of the session ended,
//!   and for a failed one which part failed ([`TaskFailure`]).
//!
//! A turn's live stream (`agent/<id>/stream`) carries each agent event in
//! the session-event wire shape the headless json mode prints.

use serde::{Deserialize, Serialize};

use super::{Priority, TaskState};
use crate::ai::ImageContent;

/// What a client publishes on `agent/<id>/input`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInput {
    /// The prompt the turn starts from.
    pub text: String,
    /// The images the prompt carries (a print run's `@file` attachments).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageContent>,
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
    /// Which part of a failed task failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TaskFailure>,
}

/// Which part of a task failed: what a client does next depends on it (a
/// failed reply is the transcript's own record and the session goes on; a
/// failed command or turn stops a print run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFailure {
    /// The turn ran, and its last reply is a failed model request.
    Reply,
    /// A session command (`/compact`, `/refine`, `/goal`, `/autonomous`)
    /// failed.
    Command,
    /// The turn could not run, or what runs around it (a compaction, a
    /// refinement, a goal or autonomous continuation) failed.
    Turn,
}
