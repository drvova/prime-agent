//! A session's host: its own OS thread and multi-threaded Tokio runtime,
//! entered with the session's scope, owning the session's engine and the
//! turn driver around it (the turn boundary, the goal surface, the
//! autonomous run), and running its work one item at a time as it arrives:
//! a task's turn, a settle (the run driven to rest and its end reported) or
//! a finish (the refinements drained and the subagents ended).
//!
//! A turn that panics fails its task with the panic's text. The panic is
//! caught at the thread's work loop, so the engine it left behind is never
//! used again: that lifetime ends (its scope is ended, so nothing of it
//! writes again, and its runtime is shut down) and the next task runs on a
//! fresh engine the builder makes in a new lifetime. Other sessions never
//! see it.

use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_agent::agent::Agent;
use pa_core::session_engine::goal_continuation::{ContinuationTrigger, GoalContinuationDecision};
use pa_core::session_engine::session_events::agent_event_json;
use pa_types::ai::ImageContent;
use pa_types::event_log::{EventId, NewEvent, Task, TaskFailure, Trust};
use pa_types::session_scope::{self, SessionScope};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{
    done_event, BuiltSession, DriverEvents, Finished, Outbox, SessionBuilder, SessionSpec,
    SessionTopics, SettleAfter, Settled,
};
use crate::lock;
use crate::tasks::{Steered, TaskOutcome};
use crate::turn::autonomous::{latest_assistant_error, HeadlessAutonomous};
use crate::turn::boundary::TurnBoundary;
use crate::turn::continuation::{wire_continuation_hook, UnsettledWork};
use crate::turn::goal::PrintGoalSurface;

/// How long a lifetime's runtime may take to drop its tasks once the
/// lifetime has ended; threads still blocked after that are abandoned.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// What the host runs, and where each answer goes.
enum Work {
    /// A task's turn.
    Task {
        task: Task,
        images: Vec<pa_agent::types::ImageContent>,
        outcome: oneshot::Sender<Ran>,
    },
    /// The run driven to rest, and how it ended.
    Settle(SettleAnswer),
    /// The refinements drained and the subagents ended.
    Finish(oneshot::Sender<Result<Finished, String>>),
}

/// Where a settle's answer goes: how the run ended, or `None` when the task
/// it was to follow stopped the run.
type SettleAnswer = oneshot::Sender<Result<Option<Settled>, String>>;

/// A task as the host's caller sees it end: whether its `done` is queued.
pub(crate) enum Ran {
    /// It ran on the session's thread, which queued its `done`.
    Reported(TaskEnd),
    /// It never ran there: its caller reports it.
    Unreported(TaskEnd),
}

/// How a task ended, and which part of a failed one failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskEnd {
    pub(crate) outcome: TaskOutcome,
    pub(crate) failure: Option<TaskFailure>,
}

impl TaskEnd {
    const DONE: TaskEnd = TaskEnd {
        outcome: TaskOutcome::Done,
        failure: None,
    };
    const CANCELLED: TaskEnd = TaskEnd {
        outcome: TaskOutcome::Cancelled,
        failure: None,
    };

    pub(crate) fn failed(failure: TaskFailure, reason: String) -> Self {
        TaskEnd {
            outcome: TaskOutcome::Failed(reason),
            failure: Some(failure),
        }
    }
}

