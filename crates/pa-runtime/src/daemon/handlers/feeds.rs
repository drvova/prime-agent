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
//!   run in the daemon); then the rows the roster keeps beside them, the
//!   supervisor's, by the rules pa-core's `rlm_roster` holds for both:
//!   - an open root's ledger family (every live edge below its
//!     transcript), seeded as edge-only rows when the root's entry is
//!     first published, in the same update, then hydrated from each
//!     child's transcript off that update (cwd, model, level, own usage);
//!     a child the runtime opened again as settled and that has not run
//!     or been opened since is listed as this row, not as a live session
//!     (the old daemon keeps such a child passive, so a client opens it by
//!     its transcript);
//!   - a stopped root's row, passivated in place (`status: inactive`, no
//!     `activeSessionId`); an ephemeral session's row and a subagent's go.
//!
//!   Every row carries its deleted descendants' spend
//!   (`deletedDescendantUsage`, the ledger's fold by canonical parent
//!   transcript), refreshed as a root is first published and as a member
//!   leaves. `runtime/roster`, live only, carries the old `roster_update`
//!   frame without its `type`: `{changed: [entry]}` when an entry changed,
//!   `{changed: [...], removed: [agentId, ...]}` when its session closed or
//!   its subagent was deleted (with the subagents and seeded rows below
//!   it). An entry is composed again once its session's stream says its
//!   activity may have changed (a run's or a turn's start and end, a
//!   message's or a tool's end, its queue, a child's status, a compaction,
//!   a fault) or its wait edge changed (the summary's `waitingOn`, which a
//!   parked `agent_message.wait` names and a cycle walk follows), and
//!   published only when it differs from the one published
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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_core::session::lease::canonical_session_path;
use pa_core::session::usage_summary::SessionUsageSummary;
use pa_core::session_engine::rlm_roster::{
    attach_deleted_descendant_usage, hydrate_seeded_summary, passivated_summary,
    seeded_child_summary,
};
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

/// A row the roster keeps beside the open sessions' entries.
#[derive(Clone, Debug, PartialEq)]
struct KeptRow {
    /// The `AgentRosterEntry` as published.
    entry: Value,
    kind: Kept,
}

/// What keeps a [`KeptRow`].
#[derive(Clone, Debug, PartialEq)]
enum Kept {
    /// A stopped root's row, passivated in place.
    Passivated,
    /// A ledger child seeded for open root `root`; `session` is the child's
    /// session id (its transcript's stem).
    Seeded { root: String, session: String },
}

/// The roster entries published and due, and the traffic rings.
#[derive(Default)]
pub(in crate::daemon) struct Feeds {
    /// The roster entry last published for each session, by its id.
    published: Mutex<HashMap<String, Value>>,
    /// The passivated and seeded rows, by agent id.
    kept: Mutex<BTreeMap<String, KeptRow>>,
    /// The open roots whose ledger family was seeded.
    seeded_roots: Mutex<HashSet<String>>,
    /// The ephemeral sessions among the published: their rows go at their
    /// stop.
    ephemeral: Mutex<HashSet<String>>,
    /// The deleted descendants' spend by canonical parent transcript.
    bucket: Mutex<HashMap<String, SessionUsageSummary>>,
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

