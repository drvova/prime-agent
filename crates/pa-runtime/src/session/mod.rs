//! Sessions: each runs on its own host thread ([`host`]), and the task queue
//! reaches them through the [`SessionRunner`] and the bus through the
//! [`RuntimeTaskSink`] ([`runner`]).
//!
//! What a session's engine is built from is the embedder's: the runtime
//! hands a [`SessionBuilder`] the session's id and [`SessionSpec`] on the
//! session's own thread, inside its scope, and gets the engine back, with
//! what the turn driver around it reports ([`DriverEvents`]), the autonomous
//! run it starts with, and the embedder's [`InlineSubagents`] if it hosts
//! any.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use pa_core::autonomous::AgentAutonomousConfig;
use pa_core::kernel::CallPolicy;
use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::headless::HeadlessTerminalResult;
use pa_types::ai::{ImageContent, Model};
use pa_types::event_log::{AgentDone, EventId, NewEvent, Task, TaskState, Topic};
use tokio::sync::{mpsc, oneshot};

use crate::turn::boundary::TurnBoundary;

use crate::lock;

mod host;
mod runner;

use host::SessionHost;
pub(crate) use runner::{RuntimeTaskSink, SessionRunner};

/// A session to open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSpec {
    /// The session's id: the `<id>` of its `agent/<id>/*` topics, and the id
    /// the builder opens or creates its transcript under. `None` mints a new
    /// one.
    pub id: Option<String>,
    /// What the session is called, when an event named it (`AgentTarget::Spawn`).
    pub name: Option<String>,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session's environment over the process's own
    /// (`pa_types::session_scope`).
    pub env: BTreeMap<String, String>,
}

/// A session's built engine, with the model and request auth its turns run on
/// and what its turn driver starts from.
pub struct BuiltSession {
    pub engine: Arc<SessionEngine>,
    pub model: Model,
    pub api_key: Option<String>,
    /// The agent dir the engine was built over: its settings and harness
    /// state.
    pub agent_dir: PathBuf,
    /// What the turn driver reports beside the agent's own events.
    pub events: DriverEvents,
    /// The autonomous run the session starts with (the print modes'
    /// autonomous flags); `None` starts it disabled, and `/autonomous`
    /// rewrites it live.
    pub autonomous: Option<AgentAutonomousConfig>,
    /// The subagents the embedder runs inside the session's process, which
    /// the goal and autonomous continuations wait behind and a settle drives
    /// to rest.
    pub subagents: Option<Arc<dyn InlineSubagents>>,
}

/// What a session's turn driver (its compaction, refinement, goal and
/// autonomous arms) reports beside the agent's own events, which always go
/// on the session's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverEvents {
    /// Its events, in the json stream's shapes, on the session's stream.
    Stream,
    /// None on the stream; a background refinement that fails says so on
    /// stderr: the text print mode's surface.
    Quiet,
}

/// The subagents an embedder runs inside a session's process (the print
/// modes' inline RLM children): the facts the turn driver reads about them,
/// the drive that runs the session's turns their messages leave, and their
/// end.
///
/// A session holds its embedder's subagents as a trait object, so the
/// methods return boxed futures. `unsettled` answers from the live state
/// (a running child, a row queued on a child's or the session's agent, live
/// background work), never a stale flag. `drive_to_quiescence` runs every
/// turn the queued rows ask for through `boundary`, waits out unsettled
/// subagents, and returns once all of it is at rest; `dispose` ends every
/// subagent (no kernel or driver outlives it) and is the last call.
pub trait InlineSubagents: Send + Sync {
    /// Whether any subagent work is unsettled.
    fn unsettled(&self) -> pa_agent::BoxFut<'static, bool>;

    /// Drives the session to quiescence; whether any turn ran.
    ///
    /// # Errors
    ///
    /// A turn, or the boundary around it, failed; the text is its reason.
    fn drive_to_quiescence<'a>(
        &'a self,
        engine: &'a Arc<SessionEngine>,
        boundary: &'a mut TurnBoundary,
        model: &'a Model,
        api_key: Option<String>,
        global_harness_dir: PathBuf,
    ) -> pa_agent::BoxFut<'a, Result<bool, String>>;

    /// Ends every subagent.
    fn dispose(&self) -> pa_agent::BoxFut<'_, ()>;
}

