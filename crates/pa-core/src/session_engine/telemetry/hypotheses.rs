//! `agent repl hypotheses` (v2): finished `rlm.hypotheses` runs, the approaches they tried,
//! those that passed their check, and the runs no approach won (counts only: never a task, an
//! approach, or a diff).

use super::{SessionCounters, SessionTelemetry, Value};
use crate::kernel::hypotheses::HypothesesRun;

/// The session's `rlm.hypotheses` runs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct HypothesesCounts {
    runs: u64,
    approaches: u64,
    passed: u64,
    no_winner: u64,
}

impl SessionCounters {
    /// One finished `rlm.hypotheses` run.
    pub(crate) fn note_hypotheses(&self, run: HypothesesRun) {
        self.with(|values| {
            let counts = &mut values.hypotheses;
            counts.runs += 1;
            counts.approaches += run.approaches;
            counts.passed += run.passed;
            counts.no_winner += u64::from(!run.winner);
        });
    }
}

impl SessionTelemetry {
    /// Track `agent repl hypotheses`, unless no run finished.
    pub(super) fn track_hypotheses(&self) {
        let mut counts = HypothesesCounts::default();
        self.counters.with(|values| counts = values.hypotheses);
        if counts == HypothesesCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            ("hypotheses_run_count", counts.runs),
            ("hypotheses_approach_count", counts.approaches),
            ("hypotheses_passed_count", counts.passed),
            ("hypotheses_no_winner_count", counts.no_winner),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent repl hypotheses", properties);
    }
}
