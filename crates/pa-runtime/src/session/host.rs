//! A session's host: its own OS thread and current-thread Tokio runtime,
//! entered with the session's scope, owning the session's engine and
//! running its tasks one at a time as they arrive.
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
use pa_core::session_engine::session_events::agent_event_json;
use pa_types::event_log::{NewEvent, Task, Trust};
use pa_types::session_scope::{self, SessionScope};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{BuiltSession, Outbox, SessionBuilder, SessionSpec, SessionTopics};
use crate::lock;
use crate::tasks::{Steered, TaskOutcome};
use crate::turn::autonomous::latest_assistant_error;
use crate::turn::boundary::TurnBoundary;

/// How long a lifetime's runtime may take to drop its tasks once the
/// lifetime has ended; threads still blocked after that are abandoned.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// A task for the host to run, and where its outcome goes.
struct Work {
    task: Task,
    outcome: oneshot::Sender<TaskOutcome>,
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

    /// Runs `task` on the session's thread.
    pub(crate) async fn run(&self, task: Task) -> TaskOutcome {
        let (outcome, answer) = oneshot::channel();
        let sent = lock(&self.work)
            .as_ref()
            .is_some_and(|work| work.send(Work { task, outcome }).is_ok());
        if !sent {
            return TaskOutcome::Failed("the session is closed".to_string());
        }
        answer.await.unwrap_or_else(|_| {
            TaskOutcome::Failed("the session's host ended before the task did".to_string())
        })
    }

    /// Hands `steer`'s prompt to `running`'s turn while it still reads
    /// steering.
    pub(crate) fn steer(&self, running: &Task, steer: &Task) -> Steered {
        let turn = lock(&self.control.turn);
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
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .on_thread_start(session_scope::inherit())
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let reason = format!("build the session's runtime: {error}");
                while let Ok(work) = queued.try_recv() {
                    let _ = work.outcome.send(TaskOutcome::Failed(reason.clone()));
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

/// A lifetime's engine and the turn boundary its turns run through.
struct Hosted {
    built: BuiltSession,
    boundary: TurnBoundary,
    global_harness_dir: PathBuf,
}

impl<B: SessionBuilder> Lifetime<B> {
    /// Serves tasks on `runtime` until the session closes or a turn panics.
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
                    let _ = work.outcome.send(TaskOutcome::Failed(error.clone()));
                    continue;
                }
            };
            {
                let mut turn = lock(&self.control.turn);
                turn.task = Some(work.task.id.clone());
                turn.steerable = true;
                turn.cancelled = false;
                turn.trust = Some(work.task.trust);
            }
            let ran = session_scope::catching_panics(AssertUnwindSafe(|| {
                runtime.block_on(self.run_task(session, &work.task))
            }));
            let cancelled = {
                let mut turn = lock(&self.control.turn);
                turn.task = None;
                turn.steerable = false;
                turn.trust = None;
                std::mem::take(&mut turn.cancelled)
            };
            match ran {
                Ok(outcome) => {
                    let outcome = if cancelled {
                        TaskOutcome::Cancelled
                    } else {
                        outcome
                    };
                    let _ = work.outcome.send(outcome);
                }
                Err(panic) => {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "a panic with a non-text payload".to_string());
                    let _ = work
                        .outcome
                        .send(TaskOutcome::Failed(format!("the turn panicked: {message}")));
                    // The engine the panic left is not used again; dropping it
                    // may panic too, and that is this lifetime's end as well.
                    let _ = session_scope::catching_panics(AssertUnwindSafe(|| {
                        runtime.block_on(async move { drop(hosted) });
                    }));
                    return Ended::Crashed;
                }
            }
        }
    }

    /// Builds the lifetime's engine and wires its events onto the bus.
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
        runtime.block_on(agent.subscribe({
            let publisher = self.publisher();
            move |event, _signal| {
                if let Some(payload) = agent_event_json(&event) {
                    publisher(&payload);
                }
                Box::pin(std::future::ready(Ok(())))
            }
        }));
        lock(&self.control.turn).agent = Some(agent);
        let global_harness_dir =
            pa_core::refinement::get_global_harness_state_dir(&built.agent_dir);
        Ok(Hosted {
            built,
            boundary: TurnBoundary::with_sink(/*json_mode*/ true, self.publisher()),
            global_harness_dir,
        })
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

    /// One task's turn: the pre-turn boundary, the prompt with its retries,
    /// steering the run did not read, then the settled-turn boundary.
    async fn run_task(&self, session: &mut Hosted, task: &Task) -> TaskOutcome {
        let Hosted {
            built,
            boundary,
            global_harness_dir,
        } = session;
        let engine = &built.engine;
        if let Err(error) = boundary
            .run_pre_turn(engine, &built.model, built.api_key.clone())
            .await
        {
            return TaskOutcome::Failed(error);
        }
        // Cancelled before its run started: there is no run to abort.
        if lock(&self.control.turn).cancelled {
            return TaskOutcome::Cancelled;
        }
        let retry_policy =
            pa_core::settings::SettingsManager::create(&self.spec.cwd, &built.agent_dir)
                .get_provider_retry_policy();
        let publish = self.publisher();
        let prompted = engine
            .session
            .prompt_with_auto_retry(
                &task.prompt,
                Vec::new(),
                &retry_policy,
                built.model.context_window,
                |event| {
                    publish(&crate::turn::auto_retry::auto_retry_event_json(event));
                    std::future::ready(Ok(()))
                },
            )
            .await;
        let agent = engine.session.agent();
        let cancelled = {
            let mut turn = lock(&self.control.turn);
            turn.steerable = false;
            turn.cancelled
        };
        if cancelled {
            return TaskOutcome::Cancelled;
        }
        if let Err(error) = prompted {
            return TaskOutcome::Failed(format!("{error:#}"));
        }
        // A steer taken as the run settled was never read: it runs now, as
        // part of this task.
        if agent.has_queued_messages() {
            if let Err(error) = agent.continue_run().await {
                return TaskOutcome::Failed(format!("{error:#}"));
            }
            agent.wait_for_idle().await;
        }
        if let Err(error) = boundary
            .run_at_settled_turn(
                engine,
                &built.model,
                built.api_key.clone(),
                global_harness_dir.clone(),
            )
            .await
        {
            return TaskOutcome::Failed(error);
        }
        match latest_assistant_error(engine).await {
            Some(error) => {
                TaskOutcome::Failed(error.unwrap_or_else(|| "the model request failed".to_string()))
            }
            None => TaskOutcome::Done,
        }
    }
}
