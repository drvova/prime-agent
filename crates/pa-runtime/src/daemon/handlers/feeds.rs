//! The agents view's feeds on the runtime daemon, in the old supervisor's
//! shapes (`pa-daemon` `supervisor_roster.rs`, `agent_traffic.rs`), so a
//! client of the old wire reads them unchanged (pa-tui's `wire2_compat`
//! maps `roster_subscribe` and `agent_traffic_subscribe` here):
//!
//! - **Roster.** `runtime.roster` answers `{roster: [entry, ...]}`, the old
//!   `roster_subscribe` reply: one entry per open session and per subagent
//!   below one (each session's family depth first, in spawn order), each
//!   the old `AgentRosterEntry`: `agentId` (pa-types
//!   `roster_agent_id_for_summary`: a session's id, a subagent's
//!   `<parent transcript>#<handle>`), `summary` (the session's old summary,
//!   `handlers/attach.rs`, without `sessionActions`), `status`
//!   (`classify_summary_value`) and `workerId` (the session's id: sessions
//!   run in the daemon). `runtime/roster`, live only, carries the old
//!   `roster_update` frame without its `type`: `{changed: [entry]}` when an
//!   entry changed, `{changed: [], removed: [agentId, ...]}` when its
//!   session closed or its subagent was deleted (with the subagents below
//!   it). An entry is composed again once its session's stream says its
//!   activity may have changed (a run's or a turn's start and end, a
//!   message's or a tool's end, its queue, a child's status, a compaction,
//!   a fault), and published only when it differs from the one published
//!   last, as the old worker pushed its summary deltas.
//! - **Traffic.** `runtime.traffic` answers `{entries}`, the old
//!   `agent_traffic_subscribe` reply: the newest
//!   [`AGENT_TRAFFIC_RING_CAPACITY`] entries of each root's family, oldest
//!   first, roots in order; `runtime/traffic`, live only, carries each new
//!   entry as the inbox publishes it (`subagents/inbox.rs`), the old
//!   `agent_traffic` frame's `entry`.
//!
//! Both feeds are kept off every turn's path: one task hears the bus and
//! marks entries due, another composes them, so a slow composition delays
//! the feed only. What the old roster seeded and this one does not: the
//! saved sessions of a client's cwd (`seededCwd` rows) and the queued
//! child (`queuedChild`): a runtime subagent has its session at once.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use pa_types::agent_traffic::{AgentTrafficEntry, AGENT_TRAFFIC_RING_CAPACITY};
use pa_types::daemon::agent_roster::{
    classify_summary_value, roster_agent_id_for_summary, AgentRosterEntry,
};
use pa_types::event_log::{NewEvent, Topic, Trust};
use serde_json::{json, Map, Value};
use tokio::task::JoinHandle;

use super::RuntimeHandlers;
use crate::bus::Heard;
use crate::lock;
use crate::session::SessionBuilder;

/// The roster feed's topic.
const ROSTER_TOPIC: &str = "runtime/roster";

/// The stream events after which a session's roster entry may read
/// differently.
const ROSTER_TRIGGERS: &[&str] = &[
    "agent_start",
    "agent_end",
    "turn_start",
    "turn_end",
    "message_end",
    "tool_execution_start",
    "tool_execution_end",
    "session_action_update",
    "rlm_child_update",
    "compaction_start",
    "compaction_end",
    "auto_compaction_start",
    "auto_compaction_end",
    "session_info_changed",
    "session_crashed",
    "session_recovered",
    "session_failed",
];

/// The roster entries published and due, and the traffic rings.
#[derive(Default)]
pub(in crate::daemon) struct Feeds {
    /// The roster entry last published for each session, by its id.
    published: Mutex<HashMap<String, Value>>,
    /// The sessions whose entries are to be composed again, in the order
    /// they became due, each once.
    due: Mutex<Vec<String>>,
    wake: tokio::sync::Notify,
    /// Each root's newest traffic entries, oldest first.
    traffic: Mutex<HashMap<String, VecDeque<AgentTrafficEntry>>>,
}

impl Feeds {
    /// Session `id`'s roster entry is to be composed again: its session
    /// opened, changed or closed.
    pub(in crate::daemon) fn due(&self, id: &str) {
        let mut due = lock(&self.due);
        if !due.iter().any(|queued| queued == id) {
            due.push(id.to_string());
        }
        drop(due);
        self.wake.notify_one();
    }

