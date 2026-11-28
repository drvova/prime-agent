//! The receiving side of `agent_message.wait`: the session's parked waiters, its `waitingOn`
//! report, and the backlog of direct messages that arrived while the session was busy and are
//! still queued. A daemon's delivery path reports every accepted delivery here; its
//! `agent_message.wait` controller parks on it ([`wait_for_arrival`]). A waiter holds no
//! admission slot and no lock: it is a oneshot receiver.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::oneshot;

use super::{
    AgentFamilyMember, AgentMessageKind, AgentMessageWaitArrival, AgentMessageWaitFilter,
    AgentMessageWaitOutcome, AgentMessageWaitRequest,
};

/// One accepted delivery, as a wait sees it: identity and a sanitized preview, never the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageArrival {
    pub message_id: String,
    /// The sender's live and durable session ids (the forms a wait filter matches).
    pub sender_ids: Vec<String>,
    pub sender_name: String,
    pub preview: String,
    pub kind: AgentMessageKind,
}

/// The senders one wait accepts: `None` accepts any direct message.
pub type AgentMessageWaitSenders = Option<Vec<String>>;

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
const BACKLOG_LIMIT: usize = super::DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION;

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
pub type WaitingOnChanged = Arc<dyn Fn() + Send + Sync>;
/// Answers whether the delivery with this message id is still queued (not yet in the context).
pub type StillQueued = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// One session's agent-message waits.
#[derive(Default)]
pub struct AgentMessageWaits {
    state: Mutex<WaitState>,
    on_change: Mutex<Option<WaitingOnChanged>>,
    still_queued: Mutex<Option<StillQueued>>,
}

/// A started wait: the arrival when the backlog already held one, else the parked receiver.
/// Dropping it (a timeout, a cycle, a cancelled cell) withdraws the waiter and its `waitingOn`.
pub struct AgentMessageWait {
    waits: Arc<AgentMessageWaits>,
    id: u64,
    pub started: AgentMessageWaitStart,
}

/// How a wait started: claimed from the backlog, or parked for the next arrival.
pub enum AgentMessageWaitStart {
    Ready(AgentMessageArrival),
    Parked(oneshot::Receiver<AgentMessageArrival>),
}

