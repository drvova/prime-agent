//! The runtime: the bus over the agent's SQLite database, the task queue,
//! and the sessions that run its tasks, behind one handle.
//!
//! At open it subscribes every session's input: an event on
//! `agent/<id>/input` (an `AgentInput`) queues a `Prompt` task in session
//! `<id>`. A task's turn streams its agent events on `agent/<id>/stream`
//! (live only, each event in the session-event wire shape the headless json
//! mode prints) and ends with `agent/<id>/done` (stored, an `AgentDone`).
//! The kernel's `event-*` functions of its sessions reach its bus
//! ([`crate::events`]).

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use pa_types::event_log::{
    AgentDone, AgentTarget, NewEvent, Priority, Route, Scope, Subscription, TopicFilter, Trust,
};
use tokio::sync::mpsc;

use crate::bus::{Bus, BusParts, Listener, Published};
use crate::events::BusEvents;
use crate::log_stores::{LogEventStore, LogStateStore, LogTaskStore};
use crate::routes::{ModelCall, ModelCaller};
use crate::session::{
    Finished, JobsSlot, Opening, Outbox, Outgoing, RuntimeTaskSink, SessionBuilder, SessionFaults,
    SessionJobs, SessionRunner, SessionSpec, Sessions, SettleAfter, Settled,
};
use crate::sources::{SourceDefinition, SourceManager};
use crate::subagents::{SessionControl, Subagents};
use crate::subscriptions::DuplicateSubscription;
use crate::subscriptions::MemorySessionTree;
use crate::tasks::TaskQueue;
use crate::wire::{Handlers, WireConfig, WireServer};

/// The id of the subscription that turns every session's input into its
/// tasks.
const INPUT_SUBSCRIPTION: &str = "runtime/input";

/// What becomes of a session's queue when its running turn is aborted
/// ([`Runtime::abort`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterAbort {
    /// The lane is suspended: what is queued stays queued until
    /// [`Runtime::resume_queue`].
    Park,
    /// The lane keeps going (resumed if it was suspended): the steers queued
    /// run next, delivered together, then the rest.
    SendQueued,
}

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
pub(crate) struct NoModelCalls;

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
    /// What the subagents open, cancel and close sessions through; they hold
    /// it weakly.
    _control: Arc<dyn SessionControl>,
    /// The agent's database.
    log: Arc<pa_log::Log>,
    sources: SourceManager,
    publisher: tokio::task::JoinHandle<()>,
}

/// The runtime's sessions and their tasks, as the subagents reach them.
struct Control<B: SessionBuilder> {
    sessions: Arc<Sessions<B>>,
    queue: TaskQueue<SessionRunner<B>, LogTaskStore>,
    runner: SessionRunner<B>,
}

impl<B: SessionBuilder> SessionControl for Control<B> {
    fn open(&self, spec: SessionSpec) -> anyhow::Result<String> {
        self.sessions.open(spec, Opening::Fresh)
    }

