//! `agent_message.wait` for daemon workers: park on this session's receiving side, publish
//! `waitingOn` through the roster, and refuse a wait that would close a cycle.
//!
//! The roster is a pushed copy of each session's state, so an edge in it can outlive the wait it
//! records: a delivery ends the receiver's wait at once, and the roster hears of it a moment
//! later. A cycle the roster shows is therefore confirmed against each session's live state
//! (`get_state`) before the wait answers `cycle`.

use std::collections::HashMap;

use pa_core::session_engine::agent_messaging::{
    AgentFamilyMember, AgentMessageWaitArrival, AgentMessageWaitFilter, AgentMessageWaitOutcome,
    AgentMessageWaitRequest,
};
use serde_json::{json, Value};

use super::message::roster_summaries;
use super::LinkAgentMessageController;
use crate::agent_message_wait::{AgentMessageArrival, AgentMessageWaitStart};

/// The longest `waitingOn` chain a cycle walk follows (a family is never this deep).
const MAX_WAIT_CHAIN: usize = 64;

/// How long confirming one chain step's live state may take.
const LIVE_STATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One roster row's wait edge: the session it waits on, and its display name.
struct WaitEdge {
    active: String,
    waiting_on: Option<String>,
    name: String,
}

/// One session on a wait chain: its live id, its display name, and the session it waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChainStep {
    active: String,
    name: String,
    waiting_on: String,
}

/// Follow `waitingOn` from `start` through the roster: the steps along the chain when it reaches
/// one of `own_ids`, else `None`.
fn wait_cycle(roster: &[Value], start: &str, own_ids: &[&str]) -> Option<Vec<ChainStep>> {
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
            active: active.to_string(),
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
        let next = edge.waiting_on.as_deref()?;
        chain.push(ChainStep {
            active: edge.active.clone(),
            name: edge.name.clone(),
            waiting_on: next.to_string(),
        });
        if own_ids.contains(&next) {
            return Some(chain);
        }
        current = next.to_string();
    }
    None
}

