//! A client's edit of one row of a session's queue view (the old daemon's
//! `mutate_queued_message`): the row is addressed by its lane and index in
//! the view and by the text the client saw there, and an edit that finds
//! another row there is rejected, not an error.
//!
//! A row is a task the lane queues, or a batch waiting on the agent (a steer
//! the running turn took, an agent message, a child's notice, a heartbeat's
//! row). Only a task is edited: a waiting batch is the running turn's, which
//! reads it at its next step (on the runtime a turn takes a steer at once,
//! where the old daemon's steering lane held it until then), so every edit
//! of one is rejected. The edits of a task:
//!
//! - **Delete.** It is cancelled out of the lane (it ends `Cancelled`).
//! - **Move.** It swaps places with the task `direction` rows away in its
//!   lane; a row there that is no task is not swapped with.
//! - **Replace.** Its prompt becomes the edit's text (its images too, when
//!   the edit carries `images`), in its place; moved to the other lane, it
//!   arrives there anew (a steer is offered to the running turn).
//!
//! An applied edit resumes the session's parked lane, as the old daemon's
//! resumes its queue.

use pa_types::ai::ImageContent;
use pa_types::event_log::Priority;

use super::Runtime;
use crate::session::{QueueLane, RowAt, SessionBuilder};

/// One edit of a queue view's row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum QueueEdit {
    Delete,
    Move {
        direction: i64,
    },
    Replace {
        text: String,
        /// The row's images from now on; `None` keeps them.
        images: Option<Vec<ImageContent>>,
        /// The lane the row moves to; `None` keeps it in its own.
        lane: Option<QueueLane>,
    },
}

/// Whether an edit was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edited {
    Applied,
    Rejected,
}

impl Edited {
    /// The old reply's `status`.
    pub(crate) fn status(self) -> &'static str {
        match self {
            Edited::Applied => "applied",
            Edited::Rejected => "rejected",
        }
    }
}

impl From<bool> for Edited {
    fn from(applied: bool) -> Self {
        if applied {
            Edited::Applied
        } else {
            Edited::Rejected
        }
    }
}

impl<B: SessionBuilder> Runtime<B> {
    /// Applies `edit` to the row at `index` of `lane` in open session
    /// `session`'s queue view, when that row shows `expected` (see the
    /// module documentation).
    ///
    /// # Errors
    ///
    /// The session is not open, or the database failed to save a task.
    pub(crate) async fn edit_queued(
        &self,
        session: &str,
        lane: QueueLane,
        index: usize,
        expected: &str,
        edit: QueueEdit,
    ) -> anyhow::Result<Edited> {
        let stream = self
            .sessions
            .subagents()
            .stream(session)
            .ok_or_else(|| anyhow::anyhow!("session {session} is not open"))?;
        let rows = stream.queue_rows();
        let rows = rows.lane(lane);
        let task_at = |at: usize| match rows.get(at).map(|row| &row.at) {
            Some(RowAt::Task(task)) => Some(task),
            Some(RowAt::Waiting) | None => None,
        };
        let Some(task) = task_at(index).filter(|_| rows[index].text == expected) else {
            return Ok(Edited::Rejected);
        };
        let edited: Edited = match edit {
            QueueEdit::Delete => self.queue.cancel_task(session, &task.id).await?.is_some(),
            QueueEdit::Move { direction } => i64::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(direction))
                .and_then(|target| usize::try_from(target).ok())
                .filter(|_| direction != 0)
                .and_then(task_at)
                .is_some_and(|other| self.queue.swap_queued(session, &task.id, &other.id)),
            QueueEdit::Replace {
                text,
                images,
                lane: target,
            } => {
                if let Some(images) = images {
                    self.sessions.set_images(task, images);
                }
                match target.filter(|target| *target != lane) {
                    None => {
                        self.queue
                            .edit_queued(session, &task.id, |task| task.prompt = text)
                            .await?
                    }
                    Some(target) => self
                        .queue
                        .requeue(session, &task.id, |task| {
                            task.prompt = text;
                            task.priority = match target {
                                QueueLane::Steering => Priority::Steer,
                                QueueLane::FollowUp => Priority::FollowUp,
                            };
                        })
                        .await?
                        .is_some(),
                }
            }
        }
        .into();
        if edited == Edited::Applied {
            self.queue.resume(session);
        }
        Ok(edited)
    }
}
