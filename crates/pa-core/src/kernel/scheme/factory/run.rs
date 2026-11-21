//! One factory run's state: its states, their entries and spawned
//! instances, the event ledger, and every step that needs no host call:
//! entering a state, binding inputs and expanding `foreach`, capturing a
//! settle's outputs, evaluating which transitions a settle fires, and
//! telling when the run has gone quiet.
//!
//! The executor owns the order of these steps and everything that waits on
//! the host; a run is only touched under its lock, between two waits.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use serde_json::{json, Map, Value};
use tokio::time::Instant;

use super::binding::{guard_passes, parse_json_output, render_prompt};
use super::python_state::PythonCall;
use super::python_text::{json_dumps, repr, repr_str};
use super::spec::{declared_port_types, field, NODE_LIFECYCLE_DEFAULT};

/// Cap of a captured-answer preview: the `answer_captured` ledger event and
/// a status node's `answer_preview`.
pub(super) const ANSWER_CAPTURE_CAP: usize = 200;
/// Cap of the answer a settle binds from, the size the host bounds a
/// child's final answer text at.
pub(super) const ANSWER_BINDING_CAP: usize = 65_536;

/// The first `cap` characters of a text.
pub(super) fn capped(text: &str, cap: usize) -> String {
    match text.char_indices().nth(cap) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

/// Where an instance or an entry stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Pending,
    Running,
    Done,
    Error,
    Cancelled,
}

impl Status {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Done => "done",
            Status::Error => "error",
            Status::Cancelled => "cancelled",
        }
    }

    /// Pending or running: still to be worked on.
    pub(super) fn in_flight(self) -> bool {
        matches!(self, Status::Pending | Status::Running)
    }
}

/// Where a run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunState {
    Running,
    Stopping,
    Paused,
    Done,
    Failed,
    Stopped,
}

impl RunState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Stopping => "stopping",
            RunState::Paused => "paused",
            RunState::Done => "done",
            RunState::Failed => "failed",
            RunState::Stopped => "stopped",
        }
    }

    /// Running, stopping or paused: the run may still act.
    pub(super) fn is_live(self) -> bool {
        matches!(
            self,
            RunState::Running | RunState::Stopping | RunState::Paused
        )
    }
}

/// One spawned child of one state entry (a foreach entry has one per item),
/// or one python call.
pub(super) struct Instance {
    /// Per-state running counter; unique within the state.
    pub(super) index: u64,
    /// Fully rendered; a re-spawn reuses it verbatim.
    pub(super) prompt: String,
    pub(super) status: Status,
    /// Spawn admissions tried, rate-limit deferrals included.
    pub(super) attempt: u64,
    /// The attempt count at the last retry-budget reset: an infra respawn
    /// or a resume grants fresh retries by moving this base, so the attempt
    /// counter the spawn labels count on stays monotonic.
    pub(super) retry_base: u64,
    /// The one free respawn after a verified worker death is spent.
    pub(super) infra_respawned: bool,
    /// The settle completed but a declared output never bound.
    pub(super) capture_failed: bool,
    /// Consecutive rate-limited admissions in the control loop.
    pub(super) rate_limit_streak: u32,
    pub(super) child_id: Option<String>,
    pub(super) spawned_at: Option<Instant>,
    pub(super) duration_ms: Option<u64>,
    /// The answer the settle binds from.
    pub(super) answer: Option<String>,
    /// The answer came from a child exit, not a settle.
    pub(super) provisional: bool,
    pub(super) error: Option<String>,
    pub(super) tool_uses: u64,
    /// A python state's bound inputs; a re-run reuses them.
    pub(super) inputs: Option<Map<String, Value>>,
    /// A python state's running call.
    pub(super) call: Option<PythonCall>,
    /// A python call's time limit.
    pub(super) deadline: Option<Instant>,
}

impl Instance {
    fn new(index: u64, prompt: String, inputs: Option<Map<String, Value>>) -> Self {
        Instance {
            index,
            prompt,
            status: Status::Pending,
            attempt: 0,
            retry_base: 0,
            infra_respawned: false,
            capture_failed: false,
            rate_limit_streak: 0,
            child_id: None,
            spawned_at: None,
            duration_ms: None,
            answer: None,
            provisional: false,
            error: None,
            tool_uses: 0,
            inputs,
            call: None,
            deadline: None,
        }
    }
}

