//! The write-time dry run of a factory specification: the machine
//! validator, the dag-to-machine compiler, the entry point that detects the
//! form, and the canonicalizer that applies defaults and returns the one
//! shape the executor runs (`{"run": ..., "states": [...], "transitions":
//! [...]}`).
//!
//! Validation reports every problem as a sentence and never fails itself.
//! The sentences are the ones factory authors already know, values quoted
//! the way Python quoted them.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use serde_json::{json, Map, Value};

use super::naming::suffixed_spawn_form;
use super::python_text::{repr, repr_str, repr_words, truthy};

pub(super) const FAILURE_POLICIES: [&str; 3] = ["fail_fast", "continue", "escalate"];
const PORT_TYPES: [&str; 2] = ["text", "json"];
const LIFECYCLES: [&str; 2] = ["task", "resident"];
const TRANSITION_ON_KINDS: [&str; 1] = ["settled"];
const GUARD_OPS: [&str; 8] = ["eq", "ne", "gt", "gte", "lt", "lte", "exists", "contains"];
const MAX_STATES: usize = 1024;
const MAX_RETRIES: i128 = 10;
const MAX_PARALLEL_MIN: i128 = 1;
const MAX_PARALLEL_MAX: i128 = 64;
const FOREACH_MAX_MIN: i128 = 1;
const FOREACH_MAX_MAX: i128 = 256;
pub(super) const MAX_TRANSITIONS_CAP: u64 = 10_000;
const MAX_CHILDREN_CAP: i128 = 1_000_000;
const TRANSITIONS_PER_STATE_DEFAULT: u64 = 10;
pub(super) const RUN_FAILURE_POLICY_DEFAULT: &str = "escalate";
pub(super) const RUN_MAX_PARALLEL_DEFAULT: u64 = 8;
pub(super) const RUN_MAX_CHILDREN_DEFAULT: u64 = 10_000;
pub(super) const NODE_LIFECYCLE_DEFAULT: &str = "task";
/// The host caps subagent session names at 64 characters, so a longer
/// configured inline `name` is rejected at write time instead of failing
/// every spawn admission.
pub(super) const SUBAGENT_NAME_MAX_LENGTH: usize = 64;
/// Nesting bound on one guard comparison value: deeper values cannot ride
/// the reply frames, so they reject with the validation answer.
const MAX_GUARD_VALUE_DEPTH: usize = 256;

static NULL: Value = Value::Null;

/// An object's field, or null when absent (Python's `dict.get`).
pub(super) fn field<'a>(object: &'a Map<String, Value>, key: &str) -> &'a Value {
    object.get(key).unwrap_or(&NULL)
}

/// A real integer: booleans and floats are not accepted.
fn integer(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

fn nonempty_str(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

/// A state or node id: `[a-z0-9][a-z0-9-]{0,63}`.
fn valid_node_id(id: &str) -> bool {
    let mut letters = id.chars();
    letters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && id.len() <= 64
        && letters
            .all(|letter| letter.is_ascii_lowercase() || letter.is_ascii_digit() || letter == '-')
}

/// The state's inputs/outputs list, or nothing when absent or malformed.
fn port_list<'a>(node: &'a Map<String, Value>, key: &str) -> &'a [Value] {
    node.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Port name to type for the well-formed entries of a port list.
pub(super) fn declared_port_types<'a>(
    node: &'a Map<String, Value>,
    key: &str,
) -> Vec<(&'a str, &'a str)> {
    let mut ports: Vec<(&str, &str)> = Vec::new();
    for entry in port_list(node, key) {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let (Some(name), Some(port_type)) = (
            nonempty_str(field(entry, "name")),
            field(entry, "type")
                .as_str()
                .filter(|kind| PORT_TYPES.contains(kind)),
        ) else {
            continue;
        };
        match ports.iter_mut().find(|(known, _)| *known == name) {
            Some(port) => port.1 = port_type,
            None => ports.push((name, port_type)),
        }
    }
    ports
}

/// Source node ids referenced by the node's inputs.
fn input_sources(node: &Map<String, Value>) -> Vec<&str> {
    port_list(node, "inputs")
        .iter()
        .filter_map(|input| input.as_object())
        .filter_map(|input| field(input, "from").as_str())
        .filter_map(|source| source.split_once('.'))
        .map(|(source, _)| source)
        .collect()
}

fn lifecycle_of(state: &Map<String, Value>) -> &Value {
    static TASK: std::sync::LazyLock<Value> =
        std::sync::LazyLock::new(|| Value::String(NODE_LIFECYCLE_DEFAULT.to_string()));
    state.get("lifecycle").unwrap_or(&TASK)
}

fn is_machine_form(spec: &Map<String, Value>) -> bool {
    spec.contains_key("states") || spec.contains_key("transitions")
}

