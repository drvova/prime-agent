//! What a run shows of itself: the status payload, the graph snapshot (the
//! machine's structure fused with live state), the shape a watch treats as
//! a change, and the wire form the daemon and TUI read (camelCase keys, one
//! capped reply frame).

use serde_json::{json, Map, Value};

use super::run::{capped, Entry, Run, RunState, StateRun, Status, ANSWER_CAPTURE_CAP};
use super::spec::{
    field, MAX_TRANSITIONS_CAP, NODE_LIFECYCLE_DEFAULT, RUN_FAILURE_POLICY_DEFAULT,
    RUN_MAX_CHILDREN_DEFAULT, RUN_MAX_PARALLEL_DEFAULT,
};

/// Trailing ledger events `status` returns.
const EVENT_WINDOW: usize = 200;
/// Trailing fired transitions a graph snapshot reports for edge marking.
const LAST_FIRED_WINDOW: usize = 10;
/// Trailing ledger events a compact (daemon and TUI) snapshot carries.
const GRAPH_EVENTS_TAIL: usize = 40;
/// Serialized byte cap on one activity reply frame.
pub(super) const FACTORY_FRAME_CAP: usize = 262_144;

/// Whether a snapshot is the agent's own (full) or the compact one the
/// daemon and TUI render, which sheds answers and carries a shorter tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Detail {
    Full,
    Compact,
}

/// One entry's report row: its status and error plus, for an entry to
/// verify, the provisional answer the child exit left.
fn entry_report(entry: &Entry) -> Value {
    let mut row =
        json!({"index": entry.index, "status": entry.status.as_str(), "error": entry.error});
    if entry.needs_verify {
        row["needs_verify"] = json!(true);
        if let Some(provisional) = entry
            .instances
            .iter()
            .rev()
            .filter(|instance| instance.provisional)
            .find_map(|instance| {
                instance
                    .answer
                    .as_deref()
                    .filter(|answer| !answer.is_empty())
            })
        {
            row["provisional_answer"] = json!(capped(provisional, ANSWER_CAPTURE_CAP));
        }
    }
    row
}

/// One state's live report, the `status` node shape. `running` and `queued`
/// are the stage's occupancy: admitted children in flight, and prepared
/// instances waiting for a parallel slot.
fn state_report(state: &StateRun, detail: Detail) -> Value {
    let instances = || state.entries.iter().flat_map(|entry| &entry.instances);
    let count = |status: Status| {
        instances()
            .filter(|instance| instance.status == status)
            .count()
    };
    let rows: Vec<Value> = state
        .entries
        .iter()
        .flat_map(|entry| {
            entry.instances.iter().map(|instance| {
                json!({
                    "index": instance.index,
                    "entry": entry.index,
                    "status": instance.status.as_str(),
                    "attempt": instance.attempt,
                    "child": instance.child_id,
                    "duration_ms": instance.duration_ms,
                    "error": instance.error,
                })
            })
        })
        .collect();
    let mut report = json!({
        "id": state.state_id,
        "status": state.status(),
        "lifecycle": state.lifecycle(),
        "attempts": instances().map(|instance| instance.attempt).sum::<u64>(),
        "entries_used": state.entries_used,
        "max_entries": state.max_entries,
        "entries": state.entries.iter().map(entry_report).collect::<Vec<_>>(),
        "instances": rows,
        "running": count(Status::Running),
        "queued": count(Status::Pending),
    });
    if detail == Detail::Full {
        if let Some(answer) = state
            .latest_settle()
            .and_then(|latest| latest.answer.as_deref())
            .filter(|answer| !answer.is_empty())
        {
            report["answer_preview"] = json!(capped(answer, ANSWER_CAPTURE_CAP));
        }
    }
    if let Some(error) = &state.error {
        report["error"] = json!(error);
    }
    if state.entries.iter().any(|entry| entry.needs_verify) {
        report["needs_verify"] = json!(true);
    }
    report
}

/// A state's resolved spawn settings: model and thinking level.
pub(super) type SpawnSettings = (Option<String>, Option<String>);

