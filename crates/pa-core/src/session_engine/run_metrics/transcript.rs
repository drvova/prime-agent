//! The offline feed: a saved session file's runs through the same
//! [`MetricsMeter`] the live telemetry uses.
//!
//! Lines are read in file order, which is the order the requests were
//! sent (the file is append-only); a branched session therefore yields the
//! runs of every branch in the order they ran. Child usage attributed into
//! a parent row is not folded in (that is the subagent's own request, not
//! this session's), so each request's tokens are the ones it reported.
//!
//! A run opens at a user message that follows a finished turn (no turn yet,
//! a last assistant message that did not end in tool use, or a tool result
//! with which the loop guard ended the turn): a message
//! typed while a turn runs is steering inside that run, and a retried
//! attempt adds no user message. User messages before the run's first
//! request (a harness digest ahead of the prompt) open no run of their own.
//! A compaction counts into the open run, as
//! the live post-run compaction drain does; one before any run is skipped.

use pa_agent::types::{Message, StopReason};
use serde_json::Value;

use super::{loop_guard_stopped, MetricsMeter, RunMetrics, SpawnVisibility};

/// The open run's first and last entry timestamps (epoch ms, 0 while none
/// was readable), and whether it made a model request yet.
struct OpenRun {
    started_at: u64,
    last_at: u64,
    requested: bool,
}

impl OpenRun {
    fn wall_ms(&self) -> u64 {
        if self.started_at == 0 {
            0
        } else {
            self.last_at.saturating_sub(self.started_at)
        }
    }
}

/// Every run of a saved session file body, in order. Lines that are not
/// JSON, entries that are not messages or compactions, and messages of
/// custom roles are skipped.
#[must_use]
pub fn transcript_run_metrics(content: &str) -> Vec<RunMetrics> {
    let mut meter = MetricsMeter::new(SpawnVisibility::NotRecorded);
    let mut runs = Vec::new();
    let mut open: Option<OpenRun> = None;
    let mut turn_open = false;
    for line in content.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("message") => {}
            Some("compaction") => {
                if open.is_some() {
                    meter.compaction();
                }
                continue;
            }
            _ => continue,
        }
        let Some(message) = entry
            .get("message")
            .and_then(|message| serde_json::from_value::<Message>(message.clone()).ok())
        else {
            continue;
        };
        // The entry's own stamp: when the row was written (an assistant row
        // at its message end), the run's wall clock.
        let timestamp = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .map_or(0, crate::session::timestamp_to_millis);
        if matches!(message, Message::User(_)) && !turn_open {
            if let Some(run) = open.take_if(|run| run.requested) {
                runs.push(meter.finish_run(run.wall_ms()));
            }
        }
        let run = open.get_or_insert(OpenRun {
            started_at: timestamp,
            last_at: timestamp,
            requested: false,
        });
        if run.started_at == 0 {
            run.started_at = timestamp;
        }
        run.last_at = run.last_at.max(timestamp);
        match &message {
            Message::User(_) => {}
            Message::Assistant(assistant) => {
                run.requested = true;
                turn_open = assistant.stop_reason == StopReason::ToolUse;
                meter.request(assistant);
            }
            Message::ToolResult(result) => {
                let details = result.details.as_ref().unwrap_or(&Value::Null);
                meter.tool_result(&result.tool_name, result.is_error, details);
                // A turn the loop guard ended has no further request: the
                // next user message opens a run.
                if loop_guard_stopped(details) {
                    turn_open = false;
                }
            }
        }
    }
    if let Some(run) = open {
        runs.push(meter.finish_run(run.wall_ms()));
    }
    runs
}
