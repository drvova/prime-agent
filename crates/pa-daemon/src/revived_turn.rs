//! What becomes of the turn a session was running when its daemon went down.
//!
//! The recovery journal keeps the rows of a picked-up turn so a revival can
//! redeliver them. Redelivering them blindly prompts the model a second time
//! with a row the transcript already holds. The transcript says how far the
//! turn got, and the revival follows it:
//!
//! - nothing of the turn was saved: it is delivered as it was queued;
//! - its prompt was saved but no final answer: the turn continues from the
//!   transcript behind one notice row that tells the model (and the user)
//!   it was interrupted, and the prompt is not recorded again;
//! - its final answer was saved: the turn is over, and nothing is delivered.

use std::io::BufRead as _;
use std::path::Path;

use serde_json::{json, Value};

use crate::journal::{WorkerQueueItemRecord, WorkerRecoveryJournal};

/// The custom type of the notice a continued turn opens with.
pub(crate) const TURN_INTERRUPTED_CUSTOM_TYPE: &str = "turn_interrupted";

/// The notice text.
const NOTICE_CONTENT: &str = "[turn-interrupted]\n\nThe daemon hosting this session restarted while your previous turn was running. Everything above was saved; a reply or tool call that was in flight was lost, and a tool call with no result did not finish (check what it changed before you repeat it). Continue the interrupted turn from where it stopped.";

/// How far the transcript shows a redelivered turn got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevivedTurn {
    /// None of its rows were saved.
    Unstarted,
    /// Its prompt was saved, its final answer was not.
    Interrupted,
    /// Its final answer was saved.
    Answered,
}