/// A canonical machine's static graph structure: the run block, every
/// state's declared shape, the transitions with their guards, and the
/// declared state order. A live run passes its resolved spawn settings; a
/// spec graph has only the inline-declared ones.
pub(super) fn machine_structure(
    machine: &Value,
    resolved: &dyn Fn(&str) -> SpawnSettings,
) -> Value {
    let empty = Map::new();
    let run_block = machine["run"].as_object().unwrap_or(&empty);
    let setting = |key: &str, default: Value| run_block.get(key).cloned().unwrap_or(default);
    let mut run_out = json!({
        "max_parallel": setting("max_parallel", json!(RUN_MAX_PARALLEL_DEFAULT)),
        "max_transitions": setting("max_transitions", json!(MAX_TRANSITIONS_CAP)),
        "failure_policy": setting("failure_policy", json!(RUN_FAILURE_POLICY_DEFAULT)),
        "max_children": setting("max_children", json!(RUN_MAX_CHILDREN_DEFAULT)),
    });
    if let Some(budget) = run_block.get("budget_ms") {
        run_out["budget_ms"] = budget.clone();
    }
    let mut states_out: Vec<Value> = Vec::new();
    let mut order: Vec<Value> = Vec::new();
    for state in machine["states"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        let id = field(state, "id");
        order.push(id.clone());
        let or = |key: &str, default: Value| state.get(key).cloned().unwrap_or(default);
        let mut row = json!({
            "id": id,
            "entry": field(state, "entry") == &Value::Bool(true),
            "lifecycle": or("lifecycle", json!(NODE_LIFECYCLE_DEFAULT)),
            "max_entries": or("max_entries", json!(1)),
            "retries": or("retries", json!(0)),
        });
        let (mut model, mut thinking) = resolved(id.as_str().unwrap_or_default());
        let subagent = field(state, "subagent");
        if let Some(python) = state.get("python") {
            row["python"] = json!(format!(
                "{}.{}",
                python["import"].as_str().unwrap_or_default(),
                python["callable"].as_str().unwrap_or_default()
            ));
        } else if let Some(inline) = subagent.as_object() {
            model = model.or_else(|| field(inline, "model").as_str().map(str::to_string));
            thinking = thinking.or_else(|| field(inline, "thinking").as_str().map(str::to_string));
            if let Some(name) = field(inline, "name")
                .as_str()
                .filter(|name| !name.is_empty())
            {
                row["subagent"] = json!(name);
            }
        } else {
            row["subagent"] = subagent.clone();
        }
        if let Some(model) = model {
            row["model"] = json!(model);
        }
        if let Some(thinking) = thinking {
            row["thinking"] = json!(thinking);
        }
        states_out.push(row);
    }
    let transitions_out: Vec<Value> = machine["transitions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .map(|transition| {
            let mut row = json!({
                "from": field(transition, "from"),
                "to": field(transition, "to"),
                "on": transition.get("on").cloned().unwrap_or_else(|| json!("settled")),
            });
            if let Some(when) = transition.get("when") {
                row["when"] = when.clone();
            }
            row
        })
        .collect();
    json!({"run": run_out, "states": states_out, "transitions": transitions_out, "order": order})
}

impl Run {
    fn usage_report(&self) -> Value {
        json!({
            "spawns": self.spawn_count,
            "settled": self.settle_count,
            "tool_uses": self.tool_use_total,
            "max_parallel": self.max_parallel,
            "max_children": self.max_children,
            "running": self.running_instance_count(),
            "transitions_fired": self.transitions_fired,
        })
    }

