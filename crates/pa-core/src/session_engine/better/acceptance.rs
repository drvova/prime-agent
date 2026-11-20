//! Acceptance checks at turn end: a prompt that carries an `<acceptance>` block (a sent
//! `/better` rewrite, or a `better=True` subagent's brief) arms its checks, and every run that
//! ends cleanly afterwards runs them, until they pass or [`MAX_ROUNDS`] rounds have run.
//!
//! The hook is the agent's own `AgentEnd` event, which every transport's run emits. The listener
//! only decides and spawns: the round runs on a background task, so no run's settle, and no
//! frame a client paints after it, waits on a check. The round ends in one evidence record (kept
//! in the session's spill store as an `out:<id>` handle) and one notice row through the kernel
//! notice seam: a failure steers onto the session so the model continues with the raw reasons; a
//! pass is only recorded, a compact row with no turn behind it.
//!
//! A new prompt without a block disarms the checks (the user moved on); a prompt with one
//! replaces them.

use std::collections::VecDeque;
use std::sync::{Arc, PoisonError};

use pa_agent::types::{AgentMessage, Message, StopReason, UserContent, UserPart};
use serde_json::{json, Value};

use super::checks::{parse_acceptance, run_checks, AcceptanceCheck, CheckOutcome, CheckVerdict};
use super::BetterHost;
use crate::session_engine::bash_notices::{KernelNotice, NoticeDelivery};
use crate::session_engine::messages::{BETTER_CHECKS_CUSTOM_TYPE, BETTER_CHECKS_PREVIEW_LABEL};
use crate::session_engine::telemetry::BetterTelemetryFact;

/// Rounds one armed set runs before it stops re-running on its own: enough for a fix and a
/// second try, few enough that a check the model cannot satisfy does not loop the session.
pub(crate) const MAX_ROUNDS: u32 = 3;

/// Evidence records the host keeps for `better.evidence(handle)` (the spill store keeps every
/// report's text past that).
const EVIDENCE_KEPT: usize = 16;

/// Where the armed checks stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// No prompt armed any checks.
    None,
    /// Armed; no round has finished yet.
    Pending,
    /// The last finished round passed every check.
    Passed,
    /// The last finished round failed a check.
    Failed,
}

impl CheckStatus {
    /// The wire name (`none`, `pending`, `passed`, `failed`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// One finished round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// `out:<id>` when the spill store kept the report, `evidence:<n>` otherwise.
    pub handle: String,
    pub round: u32,
    pub outcomes: Vec<CheckOutcome>,
}

impl Evidence {
    fn failed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| !outcome.passed())
            .count()
    }

    /// The REPL's view.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "handle": self.handle,
            "round": self.round,
            "passed": self.outcomes.len() - self.failed(),
            "failed": self.failed(),
            "checks": self.outcomes.iter().map(CheckOutcome::to_json).collect::<Vec<_>>(),
        })
    }
}

/// The checks one prompt armed.
#[derive(Debug, Clone)]
struct Armed {
    checks: Vec<AcceptanceCheck>,
    /// Rounds started for this set.
    rounds: u32,
}

/// The host's acceptance state.
#[derive(Debug, Default)]
pub(super) struct AcceptanceState {
    armed: Option<Armed>,
    /// Bumped by every arm and disarm: a round started under an older generation only records.
    generation: u64,
    running: bool,
    /// The status of the set the last finished round belongs to.
    last_status: Option<CheckStatus>,
    evidence: VecDeque<Evidence>,
    minted: u64,
}

impl BetterHost {
    fn acceptance(&self) -> std::sync::MutexGuard<'_, AcceptanceState> {
        self.acceptance
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Bind the seam a round's notice goes out through (first bind wins): the daemon worker's
    /// queue lanes, or a daemonless embedding's in-process steer.
    pub fn bind_notice_sink(&self, sink: crate::session_engine::bash_notices::KernelNoticeSink) {
        let _ = self.notice.set(sink);
    }

    /// Run the armed checks after every run `agent` finishes (see the module docs).
    pub async fn watch_turn_ends(self: &Arc<Self>, agent: &pa_agent::agent::Agent) {
        let host = Arc::clone(self);
        agent
            .subscribe(move |event, _signal| {
                if let pa_agent::types::AgentEvent::AgentEnd { messages } = &event {
                    // Spawned, never awaited: the run settles while the round runs.
                    let _round = host.turn_ended(messages);
                }
                Box::pin(async { Ok(()) })
            })
            .await;
    }