/// The run-block checks both forms share. Returns the run budget when valid.
fn validate_run_fields(run: Option<&Map<String, Value>>, errors: &mut Vec<String>) -> Option<i128> {
    let run = run?;
    // Typed fields are validated by presence: an explicit JSON null is
    // rejected with the field's own message, never read as an omitted
    // default.
    let budget = integer(field(run, "budget_ms")).filter(|budget| *budget > 0);
    if run.contains_key("budget_ms") && budget.is_none() {
        errors.push("run budget_ms must be a positive integer".to_string());
    }
    if let Some(policy) = run.get("failure_policy") {
        if !policy
            .as_str()
            .is_some_and(|policy| FAILURE_POLICIES.contains(&policy))
        {
            errors.push(format!(
                "run failure_policy must be one of {}, got {}",
                repr_words(&FAILURE_POLICIES),
                repr(policy)
            ));
        }
    }
    if let Some(max_parallel) = run.get("max_parallel") {
        if !integer(max_parallel)
            .is_some_and(|value| (MAX_PARALLEL_MIN..=MAX_PARALLEL_MAX).contains(&value))
        {
            errors.push(format!(
                "run max_parallel must be an integer between {MAX_PARALLEL_MIN} and {MAX_PARALLEL_MAX}"
            ));
        }
    }
    if let Some(max_transitions) = run.get("max_transitions") {
        if !integer(max_transitions)
            .is_some_and(|value| value > 0 && value <= i128::from(MAX_TRANSITIONS_CAP))
        {
            errors.push(format!(
                "run max_transitions must be a positive integer no greater than {MAX_TRANSITIONS_CAP}"
            ));
        }
    }
    if let Some(max_children) = run.get("max_children") {
        if !integer(max_children).is_some_and(|value| value > 0 && value <= MAX_CHILDREN_CAP) {
            errors.push(format!(
                "run max_children must be a positive integer no greater than {MAX_CHILDREN_CAP}"
            ));
        }
    }
    budget
}

