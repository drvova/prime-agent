//! Stop when compaction keeps refilling the context (the idea of `ZCode`'s
//! rapid-refill guard, reimplemented).
//!
//! A compaction keeps the recent tail verbatim, the user's own messages, and
//! the files the work read last. When those alone are at or over the
//! threshold (one huge reply or tool result in the tail, or settings whose
//! kept budgets do not fit the window), the next request is due again at
//! once, and an automatic compaction after every request summarizes almost
//! nothing while the turn makes no progress. Two readings catch it, each
//! only the second time in a row ([`STOPS_AT_IN_A_ROW`], [`RefillGuard`]):
//!
//! - right after a compaction commits, the live context is still at or over
//!   its threshold by estimate ([`ContextReading::over_threshold`]);
//! - a compaction comes due within [`RAPID_REFILL_REQUESTS`] model requests
//!   of the one before it.
//!
//! Either stops automatic compaction for the run, and the turn ends with one
//! `compaction_outcome` row (outcome `unreducible`) that says what to do.

use pa_types::session::AgentMessage;

use super::{compaction, provider_adapter, AgentSession, SessionAgentMessage};

/// A compaction due again within this many model requests of the previous
/// one refilled rapidly. A compaction frees at least the room between the
/// kept budgets and the threshold (the trigger's retention floor keeps the
/// threshold above them), so refilling it in three requests means each one
/// added a third of that room or more: the same large content coming back,
/// not ordinary work. `ZCode` uses the same three tool turns.
pub const RAPID_REFILL_REQUESTS: u64 = 3;

/// Times in a row a reading must recur before automatic compaction stops.
/// Once can be a single large message that was newest at the cut: the next
/// compaction, after newer messages moved the cut past it, summarizes it
/// away. The second time in a row means each compaction is followed by the
/// same overflow or refill.
pub const STOPS_AT_IN_A_ROW: u32 = 2;

/// Why automatic compaction stopped for the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefillStop {
    /// The context was still at or over its threshold right after a
    /// compaction, [`STOPS_AT_IN_A_ROW`] compactions in a row.
    StillOverThreshold,
    /// Compaction came due again within [`RAPID_REFILL_REQUESTS`] requests
    /// of the previous one, [`STOPS_AT_IN_A_ROW`] times in a row.
    RapidRefill,
}

impl RefillStop {
    /// The row's text: what happened, and what the user can do about it.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::StillOverThreshold => "Auto-compaction stopped: the context was still over its compaction threshold right after compacting, twice in a row, so compacting again cannot reduce it. What a compaction keeps (the recent messages, your own messages and the files read again) does not fit. Start a new session with /new, or lower `compaction.keepRecentTokens` in settings, then continue.".to_string(),
            Self::RapidRefill => format!("Auto-compaction stopped: compaction came due again within {RAPID_REFILL_REQUESTS} model requests of the previous one {STOPS_AT_IN_A_ROW} times in a row, so the context refills faster than compaction can reduce it. Start a new session with /new, or ask for smaller reads and command output, then continue."),
        }
    }
}

/// One run's counts of each reading in a row; a run starts with a fresh one.
#[derive(Debug, Default)]
pub struct RefillGuard {
    rapid_refills: u32,
    over_threshold_compactions: u32,
}

impl RefillGuard {
    /// Called when an automatic compaction is due, with the model requests
    /// since the newest compaction (`None` when the context has none):
    /// `Some` when this compaction would be the [`STOPS_AT_IN_A_ROW`]th
    /// rapid refill in a row and must not run.
    pub fn before_compaction(
        &mut self,
        requests_since_compaction: Option<u64>,
    ) -> Option<RefillStop> {
        match requests_since_compaction {
            Some(requests) if requests <= RAPID_REFILL_REQUESTS => {
                self.rapid_refills += 1;
                (self.rapid_refills >= STOPS_AT_IN_A_ROW).then_some(RefillStop::RapidRefill)
            }
            Some(_) | None => {
                self.rapid_refills = 0;
                None
            }
        }
    }

    /// Called after an automatic compaction committed, with whether the
    /// context is still at or over its threshold: `Some` when it is, the
    /// [`STOPS_AT_IN_A_ROW`]th compaction in a row.
    pub fn after_compaction(&mut self, over_threshold: bool) -> Option<RefillStop> {
        if !over_threshold {
            self.over_threshold_compactions = 0;
            return None;
        }
        self.over_threshold_compactions += 1;
        (self.over_threshold_compactions >= STOPS_AT_IN_A_ROW)
            .then_some(RefillStop::StillOverThreshold)
    }
}

/// What the refill guard reads off the live context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextReading {
    /// Assistant messages after the newest compaction summary (the kept
    /// tail's are older than it); `None` without one.
    pub requests_since_compaction: Option<u64>,
    /// The live messages' chars/4 estimate is at or over the threshold. No
    /// usage reading counts (right after a compaction every one predates
    /// it), and the system prompt and tools are left out, so the estimate
    /// errs low: a stop is never a near miss.
    pub over_threshold: bool,
}

impl AgentSession {
    /// The refill guard's reading of the live context against `model` (the
    /// model that serves the turn, as [`Self::auto_compaction_due`] reads it).
    pub async fn context_reading(&self, model: &pa_types::ai::Model) -> ContextReading {
        let state = self.agent.state().await;
        let messages: Vec<SessionAgentMessage> = state
            .messages
            .iter()
            .filter_map(|message| serde_json::to_value(message).ok())
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect();
        let settings = self.compaction_settings();
        let summary_at = messages.iter().rev().find_map(|message| match message {
            AgentMessage::CompactionSummary(summary) => Some(summary.timestamp),
            _ => None,
        });
        let requests_since_compaction = summary_at.map(|at| {
            messages
                .iter()
                .filter(|message| matches!(message, AgentMessage::Assistant(assistant) if assistant.timestamp > at))
                .count() as u64
        });
        let threshold = compaction::compaction_threshold(
            model.context_window,
            compaction::request_output_budget(
                model,
                provider_adapter::model_thinking_level(state.thinking_level),
            ),
            &settings,
        );
        let estimate: u64 = messages.iter().map(compaction::estimate_tokens).sum();
        ContextReading {
            requests_since_compaction,
            over_threshold: settings.enabled && threshold > 0 && estimate >= threshold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compaction due within three requests of the previous one is a
    /// rapid refill; the second in a row stops, a slower cycle resets.
    #[test]
    fn the_second_rapid_refill_in_a_row_stops_compaction() {
        let mut guard = RefillGuard::default();
        let verdicts: Vec<_> = [None, Some(2), Some(9), Some(3), Some(1)]
            .into_iter()
            .map(|requests| guard.before_compaction(requests))
            .collect();
        assert_eq!(
            verdicts,
            [None, None, None, None, Some(RefillStop::RapidRefill)]
        );
    }

    /// One compaction that leaves the context over its threshold can
    /// recover; the second in a row stops, one that gets under resets.
    #[test]
    fn the_second_compaction_in_a_row_left_over_the_threshold_stops() {
        let mut guard = RefillGuard::default();
        let verdicts: Vec<_> = [true, false, true, true]
            .into_iter()
            .map(|over| guard.after_compaction(over))
            .collect();
        assert_eq!(
            verdicts,
            [None, None, None, Some(RefillStop::StillOverThreshold)]
        );
    }
}