    /// Arm or disarm from the run's latest prompt, then start a round when one is due; the
    /// round's task when one started.
    pub(crate) fn turn_ended(
        self: &Arc<Self>,
        messages: &[AgentMessage],
    ) -> Option<tokio::task::JoinHandle<()>> {
        let ended_cleanly = messages.iter().rev().find_map(|message| match message {
            AgentMessage::Standard(Message::Assistant(assistant)) => Some(!matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            )),
            AgentMessage::Standard(Message::User(_) | Message::ToolResult(_))
            | AgentMessage::Custom(_) => None,
        });
        let (generation, round, checks) = {
            let mut state = self.acceptance();
            if let Some(prompt) = messages.iter().rev().find_map(prompt_text) {
                state.generation += 1;
                state.armed = parse_acceptance(&prompt).map(|checks| Armed { checks, rounds: 0 });
                if state.armed.is_some() {
                    // A new set starts with no verdict; a plain prompt keeps the last one.
                    state.last_status = None;
                }
            }
            if ended_cleanly != Some(true) || state.running {
                return None;
            }
            let Some(armed) = state.armed.as_mut() else {
                return None;
            };
            armed.rounds += 1;
            let (round, checks) = (armed.rounds, armed.checks.clone());
            state.running = true;
            (state.generation, round, checks)
        };
        self.note(BetterTelemetryFact::CheckRound);
        let host = Arc::clone(self);
        Some(tokio::spawn(async move {
            let outcomes = run_checks(&checks, &host.cwd, host.check_runner.as_ref()).await;
            host.finish_round(generation, round, outcomes);
        }))
    }

    /// Record one finished round, settle the armed set, and send its notice.
    fn finish_round(&self, generation: u64, round: u32, outcomes: Vec<CheckOutcome>) {
        for outcome in &outcomes {
            self.note(if outcome.passed() {
                BetterTelemetryFact::CheckPassed
            } else {
                BetterTelemetryFact::CheckFailed
            });
        }
        let passed = outcomes.iter().all(CheckOutcome::passed);
        // The spill id is the report's content hash, so the stored report names no handle.
        let stored = self
            .keep(report_text(None, round, &outcomes, None))
            .stored
            .map(|(handle, _)| handle);
        let (evidence, current) = {
            let mut state = self.acceptance();
            state.running = false;
            let current = state.generation == generation;
            if current {
                state.last_status = Some(if passed {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Failed
                });
                if passed || round >= MAX_ROUNDS {
                    state.armed = None;
                }
            }
            state.minted += 1;
            let evidence = Evidence {
                handle: stored.unwrap_or_else(|| format!("evidence:{}", state.minted)),
                round,
                outcomes,
            };
            if state.evidence.len() == EVIDENCE_KEPT {
                state.evidence.pop_front();
            }
            state.evidence.push_back(evidence.clone());
            (evidence, current)
        };
        let Some(sink) = self.notice.get() else {
            return;
        };
        // A failure of the set still armed steers the model on; anything else only records.
        let delivery = if !passed && current {
            NoticeDelivery::Steer
        } else {
            NoticeDelivery::Record
        };
        let ending = match delivery {
            NoticeDelivery::Steer if round >= MAX_ROUNDS => Some(LAST_ROUND_NOTE),
            NoticeDelivery::Steer => Some(RETRY_NOTE),
            NoticeDelivery::Record => None,
        };
        sink(KernelNotice {
            row: notice_row(&evidence, ending),
            preview_label: BETTER_CHECKS_PREVIEW_LABEL,
            delivery,
        });
    }

    /// Where the armed checks stand: the REPL's `better.checks()`.
    #[must_use]
    pub fn checks_status(&self) -> Value {
        let state = self.acceptance();
        json!({
            "status": status_of(&state).as_str(),
            "checks": state.armed.as_ref().map_or_else(Vec::new, |armed| {
                armed.checks.iter().map(AcceptanceCheck::to_json).collect()
            }),
            "rounds": state.armed.as_ref().map_or(0, |armed| armed.rounds),
            "max_rounds": MAX_ROUNDS,
            "running": state.running,
            "last": state.evidence.back().map(Evidence::to_json),
        })
    }

    /// The status of the checks this session's prompts armed.
    #[must_use]
    pub fn check_status(&self) -> CheckStatus {
        status_of(&self.acceptance())
    }

    /// One evidence record by its handle: the REPL's `better.evidence(handle)`.
    #[must_use]
    pub fn evidence(&self, handle: &str) -> Option<Evidence> {
        self.acceptance()
            .evidence
            .iter()
            .find(|evidence| evidence.handle == handle)
            .cloned()
    }
}