/// Field rules shared by dag nodes (`noun` "node") and machine states
/// (`noun` "state"): subagent forms, lifecycle, budgets, retries, failure
/// policies, port lists, foreach, and the resident exclusions.
#[allow(clippy::too_many_lines)] // one rule list, in the order its sentences are reported
fn validate_state_fields(
    state: &Map<String, Value>,
    id: &str,
    run_budget: Option<i128>,
    states_by_id: &HashMap<&str, &Map<String, Value>>,
    noun: &str,
    errors: &mut Vec<String>,
) {
    let lifecycle = lifecycle_of(state);
    if state.contains_key("lifecycle")
        && !lifecycle
            .as_str()
            .is_some_and(|kind| LIFECYCLES.contains(&kind))
    {
        errors.push(format!(
            "{noun} {id} lifecycle must be 'task' or 'resident', got {}",
            repr(lifecycle)
        ));
    }
    let is_resident = lifecycle.as_str() == Some("resident");

    if !field(state, "wait").is_null() {
        // Gated: the watch host handlers (rlm.watch.*) do not exist yet; a
        // wait block would silently do nothing.
        errors.push(format!(
            "{noun} {id}: wait states require the watch host handlers (rlm.watch.*); \
             they arrive with the communication series - remove the wait block until then"
        ));
    }

    let subagent = field(state, "subagent");
    if let Some(python) = state.get("python") {
        if state.contains_key("subagent") {
            errors.push(format!(
                "{noun} {id} declares both subagent and python; use exactly one"
            ));
        }
        if !python.as_object().is_some_and(|python| {
            nonempty_str(field(python, "import")).is_some()
                && nonempty_str(field(python, "callable")).is_some()
        }) {
            errors.push(format!(
                "{noun} {id} python requires non-empty 'import' and 'callable' strings"
            ));
        }
        if is_resident {
            errors.push(format!("python {noun} {id} cannot be resident"));
        }
        if state.contains_key("foreach") {
            errors.push(format!("python {noun} {id} cannot use foreach"));
        }
    } else if nonempty_str(subagent).is_some() {
        // A harness subagent entry id or title; resolved at run time.
    } else if let Some(subagent) = subagent.as_object() {
        // Run-time resolution strips these fields, so whitespace-only
        // values are rejected here too: a storable factory is spawnable.
        if field(subagent, "prompt")
            .as_str()
            .is_none_or(|prompt| prompt.trim().is_empty())
        {
            errors.push(format!(
                "{noun} {id} inline subagent requires a non-empty prompt"
            ));
        }
        for key in ["name", "model", "thinking"] {
            let value = field(subagent, key);
            if !value.is_null() && value.as_str().is_none_or(|text| text.trim().is_empty()) {
                errors.push(format!(
                    "{noun} {id} inline subagent {key} must be a non-empty string when provided"
                ));
            }
        }
        if let Some(length) = field(subagent, "name")
            .as_str()
            .map(|name| name.trim().chars().count())
            .filter(|length| *length > SUBAGENT_NAME_MAX_LENGTH)
        {
            errors.push(format!(
                "{noun} {id} inline subagent name must be at most \
                 {SUBAGENT_NAME_MAX_LENGTH} characters, got {length}"
            ));
        }
    } else {
        errors.push(format!(
            "{noun} {id} requires a subagent: a harness subagent id/title string \
             or an inline object with a prompt (or a python state: \
             {{\"python\": {{\"import\": \"<module>\", \"callable\": \"<function>\"}}}})"
        ));
    }

    if let Some(budget) = state.get("budget_ms") {
        match integer(budget).filter(|budget| *budget > 0) {
            None => errors.push(format!("{noun} {id} budget_ms must be a positive integer")),
            Some(budget) => {
                if let Some(run_budget) = run_budget.filter(|run_budget| budget > *run_budget) {
                    errors.push(format!(
                        "{noun} {id} budget_ms {budget} exceeds the run budget_ms {run_budget}"
                    ));
                }
            }
        }
    }

    let retries = integer(field(state, "retries"));
    if state.contains_key("retries")
        && !retries.is_some_and(|retries| (0..=MAX_RETRIES).contains(&retries))
    {
        errors.push(format!(
            "{noun} {id} retries must be an integer between 0 and {MAX_RETRIES}"
        ));
    } else if is_resident && retries.unwrap_or(0) > 0 {
        errors.push(format!("resident {noun} {id} cannot declare retries"));
    }

    if let Some(policy) = state.get("failure_policy") {
        if !policy
            .as_str()
            .is_some_and(|policy| FAILURE_POLICIES.contains(&policy))
        {
            errors.push(format!(
                "{noun} {id} failure_policy must be one of {}, got {}",
                repr_words(&FAILURE_POLICIES),
                repr(policy)
            ));
        }
    }

    let outputs = field(state, "outputs");
    if !outputs.is_null() && !outputs.is_array() {
        errors.push(format!("{noun} {id} outputs must be a list"));
    } else if is_resident
        && outputs
            .as_array()
            .is_some_and(|outputs| !outputs.is_empty())
    {
        errors.push(format!("resident {noun} {id} cannot declare outputs"));
    }
    // Duplicates are tracked with seen sets, one pass over the list: a
    // state with tens of thousands of ports stays cheap to validate.
    let mut seen_outputs: HashSet<&str> = HashSet::new();
    let mut reported_outputs: HashSet<&str> = HashSet::new();
    for (index, output) in port_list(state, "outputs").iter().enumerate() {
        let Some(output) = output.as_object() else {
            errors.push(format!("{noun} {id} outputs[{index}] must be an object"));
            continue;
        };
        let (name, port_type) = (field(output, "name"), field(output, "type"));
        match nonempty_str(name) {
            None => errors.push(format!(
                "{noun} {id} outputs[{index}] requires a non-empty name"
            )),
            Some(name) => {
                if !seen_outputs.insert(name) && reported_outputs.insert(name) {
                    errors.push(format!(
                        "{noun} {id} declares duplicate output name {}",
                        repr_str(name)
                    ));
                }
            }
        }
        if !port_type
            .as_str()
            .is_some_and(|kind| PORT_TYPES.contains(&kind))
        {
            errors.push(format!(
                "{noun} {id} output {} type must be 'text' or 'json'",
                repr(name)
            ));
        }
    }

    let inputs = field(state, "inputs");
    if !inputs.is_null() && !inputs.is_array() {
        errors.push(format!("{noun} {id} inputs must be a list"));
    }
    let mut seen_inputs: HashSet<&str> = HashSet::new();
    let mut reported_inputs: HashSet<&str> = HashSet::new();
    // One output-type map per source state, so many inputs from one source
    // stay linear in the source's output count.
    let mut output_types_by_source: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    for (index, input) in port_list(state, "inputs").iter().enumerate() {
        let Some(input) = input.as_object() else {
            errors.push(format!("{noun} {id} inputs[{index}] must be an object"));
            continue;
        };
        let (name, port_type, source) = (
            field(input, "name"),
            field(input, "type"),
            field(input, "from"),
        );
        let optional = field(input, "optional");
        if !optional.is_null() && !optional.is_boolean() {
            errors.push(format!(
                "{noun} {id} input {} optional must be a boolean when provided",
                repr(name)
            ));
        }
        match nonempty_str(name) {
            None => errors.push(format!(
                "{noun} {id} inputs[{index}] requires a non-empty name"
            )),
            Some(name) => {
                if !seen_inputs.insert(name) && reported_inputs.insert(name) {
                    errors.push(format!(
                        "{noun} {id} declares duplicate input name {}",
                        repr_str(name)
                    ));
                }
            }
        }
        let port_type = port_type.as_str().filter(|kind| PORT_TYPES.contains(kind));
        if port_type.is_none() {
            errors.push(format!(
                "{noun} {id} input {} type must be 'text' or 'json'",
                repr(name)
            ));
        }
        let Some((source_id, source_output)) =
            source.as_str().and_then(|source| source.split_once('.'))
        else {
            errors.push(format!(
                "{noun} {id} input {} requires a 'from' reference of the form '<node_id>.<output_name>'",
                repr(name)
            ));
            continue;
        };
        let Some(source_state) = states_by_id.get(source_id) else {
            errors.push(format!(
                "{noun} {id} input {} references unknown {noun} {}",
                repr(name),
                repr_str(source_id)
            ));
            continue;
        };
        if lifecycle_of(source_state).as_str() == Some("resident") {
            errors.push(format!(
                "{noun} {id} input {} cannot read from resident {noun} {}",
                repr(name),
                repr_str(source_id)
            ));
            continue;
        }
        let source_types = output_types_by_source.entry(source_id).or_insert_with(|| {
            declared_port_types(source_state, "outputs")
                .into_iter()
                .collect()
        });
        match (source_types.get(source_output), port_type) {
            (None, _) => errors.push(format!(
                "{noun} {id} input {} references output {} that {noun} {} does not declare",
                repr(name),
                repr_str(source_output),
                repr_str(source_id)
            )),
            (Some(source_type), Some(port_type)) if *source_type != port_type => {
                errors.push(format!(
                    "{noun} {id} input {} of type {} cannot read from output {} of type {}",
                    repr(name),
                    repr_str(port_type),
                    repr_str(source_output),
                    repr_str(source_type)
                ));
            }
            (Some(_), Some(_) | None) => {}
        }
    }

    let foreach = field(state, "foreach");
    if !foreach.is_null() {
        if is_resident {
            errors.push(format!("resident {noun} {id} cannot use foreach"));
        }
        match foreach.as_object() {
            None => errors.push(format!("{noun} {id} foreach must be an object")),
            Some(foreach) => {
                let over = field(foreach, "over");
                match nonempty_str(over) {
                    None => errors.push(format!(
                        "{noun} {id} foreach.over must be a non-empty input name"
                    )),
                    Some(over) => {
                        let declared = declared_port_types(state, "inputs");
                        match declared.iter().find(|(name, _)| *name == over) {
                            None => errors.push(format!(
                                "{noun} {id} foreach.over must name one of this {noun}'s inputs, got {}",
                                repr_str(over)
                            )),
                            Some((_, port_type)) if *port_type != "json" => errors.push(format!(
                                "{noun} {id} foreach.over input {} must have type 'json'",
                                repr_str(over)
                            )),
                            Some(_) => {}
                        }
                    }
                }
                if !integer(field(foreach, "max"))
                    .is_some_and(|max| (FOREACH_MAX_MIN..=FOREACH_MAX_MAX).contains(&max))
                {
                    errors.push(format!(
                        "{noun} {id} foreach.max must be an integer between {FOREACH_MAX_MIN} and {FOREACH_MAX_MAX}"
                    ));
                }
            }
        }
    }
}