/// One entry (activation) of a state; re-entry creates a fresh one. An
/// entry settles when all its instances are done; the settle captures the
/// state's declared outputs and its outgoing transitions are evaluated once.
pub(super) struct Entry {
    pub(super) index: usize,
    pub(super) status: Status,
    pub(super) instances: Vec<Instance>,
    pub(super) error: Option<String>,
    /// The joined captured answers of this entry.
    pub(super) answer: Option<String>,
    /// Captured settle outputs, port name to value.
    pub(super) outputs: Map<String, Value>,
    /// Ports whose JSON capture failed, with the reason.
    pub(super) output_errors: Map<String, Value>,
    pub(super) is_settle: bool,
    /// The settle's transitions were evaluated.
    pub(super) consumed: bool,
    /// The entry failed but keeps a provisional answer to verify.
    pub(super) needs_verify: bool,
}

/// One machine state of one run.
pub(super) struct StateRun {
    pub(super) state_id: String,
    /// The canonical state spec.
    pub(super) spec: Map<String, Value>,
    pub(super) prompt_template: Option<String>,
    /// The configured inline subagent name; labels the children.
    pub(super) name: Option<String>,
    pub(super) model: Option<String>,
    pub(super) thinking: Option<String>,
    pub(super) max_entries: u64,
    pub(super) entries_used: u64,
    pub(super) entries: Vec<Entry>,
    pub(super) instance_counter: u64,
    pub(super) error: Option<String>,
    /// Set for a never-entered state by a stop or a fail-fast cascade.
    pub(super) cancelled: bool,
}

impl StateRun {
    pub(super) fn is_resident(&self) -> bool {
        field(&self.spec, "lifecycle").as_str() == Some("resident")
    }

    pub(super) fn is_python(&self) -> bool {
        self.spec.contains_key("python")
    }

    pub(super) fn status(&self) -> &'static str {
        match self.entries.last() {
            Some(entry) => entry.status.as_str(),
            None if self.cancelled => "cancelled",
            None => "pending",
        }
    }

    pub(super) fn latest_settle(&self) -> Option<&Entry> {
        self.entries.iter().rev().find(|entry| entry.is_settle)
    }

    pub(super) fn lifecycle(&self) -> &str {
        field(&self.spec, "lifecycle")
            .as_str()
            .unwrap_or(NODE_LIFECYCLE_DEFAULT)
    }
}

/// An instance's place in its run: state position, entry index, and
/// position among the entry's instances. Entries and instances are only
/// ever appended, so a place stays valid across waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Place {
    pub(super) state: usize,
    pub(super) entry: usize,
    pub(super) instance: usize,
}

/// Which node, entry and instance a ledger event is about.
#[derive(Default, Clone, Copy)]
pub(super) struct About<'a> {
    pub(super) node: Option<&'a str>,
    pub(super) entry: Option<usize>,
    pub(super) instance: Option<u64>,
}

/// What preparing a pending entry found.
pub(super) enum Prepared {
    /// Inputs bound: the instances to run (none for an empty foreach).
    Ready(Vec<Instance>),
    /// An input source has not settled yet; the entry stays pending.
    Waiting,
    /// A binding failure; the entry's failure policy applies.
    Failed(String),
}

/// A pause the settle evaluation ran into, for the executor to announce.
pub(super) struct TransitionPause {
    pub(super) detail: String,
}

