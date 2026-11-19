//! Agent-engine turn execution: the turn state machine — the model-turn
//! runner, the turn boundary, the turn loop, the once-runner with its
//! same-provider retry and quota-park machinery, and the session-agent
//! constructor.
use super::{
    aborted_message, drop_trailing_assistant, json, json_round_trip, retry_event_to_engine_event,
    AbortController, AgentSessionEngine, AutoCompactionRun, BoundaryRun, EngineEvent, GoalBoundary,
    Model, OverflowArmRun, QuotaParkState, StopReason, TurnAdmission, TurnOnce, TurnPrompt,
    TurnResult, Value, QUOTA_WAKE_MAX_RETRIES, QUOTA_WAKE_RETRY_DELAY_MS,
};

mod boundary;
mod model;
mod quota;
mod run_loop;
mod run_once;