/// Whether no container in a guard value nests deeper than the bound. A
/// JSON value is always finite, string-keyed and acyclic, so depth is the
/// one part of the finite-JSON-data rule that can still fail.
fn within_guard_depth(value: &Value, depth: usize) -> bool {
    if depth > MAX_GUARD_VALUE_DEPTH {
        return false;
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => true,
        Value::Array(items) => items.iter().all(|item| within_guard_depth(item, depth + 1)),
        Value::Object(fields) => fields
            .values()
            .all(|item| within_guard_depth(item, depth + 1)),
    }
}

fn validate_guard(
    when: &Value,
    index: usize,
    source_state: &Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(when) = when.as_object() else {
        errors.push(format!("transitions[{index}] when must be an object"));
        return;
    };
    let output = field(when, "output");
    let source_types = declared_port_types(source_state, "outputs");
    match nonempty_str(output) {
        None => errors.push(format!(
            "transitions[{index}] when requires a non-empty output"
        )),
        Some(output) => match source_types.iter().find(|(name, _)| *name == output) {
            None => errors.push(format!(
                "transitions[{index}] when.output {} is not a declared output of state {}",
                repr_str(output),
                repr(field(source_state, "id"))
            )),
            Some((_, port_type)) => {
                let path = field(when, "path");
                if !path.is_null() {
                    if nonempty_str(path).is_none() {
                        errors.push(format!(
                            "transitions[{index}] when.path must be a non-empty dotted path"
                        ));
                    } else if *port_type != "json" {
                        errors.push(format!(
                            "transitions[{index}] when.path requires a json output, got text output {}",
                            repr_str(output)
                        ));
                    }
                }
            }
        },
    }
    let op = field(when, "op");
    let Some(op) = op.as_str().filter(|op| GUARD_OPS.contains(op)) else {
        errors.push(format!(
            "transitions[{index}] when.op must be one of {}, got {}",
            repr_words(&GUARD_OPS),
            repr(op)
        ));
        return;
    };
    let value = field(when, "value");
    match op {
        // Existence carries no value.
        "exists" => return,
        "gt" | "gte" | "lt" | "lte" => {
            if !value.is_number() {
                errors.push(format!(
                    "transitions[{index}] when.op {} requires a numeric value",
                    repr_str(op)
                ));
            }
        }
        "contains" => {
            if value.as_array().is_none_or(Vec::is_empty) {
                errors.push(format!(
                    "transitions[{index}] when.op 'contains' requires a non-empty list value"
                ));
            }
        }
        // "eq" | "ne": the only remaining guard ops.
        _ => {
            if value.is_array() || value.is_object() {
                errors.push(format!(
                    "transitions[{index}] when.op {} requires a scalar value",
                    repr_str(op)
                ));
            }
        }
    }
    if !within_guard_depth(value, 0) {
        errors.push(format!(
            "transitions[{index}] when.value must be finite JSON data \
             (JSON carries no NaN or Infinity, and only JSON shapes \
             serialize: lists, objects, strings, numbers, booleans, null, \
             and no container nests deeper than {MAX_GUARD_VALUE_DEPTH} levels)"
        ));
    }
}