/// One run. It lives in kernel memory only: it does not survive a kernel
/// restart, while its children, owned by the supervisor, keep running.
pub(super) struct Run {
    pub(super) id: String,
    pub(super) spec_id: String,
    pub(super) name: Option<String>,
    /// The canonical machine this run executes.
    pub(super) machine: Value,
    pub(super) state: RunState,
    pub(super) started_at: Instant,
    pub(super) max_parallel: u64,
    pub(super) max_transitions: u64,
    pub(super) max_children: u64,
    pub(super) max_transitions_reported: bool,
    pub(super) max_children_reported: bool,
    pub(super) budget_ms: Option<u64>,
    pub(super) budget_reported: bool,
    pub(super) pause_reason: Option<String>,
    /// The last entry-failure reason, for a paused run's status.
    pub(super) last_error: Option<String>,
    /// In declaration order.
    pub(super) states: Vec<StateRun>,
    positions: HashMap<String, usize>,
    /// Indices into the machine's transitions, per source state.
    transitions_from: HashMap<String, Vec<usize>>,
    /// Settles whose transitions are still to evaluate: state, entry, and
    /// the transition to resume from after a `max_transitions` pause.
    pub(super) pending_evaluations: VecDeque<(usize, usize, usize)>,
    pub(super) events: Vec<Map<String, Value>>,
    /// Milestone kinds already announced to the parent conversation.
    pub(super) milestones: HashSet<String>,
    pub(super) spawn_count: u64,
    pub(super) settle_count: u64,
    pub(super) transitions_fired: u64,
    pub(super) tool_use_total: u64,
    /// The control loop's generation: a resume bumps it, so a loop still
    /// winding down from the pause can never continue as a second loop.
    pub(super) loop_generation: u64,
    /// Admission waits until then after a rate limit.
    pub(super) admission_backoff_until: Option<Instant>,
    /// The source settles a join last fired (or was blocked) for, per
    /// transition: a join fires once per combination of source settles.
    join_fired: HashMap<usize, BTreeSet<(String, usize)>>,
    /// Bumped on every observable change; watchers wake on it.
    pub(super) revision: tokio::sync::watch::Sender<u64>,
}

/// A state's resolved subagent: prompt template, configured name, model,
/// thinking level.
pub(super) type ResolvedSubagent = (String, Option<String>, Option<String>, Option<String>);

