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
//! see it. Under the runtime daemon's supervision a panic on any of the
//! session's threads, or a stuck cell, ends the lifetime the same way, and
//! the crash is recorded and healed ([`super::faults`]).
//!
//! A session replaced by another transcript ([`SessionHost::replace`]: the
//! runtime daemon's `fork` onto an entry and `import_jsonl`) goes the same
//! way without a crash: once the work before it ends, the lifetime ends and
//! the next one builds its engine (and a fresh kernel) from the new
//! transcript in the new working directory. The session keeps its id, its
//! topics and their subscribers, its stream and its tasks.

use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pa_agent::agent::Agent;
use pa_core::session::crash::{
    restart_backoff_ms, SessionCrash, MAX_CONSECUTIVE_FAILURES, STABLE_LIFETIME_MS,
};
use pa_core::session_engine::goal_continuation::{ContinuationTrigger, GoalContinuationDecision};
use pa_types::ai::ImageContent;
use pa_types::daemon::SessionFaultEvent;
use pa_types::event_log::{Cause, EventId, Task, TaskFailure};
use pa_types::session_scope::{self, SessionScope};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use pa_core::agent_state::EventRequester;
use pa_core::session_engine::agent_messaging::{
    register_agent_message_host_handlers, register_agent_observe_host_handlers,
};
use pa_core::session_engine::event_bus_host::register_event_bus_host_handlers;

use super::checkpoints::Checkpoints;
use super::faults::{
    keep_fault, orphan_journal, publish_fault, reap_orphans, record_crash, LifetimeFaults, Rebuild,
    SessionFaults,
};
use super::{
    done_event, BuiltSession, DriverEvents, Finished, Outbox, SessionBuilder, SessionSeams,
    SessionSpec, SessionTopics, SettleAfter, Settled,
};
use crate::events::{BusEvents, SessionEvents};
use crate::lock;
use crate::subagents::{
    dispose, drive_to_quiescence, has_unsettled_rlm_work, kernel_notice_sink,
    late_sent_agent_message_sink, FamilyMessages, FamilyObserve, Node, Rest, SessionSubagents,
};
use crate::tasks::{Steered, TaskOutcome};
use crate::trust::{RunningTask, TaskTrust, TrustGate, WorkingDir};
use crate::turn::autonomous::{latest_assistant_error, HeadlessAutonomous};
use crate::turn::boundary::TurnBoundary;
use crate::turn::continuation::{wire_continuation_hook, UnsettledWork};
use crate::turn::goal::PrintGoalSurface;

/// How long a lifetime's runtime may take to drop its tasks once the
/// lifetime has ended; threads still blocked after that are abandoned.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// How a session's host starts.
pub(crate) enum Opening {
    /// Its first lifetime builds its engine.
    Fresh,
    /// The session is revived after a crash (its daemon's death): its first
    /// lifetime is the rebuild.
    Revived(Rebuild),
}

/// What the host runs, and where each answer goes.
enum Work {
    /// A task's turn.
    Task {
        task: Task,
        images: Vec<pa_agent::types::ImageContent>,
        /// Its trigger: the cause of what its turn publishes.
        cause: Cause,
        outcome: oneshot::Sender<Ran>,
    },
    /// The run driven to rest, and how it ended.
    Settle(SettleAnswer),
    /// The refinements drained and the subagents ended.
    Finish(oneshot::Sender<Result<Finished, String>>),
}

/// What the session's thread is sent: work for its lifetime, or the end of
/// the lifetime in a replacement.
enum Request {
    Work(Work),
    /// The session goes on from another transcript in a new lifetime.
    Replace(Replacement),
}

impl From<Work> for Request {
    fn from(work: Work) -> Self {
        Request::Work(work)
    }
}

impl Request {
    /// Answers the request with `reason` as its failure, without running it.
    fn fail(self, reason: &str) {
        match self {
            Request::Work(work) => work.fail(reason),
            Request::Replace(replacement) => {
                let _ = replacement.built.send(Err(reason.to_string()));
            }
        }
    }
}

