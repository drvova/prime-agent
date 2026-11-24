//! Agent-engine turn execution: the turn state machine — the model-turn
//! runner, the turn boundary, the turn loop, the once-runner with its
//! same-provider retry, and the session-agent constructor.
use super::{
    aborted_message, drop_trailing_assistant, json, json_round_trip, retry_event_to_engine_event,
    AbortController, AgentSessionEngine, AutoCompactionRun, BoundaryRun, EngineEvent, GoalBoundary,
    Model, OverflowArmRun, StopReason, TurnAdmission, TurnOnce, TurnPrompt, TurnResult, Value,
};

mod boundary;
mod model;
mod run_loop;
mod run_once;

/// How one [`AgentSessionEngine::run_turn_once`] starts its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunStart {
    /// Admit the turn's prompt.
    Prompt,
    /// Continue the loop context as it stands (a retry, after the failed
    /// reply left it, or the tool run after an artificial yield).
    Continue,
    /// The final reply was cut off at the output limit: it stays, and the
    /// run continues behind the guiding row
    /// (`AgentSession::continue_after_output_limit`).
    ContinueAfterOutputLimit,
}
