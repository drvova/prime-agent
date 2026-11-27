//! The receiving side of `agent_message.wait`: the session's parked waiters, its `waitingOn`
//! report, and the backlog of direct messages that arrived while the session was busy and are
//! still queued. The worker's delivery path reports every accepted delivery here; the
//! `agent_message.wait` controller parks on it. A waiter holds no admission slot and no lock: it
//! is a oneshot receiver.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pa_core::session_engine::agent_messaging::AgentMessageKind;
use tokio::sync::oneshot;

/// One accepted delivery, as a wait sees it: identity and a sanitized preview, never the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageArrival {
    pub(crate) message_id: String,
    /// The sender's live and durable session ids (the forms a wait filter matches).
    pub(crate) sender_ids: Vec<String>,
    pub(crate) sender_name: String,
    pub(crate) preview: String,
    pub(crate) kind: AgentMessageKind,
}

/// The senders one wait accepts: `None` accepts any direct message.
pub(crate) type AgentMessageWaitSenders = Option<Vec<String>>;

fn accepts(senders: &AgentMessageWaitSenders, arrival: &AgentMessageArrival) -> bool {
    arrival.kind == AgentMessageKind::Direct
        && senders.as_ref().is_none_or(|senders| {
            arrival
                .sender_ids
                .iter()
                .any(|id| senders.iter().any(|sender| sender == id))
        })
}

/// The backlog bound: the worker's pending-message cap.
const BACKLOG_LIMIT: usize =
    pa_core::session_engine::agent_messaging::DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION;

struct Waiter {
    id: u64,
    senders: AgentMessageWaitSenders,
    waiting_on: Option<String>,
    reply: oneshot::Sender<AgentMessageArrival>,
}

#[derive(Default)]
struct WaitState {
    next_id: u64,
    waiters: Vec<Waiter>,
    /// Direct messages no wait has claimed yet, oldest first.
    backlog: VecDeque<AgentMessageArrival>,
}

/// Hears a `waitingOn` change (the worker pushes a roster delta).
pub(crate) type WaitingOnChanged = Arc<dyn Fn() + Send + Sync>;
/// Answers whether the delivery with this message id is still queued (not yet in the context).
pub(crate) type StillQueued = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// One session's agent-message waits.
#[derive(Default)]
pub struct AgentMessageWaits {
    state: Mutex<WaitState>,
    on_change: Mutex<Option<WaitingOnChanged>>,
    still_queued: Mutex<Option<StillQueued>>,
}

/// A started wait: the arrival when the backlog already held one, else the parked receiver.
/// Dropping it (a timeout, a cycle, a cancelled cell) withdraws the waiter and its `waitingOn`.
pub(crate) struct AgentMessageWait {
    waits: Arc<AgentMessageWaits>,
    id: u64,
    pub(crate) started: AgentMessageWaitStart,
}

pub(crate) enum AgentMessageWaitStart {
    Ready(AgentMessageArrival),
    Parked(oneshot::Receiver<AgentMessageArrival>),
}

