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

use std::path::Path;

use pa_core::session::revival::{
    transcript_evidence, RevivedTurn, TurnPrompt, TURN_INTERRUPTED_CUSTOM_TYPE,
    TURN_INTERRUPTED_NOTICE,
};
use serde_json::{json, Value};

use crate::journal::{WorkerQueueItemRecord, WorkerRecoveryJournal};

/// How far the turn picked up at `picked_up_at` got in `session_file`, by
/// the first row it was delivered with.
fn journal_evidence(
    session_file: &Path,
    picked_up_at: &str,
    first: &WorkerQueueItemRecord,
) -> std::io::Result<RevivedTurn> {
    let custom_type = first
        .custom_message
        .as_ref()
        .map(|row| row.get("customType").cloned().unwrap_or(Value::Null));
    let prompt = match &custom_type {
        Some(custom_type) => TurnPrompt::Custom(custom_type),
        None => TurnPrompt::User,
    };
    transcript_evidence(session_file, picked_up_at, prompt)
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
        let evidence = match journal_evidence(
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
                message: TURN_INTERRUPTED_NOTICE.to_string(),
                priority: Some(crate::worker::QueuePriority::Human),
                preview: None,
                custom_message: Some(json!({
                    "role": "custom",
                    "customType": TURN_INTERRUPTED_CUSTOM_TYPE,
                    "content": TURN_INTERRUPTED_NOTICE,
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
