//! A client's hold on a session's input and its cancel of one prompt:
//! `session.pause_input`, `session.release_input` and `session.cancel_task`,
//! the old daemon's `acquire_session_input_pause`,
//! `release_session_input_pause` and `cancel_prompt_admission` contracts.
//!
//! - **Input holds.** `session.pause_input {session, key}` holds the open
//!   session's lane for the calling connection: nothing queued starts (input
//!   published meanwhile is admitted `parked`) until every hold on it is
//!   released; the running turn runs on. The same connection asking again
//!   with the same key gets the same `pauseId` back. A hold is independent of
//!   the lane's park (`session.abort`, `session.resume_queue`): resuming a
//!   held lane leaves it held. `session.release_input {session, pauseId}`
//!   releases one; the connection's end releases all of its holds, and a
//!   session stopped releases every hold on it, so no client that is gone
//!   keeps a session from running.
//! - **Cancel one task.** `session.cancel_task {session, task}` takes the task
//!   (the `task` a publish on `agent/<id>/input` answered) out of the lane
//!   when it is queued, or aborts its turn when it runs (the steers it took
//!   and had not read go back to the front of the lane); the lane's park and
//!   holds are left as they are. `cancelled` says whether it was queued or
//!   running.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::{string_arg, RuntimeHandlers};
use crate::lock;
use crate::session::SessionBuilder;
use crate::wire::ConnectionId;

/// One connection's hold on a session's input.
struct InputHold {
    session: String,
    key: String,
    connection: ConnectionId,
}

/// The input holds clients' connections keep, by pause id.
#[derive(Default)]
pub(super) struct InputHolds {
    holds: Mutex<HashMap<String, InputHold>>,
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) fn pause_input(
        &self,
        args: &Value,
        connection: ConnectionId,
    ) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let key = string_arg(args, "key")?;
        let mut holds = lock(&self.holds.holds);
        let existing = holds.iter().find_map(|(pause, hold)| {
            (hold.session == id && hold.key == key && hold.connection == connection)
                .then(|| pause.clone())
        });
        let pause = existing.unwrap_or_else(|| {
            let pause = uuid::Uuid::new_v4().to_string();
            self.runtime.hold_input(&id, &pause);
            holds.insert(
                pause.clone(),
                InputHold {
                    session: id.clone(),
                    key,
                    connection,
                },
            );
            pause
        });
        Ok(json!({ "session": id, "pauseId": pause }))
    }

    pub(super) fn release_input(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        let pause = string_arg(args, "pauseId")?;
        let held = {
            let mut holds = lock(&self.holds.holds);
            match holds.get(&pause) {
                Some(hold) if hold.session == id => holds.remove(&pause),
                Some(_) | None => None,
            }
        };
        let released = held.is_some() && self.runtime.release_input(&id, &pause);
        Ok(json!({ "session": id, "released": released }))
    }

    pub(super) async fn cancel_task(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let task = string_arg(args, "task")?;
        let cancelled = self.runtime.cancel_task(&id, &task).await?;
        Ok(json!({ "session": id, "task": task, "cancelled": cancelled }))
    }

    /// Releases every hold `connection` keeps.
    pub(super) fn release_connection_holds(&self, connection: ConnectionId) {
        self.release_holds(|hold| hold.connection == connection);
    }

    /// Releases every hold on session `id`.
    pub(super) fn release_session_holds(&self, id: &str) {
        self.release_holds(|hold| hold.session == id);
    }

    fn release_holds(&self, matches: impl Fn(&InputHold) -> bool) {
        let released: Vec<(String, InputHold)> = {
            let mut holds = lock(&self.holds.holds);
            let pauses: Vec<String> = holds
                .iter()
                .filter(|(_, hold)| matches(hold))
                .map(|(pause, _)| pause.clone())
                .collect();
            pauses
                .into_iter()
                .filter_map(|pause| holds.remove_entry(&pause))
                .collect()
        };
        for (pause, hold) in released {
            self.runtime.release_input(&hold.session, &pause);
        }
    }
}
