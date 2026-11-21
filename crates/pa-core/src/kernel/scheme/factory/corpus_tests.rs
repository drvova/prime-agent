//! Replays the calls the Python kernel's factory tests made to its pure
//! functions (`prime-agent-runtime/test/test_factory.py`, recorded once
//! with every argument and result) against this port: each row names the
//! Python test it came from, the function, its arguments, and what Python
//! returned or raised.

use serde_json::{json, Value};

use super::{binding, naming, spec};

const CALLS: &str = include_str!("corpus/python_factory_calls.jsonl");

/// A recorded call's argument by position, or by keyword when it was
/// passed that way.
fn argument<'a>(row: &'a Value, position: usize, keyword: &str) -> &'a Value {
    row["args"]
        .get(position)
        .unwrap_or_else(|| &row["kwargs"][keyword])
}

fn text<'a>(row: &'a Value, position: usize, keyword: &str) -> &'a str {
    argument(row, position, keyword).as_str().unwrap()
}

/// What this port answers for one recorded call, in the recorded shape:
/// `{"ok": value}` or `{"error": message}`. `None` for a function a later
/// part of the port replays.
fn replay(row: &Value) -> Option<Value> {
    let first = argument(row, 0, "");
    let outcome = |result: Result<Value, String>| match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": error }),
    };
    Some(match row["fn"].as_str().unwrap() {
        "validate_factory_spec" => json!({"ok": spec::validate_factory_spec(first)}),
        "validate_factory_machine" => json!({"ok": spec::validate_factory_machine(first)}),
        "compile_factory_dag" => match spec::compile_factory_dag(first) {
            Ok(machine) => json!({"ok": [machine, []]}),
            Err(errors) => json!({"ok": [null, errors]}),
        },
        "canonicalize_factory_spec" => outcome(spec::canonicalize_factory_spec(first)),
        "topological_order" => outcome(spec::topological_order(first).map(|order| json!(order))),
        "_child_name" => json!({"ok": naming::child_name(
            text(row, 0, "run_id"),
            text(row, 1, "state_id"),
            argument(row, 2, "instance_index").as_i64().unwrap(),
            argument(row, 3, "attempt").as_u64().unwrap(),
        )}),
        "_spawn_label" => json!({"ok": naming::spawn_label(
            first.as_str(),
            text(row, 1, "run_id"),
            text(row, 2, "state_id"),
            argument(row, 3, "instance_index").as_i64().unwrap(),
            argument(row, 4, "attempt").as_u64().unwrap(),
        )}),
        "_suffixed_spawn_form" => {
            json!({"ok": naming::suffixed_spawn_form(text(row, 0, "base"), text(row, 1, "candidate"))})
        }
        "_guard_passes" => json!({"ok": binding::guard_passes(
            first.as_object().unwrap(),
            argument(row, 1, "outputs").as_object().unwrap(),
        )}),
        "_json_equal" => json!({"ok": binding::json_equal(first, argument(row, 1, "expected"))}),
        "_terminal_json_object" => {
            json!({"ok": binding::terminal_json_object(first.as_str().unwrap())})
        }
        "_parse_json_output" => {
            let truncated_at = row["kwargs"]["truncated_at"]
                .as_u64()
                .map(|at| usize::try_from(at).unwrap());
            match binding::parse_json_output(
                first.as_str().unwrap(),
                text(row, 1, "output_name"),
                truncated_at,
            ) {
                Ok(value) => json!({"ok": [value, null]}),
                Err(error) => json!({"ok": [null, error]}),
            }
        }
        "_render_prompt" => {
            let values: Vec<(String, String)> = argument(row, 1, "values")
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap().to_string()))
                .collect();
            json!({"ok": binding::render_prompt(first.as_str().unwrap(), &values)})
        }
        "parse_machine_file"
        | "render_machine_file"
        | "machine_name_errors"
        | "machine_description_errors"
        | "_wire_payload"
        | "_machine_structure" => return None,
        unknown => panic!("the corpus names a function this replay does not know: {unknown}"),
    })
}

#[test]
fn every_recorded_python_call_gets_the_python_answer() {
    let mut differences: Vec<Value> = Vec::new();
    for line in CALLS.lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let recorded = match row.get("error") {
            Some(error) => json!({ "error": error }),
            None => json!({"ok": row["ok"]}),
        };
        if let Some(answered) = replay(&row).filter(|answered| *answered != recorded) {
            differences.push(json!({
                "test": row["test"],
                "fn": row["fn"],
                "args": row["args"],
                "python": recorded,
                "rust": answered,
            }));
        }
    }
    assert_eq!(differences, Vec::<Value>::new());
}
