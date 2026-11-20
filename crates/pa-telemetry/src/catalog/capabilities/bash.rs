//! The `bash` capability: the typed summaries of awaited test runs.

use super::super::{count, required, uuid, EventRule};

/// Typed test-run summaries (v2), emitted at session end beside `agent session ended` (which sits
/// near the backend's 128-key cap) only when the kernel summarized an awaited test run (cargo test,
/// pytest, unittest, go test, jest, vitest): runs summarized, runs with failures, the failed tests
/// they counted, the failures that got their own output handle, and runs whose whole output got a
/// handle. Counts only: never a test name, a path, a command, or output.
pub(in crate::catalog) const AGENT_TEST_SUMMARIES: EventRule = EventRule {
    name: "agent test summaries",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("test_summary_count", required(count())),
        ("test_summary_failing_count", required(count())),
        ("test_summary_failed_test_count", required(count())),
        ("test_summary_failure_handle_count", required(count())),
        ("test_summary_stored_count", required(count())),
    ],
};