    /// The `status` payload. Reading it marks the events the parent had not
    /// seen (`recorded`, `arrived`) `delivered`; `shown` events keep their
    /// stage, since their notice already reached the conversation.
    pub(super) fn status(&mut self) -> Value {
        let nodes: Vec<Value> = self
            .states
            .iter()
            .map(|state| state_report(state, Detail::Full))
            .collect();
        for event in &mut self.events {
            if matches!(event["stage"].as_str(), Some("recorded" | "arrived")) {
                event.insert("stage".to_string(), json!("delivered"));
            }
        }
        let window = self.events.len().saturating_sub(EVENT_WINDOW);
        let mut payload = json!({
            "run_id": self.id,
            "spec_id": self.spec_id,
            "name": self.name,
            "state": self.state.as_str(),
            "nodes": nodes,
            "events": self.events[window..],
            "elapsed_ms": self.elapsed_ms(),
            "usage": self.usage_report(),
        });
        // Every state whose entry keeps a provisional answer (a child
        // exit, or a settle whose declared output never bound).
        let needs_verify: Vec<&str> = self
            .states
            .iter()
            .filter(|state| state.entries.iter().any(|entry| entry.needs_verify))
            .map(|state| state.state_id.as_str())
            .collect();
        if !needs_verify.is_empty() {
            payload["needs_verify"] = json!(needs_verify);
        }
        // A paused run says what failed last and what to do next.
        if self.state == RunState::Paused {
            payload["pause_reason"] = json!(self.pause_reason);
            if let Some(last_error) = &self.last_error {
                payload["last_error"] = json!(last_error);
                payload["remedy"] = json!(format!(
                    "resume with (factory-resume \"{}\") to re-admit the \
                     failed state's entry as a fresh attempt (same rendered prompt, no extra \
                     max_entries); author the state with failure_policy 'continue' to run its \
                     dependents past a failure instead",
                    self.id
                ));
            }
        }
        payload
    }

    /// The trailing fired transitions, newest first, for a diagram's edge
    /// marking. An edge that fired twice keeps its latest firing; the guard
    /// is part of an edge's identity.
    fn last_fired(&self) -> Vec<Value> {
        let mut latest: Vec<(String, Value)> = Vec::new();
        for event in &self.events {
            if event["kind"] != "transition_fired" {
                continue;
            }
            let null = Value::Null;
            let part = |key: &str| event.get(key).unwrap_or(&null);
            let identity = format!("{}->{}@{}", part("from"), part("to"), part("when"));
            let edge = json!({
                "from": part("from"),
                "to": part("to"),
                "seq": part("seq"),
                "when": part("when"),
            });
            match latest.iter_mut().find(|(known, _)| *known == identity) {
                Some(slot) => slot.1 = edge,
                None => latest.push((identity, edge)),
            }
        }
        let mut edges: Vec<Value> = latest.into_iter().map(|(_, edge)| edge).collect();
        edges.sort_by_key(|edge| std::cmp::Reverse(edge["seq"].as_u64()));
        edges.truncate(LAST_FIRED_WINDOW);
        edges
    }

    /// The run's fused snapshot: the machine structure plus the live
    /// overlay. A pure read: only `status` marks events delivered.
    pub(super) fn graph_snapshot(&self, detail: Detail) -> Value {
        let elapsed_ms = self.elapsed_ms();
        let tail = match detail {
            Detail::Compact => GRAPH_EVENTS_TAIL,
            Detail::Full => EVENT_WINDOW,
        };
        let events: Vec<&Map<String, Value>> = self.events
            [self.events.len().saturating_sub(tail)..]
            .iter()
            .filter(|event| detail == Detail::Full || event["kind"] != "answer_captured")
            .collect();
        // A stage with live children stays active even when its entry
        // already failed: its occupancy can still be nonzero.
        let active: Vec<&str> = self
            .states
            .iter()
            .filter(|state| {
                state.entries.iter().any(|entry| {
                    entry.status.in_flight()
                        || entry
                            .instances
                            .iter()
                            .any(|instance| instance.status.in_flight())
                })
            })
            .map(|state| state.state_id.as_str())
            .collect();
        json!({
            "run_id": self.id,
            "spec_id": self.spec_id,
            "name": self.name,
            "state": self.state.as_str(),
            "pause_reason": self.pause_reason,
            "elapsed_ms": elapsed_ms,
            "machine": machine_structure(&self.machine, &|state_id| {
                self.states
                    .iter()
                    .find(|state| state.state_id == state_id)
                    .map_or((None, None), |state| (state.model.clone(), state.thinking.clone()))
            }),
            "nodes": self.states.iter().map(|state| state_report(state, detail)).collect::<Vec<_>>(),
            "active_nodes": active,
            "last_fired": self.last_fired(),
            "events": events,
            "usage": self.usage_report(),
            "budget": {"limit_ms": self.budget_ms, "consumed_ms": elapsed_ms},
        })
    }

