//! The one writer: a dedicated thread owns the read-write connection and runs
//! every change submitted to it.
//!
//! The thread commits in groups: it takes the first queued job and whatever
//! else is already queued behind it (up to [`MAX_BATCH`]) into one
//! transaction, so a burst of appends costs one commit and a lone append is
//! never held back waiting for company. Each job runs inside its own
//! savepoint, so one job's failure undoes only its own changes. A job's
//! [`Receipt`] resolves once its transaction has committed.

use std::future::Future;
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
use rusqlite::Connection;
use tokio::sync::oneshot;

/// The most jobs committed together.
const MAX_BATCH: usize = 256;

/// What runs once the job's transaction has committed (or failed to).
type Completion = Box<dyn FnOnce(Result<(), String>) + Send>;
type Job = Box<dyn FnOnce(&Connection) -> Completion + Send>;

/// The answer to one submitted change, once it is committed.
#[must_use = "a receipt says whether the change was committed"]
pub struct Receipt<T> {
    answer: oneshot::Receiver<Result<T>>,
}

impl<T> Receipt<T> {
    /// A receipt already answered, for a change refused before it reached the
    /// writer.
    pub(crate) fn ready(answer: Result<T>) -> Self {
        let (answer_to, receipt) = oneshot::channel();
        // The receiver is held right here, so the send cannot fail.
        let _ = answer_to.send(answer);
        Receipt { answer: receipt }
    }

    /// Wait for the change to commit.
    ///
    /// # Errors
    ///
    /// The change failed, its transaction did not commit, or the writer is gone.
    pub async fn wait(self) -> Result<T> {
        self.answer
            .await
            .map_err(|_| anyhow!("the database writer stopped before answering"))?
    }

    /// Wait for the change to commit, blocking this thread (as a file write
    /// would: on a runtime, call it from a blocking thread). Unlike tokio's
    /// own blocking receive, it never panics on a runtime's thread, and the
    /// calling task's spent coop budget never holds the answer back.
    ///
    /// # Errors
    ///
    /// The change failed, its transaction did not commit, or the writer is gone.
    pub fn wait_blocking(self) -> Result<T> {
        /// Wakes the waiting thread.
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: std::sync::Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = std::task::Waker::from(std::sync::Arc::new(Unpark(std::thread::current())));
        let mut context = std::task::Context::from_waker(&waker);
        // Inside a task that has spent its coop budget, tokio's receive
        // answers `Pending` and wakes at once until the task yields, which a
        // blocked thread never does: the wait would spin forever on an
        // answer already sent. The budget is the task's, not this wait's.
        let mut answer = tokio::task::unconstrained(self.answer);
        loop {
            match std::pin::Pin::new(&mut answer).poll(&mut context) {
                std::task::Poll::Ready(answer) => {
                    return answer
                        .map_err(|_| anyhow!("the database writer stopped before answering"))?;
                }
                std::task::Poll::Pending => std::thread::park(),
            }
        }
    }
}

pub(crate) struct Writer {
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    /// Start the writer thread over `connection`.
    pub(crate) fn start(connection: Connection) -> Result<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("pa-log-writer".to_string())
            .spawn(move || {
                while let Ok(first) = queue.recv() {
                    let mut batch = vec![first];
                    while batch.len() < MAX_BATCH {
                        match queue.try_recv() {
                            Ok(job) => batch.push(job),
                            Err(_) => break,
                        }
                    }
                    // Without the group's transaction each job's savepoint is
                    // a transaction of its own, committed by its release.
                    let grouped = connection.execute_batch("BEGIN IMMEDIATE").is_ok();
                    let completions: Vec<Completion> =
                        batch.into_iter().map(|job| job(&connection)).collect();
                    let committed = if grouped {
                        connection.execute_batch("COMMIT").map_err(|error| {
                            // A failed commit leaves the transaction open; the
                            // rollback's own failure would only repeat it.
                            let rolled_back = connection.execute_batch("ROLLBACK");
                            match rolled_back {
                                Ok(()) => error.to_string(),
                                Err(rollback) => format!("{error}; rollback: {rollback}"),
                            }
                        })
                    } else {
                        Ok(())
                    };
                    for completion in completions {
                        completion(committed.clone());
                    }
                }
            })?;
        Ok(Writer {
            jobs: Some(jobs),
            thread: Some(thread),
        })
    }

    /// Run `work` on the writer's connection, inside its own savepoint of
    /// the next committed transaction.
    pub(crate) fn submit<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Receipt<T> {
        let (answer_to, answer) = oneshot::channel();
        let job: Job = Box::new(move |connection: &Connection| -> Completion {
            let result = connection
                .execute_batch("SAVEPOINT job")
                .map_err(anyhow::Error::from)
                .and_then(|()| {
                    let result = work(connection);
                    let close = if result.is_ok() {
                        "RELEASE job"
                    } else {
                        "ROLLBACK TO job; RELEASE job"
                    };
                    connection.execute_batch(close)?;
                    result
                });
            Box::new(move |committed: Result<(), String>| {
                let answer = match committed {
                    Ok(()) => result,
                    Err(error) => Err(anyhow!("the transaction did not commit: {error}")),
                };
                // The caller may have stopped waiting; the change stands.
                let _ = answer_to.send(answer);
            })
        });
        if let Some(jobs) = &self.jobs {
            // A send fails only once the thread is gone, which drops the job
            // and so closes the receipt with the writer-stopped error.
            let _ = jobs.send(job);
        }
        Receipt { answer }
    }
}

impl Drop for Writer {
    /// Run every queued change before the writer goes.
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