fn status_of(state: &AcceptanceState) -> CheckStatus {
    match (state.running, &state.armed, state.last_status) {
        (true, _, _) | (false, Some(_), None) => CheckStatus::Pending,
        (false, _, Some(status)) => status,
        (false, None, None) => CheckStatus::None,
    }
}

/// Where the acceptance checks of the session whose durable file is `session_file` stand, read
/// from its rows (blocking: call off the async threads): a check round's row decides, a later
/// prompt that arms new checks reads pending, and a session with neither reads none. This is
/// how a parent reads a child whose host runs in another process.
#[must_use]
pub fn check_status_from_session_file(session_file: &std::path::Path) -> CheckStatus {
    let Ok(content) = std::fs::read_to_string(session_file) else {
        return CheckStatus::None;
    };
    let mut status = CheckStatus::None;
    for line in content.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        // A round's row: a recorded `custom_message`, or a steered notice the next turn carried.
        let row = match entry.get("type").and_then(Value::as_str) {
            Some("custom_message") => Some(&entry),
            Some("message") => entry.get("message"),
            _ => None,
        };
        let Some(row) = row else {
            continue;
        };
        if row.get("customType").and_then(Value::as_str) == Some(BETTER_CHECKS_CUSTOM_TYPE) {
            status = match row.pointer("/details/failed").and_then(Value::as_u64) {
                Some(0) => CheckStatus::Passed,
                _ => CheckStatus::Failed,
            };
        } else if row.get("role").and_then(Value::as_str) == Some("user") {
            let text = match row.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if parse_acceptance(&text).is_some() {
                status = CheckStatus::Pending;
            }
        }
    }
    status
}

/// The text of a run's prompt: a user message (a notice or other custom row is not a prompt).
fn prompt_text(message: &AgentMessage) -> Option<String> {
    match message {
        AgentMessage::Standard(Message::User(user)) => Some(match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    UserPart::Text(text) => Some(text.text.as_str()),
                    UserPart::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }),
        AgentMessage::Standard(Message::Assistant(_) | Message::ToolResult(_))
        | AgentMessage::Custom(_) => None,
    }
}

const RETRY_NOTE: &str = "The acceptance checks your task carries did not all pass. Fix what \
                          failed; they run again when your turn ends.";
const LAST_ROUND_NOTE: &str = "The acceptance checks your task carries did not all pass, and \
                               that was the last automatic round. Say which checks still fail \
                               and why.";

/// The report: a header naming the handle, one line per check, and each failure's raw reason.
fn report_text(
    handle: Option<&str>,
    round: u32,
    outcomes: &[CheckOutcome],
    ending: Option<&str>,
) -> String {
    let failed = outcomes.iter().filter(|outcome| !outcome.passed()).count();
    let handle = handle
        .map(|handle| format!(" {handle}"))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "[better-checks{handle} · {} passed · {failed} failed · round {round} of {MAX_ROUNDS}]",
        outcomes.len() - failed
    )];
    for outcome in outcomes {
        match &outcome.verdict {
            CheckVerdict::Passed => lines.push(format!("✓ {}", outcome.check.label())),
            CheckVerdict::Failed { reason } => {
                lines.push(format!("✗ {}", outcome.check.label()));
                lines.extend(reason.lines().map(|line| format!("  {line}")));
            }
        }
    }
    if let Some(ending) = ending {
        lines.push(ending.to_string());
    }
    lines.join("\n")
}