/// One session's host thread, as the runtime holds it.
pub(crate) struct SessionHost {
    work: Mutex<Option<mpsc::UnboundedSender<Work>>>,
    control: Arc<TurnControl>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// What the runtime reaches a running turn through, from outside the
/// session's thread: steering and cancellation never wait for the turn.
#[derive(Default)]
struct TurnControl {
    turn: Mutex<Turn>,
}

#[derive(Default)]
struct Turn {
    /// The engine's agent while a lifetime has one.
    agent: Option<Arc<Agent>>,
    /// The task running, if any.
    task: Option<String>,
    /// Whether the running task's turn still reads steering: from its start
    /// until its run has settled.
    steerable: bool,
    /// Whether the running task was cancelled.
    cancelled: bool,
    /// Whether the running task's turn took a steer.
    steered: bool,
    /// The settle to run right after the task an input event queued, by that
    /// event's id ([`SettleAfter::Task`]).
    settle_after: Option<(EventId, SettleAnswer)>,
    /// The input event of the task the host finished last, and whether a
    /// settle may follow it.
    finished: Option<(EventId, bool)>,
    /// What the running task's events may carry: its trust.
    trust: Option<Trust>,
}

impl SessionHost {
    /// Starts session `id`'s host thread; it builds the engine at once.
    pub(crate) fn start<B: SessionBuilder>(
        id: String,
        spec: SessionSpec,
        topics: SessionTopics,
        builder: Arc<B>,
        outbox: Outbox,
    ) -> std::io::Result<Self> {
        let (work, queued) = mpsc::unbounded_channel();
        let control = Arc::new(TurnControl::default());
        let thread = std::thread::Builder::new()
            .name(format!(
                "session-{}",
                id.chars().take(8).collect::<String>()
            ))
            .spawn({
                let control = Arc::clone(&control);
                move || {
                    serve(
                        &Lifetime {
                            id,
                            spec,
                            topics,
                            builder,
                            outbox,
                            control,
                        },
                        queued,
                    );
                }
            })?;
        Ok(SessionHost {
            work: Mutex::new(Some(work)),
            control,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Runs `task`, with the images its prompt carries, on the session's
    /// thread.
    pub(crate) async fn run(&self, task: Task, images: Vec<ImageContent>) -> Ran {
        let (outcome, answer) = oneshot::channel();
        let images = images
            .into_iter()
            .map(|image| pa_agent::types::ImageContent {
                data: image.data,
                mime_type: image.mime_type,
            })
            .collect();
        if !self.send(Work::Task {
            task,
            images,
            outcome,
        }) {
            return Ran::Unreported(TaskEnd::failed(
                TaskFailure::Turn,
                "the session is closed".to_string(),
            ));
        }
        answer.await.unwrap_or_else(|_| {
            Ran::Unreported(TaskEnd::failed(
                TaskFailure::Turn,
                "the session's host ended before the task did".to_string(),
            ))
        })
    }

    /// Settles the session's run on its thread, `after` the work it names.
    /// The settle is queued (or armed behind its task) at once, before the
    /// answer is awaited.
    pub(crate) fn settle(
        &self,
        after: SettleAfter,
    ) -> impl std::future::Future<Output = Result<Option<Settled>, String>> + Send + 'static {
        let (settled, answer) = oneshot::channel();
        let queued = match after {
            SettleAfter::Queued => self.send(Work::Settle(settled)),
            SettleAfter::Task(trigger) => {
                let mut turn = lock(&self.control.turn);
                match &turn.finished {
                    // Its task has ended already: the settle runs next, if it
                    // may follow the task at all.
                    Some((finished, settles)) if *finished == trigger => {
                        let settles = *settles;
                        drop(turn);
                        if settles {
                            self.send(Work::Settle(settled))
                        } else {
                            let _ = settled.send(Ok(None));
                            true
                        }
                    }
                    _ => {
                        turn.settle_after = Some((trigger, settled));
                        true
                    }
                }
            }
        };
        async move {
            if !queued {
                return Err("the session is closed".to_string());
            }
            answer.await.unwrap_or_else(|_| {
                Err("the session's host ended before the settle did".to_string())
            })
        }
    }

    /// Finishes the session's run on its thread, after the work before it.
    pub(crate) async fn finish(&self) -> Result<Finished, String> {
        let (finished, answer) = oneshot::channel();
        if !self.send(Work::Finish(finished)) {
            return Err("the session is closed".to_string());
        }
        answer
            .await
            .unwrap_or_else(|_| Err("the session's host ended before the finish did".to_string()))
    }

    /// Queues `work` for the session's thread; whether it is still open.
    fn send(&self, work: Work) -> bool {
        lock(&self.work)
            .as_ref()
            .is_some_and(|sender| sender.send(work).is_ok())
    }

    /// Hands `steer`'s prompt to `running`'s turn while it still reads
    /// steering.
    pub(crate) fn steer(&self, running: &Task, steer: &Task) -> Steered {
        let mut turn = lock(&self.control.turn);
        match (&turn.agent, &turn.task) {
            (Some(agent), Some(task)) if turn.steerable && *task == running.id => {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| {
                        i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
                    });
                agent.steer(pa_agent::types::AgentMessage::Standard(
                    pa_agent::types::Message::User(pa_agent::types::UserMessage {
                        content: pa_agent::types::UserContent::Text(steer.prompt.clone()),
                        timestamp,
                    }),
                ));
                turn.steered = true;
                Steered::Taken
            }
            _ => Steered::Declined,
        }
    }

    /// Cancels the running task: its turn is aborted and it ends
    /// `Cancelled`. Whether a task was running.
    pub(crate) fn cancel_running(&self) -> bool {
        let mut turn = lock(&self.control.turn);
        if turn.task.is_none() {
            return false;
        }
        turn.cancelled = true;
        if let Some(agent) = &turn.agent {
            agent.abort();
        }
        true
    }

    /// Closes the session: its thread finishes the task it runs, drops the
    /// tasks sent after it, and ends; waits until it has.
    pub(crate) async fn close(&self) {
        lock(&self.work).take();
        let thread = lock(&self.thread).take();
        if let Some(thread) = thread {
            // A host thread that panicked outside a turn has already ended;
            // there is nothing more to wait for.
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

/// Everything one session's lifetimes are built from.
struct Lifetime<B> {
    id: String,
    spec: SessionSpec,
    topics: SessionTopics,
    builder: Arc<B>,
    outbox: Outbox,
    control: Arc<TurnControl>,
}

/// A panic in a lifetime's work: the lifetime ends.
struct Panicked;

/// How a lifetime ended.
enum Ended {
    /// The session was closed.
    Closed,
    /// A turn panicked; the session goes on in a new lifetime.
    Crashed,
}

/// The host thread: one lifetime after another until the session closes.
fn serve<B: SessionBuilder>(lifetime: &Lifetime<B>, mut queued: mpsc::UnboundedReceiver<Work>) {
    loop {
        let scope = Arc::new(SessionScope::new(
            lifetime.id.clone(),
            lifetime.spec.cwd.clone(),
            lifetime.spec.env.clone(),
            BTreeSet::new(),
        ));
        let _entered = session_scope::enter(Some(Arc::clone(&scope)));
        // Multi-threaded, as the daemon's session host is: what the session
        // spawns (a background refinement, its subagents' drivers) runs
        // beside its turn, not only when the turn yields.
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name(format!(
                "session-{}-rt",
                lifetime.id.chars().take(8).collect::<String>()
            ))
            .on_thread_start(session_scope::inherit())
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let reason = format!("build the session's runtime: {error}");
                while let Ok(work) = queued.try_recv() {
                    work.fail(&reason);
                }
                scope.end();
                return;
            }
        };
        let ended = lifetime.run(&runtime, &mut queued);
        lock(&lifetime.control.turn).agent = None;
        // The lifetime is cut off from the session's durable state before its
        // runtime goes, so nothing it leaves running writes again.
        scope.end();
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
        match ended {
            Ended::Closed => return,
            Ended::Crashed => {}
        }
    }
}

impl Work {
    /// Answers the work with `reason` as its failure, without running it.
    fn fail(self, reason: &str) {
        match self {
            Work::Task { outcome, .. } => {
                let _ = outcome.send(Ran::Unreported(TaskEnd::failed(
                    TaskFailure::Turn,
                    reason.to_string(),
                )));
            }
            Work::Settle(settled) => {
                let _ = settled.send(Err(reason.to_string()));
            }
            Work::Finish(finished) => {
                let _ = finished.send(Err(reason.to_string()));
            }
        }
    }
}

/// A lifetime's engine and the turn driver around it: the turn boundary,
/// the goal surface and the autonomous run, each wired onto the engine once.
struct Hosted {
    built: BuiltSession,
    boundary: TurnBoundary,
    goal: Arc<PrintGoalSurface>,
    autonomous: Arc<HeadlessAutonomous>,
    /// The goal's `updated_at` when the lifetime began: a goal capped after
    /// it was capped by this run.
    goal_updated_at_start: Option<u64>,
    global_harness_dir: PathBuf,
}

impl<B: SessionBuilder> Lifetime<B> {
    /// Serves work on `runtime` until the session closes or a turn panics.
    fn run(
        &self,
        runtime: &tokio::runtime::Runtime,
        queued: &mut mpsc::UnboundedReceiver<Work>,
    ) -> Ended {
        let mut hosted = self.build(runtime);
        loop {
            let Some(work) = runtime.block_on(queued.recv()) else {
                // Dropped on its runtime: what the engine spawns as it goes
                // down still has one to run on.
                runtime.block_on(async move { drop(hosted) });
                return Ended::Closed;
            };
            if hosted.is_err() {
                hosted = self.build(runtime);
            }
            let session = match &mut hosted {
                Ok(session) => session,
                Err(error) => {
                    work.fail(error);
                    continue;
                }
            };
            if self.answer(runtime, session, work).is_err() {
                // The engine the panic left is not used again; dropping it
                // may panic too, and that is this lifetime's end as well.
                let _ = session_scope::catching_panics(AssertUnwindSafe(|| {
                    runtime.block_on(async move { drop(hosted) });
                }));
                return Ended::Crashed;
            }
        }
    }

    /// Runs one item of work on `session` and answers it. A panic in it is
    /// answered as its failure and returned: the lifetime ends.
    fn answer(
        &self,
        runtime: &tokio::runtime::Runtime,
        session: &mut Hosted,
        work: Work,
    ) -> Result<(), Panicked> {
        match work {
            Work::Task {
                task,
                images,
                outcome,
            } => {
                {
                    let mut turn = lock(&self.control.turn);
                    turn.task = Some(task.id.clone());
                    turn.steerable = true;
                    turn.cancelled = false;
                    turn.steered = false;
                    turn.trust = Some(task.trust);
                }
                let ran = session_scope::catching_panics(AssertUnwindSafe(|| {
                    runtime.block_on(self.run_task(session, &task, images))
                }));
                let cancelled = {
                    let mut turn = lock(&self.control.turn);
                    turn.task = None;
                    turn.steerable = false;
                    turn.trust = None;
                    std::mem::take(&mut turn.cancelled)
                };
                let (end, panicked) = match ran {
                    Ok(_) if cancelled => (TaskEnd::CANCELLED, Ok(())),
                    Ok(end) => (end, Ok(())),
                    Err(panic) => (
                        TaskEnd::failed(
                            TaskFailure::Turn,
                            format!("the turn panicked: {}", panic_text(&*panic)),
                        ),
                        Err(Panicked),
                    ),
                };
                // A settle may follow a task that ended done or with a failed
                // reply; one armed behind this task runs now, with nothing of
                // the session's between them.
                let settles = panicked.is_ok()
                    && (end.outcome == TaskOutcome::Done
                        || end.failure == Some(TaskFailure::Reply));
                let armed = {
                    let mut turn = lock(&self.control.turn);
                    turn.finished = Some((task.trigger.clone(), settles));
                    match turn.settle_after.take() {
                        Some((trigger, settled)) if trigger == task.trigger => Some(settled),
                        other => {
                            turn.settle_after = other;
                            None
                        }
                    }
                };
                // The `done` goes behind the turn's stream and ahead of
                // anything a settle streams.
                self.outbox.send(done_event(&self.topics, &task, &end));
                let _ = outcome.send(Ran::Reported(end));
                match armed {
                    Some(settled) if settles => {
                        self.answer(runtime, session, Work::Settle(settled))
                    }
                    Some(settled) => {
                        let _ = settled.send(Ok(None));
                        panicked
                    }
                    None => panicked,
                }
            }
            Work::Settle(settled) => {
                let answer = session_scope::catching_panics(AssertUnwindSafe(|| {
                    runtime.block_on(Self::settle(session))
                }));
                let (answer, panicked) = match answer {
                    Ok(answer) => (answer.map(Some), Ok(())),
                    Err(panic) => (
                        Err(format!("the settle panicked: {}", panic_text(&*panic))),
                        Err(Panicked),
                    ),
                };
                // Behind the settle's events: its caller has them all first.
                self.outbox.then(move || {
                    let _ = settled.send(answer);
                });
                panicked
            }
            Work::Finish(finished) => {
                let answer = session_scope::catching_panics(AssertUnwindSafe(|| {
                    runtime.block_on(Self::finish(session))
                }));
                let (answer, panicked) = match answer {
                    Ok(answer) => (Ok(answer), Ok(())),
                    Err(panic) => (
                        Err(format!("the finish panicked: {}", panic_text(&*panic))),
                        Err(Panicked),
                    ),
                };
                self.outbox.then(move || {
                    let _ = finished.send(answer);
                });
                panicked
            }
        }
    }

    /// Builds the lifetime's engine, wires its events onto the bus, and
    /// wires the turn driver onto it: the goal usage accounting, the
    /// autonomous accounting and the natural-turn-end continuation hook, in
    /// that order after the stream.
    fn build(&self, runtime: &tokio::runtime::Runtime) -> Result<Hosted, String> {
        let built = session_scope::catching_panics(AssertUnwindSafe(|| {
            runtime.block_on(self.builder.build(&self.id, &self.spec))
        }));
        let built = match built {
            Ok(Ok(built)) => built,
            Ok(Err(error)) => return Err(format!("build the session: {error:#}")),
            Err(_) => return Err("building the session panicked".to_string()),
        };
        let agent = Arc::clone(built.engine.session.agent());
        let json = built.events == DriverEvents::Stream;
        let hosted = runtime.block_on(async {
            // The stream's listener first: on every agent event the stream
            // has it before the goal's accounting reports what it changed.
            agent
                .subscribe({
                    let publisher = self.publisher();
                    move |event, _signal| {
                        if let Some(payload) = agent_event_json(&event) {
                            publisher(&payload);
                        }
                        Box::pin(std::future::ready(Ok(())))
                    }
                })
                .await;
            let engine = &built.engine;
            let goal = Arc::new(PrintGoalSurface::with_sink(json, self.publisher()));
            goal.seed_publish_baseline(engine).await;
            let goal_updated_at_start = engine.goal_state().await.updated_at;
            // The accounting listeners stay for the engine's life: dropping
            // a subscription handle keeps its listener.
            let _ = goal.wire_accounting(engine, &agent).await;
            let autonomous = Arc::new(match &built.autonomous {
                Some(config) => HeadlessAutonomous::with_config(config, &self.spec.cwd),
                None => HeadlessAutonomous::disabled(&self.spec.cwd),
            });
            let _ = autonomous.wire_accounting(&agent).await;
            // Both continuation arms defer while subagent work is unsettled;
            // the hook holds the subagents weakly, as the engine's own agent
            // keeps the hook.
            let unsettled = built.subagents.as_ref().map(|subagents| {
                let subagents = Arc::downgrade(subagents);
                Arc::new(move || match subagents.upgrade() {
                    Some(subagents) => subagents.unsettled(),
                    None => Box::pin(std::future::ready(false)),
                }) as UnsettledWork
            });
            wire_continuation_hook(engine, &agent, &built.model, &goal, &autonomous, unsettled);
            let global_harness_dir =
                pa_core::refinement::get_global_harness_state_dir(&built.agent_dir);
            Hosted {
                boundary: TurnBoundary::with_sink(json, self.publisher()),
                goal,
                autonomous,
                goal_updated_at_start,
                global_harness_dir,
                built,
            }
        });
        lock(&self.control.turn).agent = Some(agent);
        Ok(hosted)
    }

    /// Publishes one json payload on the session's stream, with the running
    /// task's trust.
    fn publisher(&self) -> Arc<dyn Fn(&Value) + Send + Sync> {
        let outbox = self.outbox.clone();
        let control = Arc::clone(&self.control);
        let topic = self.topics.stream.clone();
        let id = self.id.clone();
        Arc::new(move |payload: &Value| {
            let trust = lock(&control.turn).trust.unwrap_or(Trust::Owner);
            outbox.send(NewEvent {
                topic: topic.clone(),
                source: id.clone(),
                session: Some(id.clone()),
                payload: payload.clone(),
                trust,
                cause: None,
                reply: None,
            });
        })
    }

    /// One task. A session command runs at the boundary instead of a turn
    /// ([`Lifetime::run_session_command`]). A prompt runs the pre-turn boundary,
    /// the prompt with its retries, a steer the run took but did not read,
    /// the settled-turn boundary, then the goal's queued turns and, when the
    /// goal does not own the wake-up, the autonomous run's held ones.
    async fn run_task(
        &self,
        session: &mut Hosted,
        task: &Task,
        images: Vec<pa_agent::types::ImageContent>,
    ) -> TaskEnd {
        let command = session
            .built
            .engine
            .session
            .classify_session_command(&task.prompt);
        if let Some(command) = command {
            return Self::run_session_command(session, &command, &images).await;
        }
        let Hosted {
            built,
            boundary,
            goal,
            autonomous,
            global_harness_dir,
            ..
        } = session;
        let engine = &built.engine;
        let turn_failed = |reason: String| TaskEnd::failed(TaskFailure::Turn, reason);
        if let Err(error) = boundary
            .run_pre_turn(engine, &built.model, built.api_key.clone())
            .await
        {
            return turn_failed(error);
        }
        // Cancelled before its run started: there is no run to abort.
        if lock(&self.control.turn).cancelled {
            return TaskEnd::CANCELLED;
        }
        let retry_policy =
            pa_core::settings::SettingsManager::create(&self.spec.cwd, &built.agent_dir)
                .get_provider_retry_policy();
        let publish = self.publisher();
        let prompted = engine
            .session
            .prompt_with_auto_retry(
                &task.prompt,
                images,
                &retry_policy,
                built.model.context_window,
                |event| {
                    publish(&crate::turn::auto_retry::auto_retry_event_json(event));
                    std::future::ready(Ok(()))
                },
            )
            .await;
        let agent = engine.session.agent();
        let (cancelled, steered) = {
            let mut turn = lock(&self.control.turn);
            turn.steerable = false;
            (turn.cancelled, turn.steered)
        };
        if cancelled {
            return TaskEnd::CANCELLED;
        }
        if let Err(error) = prompted {
            return turn_failed(format!("{error:#}"));
        }
        // A steer taken as the run settled was never read: it runs now, as
        // part of this task. Rows other work queued on the agent (a
        // subagent's reply) wait for the settle's quiescence drive.
        if steered && agent.has_queued_messages() {
            if let Err(error) = agent.continue_run().await {
                return turn_failed(format!("{error:#}"));
            }
            agent.wait_for_idle().await;
        }
        let settled = async {
            let (model, api_key) = (&built.model, &built.api_key);
            boundary
                .run_at_settled_turn(engine, model, api_key.clone(), global_harness_dir.clone())
                .await?;
            goal.drive_boundary(
                engine,
                boundary,
                model,
                api_key.clone(),
                global_harness_dir.clone(),
            )
            .await?;
            // The goal arm takes exclusive priority: the autonomous arm
            // drains its held continuations only when the goal does not own
            // the wake-up.
            if !engine.goal_owns_continuation_wakeup().await {
                autonomous
                    .drive_boundary(
                        engine,
                        boundary,
                        model,
                        api_key.clone(),
                        global_harness_dir.clone(),
                    )
                    .await?;
            }
            Ok::<_, String>(())
        };
        if let Err(error) = settled.await {
            return turn_failed(error);
        }
        match latest_assistant_error(engine).await {
            Some(error) => TaskEnd::failed(
                TaskFailure::Reply,
                error.unwrap_or_else(|| "the model request failed".to_string()),
            ),
            None => TaskEnd::DONE,
        }
    }

    /// A session command's task (`/compact`, `/refine`, `/goal`,
    /// `/autonomous`, `/better`): it runs at the boundary instead of a
    /// turn, then the continuation it schedules and the goal's queue drain.
    async fn run_session_command(
        session: &mut Hosted,
        command: &pa_core::session_engine::slash_commands::SessionSlashCommand,
        images: &[pa_agent::types::ImageContent],
    ) -> TaskEnd {
        let Hosted {
            built,
            boundary,
            goal,
            autonomous,
            global_harness_dir,
            ..
        } = session;
        let engine = &built.engine;
        let turn_failed = |reason: String| TaskEnd::failed(TaskFailure::Turn, reason);
        let execution = crate::turn::session_command::execute_prompt_session_command(
            engine,
            goal,
            &built.model,
            built.api_key.clone(),
            global_harness_dir.clone(),
            &autonomous.state_handle(),
            boundary,
            command,
            images,
        )
        .await;
        if let Some(error) = execution.error {
            return TaskEnd::failed(TaskFailure::Command, error);
        }
        // A `/goal` start scheduled its continuation: it runs as part of
        // this task.
        if let Some(continuation) = execution.continuation_message {
            if let Err(error) = goal
                .run_session_command_continuation(
                    engine,
                    boundary,
                    &built.model,
                    built.api_key.clone(),
                    global_harness_dir.clone(),
                    &continuation,
                )
                .await
            {
                return turn_failed(error);
            }
        }
        // The queue drain a settled turn gets: held continuations and
        // armed steers run as this task's follow-up turns.
        match goal
            .drive_boundary(
                engine,
                boundary,
                &built.model,
                built.api_key.clone(),
                global_harness_dir.clone(),
            )
            .await
        {
            Ok(_) => TaskEnd::DONE,
            Err(error) => turn_failed(error),
        }
    }

    /// Drives the run to rest, then reports how it ended. Every admitted
    /// subagent settles and its replies and notices run as turns first; then
    /// one loop keeps the run going while deferred work exists: the goal's
    /// owed continuation, the autonomous continuation held behind the
    /// subagents, and the boundary drains whatever turns those armed (a
    /// threshold continuation, a budget steer, more subagents).
    async fn settle(session: &mut Hosted) -> Result<Settled, String> {
        Self::drive_to_rest(session).await?;
        let engine = &session.built.engine;
        let messages: Vec<pa_types::session::AgentMessage> = engine
            .session
            .agent()
            .state()
            .await
            .messages
            .iter()
            .filter_map(pa_core::session_engine::provider_adapter::json_round_trip)
            .collect();
        let goal_state = engine.goal_state().await;
        let cap_reason = pa_core::session_engine::goal_driver::CONTINUATION_NO_PROGRESS_CAP_REASON;
        let goal_stopped = (goal_state.status == pa_types::goal::GoalStatus::Error
            && goal_state.last_error.as_deref() == Some(cap_reason)
            && goal_state.updated_at != session.goal_updated_at_start)
            .then(|| cap_reason.to_string());
        Ok(Settled {
            terminal: pa_core::session_engine::headless::select_headless_terminal_result(&messages),
            goal_stopped,
            autonomous_failure: session.autonomous.exit_stderr().await,
        })
    }

    /// The settle's drive: every admitted subagent settles and its replies
    /// and notices run as turns, then the deferred continuations deliver and
    /// the boundary drains what they armed, until nothing more runs.
    async fn drive_to_rest(session: &mut Hosted) -> Result<(), String> {
        let Hosted {
            built,
            boundary,
            goal,
            autonomous,
            global_harness_dir,
            ..
        } = session;
        let engine = &built.engine;
        let model = &built.model;
        loop {
            if let Some(subagents) = &built.subagents {
                if subagents
                    .drive_to_quiescence(
                        engine,
                        boundary,
                        model,
                        built.api_key.clone(),
                        global_harness_dir.clone(),
                    )
                    .await?
                {
                    continue;
                }
            }
            // The subagents are at rest: only an owed continuation delivers.
            match engine
                .consult_goal_continuation(ContinuationTrigger::OwedRetry, false)
                .await
            {
                GoalContinuationDecision::Minted {
                    message: continuation,
                    ..
                } => {
                    goal.publish_goal_update(engine).await;
                    goal.run_queued_turn(
                        engine,
                        boundary,
                        model,
                        built.api_key.clone(),
                        global_harness_dir.clone(),
                        &continuation,
                    )
                    .await?;
                    continue;
                }
                // A refused or failed mint can still move the goal (a failed
                // persist fails it).
                GoalContinuationDecision::NotMinted { .. } => {
                    goal.publish_goal_update(engine).await;
                }
                GoalContinuationDecision::NotOwned
                | GoalContinuationDecision::NothingOwed
                | GoalContinuationDecision::Owed => {}
            }
            if autonomous
                .deliver_rlm_continuation(
                    engine,
                    boundary,
                    model,
                    built.api_key.clone(),
                    global_harness_dir.clone(),
                )
                .await?
            {
                continue;
            }
            // The turns a drain runs may start subagents or arm more holds:
            // the loop goes around, and its quiescence pass waits for what
            // they left.
            let mut boundary_ran = goal
                .drive_boundary(
                    engine,
                    boundary,
                    model,
                    built.api_key.clone(),
                    global_harness_dir.clone(),
                )
                .await?;
            if !engine.goal_owns_continuation_wakeup().await {
                boundary_ran |= autonomous
                    .drive_boundary(
                        engine,
                        boundary,
                        model,
                        built.api_key.clone(),
                        global_harness_dir.clone(),
                    )
                    .await?;
            }
            if !boundary_ran {
                break;
            }
        }
        Ok(())
    }

    /// Finishes the run: the disposal drain (the refinements its turns
    /// started), then the subagents' end.
    async fn finish(session: &mut Hosted) -> Finished {
        let Hosted {
            built,
            boundary,
            global_harness_dir,
            ..
        } = session;
        boundary
            .drain_compact_auto_refine_at_disposal(
                &built.engine,
                &built.model,
                built.api_key.clone(),
                global_harness_dir.clone(),
            )
            .await;
        if let Some(subagents) = &built.subagents {
            subagents.dispose().await;
        }
        Finished {
            refine_command_failed: boundary.refine_command_failed(),
        }
    }
}

/// A panic payload's text.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with a non-text payload".to_string())
}
