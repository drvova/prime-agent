//! `agent test summaries` (v2): awaited test runs the kernel summarized
//! (counts only: never a test name, a path, a command, or output). The `bash` capability's rule
//! file names the event; session end emits it through the generic capability counter.

use super::SessionCounters;
use crate::kernel::test_runs::TestRunFact;

impl SessionCounters {
    /// One awaited test run the kernel summarized.
    pub(crate) fn note_test_summary(&self, fact: TestRunFact) {
        for (property, count) in [
            ("test_summary_count", 1),
            ("test_summary_failing_count", u64::from(fact.failed > 0)),
            ("test_summary_failed_test_count", fact.failed),
            ("test_summary_failure_handle_count", fact.failures as u64),
            ("test_summary_stored_count", u64::from(fact.stored)),
        ] {
            self.note_capability(property, count);
        }
    }
}
