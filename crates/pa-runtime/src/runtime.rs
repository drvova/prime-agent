//! The runtime: the bus over the agent's SQLite database, the task queue,
//! and the sessions that run its tasks, behind one handle.
//!
//! At open it subscribes every session's input: an event on
//! `agent/<id>/input` (an `AgentInput`) queues a `Prompt` task in session
//! `<id>`. A task's turn streams its agent events on `agent/<id>/stream`
//! (live only, each event in the session-event wire shape the headless json
//! mode prints) and ends with `agent/<id>/done` (stored, an `AgentDone`).

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use pa_types::event_log::{
    AgentTarget, NewEvent, Priority, Route, Scope, Subscription, TopicFilter, Trust,
};
use tokio::sync::mpsc;

use crate::bus::{Bus, BusParts, Listener, Published};
use crate::log_stores::{LogEventStore, LogStateStore, LogTaskStore};
use crate::routes::{ModelCall, ModelCaller};
use crate::session::{
    Finished, Outbox, Outgoing, RuntimeTaskSink, SessionBuilder, SessionRunner, SessionSpec,
    Sessions, SettleAfter, Settled,
};
use crate::subscriptions::MemorySessionTree;
use crate::tasks::TaskQueue;

/// The id of the subscription that turns every session's input into its
/// tasks.
const INPUT_SUBSCRIPTION: &str = "runtime/input";

/// What a [`Runtime`] is opened with.
pub struct RuntimeOptions<B> {
    /// Builds each session's engine.
    pub builder: B,
    /// How many spare Scheme kernels the process keeps built ahead of the
    /// sessions that take them.
    pub spare_kernels: usize,
    /// How many tasks run at once across every session.
    pub max_concurrent_runs: NonZeroUsize,
    /// How many events a listener may fall behind before it loses the
    /// oldest.
    pub listener_capacity: NonZeroUsize,
    /// Where a session spawned for an event without a `cwd` of its own runs.
    pub cwd: PathBuf,
    /// The sessions to open with the runtime: their engines start building
    /// while its database opens, so opening it costs a first turn nothing.
    pub sessions: Vec<SessionSpec>,
}

/// The runtime's model caller: `Call` routes have no model to call yet, so
/// each is refused and reported as `system/route_failed`.
struct NoModelCalls;

impl ModelCaller for NoModelCalls {
    fn call(
        &self,
        call: ModelCall,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
        std::future::ready(Err(anyhow::anyhow!(
            "subscription {}: this runtime makes no model calls",
            call.subscription
        )))
    }
}

type RuntimeBus<B> =
    Bus<LogEventStore, LogStateStore, MemorySessionTree, NoModelCalls, RuntimeTaskSink<B>>;

/// The event-driven runtime of one agent dir.
pub struct Runtime<B: SessionBuilder> {
    bus: RuntimeBus<B>,
    queue: TaskQueue<SessionRunner<B>, LogTaskStore>,
    runner: SessionRunner<B>,
    sessions: Arc<Sessions<B>>,
    publisher: tokio::task::JoinHandle<()>,
}

impl<B: SessionBuilder> Runtime<B> {
    /// Opens the runtime over `agent_dir`'s database (the transcript store's
    /// `agent.db`), keeps `options.spare_kernels` Scheme kernels built in the
    /// background, opens `options.sessions` while the database opens,
    /// restores those sessions' tasks and subscribes every session's input.
    /// It restores no other session's task: a session's tasks are restored
    /// when it is opened (here or by [`Runtime::create_session`]), so
    /// another process running other sessions on the same database is never
    /// touched. Nothing here touches the network.
    ///
    /// # Errors
    ///
    /// A session cannot be opened (see [`Runtime::create_session`]), the
    /// database cannot be opened or migrated, or a session's tasks cannot be
    /// restored from it; the sessions opened are closed again.
    ///
    /// # Panics
    ///
    /// Called outside a Tokio runtime.
    pub async fn open(agent_dir: &Path, options: RuntimeOptions<B>) -> anyhow::Result<Self> {
        let database = pa_core::session::store::database_path(agent_dir);
        let log = tokio::task::spawn_blocking(move || {
            pa_log::Log::open(database, pa_log::DEFAULT_EVENT_RETENTION)
        });
        pa_core::kernel::keep_spare_scheme_kernels(options.spare_kernels);
        // The sessions' events wait in the outbox until the bus is up.
        let (outbox, outgoing) = Outbox::channel();
        let sessions = Arc::new(Sessions::new(options.builder, outbox));
        let runner = SessionRunner::new(Arc::clone(&sessions));
        let restored = async {
            let mut opened = Vec::with_capacity(options.sessions.len());
            for spec in options.sessions {
                opened.push(sessions.open(spec)?);
            }
            let log = Arc::new(log.await.context("open the runtime's database")??);
            let queue = TaskQueue::new(
                runner.clone(),
                LogTaskStore::new(Arc::clone(&log)),
                options.max_concurrent_runs,
            );
            for id in &opened {
                queue
                    .restore(id)
                    .await
                    .with_context(|| format!("restore the tasks of session {id}"))?;
            }
            anyhow::Ok((log, queue))
        }
        .await;
        let (log, queue) = match restored {
            Ok(restored) => restored,
            Err(error) => {
                sessions.close_all().await;
                return Err(error);
            }
        };
        let tree = MemorySessionTree::default();
        let bus = Bus::new(BusParts {
            events: LogEventStore::new(Arc::clone(&log)),
            state: LogStateStore::new(log),
            sessions: tree.clone(),
            model: NoModelCalls,
            tasks: RuntimeTaskSink {
                queue: queue.clone(),
                sessions: Arc::clone(&sessions),
                tree,
                cwd: options.cwd,
            },
            listener_capacity: options.listener_capacity,
        });
        bus.add_subscription(Subscription {
            id: INPUT_SUBSCRIPTION.to_string(),
            scope: Scope::Global,
            filter: TopicFilter::new("agent/+/input").expect("the input filter is valid"),
            when: None,
            throttle_ms: 0,
            debounce_ms: 0,
            route: Route::Agent {
                target: AgentTarget::Origin,
                prompt: String::new(),
                priority: Priority::Prompt,
            },
            trust: Trust::Owner,
        })?;
        let publisher = tokio::spawn(publish_outgoing(bus.clone(), outgoing));
        Ok(Runtime {
            bus,
            queue,
            runner,
            sessions,
            publisher,
        })
    }