    /// Session `id` left the daemon: its published entry and those of the
    /// subagents below it are taken out, with the rows seeded for it or for
    /// its transcript. A root's entry (not an ephemeral one's) passivates in
    /// place: answered as the changed entry, the rest by agent id.
    fn leave(&self, id: &str) -> (Option<Value>, Vec<String>) {
        let mut published = lock(&self.published);
        let own = published.remove(id);
        let mut gone = vec![id.to_string()];
        let mut removed = Vec::new();
        while let Some(session) = gone.pop() {
            if session != id {
                if let Some(entry) = published.remove(&session) {
                    if let Some(agent) = entry["agentId"].as_str() {
                        removed.push(agent.to_string());
                    }
                }
            }
            gone.extend(
                published
                    .iter()
                    .filter(|(_, entry)| entry["summary"]["parentActiveSessionId"] == session)
                    .map(|(child, _)| child.clone()),
            );
        }
        drop(published);
        lock(&self.seeded_roots).remove(id);
        let ephemeral = lock(&self.ephemeral).remove(id);
        let mut kept = lock(&self.kept);
        kept.retain(|agent, row| match &row.kind {
            Kept::Seeded { root, session } if root == id || session == id => {
                removed.push(agent.clone());
                false
            }
            Kept::Seeded { .. } | Kept::Passivated => true,
        });
        let Some(own) = own else {
            return (None, removed);
        };
        let agent = own["agentId"].as_str().unwrap_or_default().to_string();
        if ephemeral || own["summary"]["runtimeKind"] != "top-level" {
            removed.push(agent);
            return (None, removed);
        }
        let summary = passivated_summary(own["summary"].clone());
        let entry = json!(AgentRosterEntry {
            agent_id: agent.clone(),
            queued_child: None,
            seeded_cwd: None,
            status: classify_summary_value(&summary, false),
            status_label: None,
            last_heard_from_at: None,
            worker_id: None,
            summary,
            rest: Map::new(),
        });
        kept.insert(
            agent,
            KeptRow {
                entry: entry.clone(),
                kind: Kept::Passivated,
            },
        );
        (Some(entry), removed)
    }

    /// A live `entry` is published: the kept rows it supersedes (the same
    /// agent, or the same transcript) are taken out; answers the agent ids
    /// a client must drop (those it does not replace by the same id).
    fn supersede(&self, entry: &Value) -> Vec<String> {
        let agent = entry["agentId"].as_str().unwrap_or_default();
        let file = entry["summary"]["sessionFile"]
            .as_str()
            .map(|file| canonical_session_path(Path::new(file)));
        let mut removed = Vec::new();
        lock(&self.kept).retain(|kept, row| {
            if kept == agent {
                return false;
            }
            let same_file = file.is_some()
                && row.entry["summary"]["sessionFile"]
                    .as_str()
                    .map(|other| canonical_session_path(Path::new(other)))
                    == file;
            if same_file {
                removed.push(kept.clone());
            }
            !same_file
        });
        removed
    }

    /// Keeps `rows`, the ledger family seeded for root `root` (each with its
    /// child's session id), except those a published or kept row already
    /// holds; answers the entries kept.
    fn seed(&self, root: &str, rows: Vec<(String, Value)>) -> Vec<Value> {
        let published: HashSet<String> = lock(&self.published)
            .values()
            .filter_map(|entry| entry["agentId"].as_str().map(str::to_string))
            .collect();
        let mut kept = lock(&self.kept);
        let mut seeded = Vec::new();
        for (session, entry) in rows {
            let agent = entry["agentId"].as_str().unwrap_or_default().to_string();
            if published.contains(&agent) || kept.contains_key(&agent) {
                continue;
            }
            kept.insert(
                agent,
                KeptRow {
                    entry: entry.clone(),
                    kind: Kept::Seeded {
                        root: root.to_string(),
                        session,
                    },
                },
            );
            seeded.push(entry);
        }
        seeded
    }

    /// Replaces kept row `before` with `after`, only while the roster still
    /// holds `before`; whether it did.
    fn replace_kept(&self, before: &Value, after: Value) -> bool {
        let agent = before["agentId"].as_str().unwrap_or_default();
        let mut kept = lock(&self.kept);
        match kept.get_mut(agent) {
            Some(row) if row.entry == *before => {
                row.entry = after;
                true
            }
            Some(_) | None => false,
        }
    }

    /// The kept rows, by agent id.
    fn kept(&self) -> Vec<Value> {
        lock(&self.kept)
            .values()
            .map(|row| row.entry.clone())
            .collect()
    }

    /// Attaches the bucket's spend to `entry` (see [`bill`]); whether it
    /// changed.
    fn attach_bucket(&self, entry: &mut Value) -> bool {
        bill(&lock(&self.bucket), entry)
    }

    /// Takes `bucket` as the deleted descendants' spend: the published and
    /// kept entries whose spend it changes are rewritten and answered.
    fn rebill(&self, bucket: HashMap<String, SessionUsageSummary>) -> Vec<Value> {
        let mut changed = Vec::new();
        for entry in lock(&self.published).values_mut() {
            if bill(&bucket, entry) {
                changed.push(entry.clone());
            }
        }
        for row in lock(&self.kept).values_mut() {
            if bill(&bucket, &mut row.entry) {
                changed.push(row.entry.clone());
            }
        }
        *lock(&self.bucket) = bucket;
        changed
    }
}

