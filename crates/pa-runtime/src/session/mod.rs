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
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context as _;
use pa_core::autonomous::AgentAutonomousConfig;
use pa_core::kernel::{CallPolicy, HostRequestHandlers};
use pa_core::session_engine::bash_notices::{BashNoticeWake, KernelNoticeSink};
use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::headless::HeadlessTerminalResult;
use pa_core::session_engine::rlm_host::RlmSubagentHost;
use pa_core::session_engine::runtime_wiring::KernelCronWiring;
use pa_core::LateSentAgentMessageHandler;
use pa_types::ai::{ImageContent, Model, ModelThinkingLevel};
use pa_types::event_log::{AgentDone, Cause, EventId, NewEvent, Task, TaskState, Topic};
use tokio::sync::{mpsc, oneshot};

use crate::bus::Published;
use crate::events::BusEvents;
use crate::lock;
use crate::subagents::Subagents;

mod faults;
mod host;
mod runner;

pub(crate) use faults::{reap_orphans, Rebuild, SessionFaults, Supervision};
use host::SessionHost;
pub(crate) use host::{LiveEngine, Opening};
pub(crate) use runner::{RuntimeTaskSink, SessionRunner, SCHEDULE_PREFIX};

/// A session to open.
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// What a client chose for the session beyond the embedder's defaults
    /// (`session.create`'s `options`: the flags an ACP launch gives its
    /// session). The default chooses nothing.
    pub options: SessionOptions,
    /// The session keeps nothing: its transcript is in memory only (the
    /// builder writes no file for it), and it ends with the client that
    /// created it (`--no-session`, the old daemon's client-owned in-memory
    /// session). `transcript` names no file for it. The runtime's stores
    /// keep nothing of it or of the subagents it spawns: no event, task or
    /// state of theirs is written to the agent's database.
    pub ephemeral: bool,
}

/// A client's choices for one session's build, over the embedder's own
/// (the launch flags a print run takes from its command line): its model,
/// thinking level, system prompt, resources, autonomous run and goal.
/// Each `None` or empty field leaves the embedder's choice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionOptions {
    /// `--provider`.
    pub provider: Option<String>,
    /// `--model`, a selector the model registry resolves.
    pub model: Option<String>,
    /// `--thinking`.
    pub thinking: Option<ModelThinkingLevel>,
    /// `--system-prompt`.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt`, each in order.
    pub append_system_prompt: Vec<String>,
    /// `--skill` paths.
    pub skills: Vec<PathBuf>,
    /// `--prompt-template` paths.
    pub prompt_templates: Vec<PathBuf>,
    /// The autonomous run the session starts with (`--autonomous` and its
    /// limits and gates).
    pub autonomous: Option<AgentAutonomousConfig>,
    /// `--goal`: a fresh session starts it; a resumed one keeps its own.
    pub goal: Option<SessionGoal>,
    /// The telemetry execution mode the session reports (`acp`); `None` is
    /// the embedder's.
    pub execution_mode: Option<String>,
}

/// The goal a session starts with (`--goal`, `--goal-token-budget`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionGoal {
    pub objective: String,
    pub token_budget: Option<u64>,
}