    /// Rings `entry` in its root's family.
    fn ring(&self, entry: AgentTrafficEntry) {
        let mut rings = lock(&self.traffic);
        let ring = rings
            .entry(entry.root_active_session_id.clone())
            .or_default();
        if ring.len() >= AGENT_TRAFFIC_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(entry);
    }

    /// Every ring's entries, oldest first per root, roots in order.
    fn traffic(&self) -> Vec<AgentTrafficEntry> {
        let rings = lock(&self.traffic);
        let mut roots: Vec<&String> = rings.keys().collect();
        roots.sort();
        roots
            .into_iter()
            .flat_map(|root| rings[root].iter().cloned())
            .collect()
    }

    /// Hears `event`: a traffic entry is rung, and a session event that may
    /// change a roster entry makes it due.
    fn hear(&self, event: &pa_types::event_log::Event) {
        let levels: Vec<&str> = event.event.topic.levels().collect();
        let payload = &event.event.payload;
        match levels.as_slice() {
            ["runtime", "traffic"] => {
                match serde_json::from_value::<AgentTrafficEntry>(payload.clone()) {
                    Ok(entry) => self.ring(entry),
                    Err(error) => tracing::warn!("a traffic entry is malformed: {error}"),
                }
            }
            ["agent", id, "stream"] => {
                let kind = payload["type"].as_str().unwrap_or_default();
                if !ROSTER_TRIGGERS.contains(&kind) {
                    return;
                }
                self.due(id);
                // A child's status is its own entry's news too.
                if let Some(child) = payload["child"]["activeSessionId"].as_str() {
                    self.due(child);
                }
            }
            ["agent", id, "done" | "accepted" | "recovered" | "failed"] => self.due(id),
            _ => {}
        }
    }

    /// The published entries of `id` and of the subagents below it, taken
    /// out: their agent ids.
    fn retire(&self, id: &str) -> Vec<String> {
        let mut published = lock(&self.published);
        let mut gone = vec![id.to_string()];
        let mut removed = Vec::new();
        while let Some(session) = gone.pop() {
            if let Some(entry) = published.remove(&session) {
                if let Some(agent) = entry["agentId"].as_str() {
                    removed.push(agent.to_string());
                }
            }
            gone.extend(
                published
                    .iter()
                    .filter(|(_, entry)| entry["summary"]["parentActiveSessionId"] == session)
                    .map(|(child, _)| child.clone()),
            );
        }
        removed
    }
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// Session `id`'s roster entry (see the module documentation).
    async fn roster_entry(&self, id: &str) -> anyhow::Result<Value> {
        let mut summary = self.compose_session(id).await?.summary;
        if let Some(summary) = summary.as_object_mut() {
            summary.remove("sessionActions");
        }
        let entry = AgentRosterEntry {
            agent_id: roster_agent_id_for_summary(&summary),
            queued_child: None,
            seeded_cwd: None,
            status: classify_summary_value(&summary, false),
            status_label: None,
            last_heard_from_at: None,
            worker_id: Some(id.to_string()),
            summary,
            rest: Map::new(),
        };
        Ok(serde_json::to_value(entry)?)
    }

    /// The sessions the roster lists: each open session, then the subagents
    /// below it, depth first in spawn order.
    async fn roster_members(&self) -> Vec<String> {
        let mut roots: Vec<String> = self.open.paths().into_keys().collect();
        roots.sort();
        let mut members = Vec::new();
        for root in roots {
            let family = self.runtime.subagents().family_of(&root).await;
            members.push(root);
            let mut below: Vec<_> = family.into_iter().rev().collect();
            while let Some(member) = below.pop() {
                members.push(member.session_id);
                below.extend(member.children.into_iter().rev());
            }
        }
        members
    }

    /// `runtime.roster`: every member's entry; a member that closed while
    /// it was read is left out.
    pub(super) async fn roster(&self) -> Value {
        let mut roster = Vec::new();
        for id in self.roster_members().await {
            match self.roster_entry(&id).await {
                Ok(entry) => roster.push(entry),
                Err(error) => tracing::debug!("{id} left the roster: {error:#}"),
            }
        }
        json!({ "roster": roster })
    }

    /// `runtime.traffic`: the rings' entries.
    pub(super) fn traffic(&self) -> Value {
        json!({ "entries": self.feeds.traffic() })
    }

    /// Whether session `id` still runs in the daemon: open, or a subagent
    /// below an open session.
    fn in_roster(&self, id: &str) -> bool {
        self.open.contains(id) || self.runtime.subagents().lineage(id).is_some()
    }