    /// The live shape a watch treats as a change: the run state, every
    /// state's entries and instance statuses, and the transition counter.
    /// Ledger-only movement (a backoff notice, a repeated milestone) leaves
    /// it unchanged.
    pub(super) fn signature(&self) -> Value {
        let states: Vec<Value> = self
            .states
            .iter()
            .map(|state| {
                let entries: Vec<Value> = state
                    .entries
                    .iter()
                    .map(|entry| {
                        let instances: Vec<&str> = entry
                            .instances
                            .iter()
                            .map(|instance| instance.status.as_str())
                            .collect();
                        json!([entry.index, entry.status.as_str(), instances])
                    })
                    .collect();
                json!([state.state_id, state.status(), state.entries_used, entries])
            })
            .collect();
        json!([
            self.state.as_str(),
            self.pause_reason,
            self.transitions_fired,
            states
        ])
    }
}

/// A stored spec's static graph: no run exists, so the overlay is the
/// shape a fresh run starts from.
pub(super) fn spec_snapshot(spec_id: &str, canonical: &Value) -> Value {
    json!({
        "run_id": null,
        "spec_id": spec_id,
        "name": null,
        "state": null,
        "pause_reason": null,
        "elapsed_ms": 0,
        "machine": machine_structure(canonical, &|_| (None, None)),
        "nodes": [],
        "active_nodes": [],
        "last_fired": [],
        "events": [],
        "usage": null,
        "budget": {"limit_ms": canonical["run"].get("budget_ms"), "consumed_ms": 0},
    })
}

/// The wire form of a reply: every object key from `snake_case` to `camelCase`
/// (`run_id` to `runId`). A guard object (it has `output` and `op`) rides
/// verbatim, so the shown condition stays the machine's declared one.
pub(super) fn wire_payload(value: &Value) -> Value {
    match value {
        Value::Object(fields) if fields.contains_key("output") && fields.contains_key("op") => {
            value.clone()
        }
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| {
                    let mut parts = key.split('_');
                    let mut wire = parts.next().unwrap_or_default().to_string();
                    for part in parts {
                        let mut letters = part.chars();
                        if let Some(first) = letters.next() {
                            wire.extend(first.to_uppercase());
                            wire.push_str(&letters.as_str().to_lowercase());
                        }
                    }
                    (wire, wire_payload(item))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(wire_payload).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

/// Whether a wire run row is live: a live state, or children in flight.
fn wire_row_is_live(row: &Value) -> bool {
    matches!(
        row["state"].as_str(),
        Some("running" | "stopping" | "paused")
    ) || row["usage"]["running"]
        .as_u64()
        .is_some_and(|running| running > 0)
}

/// Keep one reply frame under [`FACTORY_FRAME_CAP`]. The events tail trims
/// from its oldest end first; an all-runs reply trims each row's tail the
/// same way before dropping whole rows, oldest droppable first. A live row
/// never drops silently, and at least one row always stays: a frame that
/// still cannot fit fails loudly instead of truncating.
pub(super) fn cap_frame(frame: &mut Value) {
    while frame.to_string().len() > FACTORY_FRAME_CAP {
        let result = &mut frame["result"];
        if let Some(events) = result
            .get_mut("events")
            .and_then(Value::as_array_mut)
            .filter(|events| events.len() > 1)
        {
            events.remove(0);
            continue;
        }
        if let Some(runs) = result.get_mut("runs").and_then(Value::as_array_mut) {
            if let Some(tail) = runs
                .iter_mut()
                .filter_map(|row| row.get_mut("events").and_then(Value::as_array_mut))
                .find(|tail| tail.len() > 1)
            {
                tail.remove(0);
                continue;
            }
            if let Some(droppable) = (runs.len() > 1)
                .then(|| runs.iter().position(|row| !wire_row_is_live(row)))
                .flatten()
            {
                runs.remove(droppable);
                continue;
            }
        }
        *frame = json!({
            "event": "done",
            "id": frame["id"],
            "status": "error",
            "reason": "factory activity reply exceeds the wire cap",
        });
        return;
    }
}