/// How a session's run ended once it settled ([`crate::Runtime::settle`]):
/// what a print run's exit contract reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    /// The session's terminal result: its last reply or failure and the
    /// compaction outcomes after it.
    pub terminal: HeadlessTerminalResult,
    /// The goal stopped at its no-progress cap during this run: the reason.
    pub goal_stopped: Option<String>,
    /// The autonomous run ends with a gate still failing or a limit before
    /// terminal evidence: why.
    pub autonomous_failure: Option<String>,
}

/// When a settle ([`crate::Runtime::settle`]) runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleAfter {
    /// After the work the session's thread holds now.
    Queued,
    /// Right after the task the input event with this id queued, on the
    /// session's thread with nothing between them, and only when that task
    /// ended done or with a failed reply: a task that stops the run (a failed
    /// command or turn, a cancel) leaves nothing to settle. The task must
    /// run as its own (not a steer a running turn takes).
    Task(EventId),
}

/// What finishing a session's run found ([`crate::Runtime::finish`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    /// A `/refine` command's refinement failed.
    pub refine_command_failed: bool,
}

/// Builds the engine a session runs its turns on: the embedder's choice of
/// provider, settings and session manager (the composition root's headless
/// assembly in the product, a faux provider in tests).
///
/// `build` runs on the session's host thread, inside the session's scope
/// (`pa_types::session_scope`) and on its runtime, so what the engine
/// spawns belongs to the session. It is called when the session opens and
/// again after a turn panicked, each time for a fresh engine of the same
/// session: it opens the session `id`'s transcript, creating it the first
/// time. It must not copy another engine (each engine is built on its own;
/// its kernel comes from pa-core's spare pool), and an error fails the task
/// that needed the engine, with the error as its reason.
///
/// `call_policy` is the session's trust gate: the engine's kernel must ask
/// it before every call a cell makes past the kernel
/// (`SessionEngineConfig::call_policy`), or an untrusted task's cells run
/// with an owner's reach.
pub trait SessionBuilder: Send + Sync + 'static {
    /// Builds session `id`'s engine.
    fn build(
        &self,
        id: &str,
        spec: &SessionSpec,
        call_policy: Arc<dyn CallPolicy>,
    ) -> impl Future<Output = anyhow::Result<BuiltSession>> + Send;
}

/// The topics a session publishes on.
#[derive(Debug, Clone)]
pub(crate) struct SessionTopics {
    /// `agent/<id>/stream`: live only.
    pub(crate) stream: Topic,
    /// `agent/<id>/done`: stored.
    pub(crate) done: Topic,
}

impl SessionTopics {
    /// The topics of session `id`.
    ///
    /// # Errors
    ///
    /// `id` cannot be a topic level (it is empty, holds `/`, `+` or `#`, or
    /// makes a topic too long).
    pub(crate) fn of(id: &str) -> anyhow::Result<Self> {
        let topic = |kind: &str| {
            let topic = Topic::new(format!("agent/{id}/{kind}"))
                .with_context(|| format!("session id {id:?} is not a topic level"))?;
            anyhow::ensure!(
                topic.levels().count() == 3,
                "session id {id:?} is not a topic level"
            );
            Ok(topic)
        };
        Ok(SessionTopics {
            stream: topic("stream")?,
            done: topic("done")?,
        })
    }
}

/// The stored `agent/<id>/done` that reports how `task` ended, with the
/// task's trust.
pub(crate) fn done_event(topics: &SessionTopics, task: &Task, end: &host::TaskEnd) -> NewEvent {
    let (outcome, error) = match &end.outcome {
        crate::tasks::TaskOutcome::Done => (TaskState::Done, None),
        crate::tasks::TaskOutcome::Failed(error) => (TaskState::Failed, Some(error.clone())),
        crate::tasks::TaskOutcome::Cancelled => (TaskState::Cancelled, None),
    };
    let done = AgentDone {
        task: task.id.clone(),
        outcome,
        error,
        failure: end.failure,
    };
    NewEvent {
        topic: topics.done.clone(),
        source: task.session.clone(),
        session: Some(task.session.clone()),
        payload: serde_json::to_value(done).unwrap_or_default(),
        trust: task.trust,
        cause: None,
        reply: None,
    }
}

/// What the sessions queue for the bus, in order.
#[expect(
    clippy::large_enum_variant,
    reason = "every stream event is an `Event`: boxing it would allocate per event for the rare answer"
)]
pub(crate) enum Outgoing {
    /// One event, and who waits for it to be published.
    Event {
        event: NewEvent,
        published: Option<oneshot::Sender<anyhow::Result<()>>>,
    },
    /// Runs once everything queued before it is published: an answer that
    /// must reach its caller after the events it follows.
    Then(Box<dyn FnOnce() + Send>),
}

