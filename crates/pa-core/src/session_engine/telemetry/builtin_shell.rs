//! `agent builtin shell` (v2): commands the kernel ran on the built-in
//! shell, and where what they named ran (counts only: never a command, a
//! path, or output). The `bash` capability's rule file names the event;
//! session end emits it through the generic capability counter.

use super::SessionCounters;
use crate::kernel::shell_runs::ShellRun;

impl SessionCounters {
    /// One command on the built-in shell.
    pub(crate) fn note_shell_run(&self, run: ShellRun) {
        for (property, count) in [
            ("builtin_shell_command_count", 1),
            ("builtin_shell_in_process_count", run.in_process),
            ("builtin_shell_spawned_count", run.spawned),
            ("builtin_shell_handed_over_count", run.handed_over),
            ("builtin_shell_unavailable_count", run.unavailable),
        ] {
            self.note_capability(property, count);
        }
    }
}
