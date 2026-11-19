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