/// Whether every step of a chain the roster showed still holds: each session's live `waitingOn`
/// (from `live_waiting_on`, by live id) is the one the roster recorded. A step that no longer
/// holds means the roster was stale and there is no cycle.
async fn chain_holds<F, Fut>(chain: &[ChainStep], mut live_waiting_on: F) -> anyhow::Result<bool>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<String>>>,
{
    for step in chain {
        if live_waiting_on(step.active.clone()).await?.as_deref() != Some(step.waiting_on.as_str())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every selector form that names one family member.
fn member_ids(member: &AgentFamilyMember) -> impl Iterator<Item = String> + '_ {
    std::iter::once(member.id.clone()).chain(member.aliases.iter().cloned())
}

impl LinkAgentMessageController {
    pub(super) async fn wait_for_message(
        &self,
        request: AgentMessageWaitRequest,
    ) -> anyhow::Result<AgentMessageWaitOutcome> {
        let senders = match &request.filter {
            AgentMessageWaitFilter::Any => None,
            AgentMessageWaitFilter::Members(members) => {
                Some(members.iter().flat_map(member_ids).collect::<Vec<_>>())
            }
        };
        let waiting_on = request.waiting_on.as_ref().map(|member| member.id.clone());
        // Park (and publish `waitingOn`) BEFORE the cycle walk: of two sessions that start waiting
        // on each other at once, the later walk sees the earlier wait.
        let mut wait = self.waits.begin(senders, waiting_on.clone());
        let arrival = match &mut wait.started {
            AgentMessageWaitStart::Ready(arrival) => arrival.clone(),
            AgentMessageWaitStart::Parked(receiver) => {
                if let Some(target) = &waiting_on {
                    let own_session_id = self.family_identity().session_id;
                    let mut own_ids = vec![self.active_session_id.as_str()];
                    own_ids.extend(own_session_id.as_deref());
                    let roster = roster_summaries(&self.link, &self.worker_token).await?;
                    if let Some(chain) = wait_cycle(&roster, target, &own_ids) {
                        let live = |active: String| async move {
                            let state = self
                                .link
                                .request_success(
                                    json!({ "type": "get_state", "activeSessionId": active }),
                                    LIVE_STATE_TIMEOUT,
                                )
                                .await?;
                            Ok(state
                                .get("waitingOn")
                                .and_then(Value::as_str)
                                .filter(|waiting_on| !waiting_on.is_empty())
                                .map(str::to_string))
                        };
                        if chain_holds(&chain, live).await? {
                            return Ok(AgentMessageWaitOutcome::Cycle {
                                blocked_via: chain.into_iter().map(|step| step.name).collect(),
                            });
                        }
                    }
                }
                match tokio::time::timeout(request.timeout, receiver).await {
                    Ok(Ok(arrival)) => arrival,
                    // A dropped sender is the session closing: nothing will arrive.
                    Ok(Err(_)) | Err(_) => return Ok(AgentMessageWaitOutcome::Timeout),
                }
            }
        };
        Ok(AgentMessageWaitOutcome::Message(arrival_outcome(
            arrival,
            &request.family,
        )))
    }
}

/// The wait result for one arrival: the sender's role resolves against the family at wait start.
fn arrival_outcome(
    arrival: AgentMessageArrival,
    family: &[AgentFamilyMember],
) -> AgentMessageWaitArrival {
    let sender_role = family
        .iter()
        .find(|member| member_ids(member).any(|id| arrival.sender_ids.contains(&id)))
        .map(|member| member.relationship);
    AgentMessageWaitArrival {
        message_id: arrival.message_id,
        sender_name: arrival.sender_name,
        sender_role,
        preview: arrival.preview,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{chain_holds, wait_cycle, ChainStep};

    /// The walk follows `waitingOn` by live or durable id and names the chain back to the caller;
    /// a chain that ends elsewhere is no cycle.
    #[test]
    fn a_wait_chain_back_to_the_caller_is_a_cycle() {
        let roster = [
            json!({ "activeSessionId": "a1", "sessionId": "sa", "sessionName": "a" }),
            json!({ "activeSessionId": "b1", "sessionId": "sb", "sessionName": "b", "waitingOn": "sc" }),
            json!({ "activeSessionId": "c1", "sessionId": "sc", "sessionName": "c", "waitingOn": "a1" }),
            json!({ "activeSessionId": "d1", "sessionName": "d", "waitingOn": "b1" }),
        ];
        assert_eq!(
            wait_cycle(&roster, "b1", &["a1", "sa"]),
            Some(vec![
                ChainStep {
                    active: "b1".to_string(),
                    name: "b".to_string(),
                    waiting_on: "sc".to_string()
                },
                ChainStep {
                    active: "c1".to_string(),
                    name: "c".to_string(),
                    waiting_on: "a1".to_string()
                },
            ])
        );
        assert_eq!(wait_cycle(&roster, "d1", &["c1"]), None);
    }

    /// A cycle the roster still shows after one of its waits ended is not a cycle: each step is
    /// checked against the session's live state, and a step whose live wait moved on breaks it.
    /// (The roster hears of an ended wait a moment after the delivery that ended it.)
    #[tokio::test]
    async fn a_roster_cycle_whose_live_wait_ended_is_no_cycle() {
        let roster = [
            json!({ "activeSessionId": "beta", "sessionName": "beta" }),
            json!({ "activeSessionId": "alpha", "sessionName": "alpha", "waitingOn": "beta" }),
        ];
        let chain = wait_cycle(&roster, "alpha", &["beta"]).expect("the roster shows a cycle");
        let live = |still_waiting: bool| {
            move |active: String| {
                std::future::ready(Ok(
                    (active == "alpha" && still_waiting).then(|| "beta".to_string())
                ))
            }
        };
        assert_eq!(
            (
                chain_holds(&chain, live(true)).await.unwrap(),
                chain_holds(&chain, live(false)).await.unwrap(),
            ),
            (true, false)
        );
    }
}
