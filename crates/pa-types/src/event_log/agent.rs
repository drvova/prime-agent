//! The payloads of a session's stored topics.
//!
//! - `agent/<id>/input`: an [`AgentInput`], the text (and images) a turn
//!   starts from and its place in the session's lane.
//! - `agent/<id>/accepted`: an [`AgentAccepted`], the task an input became
//!   and how its lane admitted it ([`Admission`]), caused by the input; a
//!   client that publishes a prompt and waits for it waits for the `done`
//!   of that task.
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

/// What the runtime publishes on `agent/<id>/accepted` when an input event
/// becomes a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccepted {
    /// The task's id: its `agent/<id>/done` names it.
    pub task: String,
    /// Where it went in the session's lane.
    pub priority: Priority,
    pub admission: Admission,
}

/// How a session's lane admitted a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// It waits in the lane and runs in its turn.
    Queued,
    /// The running turn took it (a steer): it reads it at its next step, and
    /// the task is done at once.
    Steered,
    /// The lane is suspended (the session was aborted): it runs once the
    /// queue is resumed.
    Parked,
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