/// The sessions' one way onto the bus: a session's thread queues events
/// without blocking and the runtime's own task publishes them in order, so a
/// task's `done` always follows its stream, and nothing a route sets off
/// runs on a session's runtime.
#[derive(Clone)]
pub(crate) struct Outbox {
    sender: mpsc::UnboundedSender<Outgoing>,
}

impl Outbox {
    pub(crate) fn channel() -> (Outbox, mpsc::UnboundedReceiver<Outgoing>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Outbox { sender }, receiver)
    }

    /// Queues `event`; once the runtime has stopped it is dropped.
    pub(crate) fn send(&self, event: NewEvent) {
        // A closed outbox is a runtime that stopped: nobody hears the event.
        let _ = self.sender.send(Outgoing::Event {
            event,
            published: None,
        });
    }

    /// Queues `event` and waits until it is published.
    pub(crate) async fn publish(&self, event: NewEvent) -> anyhow::Result<()> {
        let (published, answer) = oneshot::channel();
        self.sender
            .send(Outgoing::Event {
                event,
                published: Some(published),
            })
            .map_err(|_| anyhow::anyhow!("the runtime has stopped"))?;
        answer
            .await
            .map_err(|_| anyhow::anyhow!("the runtime has stopped"))?
    }

    /// Runs `then` once every event queued so far is published (a listener
    /// has them all before what `then` sends), without waiting for it.
    pub(crate) fn then(&self, then: impl FnOnce() + Send + 'static) {
        // A stopped runtime publishes nothing more: nothing is ahead of it.
        if let Err(mpsc::error::SendError(Outgoing::Then(then))) =
            self.sender.send(Outgoing::Then(Box::new(then)))
        {
            then();
        }
    }
}

/// The open sessions, and what opens more.
pub(crate) struct Sessions<B> {
    builder: Arc<B>,
    outbox: Outbox,
    hosts: Mutex<HashMap<String, Arc<SessionHost>>>,
    /// The images of queued prompts, by task id, until their task runs: the
    /// task store keeps a prompt's text only, so a task restored after a
    /// restart runs without them (and a queued task cancelled before it ran
    /// leaves its images here until the runtime closes).
    images: Mutex<HashMap<String, Vec<ImageContent>>>,
}

impl<B: SessionBuilder> Sessions<B> {
    pub(crate) fn new(builder: B, outbox: Outbox) -> Self {
        Sessions {
            builder: Arc::new(builder),
            outbox,
            hosts: Mutex::new(HashMap::new()),
            images: Mutex::new(HashMap::new()),
        }
    }

    /// Keeps `images` for the task `task` until it runs.
    pub(crate) fn attach_images(&self, task: &str, images: Vec<ImageContent>) {
        if !images.is_empty() {
            lock(&self.images).insert(task.to_string(), images);
        }
    }

    /// The images the task `task` was queued with.
    pub(crate) fn take_images(&self, task: &str) -> Vec<ImageContent> {
        lock(&self.images).remove(task).unwrap_or_default()
    }

    /// Opens a session on its own host thread, which starts building its
    /// engine; returns its id.
    ///
    /// # Errors
    ///
    /// The id is not a topic level or is open already, or the operating
    /// system refused the thread.
    pub(crate) fn open(&self, spec: SessionSpec) -> anyhow::Result<String> {
        let id = spec
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let topics = SessionTopics::of(&id)?;
        let mut hosts = lock(&self.hosts);
        anyhow::ensure!(!hosts.contains_key(&id), "session {id} is open already");
        let host = SessionHost::start(
            id.clone(),
            spec,
            topics,
            Arc::clone(&self.builder),
            self.outbox.clone(),
        )
        .context("start the session's host thread")?;
        hosts.insert(id.clone(), Arc::new(host));
        Ok(id)
    }

    pub(crate) fn host(&self, id: &str) -> Option<Arc<SessionHost>> {
        lock(&self.hosts).get(id).cloned()
    }

    pub(crate) fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// Closes every session and waits until their threads have let go.
    pub(crate) async fn close_all(&self) {
        let hosts: Vec<Arc<SessionHost>> =
            lock(&self.hosts).drain().map(|(_, host)| host).collect();
        for host in hosts {
            host.close().await;
        }
    }
}

#[cfg(test)]
mod tests;
