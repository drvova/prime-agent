//! `agent repl hypotheses` (v2): finished `rlm.hypotheses` runs, the approaches they tried,
//! those that passed their check, and the runs no approach won (counts only: never a task, an
//! approach, or a diff). The `hypothesis` capability's rule file names the event; session end
//! emits it through the generic capability counter.

use super::SessionCounters;
use crate::kernel::hypotheses::HypothesesRun;

impl SessionCounters {
    /// One finished `rlm.hypotheses` run.
    pub(crate) fn note_hypotheses(&self, run: HypothesesRun) {
        for (property, count) in [
            ("hypotheses_run_count", 1),
            ("hypotheses_approach_count", run.approaches),
            ("hypotheses_passed_count", run.passed),
            ("hypotheses_no_winner_count", u64::from(!run.winner)),
        ] {
            self.note_capability(property, count);
        }
    }
}
