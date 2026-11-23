//! Crash visibility: the rows and labels that tell the user a hosted session
//! crashed, recovered or was given up on, and that the daemon itself died and
//! was restarted. Everything renders from the wire types alone.

use pa_types::daemon::{
    DaemonDeathOutcome, DaemonDeathRecord, SessionCrashInfo, SessionFaultEvent, SessionFaultState,
};

use crate::chat::{ChatEntry, StatusKind};

/// A daemon death older than this is history: a window opened later does not
/// lead with it (a window that lived through it always reports it).
const RECENT_DEATH_MS: i64 = 24 * 60 * 60 * 1000;

/// Report one crash notice shown to the user (event `tui crash notice`). Fire-and-forget:
/// the report is handed to a spawned task, so the notice's paint never waits on telemetry,
/// whatever the sink does.
pub(crate) fn report_shown(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    kind: &'static str,
) {
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry.crash_notice_shown(kind).await;
        });
    }
}

/// The fault's telemetry kind: a fixed word per fault, never its text.
pub(crate) fn fault_kind(fault: &SessionFaultEvent) -> &'static str {
    match fault {
        SessionFaultEvent::SessionCrashed(_) => "session_crashed",
        SessionFaultEvent::SessionRecovered { .. } => "session_recovered",
        SessionFaultEvent::SessionFailed { .. } => "session_failed",
    }
}

/// One line saying what a session went down with: the fault's kind, its own
/// message and, for a panic, where it was raised.
pub(crate) fn crash_reason(crash: &SessionCrashInfo) -> String {
    let what = match crash.kind.as_str() {
        "daemon_restart" => "interrupted by a daemon restart".to_string(),
        "stuck" => "stopped (stuck)".to_string(),
        kind => format!("crashed ({kind})"),
    };
    if crash.location.is_empty() {
        format!("Session {what}: {}", crash.message)
    } else {
        format!("Session {what}: {} at {}", crash.message, crash.location)
    }
}

/// The transcript row for one session fault event.
pub(crate) fn fault_row(fault: &SessionFaultEvent) -> ChatEntry {
    let (text, kind) = match fault {
        SessionFaultEvent::SessionCrashed(crash) => (
            format!(
                "{} \u{2014} the session is being rebuilt from its transcript.",
                crash_reason(crash)
            ),
            StatusKind::Error,
        ),
        SessionFaultEvent::SessionRecovered {
            queued_input_suspended: false,
            ..
        } => (
            "Session recovered \u{2014} it was rebuilt from its transcript and is ready.".to_string(),
            StatusKind::Info,
        ),
        SessionFaultEvent::SessionRecovered {
            queued_input_suspended: true,
            ..
        } => (
            "Session recovered \u{2014} it was rebuilt from its transcript. It is still stopped by your abort: queued input waits until you resume it.".to_string(),
            StatusKind::Info,
        ),
        SessionFaultEvent::SessionFailed { failures } => (
            format!(
                "Session failed \u{2014} it went down {failures} times in a row and was not started again. The transcript is saved; reopen the session from Agents View."
            ),
            StatusKind::Error,
        ),
    };
    ChatEntry::Status { text, kind }
}

/// The fault a roster row carries (its summary's `lastCrash`), when the daemon tracks what
/// became of the session since.
pub(crate) fn row_fault(summary: &serde_json::Value) -> Option<SessionCrashInfo> {
    summary
        .get("lastCrash")
        .and_then(|crash| serde_json::from_value::<SessionCrashInfo>(crash.clone()).ok())
        .filter(|crash| crash.state.is_some())
}

/// The agents-view badge of a faulted session and whether it is still a problem (a session
/// that recovered is marked, not alarmed about).
pub(crate) fn fault_badge(crash: &SessionCrashInfo) -> Option<(&'static str, bool)> {
    Some(match crash.state? {
        SessionFaultState::Failed => ("\u{2717} failed", true),
        SessionFaultState::Rebuilding => ("\u{26a0} crashed", true),
        SessionFaultState::Recovered => ("\u{26a0} recovered", false),
    })
}

/// The selected row's fault line: the reason, and what became of the session.
pub(crate) fn fault_detail(crash: &SessionCrashInfo) -> Option<String> {
    let after = match crash.state? {
        SessionFaultState::Failed => {
            "it kept going down and was not started again; open it to start it"
        }
        SessionFaultState::Rebuilding => "it is being rebuilt from its transcript",
        SessionFaultState::Recovered => "it was rebuilt from its transcript and is serving",
    };
    Some(format!(
        "{} ({}) \u{2014} {after}",
        crash_reason(crash),
        crash.at
    ))
}

