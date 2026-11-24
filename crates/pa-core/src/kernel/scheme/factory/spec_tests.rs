//! The Python validation tests whose inputs the recorded corpus could not
//! carry (specs too large to record, and one built too deep to serialize),
//! plus the Scheme surface of the dry run.

use serde_json::{json, Value};

use super::spec::{
    canonicalize_factory_spec, compile_factory_dag, validate_factory_machine, validate_factory_spec,
};
use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::HostRequestHandlers;
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelProvisioner};

fn node(id: &str) -> Value {
    json!({"id": id, "subagent": "worker"})
}

fn nodes(count: usize) -> Vec<Value> {
    (0..count).map(|index| node(&format!("n{index}"))).collect()
}

/// `test_node_cap`: 1024 nodes validate, 1025 report the one cap sentence.
#[test]
fn a_dag_holds_at_most_1024_nodes() {
    assert_eq!(
        (
            validate_factory_spec(&json!({"nodes": nodes(1024)})),
            validate_factory_spec(&json!({"nodes": nodes(1025)})),
        ),
        (
            Vec::<String>::new(),
            vec!["factory dag must declare between 1 and 1024 nodes, got 1025".to_string()],
        )
    );
}

/// `test_state_cap`.
#[test]
fn a_machine_holds_between_1_and_1024_states() {
    let machine = |count: usize| {
        let mut states: Vec<Value> = (0..count).map(|index| node(&format!("s{index}"))).collect();
        if let Some(first) = states.first_mut() {
            first["entry"] = json!(true);
        }
        json!({"states": states, "transitions": []})
    };
    assert_eq!(
        (
            validate_factory_machine(&machine(1024)),
            validate_factory_machine(&machine(1025)),
            validate_factory_machine(&machine(0)),
        ),
        (
            Vec::<String>::new(),
            vec!["factory machine must declare between 1 and 1024 states, got 1025".to_string()],
            vec!["factory machine must declare between 1 and 1024 states, got 0".to_string()],
        )
    );
}

/// `test_max_transitions_defaults_to_ten_per_state_capped`.
#[test]
fn max_transitions_defaults_to_ten_per_state_capped_at_ten_thousand() {
    let default_of = |count: usize| {
        canonicalize_factory_spec(&json!({"nodes": nodes(count)})).unwrap()["run"]
            ["max_transitions"]
            .clone()
    };
    assert_eq!(
        (default_of(10), default_of(1024)),
        (json!(100), json!(10_000))
    );
}

/// `test_wide_fan_in_stays_one_join_per_target`: 300 predecessors, data
/// edges plus `depends_on`, compile to one join listing them all.
#[test]
fn a_wide_fan_in_compiles_to_one_join_transition() {
    let mut all: Vec<Value> = (0..300)
        .map(|index| json!({"id": format!("s{index}"), "subagent": "worker", "outputs": [{"name": "o", "type": "text"}]}))
        .collect();
    let depends_on: Vec<String> = (0..300)
        .step_by(2)
        .map(|index| format!("s{index}"))
        .collect();
    let inputs: Vec<Value> = (1..300)
        .step_by(2)
        .map(|index| json!({"name": format!("i{index}"), "type": "text", "from": format!("s{index}.o")}))
        .collect();
    all.push(json!({"id": "t", "subagent": "worker", "depends_on": depends_on, "inputs": inputs}));
    let machine = compile_factory_dag(&json!({"nodes": all})).unwrap();
    let mut sources: Vec<String> = machine["transitions"][0]["from"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source.as_str().unwrap().to_string())
        .collect();
    sources.sort();
    let mut expected: Vec<String> = (0..300).map(|index| format!("s{index}")).collect();
    expected.sort();
    assert_eq!(
        (
            machine["transitions"].as_array().unwrap().len(),
            machine["transitions"][0]["to"].clone(),
            sources,
            validate_factory_machine(&machine),
        ),
        (1, json!("t"), expected, Vec::<String>::new())
    );
}

