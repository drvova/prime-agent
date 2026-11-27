//! The headless turn driver: what runs around one agent turn of a session.
//!
//! - [`boundary`]: the turn boundary ([`TurnBoundary`]): the overflow
//!   compact-and-retry, the requested and threshold compaction arms, and the
//!   auto-refine reviews they schedule, before every admitted prompt and at
//!   every settled turn.
//! - [`goal`]: the goal continuation surface ([`PrintGoalSurface`]): goal
//!   updates, queued goal turns, budget steers and the backoff wake.
//! - [`autonomous`]: the autonomous run ([`HeadlessAutonomous`]): gate
//!   commands after every settled turn, its continuations and exit contract.
//! - [`continuation`]: the natural-turn-end hook that composes the goal and
//!   autonomous arms ([`wire_continuation_hook`]).
//! - [`auto_retry`]: a turn's auto-retry events in the json stream's shape.
//! - [`session_command`]: `/compact`, `/refine`, `/goal` and `/autonomous`
//!   run at the boundary ([`execute_prompt_session_command`]).
//!
//! Each surface emits its json events through an [`EventSink`]: the session's
//! stream for a runtime session, stdout for a boundary made with
//! `TurnBoundary::new` (the tests' own).

pub(crate) mod auto_retry;
pub(crate) mod autonomous;
pub(crate) mod boundary;
pub(crate) mod continuation;
pub(crate) mod goal;
pub(crate) mod session_command;
