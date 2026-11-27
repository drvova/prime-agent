//! Where the task queue keeps its tasks, so queued work survives a restart.

use std::future::Future;
use std::sync::{Arc, Mutex};

use pa_types::event_log::{Task, TaskState};

use crate::lock;

/// The task queue's storage.
///
/// `save` stores the task, replacing the one with its id; the first save of
/// an id fixes its place in insertion order, and a later save keeps it.
/// The queue saves a task when it is queued, when it starts running, and
/// when it ends, and awaits each save before it goes on. `unfinished`
/// returns every task whose state is `Queued` or `Running`, in insertion
/// order: what [`TaskQueue::restore`](crate::TaskQueue::restore) starts
/// from after a restart.
pub trait TaskStore: Send + Sync + 'static {
    /// Stores `task`.
    fn save(&self, task: Task) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// The queued and running tasks, oldest first.
    fn unfinished(&self) -> impl Future<Output = anyhow::Result<Vec<Task>>> + Send;
}

/// Tasks in memory, for tests and headless runs. Clones share the same
/// tasks.
#[derive(Debug, Clone, Default)]
pub struct MemoryTaskStore {
    tasks: Arc<Mutex<Vec<Task>>>,
}

impl MemoryTaskStore {
    /// Every task saved, in insertion order.
    #[must_use]
    pub fn tasks(&self) -> Vec<Task> {
        lock(&self.tasks).clone()
    }
}

impl TaskStore for MemoryTaskStore {
    fn save(&self, task: Task) -> impl Future<Output = anyhow::Result<()>> + Send {
        let mut tasks = lock(&self.tasks);
        match tasks.iter_mut().find(|held| held.id == task.id) {
            Some(held) => *held = task,
            None => tasks.push(task),
        }
        std::future::ready(Ok(()))
    }

    fn unfinished(&self) -> impl Future<Output = anyhow::Result<Vec<Task>>> + Send {
        let unfinished = lock(&self.tasks)
            .iter()
            .filter(|task| match task.state {
                TaskState::Queued | TaskState::Running => true,
                TaskState::Done | TaskState::Failed | TaskState::Cancelled => false,
            })
            .cloned()
            .collect();
        std::future::ready(Ok(unfinished))
    }
}