    /// Publishes `event` on the bus (`Bus::publish`).
    ///
    /// # Errors
    ///
    /// The database failed to store it or an event its routes caused.
    pub async fn publish(&self, event: NewEvent) -> anyhow::Result<Published> {
        self.bus.publish(event).await
    }

    /// A listener for every event from now on (`Bus::listen`).
    #[must_use]
    pub fn listen(&self) -> Listener {
        self.bus.listen()
    }

    /// Opens a session on its own thread, which starts building its engine,
    /// and restores its tasks from the database (`TaskQueue::restore`: the
    /// queued ones queued again, a running one failed as interrupted);
    /// returns its id. Its tasks queue at once and run once it is built.
    /// The caller holds the session (the print mode's `SessionLease`), so
    /// no other process runs the tasks restored.
    ///
    /// # Errors
    ///
    /// The id is not a topic level or is open already, or the operating
    /// system refused the thread; or the database failed to restore its
    /// tasks, and the session stays open without them.
    pub async fn create_session(&self, spec: SessionSpec) -> anyhow::Result<String> {
        let id = self.sessions.open(spec)?;
        self.queue
            .restore(&id)
            .await
            .with_context(|| format!("restore the tasks of session {id}"))?;
        Ok(id)
    }

    /// Cancels `session`'s queued tasks and its running one, whose turn is
    /// aborted; each ends `Cancelled`.
    ///
    /// # Errors
    ///
    /// The database failed to save a cancelled task.
    pub async fn cancel(&self, session: &str) -> anyhow::Result<()> {
        self.queue.cancel_queued(session).await?;
        self.runner.cancel_running(session);
        Ok(())
    }

    /// Waits until `session` has no task queued or running.
    pub async fn idle(&self, session: &str) {
        self.queue.session_idle(session).await;
    }

    /// Runs `session` to rest `after` the work it names, on the session's
    /// own thread: drives its subagents to quiescence and runs the turns
    /// they leave, the goal's owed continuation, the autonomous one held
    /// behind them, and whatever those turns arm; then reports how the run
    /// ended. A settle armed behind a task that stops the run (a failed
    /// command or turn, a cancel) answers `None`. It is queued or armed when
    /// this is called, and answers once every event it streamed is on the
    /// bus, behind the `done` of the task it follows.
    ///
    /// # Errors
    ///
    /// The session is not open or its engine could not be built, or a turn
    /// the settle ran failed; the error is its reason.
    pub fn settle(
        &self,
        session: &str,
        after: SettleAfter,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<Settled>>> + Send + 'static {
        let settled = self.sessions.host(session).map(|host| host.settle(after));
        let session = session.to_string();
        async move {
            match settled {
                Some(settled) => settled.await.map_err(anyhow::Error::msg),
                None => Err(anyhow::anyhow!(
                    "session {session} is not open in this runtime"
                )),
            }
        }
    }

    /// Finishes `session`'s run: waits for the refinements its turns started
    /// and ends its subagents, then reports what that found. Answers once
    /// every event it streamed is on the bus. The session stays open, with
    /// no subagents, until [`Runtime::shutdown`].
    ///
    /// # Errors
    ///
    /// The session is not open or its engine could not be built.
    pub async fn finish(&self, session: &str) -> anyhow::Result<Finished> {
        let host = self
            .sessions
            .host(session)
            .with_context(|| format!("session {session} is not open in this runtime"))?;
        host.finish().await.map_err(anyhow::Error::msg)
    }

    /// Closes every session (each finishes the task it is running) and stops
    /// publishing their events.
    pub async fn shutdown(self) {
        self.sessions.close_all().await;
        self.publisher.abort();
    }
}

/// Publishes the sessions' events in the order they were queued.
async fn publish_outgoing<B: SessionBuilder>(
    bus: RuntimeBus<B>,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
) {
    while let Some(outgoing) = outgoing.recv().await {
        let (event, published) = match outgoing {
            Outgoing::Event { event, published } => (event, published),
            Outgoing::Then(then) => {
                then();
                continue;
            }
        };
        let topic = event.topic.clone();
        let result = bus.publish(event).await.map(|_| ());
        match published {
            Some(published) => {
                let _ = published.send(result);
            }
            None => {
                if let Err(error) = result {
                    tracing::error!("a session's event on {topic} was not published: {error:#}");
                }
            }
        }
    }
}
