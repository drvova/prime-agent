//! Replays the calls the Python kernel's factory tests made to its pure
//! functions (`prime-agent-runtime/test/test_factory.py`, recorded once
//! with every argument and result) against this port: each row names the
//! Python test it came from, the function, its arguments, and what Python
//! returned or raised.

use serde_json::{json, Value};

use super::{binding, library, naming, report, spec};

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

/// A parsed machine file in the shape the recording gave Python's.
fn machine_json(machine: &library::MachineFile) -> Value {
    json!({"__machine_file__": {
        "name": machine.name,
        "description": machine.description,
        "version": machine.version,
        "author": machine.author,
        "spec": machine.spec,
    }})
}

/// What this port answers for one recorded call, in the recorded shape:
/// `{"ok": value}` or `{"error": message}`.
fn replay(row: &Value) -> Value {
    let first = argument(row, 0, "");
    let outcome = |result: Result<Value, String>| match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": error }),
    };
    match row["fn"].as_str().unwrap() {
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
        "parse_machine_file" => {
            let source = row["kwargs"]["source"].as_str().unwrap_or("machine file");
            match library::parse_machine_file(first.as_str().unwrap(), source) {
                Ok(machine) => json!({"ok": [machine_json(&machine), []]}),
                Err(errors) => json!({"ok": [null, errors]}),
            }
        }
        "render_machine_file" => {
            let fields = &first["__machine_file__"];
            let field = |key: &str| fields[key].as_str().unwrap().to_string();
            json!({"ok": library::render_machine_file(&library::MachineFile {
                name: field("name"),
                description: field("description"),
                version: field("version"),
                author: field("author"),
                spec: fields["spec"].clone(),
            })})
        }
        "machine_name_errors" => json!({"ok": library::machine_name_errors(first)}),
        "machine_description_errors" => {
            json!({"ok": library::machine_description_errors(first)})
        }
        "_wire_payload" => json!({"ok": report::wire_payload(first)}),
        "_machine_structure" => {
            let resolved = row["args"].get(1).cloned().unwrap_or_default();
            json!({"ok": report::machine_structure(first, &|state_id| {
                let setting = |index: usize| resolved[state_id][index].as_str().map(str::to_string);
                (setting(0), setting(1))
            })})
        }
        unknown => panic!("the corpus names a function this replay does not know: {unknown}"),
    }
}

/// A recorded or replayed answer with the one intended difference taken
/// out: where a sentence quotes the JSON parser's own complaint, Python's
/// parser and this one word it differently.
fn without_parser_wording(answer: &Value) -> Value {
    let marker = "block must contain a JSON object (";
    match answer {
        Value::String(text) => match text.find(marker) {
            Some(at) => json!(text[..at + marker.len()]),
            None => answer.clone(),
        },
        Value::Array(items) => items.iter().map(without_parser_wording).collect(),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, item)| (key.clone(), without_parser_wording(item)))
            .collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) => answer.clone(),
    }
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
        let answered = replay(&row);
        if without_parser_wording(&answered) != without_parser_wording(&recorded) {
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
