//! Goal completion verification (`goal.create(..., check=..., verify="judge")`).
//!
//! Policy: verification runs when the model claims completion (`goal.complete()`), not at every
//! turn end. A claim is the only moment the result changes anything, and a turn-end check would
//! rerun a test suite after every working turn. The check runs in the kernel's host request, off
//! the paint path and without the goal driver's lock, so turn-end accounting never waits on it;
//! a failed claim answers the calling cell with the gap at once, and the same gap rides the next
//! goal continuation.
//!
//! The check reuses the autonomous gate lane: the same shell runner, timeout, and output caps,
//! and the same dedupe (a failed check is not rerun while the git workspace snapshot is
//! unchanged). The judge ([`super::goal_judge`]) runs only after the check passes.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::autonomous::{
    run_gate_lane, AgentAutonomousGateFailure, AutonomousGateResult, GateCommandRunner,
    GateLaneMemory, GateLanePolicy, GateOutputCap, GitWorktreeSnapshot, DEFAULT_GATE_TIMEOUT_MS,
    GATE_NOT_RERUN_EXIT_TEXT,
};
use crate::goals::{
    GoalState, GoalStatus, GoalVerificationReport, GoalVerificationStage, GoalVerifier,
};

use super::goal_driver::GoalTelemetrySlot;
use super::goal_judge::{GoalJudge, JudgeVerdict};
use super::telemetry::GoalTelemetryFact;

/// Timeout of one `check` run (the autonomous gate default).
pub const GOAL_CHECK_TIMEOUT_MS: u64 = DEFAULT_GATE_TIMEOUT_MS;
/// Output tail a failed check contributes to the gap: the summary end, never the full log.
const GOAL_CHECK_GAP_TAIL_CHARS: usize = 1500;
const GOAL_CHECK_UNCHANGED_NOTE: &str = "The check was not rerun because the workspace has not changed since it failed. Change files before claiming completion again.";

/// One `goal.complete()` claim on a goal that has a verifier, read under the driver lock and
/// verified without it.
#[derive(Debug, Clone, PartialEq)]
pub struct GoalClaim {
    pub goal_id: String,
    pub objective: String,
    pub check: Option<String>,
    pub verify: Option<GoalVerifier>,
}

impl GoalClaim {
    /// The claim a completion of `goal` must pass; `None` when the goal has no verifier (the
    /// plain `goal.complete()` path) or no live goal to complete.
    #[must_use]
    pub fn of(goal: &GoalState) -> Option<Self> {
        match goal.status {
            GoalStatus::Active | GoalStatus::Paused | GoalStatus::BudgetLimited => {}
            GoalStatus::Idle | GoalStatus::Complete | GoalStatus::Error => return None,
        }
        if goal.check.is_none() && goal.verify.is_none() {
            return None;
        }
        Some(Self {
            goal_id: goal.goal_id.clone()?,
            objective: goal.objective.clone()?,
            check: goal.check.clone(),
            verify: goal.verify,
        })
    }
}

/// The check lane's failure memory for one goal.
#[derive(Default)]
struct CheckMemory {
    goal_id: Option<String>,
    attempts: HashMap<String, u64>,
    last_failure: Option<AgentAutonomousGateFailure>,
    last_failure_snapshot: Option<GitWorktreeSnapshot>,
}

/// Verifies completion claims for one session: the check lane plus the judge the engine binds
/// once its agent exists.
pub struct GoalClaimVerifier {
    runner: Arc<dyn GateCommandRunner>,
    memory: tokio::sync::Mutex<CheckMemory>,
    judge: OnceLock<GoalJudge>,
    telemetry: GoalTelemetrySlot,
}

impl GoalClaimVerifier {
    #[must_use]
    pub fn new(runner: Arc<dyn GateCommandRunner>, telemetry: GoalTelemetrySlot) -> Self {
        Self {
            runner,
            memory: tokio::sync::Mutex::default(),
            judge: OnceLock::new(),
            telemetry,
        }
    }

    /// Bind the session's judge (the engine, once its agent exists). A second bind is ignored.
    pub fn bind_judge(&self, judge: GoalJudge) {
        let _ = self.judge.set(judge);
    }

