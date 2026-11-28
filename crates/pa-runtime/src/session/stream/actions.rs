//! The session's queue view on its stream, in the session-event shapes: the
//! old daemon's `session_action_update {"actions": ...}`, built from the
//! runtime's lane and the rows waiting on the session's agent.
//!
//! - **The lanes.** A queued task admitted behind work (its lane was
//!   running or holding a task, or stopped, when it joined) is listed, its
//!   prompt as the row: a `Steer` task under `steering`, any other under
//!   `followUps`. A task that joined an idle lane runs at once and is never
//!   listed, as the old daemon's direct prompt was not; a task with no
//!   prompt (the one that runs delivered rows) is not a row of its own.
//!   After them come the rows waiting on the agent (a child's status notice,
//!   an agent message), one per batch, in their lane; `rlmChildStatus` marks
//!   the child status notices by index. `queuedCount` counts every row.
//! - **The active action.** As a task leaves the lane to run, it is the
//!   active `turn` in phase `preparing`, labelled with its row (a task with
//!   no prompt with the first waiting row; one that was never listed has no
//!   active action, as the old daemon's had none); it is `committing` once
//!   its first user or custom row settles and `running` from its reply's
//!   first frame; it ends before the task's `done`.
//!
//! A view that did not change is not sent again, and the view the session
//! starts from (nothing queued, nothing active) is not sent at all.

use std::collections::HashSet;

use pa_agent::agent::Agent;
use pa_agent::types::{AgentMessage, Message, UserContent, UserPart};
use pa_core::session_engine::agent_messaging::{
    AGENT_MESSAGE_CUSTOM_TYPE, AGENT_MESSAGE_RECEIVED_PREVIEW_LABEL,
};
use pa_core::session_engine::rlm_notices::{
    compact_rlm_text, RLM_CHILD_FAILURE_CUSTOM_TYPE, RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE,
};
use pa_types::event_log::{Priority, Task};
use serde_json::{json, Value};

use super::{SessionStream, StreamState};
use crate::lock;
use crate::tasks::LaneState;

/// The queue view's state, kept under the stream's lock.
#[derive(Default)]
pub(super) struct QueueView {
    /// The lane's queued tasks, in run order.
    queued: Vec<Task>,
    /// The ids of the queued tasks admitted behind work: the listed ones.
    behind_work: HashSet<String>,
    /// Whether the lane held work at its last report.
    busy: bool,
    /// The task the lane runs, as the view last heard.
    running: Option<String>,
    active: Option<Active>,
    /// The view last sent; `None` until the first.
    sent: Option<Value>,
}

struct Active {
    task: String,
    phase: &'static str,
    label: String,
}

/// A point in the running task's turn that moves its phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnPoint {
    /// A user or custom row settled.
    RowSettled,
    /// The reply's first frame went out.
    ReplyStarted,
}

impl SessionStream {
    /// The lane's report ([`crate::TaskRunner::lane_changed`]): the view
    /// takes in what joined, left and runs.
    pub(crate) fn lane_changed(&self, lane: &LaneState) {
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        // Kept before the build chose the stream's shapes too: the lane
        // runs ahead of it.
        let agent = state.agent.upgrade();
        let view = &mut state.view;
        // A task that joins while the lane holds work, or is stopped, waits
        // behind it: it is listed.
        if view.busy || lane.suspended || lane.holds > 0 {
            for task in &lane.queued {
                if !view.queued.iter().any(|known| known.id == task.id) {
                    view.behind_work.insert(task.id.clone());
                }
            }
        }
        let running = lane.running.as_ref();
        if let Some(task) = running.filter(|task| view.running.as_ref() != Some(&task.id)) {
            let label = if task.prompt.is_empty() {
                waiting_rows(agent.as_deref())
                    .into_iter()
                    .next()
                    .map(|(preview, ..)| preview)
            } else {
                view.behind_work
                    .contains(&task.id)
                    .then(|| task.prompt.clone())
            };
            view.active = label.map(|label| Active {
                task: task.id.clone(),
                phase: "preparing",
                label: compact_rlm_text(&label),
            });
        }
        if view
            .active
            .as_ref()
            .is_some_and(|active| running.map(|task| &task.id) != Some(&active.task))
        {
            view.active = None;
        }
        view.behind_work.retain(|id| {
            lane.queued.iter().any(|task| task.id == *id)
                || running.is_some_and(|task| task.id == *id)
        });
        view.queued.clone_from(&lane.queued);
        view.busy = running.is_some() || !lane.queued.is_empty();
        view.running = running.map(|task| task.id.clone());
        self.send_view(state);
    }

