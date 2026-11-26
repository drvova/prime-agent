//! Where the task queue and the bus meet the sessions: the
//! [`SessionRunner`] runs a task on its session's host and reports how it
//! ended, and the [`RuntimeTaskSink`] turns an `Agent` route into a task.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use pa_types::event_log::{
    AgentAccepted, AgentInput, AgentTarget, Cause, Task, TaskFailure, TaskState, Topic, Trust,
};

use super::host::{Ran, TaskEnd};
use super::{done_event, JobsSlot, Queued, SessionBuilder, SessionSpec, SessionTopics, Sessions};
use crate::events::BusEvents;
use crate::log_stores::LogTaskStore;
use crate::routes::{Announced, TaskRequest, TaskSink};
use crate::subagents::{
    Subagents, INBOX_SUBSCRIPTION, NOTICE_SUBSCRIPTION, SUBSCRIPTION_PREFIX as SUBAGENT_PREFIX,
};
use crate::subscriptions::MemorySessionTree;
use crate::tasks::{Steered, TaskOutcome, TaskQueue, TaskRunner};

/// The prefix of the subscriptions that hear a scheduled job's timer
/// (`schedule:<job>`, routed to the job's session).
pub(crate) const SCHEDULE_PREFIX: &str = "schedule:";

/// What an `Agent` route of the runtime does, by its subscription's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteKind<'a> {
    /// `subagent/<child>`: settles the child on its `done`; no task.
    SubagentDone { child: &'a str },
    /// [`INBOX_SUBSCRIPTION`]: delivers the agent message; no task of its
    /// own (a settled child it reaches is given its next one).
    Inbox,
    /// [`NOTICE_SUBSCRIPTION`]: wakes the session for a notice it took; no
    /// task of its own (as the inbox).
    Notice,
    /// `event-bus/<id>`: a kernel event subscription's task, over the
    /// message its event delivers.
    Event { subscription: &'a str },
    /// [`SCHEDULE_PREFIX`]`<job>`: a scheduled job's timer fired; the
    /// runtime daemon's jobs run the session's due ones. No task of its own.
    Schedule,
    /// Any other: a task over the route's prompt or the event's input.
    Task,
}

impl<'a> RouteKind<'a> {
    fn of(subscription: &'a str) -> Self {
        if let Some(child) = subscription.strip_prefix(SUBAGENT_PREFIX) {
            return RouteKind::SubagentDone { child };
        }
        if let Some(subscription) = subscription.strip_prefix(crate::events::SUBSCRIPTION_PREFIX) {
            return RouteKind::Event { subscription };
        }
        if subscription.starts_with(SCHEDULE_PREFIX) {
            return RouteKind::Schedule;
        }
        match subscription {
            INBOX_SUBSCRIPTION => RouteKind::Inbox,
            NOTICE_SUBSCRIPTION => RouteKind::Notice,
            _ => RouteKind::Task,
        }
    }
}

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
    /// ends `Cancelled`. The steers it took and had not read, queued again
    /// for the caller to put back in the lane.
    pub(crate) fn cancel_running(&self, session: &str) -> Vec<Task> {
        self.sessions
            .host(session)
            .map_or_default(|host| host.cancel_running())
    }

    /// [`Self::cancel_running`] when `session` runs task `task`: the steers
    /// to put back, or `None` when it does not run it.
    pub(crate) fn cancel_running_task(&self, session: &str, task: &str) -> Option<Vec<Task>> {
        self.sessions.host(session)?.cancel_running_task(task)
    }
}