    /// Whether `verify="judge"` can run in this session.
    #[must_use]
    pub fn has_judge(&self) -> bool {
        self.judge.get().is_some()
    }

    fn note(&self, fact: GoalTelemetryFact) {
        if let Some(counters) = self.telemetry.get() {
            counters.note_goal(fact);
        }
    }

    /// Verify one claim: the check (when set) must exit 0, then the judge (when set) must answer
    /// MET with cited evidence. The first failing stage decides, and its gap is reported.
    #[tracing::instrument(name = "goal_verify_claim", skip_all)]
    pub async fn verify(&self, claim: &GoalClaim) -> GoalVerificationReport {
        if let Some(command) = &claim.check {
            let mut memory = self.memory.lock().await;
            if memory.goal_id.as_deref() != Some(claim.goal_id.as_str()) {
                *memory = CheckMemory {
                    goal_id: Some(claim.goal_id.clone()),
                    ..CheckMemory::default()
                };
            }
            let commands = [command.clone()];
            let CheckMemory {
                attempts,
                last_failure,
                last_failure_snapshot,
                ..
            } = &mut *memory;
            let result = run_gate_lane(
                &GateLanePolicy {
                    commands: &commands,
                    // A goal's check never exhausts: the goal budgets bound the attempts.
                    max_retries: u64::MAX,
                    timeout_ms: GOAL_CHECK_TIMEOUT_MS,
                    output_cap: GateOutputCap::Tail(GOAL_CHECK_GAP_TAIL_CHARS),
                    unchanged_note: GOAL_CHECK_UNCHANGED_NOTE,
                },
                GateLaneMemory {
                    attempts,
                    last_failure,
                    last_failure_snapshot,
                },
                self.runner.as_ref(),
            )
            .await;
            match result {
                AutonomousGateResult::Passed => self.note(GoalTelemetryFact::CheckRun),
                AutonomousGateResult::Failed | AutonomousGateResult::RetryExhausted => {
                    let failure = memory.last_failure.clone();
                    let gap = match failure {
                        Some(failure) if failure.exit_text == GATE_NOT_RERUN_EXIT_TEXT => {
                            self.note(GoalTelemetryFact::CheckSkipped);
                            format!(
                                "check `{}` {}. {}",
                                failure.command, failure.exit_text, failure.output
                            )
                        }
                        Some(failure) => {
                            self.note(GoalTelemetryFact::CheckRun);
                            self.note(GoalTelemetryFact::CheckFailed);
                            let output = if failure.output.is_empty() {
                                "(no output)".to_string()
                            } else {
                                failure.output
                            };
                            format!(
                                "check `{}` {}; output tail:\n{output}",
                                failure.command, failure.exit_text
                            )
                        }
                        None => format!("check `{command}` failed"),
                    };
                    return GoalVerificationReport {
                        passed: false,
                        stage: GoalVerificationStage::Check,
                        gap: Some(gap),
                    };
                }
            }
        }
        match claim.verify {
            Some(GoalVerifier::Judge) => {
                let Some(judge) = self.judge.get() else {
                    return GoalVerificationReport {
                        passed: false,
                        stage: GoalVerificationStage::Judge,
                        gap: Some("the goal judge is unavailable in this session".to_string()),
                    };
                };
                self.note(GoalTelemetryFact::JudgeCall);
                match judge.judge(&claim.objective).await {
                    JudgeVerdict::Met => GoalVerificationReport {
                        passed: true,
                        stage: GoalVerificationStage::Judge,
                        gap: None,
                    },
                    JudgeVerdict::NotMet { gap } => {
                        self.note(GoalTelemetryFact::JudgeNotMet);
                        GoalVerificationReport {
                            passed: false,
                            stage: GoalVerificationStage::Judge,
                            gap: Some(format!("judge: NOT MET: {gap}")),
                        }
                    }
                }
            }
            None => GoalVerificationReport {
                passed: true,
                stage: GoalVerificationStage::Check,
                gap: None,
            },
        }
    }
}

#[cfg(test)]
mod tests;