    /// The rows waiting on the agent changed (one was delivered to it).
    pub(crate) fn queue_changed(&self) {
        self.send_view(&mut lock(&self.state));
    }

    /// Task `task` ended: it is no longer the active action. Sent ahead of
    /// its `done`.
    pub(crate) fn task_ended(&self, task: &str) {
        let mut state = lock(&self.state);
        if state
            .view
            .active
            .as_ref()
            .is_some_and(|active| active.task == task)
        {
            state.view.active = None;
        }
        self.send_view(&mut state);
    }

    /// The running task's turn reached `point`; the view, sent after the
    /// frames that mark it.
    pub(super) fn turn_point(&self, state: &mut StreamState, point: TurnPoint) {
        if let Some(active) = state.view.active.as_mut() {
            active.phase = match (point, active.phase) {
                (TurnPoint::RowSettled, "preparing") => "committing",
                (TurnPoint::RowSettled, phase) => phase,
                (TurnPoint::ReplyStarted, _) => "running",
            };
        }
        self.send_view(state);
    }

    /// The queue view's rows as it stands, each with what it shows.
    pub(crate) fn queue_rows(&self) -> QueueRows {
        let state = lock(&self.state);
        rows(&state.view, state.agent.upgrade().as_deref())
    }

    /// The queue view as it stands (the old summary's `sessionActions`).
    pub(crate) fn actions(&self) -> Value {
        let state = lock(&self.state);
        render(&state.view, state.agent.upgrade().as_deref())
    }

    /// Sends the view when it changed, in the session-event shapes.
    fn send_view(&self, state: &mut StreamState) {
        if !state.sequenced {
            return;
        }
        let view = render(&state.view, state.agent.upgrade().as_deref());
        let unchanged = match &state.view.sent {
            Some(sent) => *sent == view,
            None => view == render(&QueueView::default(), None),
        };
        if unchanged {
            return;
        }
        state.view.sent = Some(view.clone());
        let trust = self.trust.current();
        self.direct(
            state,
            vec![json!({ "type": "session_action_update", "actions": view })],
            trust,
        );
    }
}

/// One row of the queue view, and what it shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueueRow {
    /// The row's text, as the view lists it.
    pub(crate) text: String,
    pub(crate) at: RowAt,
    /// Whether it is a child's status notice.
    notice: bool,
}

/// What a queue view's row shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowAt {
    /// A task the lane queues.
    Task(Task),
    /// A batch waiting on the agent.
    Waiting,
}

/// The queue view's rows by lane, in the order it lists them.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct QueueRows {
    pub(crate) steering: Vec<QueueRow>,
    pub(crate) follow_ups: Vec<QueueRow>,
}

impl QueueRows {
    /// The rows of `lane`.
    pub(crate) fn lane(&self, lane: QueueLane) -> &[QueueRow] {
        match lane {
            QueueLane::Steering => &self.steering,
            QueueLane::FollowUp => &self.follow_ups,
        }
    }
}

/// The view's rows: the listed tasks, then the batches waiting on `agent`.
fn rows(view: &QueueView, agent: Option<&Agent>) -> QueueRows {
    let mut rows = QueueRows::default();
    for task in &view.queued {
        if task.prompt.is_empty() || !view.behind_work.contains(&task.id) {
            continue;
        }
        let row = QueueRow {
            text: task.prompt.clone(),
            at: RowAt::Task(task.clone()),
            notice: false,
        };
        match task.priority {
            Priority::Steer => rows.steering.push(row),
            Priority::Prompt | Priority::FollowUp | Priority::Event | Priority::Background => {
                rows.follow_ups.push(row);
            }
        }
    }
    for (preview, lane, notice) in waiting_rows(agent) {
        let lane_rows = match lane {
            QueueLane::Steering => &mut rows.steering,
            QueueLane::FollowUp => &mut rows.follow_ups,
        };
        lane_rows.push(QueueRow {
            text: preview,
            at: RowAt::Waiting,
            notice,
        });
    }
    rows
}

