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
//! of its own, and declined, it waits in the lane and runs next.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use pa_types::event_log::{Priority, Task, TaskState};
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

/// Runs a session's tasks; the later turn runner implements it.
///
/// `run` runs one task to its end and says how it ended. The queue runs it
/// on a Tokio task of its own, so a panic in it fails that task only. The
/// queue never runs two tasks of one session at once.
///
/// `steer` is offered a `Steer` task queued while `running` runs. It must
/// not block or call into the queue: it hands the task's prompt to the
/// running turn (which reads it at its next step) and answers
/// [`Steered::Taken`], or answers [`Steered::Declined`] when the turn will
/// not read it, including when `running` has already ended. A taken task is
/// the running turn's to honour; the queue marks it Done.
pub trait TaskRunner: Send + Sync + 'static {
    /// Runs `task`.
    fn run(&self, task: Task) -> impl Future<Output = TaskOutcome> + Send;

    /// Offers the `Steer` task `steer` to the running `running`.
    fn steer(&self, running: &Task, steer: &Task) -> Steered;
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
}

impl Lanes {
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

    /// The queue as `store` left it: its queued tasks queued again in their
    /// order, and each task that was running Failed with
    /// [`INTERRUPTED_BY_RESTART`], since it may have done part of its work.
    ///
    /// # Errors
    ///
    /// The store failed to load the tasks or to save an interrupted one.
    pub async fn restore(
        runner: R,
        store: S,
        max_concurrent_runs: NonZeroUsize,
    ) -> anyhow::Result<Self> {
        let unfinished = store.unfinished().await?;
        let queue = TaskQueue::new(runner, store, max_concurrent_runs);
        for task in unfinished {
            match task.state {
                TaskState::Running => {
                    let failed = Task {
                        state: TaskState::Failed,
                        error: Some(INTERRUPTED_BY_RESTART.to_string()),
                        ..task
                    };
                    queue.inner.store.save(failed).await?;
                }
                TaskState::Queued => queue.enqueue(task),
                TaskState::Done | TaskState::Failed | TaskState::Cancelled => {}
            }
        }
        Ok(queue)
    }

    /// Queues `task` in its session's lane, whatever its state says (it
    /// becomes Queued), or hands it to the running task when it is a taken
    /// `Steer`.
    ///
    /// # Errors
    ///
    /// The store failed to save it.
    pub async fn push(&self, task: Task) -> anyhow::Result<()> {
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
                        return self.inner.store.save(taken).await;
                    }
                    Steered::Declined => {}
                }
            }
        }
        self.enqueue(task);
        Ok(())
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
                .map(|lane| std::mem::take(&mut lane.queued))
                .unwrap_or_default();
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

    fn enqueue(&self, task: Task) {
        let session = task.session.clone();
        let start = {
            let mut lanes = lock(&self.inner.lanes);
            let order = lanes.next_order;
            lanes.next_order += 1;
            let lane = lanes.by_session.entry(session.clone()).or_default();
            lane.queued.insert((task.priority, order), task);
            let start = !lane.served;
            lane.served = true;
            self.inner.busy.send_replace(lanes.busy());
            start
        };
        if start {
            tokio::spawn(serve(Arc::clone(&self.inner), session));
        }
    }
}

/// Runs `session`'s lane until it is empty.
async fn serve<R: TaskRunner, S: TaskStore>(inner: Arc<Inner<R, S>>, session: String) {
    loop {
        // The semaphore is never closed, so the permit always comes.
        let Ok(_permit) = inner.permits.acquire().await else {
            return;
        };
        let task = {
            let mut lanes = lock(&inner.lanes);
            let Some(lane) = lanes.by_session.get_mut(&session) else {
                return;
            };
            let Some((_, task)) = lane.queued.pop_first() else {
                lanes.by_session.remove(&session);
                inner.busy.send_replace(lanes.busy());
                return;
            };
            let task = Task {
                state: TaskState::Running,
                attempts: task.attempts + 1,
                ..task
            };
            lane.running = Some(task.clone());
            task
        };
        if let Err(error) = inner.store.save(task.clone()).await {
            tracing::error!(
                "task {:?} runs without its Running state saved: {error:#}",
                task.id
            );
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