/// Dry-run validation of a machine-form spec: the sentences describing
/// every problem, empty when the machine is valid.
///
/// States are 1..1024 with unique slug ids and at least one entry state;
/// every state has a subagent or a python callable; entry states declare
/// only optional inputs; residents declare no outputs, foreach, retries or
/// outgoing transitions; wait blocks are rejected; transitions reference
/// existing states and may carry one guard over the from-state's latest
/// settle; a transition from a list of states is a join. Cycles are legal.
#[allow(clippy::too_many_lines)] // one rule list, in the order its sentences are reported
pub(super) fn validate_factory_machine(machine: &Value) -> Vec<String> {
    let Some(machine) = machine.as_object() else {
        return vec!["factory machine must be a JSON object".to_string()];
    };
    let mut errors: Vec<String> = Vec::new();
    let run = field(machine, "run");
    if !run.is_null() && !run.is_object() {
        errors.push("run must be an object".to_string());
    }
    let run_budget = validate_run_fields(run.as_object(), &mut errors);

    let Some(states) = field(machine, "states").as_array() else {
        errors.push("factory machine requires a states list".to_string());
        return errors;
    };
    if !(1..=MAX_STATES).contains(&states.len()) {
        errors.push(format!(
            "factory machine must declare between 1 and {MAX_STATES} states, got {}",
            states.len()
        ));
        return errors;
    }

    let mut ordered: Vec<(&str, &Map<String, Value>)> = Vec::new();
    let mut states_by_id: HashMap<&str, &Map<String, Value>> = HashMap::new();
    for (index, state) in states.iter().enumerate() {
        let Some(state) = state.as_object() else {
            errors.push(format!("states[{index}] must be an object"));
            continue;
        };
        let state_id = field(state, "id");
        match nonempty_str(state_id) {
            None => errors.push(format!("states[{index}] requires a non-empty id")),
            Some(id) if !valid_node_id(id) => errors.push(format!(
                "states[{index}] id must match ^[a-z0-9][a-z0-9-]{{0,63}}$, got {}",
                repr_str(id)
            )),
            Some(id) if states_by_id.contains_key(id) => errors.push(format!(
                "states[{index}] duplicates state id {}",
                repr_str(id)
            )),
            Some(id) => {
                states_by_id.insert(id, state);
                ordered.push((id, state));
            }
        }
    }

    // Configured inline subagent names label the spawned children, and the
    // supervisor requires unique sibling names: a duplicate (or a name
    // another state's suffixed labels produce) is rejected here instead of
    // failing at spawn time.
    let mut seen_names: Vec<(String, &str)> = Vec::new();
    for (state_id, state) in &ordered {
        validate_state_fields(
            state,
            state_id,
            run_budget,
            &states_by_id,
            "state",
            &mut errors,
        );
        if let Some(configured) = field(state, "subagent")
            .as_object()
            .and_then(|subagent| nonempty_str(field(subagent, "name")))
            .map(str::trim)
        {
            let base_seen = seen_names
                .iter()
                .find(|(seen, _)| suffixed_spawn_form(seen, configured));
            let base_current = seen_names
                .iter()
                .find(|(seen, _)| suffixed_spawn_form(configured, seen));
            if let Some((_, owner)) = seen_names.iter().find(|(seen, _)| seen == configured) {
                errors.push(format!(
                    "state {state_id} subagent name {} is already configured by state {}",
                    repr_str(configured),
                    repr_str(owner)
                ));
            } else if let Some((base, owner)) = base_seen {
                let base = repr_str(base);
                errors.push(format!(
                    "state {state_id} subagent name {} collides with the \
                     suffixed spawn labels of state {} \
                     (configured {base}): re-entry, foreach, and retries name children \
                     {base}-i<n> and {base}-a<n>",
                    repr_str(configured),
                    repr_str(owner)
                ));
            } else if let Some((base, owner)) = base_current {
                let name = repr_str(configured);
                errors.push(format!(
                    "state {state_id} subagent name {name} suffixed by re-entry, \
                     foreach, and retries ({name}-i<n>, {name}-a<n>) \
                     collides with state {} \
                     (configured {})",
                    repr_str(owner),
                    repr_str(base)
                ));
            } else {
                seen_names.push((configured.to_string(), state_id));
            }
        }
        if state.get("entry").is_some_and(|entry| !entry.is_boolean()) {
            errors.push(format!("state {state_id} entry must be a boolean"));
        }
        if state
            .get("max_entries")
            .is_some_and(|max| integer(max).is_none_or(|max| max < 1))
        {
            errors.push(format!(
                "state {state_id} max_entries must be an integer >= 1"
            ));
        }
        // A required input can never bind on an entry state's first entry
        // nor on a self-input's first entry; optional inputs are the loop
        // form, so only the required variants are rejected.
        for input in port_list(state, "inputs") {
            let Some(input) = input
                .as_object()
                .filter(|input| !truthy(field(input, "optional")))
            else {
                continue;
            };
            if field(state, "entry") == &Value::Bool(true) {
                errors.push(format!(
                    "entry state {state_id} input {} must be optional: \
                     an entry state's first entry has no upstream settle to bind",
                    repr(field(input, "name"))
                ));
                continue;
            }
            if field(input, "from")
                .as_str()
                .and_then(|source| source.split_once('.'))
                .is_some_and(|(source, _)| source == *state_id)
            {
                errors.push(format!(
                    "state {state_id} input {} cannot require itself: \
                     mark the self-input optional - a required one can never bind on the state's first entry",
                    repr(field(input, "name"))
                ));
            }
        }
    }

    // A machine whose only state failed its id check reports that problem
    // alone, and a flag that is not a boolean never declares an entry.
    if !ordered.is_empty()
        && !ordered
            .iter()
            .any(|(_, state)| field(state, "entry") == &Value::Bool(true))
    {
        errors.push("factory machine requires at least one entry state".to_string());
    }

    let transitions = field(machine, "transitions");
    let Some(transitions) = transitions
        .as_array()
        .map(Vec::as_slice)
        .or_else(|| transitions.is_null().then_some(&[][..]))
    else {
        errors.push("factory machine transitions must be a list".to_string());
        return errors;
    };
    for (index, transition) in transitions.iter().enumerate() {
        let Some(transition) = transition.as_object() else {
            errors.push(format!("transitions[{index}] must be an object"));
            continue;
        };
        let raw_source = field(transition, "from");
        let target = field(transition, "to");
        // `from` is one state id, or a list of them: a join that fires once
        // every source settled and carries no guard.
        let mut sources: Vec<&str> = Vec::new();
        if let Some(listed) = raw_source.as_array() {
            let names: Option<Vec<&str>> = listed.iter().map(nonempty_str).collect();
            sources = listed.iter().filter_map(nonempty_str).collect();
            match names {
                _ if listed.is_empty() => errors.push(format!(
                    "transitions[{index}] from must name at least one state"
                )),
                None => errors.push(format!(
                    "transitions[{index}] from entries must be non-empty state id strings"
                )),
                Some(names) => {
                    let distinct: HashSet<&str> = names.iter().copied().collect();
                    if distinct.len() != names.len() {
                        errors.push(format!("transitions[{index}] from must not repeat a state"));
                    } else if let Some(missing) =
                        names.iter().find(|name| !states_by_id.contains_key(*name))
                    {
                        errors.push(format!(
                            "transitions[{index}] references unknown from-state {}",
                            repr_str(missing)
                        ));
                    }
                }
            }
            if !field(transition, "when").is_null() {
                errors.push(format!(
                    "transitions[{index}] with multiple from-states cannot carry a when guard; \
                     use single-state transitions for guards"
                ));
            }
        } else if let Some(source) = nonempty_str(raw_source) {
            sources.push(source);
            if !states_by_id.contains_key(source) {
                errors.push(format!(
                    "transitions[{index}] references unknown from-state {}",
                    repr_str(source)
                ));
            }
        } else {
            errors.push(format!("transitions[{index}] requires a non-empty from"));
        }
        match nonempty_str(target) {
            None => errors.push(format!("transitions[{index}] requires a non-empty to")),
            Some(target) if !states_by_id.contains_key(target) => errors.push(format!(
                "transitions[{index}] references unknown to-state {}",
                repr_str(target)
            )),
            Some(_) => {}
        }
        if let Some(on) = transition.get("on") {
            if !on
                .as_str()
                .is_some_and(|on| TRANSITION_ON_KINDS.contains(&on))
            {
                errors.push(format!(
                    "transitions[{index}] on must be one of {}, got {}",
                    repr_words(&TRANSITION_ON_KINDS),
                    repr(on)
                ));
            }
        }
        // A join's listed length decides the guard rule, malformed entries
        // included: they reported their own error above.
        let single = raw_source.as_array().is_none_or(|listed| listed.len() == 1);
        for source in &sources {
            let Some(source_state) = states_by_id.get(source) else {
                continue;
            };
            if lifecycle_of(source_state).as_str() == Some("resident") {
                errors.push(format!(
                    "transitions[{index}] cannot leave resident state {}",
                    repr_str(source)
                ));
            }
            let when = field(transition, "when");
            if single && !when.is_null() {
                validate_guard(when, index, source_state, &mut errors);
            }
        }
    }
    errors
}