    fn cancel(&self, session: &str) -> pa_agent::BoxFut<'_, anyhow::Result<()>> {
        let session = session.to_string();
        Box::pin(async move {
            self.queue.cancel_queued(&session).await?;
            if let Some(running) = self.queue.lane(&session).running {
                self.runner.cancel_running_task(&session, &running.id);
            }
            Ok(())
        })
    }

    fn close(&self, session: &str) -> pa_agent::BoxFut<'_, ()> {
        let session = session.to_string();
        Box::pin(async move { self.sessions.close(&session).await })
    }
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
        Self::open_with(agent_dir, options, SessionFaults::Contained).await
    }

    /// [`Runtime::open`], with `faults` saying what a session's crash does.
    pub(crate) async fn open_with(
        agent_dir: &Path,
        options: RuntimeOptions<B>,
        faults: SessionFaults,
    ) -> anyhow::Result<Self> {
        let database = pa_core::session::store::database_path(agent_dir);
        let log = tokio::task::spawn_blocking(move || {
            pa_log::Log::open(database, pa_log::DEFAULT_EVENT_RETENTION)
        });
        pa_core::kernel::keep_spare_scheme_kernels(options.spare_kernels);
        // The sessions' events wait in the outbox until the bus is up.
        let (outbox, outgoing) = Outbox::channel();
        let events = Arc::new(BusEvents::new(agent_dir, outbox.clone()));
        let tree = MemorySessionTree::default();
        let subagents = Arc::new(Subagents::new(outbox.clone(), tree.clone()));
        let jobs = JobsSlot::default();
        let sessions = Arc::new(Sessions::new(
            options.builder,
            outbox,
            Arc::clone(&events),
            Arc::clone(&subagents),
            faults,
            Arc::clone(&jobs),
        ));
        let runner = SessionRunner::new(Arc::clone(&sessions));
        let restored = async {
            let mut opened = Vec::with_capacity(options.sessions.len());
            for spec in options.sessions {
                opened.push(sessions.open(spec, Opening::Fresh)?);
            }
            let log = Arc::new(log.await.context("open the runtime's database")??);
            let queue = TaskQueue::new(
                runner.clone(),
                LogTaskStore::sparing(Arc::clone(&log), sessions.unstored().clone()),
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
        let bus = Bus::new(BusParts {
            events: LogEventStore::sparing(Arc::clone(&log), sessions.unstored().clone()),
            state: LogStateStore::sparing(Arc::clone(&log), sessions.unstored().clone()),
            sessions: tree.clone(),
            model: NoModelCalls,
            tasks: RuntimeTaskSink {
                queue: queue.clone(),
                sessions: Arc::clone(&sessions),
                tree,
                events: Arc::clone(&events),
                subagents: Arc::clone(&subagents),
                cwd: options.cwd,
                jobs: Arc::clone(&jobs),
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
        let sources =
            SourceManager::spawn(Arc::clone(&log), &bus, Arc::new(pa_sources::SystemClock));
        events
            .attach(Arc::clone(&log), bus.registry(), sources.clone())
            .await;
        let control: Arc<dyn SessionControl> = Arc::new(Control {
            sessions: Arc::clone(&sessions),
            queue: queue.clone(),
            runner: runner.clone(),
        });
        subagents.attach(bus.registry(), Arc::downgrade(&control));
        let publisher = tokio::spawn(publish_outgoing(bus.clone(), queue.clone(), outgoing));
        Ok(Runtime {
            bus,
            queue,
            runner,
            sessions,
            _control: control,
            log,
            sources,
            publisher,
        })
    }

    /// Attaches the runtime daemon's scheduled jobs: every session built
    /// from now on binds its kernel's `schedule.*` and `rlm_heartbeat.*` to
    /// them, and their timers' fires reach them. Attached once; a second
    /// attach changes nothing.
    pub(crate) fn attach_jobs(&self, jobs: Arc<dyn SessionJobs>) {
        self.sessions.attach_jobs(jobs);
    }

    /// Keeps the kernel's timer and watch sources defined from now on in the
    /// database, so a runtime opened after this one runs them again for the
    /// subscriptions the bus keeps (the runtime daemon's). Once; a second
    /// call changes nothing.
    pub(crate) fn keep_kernel_sources(&self) {
        self.sessions.events().keep_sources();
    }

    /// Undefines the source `name` ([`Runtime::define_source`]), stopping
    /// it when it runs and removing a kept one from the database; a name
    /// not defined changes nothing.
    ///
    /// # Errors
    ///
    /// The database failed to store the change, or the runtime is shutting
    /// down.
    pub(crate) async fn undefine_source(&self, name: &str) -> anyhow::Result<()> {
        self.sources.undefine(name).await
    }

    /// Steers `row` onto open session `session`'s agent once its engine is
    /// built: a running turn reads it at its next step, and an empty input
    /// queued behind it runs it when none does.
    ///
    /// # Errors
    ///
    /// The session is not open, its engine could not be built, or the row
    /// has no loop form.
    pub(crate) async fn deliver_row(
        &self,
        session: &str,
        row: &pa_types::session::CustomMessage,
    ) -> anyhow::Result<()> {
        self.sessions.subagents().deliver_row_to(session, row).await
    }

    /// Adds `subscription` to the bus (`Bus::add_subscription`); a source it
    /// is the first to hear starts.
    ///
    /// # Errors
    ///
    /// A subscription with its id is already on the bus.
    pub fn add_subscription(
        &self,
        subscription: Subscription,
    ) -> Result<(), DuplicateSubscription> {
        self.bus.add_subscription(subscription)
    }

    /// Removes the subscription `id` (`Bus::remove_subscription`); a source
    /// it was the last to hear stops.
    #[must_use]
    pub fn remove_subscription(&self, id: &str) -> Option<Subscription> {
        self.bus.remove_subscription(id)
    }

    /// The subscriptions on the bus, in id order (`Bus::subscriptions`).
    #[must_use]
    pub fn subscriptions(&self) -> Vec<Subscription> {
        self.bus.subscriptions()
    }

    /// Defines a timer or folder-watch source, kept in the agent's
    /// database when its lifetime is `Kept`: it runs while a subscription's filter matches a topic it
    /// publishes on (`timer/<name>`, `source/<name>/changed` or
    /// `source/<name>/error`). Defining a name again the same way changes
    /// nothing.
    ///
    /// # Errors
    ///
    /// The name is not one topic level, the options are not the kind's, the
    /// kind is a webhook (webhook sources arrive with the daemon), the name
    /// is defined already another way, or the database failed to store it.
    pub async fn define_source(&self, definition: SourceDefinition) -> anyhow::Result<()> {
        self.sources.define(definition).await
    }

    /// The names of the sources running now.
    #[must_use]
    pub fn running_sources(&self) -> BTreeSet<String> {
        self.sources.running().borrow().clone()
    }

    /// The names of the sources running, as it changes: for tests to wait
    /// on.
    #[cfg(test)]
    pub(crate) fn running_sources_changes(&self) -> tokio::sync::watch::Receiver<BTreeSet<String>> {
        self.sources.running()
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
        let id = self.sessions.open(spec, Opening::Fresh)?;
        self.queue
            .restore(&id)
            .await
            .with_context(|| format!("restore the tasks of session {id}"))?;
        Ok(id)
    }

    /// Opens session `spec.id` again after the daemon died under it
    /// (`revival`): the death is recorded as its crash, its first lifetime
    /// rebuilds it from its transcript, and its tasks are restored; then the
    /// turn the daemon died under is run again as it was queued, continued
    /// behind the interrupted-turn notice (the same task, so its `done` ends
    /// the continued turn), left over as done, or settled as failed with the
    /// crash's reason, as `revival` says.
    ///
    /// # Errors
    ///
    /// As [`Runtime::create_session`]; or the database failed to save the
    /// interrupted task.
    pub(crate) async fn revive_session(
        &self,
        spec: SessionSpec,
        revival: crate::daemon::Revival,
    ) -> anyhow::Result<String> {
        use pa_core::session::revival::{InterruptedTurn, RevivedTurn, TURN_INTERRUPTED_NOTICE};
        use pa_types::event_log::{Task, TaskState};
        let reason = revival.rebuild.crash.reason();
        let id = self.sessions.revive(spec, revival.rebuild)?;
        self.queue
            .restore(&id)
            .await
            .with_context(|| format!("restore the tasks of session {id}"))?;
        let Some((task, evidence, turn)) = revival.running else {
            return Ok(id);
        };
        let queued = Task {
            state: TaskState::Queued,
            error: None,
            ..task
        };
        let ended = |state, error| Task {
            state,
            error,
            ..queued.clone()
        };
        match (turn, evidence) {
            (InterruptedTurn::Replay, Some(RevivedTurn::Unstarted) | None) => {
                self.queue.push(queued).await?;
            }
            (InterruptedTurn::Replay, Some(RevivedTurn::Interrupted)) => {
                self.queue
                    .push(Task {
                        prompt: TURN_INTERRUPTED_NOTICE.to_string(),
                        ..queued
                    })
                    .await?;
            }
            (InterruptedTurn::Replay, Some(RevivedTurn::Answered)) => {
                self.log
                    .save_task(ended(TaskState::Done, None))
                    .wait()
                    .await?;
                self.sessions
                    .report_end(&queued, crate::tasks::TaskOutcome::Done)?;
            }
            (InterruptedTurn::Settle, _) => {
                self.log
                    .save_task(ended(TaskState::Failed, Some(reason.clone())))
                    .wait()
                    .await?;
                self.sessions
                    .report_end(&queued, crate::tasks::TaskOutcome::Failed(reason))?;
            }
        }
        Ok(id)
    }

    /// The agent's database.
    pub(crate) fn log(&self) -> &Arc<pa_log::Log> {
        &self.log
    }

    /// Cancels `session`'s queued tasks and its running one, whose turn is
    /// aborted; each ends `Cancelled`.
    ///
    /// # Errors
    ///
    /// The database failed to save a cancelled task.
    pub async fn cancel(&self, session: &str) -> anyhow::Result<()> {
        self.queue.cancel_queued(session).await?;
        if let Some(running) = self.queue.lane(session).running {
            self.runner.cancel_running_task(session, &running.id);
        }
        Ok(())
    }

    /// Aborts `session`'s running turn (its task ends `Cancelled`) and puts
    /// the steers it took and had not read back at the front of its lane;
    /// the lane is then parked or kept going, as `after` says. How many
    /// steers went back.
    ///
    /// # Errors
    ///
    /// The database failed to save a steer put back.
    pub(crate) async fn abort(&self, session: &str, after: AfterAbort) -> anyhow::Result<usize> {
        match after {
            AfterAbort::Park => self.queue.suspend(session),
            AfterAbort::SendQueued => {
                self.queue.resume(session);
            }
        }
        let unread = self
            .queue
            .lane(session)
            .running
            .and_then(|running| self.runner.cancel_running_task(session, &running.id))
            .unwrap_or_default();
        let count = unread.len();
        for steer in unread {
            self.queue.push(steer).await?;
        }
        Ok(count)
    }

    /// Resumes `session`'s parked lane ([`AfterAbort::Park`]); whether it
    /// was parked.
    pub(crate) fn resume_queue(&self, session: &str) -> bool {
        self.queue.resume(session)
    }

    /// Withdraws `session`'s queued tasks (each ends `Cancelled`) and returns
    /// them in the order they would have run; the running one runs on.
    ///
    /// # Errors
    ///
    /// The database failed to save a withdrawn task.
    pub(crate) async fn withdraw(
        &self,
        session: &str,
    ) -> anyhow::Result<Vec<pa_types::event_log::Task>> {
        self.queue.cancel_queued(session).await
    }

    /// Holds `session`'s input under `hold`: nothing queued in its lane
    /// starts until every hold is released ([`crate::tasks::TaskQueue::hold`]);
    /// whether the hold is new.
    pub(crate) fn hold_input(&self, session: &str, hold: &str) -> bool {
        self.queue.hold(session, hold)
    }

    /// Releases `hold` on `session`'s input; whether it held it.
    pub(crate) fn release_input(&self, session: &str, hold: &str) -> bool {
        self.queue.release(session, hold)
    }

    /// Cancels `session`'s task `task`: taken out of the lane when it is
    /// queued, or its turn aborted when it runs (the steers it took and had
    /// not read go back to the front of the lane, which is neither parked nor
    /// resumed). Whether it was queued or running.
    ///
    /// # Errors
    ///
    /// The database failed to save the cancelled task or a steer put back.
    pub(crate) async fn cancel_task(&self, session: &str, task: &str) -> anyhow::Result<bool> {
        if self.queue.cancel_task(session, task).await?.is_some() {
            return Ok(true);
        }
        let runs = self
            .queue
            .lane(session)
            .running
            .is_some_and(|running| running.id == task);
        let Some(unread) = runs
            .then(|| self.runner.cancel_running_task(session, task))
            .flatten()
        else {
            return Ok(false);
        };
        for steer in unread {
            self.queue.push(steer).await?;
        }
        Ok(true)
    }

    /// What `session`'s lane holds now.
    pub(crate) fn lane(&self, session: &str) -> crate::tasks::LaneState {
        self.queue.lane(session)
    }

    /// Closes `session`: cancels its queued tasks and its running one, then
    /// closes it and waits until its thread has let go.
    ///
    /// # Errors
    ///
    /// The database failed to save a cancelled task; the session is closed
    /// all the same.
    pub(crate) async fn close_session(&self, session: &str) -> anyhow::Result<()> {
        let cancelled = self.cancel(session).await;
        self.sessions.close(session).await;
        self.sessions.subagents().forget_root(session).await;
        cancelled
    }

    /// Queues each payload it is given on `session`'s stream
    /// (`agent/<id>/stream`, live only) as the owner's, through the
    /// sessions' outbox: in order with what the session's turns stream.
    ///
    /// # Errors
    ///
    /// `session` cannot be a topic level.
    pub(crate) fn stream_sender(
        &self,
        session: &str,
    ) -> anyhow::Result<Arc<dyn Fn(serde_json::Value) + Send + Sync>> {
        let topic = crate::session::SessionTopics::of(session)?.stream;
        let outbox = self.sessions.outbox().clone();
        let id = session.to_string();
        Ok(Arc::new(move |payload| {
            outbox.send(NewEvent {
                topic: topic.clone(),
                source: id.clone(),
                session: Some(id.clone()),
                payload,
                trust: Trust::Owner,
                cause: None,
                reply: None,
            });
        }))
    }

    /// The engine of `session`'s current lifetime, once it is built.
    pub(crate) fn session_engine(&self, session: &str) -> Option<crate::session::LiveEngine> {
        self.sessions.host(session)?.engine()
    }

    /// The engine of open session `session`, once its first build is done:
    /// a client's change or read right after `session.create` waits for it.
    ///
    /// # Errors
    ///
    /// The session is not open, its engine could not be built, or it is
    /// being rebuilt after a crash.
    pub(crate) async fn built_engine(
        &self,
        session: &str,
    ) -> anyhow::Result<crate::session::LiveEngine> {
        self.sessions.subagents().built_node(session).await?;
        self.session_engine(session)
            .with_context(|| format!("session {session}'s engine is being rebuilt"))
    }

    /// A wire2 server whose clients read and publish on the runtime's bus,
    /// whose queries and calls `handlers` answers, and which records in
    /// `attached` which connections follow which sessions.
    pub(crate) fn wire_server<H: Handlers>(
        &self,
        handlers: H,
        config: WireConfig,
        attached: Arc<crate::wire::Attached>,
    ) -> WireServer<
        LogEventStore,
        LogStateStore,
        MemorySessionTree,
        NoModelCalls,
        RuntimeTaskSink<B>,
        H,
    > {
        WireServer::attached_to(self.bus.clone(), handlers, config, attached)
    }

    /// The sessions' family tree: the runtime daemon attaches its family
    /// records to it and its state queries read it.
    pub(crate) fn subagents(&self) -> Arc<crate::subagents::Subagents> {
        Arc::clone(self.sessions.subagents())
    }

    /// Waits until `session` has no task queued or running.
    pub async fn idle(&self, session: &str) {
        self.queue.session_idle(session).await;
    }

    /// Waits until `session` is at rest: no task queued or running, no row
    /// delivered to it waiting on its agent (the wake it queued has not
    /// reached its lane yet, [`crate::subagents::Subagents::rows_waiting`]),
    /// and every subagent below it settled
    /// ([`crate::subagents::Subagents::subtree_settled`]). A child's settle
    /// can queue work (the notice turn it wakes the session for), so the wait
    /// goes back to the lane until all hold at once. Answers at once for a
    /// session not open.
    pub(crate) async fn at_rest(&self, session: &str) {
        let subagents = self.sessions.subagents();
        loop {
            self.queue
                .session_idle_and(session, || !subagents.rows_waiting(session))
                .await;
            subagents.subtree_settled(session).await;
            let lane = self.queue.lane(session);
            if lane.running.is_none() && lane.queued.is_empty() && !subagents.rows_waiting(session)
            {
                return;
            }
        }
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

    /// Stops every source (storing each timer's last fire), closes every
    /// session (each finishes the task it is running) and stops publishing
    /// their events.
    pub async fn shutdown(self) {
        self.sources.shutdown().await;
        self.sessions.close_all().await;
        self.publisher.abort();
    }
}

/// Publishes the sessions' events in the order they were queued. A task's
/// `agent/<id>/done` first ends it in its lane, so a client that hears the
/// `done` and reads the lane does not find the task still running.
async fn publish_outgoing<B: SessionBuilder>(
    bus: RuntimeBus<B>,
    queue: TaskQueue<SessionRunner<B>, LogTaskStore>,
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
        if let Some(session) = event.session.as_deref() {
            let of = topic
                .as_str()
                .strip_prefix("agent/")
                .and_then(|rest| rest.strip_suffix("/done"));
            if of == Some(session) {
                if let Ok(done) = serde_json::from_value::<AgentDone>(event.payload.clone()) {
                    queue.reported(session, &done.task);
                }
            }
        }
        let result = bus.publish(event).await;
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