/// Attaches `bucket`'s spend for `entry`'s canonical transcript to its
/// summary (pa-core's `attach_deleted_descendant_usage`); whether it
/// changed.
fn bill(bucket: &HashMap<String, SessionUsageSummary>, entry: &mut Value) -> bool {
    let file = entry["summary"]["sessionFile"].as_str().map(|file| {
        canonical_session_path(Path::new(file))
            .to_string_lossy()
            .into_owned()
    });
    let usage = file.as_deref().and_then(|file| bucket.get(file));
    attach_deleted_descendant_usage(&mut entry["summary"], usage)
}

/// A seeded row's entry: no worker owns it.
fn seeded_entry(summary: Value, seeded_cwd: Option<bool>) -> Value {
    json!(AgentRosterEntry {
        agent_id: roster_agent_id_for_summary(&summary),
        queued_child: None,
        seeded_cwd,
        status: classify_summary_value(&summary, false),
        status_label: None,
        last_heard_from_at: None,
        worker_id: None,
        summary,
        rest: Map::new(),
    })
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
        let mut entry = serde_json::to_value(entry)?;
        self.feeds.attach_bucket(&mut entry);
        Ok(entry)
    }

    /// The sessions the roster lists: each open session, then the subagents
    /// below it, depth first in spawn order; a dormant child (opened again
    /// and not run since) is listed as its seeded row instead.
    async fn roster_members(&self) -> Vec<String> {
        let mut roots: Vec<String> = self.open.paths().into_keys().collect();
        roots.sort();
        let mut members = Vec::new();
        for root in roots {
            let family = self.runtime.subagents().family_of(&root).await;
            members.push(root);
            let mut below: Vec<_> = family.into_iter().rev().collect();
            while let Some(member) = below.pop() {
                if !member.dormant {
                    members.push(member.session_id);
                }
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
        // The kept rows a live entry does not stand for.
        let live: HashSet<Value> = roster
            .iter()
            .map(|entry| entry["agentId"].clone())
            .collect();
        roster.extend(
            self.feeds
                .kept()
                .into_iter()
                .filter(|entry| !live.contains(&entry["agentId"])),
        );
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

    /// Reads the ledger's deleted descendants' spend again and publishes the
    /// entries whose spend it changes.
    async fn refresh_bucket(&self) {
        let subagents = self.runtime.subagents();
        let Some(family) = subagents.daemon() else {
            return;
        };
        match family.deleted_descendant_usage().await {
            Ok(bucket) => {
                let changed = self.feeds.rebill(bucket);
                if !changed.is_empty() {
                    self.publish_roster(json!({ "changed": changed })).await;
                }
            }
            Err(error) => tracing::warn!("the deleted-descendant spend was not read: {error:#}"),
        }
    }

    /// The ledger family below open root `id`'s transcript `file` as
    /// edge-only seeded rows, each with its child's session id; none when
    /// the ledger cannot be read.
    async fn family_seed(&self, file: PathBuf) -> Vec<(String, Value)> {
        let subagents = self.runtime.subagents();
        let Some(family) = subagents.daemon() else {
            return Vec::new();
        };
        let edges = match family.family_below(file).await {
            Ok(edges) => edges,
            Err(error) => {
                tracing::warn!("the roster was not seeded from the spawn ledger: {error:#}");
                return Vec::new();
            }
        };
        edges
            .iter()
            .map(|edge| {
                let mut entry = seeded_entry(seeded_child_summary(edge), Some(true));
                self.feeds.attach_bucket(&mut entry);
                let session = Path::new(&edge.child)
                    .file_stem()
                    .map_or_default(|stem| stem.to_string_lossy().into_owned());
                (session, entry)
            })
            .collect()
    }

    /// Hydrates the edge-only `seeded` rows from their children's
    /// transcripts, one read at a time off the async runtime, and publishes
    /// those the roster still holds as one update.
    async fn hydrate_seeded(&self, seeded: Vec<Value>) {
        let mut changed = Vec::new();
        for entry in seeded {
            let Some(file) = entry["summary"]["sessionFile"].as_str().map(PathBuf::from) else {
                continue;
            };
            let info = tokio::task::spawn_blocking(move || {
                pa_core::session::catalog::read_session_info(&file)
            })
            .await
            .ok()
            .flatten();
            let Some(info) = info else {
                continue;
            };
            let mut summary = entry["summary"].clone();
            hydrate_seeded_summary(&mut summary, &info);
            let mut hydrated = seeded_entry(summary, None);
            self.feeds.attach_bucket(&mut hydrated);
            if self.feeds.replace_kept(&entry, hydrated.clone()) {
                changed.push(hydrated);
            }
        }
        if !changed.is_empty() {
            self.publish_roster(json!({ "changed": changed })).await;
        }
    }

    /// Session `id` is due: its entry is composed and published when it
    /// changed; an open root's first entry brings its seeded family along,
    /// hydrated afterwards (answered); a session that left is passivated or
    /// removed.
    async fn compose_one(&self, id: &str) -> Vec<Value> {
        if !self.in_roster(id) {
            let (passivated, removed) = self.feeds.leave(id);
            if passivated.is_none() && removed.is_empty() {
                return Vec::new();
            }
            let mut update = json!({ "changed": passivated.into_iter().collect::<Vec<_>>() });
            if !removed.is_empty() {
                update["removed"] = json!(removed);
            }
            self.publish_roster(update).await;
            // A member that left may have taken a delete's tombstone along.
            self.refresh_bucket().await;
            return Vec::new();
        }
        if self.runtime.subagents().is_dormant(id).await {
            return Vec::new();
        }
        let first_root =
            self.open.contains(id) && lock(&self.feeds.seeded_roots).insert(id.to_string());
        if first_root {
            self.refresh_bucket().await;
        }
        // A session being rebuilt is composed again once it recovers (its
        // `recovered` makes it due).
        let entry = match self.roster_entry(id).await {
            Ok(entry) => entry,
            Err(error) => {
                if first_root {
                    lock(&self.feeds.seeded_roots).remove(id);
                }
                tracing::debug!("{id}'s roster entry was not composed: {error:#}");
                return Vec::new();
            }
        };
        if self.open.ephemeral(id) {
            lock(&self.feeds.ephemeral).insert(id.to_string());
        }
        let removed = self.feeds.supersede(&entry);
        let mut changed = Vec::new();
        if lock(&self.feeds.published)
            .insert(id.to_string(), entry.clone())
            .as_ref()
            != Some(&entry)
        {
            changed.push(entry.clone());
        }
        let mut seeded = Vec::new();
        if first_root {
            if let Some(file) = entry["summary"]["sessionFile"]
                .as_str()
                .filter(|_| !self.open.ephemeral(id))
            {
                let rows = self.family_seed(PathBuf::from(file)).await;
                seeded = self.feeds.seed(id, rows);
                changed.extend(seeded.iter().cloned());
            }
        }
        if !changed.is_empty() || !removed.is_empty() {
            let mut update = json!({ "changed": changed });
            if !removed.is_empty() {
                update["removed"] = json!(removed);
            }
            self.publish_roster(update).await;
        }
        seeded
    }

    /// Composes the due entries and publishes what changed, until the
    /// daemon ends; a root's seeded family is hydrated on its own task.
    async fn compose_due(self: &Arc<Self>) {
        loop {
            let due = std::mem::take(&mut *lock(&self.feeds.due));
            if due.is_empty() {
                self.feeds.wake.notified().await;
                continue;
            }
            for id in due {
                let seeded = self.compose_one(&id).await;
                if !seeded.is_empty() {
                    let handlers = Arc::clone(self);
                    tokio::spawn(async move { handlers.hydrate_seeded(seeded).await });
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
    /// with the rows seeded for it, and only entries that were published
    /// are removed.
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
        feeds.seed(
            "root",
            vec![("sub-3".to_string(), row("/r.jsonl#sub-3", None))],
        );
        let (passivated, mut removed) = feeds.leave("root");
        removed.sort();
        assert_eq!(
            (passivated, removed, feeds.leave("never")),
            (
                None,
                vec![
                    "/c.jsonl#sub-2".to_string(),
                    "/r.jsonl#sub-1".to_string(),
                    "/r.jsonl#sub-3".to_string(),
                    "root".to_string()
                ],
                (None, Vec::new())
            )
        );
    }
}