/// The transcript and working directory a replaced session goes on from,
/// and where the answer goes once its new engine is built (or failed to
/// be).
pub(crate) struct Replacement {
    pub(crate) transcript: PathBuf,
    pub(crate) cwd: PathBuf,
    built: oneshot::Sender<Result<(), String>>,
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
    pub(crate) const CANCELLED: TaskEnd = TaskEnd {
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

/// A lifetime's engine, and a handle on the runtime it runs on (whose
/// threads carry the lifetime's scope).
#[derive(Clone)]
pub(crate) struct LiveEngine {
    pub(crate) engine: Arc<pa_core::session_engine::engine::SessionEngine>,
    pub(crate) runtime: tokio::runtime::Handle,
    /// The lifetime's autonomous run state, as its turns account it.
    pub(crate) autonomous: Arc<tokio::sync::Mutex<pa_core::autonomous::AutonomousRuntimeState>>,
    /// The slot its engine's stream reads its provider target from
    /// ([`crate::BuiltSession::provider_target`]).
    pub(crate) provider_target: pa_core::session_engine::provider_adapter::ProviderTargetSlot,
    /// The lifetime's workspace checkpoints ([`Checkpoints`]); `None` for a
    /// subagent or a session kept in memory.
    pub(crate) checkpoints: Option<Arc<Checkpoints>>,
}

/// One session's host thread, as the runtime holds it.
pub(crate) struct SessionHost {
    work: Mutex<Option<mpsc::UnboundedSender<Request>>>,
    control: Arc<TurnControl>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// What the runtime reaches a running turn through, from outside the
/// session's thread: steering and cancellation never wait for the turn.
struct TurnControl {
    turn: Mutex<Turn>,
    /// The running task's trust: what its events carry and what its
    /// kernel's trust gate admits.
    trust: TaskTrust,
}

#[derive(Default)]
struct Turn {
    /// The engine's agent while a lifetime has one.
    agent: Option<Arc<Agent>>,
    /// The lifetime's engine and the runtime it runs on, while it has one:
    /// what a daemon call that changes the open session reaches.
    engine: Option<LiveEngine>,
    /// The task running, if any.
    task: Option<String>,
    /// Whether the running task's turn still reads steering: from its start
    /// until its run has settled.
    steerable: bool,
    /// Whether the running task was cancelled.
    cancelled: bool,
    /// A task the lane runs that was cancelled before it reached this
    /// thread: it starts cancelled and runs nothing.
    cancelled_ahead: Option<String>,
    /// Whether the running task's turn took a steer.
    steered: bool,
    /// The steers the running task's turn took, each with the message it
    /// handed the agent: the ones still unread when the turn is cancelled
    /// go back to the lane ([`SessionHost::cancel_running`]).
    taken: Vec<(Task, pa_agent::types::AgentMessage)>,
    /// Steers offered to a task the queue is about to run, before it reached
    /// this thread, by that task's id: its turn takes them as it starts.
    pending: Vec<(String, Task, pa_agent::types::AgentMessage)>,
    /// The task that ended here last: a steer offered to it is too late.
    ended: Option<String>,
    /// The settle to run right after the task an input event queued, by that
    /// event's id ([`SettleAfter::Task`]).
    settle_after: Option<(EventId, SettleAnswer)>,
    /// The input event of the task the host finished last, and whether a
    /// settle may follow it.
    finished: Option<(EventId, bool)>,
    /// Why the session takes no more work, once its supervision gave it up.
    given_up: Option<String>,
}

impl SessionHost {
    /// Starts session `id`'s host thread; it builds the engine at once.
    #[expect(
        clippy::too_many_arguments,
        reason = "each is a distinct part of the session the host thread owns"
    )]
    pub(crate) fn start<B: SessionBuilder>(
        id: String,
        spec: SessionSpec,
        node: Arc<Node>,
        topics: SessionTopics,
        builder: Arc<B>,
        outbox: Outbox,
        events: Arc<BusEvents>,
        faults: SessionFaults,
        jobs: super::JobsSlot,
        opening: Opening,
    ) -> std::io::Result<Self> {
        let (work, queued) = mpsc::unbounded_channel();
        let control = Arc::new(TurnControl {
            turn: Mutex::new(Turn::default()),
            trust: node.trust.clone(),
        });
        let thread = std::thread::Builder::new()
            .name(format!(
                "session-{}",
                id.chars().take(8).collect::<String>()
            ))
            .spawn({
                let control = Arc::clone(&control);
                move || {
                    serve(
                        &mut Lifetime {
                            id,
                            spec,
                            node,
                            topics,
                            builder,
                            outbox,
                            control,
                            events,
                            faults,
                            jobs,
                        },
                        queued,
                        opening,
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
    /// thread; what its turn publishes is caused by `cause`, its trigger.
    pub(crate) async fn run(&self, task: Task, images: Vec<ImageContent>, cause: Cause) -> Ran {
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
            cause,
            outcome,
        }) {
            return Ran::Unreported(TaskEnd::failed(TaskFailure::Turn, self.closed_reason()));
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
        let closed = self.closed_reason();
        async move {
            if !queued {
                return Err(closed);
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
            return Err(self.closed_reason());
        }
        answer
            .await
            .unwrap_or_else(|_| Err("the session's host ended before the finish did".to_string()))
    }

    /// Ends the session's lifetime once the work before it has, and goes on
    /// from `transcript` in `cwd` in a new one (see the module
    /// documentation); answers once its engine is built.
    ///
    /// # Errors
    ///
    /// The session is closed, or its new engine could not be built (the
    /// session runs on from the new transcript all the same, building again
    /// for its next work).
    pub(crate) async fn replace(&self, transcript: PathBuf, cwd: PathBuf) -> Result<(), String> {
        let (built, answer) = oneshot::channel();
        if !self.send(Request::Replace(Replacement {
            transcript,
            cwd,
            built,
        })) {
            return Err(self.closed_reason());
        }
        answer
            .await
            .unwrap_or_else(|_| Err("the session's host ended before its replacement".to_string()))
    }

    /// Why work sent to the session's thread is refused.
    fn closed_reason(&self) -> String {
        lock(&self.control.turn)
            .given_up
            .clone()
            .unwrap_or_else(|| "the session is closed".to_string())
    }

    /// Queues `request` for the session's thread; whether it is still open.
    fn send(&self, request: impl Into<Request>) -> bool {
        lock(&self.work)
            .as_ref()
            .is_some_and(|sender| sender.send(request.into()).is_ok())
    }

    /// Hands `steer`'s prompt to `running`'s turn while it still reads
    /// steering, unless it carries less trust than `running`: it would run
    /// with the running task's, so it runs as its own task instead. A steer
    /// offered to a task that has not reached this thread yet (the queue
    /// offers the steers queued behind a steer as it starts it) is kept for
    /// that task's turn, which takes it as it starts.
    ///
    /// An empty steer is a wake over rows already on the agent (a root's in
    /// the runtime daemon): a turn that still reads steering takes it with
    /// nothing to hand over, and any other runs it as its own task.
    pub(crate) fn steer(&self, running: &Task, steer: &Task) -> Steered {
        if steer.trust.lowest(running.trust) != running.trust {
            return Steered::Declined;
        }
        let mut turn = lock(&self.control.turn);
        if steer.prompt.is_empty() {
            return match (&turn.agent, &turn.task) {
                (Some(_), Some(task)) if turn.steerable && *task == running.id => {
                    turn.steered = true;
                    Steered::Taken
                }
                _ => Steered::Declined,
            };
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
            });
        let message = pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::User(
            pa_agent::types::UserMessage {
                content: pa_agent::types::UserContent::Text(steer.prompt.clone()),
                timestamp,
            },
        ));
        match (&turn.agent, &turn.task) {
            (Some(agent), Some(task)) if turn.steerable && *task == running.id => {
                steer_as_a_client(agent, message.clone());
                turn.steered = true;
                turn.taken.push((steer.clone(), message));
                Steered::Taken
            }
            (_, None)
                if turn.ended.as_deref() != Some(running.id.as_str())
                    && turn.cancelled_ahead.as_deref() != Some(running.id.as_str()) =>
            {
                turn.pending
                    .push((running.id.clone(), steer.clone(), message));
                Steered::Taken
            }
            _ => Steered::Declined,
        }
    }

    /// Cancels task `task`, the one the session's lane runs: its turn is
    /// aborted and it ends `Cancelled`. The queue saves a task as running
    /// before it hands it to this thread, so one that has not reached it yet
    /// is marked to start cancelled and run nothing. The steers handed to the
    /// task and not read yet are withdrawn from it and returned, queued
    /// again, for the caller to put back in the lane; `None` when `task`
    /// already ended here.
    pub(crate) fn cancel_running_task(&self, task: &str) -> Option<Vec<Task>> {
        let mut turn = lock(&self.control.turn);
        if turn.ended.as_deref() == Some(task) {
            return None;
        }
        if turn.task.as_deref() != Some(task) {
            let (pending, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut turn.pending)
                .into_iter()
                .partition(|(id, ..)| id == task);
            turn.pending = kept;
            turn.cancelled_ahead = Some(task.to_string());
            return Some(
                pending
                    .into_iter()
                    .map(|(_, steer, _)| Task {
                        state: pa_types::event_log::TaskState::Queued,
                        ..steer
                    })
                    .collect(),
            );
        }
        turn.cancelled = true;
        turn.steerable = false;
        let taken = std::mem::take(&mut turn.taken);
        let Some(agent) = &turn.agent else {
            return Some(Vec::new());
        };
        agent.abort();
        let unread = agent
            .remove_queued_messages(|message| taken.iter().any(|(_, handed)| handed == message));
        Some(
            taken
                .into_iter()
                .filter(|(_, handed)| unread.contains(handed))
                .map(|(task, _)| Task {
                    state: pa_types::event_log::TaskState::Queued,
                    ..task
                })
                .collect(),
        )
    }

    /// The engine of the session's current lifetime, once it is built.
    pub(crate) fn engine(&self) -> Option<LiveEngine> {
        lock(&self.control.turn).engine.clone()
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
    /// The session's place in the family tree.
    node: Arc<Node>,
    topics: SessionTopics,
    builder: Arc<B>,
    outbox: Outbox,
    control: Arc<TurnControl>,
    /// The runtime's event bus, which the kernel's `event.*` reach.
    events: Arc<BusEvents>,
    /// What a crash does to the session.
    faults: SessionFaults,
    /// The runtime daemon's scheduled jobs, once attached.
    jobs: super::JobsSlot,
}

/// How a lifetime ended.
enum Ended {
    /// The session was closed.
    Closed,
    /// It goes on from another transcript in a new lifetime.
    Replaced(Replacement),
    /// It crashed; the session goes on in a new lifetime (or, supervised and
    /// crashing too often, is given up).
    Crashed(SessionCrash),
}

/// The host thread: one lifetime after another until the session closes
/// (or, supervised, is given up).
#[expect(
    clippy::too_many_lines,
    reason = "a lifetime's end reads in the order it is handled: closed, replaced, crashed, \
              recorded, given up or rebuilt"
)]
fn serve<B: SessionBuilder>(
    lifetime: &mut Lifetime<B>,
    mut queued: mpsc::UnboundedReceiver<Request>,
    opening: Opening,
) {
    let mut rebuild = match opening {
        Opening::Fresh => None,
        // The crash the daemon's death was, recorded as the session opened:
        // mirrored as a live one is (a client that attaches later replays
        // it from the stored topics).
        Opening::Revived(rebuild) => {
            lifetime.node.stream.crashed(&rebuild.crash);
            Some(rebuild)
        }
    };
    let mut failures = rebuild.as_ref().map_or(0, |rebuild| rebuild.failures);
    // The replacement whose new engine the next lifetime answers.
    let mut replaced = None;
    loop {
        let scope = lifetime.scope();
        let faults = Arc::new(LifetimeFaults::default());
        match &lifetime.faults {
            SessionFaults::Contained => {}
            SessionFaults::Supervised(_) => faults.install(&scope),
        }
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
        let started = Instant::now();
        let ended = lifetime.run(
            &runtime,
            &mut queued,
            &faults,
            rebuild.take(),
            replaced.take(),
        );
        {
            let mut turn = lock(&lifetime.control.turn);
            turn.agent = None;
            turn.engine = None;
        }
        // The lifetime is cut off from the session's durable state before its
        // runtime goes, so nothing it leaves running writes again.
        scope.end();
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
        let crash = match ended {
            Ended::Closed => return,
            Ended::Replaced(replacement) => {
                lifetime.spec.transcript = Some(replacement.transcript);
                lifetime.node.set_cwd(&replacement.cwd);
                lifetime.spec.cwd = replacement.cwd;
                replaced = Some(replacement.built);
                continue;
            }
            Ended::Crashed(crash) => crash,
        };
        let supervision = match &lifetime.faults {
            SessionFaults::Contained => continue,
            SessionFaults::Supervised(supervision) => supervision,
        };
        // A lifetime that lived long enough proved healthy: this crash
        // starts a fresh count.
        failures = if started.elapsed() >= Duration::from_millis(STABLE_LIFETIME_MS) {
            1
        } else {
            failures + 1
        };
        // The lifetime's runtime is gone: the programs its commands started
        // and left running have no owner.
        let reaped_programs = orphan_journal(&lifetime.spec)
            .map_or(0, |journal| reap_orphans(&journal, std::process::id()));
        let crashed = Rebuild {
            crash,
            failures,
            reaped_programs,
        };
        record_crash(
            supervision,
            &lifetime.outbox,
            &lifetime.topics,
            &lifetime.id,
            &crashed,
        );
        lifetime.node.stream.crashed(&crashed.crash);
        if failures > MAX_CONSECUTIVE_FAILURES {
            let given_up = SessionFaultEvent::SessionFailed { failures };
            {
                // The transcript keeps the give-up, written in a lifetime of
                // its own (the crashed one's scope has ended).
                let scope = lifetime.scope();
                let _entered = session_scope::enter(Some(Arc::clone(&scope)));
                keep_fault(&lifetime.spec, &crashed, &given_up);
                scope.end();
            }
            publish_fault(
                &lifetime.outbox,
                &lifetime.topics.failed,
                &lifetime.id,
                &given_up,
            );
            lifetime.node.stream.fault(&given_up);
            let reason = format!(
                "the session was given up after {failures} crashes in a row: {}",
                crashed.crash.reason()
            );
            lock(&lifetime.control.turn).given_up = Some(reason.clone());
            while let Ok(work) = queued.try_recv() {
                work.fail(&reason);
            }
            return;
        }
        // Closing the session meanwhile waits the backoff out: at most the
        // fifth crash's 4 s.
        std::thread::sleep(Duration::from_millis(restart_backoff_ms(failures)));
        rebuild = Some(crashed);
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
    /// The lifetime's workspace checkpoints; `None` for a subagent or a
    /// session kept in memory.
    checkpoints: Option<Arc<Checkpoints>>,
}

impl<B: SessionBuilder> Lifetime<B> {
    /// A new lifetime's scope.
    fn scope(&self) -> Arc<SessionScope> {
        SessionScope::new(
            self.id.clone(),
            self.spec.cwd.clone(),
            self.spec.env.clone(),
            BTreeSet::new(),
        )
    }

    /// Serves work on `runtime` until the session closes or the lifetime
    /// crashes. A rebuild after a crash first keeps the fault in the
    /// transcript and answers the calls it cut off, then builds; once the
    /// engine is built the session's clients hear it recovered. A lifetime
    /// that follows a replacement answers it (`replaced`) once it built.
    fn run(
        &self,
        runtime: &tokio::runtime::Runtime,
        queued: &mut mpsc::UnboundedReceiver<Request>,
        faults: &LifetimeFaults,
        rebuild: Option<Rebuild>,
        replaced: Option<oneshot::Sender<Result<(), String>>>,
    ) -> Ended {
        let mut recovering = rebuild.map(|rebuild| {
            let recovered = SessionFaultEvent::SessionRecovered {
                failures: rebuild.failures,
                queued_input_suspended: false,
            };
            keep_fault(&self.spec, &rebuild, &recovered);
            recovered
        });
        let mut hosted = self.build(runtime);
        if let Some(replaced) = replaced {
            let _ = replaced.send(hosted.as_ref().map(|_| ()).map_err(Clone::clone));
        }
        loop {
            if hosted.is_ok() {
                if let Some(recovered) = recovering.take() {
                    publish_fault(&self.outbox, &self.topics.recovered, &self.id, &recovered);
                    self.node.stream.fault(&recovered);
                }
            }
            let next = runtime.block_on(async {
                tokio::select! {
                    biased;
                    crash = faults.faulted() => Err(crash),
                    work = queued.recv() => Ok(work),
                }
            });
            let work = match next {
                Ok(Some(Request::Work(work))) => work,
                Ok(Some(Request::Replace(replacement))) => {
                    // Dropped on its runtime, as a close drops it.
                    runtime.block_on(async move { drop(hosted) });
                    return Ended::Replaced(replacement);
                }
                Ok(None) => {
                    // Its subagents end with it (a family opened again
                    // stops reopening), then the engine is dropped on its
                    // runtime: what it spawns as it goes down still has one
                    // to run on.
                    let node = Arc::clone(&self.node);
                    runtime.block_on(async move {
                        dispose(&node).await;
                        drop(hosted);
                    });
                    return Ended::Closed;
                }
                Err(crash) => return Self::crashed(runtime, hosted, crash),
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
            if let Err(crash) = self.answer(runtime, session, work, faults) {
                return Self::crashed(runtime, hosted, crash);
            }
        }
    }

    /// The lifetime's end after `crash`: the engine it left is not used
    /// again. Dropping it may panic too, and that is this lifetime's end as
    /// well.
    fn crashed(
        runtime: &tokio::runtime::Runtime,
        hosted: Result<Hosted, String>,
        crash: SessionCrash,
    ) -> Ended {
        let _ = session_scope::catching_panics(AssertUnwindSafe(|| {
            runtime.block_on(async move { drop(hosted) });
        }));
        Ended::Crashed(crash)
    }

    /// Runs `work` on `runtime` until it ends or a fault ends the lifetime:
    /// its value, or the crash (a panic it raised, caught here, or a fault
    /// on another of the session's threads, which the work's own result is
    /// not trusted over).
    fn contained<T>(
        runtime: &tokio::runtime::Runtime,
        faults: &LifetimeFaults,
        work: impl std::future::Future<Output = T>,
    ) -> Result<T, SessionCrash> {
        let ran = session_scope::catching_panics(AssertUnwindSafe(|| {
            runtime.block_on(async {
                tokio::select! {
                    biased;
                    crash = faults.faulted() => Err(crash),
                    value = work => Ok(value),
                }
            })
        }));
        match ran {
            Ok(Ok(value)) => faults.fault().map_or(Ok(value), Err),
            Ok(Err(crash)) => Err(crash),
            Err(panic) => Err(faults.caught(&*panic)),
        }
    }

    /// Why `what` failed when `crash` ended the lifetime under it: the
    /// panic's text, or under supervision the crash's reason.
    fn crash_reason(&self, what: &str, crash: &SessionCrash) -> String {
        match &self.faults {
            SessionFaults::Contained => format!("{what} panicked: {}", crash.message),
            SessionFaults::Supervised(_) => crash.reason(),
        }
    }

    /// Runs one item of work on `session` and answers it. A crash under it
    /// is answered as its failure and returned: the lifetime ends.
    fn answer(
        &self,
        runtime: &tokio::runtime::Runtime,
        session: &mut Hosted,
        work: Work,
        faults: &LifetimeFaults,
    ) -> Result<(), SessionCrash> {
        match work {
            Work::Task {
                task,
                images,
                cause,
                outcome,
            } => {
                {
                    let mut turn = lock(&self.control.turn);
                    turn.task = Some(task.id.clone());
                    turn.cancelled = turn.cancelled_ahead.take().as_ref() == Some(&task.id);
                    turn.steerable = !turn.cancelled;
                    turn.steered = false;
                    // The steers kept for this task: its run reads them as
                    // it starts. Any kept for another never reached here.
                    let pending = std::mem::take(&mut turn.pending);
                    let mut taken = Vec::new();
                    if let Some(agent) = turn.agent.clone() {
                        for (_, steer, message) in
                            pending.into_iter().filter(|(id, ..)| *id == task.id)
                        {
                            steer_as_a_client(&agent, message.clone());
                            taken.push((steer, message));
                        }
                    }
                    turn.steered = !taken.is_empty();
                    turn.taken = taken;
                }
                self.control.trust.set(Some(RunningTask {
                    trust: task.trust,
                    cause: cause.clone(),
                }));
                let ran = Self::contained(runtime, faults, self.run_task(session, &task, images));
                let cancelled = {
                    let mut turn = lock(&self.control.turn);
                    turn.ended = turn.task.take();
                    turn.steerable = false;
                    turn.taken.clear();
                    std::mem::take(&mut turn.cancelled)
                };
                self.control.trust.set(None);
                let (end, panicked) = match ran {
                    Ok(_) if cancelled => (TaskEnd::CANCELLED, Ok(())),
                    Ok(end) => (end, Ok(())),
                    Err(crash) => (
                        TaskEnd::failed(TaskFailure::Turn, self.crash_reason("the turn", &crash)),
                        Err(crash),
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
                // anything a settle streams; the queue view's end of the
                // task goes ahead of it.
                self.node.stream.task_ended(&task.id);
                self.outbox
                    .send(done_event(&self.topics, &task, &end, &cause));
                let _ = outcome.send(Ran::Reported(end));
                match armed {
                    Some(settled) if settles => {
                        self.answer(runtime, session, Work::Settle(settled), faults)
                    }
                    Some(settled) => {
                        let _ = settled.send(Ok(None));
                        panicked
                    }
                    None => panicked,
                }
            }
            Work::Settle(settled) => {
                let answer = Self::contained(runtime, faults, self.settle(session));
                let (answer, panicked) = match answer {
                    Ok(answer) => (answer.map(Some), Ok(())),
                    Err(crash) => (Err(self.crash_reason("the settle", &crash)), Err(crash)),
                };
                // Behind the settle's events: its caller has them all first.
                self.outbox.then(move || {
                    let _ = settled.send(answer);
                });
                panicked
            }
            Work::Finish(finished) => {
                let answer = Self::contained(runtime, faults, self.finish(session));
                let (answer, panicked) = match answer {
                    Ok(answer) => (Ok(answer), Ok(())),
                    Err(crash) => (Err(self.crash_reason("the finish", &crash)), Err(crash)),
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
    #[expect(
        clippy::too_many_lines,
        reason = "the seams, the engine build and the driver wiring read in the order they run"
    )]
    fn build(&self, runtime: &tokio::runtime::Runtime) -> Result<Hosted, String> {
        let built = session_scope::catching_panics(AssertUnwindSafe(|| {
            let gate = TrustGate::new(self.control.trust.clone(), &self.spec.cwd);
            let mut host_handlers = pa_core::kernel::HostRequestHandlers::new();
            register_event_bus_host_handlers(
                &Arc::new(SessionEvents {
                    events: Arc::clone(&self.events),
                    task: self.control.trust.clone(),
                    requester: EventRequester {
                        session_id: self.id.clone(),
                        name: self.spec.name.clone().unwrap_or_else(|| self.id.clone()),
                        cwd: self.spec.cwd.to_string_lossy().into_owned(),
                    },
                    dir: WorkingDir::new(&self.spec.cwd),
                }),
                &mut host_handlers,
            );
            register_agent_message_host_handlers(
                Arc::new(FamilyMessages(Arc::clone(&self.node))),
                &mut host_handlers,
            );
            let in_daemon = self
                .node
                .tree
                .upgrade()
                .is_some_and(|tree| tree.daemon().is_some());
            // The runtime daemon's sessions observe their families.
            if in_daemon {
                register_agent_observe_host_handlers(
                    Arc::new(FamilyObserve(Arc::clone(&self.node))),
                    &mut host_handlers,
                );
            }
            let seams = SessionSeams {
                call_policy: Arc::new(gate),
                host_handlers,
                subagents: Arc::new(SessionSubagents(Arc::clone(&self.node))),
                on_late_sent_agent_message: late_sent_agent_message_sink(&self.node),
                bash_wake: {
                    let node = Arc::clone(&self.node);
                    Arc::new(move || node.activity.notify_waiters())
                },
                kernel_notices: kernel_notice_sink(&self.node),
                scheduled_jobs: self
                    .jobs
                    .get()
                    .and_then(|jobs| jobs.wiring(&self.id, &self.spec)),
                // The runtime daemon's sessions ask their attached clients.
                interaction: in_daemon.then(|| self.node.interaction.lifetime_hub()),
            };
            runtime.block_on(self.builder.build(&self.id, &self.spec, seams))
        }));
        let built = match built {
            Ok(Ok(built)) => built,
            Ok(Err(error)) => {
                self.node.built(Err(format!("{error:#}")));
                return Err(format!("build the session: {error:#}"));
            }
            Err(_) => {
                let error = "building the session panicked".to_string();
                self.node.built(Err(error.clone()));
                return Err(error);
            }
        };
        let agent = Arc::clone(built.engine.session.agent());
        let json = built.events != DriverEvents::Quiet;
        let hosted = runtime.block_on(async {
            let stream = &self.node.stream;
            stream.adopt(built.events, &built.engine).await;
            // The stream's listener first: on every agent event the stream
            // has it before the goal's accounting reports what it changed.
            agent
                .subscribe({
                    let stream = Arc::clone(stream);
                    let agent = Arc::downgrade(&agent);
                    move |event, _signal| {
                        let stream = Arc::clone(&stream);
                        let agent = agent.clone();
                        Box::pin(async move {
                            stream.agent_event(&event, &agent).await;
                            Ok(())
                        })
                    }
                })
                .await;
            // A cancel that lands after its task's last check and before the
            // agent's run registers finds no run to abort (`Agent::abort`
            // aborts only a live one): that run aborts as it starts.
            agent
                .subscribe({
                    let control = Arc::clone(&self.control);
                    move |event, _signal| {
                        if matches!(event, pa_agent::types::AgentEvent::AgentStart) {
                            let turn = lock(&control.turn);
                            if let Some(agent) = turn.agent.as_ref().filter(|_| turn.cancelled) {
                                agent.abort();
                            }
                        }
                        Box::pin(std::future::ready(Ok(())))
                    }
                })
                .await;
            let engine = &built.engine;
            // The summary's deltas while a compaction writes it, in the
            // session-event shapes only (the json stream never had them).
            if built.events == DriverEvents::SessionEvents {
                let stream = Arc::clone(stream);
                engine
                    .session
                    .set_compaction_summary_sink(Arc::new(move |delta: &str| {
                        stream.publish(&serde_json::json!({
                            "type": "compaction_summary_delta",
                            "delta": delta,
                        }));
                    }));
            }
            self.node
                .adopt(engine, &built.model, &built.agent_dir)
                .await;
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
            // the hook holds the node weakly, as the engine's own agent keeps
            // the hook.
            let unsettled = {
                let node = Arc::downgrade(&self.node);
                Arc::new(move || match node.upgrade() {
                    Some(node) => Box::pin(async move { has_unsettled_rlm_work(&node).await })
                        as pa_agent::BoxFut<'static, bool>,
                    None => Box::pin(std::future::ready(false)),
                }) as UnsettledWork
            };
            wire_continuation_hook(
                engine,
                &agent,
                &built.model,
                &goal,
                &autonomous,
                Some(unsettled),
            );
            let global_harness_dir =
                pa_core::refinement::get_global_harness_state_dir(&built.agent_dir);
            // A transcript kept on disk, of a root session: its workspace's
            // checkpoints (a subagent's writes land in its parent's turn).
            let checkpoints = (self.spec.subagent.is_none() && !self.spec.ephemeral).then(|| {
                Arc::new(Checkpoints::new(
                    built.agent_dir.clone(),
                    self.spec.cwd.clone(),
                ))
            });
            Hosted {
                boundary: TurnBoundary::with_sink(json, self.publisher()),
                goal,
                autonomous,
                goal_updated_at_start,
                global_harness_dir,
                checkpoints,
                built,
            }
        });
        {
            let mut turn = lock(&self.control.turn);
            turn.agent = Some(agent);
            turn.engine = Some(LiveEngine {
                engine: Arc::clone(&hosted.built.engine),
                runtime: runtime.handle().clone(),
                autonomous: hosted.autonomous.state_handle(),
                provider_target: Arc::clone(&hosted.built.provider_target),
                checkpoints: hosted.checkpoints.clone(),
            });
        }
        self.node.built(Ok(()));
        Ok(hosted)
    }

    /// Publishes one json payload on the session's stream, with the running
    /// task's trust.
    fn publisher(&self) -> Arc<dyn Fn(&Value) + Send + Sync> {
        let stream = Arc::clone(&self.node.stream);
        Arc::new(move |payload: &Value| stream.publish(payload))
    }

    /// One task. A session command runs at the boundary instead of a turn
    /// ([`Lifetime::run_session_command`]). A prompt runs the pre-turn boundary,
    /// the prompt with its retries, a steer the run took but did not read,
    /// the settled-turn boundary, then the goal's queued turns and, when the
    /// goal does not own the wake-up, the autonomous run's held ones. An
    /// empty prompt runs none of that: it drives the rows delivered to the
    /// session to quiescence, as a subagent's every task ends doing.
    #[expect(
        clippy::too_many_lines,
        reason = "a task's steps read linearly, each with the end it fails with"
    )]
    async fn run_task(
        &self,
        session: &mut Hosted,
        task: &Task,
        images: Vec<pa_agent::types::ImageContent>,
    ) -> TaskEnd {
        // Cancelled before it started: nothing of it runs.
        if lock(&self.control.turn).cancelled {
            return TaskEnd::CANCELLED;
        }
        let command = session
            .built
            .engine
            .session
            .classify_session_command(&task.prompt);
        if let Some(command) = command {
            self.node.stream.session_command_started();
            let end = Self::run_session_command(session, &command, &images).await;
            let error = match &end.outcome {
                TaskOutcome::Failed(error) => Some(error.as_str()),
                TaskOutcome::Done | TaskOutcome::Cancelled => None,
            };
            self.node.stream.session_command_ended(error);
            return end;
        }
        let Hosted {
            built,
            boundary,
            goal,
            autonomous,
            global_harness_dir,
            checkpoints,
            ..
        } = session;
        let engine = &built.engine;
        let turn_failed = |reason: String| TaskEnd::failed(TaskFailure::Turn, reason);
        // An empty input wakes the session over the rows delivered to it
        // (a subagent's task, a message to it): they run as its quiescence
        // drive runs them, below. An input of images alone is a prompt.
        let wake = task.prompt.is_empty() && images.is_empty();
        if !wake {
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
            let settings =
                pa_core::settings::SettingsManager::create(&self.spec.cwd, &built.agent_dir);
            let retry_policy = settings.get_provider_retry_policy();
            let agent = engine.session.agent();
            match built.queue_modes {
                super::QueueModes::Built => {}
                super::QueueModes::Settings => {
                    let queue_mode = |mode| match mode {
                        pa_core::settings::QueueModeSetting::All => pa_agent::agent::QueueMode::All,
                        pa_core::settings::QueueModeSetting::OneAtATime => {
                            pa_agent::agent::QueueMode::OneAtATime
                        }
                    };
                    agent.set_steering_mode(queue_mode(settings.get_steering_mode()));
                    agent.set_follow_up_mode(queue_mode(settings.get_follow_up_mode()));
                }
            }
            let publish = self.publisher();
            // The workspace before the turn, recorded with the workspace as
            // it settled on the entries the turn appended.
            let opening = match checkpoints {
                Some(checkpoints) => checkpoints.begin(engine).await,
                None => None,
            };
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
            let (cancelled, steered) = {
                let mut turn = lock(&self.control.turn);
                turn.steerable = false;
                (turn.cancelled, turn.steered)
            };
            if let Some(opening) = opening {
                Checkpoints::settle(opening, engine).await;
            }
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
        }
        // A subagent's task ends at rest: the rows its turns left and its own
        // subagents' settles run first, so its `done` follows its subtree. A
        // root's empty input (its wake in the runtime daemon) runs the rows
        // delivered to it and leaves its subagents to wake it again.
        if wake || self.node.is_subagent() {
            let rest = if self.node.is_subagent() {
                Rest::Subtree
            } else {
                Rest::OwnRows
            };
            loop {
                if let Err(error) = drive_to_quiescence(
                    &self.node,
                    engine,
                    boundary,
                    &built.model,
                    built.api_key.clone(),
                    global_harness_dir.clone(),
                    rest,
                )
                .await
                {
                    return turn_failed(error);
                }
                // A root's wake takes no steer from here on (a later wake runs
                // as its own task); a row whose wake it took as the drive
                // returned runs now, once.
                let steerable = match rest {
                    Rest::OwnRows => std::mem::take(&mut lock(&self.control.turn).steerable),
                    Rest::Subtree => false,
                };
                let taken_late = steerable && engine.session.agent().has_queued_messages();
                if !taken_late {
                    break;
                }
            }
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
    async fn settle(&self, session: &mut Hosted) -> Result<Settled, String> {
        self.drive_to_rest(session).await?;
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
    async fn drive_to_rest(&self, session: &mut Hosted) -> Result<(), String> {
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
            if drive_to_quiescence(
                &self.node,
                engine,
                boundary,
                model,
                built.api_key.clone(),
                global_harness_dir.clone(),
                Rest::Subtree,
            )
            .await?
            {
                continue;
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
    async fn finish(&self, session: &mut Hosted) -> Finished {
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
        dispose(&self.node).await;
        Finished {
            refine_command_failed: boundary.refine_command_failed(),
        }
    }
}

/// Offers a client's steer to `agent`: like the old daemon's steering lane
/// (`enqueue_priority`, where a plain user row ranks above a custom row), it
/// goes ahead of the custom rows waiting at the queue's end (agent messages,
/// children's and kernel notices), not behind them.
fn steer_as_a_client(agent: &Agent, message: pa_agent::types::AgentMessage) {
    agent.steer_ahead_of(message, |batch| {
        batch
            .iter()
            .all(|row| matches!(row, pa_agent::types::AgentMessage::Custom(_)))
    });
}