/// The notice row: the report as content, the counts and the compact summary in `details`.
fn notice_row(evidence: &Evidence, ending: Option<&str>) -> pa_types::session::CustomMessage {
    let failed = evidence.failed();
    let total = evidence.outcomes.len();
    let summary = if failed == 0 {
        format!(
            "Acceptance checks: {total}/{total} passed · {}",
            evidence.handle
        )
    } else {
        format!(
            "Acceptance checks: {failed} of {total} failed · round {} of {MAX_ROUNDS} · {}",
            evidence.round, evidence.handle
        )
    };
    pa_types::session::CustomMessage {
        custom_type: BETTER_CHECKS_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(report_text(
            Some(&evidence.handle),
            evidence.round,
            &evidence.outcomes,
            ending,
        )),
        display: true,
        details: Some(json!({
            "handle": evidence.handle,
            "round": evidence.round,
            "passed": total - failed,
            "failed": failed,
            "summary": summary,
        })),
        timestamp: crate::autonomous::now_millis(),
        rest: serde_json::Map::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::autonomous::{ChildProcessResult, GateCommandRunner, GateRunFuture};

    /// Answers `exit N` with that exit code; `hang` never answers.
    struct ScriptRunner;

    impl GateCommandRunner for ScriptRunner {
        fn run_gate(&self, command: &str, _timeout_ms: u64) -> GateRunFuture<'_> {
            if command == "hang" {
                return Box::pin(std::future::pending());
            }
            let status = command
                .strip_prefix("exit ")
                .and_then(|code| code.parse().ok());
            Box::pin(async move {
                Ok(ChildProcessResult {
                    status,
                    stderr: "boom".to_string(),
                    ..ChildProcessResult::default()
                })
            })
        }
    }

    fn host(cwd: &std::path::Path) -> (Arc<BetterHost>, Arc<Mutex<Vec<KernelNotice>>>) {
        let spill = crate::kernel::spill::SpillStore::for_session_dir(&cwd.join(".session"));
        let host = Arc::new(BetterHost::new(
            cwd.to_path_buf(),
            None,
            None,
            Some(spill),
            Arc::new(ScriptRunner),
        ));
        let notices: Arc<Mutex<Vec<KernelNotice>>> = Arc::default();
        let sink = Arc::clone(&notices);
        host.bind_notice_sink(Arc::new(move |notice| sink.lock().unwrap().push(notice)));
        (host, notices)
    }

    fn assistant(stop_reason: StopReason) -> AgentMessage {
        AgentMessage::from(pa_agent::types::AssistantMessage {
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pa_agent::types::Usage::zero(),
            stop_reason,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
        })
    }

    /// A woken continuation: the notice row, then the model's turn (no prompt).
    fn continuation() -> Vec<AgentMessage> {
        vec![
            AgentMessage::Custom(pa_agent::types::CustomAgentMessage {
                role: "custom".to_string(),
                payload: json!({ "customType": BETTER_CHECKS_CUSTOM_TYPE }),
            }),
            assistant(StopReason::Stop),
        ]
    }

    /// The delivery, content, and handle of every notice sent so far.
    fn sent(notices: &Mutex<Vec<KernelNotice>>) -> Vec<(NoticeDelivery, String, String)> {
        notices
            .lock()
            .unwrap()
            .iter()
            .map(|notice| {
                let pa_types::ai::UserContent::Text(text) = &notice.row.content else {
                    panic!("text notice");
                };
                let handle = notice.row.details.as_ref().unwrap()["handle"]
                    .as_str()
                    .unwrap()
                    .to_string();
                (notice.delivery, text.clone(), handle)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_failed_round_steers_its_raw_reasons_and_the_round_that_passes_only_records() {
        let dir = tempfile::tempdir().unwrap();
        let (host, notices) = host(dir.path());
        let prompt =
            "Write done.txt.\n<acceptance>\n- run: exit 0\n- exists: done.txt\n</acceptance>";
        let first = host.turn_ended(&[AgentMessage::user(prompt), assistant(StopReason::Stop)]);
        first.expect("the armed checks run").await.unwrap();
        std::fs::write(dir.path().join("done.txt"), "done").unwrap();
        host.turn_ended(&continuation())
            .expect("a failed round runs again")
            .await
            .unwrap();
        let sent = sent(&notices);
        let handles: Vec<&str> = sent.iter().map(|(_, _, handle)| handle.as_str()).collect();
        assert!(
            handles.iter().all(|handle| handle.starts_with("out:")),
            "{handles:?}"
        );
        assert_eq!(
            sent,
            vec![
                (
                    NoticeDelivery::Steer,
                    format!(
                        "[better-checks {} · 1 passed · 1 failed · round 1 of 3]\n✓ run: exit 0\n\
                         ✗ exists: done.txt\n  done.txt does not exist\n{RETRY_NOTE}",
                        handles[0]
                    ),
                    handles[0].to_string(),
                ),
                (
                    NoticeDelivery::Record,
                    format!(
                        "[better-checks {} · 2 passed · 0 failed · round 2 of 3]\n✓ run: exit 0\n\
                         ✓ exists: done.txt",
                        handles[1]
                    ),
                    handles[1].to_string(),
                ),
            ]
        );
        assert_eq!(
            host.evidence(handles[1]).map(|evidence| evidence.to_json()),
            Some(json!({
                "handle": handles[1],
                "round": 2,
                "passed": 2,
                "failed": 0,
                "checks": [
                    { "kind": "run", "check": "run: exit 0", "passed": true, "reason": null },
                    { "kind": "exists", "check": "exists: done.txt", "passed": true, "reason": null },
                ],
            }))
        );
        // Passed: the set is done, and later runs leave it alone.
        assert_eq!(host.check_status(), CheckStatus::Passed);
        assert!(host.turn_ended(&continuation()).is_none());
    }

    #[tokio::test]
    async fn the_checks_stop_after_the_last_round_and_a_plain_prompt_disarms_them() {
        let dir = tempfile::tempdir().unwrap();
        let (host, notices) = host(dir.path());
        let armed = [
            AgentMessage::user("Fix it.\n<acceptance>\nrun: exit 1\n</acceptance>"),
            assistant(StopReason::Stop),
        ];
        let mut started = Vec::new();
        for messages in [
            armed.to_vec(),
            continuation(),
            continuation(),
            continuation(),
        ] {
            let round = host.turn_ended(&messages);
            started.push(round.is_some());
            if let Some(round) = round {
                round.await.unwrap();
            }
        }
        assert_eq!(started, vec![true, true, true, false]);
        let last = sent(&notices).pop().unwrap();
        assert_eq!(
            (last.0, last.1.ends_with(LAST_ROUND_NOTE)),
            (NoticeDelivery::Steer, true)
        );
        assert_eq!(host.check_status(), CheckStatus::Failed);
        // An aborted run runs nothing; a prompt without a block disarms what one armed.
        assert!(host
            .turn_ended(&[armed[0].clone(), assistant(StopReason::Aborted)])
            .is_none());
        assert_eq!(host.check_status(), CheckStatus::Pending);
        assert!(host
            .turn_ended(&[AgentMessage::user("thanks"), assistant(StopReason::Stop)])
            .is_none());
        assert!(host.turn_ended(&continuation()).is_none());
    }

    #[test]
    fn a_session_file_reads_its_last_round_or_a_pending_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("child.jsonl");
        let status_after = |lines: &[Value]| {
            let text: Vec<String> = lines.iter().map(Value::to_string).collect();
            std::fs::write(&file, text.join("\n")).unwrap();
            check_status_from_session_file(&file)
        };
        let prompt = json!({ "type": "message", "message": {
            "role": "user", "content": [{ "type": "text", "text": "Do it.\n<acceptance>\nrun: true\n</acceptance>" }],
        }});
        let failed = json!({ "type": "message", "message": {
            "role": "custom", "customType": BETTER_CHECKS_CUSTOM_TYPE, "details": { "failed": 1 },
        }});
        let passed = json!({ "type": "custom_message", "customType": BETTER_CHECKS_CUSTOM_TYPE,
            "details": { "failed": 0 } });
        let plain =
            json!({ "type": "message", "message": { "role": "user", "content": "thanks" } });
        assert_eq!(
            [
                check_status_from_session_file(&dir.path().join("missing.jsonl")),
                status_after(&[plain.clone()]),
                status_after(&[prompt.clone()]),
                status_after(&[prompt.clone(), failed.clone()]),
                status_after(&[prompt.clone(), failed, passed.clone(), plain]),
                status_after(&[passed, prompt]),
            ],
            [
                CheckStatus::None,
                CheckStatus::None,
                CheckStatus::Pending,
                CheckStatus::Failed,
                CheckStatus::Passed,
                CheckStatus::Pending,
            ]
        );
    }

    /// The red-first guard of the fire-and-forget boundary: a check that never answers must not
    /// hold the run's settle, so the worker's `agent_end` (what every client repaints on) goes out
    /// while the round still runs.
    #[tokio::test]
    async fn a_hanging_check_never_delays_the_run_settle() {
        let dir = tempfile::tempdir().unwrap();
        let (host, notices) = host(dir.path());
        let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(
            super::super::tests::test_model(),
        ));
        provider.push_text_turn("done");
        let agent = pa_agent::agent::Agent::new(pa_agent::agent::AgentOptions {
            initial_state: pa_agent::agent::AgentInitialState {
                model: Some(super::super::tests::test_model()),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        });
        host.watch_turn_ends(&agent).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            agent.prompt("Do it.\n<acceptance>\nrun: hang\n</acceptance>"),
        )
        .await
        .expect("the run settles while its check hangs")
        .unwrap();
        assert_eq!(
            (host.check_status(), host.checks_status()["running"].clone()),
            (CheckStatus::Pending, json!(true))
        );
        assert!(notices.lock().unwrap().is_empty());
    }
}