impl AgentMessageWaits {
    pub(crate) fn set_hooks(&self, on_change: WaitingOnChanged, still_queued: StillQueued) {
        *self
            .on_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(on_change);
        *self
            .still_queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(still_queued);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, WaitState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify_changed(&self) {
        let hook = self
            .on_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// The session the newest parked wait names, for the roster's `waitingOn`.
    pub(crate) fn waiting_on(&self) -> Option<String> {
        self.state()
            .waiters
            .iter()
            .rev()
            .find_map(|waiter| waiter.waiting_on.clone())
    }

    /// Report one accepted delivery: the oldest matching waiter takes it, else a direct message
    /// joins the backlog. A broadcast is never awaited. Answers whether the delivery ended a wait
    /// that named its target: that `waitingOn` is withdrawn here, and the caller publishes it to
    /// the roster before it acknowledges the delivery, so a sender that heard the acknowledgement
    /// never walks the stale edge into a false cycle.
    #[must_use = "a withdrawn `waitingOn` must reach the roster before the delivery is acknowledged"]
    pub(crate) fn deliver(&self, arrival: AgentMessageArrival) -> bool {
        if arrival.kind == AgentMessageKind::Broadcast {
            return false;
        }
        let mut state = self.state();
        let mut arrival = arrival;
        while let Some(position) = state
            .waiters
            .iter()
            .position(|waiter| accepts(&waiter.senders, &arrival))
        {
            let waiter = state.waiters.remove(position);
            let named = waiter.waiting_on.is_some();
            match waiter.reply.send(arrival) {
                Ok(()) => return named,
                // The waiter's cell was cancelled before its guard dropped: try the next one.
                Err(returned) => arrival = returned,
            }
        }
        if state.backlog.len() >= BACKLOG_LIMIT {
            state.backlog.pop_front();
        }
        state.backlog.push_back(arrival);
        false
    }

    /// Start one wait: claim a still-queued backlog arrival from an accepted sender, else park.
    pub(crate) fn begin(
        self: &Arc<Self>,
        senders: AgentMessageWaitSenders,
        waiting_on: Option<String>,
    ) -> AgentMessageWait {
        let still_queued = self
            .still_queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut state = self.state();
        // A backlog arrival already delivered into the context is no longer news.
        state.backlog.retain(|arrival| {
            still_queued
                .as_ref()
                .is_some_and(|still_queued| still_queued(&arrival.message_id))
        });
        state.next_id += 1;
        let id = state.next_id;
        if let Some(position) = state
            .backlog
            .iter()
            .position(|arrival| accepts(&senders, arrival))
        {
            let arrival = state
                .backlog
                .remove(position)
                .expect("position is in range");
            return AgentMessageWait {
                waits: Arc::clone(self),
                id,
                started: AgentMessageWaitStart::Ready(arrival),
            };
        }
        let (reply, receiver) = oneshot::channel();
        let named = waiting_on.is_some();
        state.waiters.push(Waiter {
            id,
            senders,
            waiting_on,
            reply,
        });
        drop(state);
        if named {
            self.notify_changed();
        }
        AgentMessageWait {
            waits: Arc::clone(self),
            id,
            started: AgentMessageWaitStart::Parked(receiver),
        }
    }
}

impl Drop for AgentMessageWait {
    fn drop(&mut self) {
        let mut state = self.waits.state();
        let Some(position) = state.waiters.iter().position(|waiter| waiter.id == self.id) else {
            return;
        };
        let waiter = state.waiters.remove(position);
        drop(state);
        if waiter.waiting_on.is_some() {
            self.waits.notify_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(id: &str, sender: &str, kind: AgentMessageKind) -> AgentMessageArrival {
        AgentMessageArrival {
            message_id: id.to_string(),
            sender_ids: vec![sender.to_string()],
            sender_name: sender.to_string(),
            preview: "hi".to_string(),
            kind,
        }
    }

    /// A broadcast never ends a wait or joins the backlog; a direct message from another sender
    /// waits in the backlog for its own wait while it is still queued; the awaited sender's direct
    /// message ends the wait, which withdraws `waitingOn` and leaves its publication to the
    /// delivering caller.
    #[tokio::test]
    async fn only_the_awaited_senders_direct_message_ends_a_wait() {
        let waits = Arc::new(AgentMessageWaits::default());
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&changes);
        waits.set_hooks(
            Arc::new(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
            Arc::new(|id| id == "m-other"),
        );
        let mut wait = waits.begin(Some(vec!["b".to_string()]), Some("b".to_string()));
        assert_eq!(waits.waiting_on().as_deref(), Some("b"));
        let withdrawn = [
            arrival("m-all", "b", AgentMessageKind::Broadcast),
            arrival("m-other", "c", AgentMessageKind::Direct),
            arrival("m-direct", "b", AgentMessageKind::Direct),
        ]
        .map(|delivered| waits.deliver(delivered));
        assert_eq!(withdrawn, [false, false, true]);
        let AgentMessageWaitStart::Parked(receiver) = &mut wait.started else {
            panic!("the backlog held nothing from b");
        };
        assert_eq!(
            receiver.await.unwrap(),
            arrival("m-direct", "b", AgentMessageKind::Direct)
        );
        assert_eq!(waits.waiting_on(), None);
        drop(wait);
        assert_eq!(changes.load(std::sync::atomic::Ordering::SeqCst), 1);
        let from_c = waits.begin(Some(vec!["c".to_string()]), None);
        assert!(matches!(
            &from_c.started,
            AgentMessageWaitStart::Ready(ready) if *ready == arrival("m-other", "c", AgentMessageKind::Direct)
        ));
    }
}
