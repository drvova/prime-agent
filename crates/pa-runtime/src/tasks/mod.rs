//! Task lanes: one serial lane per session, sessions in parallel, and at
//! most `max_concurrent_runs` tasks running at once across all of them.
//!
//! A lane runs its highest-priority task first (the lowest [`Priority`]),
//! then the oldest. It picks its next task only once it holds a run permit,
//! so a task queued while the lane waits for one still runs in its place.
//! A task goes Queued → Running → Done, Failed or Cancelled, and the queue
//! saves each state to its [`TaskStore`] before acting on it.
//!
//! A `Steer` task queued while its session runs a task is first offered to
//! the running one ([`TaskRunner::steer`]); taken, it is Done without a run
//! of its own, and declined, it waits in the lane and runs next. A `Steer`
//! task that runs as its own is offered the `Steer` tasks queued behind it as
//! it starts, so steers that waited together are delivered together (the
//! session's steering mode decides whether its turn reads them as one batch
//! or one per step).
//!
//! A lane can be suspended ([`TaskQueue::suspend`]): its queued tasks stay
//! queued and none starts until it is resumed; the running one runs on. It
//! can also be held ([`TaskQueue::hold`]) by any number of named holds, apart
//! from its suspension: it starts nothing while it is suspended or any hold
//! remains, so resuming a held lane leaves it held.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use pa_types::event_log::{Admission, Priority, Task, TaskState};
use tokio::sync::{watch, Semaphore};

use crate::lock;

mod store;

pub use store::{MemoryTaskStore, TaskStore};

/// The error a task running when the runtime stopped ends with, once the
/// queue is restored.
pub const INTERRUPTED_BY_RESTART: &str = "interrupted by restart";

/// How a task's run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    Done,
    Failed(String),
    Cancelled,
}

/// What a running task did with a `Steer` task offered to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Steered {
    /// The running turn will read it: it needs no run of its own.
    Taken,
    /// The running turn is past its last step: it runs as its own task.
    Declined,
}

/// What a session's lane holds now ([`TaskQueue::lane`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LaneState {
    pub(crate) running: Option<Task>,
    /// In the order they would run.
    pub(crate) queued: Vec<Task>,
    pub(crate) suspended: bool,
    /// How many holds stop it ([`TaskQueue::hold`]).
    pub(crate) holds: usize,
}

/// Runs a session's tasks; the later turn runner implements it.
///
/// `run` runs one task to its end and says how it ended. The queue runs it
/// on a Tokio task of its own, so a panic in it fails that task only. The
/// queue never runs two tasks of one session at once.
///
/// `steer` is offered a `Steer` task queued while `running` runs, and the
/// `Steer` tasks queued behind a `Steer` task `running` as it starts, before
/// its `run` is called (the runner keeps them for that run). It must
/// not block or call into the queue: it hands the task's prompt to the
/// running turn (which reads it at its next step) and answers
/// [`Steered::Taken`], or answers [`Steered::Declined`] when the turn will
/// not read it, including when `running` has already ended. A taken task is
/// the running turn's to honour; the queue marks it Done.
///
/// `cancelled` is told of each task the queue cancelled before it ran (taken
/// out of its lane and saved `Cancelled`), which `run` is never called for:
/// it reports that end as a run would have, and must not block or call into
/// the queue.
pub trait TaskRunner: Send + Sync + 'static {
    /// Runs `task`.
    fn run(&self, task: Task) -> impl Future<Output = TaskOutcome> + Send;

    /// Offers the `Steer` task `steer` to the running `running`.
    fn steer(&self, running: &Task, steer: &Task) -> Steered;

    /// Reports the end of `task`, cancelled while it was queued.
    fn cancelled(&self, task: &Task);
}

/// The task queue; clones share it.
pub struct TaskQueue<R, S> {
    inner: Arc<Inner<R, S>>,
}

