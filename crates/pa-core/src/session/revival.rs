//! What becomes of a session's work when the lifetime that ran it ended
//! without settling it (a crash, a stuck cell, the daemon's death), shared by
//! the old daemon (its recovery journal) and the runtime daemon (its task
//! lanes):
//!
//! - **How far the turn got** ([`transcript_evidence`]): the transcript says
//!   whether the interrupted turn's prompt was saved and whether its final
//!   answer was. Nothing saved: it is delivered as it was queued. Its prompt
//!   saved but no answer: it continues from the transcript behind one notice
//!   ([`TURN_INTERRUPTED_NOTICE`]) and its prompt is not recorded again. Its
//!   answer saved: it is over.
//! - **A daemon stopped on purpose under the turn** (a `shutdown`, a
//!   restart, an update's handoff): the turn is cut off and the session's
//!   next daemon runs [`UPDATE_RESTART_CONTINUATION_PROMPT`] ahead of the
//!   rest of its queue.
//! - **A daemon that died under the turn** ([`host_death`]): the first time
//!   the turn is run again (or continued); a second death in a row under the
//!   same session's turn settles it as failed instead of taking the daemon
//!   down a third time.
//! - **The calls it left without a result** ([`interrupted_call_results`]):
//!   nothing runs such a call again. The next lifetime answers it once,
//!   before the engine adopts the transcript, with a result that says its
//!   outcome is unknown and lists what is known: what the kernel functions
//!   its code names can have done outside the kernel, and the programs
//!   stopped with the session. A missing result never proves a call did
//!   nothing, nor that it ran. A call the session has moved on from (a later
//!   prompt or notice the model was shown) is left alone.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::BufRead as _;
use std::path::Path;

use serde_json::{json, Value};

/// The prompt a turn cut off by a deliberate daemon stop continues with in
/// the next daemon (TS `UPDATE_RESTART_CONTINUATION_PROMPT`, verbatim, spec
/// §10.5): both daemons queue it at the front of the session's queue.
pub const UPDATE_RESTART_CONTINUATION_PROMPT: &str = "Prime Agent restarted after an update. Continue the interrupted task from the saved transcript and restored tool/kernel state. Inspect current state before retrying commands when needed.";

/// The custom type of the notice a continued turn opens with.
pub const TURN_INTERRUPTED_CUSTOM_TYPE: &str = "turn_interrupted";

/// The notice a continued turn opens with.
pub const TURN_INTERRUPTED_NOTICE: &str = "[turn-interrupted]\n\nThe daemon hosting this session restarted while your previous turn was running. Everything above was saved; a reply that was in flight was lost, and a tool call that was running was not run again: its result says what is known of it (check what it changed before you repeat it). Continue the interrupted turn from where it stopped.";

/// How far the transcript shows an interrupted turn got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevivedTurn {
    /// None of its rows were saved.
    Unstarted,
    /// Its prompt was saved, its final answer was not.
    Interrupted,
    /// Its final answer was saved.
    Answered,
}

/// The row an interrupted turn starts with: what tells its prompt apart in
/// the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPrompt<'a> {
    /// A user message.
    User,
    /// A custom message of this `customType` (an injected prompt).
    Custom(&'a Value),
}

/// How far the turn picked up at `picked_up_at` (an ISO time on the
/// transcript's clock) got in `session_file`, the turn starting with a row
/// of the kind `prompt` names: a turn records its prompt before anything
/// else.
///
/// # Errors
///
/// The session file cannot be read.
pub fn transcript_evidence(
    session_file: &Path,
    picked_up_at: &str,
    prompt: TurnPrompt<'_>,
) -> std::io::Result<RevivedTurn> {
    let mut verdict = RevivedTurn::Unstarted;
    let transcript = crate::session::store::store().open(session_file)?;
    for line in std::io::BufReader::new(transcript).lines() {
        // A torn or foreign line is no evidence either way.
        let Ok(entry) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if entry["timestamp"]
            .as_str()
            .is_none_or(|at| at < picked_up_at)
        {
            continue;
        }
        let role = entry["message"]["role"].as_str();
        let is_prompt = match prompt {
            TurnPrompt::Custom(custom_type) => {
                entry["type"] == "custom_message" && entry["customType"] == *custom_type
            }
            TurnPrompt::User => entry["type"] == "message" && role == Some("user"),
        };
        verdict = match verdict {
            RevivedTurn::Unstarted if is_prompt => RevivedTurn::Interrupted,
            RevivedTurn::Unstarted => RevivedTurn::Unstarted,
            RevivedTurn::Interrupted | RevivedTurn::Answered if entry["type"] == "message" => {
                // An assistant row that asks for tools is mid-turn; any other
                // assistant row is the turn's last word.
                if role == Some("assistant") && entry["message"]["stopReason"] != "toolUse" {
                    RevivedTurn::Answered
                } else {
                    RevivedTurn::Interrupted
                }
            }
            RevivedTurn::Interrupted | RevivedTurn::Answered => verdict,
        };
    }
    Ok(verdict)
}