/// The transcript row for the death the connected daemon was started after.
/// `now_ms` is the wall clock when the caller wants only a recent death
/// reported (a window opening); `None` reports it whatever its age (a window
/// that lost its connection to that daemon).
pub(crate) fn daemon_death_row(
    record: &DaemonDeathRecord,
    now_ms: Option<i64>,
) -> Option<ChatEntry> {
    if let Some(now_ms) = now_ms {
        let at_ms = crate::agents_view_state::iso_to_unix_ms(&record.at)?;
        if now_ms - at_ms > RECENT_DEATH_MS {
            return None;
        }
    }
    let how = match (record.signal, record.exit_code, &record.error) {
        (Some(signal), _, _) => {
            let name = match signal {
                6 => " (SIGABRT)",
                7 => " (SIGBUS)",
                9 => " (SIGKILL)",
                11 => " (SIGSEGV)",
                15 => " (SIGTERM)",
                _ => "",
            };
            format!("was killed by signal {signal}{name}")
        }
        (None, Some(code), _) => format!("exited with status {code}"),
        (None, None, Some(error)) => format!("could not start ({error})"),
        (None, None, None) => "ended without a status".to_string(),
    };
    let sessions = match (record.live_sessions, record.busy_sessions) {
        (Some(1), Some(busy)) => format!(" with 1 live session, {busy} mid-turn"),
        (Some(live), Some(busy)) => format!(" with {live} live sessions, {busy} mid-turn"),
        (Some(1), None) => " with 1 live session".to_string(),
        (Some(live), None) => format!(" with {live} live sessions"),
        (None, _) => String::new(),
    };
    let after = match record.outcome {
        DaemonDeathOutcome::Restarted => {
            "It was restarted automatically; a turn that was running is reported in its session."
                .to_string()
        }
        DaemonDeathOutcome::GaveUp => format!(
            "That was {} deaths in a row, so automatic restarts stopped until this daemon was started.",
            record.consecutive
        ),
    };
    Some(ChatEntry::Status {
        text: format!(
            "The Prime Agent daemon {how} at {}{sessions}. {after}",
            record.at
        ),
        kind: StatusKind::Warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(text: &str, kind: StatusKind) -> ChatEntry {
        ChatEntry::Status {
            text: text.to_string(),
            kind,
        }
    }

    #[test]
    fn each_session_fault_event_decodes_to_its_transcript_row() {
        let rows: Vec<Option<crate::snapshot::TurnUpdate>> = [
            json!({ "type": "session_crashed", "kind": "panic", "message": "boom",
                "location": "src/a.rs:1:2", "thread": "session-1", "at": "2026-01-01T00:00:00.000Z" }),
            json!({ "type": "session_crashed", "kind": "daemon_restart",
                "message": "the daemon stopped while this session's turn was running" }),
            json!({ "type": "session_recovered", "failures": 1 }),
            json!({ "type": "session_failed", "failures": 5 }),
        ]
        .iter()
        .map(crate::snapshot::event_to_update)
        .collect();
        let shown: Vec<Option<ChatEntry>> = rows
            .into_iter()
            .map(|update| match update {
                Some(crate::snapshot::TurnUpdate::SessionFault(fault)) => Some(fault_row(&fault)),
                _ => None,
            })
            .collect();
        assert_eq!(
            shown,
            vec![
                Some(status(
                    "Session crashed (panic): boom at src/a.rs:1:2 \u{2014} the session is being rebuilt from its transcript.",
                    StatusKind::Error
                )),
                Some(status(
                    "Session interrupted by a daemon restart: the daemon stopped while this session's turn was running \u{2014} the session is being rebuilt from its transcript.",
                    StatusKind::Error
                )),
                Some(status(
                    "Session recovered \u{2014} it was rebuilt from its transcript and is ready.",
                    StatusKind::Info
                )),
                Some(status(
                    "Session failed \u{2014} it went down 5 times in a row and was not started again. The transcript is saved; reopen the session from Agents View.",
                    StatusKind::Error
                )),
            ]
        );
    }

    #[test]
    fn a_daemon_death_is_reported_with_its_cause_and_only_while_recent_for_a_new_window() {
        let record = |outcome| DaemonDeathRecord {
            at: "2026-01-02T00:00:00.000Z".to_string(),
            pid: 41,
            exit_code: None,
            signal: Some(9),
            error: None,
            uptime_ms: 1000,
            live_sessions: Some(3),
            busy_sessions: Some(1),
            consecutive: 5,
            outcome,
        };
        let at_ms = crate::agents_view_state::iso_to_unix_ms("2026-01-02T00:00:00.000Z")
            .expect("a timestamp");
        assert_eq!(
            (
                daemon_death_row(&record(DaemonDeathOutcome::Restarted), Some(at_ms + 1000)),
                daemon_death_row(&record(DaemonDeathOutcome::GaveUp), None),
                daemon_death_row(
                    &record(DaemonDeathOutcome::Restarted),
                    Some(at_ms + RECENT_DEATH_MS + 1)
                ),
            ),
            (
                Some(status(
                    "The Prime Agent daemon was killed by signal 9 (SIGKILL) at 2026-01-02T00:00:00.000Z with 3 live sessions, 1 mid-turn. It was restarted automatically; a turn that was running is reported in its session.",
                    StatusKind::Warning
                )),
                Some(status(
                    "The Prime Agent daemon was killed by signal 9 (SIGKILL) at 2026-01-02T00:00:00.000Z with 3 live sessions, 1 mid-turn. That was 5 deaths in a row, so automatic restarts stopped until this daemon was started.",
                    StatusKind::Warning
                )),
                None,
            )
        );
    }
}
