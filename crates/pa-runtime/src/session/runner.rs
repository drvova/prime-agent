//! Where the task queue and the bus meet the sessions: the
//! [`SessionRunner`] runs a task on its session's host and reports how it
//! ended, and the [`RuntimeTaskSink`] turns an `Agent` route into a task.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use pa_types::event_log::{AgentDone, AgentInput, AgentTarget, NewEvent, Task, TaskState};

use super::{SessionBuilder, SessionSpec, SessionTopics, Sessions};
use crate::log_stores::LogTaskStore;
use crate::routes::{TaskRequest, TaskSink};
use crate::subscriptions::MemorySessionTree;
use crate::tasks::{Steered, TaskOutcome, TaskQueue, TaskRunner};

/// Runs each task as a turn of its session: on the session's host thread,
/// then publishes the stored `agent/<id>/done` ([`AgentDone`]) after the
/// turn's stream, and returns the outcome. A task whose session is not open
/// fails.
pub(crate) struct SessionRunner<B> {
    sessions: Arc<Sessions<B>>,
}

impl<B> Clone for SessionRunner<B> {
    fn clone(&self) -> Self {
        SessionRunner {
            sessions: Arc::clone(&self.sessions),
        }
    }
}

impl<B: SessionBuilder> SessionRunner<B> {
    pub(crate) fn new(sessions: Arc<Sessions<B>>) -> Self {
        SessionRunner { sessions }
    }

    /// Cancels `session`'s running task: its turn is aborted and the task
    /// ends `Cancelled`. Whether one was running.
    pub(crate) fn cancel_running(&self, session: &str) -> bool {
        self.sessions
            .host(session)
            .is_some_and(|host| host.cancel_running())
    }
}

impl<B: SessionBuilder> TaskRunner for SessionRunner<B> {
    #[tracing::instrument(level = "debug", skip_all, fields(task = %task.id, session = %task.session))]
    async fn run(&self, task: Task) -> TaskOutcome {
        let outcome = match self.sessions.host(&task.session) {
            Some(host) => host.run(task.clone()).await,
            None => TaskOutcome::Failed(format!(
                "session {} is not open in this runtime",
                task.session
            )),
        };
        let (state, error) = match &outcome {
            TaskOutcome::Done => (TaskState::Done, None),
            TaskOutcome::Failed(error) => (TaskState::Failed, Some(error.clone())),
            TaskOutcome::Cancelled => (TaskState::Cancelled, None),
        };
        // A session id that is no topic level was never opened: nobody can
        // listen for its outcome.
        if let Ok(topics) = SessionTopics::of(&task.session) {
            let done = AgentDone {
                task: task.id.clone(),
                outcome: state,
                error,
            };
            let published = self
                .sessions
                .outbox()
                .publish(NewEvent {
                    topic: topics.done,
                    source: task.session.clone(),
                    session: Some(task.session.clone()),
                    payload: serde_json::to_value(done).unwrap_or_default(),
                    trust: task.trust,
                    cause: None,
                    reply: None,
                })
                .await;
            if let Err(error) = published {
                tracing::error!("task {:?} ended without its done event: {error:#}", task.id);
            }
        }
        outcome
    }

    fn steer(&self, running: &Task, steer: &Task) -> Steered {
        self.sessions
            .host(&running.session)
            .map_or(Steered::Declined, |host| host.steer(running, steer))
    }
}

/// Turns `Agent` routes into tasks on the runtime's queue.
///
/// The task runs in the route's target session: a named one, the matching
/// event's own (`AgentTarget::Origin`), or one opened for it
/// (`AgentTarget::Spawn`, in its `cwd` or else the runtime's, recorded as a
/// subagent of the event's session). It starts from the route's prompt at
/// the route's priority; an empty prompt takes the event's [`AgentInput`]:
/// its text, and its priority when it names one. Its trust is the
/// request's.
pub(crate) struct RuntimeTaskSink<B: SessionBuilder> {
    pub(crate) queue: TaskQueue<SessionRunner<B>, LogTaskStore>,
    pub(crate) sessions: Arc<Sessions<B>>,
    pub(crate) tree: MemorySessionTree,
    /// Where a spawned session without a `cwd` of its own runs.
    pub(crate) cwd: PathBuf,
}

impl<B: SessionBuilder> TaskSink for RuntimeTaskSink<B> {
    async fn queue(&self, request: TaskRequest) -> anyhow::Result<()> {
        let event = &request.event.event;
        let session = match request.target {
            AgentTarget::Session { session } => session,
            AgentTarget::Origin => event
                .session
                .clone()
                .context("the event was published in no session")?,
            AgentTarget::Spawn { name, cwd } => {
                let id = self.sessions.open(SessionSpec {
                    id: None,
                    name,
                    cwd: cwd.map_or_else(|| self.cwd.clone(), PathBuf::from),
                    env: std::collections::BTreeMap::new(),
                })?;
                if let Some(parent) = &event.session {
                    self.tree.insert(id.clone(), parent.clone());
                }
                id
            }
        };
        let (prompt, priority) = if request.prompt.is_empty() {
            let input = serde_json::from_value::<AgentInput>(event.payload.clone())
                .context("the event carries no input text")?;
            (input.text, input.priority.unwrap_or(request.priority))
        } else {
            (request.prompt, request.priority)
        };
        self.queue
            .push(Task {
                id: uuid::Uuid::now_v7().to_string(),
                session,
                priority,
                state: TaskState::Queued,
                trigger: request.event.id.clone(),
                prompt,
                trust: request.trust,
                attempts: 0,
                error: None,
            })
            .await
    }
}