    /// Publishes `update` (a `roster_update` frame without its `type`) on
    /// the roster feed.
    async fn publish_roster(&self, update: Value) {
        let published = self
            .runtime
            .publish(NewEvent {
                topic: Topic::new(ROSTER_TOPIC).expect("a topic"),
                source: "runtime".to_string(),
                session: None,
                payload: update,
                trust: Trust::Owner,
                cause: None,
                reply: None,
            })
            .await;
        if let Err(error) = published {
            tracing::warn!("a roster update was not published: {error:#}");
        }
    }

    /// Composes the due entries and publishes what changed, until the
    /// daemon ends.
    async fn compose_due(&self) {
        loop {
            let due = std::mem::take(&mut *lock(&self.feeds.due));
            if due.is_empty() {
                self.feeds.wake.notified().await;
                continue;
            }
            for id in due {
                if !self.in_roster(&id) {
                    let removed = self.feeds.retire(&id);
                    if !removed.is_empty() {
                        self.publish_roster(json!({ "changed": [], "removed": removed }))
                            .await;
                    }
                    continue;
                }
                // A session being rebuilt is composed again once it
                // recovers (its `recovered` makes it due).
                let entry = match self.roster_entry(&id).await {
                    Ok(entry) => entry,
                    Err(error) => {
                        tracing::debug!("{id}'s roster entry was not composed: {error:#}");
                        continue;
                    }
                };
                let changed = lock(&self.feeds.published)
                    .insert(id.clone(), entry.clone())
                    .as_ref()
                    != Some(&entry);
                if changed {
                    self.publish_roster(json!({ "changed": [entry] })).await;
                }
            }
        }
    }

    /// Starts the feeds' tasks: one hears the bus, the other composes the
    /// due roster entries. They run until aborted.
    pub(in crate::daemon) fn spawn_feeds(self: &Arc<Self>) -> [JoinHandle<()>; 2] {
        let mut listener = self.runtime.listen();
        let feeds = Arc::clone(&self.feeds);
        let hearer = tokio::spawn(async move {
            while let Some(heard) = listener.recv().await {
                match heard {
                    Heard::Event(event) => feeds.hear(&event),
                    // What was missed may have changed any entry.
                    Heard::Lagged { .. } => {
                        let published: Vec<String> =
                            lock(&feeds.published).keys().cloned().collect();
                        for id in published {
                            feeds.due(&id);
                        }
                    }
                }
            }
        });
        let handlers = Arc::clone(self);
        let composer = tokio::spawn(async move { handlers.compose_due().await });
        [hearer, composer]
    }
}

#[cfg(test)]
mod tests {
    use pa_types::agent_traffic::{AgentTrafficEntry, AGENT_TRAFFIC_RING_CAPACITY};
    use serde_json::{json, Map};

    use super::Feeds;

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

    /// Each root's ring keeps its newest entries up to the capacity, the
    /// oldest dropping first, roots in order.
    #[test]
    fn each_root_ring_is_bounded_and_drops_its_oldest() {
        let feeds = Feeds::default();
        feeds.ring(entry("root-b", 0));
        for index in 0..AGENT_TRAFFIC_RING_CAPACITY + 3 {
            feeds.ring(entry("root-a", index));
        }
        let expected: Vec<AgentTrafficEntry> = (3..AGENT_TRAFFIC_RING_CAPACITY + 3)
            .map(|index| entry("root-a", index))
            .chain([entry("root-b", 0)])
            .collect();
        assert_eq!(feeds.traffic(), expected);
    }

    /// A session that leaves the roster takes the subagents below it along,
    /// and only entries that were published are removed.
    #[test]
    fn a_retired_session_takes_its_subagents_along() {
        let feeds = Feeds::default();
        let row = |agent: &str, parent: Option<&str>| json!({ "agentId": agent, "summary": { "parentActiveSessionId": parent } });
        {
            let mut published = super::lock(&feeds.published);
            published.insert("root".to_string(), row("root", None));
            published.insert("child".to_string(), row("/r.jsonl#sub-1", Some("root")));
            published.insert("grand".to_string(), row("/c.jsonl#sub-2", Some("child")));
            published.insert("other".to_string(), row("other", None));
        }
        let mut removed = feeds.retire("root");
        removed.sort();
        assert_eq!(
            (removed, feeds.retire("never")),
            (
                vec![
                    "/c.jsonl#sub-2".to_string(),
                    "/r.jsonl#sub-1".to_string(),
                    "root".to_string()
                ],
                Vec::new()
            )
        );
    }
}