/// How far the turn picked up at `picked_up_at` (the journal's clock, which
/// is the transcript's) got in `session_file`. `first` is the first row the
/// turn was delivered with: a turn records it before anything else.
///
/// # Errors
///
/// Returns an error when the session file cannot be read.
pub(crate) fn transcript_evidence(
    session_file: &Path,
    picked_up_at: &str,
    first: &WorkerQueueItemRecord,
) -> std::io::Result<RevivedTurn> {
    let prompt_type = first
        .custom_message
        .as_ref()
        .map(|row| row.get("customType").cloned().unwrap_or(Value::Null));
    let mut verdict = RevivedTurn::Unstarted;
    for line in std::io::BufReader::new(std::fs::File::open(session_file)?).lines() {
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
        let is_prompt = match &prompt_type {
            Some(custom_type) => {
                entry["type"] == "custom_message" && entry["customType"] == *custom_type
            }
            None => entry["type"] == "message" && role == Some("user"),
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

/// Make the journal at `journal_path` redeliver what the transcript says is
/// still owed of each turn that was running, and answer how far the turn got
/// (`None` when no turn was running). The lifetime that wrote the journal is
/// gone: this is its only writer until the session is relaunched.
///
/// # Errors
///
/// Returns an error when the journal or the session file cannot be read, or
/// the journal cannot be appended.
pub(crate) fn settle_in_journal(
    journal_path: &Path,
    session_file: &Path,
) -> anyhow::Result<Option<RevivedTurn>> {
    let mut settled = None;
    let mut failure = None;
    WorkerRecoveryJournal::replace_running_turns(journal_path, |snapshot| {
        let evidence = match transcript_evidence(
            session_file,
            &snapshot.recorded_at,
            snapshot.running.first()?,
        ) {
            Ok(evidence) => evidence,
            Err(error) => {
                failure = Some(error);
                return None;
            }
        };
        settled = Some(evidence);
        match evidence {
            RevivedTurn::Unstarted => None,
            RevivedTurn::Answered => Some(Vec::new()),
            RevivedTurn::Interrupted => Some(vec![WorkerQueueItemRecord {
                message: NOTICE_CONTENT.to_string(),
                priority: Some(crate::worker::QueuePriority::Human),
                preview: None,
                custom_message: Some(json!({
                    "role": "custom",
                    "customType": TURN_INTERRUPTED_CUSTOM_TYPE,
                    "content": NOTICE_CONTENT,
                    "display": true,
                    "details": { "cause": "daemon_restart" },
                })),
                queue_key: None,
                queue_visible: false,
                policy: "injected".to_string(),
                forced_batch: false,
                images: Vec::new(),
            }]),
        }
    })?;
    if let Some(error) = failure {
        return Err(error.into());
    }
    if settled == Some(RevivedTurn::Answered) {
        WorkerRecoveryJournal::settle_busy_records(journal_path, "turn_answered")?;
    }
    Ok(settled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_item(custom_type: Option<&str>) -> WorkerQueueItemRecord {
        WorkerQueueItemRecord {
            message: "do it".to_string(),
            priority: None,
            preview: None,
            custom_message: custom_type.map(|custom_type| json!({ "customType": custom_type })),
            queue_key: None,
            queue_visible: true,
            policy: "queued".to_string(),
            forced_batch: false,
            images: Vec::new(),
        }
    }

    /// The transcript decides: rows older than the pickup belong to earlier
    /// turns, the turn's prompt alone (or a tool round) is an interrupted
    /// turn, and a closing assistant row is an answered one.
    #[test]
    fn the_transcript_says_how_far_a_redelivered_turn_got() {
        let dir = tempfile::tempdir().expect("temp dir");
        let evidence = |name: &str, rows: &[Value], custom_type: Option<&str>| {
            let path = dir.path().join(name);
            let text = rows
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(&path, text).expect("write the transcript");
            transcript_evidence(&path, "2026-01-01T00:00:05.000Z", &prompt_item(custom_type))
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
        assert_eq!(
            [
                evidence("unstarted", &earlier, None),
                evidence(
                    "prompted",
                    &[earlier.to_vec(), vec![prompt.clone()]].concat(),
                    None
                ),
                evidence(
                    "mid-tools",
                    &[vec![prompt.clone()], tool_round.to_vec()].concat(),
                    None
                ),
                evidence(
                    "answered",
                    &[vec![prompt.clone()], tool_round.to_vec(), vec![answer]].concat(),
                    None
                ),
                evidence("other-prompt", &[prompt], Some("agent_message")),
                evidence("injected", &[injected], Some("agent_message")),
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

    /// A turn whose answer is in the transcript is not redelivered and no
    /// longer counts as live work; one cut off before its answer is
    /// redelivered as the notice row alone, never as its prompt again.
    #[test]
    fn a_revival_redelivers_only_what_the_transcript_says_is_still_owed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let revive = |name: &str, answered: bool| {
            let journal_path = dir.path().join(format!("{name}.recovery.jsonl"));
            let transcript = dir.path().join(format!("{name}.jsonl"));
            {
                let mut journal = WorkerRecoveryJournal::open(&journal_path).expect("journal");
                journal
                    .record("live", "session", None, /*busy*/ true, "prompt")
                    .expect("busy record");
                journal
                    .record_turn_pickup(
                        "live",
                        vec![prompt_item(None)],
                        /*running_steering*/ false,
                        crate::worker::QueueLanes {
                            steering: Vec::new(),
                            follow_up: vec![prompt_item(None)],
                        },
                        /*suspended*/ false,
                    )
                    .expect("pickup record");
            }
            let at = crate::util::now_iso();
            let mut rows = vec![json!({
                "type": "message", "timestamp": at, "message": { "role": "user" },
            })];
            if answered {
                rows.push(json!({
                    "type": "message",
                    "timestamp": at,
                    "message": { "role": "assistant", "stopReason": "stop" },
                }));
            }
            let text = rows
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(&transcript, text).expect("write the transcript");
            let settled = settle_in_journal(&journal_path, &transcript).expect("settle");
            let (_, follow_up, _) =
                WorkerRecoveryJournal::read_queue_snapshot(&journal_path, "live")
                    .expect("read the snapshot")
                    .expect("the snapshot");
            (
                settled,
                follow_up
                    .iter()
                    .map(|item| {
                        item.custom_message.as_ref().map_or_else(
                            || "prompt".to_string(),
                            |row| row["customType"].as_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect::<Vec<_>>(),
                WorkerRecoveryJournal::read_interrupted(&journal_path),
            )
        };
        assert_eq!(
            [revive("answered", true), revive("cut-off", false)],
            [
                (
                    Some(RevivedTurn::Answered),
                    vec!["prompt".to_string()],
                    false
                ),
                (
                    Some(RevivedTurn::Interrupted),
                    vec![
                        TURN_INTERRUPTED_CUSTOM_TYPE.to_string(),
                        "prompt".to_string()
                    ],
                    true
                ),
            ]
        );
    }
}