impl<B: SessionBuilder> TaskRunner for SessionRunner<B> {
    #[tracing::instrument(level = "debug", skip_all, fields(task = %task.id, session = %task.session))]
    async fn run(&self, task: Task) -> TaskOutcome {
        let Queued { images, cause } = self.sessions.take_queued(&task);
        let ran = match self.sessions.host(&task.session) {
            Some(host) => host.run(task.clone(), images, cause.clone()).await,
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
                        .publish(done_event(&topics, &task, &end, &cause))
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
/// event. The runtime's own routes ([`RouteKind`]) queue no task of their
/// own: a subagent's `done` settles the subagent, whose notice its parent's
/// agent takes ([`Subagents::deliver_done`]); an agent message is delivered
/// to its receiver ([`Subagents::deliver_inbox`]); a kernel notice the
/// session took wakes it ([`Subagents::deliver_notice`]). A task's trust is
/// the request's, and the event it runs for is the cause of what its turn
/// publishes; an untrusted task's prompt is wrapped as
/// `<untrusted-event source="<the event's source>">…</untrusted-event>`.
/// A task queued for a session's input (`agent/<id>/input`) is announced on
/// `agent/<id>/accepted` (an [`AgentAccepted`]: the task and how its lane
/// admitted it), caused by the input, so the publish of the input carries it.
pub(crate) struct RuntimeTaskSink<B: SessionBuilder> {
    pub(crate) queue: TaskQueue<SessionRunner<B>, LogTaskStore>,
    pub(crate) sessions: Arc<Sessions<B>>,
    pub(crate) tree: MemorySessionTree,
    /// The kernel's event bus: what its subscriptions' events deliver.
    pub(crate) events: Arc<BusEvents>,
    /// The sessions' subagents: what their `done` settles.
    pub(crate) subagents: Arc<Subagents>,
    /// Where a spawned session without a `cwd` of its own runs.
    pub(crate) cwd: PathBuf,
    /// The runtime daemon's scheduled jobs, once attached.
    pub(crate) jobs: JobsSlot,
}

impl<B: SessionBuilder> TaskSink for RuntimeTaskSink<B> {
    #[expect(
        clippy::too_many_lines,
        reason = "the route's kinds, its prompt, its task and its announcement read top to bottom"
    )]
    async fn queue(&self, request: TaskRequest) -> anyhow::Result<Option<Announced>> {
        let kind = RouteKind::of(&request.subscription);
        match kind {
            RouteKind::SubagentDone { child } => {
                self.subagents.deliver_done(child, &request.event).await;
                return Ok(None);
            }
            RouteKind::Inbox => {
                self.subagents.deliver_inbox(&request.event).await;
                return Ok(None);
            }
            RouteKind::Notice => {
                return self
                    .subagents
                    .deliver_notice(&request.event)
                    .await
                    .map(|()| None);
            }
            RouteKind::Schedule => {
                let AgentTarget::Session { session } = &request.target else {
                    anyhow::bail!("a scheduled job's timer routes to its session");
                };
                let jobs = self
                    .jobs
                    .get()
                    .context("this runtime runs no scheduled jobs")?;
                jobs.fired(session);
                return Ok(None);
            }
            RouteKind::Event { .. } | RouteKind::Task => {}
        }
        let event = &request.event.event;
        let session = match request.target {
            AgentTarget::Session { session } => session,
            AgentTarget::Origin => event
                .session
                .clone()
                .context("the event was published in no session")?,
            AgentTarget::Spawn { name, cwd } => {
                let id = self.sessions.open(
                    SessionSpec {
                        id: None,
                        name,
                        cwd: cwd.map_or_else(|| self.cwd.clone(), PathBuf::from),
                        env: std::collections::BTreeMap::new(),
                        subagent: None,
                        transcript: None,
                        options: crate::SessionOptions::default(),
                    },
                    super::Opening::Fresh,
                )?;
                if let Some(parent) = &event.session {
                    self.tree.insert(id.clone(), parent.clone());
                }
                id
            }
        };
        let (prompt, priority, images) = if let RouteKind::Event { subscription } = kind {
            // A kernel event subscription: the message its event delivers, or
            // nothing (the subscriber's own event, or a session this runtime
            // does not run).
            let Some(message) = self.events.deliver(subscription, &request.event).await? else {
                return Ok(None);
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
            // An empty input carries no content: it wakes the session over
            // the rows already delivered to it.
            Trust::Untrusted if prompt.is_empty() => prompt,
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
        self.sessions.attach(
            &id,
            Queued {
                images,
                cause: Cause {
                    id: request.event.id.clone(),
                    trust: event.trust,
                    hops: request.event.hops(),
                },
            },
        );
        let input =
            Topic::new(format!("agent/{session}/input")).is_ok_and(|input| input == event.topic);
        let admission = self
            .queue
            .push(Task {
                id: id.clone(),
                session: session.clone(),
                priority,
                state: TaskState::Queued,
                trigger: request.event.id.clone(),
                prompt,
                trust: request.trust,
                attempts: 0,
                error: None,
            })
            .await?;
        if !input {
            return Ok(None);
        }
        let accepted = AgentAccepted {
            task: id,
            priority,
            admission,
        };
        Ok(Some(Announced {
            topic: Topic::new(format!("agent/{session}/accepted"))?,
            payload: serde_json::to_value(accepted)?,
        }))
    }
}