impl Run {
    /// A new run of a canonical machine whose subagent references are
    /// resolved (python states have none).
    pub(super) fn new(
        spec_id: String,
        canonical: Value,
        mut resolved: HashMap<String, ResolvedSubagent>,
        name: Option<String>,
    ) -> Self {
        let run_spec = &canonical["run"];
        let mut states = Vec::new();
        let mut positions = HashMap::new();
        for spec in canonical["states"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let state_id = field(spec, "id").as_str().unwrap_or_default().to_string();
            let (prompt_template, configured, model, thinking) = match resolved.remove(&state_id) {
                Some((prompt, configured, model, thinking)) => {
                    (Some(prompt), configured, model, thinking)
                }
                None => (None, None, None, None),
            };
            positions.insert(state_id.clone(), states.len());
            states.push(StateRun {
                max_entries: field(spec, "max_entries").as_u64().unwrap_or(1),
                state_id,
                spec: spec.clone(),
                prompt_template,
                name: configured,
                model,
                thinking,
                entries_used: 0,
                entries: Vec::new(),
                instance_counter: 0,
                error: None,
                cancelled: false,
            });
        }
        // A join is visible from every source state's settle evaluation.
        let mut transitions_from: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, transition) in canonical["transitions"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let sources = match &transition["from"] {
                Value::Array(sources) => sources.iter().filter_map(Value::as_str).collect(),
                single => single.as_str().into_iter().collect::<Vec<_>>(),
            };
            for source in sources {
                transitions_from
                    .entry(source.to_string())
                    .or_default()
                    .push(index);
            }
        }
        Run {
            id: uuid::Uuid::new_v4().simple().to_string(),
            spec_id,
            name,
            state: RunState::Running,
            started_at: Instant::now(),
            max_parallel: run_spec["max_parallel"].as_u64().unwrap_or(8),
            max_transitions: run_spec["max_transitions"].as_u64().unwrap_or(10_000),
            max_children: run_spec["max_children"].as_u64().unwrap_or(10_000),
            max_transitions_reported: false,
            max_children_reported: false,
            budget_ms: run_spec["budget_ms"].as_u64(),
            budget_reported: false,
            pause_reason: None,
            last_error: None,
            states,
            positions,
            transitions_from,
            pending_evaluations: VecDeque::new(),
            events: Vec::new(),
            milestones: HashSet::new(),
            spawn_count: 0,
            settle_count: 0,
            transitions_fired: 0,
            tool_use_total: 0,
            loop_generation: 0,
            admission_backoff_until: None,
            join_fired: HashMap::new(),
            revision: tokio::sync::watch::Sender::new(0),
            machine: canonical,
        }
    }

    /// Milliseconds since the run started.
    pub(super) fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Wake every watcher: something observable changed.
    pub(super) fn touch(&self) {
        self.revision.send_modify(|revision| *revision += 1);
    }

    /// Append one ledger event and return its position. Stages: `arrived`
    /// (a child answer was captured), `recorded` (everything else), later
    /// `shown` (a milestone notice reached the parent conversation) and
    /// `delivered` (the parent read it through status).
    pub(super) fn event(
        &mut self,
        kind: &str,
        about: About<'_>,
        detail: Option<String>,
        extra: Value,
    ) -> usize {
        let mut event = Map::new();
        event.insert("seq".to_string(), json!(self.events.len() + 1));
        event.insert("kind".to_string(), json!(kind));
        event.insert("stage".to_string(), json!("recorded"));
        if let Some(node) = about.node {
            event.insert("node".to_string(), json!(node));
        }
        if let Some(entry) = about.entry {
            event.insert("entry".to_string(), json!(entry));
        }
        if let Some(instance) = about.instance {
            event.insert("instance".to_string(), json!(instance));
        }
        if let Some(detail) = detail {
            event.insert("detail".to_string(), json!(detail));
        }
        if let Value::Object(extra) = extra {
            event.extend(extra);
        }
        self.events.push(event);
        self.touch();
        self.events.len() - 1
    }

    /// An event about one instance.
    pub(super) fn instance_event(
        &mut self,
        kind: &str,
        place: Place,
        detail: Option<String>,
        extra: Value,
    ) -> usize {
        let node = self.states[place.state].state_id.clone();
        let instance =
            self.states[place.state].entries[place.entry].instances[place.instance].index;
        self.event(
            kind,
            About {
                node: Some(&node),
                entry: Some(place.entry),
                instance: Some(instance),
            },
            detail,
            extra,
        )
    }

    /// An event about one entry.
    pub(super) fn entry_event(
        &mut self,
        kind: &str,
        state: usize,
        entry: usize,
        detail: Option<String>,
        extra: Value,
    ) -> usize {
        let node = self.states[state].state_id.clone();
        self.event(
            kind,
            About {
                node: Some(&node),
                entry: Some(entry),
                instance: None,
            },
            detail,
            extra,
        )
    }

    pub(super) fn instance(&self, place: Place) -> &Instance {
        &self.states[place.state].entries[place.entry].instances[place.instance]
    }

    pub(super) fn instance_mut(&mut self, place: Place) -> &mut Instance {
        &mut self.states[place.state].entries[place.entry].instances[place.instance]
    }

    /// Every instance's place, in state, entry, instance order.
    pub(super) fn places(&self) -> Vec<Place> {
        let mut places = Vec::new();
        for (state, state_run) in self.states.iter().enumerate() {
            for (entry, entry_run) in state_run.entries.iter().enumerate() {
                for instance in 0..entry_run.instances.len() {
                    places.push(Place {
                        state,
                        entry,
                        instance,
                    });
                }
            }
        }
        places
    }

    /// Create one new entry of a state (bounded by `max_entries` upstream).
    pub(super) fn enter_state(&mut self, state: usize, from_state: Option<&str>) {
        let index = self.states[state].entries.len();
        self.states[state].entries.push(Entry {
            index,
            status: Status::Pending,
            instances: Vec::new(),
            error: None,
            answer: None,
            outputs: Map::new(),
            output_errors: Map::new(),
            is_settle: false,
            consumed: false,
            needs_verify: false,
        });
        self.states[state].entries_used += 1;
        let detail = match from_state {
            None => "entry state".to_string(),
            Some(from_state) => format!("entered from {from_state}"),
        };
        self.entry_event("state_entry", state, index, Some(detail), Value::Null);
    }

    /// Mark an entry settled and queue its transitions for evaluation.
    pub(super) fn queue_settle(&mut self, state: usize, entry: usize) {
        self.states[state].entries[entry].is_settle = true;
        self.pending_evaluations.push_back((state, entry, 0));
    }

    /// Evaluate every queued settle's outgoing transitions once. Every
    /// transition whose guard passes fires (fan-out is legal); a join fires
    /// once per combination of its sources' settles; a fire enters the
    /// target unless it is out of `max_entries` (recorded as
    /// `transition_blocked`). Reaching `max_transitions` pauses the run
    /// once, leaving the settle unconsumed at the transition where the
    /// pause landed, so a resume continues after the ones that fired.
    pub(super) fn evaluate_settles(&mut self) -> Option<TransitionPause> {
        while self.state == RunState::Running {
            let (state, entry, resume_from) = self.pending_evaluations.pop_front()?;
            if self.states[state].entries[entry].consumed
                || !self.states[state].entries[entry].is_settle
            {
                continue;
            }
            self.states[state].entries[entry].consumed = true;
            let state_id = self.states[state].state_id.clone();
            let outgoing = self
                .transitions_from
                .get(&state_id)
                .cloned()
                .unwrap_or_default();
            for (position, transition_index) in outgoing.into_iter().enumerate() {
                if position < resume_from {
                    continue;
                }
                let transition = self.machine["transitions"][transition_index].clone();
                let from_field = &transition["from"];
                let mut join_signature: Option<BTreeSet<(String, usize)>> = None;
                if let Some(sources) = from_field.as_array() {
                    let signature: Option<BTreeSet<(String, usize)>> = sources
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|source| {
                            self.positions
                                .get(source)
                                .and_then(|position| self.states[*position].latest_settle())
                                .map(|latest| (source.to_string(), latest.index))
                        })
                        .collect();
                    // A source that never settled: the join cannot fire yet.
                    let Some(signature) = signature else {
                        continue;
                    };
                    if self.join_fired.get(&transition_index) == Some(&signature) {
                        continue;
                    }
                    // Claimed only when the transition fires or is blocked:
                    // a pause that re-queues it must leave no stale mark.
                    join_signature = Some(signature);
                } else if let Some(when) = transition["when"].as_object() {
                    if !guard_passes(when, &self.states[state].entries[entry].outputs) {
                        continue;
                    }
                }
                let target_id = transition["to"].as_str().unwrap_or_default().to_string();
                let Some(target) = self.positions.get(&target_id).copied() else {
                    continue;
                };
                let edge = format!("{} -> {}", repr(from_field), repr_str(&target_id));
                if self.states[target].entries_used >= self.states[target].max_entries {
                    if let Some(signature) = join_signature {
                        self.join_fired.insert(transition_index, signature);
                    }
                    let max_entries = self.states[target].max_entries;
                    self.event(
                        "transition_blocked",
                        About::default(),
                        Some(format!(
                            "state {} is at max_entries {max_entries}; transition {edge} blocked",
                            repr_str(&target_id)
                        )),
                        json!({"from": from_field, "to": target_id}),
                    );
                    continue;
                }
                if self.transitions_fired >= self.max_transitions && !self.max_transitions_reported
                {
                    self.max_transitions_reported = true;
                    self.states[state].entries[entry].consumed = false;
                    self.pending_evaluations
                        .push_front((state, entry, position));
                    self.state = RunState::Paused;
                    self.pause_reason = Some("max_transitions exceeded".to_string());
                    return Some(TransitionPause {
                        detail: format!(
                            "max_transitions {} exceeded; no new entries; \
                             resume with (factory-resume \"{}\")",
                            self.max_transitions, self.id
                        ),
                    });
                }
                if let Some(signature) = join_signature {
                    self.join_fired.insert(transition_index, signature);
                }
                self.transitions_fired += 1;
                // The guard rides the fired event: two guarded transitions
                // may share one from+to pair, and only the guard tells
                // which of them fired.
                let mut fired = json!({"from": from_field, "to": target_id});
                if !transition["when"].is_null() {
                    fired["when"] = transition["when"].clone();
                }
                self.event("transition_fired", About::default(), Some(edge), fired);
                self.enter_state(target, Some(&state_id));
            }
        }
        None
    }

    /// Bind a pending entry's inputs, expand `foreach`, and render one
    /// prompt per instance (a python state gets one instance carrying the
    /// bound values). Binding failures never retry: they would recur on
    /// every re-render.
    pub(super) fn prepare_entry(&mut self, state: usize) -> Prepared {
        let state_run = &self.states[state];
        let foreach = field(&state_run.spec, "foreach").as_object();
        let over = foreach.and_then(|foreach| field(foreach, "over").as_str());
        let mut bound: Vec<(String, Value)> = Vec::new();
        let mut items: Option<Vec<Value>> = None;
        let inputs: Vec<&Map<String, Value>> = field(&state_run.spec, "inputs")
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .collect();
        for input in &inputs {
            let name = field(input, "name").as_str().unwrap_or_default();
            let port_type = field(input, "type").as_str().unwrap_or_default();
            let (source_id, source_output) = field(input, "from")
                .as_str()
                .and_then(|source| source.split_once('.'))
                .unwrap_or_default();
            let source_state = self
                .positions
                .get(source_id)
                .map(|position| &self.states[*position]);
            let latest = source_state.and_then(StateRun::latest_settle);
            // A settled source offers a captured value or a binding
            // failure: an errored settle, a port whose capture failed, and
            // a port the settle captured nothing for are all no-value.
            let offered: Option<Result<&Value, String>> = latest.map(|latest| {
                if latest.status == Status::Error {
                    Err(format!(
                        "input {} from state {} is unavailable (latest settle status 'error')",
                        repr_str(name),
                        repr_str(source_id)
                    ))
                } else if let Some(error) = latest.output_errors.get(source_output) {
                    Err(format!(
                        "input {}: {}",
                        repr_str(name),
                        error.as_str().unwrap_or_default()
                    ))
                } else {
                    latest.outputs.get(source_output).ok_or_else(|| {
                        format!(
                            "input {} from state {} has no captured output {}",
                            repr_str(name),
                            repr_str(source_id),
                            repr_str(source_output)
                        )
                    })
                }
            });
            let value = match offered {
                Some(Ok(value)) => value,
                no_value => {
                    if field(input, "optional") == &Value::Bool(true) {
                        // An optional input over another state with a live
                        // entry waits: that source is about to settle, and
                        // a sentinel now would spawn the dependent while
                        // its upstream still runs. The sentinel binds for
                        // the self-input loop form and for a source with
                        // no live entry.
                        if no_value.is_none()
                            && source_id != state_run.state_id
                            && source_state.is_some_and(|source| {
                                source.entries.iter().any(|other| other.status.in_flight())
                            })
                        {
                            return Prepared::Waiting;
                        }
                        // The foreach input cannot bind a sentinel: a
                        // value-less optional one expands to zero items,
                        // and a later re-entry binds the real list.
                        if over == Some(name) {
                            items = Some(Vec::new());
                        } else {
                            bound.push((name.to_string(), Value::Null));
                        }
                        continue;
                    }
                    return match no_value {
                        Some(Err(failure)) => Prepared::Failed(failure),
                        Some(Ok(_)) | None => Prepared::Waiting,
                    };
                }
            };
            if port_type == "text" {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => json_dumps(other),
                };
                bound.push((name.to_string(), Value::String(text)));
            } else if over == Some(name) {
                let Some(listed) = value.as_array() else {
                    return Prepared::Failed(format!(
                        "foreach.over input {} is not a JSON list",
                        repr_str(name)
                    ));
                };
                items = Some(listed.clone());
            } else {
                bound.push((name.to_string(), value.clone()));
            }
        }
        let first = state_run.instance_counter;
        if state_run.is_python() {
            self.states[state].instance_counter += 1;
            return Prepared::Ready(vec![Instance::new(
                first,
                String::new(),
                Some(bound.into_iter().collect()),
            )]);
        }
        // The prompt form of a bound value: a json input as JSON text (a
        // missing optional one reads "null"), a text input verbatim (a
        // missing optional one reads "None").
        let json_ports: HashSet<&str> = inputs
            .iter()
            .filter(|input| field(input, "type").as_str() == Some("json"))
            .filter_map(|input| field(input, "name").as_str())
            .collect();
        let mut values: Vec<(String, String)> = bound
            .iter()
            .map(|(name, value)| {
                let text = match value {
                    value if json_ports.contains(name.as_str()) => json_dumps(value),
                    Value::String(text) => text.clone(),
                    _ => "None".to_string(),
                };
                (name.clone(), text)
            })
            .collect();
        let template = state_run.prompt_template.clone().unwrap_or_default();
        let Some(foreach) = foreach else {
            self.states[state].instance_counter += 1;
            return Prepared::Ready(vec![Instance::new(
                first,
                render_prompt(&template, &values),
                None,
            )]);
        };
        let Some(items) = items else {
            return Prepared::Failed("foreach entry did not resolve its over input".to_string());
        };
        let over = over.unwrap_or_default().to_string();
        let max = usize::try_from(field(foreach, "max").as_u64().unwrap_or(0)).unwrap_or(0);
        values.retain(|(name, _)| *name != over);
        values.push((over, String::new()));
        let mut instances = Vec::new();
        for (offset, item) in items.iter().take(max).enumerate() {
            let item_text = match item {
                Value::String(text) => text.clone(),
                other => json_dumps(other),
            };
            if let Some(slot) = values.last_mut() {
                slot.1 = item_text;
            }
            instances.push(Instance::new(
                first + offset as u64,
                render_prompt(&template, &values),
                None,
            ));
        }
        self.states[state].instance_counter += instances.len() as u64;
        Prepared::Ready(instances)
    }

    /// The next instance to admit. Only running entries are served: a
    /// foreach entry that already failed must never run its queued
    /// siblings.
    pub(super) fn next_pending_instance(&self) -> Option<Place> {
        self.places().into_iter().find(|place| {
            self.states[place.state].entries[place.entry].status == Status::Running
                && self.instance(*place).status == Status::Pending
        })
    }

    /// The entry's answer to bind from: its one answer, or all its done
    /// instances' answers joined.
    pub(super) fn entry_answer(&self, state: usize, entry: usize) -> Option<String> {
        let answers: Vec<&str> = self.states[state].entries[entry]
            .instances
            .iter()
            .filter(|instance| instance.status == Status::Done)
            .filter_map(|instance| instance.answer.as_deref())
            .filter(|answer| !answer.is_empty())
            .collect();
        (!answers.is_empty()).then(|| answers.join("\n\n"))
    }

    /// Capture the state's declared output ports from the entry's answer:
    /// a text port keeps the answer, a json port parses, and a failed parse
    /// is recorded on the settle so every reader fails the same way.
    pub(super) fn capture_ports(&mut self, state: usize, entry: usize) {
        let mut outputs = Map::new();
        let mut errors = Map::new();
        let state_run = &self.states[state];
        if let Some(answer) = &state_run.entries[entry].answer {
            let length = answer.chars().count();
            let truncated_at = (length >= ANSWER_BINDING_CAP).then_some(length);
            for (name, port_type) in declared_port_types(&state_run.spec, "outputs") {
                if port_type == "text" {
                    outputs.insert(name.to_string(), json!(answer));
                    continue;
                }
                match parse_json_output(answer, name, truncated_at) {
                    Ok(value) => outputs.insert(name.to_string(), value),
                    Err(error) => errors.insert(name.to_string(), json!(error)),
                };
            }
        }
        let entry_run = &mut self.states[state].entries[entry];
        entry_run.outputs = outputs;
        entry_run.output_errors = errors;
    }

    /// The declared output names of a state, in order.
    pub(super) fn declared_outputs(&self, state: usize) -> Vec<String> {
        declared_port_types(&self.states[state].spec, "outputs")
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect()
    }

    pub(super) fn running_instance_count(&self) -> u64 {
        self.places()
            .into_iter()
            .filter(|place| self.instance(*place).status == Status::Running)
            .count() as u64
    }

    pub(super) fn has_pending_instance(&self) -> bool {
        self.places()
            .into_iter()
            .any(|place| self.instance(place).status == Status::Pending)
    }

    /// Whether queued instances can never be admitted: every `max_parallel`
    /// slot is held by a resident instance, which never settles.
    pub(super) fn resident_cap_starved(&self) -> bool {
        let running: Vec<bool> = self
            .places()
            .into_iter()
            .filter(|place| self.instance(*place).status == Status::Running)
            .map(|place| self.states[place.state].is_resident())
            .collect();
        running.len() as u64 >= self.max_parallel
            && self.has_pending_instance()
            && running.iter().all(|resident| *resident)
    }

    /// Quiescence: no unevaluated settle, no entry in flight, and no
    /// instance queued or awaiting settlement. An admitted resident
    /// instance is the one exception: it stays alive until the run is
    /// stopped and never blocks completion, though a queued one does.
    pub(super) fn is_complete(&self) -> bool {
        if !self.pending_evaluations.is_empty() {
            return false;
        }
        self.states.iter().all(|state| {
            let resident = state.is_resident();
            state.entries.iter().all(|entry| {
                let resident_running = resident && entry.status == Status::Running;
                (!entry.status.in_flight() || resident_running)
                    && entry
                        .instances
                        .iter()
                        .all(|instance| match instance.status {
                            Status::Pending => false,
                            Status::Running => resident_running,
                            Status::Done | Status::Error | Status::Cancelled => true,
                        })
            })
        })
    }

    pub(super) fn pending_state_ids(&self) -> Vec<String> {
        self.states
            .iter()
            .filter(|state| state.status() == "pending")
            .map(|state| state.state_id.clone())
            .collect()
    }

    pub(super) fn budget_exceeded(&self) -> bool {
        match self.budget_ms {
            Some(budget) if !self.budget_reported => {
                self.started_at.elapsed() > std::time::Duration::from_millis(budget)
            }
            Some(_) | None => false,
        }
    }
}
