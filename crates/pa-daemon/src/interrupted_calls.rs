//! The tool calls a session's previous lifetime left without a result.
//!
//! A lifetime that ends while a call runs (a crash, a stuck cell, the
//! daemon's death) has saved the call but not its result. Nothing runs such
//! a call again. The next lifetime answers it once, in the transcript, before
//! the engine adopts it, so the model's next request carries a result for
//! it. A missing result never proves a call did nothing, nor that it ran
//! (the daemon's transcript keeps the call and its result, no row when the
//! call starts), so its result says the outcome is unknown and lists what is
//! known: what the kernel functions its code names can have done outside the
//! kernel, and the programs stopped with the session.
//!
//! A call the session has moved on from (a later prompt or notice the model
//! was shown) is left alone: the model's requests have answered it since.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::json;

use crate::session_store::SessionFile;

/// What a call's code can have done outside the kernel, by the kernel
/// functions it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Effect {
    /// `bash`: commands and the programs they start.
    Program,
    /// `fs-write`, `fs-edit`, `edit`.
    File,
    /// `rlm-spawn`, `rlm-create-session`.
    Subagent,
    /// `mcp-call-tool`, `web-read`, `web-search`.
    Network,
}

/// One call the previous lifetime left without a result.
struct Interrupted {
    id: String,
    name: String,
    effects: BTreeSet<Effect>,
}