/// Effective dependency edges of a dag node: `depends_on` plus every
/// `inputs[].from` source, deduplicated in first-seen order.
fn effective_dag_edges(node: &Map<String, Value>) -> Vec<&str> {
    let mut edges: Vec<&str> = Vec::new();
    let depends_on = port_list(node, "depends_on")
        .iter()
        .filter_map(nonempty_str);
    for edge in depends_on.chain(input_sources(node)) {
        if !edges.contains(&edge) {
            edges.push(edge);
        }
    }
    edges
}

/// Compile a dag-form spec into machine form: the machine (defaults are
/// applied later by canonicalization), or the dag-level errors.
///
/// Each node becomes a state with `entry` set when it has no effective
/// dependencies and `max_entries` 1; the node's full dependency set becomes
/// one guard-less join transition, so a fan-in node waits for every parent.
pub(super) fn compile_factory_dag(dag: &Value) -> Result<Value, Vec<String>> {
    let Some(dag) = dag.as_object() else {
        return Err(vec!["factory dag must be a JSON object".to_string()]);
    };
    let mut errors: Vec<String> = Vec::new();
    let run = field(dag, "run");
    if !run.is_null() && !run.is_object() {
        errors.push("run must be an object".to_string());
    }
    let run_budget = validate_run_fields(run.as_object(), &mut errors);

    let Some(nodes) = field(dag, "nodes").as_array() else {
        errors.push("factory dag requires a nodes list".to_string());
        return Err(errors);
    };
    if !(1..=MAX_STATES).contains(&nodes.len()) {
        errors.push(format!(
            "factory dag must declare between 1 and {MAX_STATES} nodes, got {}",
            nodes.len()
        ));
        return Err(errors);
    }

    let mut ordered: Vec<(&str, &Map<String, Value>)> = Vec::new();
    let mut nodes_by_id: HashMap<&str, &Map<String, Value>> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        let Some(node) = node.as_object() else {
            errors.push(format!("nodes[{index}] must be an object"));
            continue;
        };
        match nonempty_str(field(node, "id")) {
            None => errors.push(format!("nodes[{index}] requires a non-empty id")),
            Some(id) if !valid_node_id(id) => errors.push(format!(
                "nodes[{index}] id must match ^[a-z0-9][a-z0-9-]{{0,63}}$, got {}",
                repr_str(id)
            )),
            Some(id) if nodes_by_id.contains_key(id) => errors.push(format!(
                "nodes[{index}] duplicates node id {}",
                repr_str(id)
            )),
            Some(id) => {
                nodes_by_id.insert(id, node);
                ordered.push((id, node));
            }
        }
    }

    for (node_id, node) in &ordered {
        validate_state_fields(node, node_id, run_budget, &nodes_by_id, "node", &mut errors);
        // The self-dependency rule covers the effective edge set: a node
        // reading its own output would compile to an unreachable state.
        if input_sources(node).contains(node_id) {
            errors.push(format!("node {node_id} cannot depend on itself"));
        }
        let depends_on = field(node, "depends_on");
        if depends_on.is_null() {
            continue;
        }
        let Some(depends_on) = depends_on.as_array() else {
            errors.push(format!(
                "node {node_id} depends_on must be a list of node ids"
            ));
            continue;
        };
        for dependency in depends_on {
            match nonempty_str(dependency) {
                None => errors.push(format!(
                    "node {node_id} depends_on entries must be non-empty node id strings"
                )),
                Some(dependency) if dependency == *node_id => {
                    errors.push(format!("node {node_id} cannot depend on itself"));
                }
                Some(dependency) => match nodes_by_id.get(dependency) {
                    None => errors.push(format!(
                        "node {node_id} depends on unknown node {}",
                        repr_str(dependency)
                    )),
                    Some(source) if lifecycle_of(source).as_str() == Some("resident") => {
                        errors.push(format!(
                            "node {node_id} cannot depend on resident node {}",
                            repr_str(dependency)
                        ));
                    }
                    Some(_) => {}
                },
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut states: Vec<Value> = Vec::new();
    let mut transitions: Vec<Value> = Vec::new();
    for (node_id, node) in &ordered {
        let edges = effective_dag_edges(node);
        let mut state = Map::new();
        state.insert("id".to_string(), json!(node_id));
        state.insert("entry".to_string(), json!(edges.is_empty()));
        state.insert("max_entries".to_string(), json!(1));
        for key in [
            "subagent",
            "python",
            "lifecycle",
            "budget_ms",
            "retries",
            "failure_policy",
            "inputs",
            "outputs",
            "foreach",
        ] {
            if let Some(value) = node.get(key) {
                state.insert(key.to_string(), value.clone());
            }
        }
        states.push(Value::Object(state));
        // One join transition per node with dependencies, not one per
        // edge; a single dependency stays a plain `from` string.
        match edges.as_slice() {
            [] => {}
            [only] => transitions.push(json!({"from": only, "to": node_id})),
            several => transitions.push(json!({"from": several, "to": node_id})),
        }
    }
    let mut machine = Map::new();
    machine.insert("states".to_string(), Value::Array(states));
    machine.insert("transitions".to_string(), Value::Array(transitions));
    if let Some(run) = run.as_object() {
        machine.insert("run".to_string(), Value::Object(run.clone()));
    }
    Ok(Value::Object(machine))
}

/// Dry-run validation of a spec in either form. A spec carrying `states`
/// or `transitions` is machine form; anything else is dag form and compiles
/// to machine form first. A spec carrying both forms is rejected outright.
pub(in crate::kernel::scheme) fn validate_factory_spec(spec: &Value) -> Vec<String> {
    let Some(object) = spec.as_object() else {
        return vec!["factory dag must be a JSON object".to_string()];
    };
    if is_machine_form(object) {
        if object.contains_key("nodes") {
            return vec!["pass either dag or machine form, not both".to_string()];
        }
        return validate_factory_machine(spec);
    }
    match compile_factory_dag(spec) {
        Err(errors) => errors,
        // Defense in depth: a compiled dag must be a valid machine.
        Ok(machine) => validate_factory_machine(&machine),
    }
}

/// Validate a spec in either form and return the canonical machine form
/// with every default applied: run `failure_policy` `escalate`,
/// `max_parallel` 8, `max_transitions` 10 per state capped at 10000,
/// `max_children` 10000; state `entry` false, `max_entries` 1, `lifecycle`
/// `task`, `retries` 0, `failure_policy` copied from the run; transition
/// `on` `settled`.
///
/// # Errors
///
/// The joined validation sentences when the spec is invalid.
pub(super) fn canonicalize_factory_spec(spec: &Value) -> Result<Value, String> {
    let errors = validate_factory_spec(spec);
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let compiled;
    let machine = if spec.as_object().is_some_and(is_machine_form) {
        spec
    } else {
        compiled = compile_factory_dag(spec).map_err(|errors| errors.join("; "))?;
        &compiled
    };
    let empty = Map::new();
    let machine = machine.as_object().unwrap_or(&empty);
    let run_in = field(machine, "run").as_object().unwrap_or(&empty);
    let run_policy = run_in
        .get("failure_policy")
        .cloned()
        .unwrap_or_else(|| json!(RUN_FAILURE_POLICY_DEFAULT));
    let states = field(machine, "states")
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let mut run = Map::new();
    run.insert("failure_policy".to_string(), run_policy.clone());
    run.insert(
        "max_parallel".to_string(),
        run_in
            .get("max_parallel")
            .cloned()
            .unwrap_or_else(|| json!(RUN_MAX_PARALLEL_DEFAULT)),
    );
    run.insert(
        "max_transitions".to_string(),
        run_in.get("max_transitions").cloned().unwrap_or_else(|| {
            json!((TRANSITIONS_PER_STATE_DEFAULT * states.len() as u64).min(MAX_TRANSITIONS_CAP))
        }),
    );
    run.insert(
        "max_children".to_string(),
        run_in
            .get("max_children")
            .cloned()
            .unwrap_or_else(|| json!(RUN_MAX_CHILDREN_DEFAULT)),
    );
    if let Some(budget) = run_in.get("budget_ms") {
        run.insert("budget_ms".to_string(), budget.clone());
    }
    let mut states_out: Vec<Value> = Vec::new();
    for state in states.iter().filter_map(Value::as_object) {
        let mut out = Map::new();
        out.insert("id".to_string(), field(state, "id").clone());
        out.insert(
            "entry".to_string(),
            json!(field(state, "entry") == &Value::Bool(true)),
        );
        for (key, default) in [
            ("max_entries", json!(1)),
            ("lifecycle", json!(NODE_LIFECYCLE_DEFAULT)),
            ("retries", json!(0)),
            ("failure_policy", run_policy.clone()),
        ] {
            out.insert(key.to_string(), state.get(key).cloned().unwrap_or(default));
        }
        for key in [
            "subagent",
            "python",
            "budget_ms",
            "inputs",
            "outputs",
            "foreach",
        ] {
            if let Some(value) = state.get(key) {
                out.insert(key.to_string(), value.clone());
            }
        }
        states_out.push(Value::Object(out));
    }
    let mut transitions_out: Vec<Value> = Vec::new();
    for transition in field(machine, "transitions")
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(Value::as_object)
    {
        let mut out = Map::new();
        out.insert("from".to_string(), field(transition, "from").clone());
        out.insert("to".to_string(), field(transition, "to").clone());
        out.insert(
            "on".to_string(),
            transition
                .get("on")
                .cloned()
                .unwrap_or_else(|| json!(TRANSITION_ON_KINDS[0])),
        );
        if let Some(when) = transition.get("when") {
            out.insert("when".to_string(), when.clone());
        }
        transitions_out.push(Value::Object(out));
    }
    Ok(json!({"run": run, "states": states_out, "transitions": transitions_out}))
}

/// Node ids of a dag-form node list in a dependency-respecting order;
/// among ready nodes, input order wins.
///
/// # Errors
///
/// A malformed node, a duplicate id, an unknown dependency, or a cycle.
pub(super) fn topological_order(nodes: &Value) -> Result<Vec<String>, String> {
    let Some(nodes) = nodes.as_array() else {
        return Err("nodes must be a list".to_string());
    };
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    let mut ids: Vec<&str> = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let Some(node) = node.as_object() else {
            return Err(format!("nodes[{index}] must be an object"));
        };
        let Some(id) = nonempty_str(field(node, "id")) else {
            return Err(format!("nodes[{index}] requires a non-empty id"));
        };
        if index_of.insert(id, index).is_some() {
            return Err(format!("duplicate node id {}", repr_str(id)));
        }
        ids.push(id);
    }
    let mut dependencies: Vec<Vec<&str>> = Vec::new();
    for (node, id) in nodes.iter().filter_map(Value::as_object).zip(&ids) {
        let id = repr_str(id);
        let mut edges: Vec<&str> = Vec::new();
        let depends_on = field(node, "depends_on");
        if !depends_on.is_null() {
            let Some(depends_on) = depends_on.as_array() else {
                return Err(format!("node {id} depends_on must be a list of node ids"));
            };
            for dependency in depends_on {
                let Some(dependency) = nonempty_str(dependency) else {
                    return Err(format!(
                        "node {id} depends_on entries must be non-empty node id strings"
                    ));
                };
                edges.push(dependency);
            }
        }
        let inputs = field(node, "inputs");
        if !inputs.is_null() {
            let Some(inputs) = inputs.as_array() else {
                return Err(format!("node {id} inputs must be a list"));
            };
            for input in inputs {
                let Some(input) = input.as_object() else {
                    return Err(format!("node {id} inputs entries must be objects"));
                };
                let Some((source, _)) = field(input, "from")
                    .as_str()
                    .and_then(|source| source.split_once('.'))
                else {
                    return Err(format!(
                        "node {id} inputs require a 'from' reference of the form '<node_id>.<output_name>'"
                    ));
                };
                edges.push(source);
            }
        }
        edges.sort_unstable();
        edges.dedup();
        dependencies.push(edges);
    }
    for (edges, id) in dependencies.iter().zip(&ids) {
        if let Some(unknown) = edges.iter().find(|edge| !index_of.contains_key(*edge)) {
            return Err(format!(
                "node {} depends on unknown node {}",
                repr_str(id),
                repr_str(unknown)
            ));
        }
    }
    let mut remaining: Vec<usize> = dependencies.iter().map(Vec::len).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    for (index, edges) in dependencies.iter().enumerate() {
        for edge in edges {
            dependents[index_of[edge]].push(index);
        }
    }
    let mut ready: BinaryHeap<Reverse<usize>> = remaining
        .iter()
        .enumerate()
        .filter(|(_, count)| **count == 0)
        .map(|(index, _)| Reverse(index))
        .collect();
    let mut order: Vec<String> = Vec::new();
    while let Some(Reverse(current)) = ready.pop() {
        order.push(ids[current].to_string());
        for dependent in &dependents[current] {
            remaining[*dependent] -= 1;
            if remaining[*dependent] == 0 {
                ready.push(Reverse(*dependent));
            }
        }
    }
    if order.len() != ids.len() {
        let mut stuck: Vec<&str> = ids
            .iter()
            .zip(&remaining)
            .filter(|(_, count)| **count > 0)
            .map(|(id, _)| *id)
            .collect();
        stuck.sort_unstable();
        return Err(format!(
            "the factory graph contains a cycle involving nodes: {}",
            stuck.join(", ")
        ));
    }
    Ok(order)
}
