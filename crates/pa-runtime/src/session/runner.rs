//! Where the task queue and the bus meet the sessions: the
//! [`SessionRunner`] runs a task on its session's host and reports how it
//! ended, and the [`RuntimeTaskSink`] turns an `Agent` route into a task.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use pa_types::event_log::{AgentInput, AgentTarget, Task, TaskFailure, TaskState, Trust};

use super::host::{Ran, TaskEnd};
use super::{done_event, SessionBuilder, SessionSpec, SessionTopics, Sessions};
use crate::events::BusEvents;
use crate::log_stores::LogTaskStore;
use crate::routes::{TaskRequest, TaskSink};
use crate::subscriptions::MemorySessionTree;
use crate::tasks::{Steered, TaskOutcome, TaskQueue, TaskRunner};

/// Runs each task as a turn of its session on the session's host thread,
/// which queues the stored `agent/<id>/done` (an `AgentDone`, with the
/// failed part of a failed task) behind the turn's stream, and returns the
/// outcome. A task whose session is not open, or that never reached its
/// thread, fails, and its `done` is published here.
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
        let images = self.sessions.take_images(&task.id);
        let ran = match self.sessions.host(&task.session) {
            Some(host) => host.run(task.clone(), images).await,
            None => Ran::Unreported(TaskEnd::failed(
                TaskFailure::Turn,
                format!("session {} is not open in this runtime", task.session),
            )),
        };
        let end = match ran {
            Ran::Reported(end) => end,
            Ran::Unreported(end) => {
                // A session id that is no topic level was never opened:
                // nobody can listen for its outcome.
                if let Ok(topics) = SessionTopics::of(&task.session) {
                    let published = self
                        .sessions
                        .outbox()
                        .publish(done_event(&topics, &task, &end))
                        .await;
                    if let Err(error) = published {
                        tracing::error!(
                            "task {:?} ended without its done event: {error:#}",
                            task.id
                        );
                    }
                }
                end
            }
        };
        end.outcome
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
/// its text and images, and its priority when it names one. A kernel event
/// subscription's task starts from the message its event delivers
/// ([`BusEvents::deliver`]), and none is queued for the subscriber's own
/// event. Its trust is
/// the request's; an untrusted task's prompt is wrapped as
/// `<untrusted-event source="<the event's source>">…</untrusted-event>`.
pub(crate) struct RuntimeTaskSink<B: SessionBuilder> {
    pub(crate) queue: TaskQueue<SessionRunner<B>, LogTaskStore>,
    pub(crate) sessions: Arc<Sessions<B>>,
    pub(crate) tree: MemorySessionTree,
    /// The kernel's event bus: what its subscriptions' events deliver.
    pub(crate) events: Arc<BusEvents>,
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
        let (prompt, priority, images) = if let Some(subscription) = request
            .subscription
            .strip_prefix(crate::events::SUBSCRIPTION_PREFIX)
        {
            // A kernel event subscription: the message its event delivers, or
            // nothing (the subscriber's own event, or a session this runtime
            // does not run).
            let Some(message) = self.events.deliver(subscription, &request.event).await? else {
                return Ok(());
            };
            (message, request.priority, Vec::new())
        } else if request.prompt.is_empty() {
            let input = serde_json::from_value::<AgentInput>(event.payload.clone())
                .context("the event carries no input text")?;
            (
                input.text,
                input.priority.unwrap_or(request.priority),
                input.images,
            )
        } else {
            (request.prompt, request.priority, Vec::new())
        };
        // Outside content reaches the model marked as such, with where it
        // came from; it cannot close the mark early. (The system prompt is
        // untouched: it stays cache-stable.)
        let prompt = match request.trust {
            Trust::Owner => prompt,
            Trust::Untrusted => {
                let attribute = event
                    .source
                    .replace('&', "&amp;")
                    .replace('"', "&quot;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                let body = prompt.replace("</untrusted-event", "&lt;/untrusted-event");
                format!("<untrusted-event source=\"{attribute}\">{body}</untrusted-event>")
            }
        };
        let id = uuid::Uuid::now_v7().to_string();
        self.sessions.attach_images(&id, images);
        self.queue
            .push(Task {
                id,
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