/// `NonQuadraticValidationTest`: thousands of ports validate, and every
/// repeat of one name reports exactly one sentence. (The Python tests also
/// bound the wall time; a time bound is not asserted here.)
#[test]
fn duplicate_ports_among_thousands_report_once_each() {
    let mut outputs: Vec<Value> = (0..8000)
        .map(|index| json!({"name": format!("o{index}"), "type": "text"}))
        .collect();
    outputs.push(json!({"name": "o1", "type": "json"}));
    let many_outputs = json!({"nodes": [{"id": "a", "subagent": "worker", "outputs": outputs}]});
    let source_outputs: Vec<Value> = (0..3000)
        .map(|index| json!({"name": format!("o{index}"), "type": "text"}))
        .collect();
    let mut inputs: Vec<Value> = (0..3000)
        .map(|index| json!({"name": format!("i{index}"), "type": "text", "from": format!("src.o{index}")}))
        .collect();
    inputs.push(json!({"name": "i1", "type": "text", "from": "src.o1"}));
    let many_inputs = json!({"nodes": [
        {"id": "src", "subagent": "worker", "outputs": source_outputs},
        {"id": "dst", "subagent": "worker", "inputs": inputs},
    ]});
    let repeated: Vec<Value> = (0..500)
        .map(|_| json!({"name": "dup", "type": "text"}))
        .collect();
    let many_repeats = json!({"nodes": [{"id": "a", "subagent": "worker", "outputs": repeated}]});
    assert_eq!(
        (
            validate_factory_spec(&many_outputs),
            validate_factory_spec(&many_inputs),
            validate_factory_spec(&many_repeats),
        ),
        (
            vec!["node a declares duplicate output name 'o1'".to_string()],
            vec!["node dst declares duplicate input name 'i1'".to_string()],
            vec!["node a declares duplicate output name 'dup'".to_string()],
        )
    );
}

/// `test_guard_value_depth_rejects_without_exhausting_the_stack`.
#[test]
fn a_guard_value_nested_past_the_bound_is_rejected_and_a_shallow_one_is_valid() {
    let machine_with = |value: Value| {
        json!({
            "states": [
                {"id": "a", "subagent": "worker", "entry": true, "outputs": [{"name": "verdict", "type": "json"}]},
                {"id": "b", "subagent": "worker"},
            ],
            "transitions": [
                {"from": "a", "to": "b", "when": {"output": "verdict", "op": "contains", "value": value}}
            ],
        })
    };
    let nested = |levels: usize, innermost: Value| {
        (0..levels).fold(innermost, |inner, _| Value::Array(vec![inner]))
    };
    assert_eq!(
        (
            validate_factory_machine(&machine_with(nested(306, json!([])))),
            validate_factory_machine(&machine_with(nested(10, json!(["verdict"])))),
        ),
        (
            vec!["transitions[0] when.value must be finite JSON data (JSON carries no NaN or Infinity, and only JSON shapes serialize: lists, objects, strings, numbers, booleans, null, and no container nests deeper than 256 levels)".to_string()],
            Vec::<String>::new(),
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scheme_validates_canonicalizes_compiles_and_orders_a_spec() {
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    let cell = r#"
(define dag (hash 'nodes (list (hash 'id "collect" 'subagent "researcher")
                               (hash 'id "review" 'subagent "reviewer" 'depends_on (list "collect")))))
(define refused
  (with-handler (lambda (raised) "refused") (factory-canonicalize (hash 'nodes (list)))))
(display (value->jsexpr-string
  (list (factory-validate dag)
        (factory-validate (hash 'nodes (list (hash 'id "Bad"))))
        (hash-ref (factory-canonicalize dag) 'transitions)
        (hash-ref (hash-ref (factory-canonicalize dag) 'run) 'max_transitions)
        (hash-ref (factory-compile-dag (hash 'nodes 3)) 'errors)
        (factory-topological-order (list (hash 'id "b" 'depends_on (list "a")) (hash 'id "a")))
        refused)))
"#;
    let result = kernel
        .execute(
            cell,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (
            result.error.map(|error| error.evalue),
            serde_json::from_str::<Value>(&result.stdout).ok()
        ),
        (
            None,
            Some(json!([
                [],
                [
                    "nodes[0] id must match ^[a-z0-9][a-z0-9-]{0,63}$, got 'Bad'"
                ],
                [{"from": "collect", "to": "review", "on": "settled"}],
                20,
                ["factory dag requires a nodes list"],
                ["a", "b"],
                "refused",
            ]))
        )
    );
}