/// What becomes of the turn that was running when a session went down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedTurn {
    /// Settle it as failed: the rebuilt session comes back idle.
    Settle,
    /// Run it again (or continue it, or leave it over, as the transcript
    /// says how far it got).
    Replay,
}

/// A daemon died under a session's turn: the message its crash record
/// carries (kind `daemon_restart`) and what becomes of the turn, given how
/// far the transcript shows the turn got (`None` when no picked-up turn is
/// on record) and how many daemon deaths in a row caught this session
/// mid-turn before this one. A turn whose answer was saved is over either
/// way; the first time the daemon dies under a turn it runs again (one whose
/// prompt was saved continues behind a notice); when the daemon dies under
/// the same session's turn twice in a row the turn is the likeliest cause,
/// and it is settled as failed instead.
#[must_use]
pub fn host_death(
    revived_turn: Option<RevivedTurn>,
    prior_deaths: u64,
) -> (&'static str, InterruptedTurn) {
    match (revived_turn, prior_deaths) {
        (Some(RevivedTurn::Answered), _) => (
            "the daemon stopped as this session's turn was ending; its answer was already saved, so the session was rebuilt from its transcript and the turn was not started again",
            InterruptedTurn::Replay,
        ),
        (Some(RevivedTurn::Interrupted), 0) => (
            "the daemon stopped while this session's turn was running; the session was rebuilt from its transcript and the turn continues from what was saved",
            InterruptedTurn::Replay,
        ),
        (Some(RevivedTurn::Unstarted) | None, 0) => (
            "the daemon stopped while this session's turn was running; the session was rebuilt from its transcript and the turn was started again",
            InterruptedTurn::Replay,
        ),
        (Some(RevivedTurn::Interrupted | RevivedTurn::Unstarted) | None, _) => (
            "the daemon stopped again while this session's turn was running; the session was rebuilt from its transcript and the turn was not started again",
            InterruptedTurn::Settle,
        ),
    }
}

/// How the lifetime that left calls without a result ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifetimeEnd {
    /// The daemon hosting it stopped (crash kind `daemon_restart`).
    DaemonRestart,
    /// The session crashed (a panic, a stuck cell).
    Crashed,
    /// It stopped without a crash (a plain reopen).
    Stopped,
}

impl LifetimeEnd {
    /// The end a crash of `kind` (`None`: no crash) is.
    #[must_use]
    pub fn of_crash_kind(kind: Option<&str>) -> Self {
        match kind {
            Some("daemon_restart") => LifetimeEnd::DaemonRestart,
            Some(_) => LifetimeEnd::Crashed,
            None => LifetimeEnd::Stopped,
        }
    }
}

/// What a call's code can have done outside the kernel, by the kernel
/// functions it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Effect {
    /// `bash`: commands and the programs they start.
    Program,
    /// `fs-write`, `fs-edit`, `edit`.
    File,
    /// `rlm-spawn` (`rlm-create-session` in older cells).
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

