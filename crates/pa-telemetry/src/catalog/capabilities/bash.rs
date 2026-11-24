//! The `bash` capability owns five events: the read-only command cache, the typed summaries
//! of awaited test runs, the built-in shell, elevated commands, and the `[bash-quiet]` notices.

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

/// The built-in shell (v2), emitted at session end beside `agent session ended` (which sits
/// near the backend's 128-key cap) only when a `bash()` command ran on it: commands run, the
/// utilities they ran inside the process, the programs they started as processes, the utilities
/// that handed their command to the program of the same name (an option the built-in does not
/// implement), and the ones that could do neither because the machine has no such program.
/// Counts only: never a command, a path, or output.
pub(in crate::catalog) const AGENT_BUILTIN_SHELL: EventRule = EventRule {
    name: "agent builtin shell",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("builtin_shell_command_count", required(count())),
        ("builtin_shell_in_process_count", required(count())),
        ("builtin_shell_spawned_count", required(count())),
        ("builtin_shell_handed_over_count", required(count())),
        ("builtin_shell_unavailable_count", required(count())),
    ],
};

/// Elevated commands (v2), emitted at session end beside `agent session ended` (which sits near
/// the backend's 128-key cap) only when the model asked for a `bash()` command with
/// administrator or root rights (`'elevated #t`): commands asked for, by how their consent step
/// ended (the user approved or no consent was needed, the user declined, or no consent prompt
/// could be shown) and by the route that asked (UAC, `sudo` with a graphical askpass, `pkexec`,
/// `sudo -n`, or a session already elevated). Counts only: never a command, output, or anything
/// the user typed.
pub(in crate::catalog) const AGENT_ELEVATED_COMMANDS: EventRule = EventRule {
    name: "agent elevated commands",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("elevated_command_count", required(count())),
        ("elevated_approved_count", required(count())),
        ("elevated_declined_count", required(count())),
        ("elevated_unavailable_count", required(count())),
        ("elevated_uac_count", required(count())),
        ("elevated_sudo_askpass_count", required(count())),
        ("elevated_pkexec_count", required(count())),
        ("elevated_sudo_noninteractive_count", required(count())),
        ("elevated_already_elevated_count", required(count())),
    ],
};

/// The `[bash-quiet]` notices (v2), emitted at session end beside `agent session ended` (which
/// sits near the backend's 128-key cap) only when the kernel reported background commands that
/// had all written nothing for a quiet period: notices that woke the idle session, notices dropped
/// because a turn was running, the commands the notices named, and the notices by back-off step
/// (the first, second and third quiet period of one silence). Counts only: never a command, a
/// path, output, or a duration.
pub(in crate::catalog) const AGENT_BASH_QUIET: EventRule = EventRule {
    name: "agent bash quiet",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("bash_quiet_delivered_count", required(count())),
        ("bash_quiet_dropped_busy_count", required(count())),
        ("bash_quiet_job_count", required(count())),
        ("bash_quiet_step_1_count", required(count())),
        ("bash_quiet_step_2_count", required(count())),
        ("bash_quiet_step_3_count", required(count())),
    ],
};