/// Answer, in `store`, each tool call of the branch's last assistant turn
/// that has no result. `crash_kind` is how the previous lifetime ended (the
/// create replay's `sessionCrash` kind; `None` for a plain reopen) and
/// `reaped_programs` how many programs its commands left running were
/// stopped after it ended. Returns how many calls were answered, by class:
/// `outcome_unknown` for every call, and each effect class a call's code
/// names (`program`, `file`, `subagent`, `network`).
///
/// # Errors
///
/// Returns an error when a result row cannot be saved; the rows saved
/// before it stay.
pub(crate) fn report(
    store: &mut SessionFile,
    crash_kind: Option<&str>,
    reaped_programs: u64,
) -> anyhow::Result<BTreeMap<&'static str, u64>> {
    let mut counts = BTreeMap::new();
    let interrupted = {
        let branch = store.branch();
        let Some(last) = branch.iter().rposition(|entry| {
            entry.type_ == "message" && entry.fields["message"]["role"] == "assistant"
        }) else {
            return Ok(counts);
        };
        let assistant = &branch[last].fields["message"];
        if assistant["stopReason"] != "toolUse" {
            return Ok(counts);
        }
        let mut answered = HashSet::new();
        for entry in &branch[last + 1..] {
            let fields = &entry.fields;
            match entry.type_.as_str() {
                "message" if fields["message"]["role"] == "toolResult" => {
                    answered.insert(fields["message"]["toolCallId"].as_str().unwrap_or_default());
                }
                // The fault rows are never part of the model's context.
                "custom_message"
                    if fields["customType"] == pa_types::daemon::SESSION_FAULT_CUSTOM_TYPE => {}
                // Anything else the model reads: the session moved on.
                "message" | "custom_message" | "compaction" | "branch_summary" => {
                    return Ok(counts);
                }
                _ => {}
            }
        }
        assistant["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "toolCall")
            .filter_map(|block| {
                let id = block["id"].as_str()?;
                (!answered.contains(id)).then(|| Interrupted {
                    id: id.to_string(),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    effects: block["arguments"]["code"]
                        .as_str()
                        .unwrap_or_default()
                        .split(|c: char| !(c.is_alphanumeric() || "-!?*/<>=+%_".contains(c)))
                        .filter_map(|name| match name {
                            "bash" => Some(Effect::Program),
                            "fs-write" | "fs-edit" | "edit" => Some(Effect::File),
                            "rlm-spawn" | "rlm-create-session" => Some(Effect::Subagent),
                            "mcp-call-tool" | "web-read" | "web-search" => Some(Effect::Network),
                            _ => None,
                        })
                        .collect(),
                })
            })
            .collect::<Vec<_>>()
    };
    let cause = match crash_kind {
        Some("daemon_restart") => "The daemon hosting this session stopped",
        Some(_) => "This session crashed",
        None => "This session stopped",
    };
    for call in interrupted {
        let name = &call.name;
        let mut known = Vec::new();
        for effect in &call.effects {
            let (class, line) = match effect {
                Effect::Program => (
                    "program",
                    match reaped_programs {
                        0 => "it runs `bash`: a command it started may have done part of its work; commands still running were stopped with the session, except in a persistent shell".to_string(),
                        1 => "it runs `bash`: a command it started may have done part of its work; 1 program the session's commands left running was stopped with the session".to_string(),
                        count => format!("it runs `bash`: a command it started may have done part of its work; {count} programs the session's commands left running were stopped with the session"),
                    },
                ),
                Effect::File => (
                    "file",
                    "it writes files (`fs-write`, `fs-edit` or `edit`): each write replaces its file whole, so a file it wrote holds its old text or its new, never part of either; read the files to see which writes landed".to_string(),
                ),
                Effect::Subagent => (
                    "subagent",
                    "it spawns subagents: one it spawned may exist and may still be running; see `(rlm-list-subagents)`".to_string(),
                ),
                Effect::Network => (
                    "network",
                    "it calls an MCP tool or the web: a request it sent may have reached the other side; check there before you send it again".to_string(),
                ),
            };
            *counts.entry(class).or_insert(0) += 1;
            known.push(line);
        }
        if call.effects.is_empty() {
            known.push("its code names no kernel function that acts outside the kernel (`bash`, file writes, subagents, MCP or web)".to_string());
        }
        let text = format!(
            "[outcome-unknown] {cause} while this `{name}` call ran, so its outcome is unknown: it may have run in part, in full or not at all, and its result was lost. It was not run again: check its effects before you run it again.\n\nWhat is known:\n- {}",
            known.join("\n- ")
        );
        store.persist_entry(
            "message",
            json!({ "message": {
                "role": "toolResult",
                "toolCallId": call.id,
                "toolName": name,
                "content": [{ "type": "text", "text": text }],
                "details": { "outcome": "unknown" },
                "isError": true,
                "timestamp": crate::util::now_ms(),
            } }),
        )?;
        *counts.entry("outcome_unknown").or_insert(0) += 1;
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fmt::Write as _;

    /// What `report` did to the transcript `rows`: the rows it saved, as
    /// `[call id, text]`, and the counts it returned.
    fn answered(rows: &[Value], reaped_programs: u64) -> Value {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.jsonl");
        let mut text = format!(
            "{}\n",
            json!({
                "type": "session", "version": 3, "id": "s",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/",
            })
        );
        let mut parent = Value::Null;
        for (index, row) in rows.iter().enumerate() {
            let mut row = row.clone();
            row["id"] = json!(format!("e{index}"));
            row["parentId"] = parent;
            row["timestamp"] = json!("2026-01-01T00:00:01.000Z");
            parent = row["id"].clone();
            writeln!(text, "{row}").expect("format a row");
        }
        std::fs::write(&path, text).expect("write the transcript");
        let mut store = SessionFile::open(&path).expect("open the transcript");
        let counts =
            report(&mut store, Some("daemon_restart"), reaped_programs).expect("answer the calls");
        let reopened = SessionFile::open(&path).expect("reopen the transcript");
        let saved: Vec<Value> = reopened.branch()[rows.len()..]
            .iter()
            .map(|entry| {
                let message = &entry.fields["message"];
                json!([message["toolCallId"], message["content"][0]["text"]])
            })
            .collect();
        json!({ "saved": saved, "counts": counts })
    }

    fn call(id: &str, code: &str) -> Value {
        json!({ "type": "toolCall", "id": id, "name": "eval", "arguments": { "code": code } })
    }

    /// Of a turn's three calls, the one with a result is left alone and the
    /// other two are answered as of unknown outcome, each with what its code
    /// can have done; a session that moved on past such a turn is answered
    /// nothing.
    #[test]
    fn each_call_left_without_a_result_is_answered_once_as_of_unknown_outcome() {
        let turn = [
            json!({ "type": "message", "message": { "role": "user", "content": "go" } }),
            json!({ "type": "message", "message": {
                "role": "assistant",
                "stopReason": "toolUse",
                "content": [
                    call("done", "(+ 1 2)"),
                    call("ran", "(bash \"make\") (fs-write \"a\" \"b\") (web-read \"u\")"),
                    call("plain", "(define x (+ 1 2))"),
                ],
            } }),
            json!({ "type": "message", "message": {
                "role": "toolResult", "toolCallId": "done", "toolName": "eval",
                "content": [{ "type": "text", "text": "3" }], "isError": false, "timestamp": 1,
            } }),
        ];
        let moved_on = [
            turn.to_vec(),
            vec![json!({ "type": "message", "message": { "role": "user", "content": "next" } })],
        ]
        .concat();
        let head = "[outcome-unknown] The daemon hosting this session stopped while this `eval` call ran, so its outcome is unknown: it may have run in part, in full or not at all, and its result was lost. It was not run again: check its effects before you run it again.\n\nWhat is known:\n- ";
        assert_eq!(
            [answered(&turn, 2), answered(&moved_on, 2)],
            [
                json!({
                    "saved": [
                        [
                            "ran",
                            format!(
                                "{head}it runs `bash`: a command it started may have done part of its work; 2 programs the session's commands left running were stopped with the session\n\
                                 - it writes files (`fs-write`, `fs-edit` or `edit`): each write replaces its file whole, so a file it wrote holds its old text or its new, never part of either; read the files to see which writes landed\n\
                                 - it calls an MCP tool or the web: a request it sent may have reached the other side; check there before you send it again"
                            ),
                        ],
                        [
                            "plain",
                            format!("{head}its code names no kernel function that acts outside the kernel (`bash`, file writes, subagents, MCP or web)"),
                        ],
                    ],
                    "counts": { "file": 1, "network": 1, "outcome_unknown": 2, "program": 1 },
                }),
                json!({ "saved": [], "counts": {} }),
            ]
        );
    }
}
