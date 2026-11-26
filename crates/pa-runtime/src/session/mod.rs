//! Sessions: each runs on its own host thread ([`host`]), and the task queue
//! reaches them through the [`SessionRunner`] and the bus through the
//! [`RuntimeTaskSink`] ([`runner`]).
//!
//! What a session's engine is built from is the embedder's: the runtime
//! hands a [`SessionBuilder`] the session's id and [`SessionSpec`] on the
//! session's own thread, inside its scope, and gets the engine back, with
//! what the turn driver around it reports ([`DriverEvents`]) and the
//! autonomous run it starts with. A spawned subagent is a session too: its
//! spec says where it sits under its parent ([`SubagentSpec`]).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use pa_core::autonomous::AgentAutonomousConfig;
use pa_core::kernel::{CallPolicy, HostRequestHandlers};
use pa_core::session_engine::bash_notices::{BashNoticeWake, KernelNoticeSink};
use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::headless::HeadlessTerminalResult;
use pa_core::session_engine::rlm_host::RlmSubagentHost;
use pa_core::LateSentAgentMessageHandler;
use pa_types::ai::{ImageContent, Model, ModelThinkingLevel};
use pa_types::event_log::{AgentDone, Cause, EventId, NewEvent, Task, TaskState, Topic};
use tokio::sync::{mpsc, oneshot};

use crate::bus::Published;
use crate::events::BusEvents;
use crate::lock;
use crate::subagents::Subagents;

mod host;
mod runner;

pub(crate) use host::LiveEngine;
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
    /// Where a spawned subagent sits under its parent; `None` for any other
    /// session.
    pub subagent: Option<SubagentSpec>,
    /// The transcript the session opens, or creates under its id when it is
    /// not there yet: a daemon session's, which the daemon holds the lease
    /// of. `None` leaves it to the builder (the print session's selection, a
    /// subagent's under its parent's).
    pub transcript: Option<PathBuf>,
}

/// A subagent's place under the session that spawned it (`rlm-spawn`): what
/// its builder makes its engine from. The runtime has decided its id, name,
/// working directory and depth; the builder resolves the model and thinking
/// level it runs and makes its transcript, under the parent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSpec {
    /// The parent session's id.
    pub parent: String,
    /// The parent's session file, when it keeps one: the subagent's
    /// transcript names it and lives under its artifacts.
    pub parent_session_file: Option<String>,
    /// The model (`provider/id`) and thinking level the parent runs, which
    /// the subagent inherits unless the spawn names its own.
    pub parent_model: Option<String>,
    pub parent_thinking: Option<ModelThinkingLevel>,
    /// The subagent's handle in its parent's roster (`rlm_child_id`).
    pub child_id: String,
    /// How deep it is: its parent's depth and one.
    pub depth: u32,
    /// The model and thinking level the spawn asked for.
    pub model: Option<String>,
    pub thinking: Option<String>,
    /// The host request that spawned it.
    pub spawned_by_request_id: Option<String>,
    /// A factory state spawned it.
    pub factory: bool,
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
/// `seams` is what the runtime gives the engine: the engine must install
/// all of it ([`SessionSeams`]). A subagent's `spec` carries its
/// [`SubagentSpec`]: its engine runs at that depth, its transcript is made
/// under `id` with the parent's edge, and the model it resolves is the
/// spawn's (an error is the spawn's error).
pub trait SessionBuilder: Send + Sync + 'static {
    /// Builds session `id`'s engine.
    fn build(
        &self,
        id: &str,
        spec: &SessionSpec,
        seams: SessionSeams,
    ) -> impl Future<Output = anyhow::Result<BuiltSession>> + Send;
}