impl AgentMessageWaits {
    pub fn set_hooks(&self, on_change: WaitingOnChanged, still_queued: StillQueued) {
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
    pub fn waiting_on(&self) -> Option<String> {
        self.state()
            .waiters
            .iter()
            .rev()
            .find_map(|waiter| waiter.waiting_on.clone())
    }

    /// Report one accepted delivery: the oldest matching waiter takes it, else a direct message
    /// joins the backlog. A broadcast is never awaited. Answers whether the delivery ended a wait
    /// that named its target: that `waitingOn` is withdrawn here and the change hook hears it, and
    /// a caller whose roster is a pushed copy (the old daemon's supervisor) publishes it before it
    /// acknowledges the delivery, so a sender that heard the acknowledgement never walks the stale
    /// edge into a false cycle.
    #[must_use = "a withdrawn `waitingOn` must reach the roster before the delivery is acknowledged"]
    pub fn deliver(&self, arrival: AgentMessageArrival) -> bool {
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
                Ok(()) => {
                    drop(state);
                    if named {
                        self.notify_changed();
                    }
                    return named;
                }
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
    pub fn begin(
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
        let claimed = state
            .backlog
            .iter()
            .position(|arrival| accepts(&senders, arrival))
            .and_then(|position| state.backlog.remove(position));
        if let Some(arrival) = claimed {
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

/// The longest `waitingOn` chain a cycle walk follows (a family is never this deep).
const MAX_WAIT_CHAIN: usize = 64;

/// One roster row's wait edge: the session it waits on, and its display name.
struct WaitEdge {
    waiting_on: Option<String>,
    name: String,
}

/// Follow `waitingOn` from `start` through the roster (rows of `activeSessionId` or `id`,
/// `sessionId`, `sessionName`, `waitingOn`): the names along the chain when it reaches one of
/// `own_ids`, else `None`.
#[must_use]
pub fn wait_cycle(roster: &[Value], start: &str, own_ids: &[&str]) -> Option<Vec<String>> {
    let mut edges: HashMap<String, std::rc::Rc<WaitEdge>> = HashMap::new();
    for row in roster {
        let field = |key: &str| {
            row.get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        };
        let Some(active) = field("activeSessionId").or_else(|| field("id")) else {
            continue;
        };
        let edge = std::rc::Rc::new(WaitEdge {
            waiting_on: field("waitingOn").map(str::to_string),
            name: field("sessionName")
                .or_else(|| field("sessionId"))
                .unwrap_or(active)
                .to_string(),
        });
        // A wait names its target by live id, or by durable id for a passivated child.
        if let Some(session_id) = field("sessionId") {
            edges.insert(session_id.to_string(), std::rc::Rc::clone(&edge));
        }
        edges.insert(active.to_string(), edge);
    }
    let mut chain = Vec::new();
    let mut current = start.to_string();
    for _ in 0..MAX_WAIT_CHAIN {
        let edge = edges.get(&current)?;
        chain.push(edge.name.clone());
        let next = edge.waiting_on.as_deref()?;
        if own_ids.contains(&next) {
            return Some(chain);
        }
        current = next.to_string();
    }
    None
}

/// Every selector form that names one family member.
fn member_ids(member: &AgentFamilyMember) -> impl Iterator<Item = String> + '_ {
    std::iter::once(member.id.clone()).chain(member.aliases.iter().cloned())
}

/// One `agent_message.wait` on `waits`, the session known by `own_ids`: park (publishing
/// `waitingOn`) BEFORE the cycle walk over `roster`, so of two sessions that start waiting on
/// each other at once the later walk sees the earlier wait; then the arrival, or the timeout.
///
/// # Errors
///
/// The roster could not be read.
pub async fn wait_for_arrival(
    waits: &Arc<AgentMessageWaits>,
    request: AgentMessageWaitRequest,
    own_ids: &[&str],
    roster: impl std::future::Future<Output = anyhow::Result<Vec<Value>>>,
) -> anyhow::Result<AgentMessageWaitOutcome> {
    let senders = match &request.filter {
        AgentMessageWaitFilter::Any => None,
        AgentMessageWaitFilter::Members(members) => {
            Some(members.iter().flat_map(member_ids).collect::<Vec<_>>())
        }
    };
    let waiting_on = request.waiting_on.as_ref().map(|member| member.id.clone());
    let mut wait = waits.begin(senders, waiting_on.clone());
    let arrival = match &mut wait.started {
        AgentMessageWaitStart::Ready(arrival) => arrival.clone(),
        AgentMessageWaitStart::Parked(receiver) => {
            if let Some(target) = &waiting_on {
                let roster = roster.await?;
                if let Some(blocked_via) = wait_cycle(&roster, target, own_ids) {
                    return Ok(AgentMessageWaitOutcome::Cycle { blocked_via });
                }
            }
            match tokio::time::timeout(request.timeout, receiver).await {
                Ok(Ok(arrival)) => arrival,
                // A dropped sender is the session closing: nothing will arrive.
                Ok(Err(_)) | Err(_) => return Ok(AgentMessageWaitOutcome::Timeout),
            }
        }
    };
    // The sender's role resolves against the family at wait start.
    let sender_role = request
        .family
        .iter()
        .find(|member| member_ids(member).any(|id| arrival.sender_ids.contains(&id)))
        .map(|member| member.relationship);
    Ok(AgentMessageWaitOutcome::Message(AgentMessageWaitArrival {
        message_id: arrival.message_id,
        sender_name: arrival.sender_name,
        sender_role,
        preview: arrival.preview,
    }))
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
    /// message ends the wait, which withdraws `waitingOn` (the hook hears it) and tells the
    /// delivering caller so.
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
        assert_eq!(changes.load(std::sync::atomic::Ordering::SeqCst), 2);
        let from_c = waits.begin(Some(vec!["c".to_string()]), None);
        assert!(matches!(
            &from_c.started,
            AgentMessageWaitStart::Ready(ready) if *ready == arrival("m-other", "c", AgentMessageKind::Direct)
        ));
    }

    /// The walk follows `waitingOn` by live or durable id and names the chain back to the caller;
    /// a chain that ends elsewhere is no cycle.
    #[test]
    fn a_wait_chain_back_to_the_caller_is_a_cycle() {
        let roster = [
            serde_json::json!({ "activeSessionId": "a1", "sessionId": "sa", "sessionName": "a" }),
            serde_json::json!({ "activeSessionId": "b1", "sessionId": "sb", "sessionName": "b", "waitingOn": "sc" }),
            serde_json::json!({ "activeSessionId": "c1", "sessionId": "sc", "sessionName": "c", "waitingOn": "a1" }),
            serde_json::json!({ "activeSessionId": "d1", "sessionName": "d", "waitingOn": "b1" }),
        ];
        assert_eq!(
            wait_cycle(&roster, "b1", &["a1", "sa"]),
            Some(vec!["b".to_string(), "c".to_string()])
        );
        assert_eq!(wait_cycle(&roster, "d1", &["c1"]), None);
    }
}
