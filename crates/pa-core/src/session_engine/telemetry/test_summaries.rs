//! `agent test summaries` (v2): awaited test runs the kernel summarized
//! (counts only: never a test name, a path, a command, or output).

use super::{SessionCounters, SessionTelemetry, Value};
use crate::kernel::test_runs::TestRunFact;

/// Runs summarized, runs that failed, the failed tests they counted, the
/// failures given their own handle, and runs whose output got a handle.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct TestSummaryCounts {
    runs: u64,
    failing_runs: u64,
    failed_tests: u64,
    failure_handles: u64,
    stored_runs: u64,
}

impl SessionCounters {
    /// One awaited test run the kernel summarized.
    pub(crate) fn note_test_summary(&self, fact: TestRunFact) {
        self.with(|values| {
            let counts = &mut values.test_summaries;
            counts.runs += 1;
            counts.failing_runs += u64::from(fact.failed > 0);
            counts.failed_tests += fact.failed;
            counts.failure_handles += fact.failures as u64;
            counts.stored_runs += u64::from(fact.stored);
        });
    }
}

impl SessionTelemetry {
    /// Track `agent test summaries`, unless the session summarized no test run.
    pub(super) fn track_test_summaries(&self) {
        let mut counts = TestSummaryCounts::default();
        self.counters.with(|values| counts = values.test_summaries);
        if counts == TestSummaryCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            ("test_summary_count", counts.runs),
            ("test_summary_failing_count", counts.failing_runs),
            ("test_summary_failed_test_count", counts.failed_tests),
            ("test_summary_failure_handle_count", counts.failure_handles),
            ("test_summary_stored_count", counts.stored_runs),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent test summaries", properties);
    }
}