/// What the runtime gives each engine it has built, for the engine to install.
#[derive(Clone)]
pub struct SessionSeams {
    /// The session's trust gate: the engine's kernel must ask it before
    /// every call a cell makes past the kernel
    /// (`SessionEngineConfig::call_policy`), or an untrusted task's cells
    /// run with an owner's reach.
    pub call_policy: Arc<dyn CallPolicy>,
    /// The host requests the runtime answers for the session (the kernel's
    /// `event.*`, on the runtime's bus, and its family's `agent_message.*`):
    /// merged into the engine's (`SessionEngineConfig::extra_host_handlers`)
    /// over its defaults.
    pub host_handlers: HostRequestHandlers,
    /// The session's `rlm.*` children, each a runtime session of its own
    /// (`SessionEngineConfig::rlm_subagent_host`).
    pub subagents: Arc<dyn RlmSubagentHost>,
    /// Records a fire-and-forget `agent_message.send`'s late receipt; the
    /// session is not at rest until it has
    /// (`SessionEngineConfig::on_late_sent_agent_message`).
    pub on_late_sent_agent_message: LateSentAgentMessageHandler,
    /// Wakes the session's quiescence drive when its background work
    /// settles (`SessionEngineConfig::on_background_work_settled`).
    pub bash_wake: BashNoticeWake,
    /// Admits the kernel's notices (a background command's end or silence,
    /// a background cell's end, a factory milestone): steers each the
    /// session takes onto its agent and publishes it on
    /// `session/<id>/bash/done` or `/bash/quiet`, whose default subscription
    /// wakes the session (`SessionEngineConfig::kernel_notice_sink`).
    pub kernel_notices: KernelNoticeSink,
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
/// task's trust, caused by `cause` (the task's trigger): one hop past it.
pub(crate) fn done_event(
    topics: &SessionTopics,
    task: &Task,
    end: &host::TaskEnd,
    cause: &Cause,
) -> NewEvent {
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
        cause: Some(cause.clone()),
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
        published: Option<oneshot::Sender<anyhow::Result<Published>>>,
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

    /// Queues `event` and waits until it is published, with the routes it
    /// set off run.
    pub(crate) async fn publish(&self, event: NewEvent) -> anyhow::Result<Published> {
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
    events: Arc<BusEvents>,
    subagents: Arc<Subagents>,
    hosts: Mutex<HashMap<String, Arc<SessionHost>>>,
    /// What a queued task's store row does not keep, by task id, until it
    /// runs: its prompt's images and its trigger as a cause. A task restored
    /// after a restart runs without images, and its trigger counts as the
    /// start of its chain; a queued task cancelled before it ran leaves its
    /// entry here until the runtime closes.
    queued: Mutex<HashMap<String, Queued>>,
}

/// What a queued task carries beside its store row.
pub(crate) struct Queued {
    pub(crate) images: Vec<ImageContent>,
    /// The task's trigger, as the cause of the events its turn publishes.
    pub(crate) cause: Cause,
}

impl<B: SessionBuilder> Sessions<B> {
    pub(crate) fn new(
        builder: B,
        outbox: Outbox,
        events: Arc<BusEvents>,
        subagents: Arc<Subagents>,
    ) -> Self {
        Sessions {
            builder: Arc::new(builder),
            outbox,
            events,
            subagents,
            hosts: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
        }
    }

    /// Keeps `queued` for the task `task` until it runs.
    pub(crate) fn attach(&self, task: &str, queued: Queued) {
        lock(&self.queued).insert(task.to_string(), queued);
    }

    /// What `task` was queued with: its images, and its trigger as a cause
    /// (a restored task's trigger at the start of its chain).
    pub(crate) fn take_queued(&self, task: &Task) -> Queued {
        lock(&self.queued)
            .remove(&task.id)
            .unwrap_or_else(|| Queued {
                images: Vec::new(),
                cause: Cause {
                    id: task.trigger.clone(),
                    trust: task.trust,
                    hops: 0,
                },
            })
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
        let node = self.subagents.node_for(&id, &spec);
        let host = SessionHost::start(
            id.clone(),
            spec,
            node,
            topics,
            Arc::clone(&self.builder),
            self.outbox.clone(),
            Arc::clone(&self.events),
        )
        .context("start the session's host thread")?;
        hosts.insert(id.clone(), Arc::new(host));
        self.events.opened(&id);
        Ok(id)
    }

    pub(crate) fn host(&self, id: &str) -> Option<Arc<SessionHost>> {
        lock(&self.hosts).get(id).cloned()
    }

    pub(crate) fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// The sessions' family tree, for tests to reach its nodes.
    #[cfg(test)]
    pub(crate) fn subagents(&self) -> &Arc<Subagents> {
        &self.subagents
    }

    /// Closes session `id` and waits until its thread has let go.
    pub(crate) async fn close(&self, id: &str) {
        let host = lock(&self.hosts).remove(id);
        if let Some(host) = host {
            host.close().await;
        }
        self.events.closed(id);
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