impl SessionOptions {
    /// The options a client gave as JSON (`{"provider", "model", "thinking",
    /// "systemPrompt", "appendSystemPrompt", "skills", "promptTemplates",
    /// "autonomous", "goal": {"objective", "tokenBudget"},
    /// "executionMode"}`), refusing a key it does not know.
    ///
    /// # Errors
    ///
    /// `value` is not an object, names an unknown key, or a value has the
    /// wrong shape.
    pub fn from_json(value: &serde_json::Value) -> anyhow::Result<Self> {
        use serde_json::Value;
        let object = value
            .as_object()
            .context("the session options must be an object")?;
        let text = |key: &str, value: &Value| -> anyhow::Result<String> {
            value
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("the `{key}` option must be a string"))
        };
        let texts = |key: &str, value: &Value| -> anyhow::Result<Vec<String>> {
            value
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                })
                .with_context(|| format!("the `{key}` option must be a list of strings"))
        };
        let mut options = SessionOptions::default();
        for (key, value) in object {
            if value.is_null() {
                continue;
            }
            match key.as_str() {
                "provider" => options.provider = Some(text(key, value)?),
                "model" => options.model = Some(text(key, value)?),
                "thinking" => {
                    options.thinking = Some(
                        serde_json::from_value(value.clone())
                            .with_context(|| format!("unknown thinking level {value}"))?,
                    );
                }
                "systemPrompt" => options.system_prompt = Some(text(key, value)?),
                "appendSystemPrompt" => options.append_system_prompt = texts(key, value)?,
                "skills" => {
                    options.skills = texts(key, value)?.into_iter().map(PathBuf::from).collect();
                }
                "promptTemplates" => {
                    options.prompt_templates =
                        texts(key, value)?.into_iter().map(PathBuf::from).collect();
                }
                "autonomous" => {
                    options.autonomous = Some(
                        serde_json::from_value(value.clone())
                            .context("the `autonomous` option is not an autonomous config")?,
                    );
                }
                "goal" => {
                    let objective = value
                        .get("objective")
                        .and_then(Value::as_str)
                        .filter(|objective| !objective.trim().is_empty())
                        .context("the `goal` option needs an `objective`")?;
                    let token_budget = match value.get("tokenBudget") {
                        None | Some(Value::Null) => None,
                        Some(budget) => Some(
                            budget
                                .as_u64()
                                .context("the goal's `tokenBudget` must be a whole number")?,
                        ),
                    };
                    options.goal = Some(SessionGoal {
                        objective: objective.to_string(),
                        token_budget,
                    });
                }
                "executionMode" => options.execution_mode = Some(text(key, value)?),
                unknown => anyhow::bail!("unknown session option `{unknown}`"),
            }
        }
        Ok(options)
    }

    /// The options as [`SessionOptions::from_json`] reads them, the chosen
    /// ones only.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        let mut put = |key: &str, value: serde_json::Value| {
            object.insert(key.to_string(), value);
        };
        if let Some(provider) = &self.provider {
            put("provider", provider.clone().into());
        }
        if let Some(model) = &self.model {
            put("model", model.clone().into());
        }
        if let Some(thinking) = self.thinking {
            put(
                "thinking",
                serde_json::to_value(thinking).unwrap_or_default(),
            );
        }
        if let Some(system_prompt) = &self.system_prompt {
            put("systemPrompt", system_prompt.clone().into());
        }
        if !self.append_system_prompt.is_empty() {
            put(
                "appendSystemPrompt",
                self.append_system_prompt.clone().into(),
            );
        }
        let paths = |paths: &[PathBuf]| -> serde_json::Value {
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .into()
        };
        if !self.skills.is_empty() {
            put("skills", paths(&self.skills));
        }
        if !self.prompt_templates.is_empty() {
            put("promptTemplates", paths(&self.prompt_templates));
        }
        if let Some(autonomous) = &self.autonomous {
            put(
                "autonomous",
                serde_json::to_value(autonomous).unwrap_or_default(),
            );
        }
        if let Some(goal) = &self.goal {
            put(
                "goal",
                serde_json::json!({ "objective": goal.objective, "tokenBudget": goal.token_budget }),
            );
        }
        if let Some(mode) = &self.execution_mode {
            put("executionMode", mode.clone().into());
        }
        serde_json::Value::Object(object)
    }
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
    /// Where its turns take their steering and follow-up modes from.
    pub queue_modes: QueueModes,
}

/// Where a session's turns take their steering and follow-up modes from
/// (`all`: the steers or follow-ups waiting at a step are read together;
/// `one-at-a-time`: one per step).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueModes {
    /// The modes the engine was built with, unchanged.
    Built,
    /// The global settings' `steeringMode` and `followUpMode`, read as each
    /// turn starts: a change applies from the next turn (the runtime
    /// daemon's sessions).
    Settings,
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
    /// The session's scheduled jobs and heartbeats: the store the kernel's
    /// `schedule.*` and `rlm_heartbeat.*` requests read and write, and the
    /// identity its jobs bind to (`SessionEngineConfig::cron_store`).
    /// `None` outside the runtime daemon: `schedule.*` then refuses and
    /// `rlm_heartbeat.*` is not registered.
    pub scheduled_jobs: Option<KernelCronWiring>,
}

/// The scheduled jobs of a runtime's sessions, which the runtime daemon
/// keeps (`crate::daemon`); a runtime without one runs no scheduled job.
///
/// `wiring` is asked on the session's own thread each time a session's
/// lifetime is built, and must not block: it answers the store the
/// session's kernel binds its jobs to, or `None` for a session that keeps
/// none (a subagent, a session without a transcript). `fired` is told,
/// from the bus's routing, that a job timer of `session` fired; it must
/// return at once and run the session's due jobs elsewhere.
pub(crate) trait SessionJobs: Send + Sync {
    fn wiring(&self, id: &str, spec: &SessionSpec) -> Option<KernelCronWiring>;
    fn fired(&self, session: &str);
}

/// Where a runtime's sessions and its routing reach its [`SessionJobs`],
/// once the runtime daemon attaches them.
pub(crate) type JobsSlot = Arc<OnceLock<Arc<dyn SessionJobs>>>;