impl<R, S> Clone for TaskQueue<R, S> {
    fn clone(&self) -> Self {
        TaskQueue {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<R, S> {
    runner: Arc<R>,
    store: S,
    permits: Semaphore,
    lanes: Mutex<Lanes>,
    /// How many tasks are queued or running.
    busy: watch::Sender<usize>,
}

#[derive(Default)]
struct Lanes {
    by_session: HashMap<String, Lane>,
    next_order: u64,
    /// The sessions whose lanes start no task until they are resumed.
    suspended: HashSet<String>,
    /// The holds on each session's lane: it starts no task while one remains.
    holds: HashMap<String, BTreeSet<String>>,
}

impl Lanes {
    /// Whether `session`'s lane starts nothing now: suspended or held.
    fn stopped(&self, session: &str) -> bool {
        self.suspended.contains(session)
            || self
                .holds
                .get(session)
                .is_some_and(|holds| !holds.is_empty())
    }

    fn busy(&self) -> usize {
        self.by_session
            .values()
            .map(|lane| lane.queued.len() + usize::from(lane.running.is_some()))
            .sum()
    }
}

#[derive(Default)]
struct Lane {
    queued: BTreeMap<(Priority, u64), Task>,
    running: Option<Task>,
    /// The task whose `done` is being published: its end is visible, so the
    /// lane no longer reports it running, though the queue still saves its
    /// end before it starts the next one.
    reported: Option<String>,
    /// Whether a worker task serves this lane.
    served: bool,
}

impl<R: TaskRunner, S: TaskStore> TaskQueue<R, S> {
    /// An empty queue running at most `max_concurrent_runs` tasks at once.
    #[must_use]
    pub fn new(runner: R, store: S, max_concurrent_runs: NonZeroUsize) -> Self {
        TaskQueue {
            inner: Arc::new(Inner {
                runner: Arc::new(runner),
                store,
                permits: Semaphore::new(max_concurrent_runs.get().min(Semaphore::MAX_PERMITS)),
                lanes: Mutex::new(Lanes::default()),
                busy: watch::Sender::new(0),
            }),
        }
    }

    /// Restores `session`'s lane as the store left it: its queued tasks
    /// queued again in their order, and each task that was running Failed
    /// with [`INTERRUPTED_BY_RESTART`], since it may have done part of its
    /// work. Called once, as the session opens, by whoever holds the
    /// session: no other session's tasks are read or touched, since another
    /// process on the same store may be running them.
    ///
    /// # Errors
    ///
    /// The store failed to load the tasks or to save an interrupted one.
    pub async fn restore(&self, session: &str) -> anyhow::Result<()> {
        for task in self.inner.store.unfinished(session).await? {
            match task.state {
                TaskState::Running => {
                    let failed = Task {
                        state: TaskState::Failed,
                        error: Some(INTERRUPTED_BY_RESTART.to_string()),
                        ..task
                    };
                    self.inner.store.save(failed).await?;
                }
                TaskState::Queued => {
                    self.enqueue(task);
                }
                TaskState::Done | TaskState::Failed | TaskState::Cancelled => {}
            }
        }
        Ok(())
    }

    /// Queues `task` in its session's lane, whatever its state says (it
    /// becomes Queued), or hands it to the running task when it is a taken
    /// `Steer`; says which.
    ///
    /// # Errors
    ///
    /// The store failed to save it.
    pub async fn push(&self, task: Task) -> anyhow::Result<Admission> {
        let task = Task {
            state: TaskState::Queued,
            ..task
        };
        self.inner.store.save(task.clone()).await?;
        if task.priority == Priority::Steer {
            let running = lock(&self.inner.lanes)
                .by_session
                .get(&task.session)
                .and_then(|lane| lane.running.clone());
            if let Some(running) = running {
                match self.inner.runner.steer(&running, &task) {
                    Steered::Taken => {
                        let taken = Task {
                            state: TaskState::Done,
                            ..task
                        };
                        self.inner.store.save(taken).await?;
                        return Ok(Admission::Steered);
                    }
                    Steered::Declined => {}
                }
            }
        }
        Ok(self.enqueue(task))
    }

    /// Suspends `session`'s lane: its queued tasks, and those queued after,
    /// wait until [`TaskQueue::resume`]; a running one runs on.
    pub(crate) fn suspend(&self, session: &str) {
        lock(&self.inner.lanes)
            .suspended
            .insert(session.to_string());
    }

    /// Resumes `session`'s lane, which starts its queued tasks again; whether
    /// it was suspended.
    pub(crate) fn resume(&self, session: &str) -> bool {
        let was = lock(&self.inner.lanes).suspended.remove(session);
        self.restart(session);
        was
    }

    /// Holds `session`'s lane under `hold`: its queued tasks, and those
    /// queued after, wait until every hold on it is released (and it is not
    /// suspended); a running one runs on. Whether the hold is new.
    pub(crate) fn hold(&self, session: &str, hold: &str) -> bool {
        lock(&self.inner.lanes)
            .holds
            .entry(session.to_string())
            .or_default()
            .insert(hold.to_string())
    }

    /// Releases `hold` on `session`'s lane, which starts its queued tasks
    /// again once it is neither held nor suspended; whether it held it.
    pub(crate) fn release(&self, session: &str, hold: &str) -> bool {
        let released = {
            let mut lanes = lock(&self.inner.lanes);
            let released = lanes
                .holds
                .get_mut(session)
                .is_some_and(|holds| holds.remove(hold));
            if lanes.holds.get(session).is_some_and(BTreeSet::is_empty) {
                lanes.holds.remove(session);
            }
            released
        };
        self.restart(session);
        released
    }

    /// Starts `session`'s lane worker when the lane has queued tasks, no
    /// worker and nothing stopping it.
    fn restart(&self, session: &str) {
        let start = {
            let mut lanes = lock(&self.inner.lanes);
            let stopped = lanes.stopped(session);
            lanes.by_session.get_mut(session).is_some_and(|lane| {
                let start = !stopped && !lane.served && !lane.queued.is_empty();
                lane.served |= start;
                start
            })
        };
        if start {
            tokio::spawn(serve(Arc::clone(&self.inner), session.to_string()));
        }
    }

    /// Cancels task `id` of `session`'s lane: taken out of the lane when it
    /// is queued (it ends `Cancelled`, and is returned), or `None` when the
    /// lane does not queue it (it runs, ended, or was never queued here).
    ///
    /// # Errors
    ///
    /// The store failed to save the cancelled task.
    pub(crate) async fn cancel_task(
        &self,
        session: &str,
        id: &str,
    ) -> anyhow::Result<Option<Task>> {
        let queued = {
            let mut lanes = lock(&self.inner.lanes);
            let taken = lanes.by_session.get_mut(session).and_then(|lane| {
                let key = lane
                    .queued
                    .iter()
                    .find(|(_, task)| task.id == id)
                    .map(|(key, _)| *key)?;
                lane.queued.remove(&key)
            });
            // A lane no worker serves (a stopped one) is left empty: gone,
            // so the session reads idle.
            if lanes.by_session.get(session).is_some_and(|lane| {
                !lane.served && lane.running.is_none() && lane.queued.is_empty()
            }) {
                lanes.by_session.remove(session);
            }
            self.inner.busy.send_replace(lanes.busy());
            taken
        };
        let Some(task) = queued else {
            return Ok(None);
        };
        let task = Task {
            state: TaskState::Cancelled,
            ..task
        };
        self.inner.store.save(task.clone()).await?;
        self.inner.runner.cancelled(&task);
        Ok(Some(task))
    }

    /// Marks `session`'s running task `task` as ended, ahead of its `done`
    /// being published: whoever hears the `done` and then reads the lane no
    /// longer finds it running.
    pub(crate) fn reported(&self, session: &str, task: &str) {
        let mut lanes = lock(&self.inner.lanes);
        if let Some(lane) = lanes.by_session.get_mut(session) {
            lane.reported = Some(task.to_string());
        }
    }

    /// What `session`'s lane holds now.
    pub(crate) fn lane(&self, session: &str) -> LaneState {
        let lanes = lock(&self.inner.lanes);
        let lane = lanes.by_session.get(session);
        LaneState {
            running: lane
                .and_then(|lane| lane.running.clone())
                .filter(|running| {
                    lane.and_then(|lane| lane.reported.as_deref()) != Some(&running.id)
                }),
            queued: lane.map_or_default(|lane| lane.queued.values().cloned().collect()),
            suspended: lanes.suspended.contains(session),
            holds: lanes.holds.get(session).map_or(0, BTreeSet::len),
        }
    }

    /// Cancels every task queued in `session`'s lane; a running one runs on.
    /// The cancelled tasks, in the order they would have run.
    ///
    /// # Errors
    ///
    /// The store failed to save one; the ones before it are saved.
    pub async fn cancel_queued(&self, session: &str) -> anyhow::Result<Vec<Task>> {
        let queued = {
            let mut lanes = lock(&self.inner.lanes);
            let queued = lanes
                .by_session
                .get_mut(session)
                .map_or_default(|lane| std::mem::take(&mut lane.queued));
            // A lane no worker serves (a parked or held one) is left empty:
            // gone, so the session reads idle (as `cancel_task` leaves it).
            if lanes
                .by_session
                .get(session)
                .is_some_and(|lane| !lane.served && lane.running.is_none())
            {
                lanes.by_session.remove(session);
            }
            self.inner.busy.send_replace(lanes.busy());
            queued
        };
        let mut cancelled = Vec::with_capacity(queued.len());
        for task in queued.into_values() {
            let task = Task {
                state: TaskState::Cancelled,
                ..task
            };
            self.inner.store.save(task.clone()).await?;
            self.inner.runner.cancelled(&task);
            cancelled.push(task);
        }
        Ok(cancelled)
    }

    /// Waits until no task is queued or running.
    pub async fn idle(&self) {
        let mut busy = self.inner.busy.subscribe();
        // The sender lives in `self`, so the wait cannot see it closed.
        let _ = busy.wait_for(|busy| *busy == 0).await;
    }

    /// Waits until no task of `session` is queued or running.
    pub async fn session_idle(&self, session: &str) {
        self.session_idle_and(session, || true).await;
    }

    /// Waits until no task of `session` is queued or running and `also`
    /// holds, read again on every change to a lane: `also` must turn true
    /// on its own or by work that changes a lane.
    pub(crate) async fn session_idle_and(&self, session: &str, also: impl Fn() -> bool) {
        // Subscribed before the lanes are read: every change to a lane after
        // the read republishes the busy count and wakes the wait. The lanes
        // are not read inside the watch's lock, which their writers take
        // while holding the lanes.
        let mut busy = self.inner.busy.subscribe();
        loop {
            // `also` is read with the lanes unlocked.
            let in_lane = lock(&self.inner.lanes).by_session.contains_key(session);
            if !in_lane && also() {
                return;
            }
            // The sender lives in `self`, so the wait cannot see it closed.
            let _ = busy.changed().await;
        }
    }

    /// Queues `task` in its lane, starting the lane's worker unless one
    /// serves it or the lane is suspended.
    fn enqueue(&self, task: Task) -> Admission {
        let session = task.session.clone();
        let (start, admitted) = {
            let mut lanes = lock(&self.inner.lanes);
            let order = lanes.next_order;
            lanes.next_order += 1;
            let suspended = lanes.stopped(&session);
            let lane = lanes.by_session.entry(session.clone()).or_default();
            lane.queued.insert((task.priority, order), task);
            let start = !lane.served && !suspended;
            lane.served |= start;
            self.inner.busy.send_replace(lanes.busy());
            let admitted = if suspended {
                Admission::Parked
            } else {
                Admission::Queued
            };
            (start, admitted)
        };
        if start {
            tokio::spawn(serve(Arc::clone(&self.inner), session));
        }
        admitted
    }
}

/// Runs `session`'s lane until it is empty.
async fn serve<R: TaskRunner, S: TaskStore>(inner: Arc<Inner<R, S>>, session: String) {
    loop {
        // The semaphore is never closed, so the permit always comes.
        let Ok(_permit) = inner.permits.acquire().await else {
            return;
        };
        let (task, behind) = {
            let mut lanes = lock(&inner.lanes);
            let suspended = lanes.stopped(&session);
            let Some(lane) = lanes.by_session.get_mut(&session) else {
                return;
            };
            if lane.queued.is_empty() {
                lanes.by_session.remove(&session);
                inner.busy.send_replace(lanes.busy());
                return;
            }
            if suspended {
                // Resuming the lane starts a worker for it again.
                lane.served = false;
                return;
            }
            let Some((_, task)) = lane.queued.pop_first() else {
                return;
            };
            let task = Task {
                state: TaskState::Running,
                attempts: task.attempts + 1,
                ..task
            };
            // The steers that waited behind a steer go with it.
            let mut behind = Vec::new();
            while task.priority == Priority::Steer
                && lane
                    .queued
                    .first_key_value()
                    .is_some_and(|((priority, _), _)| *priority == Priority::Steer)
            {
                behind.extend(lane.queued.pop_first());
            }
            lane.running = Some(task.clone());
            (task, behind)
        };
        if let Err(error) = inner.store.save(task.clone()).await {
            tracing::error!(
                "task {:?} runs without its Running state saved: {error:#}",
                task.id
            );
        }
        let mut declined = Vec::new();
        for (key, steer) in behind {
            match inner.runner.steer(&task, &steer) {
                Steered::Taken => {
                    let id = steer.id.clone();
                    let taken = Task {
                        state: TaskState::Done,
                        ..steer
                    };
                    if let Err(error) = inner.store.save(taken).await {
                        tracing::error!(
                            "task {id:?} was delivered without its state saved: {error:#}"
                        );
                    }
                }
                Steered::Declined => declined.push((key, steer)),
            }
        }
        if !declined.is_empty() {
            let mut lanes = lock(&inner.lanes);
            if let Some(lane) = lanes.by_session.get_mut(&session) {
                lane.queued.extend(declined);
            }
            inner.busy.send_replace(lanes.busy());
        }
        let runner = Arc::clone(&inner.runner);
        let run = task.clone();
        let outcome = match tokio::spawn(async move { runner.run(run).await }).await {
            Ok(outcome) => outcome,
            Err(error) => TaskOutcome::Failed(format!("the task's run ended abnormally: {error}")),
        };
        let (state, error) = match outcome {
            TaskOutcome::Done => (TaskState::Done, None),
            TaskOutcome::Failed(error) => (TaskState::Failed, Some(error)),
            TaskOutcome::Cancelled => (TaskState::Cancelled, None),
        };
        let ended = Task {
            state,
            error,
            ..task
        };
        if let Err(error) = inner.store.save(ended.clone()).await {
            tracing::error!(
                "task {:?} ended without its state saved: {error:#}",
                ended.id
            );
        }
        let mut lanes = lock(&inner.lanes);
        if let Some(lane) = lanes.by_session.get_mut(&session) {
            lane.running = None;
        }
        inner.busy.send_replace(lanes.busy());
    }
}

#[cfg(test)]
mod tests;