/// The results to save for each tool call of the branch's last assistant
/// turn that has none: one `toolResult` message each, in call order, and how
/// many calls were answered, by class (`outcome_unknown` for every call, and
/// each effect class a call's code names: `program`, `file`, `subagent`,
/// `network`). `branch` is the active branch's rows, oldest first, each as
/// its row type and its fields (a stored row's object). `end` is how the
/// previous lifetime ended and `reaped_programs` how many programs its
/// commands left running were stopped after it ended.
#[must_use]
pub fn interrupted_call_results<'a>(
    branch: impl IntoIterator<Item = (&'a str, &'a Value)>,
    end: LifetimeEnd,
    reaped_programs: u64,
) -> (Vec<Value>, BTreeMap<&'static str, u64>) {
    let mut counts = BTreeMap::new();
    let branch: Vec<(&str, &Value)> = branch.into_iter().collect();
    let Some(last) = branch
        .iter()
        .rposition(|(kind, fields)| *kind == "message" && fields["message"]["role"] == "assistant")
    else {
        return (Vec::new(), counts);
    };
    let assistant = &branch[last].1["message"];
    if assistant["stopReason"] != "toolUse" {
        return (Vec::new(), counts);
    }
    let mut answered = HashSet::new();
    for (kind, fields) in &branch[last + 1..] {
        match *kind {
            "message" if fields["message"]["role"] == "toolResult" => {
                answered.insert(fields["message"]["toolCallId"].as_str().unwrap_or_default());
            }
            // The fault rows are never part of the model's context.
            "custom_message"
                if fields["customType"] == pa_types::daemon::SESSION_FAULT_CUSTOM_TYPE => {}
            // Anything else the model reads: the session moved on.
            "message" | "custom_message" | "compaction" | "branch_summary" => {
                return (Vec::new(), counts);
            }
            _ => {}
        }
    }
    let interrupted: Vec<Interrupted> = assistant["content"]
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
                        "mcp-call" | "mcp-call-tool" | "web-read" | "web-search" => {
                            Some(Effect::Network)
                        }
                        _ => None,
                    })
                    .collect(),
            })
        })
        .collect();
    let cause = match end {
        LifetimeEnd::DaemonRestart => "The daemon hosting this session stopped",
        LifetimeEnd::Crashed => "This session crashed",
        LifetimeEnd::Stopped => "This session stopped",
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    let mut results = Vec::with_capacity(interrupted.len());
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
                    "it writes files (`fs-write` or `fs-edit`): each write replaces its file whole, so a file it wrote holds its old text or its new, never part of either; read the files to see which writes landed".to_string(),
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
        results.push(json!({
            "role": "toolResult",
            "toolCallId": call.id,
            "toolName": name,
            "content": [{ "type": "text", "text": text }],
            "details": { "outcome": "unknown" },
            "isError": true,
            "timestamp": timestamp,
        }));
        *counts.entry("outcome_unknown").or_insert(0) += 1;
    }
    (results, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transcript decides: rows older than the pickup belong to earlier
    /// turns, the turn's prompt alone (or a tool round) is an interrupted
    /// turn, and a closing assistant row is an answered one.
    #[test]
    fn the_transcript_says_how_far_a_redelivered_turn_got() {
        let dir = tempfile::tempdir().expect("temp dir");
        let agent_message = json!("agent_message");
        let evidence = |name: &str, rows: &[Value], prompt: TurnPrompt<'_>| {
            let path = dir.path().join(name);
            let text = rows
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(&path, text).expect("write the transcript");
            transcript_evidence(&path, "2026-01-01T00:00:05.000Z", prompt)
                .expect("read the transcript")
        };
        let message = |at: &str, role: &str, stop: &str| {
            json!({
                "type": "message",
                "timestamp": at,
                "message": { "role": role, "stopReason": stop },
            })
        };
        let earlier = [
            message("2026-01-01T00:00:01.000Z", "user", ""),
            message("2026-01-01T00:00:02.000Z", "assistant", "stop"),
        ];
        let prompt = message("2026-01-01T00:00:05.000Z", "user", "");
        let tool_round = [
            message("2026-01-01T00:00:06.000Z", "assistant", "toolUse"),
            message("2026-01-01T00:00:07.000Z", "toolResult", ""),
        ];
        let answer = message("2026-01-01T00:00:08.000Z", "assistant", "stop");
        let injected = json!({
            "type": "custom_message",
            "timestamp": "2026-01-01T00:00:05.000Z",
            "customType": "agent_message",
        });
        let custom = TurnPrompt::Custom(&agent_message);
        assert_eq!(
            [
                evidence("unstarted", &earlier, TurnPrompt::User),
                evidence(
                    "prompted",
                    &[earlier.to_vec(), vec![prompt.clone()]].concat(),
                    TurnPrompt::User
                ),
                evidence(
                    "mid-tools",
                    &[vec![prompt.clone()], tool_round.to_vec()].concat(),
                    TurnPrompt::User
                ),
                evidence(
                    "answered",
                    &[vec![prompt.clone()], tool_round.to_vec(), vec![answer]].concat(),
                    TurnPrompt::User
                ),
                evidence("other-prompt", &[prompt], custom),
                evidence("injected", &[injected], custom),
            ],
            [
                RevivedTurn::Unstarted,
                RevivedTurn::Interrupted,
                RevivedTurn::Interrupted,
                RevivedTurn::Answered,
                RevivedTurn::Unstarted,
                RevivedTurn::Interrupted,
            ]
        );
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
        let answered = |rows: &[Value]| {
            let (results, counts) = interrupted_call_results(
                rows.iter()
                    .map(|row| (row["type"].as_str().unwrap_or_default(), row)),
                LifetimeEnd::DaemonRestart,
                2,
            );
            let saved: Vec<Value> = results
                .iter()
                .map(|message| json!([message["toolCallId"], message["content"][0]["text"]]))
                .collect();
            json!({ "saved": saved, "counts": counts })
        };
        let head = "[outcome-unknown] The daemon hosting this session stopped while this `eval` call ran, so its outcome is unknown: it may have run in part, in full or not at all, and its result was lost. It was not run again: check its effects before you run it again.\n\nWhat is known:\n- ";
        assert_eq!(
            [answered(&turn), answered(&moved_on)],
            [
                json!({
                    "saved": [
                        [
                            "ran",
                            format!(
                                "{head}it runs `bash`: a command it started may have done part of its work; 2 programs the session's commands left running were stopped with the session\n\
                                 - it writes files (`fs-write` or `fs-edit`): each write replaces its file whole, so a file it wrote holds its old text or its new, never part of either; read the files to see which writes landed\n\
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