/// The topics a session publishes on.
#[derive(Debug, Clone)]
pub(crate) struct SessionTopics {
    /// `agent/<id>/stream`: live only.
    pub(crate) stream: Topic,
    /// `agent/<id>/done`: stored.
    pub(crate) done: Topic,
    /// `agent/<id>/crashed`, `/recovered` and `/failed`: stored, the
    /// session's faults under supervision (each a `SessionFaultEvent`).
    pub(crate) crashed: Topic,
    pub(crate) recovered: Topic,
    pub(crate) failed: Topic,
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
            crashed: topic("crashed")?,
            recovered: topic("recovered")?,
            failed: topic("failed")?,
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
    /// What a crash does to a session.
    faults: SessionFaults,
    /// The sessions' scheduled jobs, in the runtime daemon.
    jobs: JobsSlot,
    /// The sessions the runtime's stores keep nothing of: an ephemeral
    /// session joins as it opens, and so does a subagent of one.
    unstored: crate::log_stores::Unstored,
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
        faults: SessionFaults,
        jobs: JobsSlot,
    ) -> Self {
        Sessions {
            builder: Arc::new(builder),
            outbox,
            events,
            subagents,
            faults,
            jobs,
            unstored: crate::log_stores::Unstored::default(),
            hosts: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
        }
    }

    /// The sessions the runtime's stores keep nothing of.
    pub(crate) fn unstored(&self) -> &crate::log_stores::Unstored {
        &self.unstored
    }

    /// Attaches the runtime daemon's scheduled jobs (once; a second attach
    /// changes nothing).
    pub(crate) fn attach_jobs(&self, jobs: Arc<dyn SessionJobs>) {
        let _ = self.jobs.set(jobs);
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
    /// engine (a revived one's first lifetime is its rebuild); returns its id.
    ///
    /// # Errors
    ///
    /// The id is not a topic level or is open already, or the operating
    /// system refused the thread.
    pub(crate) fn open(&self, spec: SessionSpec, opening: Opening) -> anyhow::Result<String> {
        let id = spec
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let topics = SessionTopics::of(&id)?;
        let mut hosts = lock(&self.hosts);
        anyhow::ensure!(!hosts.contains_key(&id), "session {id} is open already");
        // Before it can publish anything.
        let spawned_by_unstored = spec
            .subagent
            .as_ref()
            .is_some_and(|subagent| self.unstored.contains(&subagent.parent));
        if spec.ephemeral || spawned_by_unstored {
            self.unstored.insert(&id);
        }
        let node = self.subagents.node_for(&id, &spec);
        let host = SessionHost::start(
            id.clone(),
            spec,
            node,
            topics,
            Arc::clone(&self.builder),
            self.outbox.clone(),
            Arc::clone(&self.events),
            self.faults.clone(),
            Arc::clone(&self.jobs),
            opening,
        )
        .context("start the session's host thread")?;
        hosts.insert(id.clone(), Arc::new(host));
        self.events.opened(&id);
        Ok(id)
    }

    /// Opens session `spec.id` again after its daemon died under it: the
    /// crash is recorded (under supervision) and the session's first lifetime
    /// is its rebuild.
    ///
    /// # Errors
    ///
    /// As [`Sessions::open`]; or the spec names no id.
    pub(crate) fn revive(&self, spec: SessionSpec, rebuild: Rebuild) -> anyhow::Result<String> {
        let id = spec.id.clone().context("a revived session needs its id")?;
        match &self.faults {
            SessionFaults::Contained => {}
            SessionFaults::Supervised(supervision) => faults::record_crash(
                supervision,
                &self.outbox,
                &SessionTopics::of(&id)?,
                &id,
                &rebuild,
            ),
        }
        self.open(spec, Opening::Revived(rebuild))
    }

    /// Stores `task`'s `done` as ending with `outcome` (a failure is its
    /// turn's), for a task that will not run on the session's thread.
    ///
    /// # Errors
    ///
    /// The task's session id is not a topic level.
    pub(crate) fn report_end(
        &self,
        task: &Task,
        outcome: crate::tasks::TaskOutcome,
    ) -> anyhow::Result<()> {
        let failure = match &outcome {
            crate::tasks::TaskOutcome::Failed(_) => Some(pa_types::event_log::TaskFailure::Turn),
            crate::tasks::TaskOutcome::Done | crate::tasks::TaskOutcome::Cancelled => None,
        };
        let cause = Cause {
            id: task.trigger.clone(),
            trust: task.trust,
            hops: 0,
        };
        self.outbox.send(done_event(
            &SessionTopics::of(&task.session)?,
            task,
            &host::TaskEnd { outcome, failure },
            &cause,
        ));
        Ok(())
    }

    pub(crate) fn host(&self, id: &str) -> Option<Arc<SessionHost>> {
        lock(&self.hosts).get(id).cloned()
    }

    pub(crate) fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// The sessions' family tree.
    pub(crate) fn subagents(&self) -> &Arc<Subagents> {
        &self.subagents
    }

    /// The runtime's event bus, which its sessions' `event.*` reach.
    pub(crate) fn events(&self) -> &Arc<BusEvents> {
        &self.events
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