/// The old frame's `actions` object.
fn render(view: &QueueView, agent: Option<&Agent>) -> Value {
    let rows = rows(view, agent);
    let texts = |lane: &[QueueRow]| lane.iter().map(|row| row.text.clone()).collect::<Vec<_>>();
    let notices = |lane: &[QueueRow]| {
        lane.iter()
            .enumerate()
            .filter(|(_, row)| row.notice)
            .map(|(index, _)| index)
            .collect::<Vec<_>>()
    };
    let notices = (notices(&rows.steering), notices(&rows.follow_ups));
    let mut actions = json!({
        "queuedCount": rows.steering.len() + rows.follow_ups.len(),
        "steering": texts(&rows.steering),
        "followUps": texts(&rows.follow_ups),
    });
    if !notices.0.is_empty() || !notices.1.is_empty() {
        let mut marks = serde_json::Map::new();
        if !notices.0.is_empty() {
            marks.insert("steering".to_string(), json!(notices.0));
        }
        if !notices.1.is_empty() {
            marks.insert("followUp".to_string(), json!(notices.1));
        }
        actions["rlmChildStatus"] = Value::Object(marks);
    }
    if let Some(active) = &view.active {
        actions["active"] = json!({
            "kind": "turn",
            "phase": active.phase,
            "label": active.label,
        });
    }
    actions
}

/// A lane of the queue view: its steers, or the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueLane {
    Steering,
    FollowUp,
}

/// The rows waiting on `agent`, one per queued batch: its preview, its
/// lane, and whether it is a child's status notice.
fn waiting_rows(agent: Option<&Agent>) -> Vec<(String, QueueLane, bool)> {
    let Some(agent) = agent else {
        return Vec::new();
    };
    let lane = |batches: Vec<Vec<AgentMessage>>, lane: QueueLane| {
        batches.into_iter().map(move |batch| {
            let preview = batch
                .iter()
                .filter_map(row_preview)
                .collect::<Vec<_>>()
                .join(" ");
            let notice = batch.iter().any(|message| {
                matches!(
                    custom_type(message),
                    Some(RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE | RLM_CHILD_FAILURE_CUSTOM_TYPE)
                )
            });
            (preview, lane, notice)
        })
    };
    lane(agent.steering_batches(), QueueLane::Steering)
        .chain(lane(agent.follow_up_batches(), QueueLane::FollowUp))
        .collect()
}

fn custom_type(message: &AgentMessage) -> Option<&str> {
    match message {
        AgentMessage::Custom(custom) => custom.payload.get("customType").and_then(Value::as_str),
        AgentMessage::Standard(_) => None,
    }
}

/// One waiting row's text, as the old queue listed it: a user row's text,
/// an agent message's labelled message, any other custom row's content.
fn row_preview(message: &AgentMessage) -> Option<String> {
    match message {
        AgentMessage::Standard(Message::User(user)) => match &user.content {
            UserContent::Text(text) => Some(text.clone()),
            UserContent::Parts(parts) => {
                let texts: Vec<&str> = parts
                    .iter()
                    .filter_map(|part| match part {
                        UserPart::Text(text) => Some(text.text.as_str()),
                        UserPart::Image(_) => None,
                    })
                    .collect();
                (!texts.is_empty()).then(|| texts.join(" "))
            }
        },
        AgentMessage::Standard(_) => None,
        AgentMessage::Custom(custom) => {
            if custom_type(message) == Some(AGENT_MESSAGE_CUSTOM_TYPE) {
                if let Some(text) = custom.payload["details"]["message"].as_str() {
                    return Some(format!("{AGENT_MESSAGE_RECEIVED_PREVIEW_LABEL}: {text}"));
                }
            }
            match &custom.payload["content"] {
                Value::String(text) => Some(text.clone()),
                Value::Array(blocks) => Some(
                    blocks
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            }
        }
    }
}
