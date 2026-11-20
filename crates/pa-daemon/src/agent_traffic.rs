//! The display-only agent traffic feed. The receiving worker reports each accepted delivery
//! (`worker_agent_traffic`) after it lands; the supervisor keeps a bounded in-memory ring per root
//! tree and pushes each new entry to the clients holding `agent_traffic_subscribe`. Nothing here
//! is persisted, reaches a model's context, or touches a transcript.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use pa_types::agent_traffic::{AgentTrafficEntry, AGENT_TRAFFIC_RING_CAPACITY};
use pa_types::daemon::DaemonOutbound;
use serde_json::{json, Map, Value};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::supervisor::ClientRouting;
use crate::supervisor::Supervisor;

/// The per-root-tree rings, oldest entry first.
#[derive(Default)]
pub(crate) struct AgentTrafficRings {
    rings: std::sync::Mutex<HashMap<String, VecDeque<AgentTrafficEntry>>>,
}

impl AgentTrafficRings {
    fn push(&self, entry: AgentTrafficEntry) {
        let mut rings = self
            .rings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ring = rings
            .entry(entry.root_active_session_id.clone())
            .or_default();
        if ring.len() >= AGENT_TRAFFIC_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(entry);
    }

    /// Every ring's entries, oldest first per root, roots in a stable order.
    fn snapshot(&self) -> Vec<AgentTrafficEntry> {
        let rings = self
            .rings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut roots: Vec<&String> = rings.keys().collect();
        roots.sort();
        roots
            .into_iter()
            .flat_map(|root| rings[root].iter().cloned())
            .collect()
    }
}

impl Supervisor {
    /// `agent_traffic_subscribe`: the rings' current entries (the caller's flag is already set).
    pub(crate) fn handle_agent_traffic_subscribe(
        &self,
        command_id: &str,
        type_name: &str,
    ) -> DaemonResponse {
        response_success(
            Some(command_id),
            type_name,
            Some(json!({ "entries": self.agent_traffic.snapshot() })),
        )
    }

    /// `worker_agent_traffic`: authenticate the worker, stamp its root tree, ring the entry, and
    /// push it to the traffic subscribers.
    pub(crate) async fn handle_worker_agent_traffic(
        self: &Arc<Self>,
        command_id: &str,
        type_name: &str,
        worker_token: &str,
        entry: &AgentTrafficEntry,
    ) -> DaemonResponse {
        let Some(worker) = self.registry.find_by_token(worker_token).await else {
            return response_failure(
                Some(command_id),
                type_name,
                "Worker authentication failed",
                None,
            );
        };
        let root = worker
            .descriptor
            .lock()
            .await
            .root_active_session_id
            .clone();
        let mut entry = entry.clone();
        entry.root_active_session_id = if root.is_empty() {
            entry.to_active_session_id.clone()
        } else {
            root
        };
        self.agent_traffic.push(entry.clone());
        if let Ok(payload) = serde_json::to_value(DaemonOutbound::AgentTraffic {
            entry,
            rest: Map::default(),
        }) {
            let _ = self
                .events
                .send((ClientRouting::AgentTrafficSubscribers, Arc::new(payload)));
        }
        response_success(Some(command_id), type_name, Some(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(root: &str, index: usize) -> AgentTrafficEntry {
        AgentTrafficEntry {
            root_active_session_id: root.to_string(),
            from: "a".to_string(),
            from_active_session_id: Some("a1".to_string()),
            to: "b".to_string(),
            to_active_session_id: "b1".to_string(),
            relationship: Some("sibling".to_string()),
            kind: "direct".to_string(),
            outcome: "delivered".to_string(),
            chars: 2,
            preview: format!("m{index}"),
            timestamp_ms: index as u64,
            rest: Map::default(),
        }
    }

    /// Each root tree keeps its own newest entries up to the capacity, oldest dropping first.
    #[test]
    fn each_root_ring_is_bounded_and_drops_its_oldest() {
        let rings = AgentTrafficRings::default();
        for index in 0..AGENT_TRAFFIC_RING_CAPACITY + 3 {
            rings.push(entry("root-a", index));
        }
        rings.push(entry("root-b", 0));
        let snapshot = rings.snapshot();
        let expected: Vec<AgentTrafficEntry> = (3..AGENT_TRAFFIC_RING_CAPACITY + 3)
            .map(|index| entry("root-a", index))
            .chain([entry("root-b", 0)])
            .collect();
        assert_eq!(snapshot, expected);
    }
}
