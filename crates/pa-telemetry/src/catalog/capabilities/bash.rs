//! The `bash` capability owns two events: the read-only command cache and the typed summaries
//! of awaited test runs.

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

/// The kernel's read-only command cache (v2), emitted at session end beside `agent session ended`
/// (which sits near the backend's 128-key cap) only when a lookup ran: allowlisted read-only
/// `bash()` commands (`git status`/`log`/`diff`, `ls`, `rg`/`grep`, `cat`) answered from the cache
/// or run, and `fs.read` calls answered from the cache or read. Counts only: never a command, a
/// path, or output.
pub(in crate::catalog) const AGENT_READONLY_CACHE: EventRule = EventRule {
    name: "agent readonly cache",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("readonly_cache_hit_count", required(count())),
        ("readonly_cache_miss_count", required(count())),
        ("readonly_cache_fs_read_hit_count", required(count())),
        ("readonly_cache_fs_read_miss_count", required(count())),
    ],
};
