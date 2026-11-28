//! Stopping one process by identity: the escalation ladder every reap of a
//! daemon's predecessor rides (the old daemon's boot reap and terminal stop,
//! the runtime daemon's lock-holder reap).
//!
//! A target is a pid and the process start id it had when it was found. The
//! ladder: gone check, SIGTERM, a grace, SIGKILL, a verify window, each wait
//! polling for the process's observable exit. The signals ride the
//! kernel-held pidfd, which pins the process, where the platform has one; on
//! unix without one they ride kill(2), with the identity check re-run
//! immediately before each signal, narrowing pid reuse to the check-to-kill
//! gap. An unverifiable identity is never signalled.

use std::time::Duration;

use super::process::Signal;

/// How often a wait checks whether the target is gone.
const POLL: Duration = Duration::from_millis(25);

/// One stop's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The process was gone before any signal.
    AlreadyGone,
    /// The process exited inside the SIGTERM grace.
    Term,
    /// The process died to SIGKILL.
    Kill,
    /// The process outlived SIGKILL (a D-state wedged task), or lives behind
    /// a handle that cannot be obtained.
    Survived,
}

/// The process to stop: its pid and the start id it had when it was found.
/// `None` as the start id is an identity that cannot be verified, and such a
/// process is never signalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopTarget {
    pub pid: u32,
    pub start_id: Option<String>,
}

/// How long each rung of the ladder waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopBudgets {
    /// After SIGTERM, before SIGKILL.
    pub term_grace: Duration,
    /// After SIGKILL, before the stop reports a survivor.
    pub kill_verify: Duration,
}

/// The budgets a daemon's boot reap stops a predecessor on: the CLI stop
/// contract's grace after SIGTERM, then a short verify after SIGKILL.
pub const BOOT_REAP_BUDGETS: StopBudgets = StopBudgets {
    term_grace: Duration::from_secs(2),
    kill_verify: Duration::from_secs(1),
};

impl StopTarget {
    /// Whether the current identity snapshot agrees with the found process.
    /// This preflight check cannot itself bind a later numeric signal to an
    /// instance.
    fn identity_current(&self) -> bool {
        match &self.start_id {
            Some(expected) => {
                crate::session::lease::get_process_start_id(self.pid).as_deref()
                    == Some(expected.as_str())
            }
            None => false,
        }
    }

    /// Whether the found process is gone: its identity changed or it no
    /// longer runs (a zombie does not).
    fn gone(&self) -> bool {
        !self.identity_current()
            || !crate::session::lease::is_process_alive(self.pid).unwrap_or(false)
    }

    /// Stop the process within `budgets`.
    pub async fn stop(&self, budgets: StopBudgets) -> StopOutcome {
        match super::process::open_pidfd(self.pid) {
            // The fd opens before the identity check and is held across it
            // and every signal, so only the pinned process can receive one.
            Ok(pidfd) => {
                let pidfd = PidfdGuard(pidfd);
                self.escalate(
                    |signal| super::process::pidfd_signal(pidfd.0, signal),
                    budgets,
                )
                .await
            }
            Err(error) if cfg!(unix) && error.kind() == std::io::ErrorKind::Unsupported => {
                self.escalate(
                    |signal| {
                        if self.identity_current() {
                            let _ = super::process::kill_pid(self.pid as i32, signal);
                        }
                        true
                    },
                    budgets,
                )
                .await
            }
            // Every other unobtainable handle keeps the conservative default:
            // it never signals. A live process behind it is not gone.
            Err(_) => {
                if self.identity_current()
                    && crate::session::lease::is_process_alive(self.pid).unwrap_or(false)
                {
                    StopOutcome::Survived
                } else {
                    StopOutcome::AlreadyGone
                }
            }
        }
    }

    /// The ladder both signal carriers ride.
    async fn escalate(&self, signal: impl Fn(Signal) -> bool, budgets: StopBudgets) -> StopOutcome {
        if self.gone() {
            return StopOutcome::AlreadyGone;
        }
        if signal(Signal::Term) {
            if self.await_gone(budgets.term_grace).await {
                return StopOutcome::Term;
            }
            if signal(Signal::Kill) && self.await_gone(budgets.kill_verify).await {
                return StopOutcome::Kill;
            }
        }
        StopOutcome::Survived
    }

    /// Poll until the process is gone or the budget runs out.
    async fn await_gone(&self, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if self.gone() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

struct PidfdGuard(i32);

impl Drop for PidfdGuard {
    fn drop(&mut self) {
        super::process::close_pidfd(self.0);
    }
}

/// The product's own binary names: `prime-agent` (release/install) and
/// `pa-daemon` (the workspace binary).
#[must_use]
pub fn is_product_binary(exe: &str) -> bool {
    matches!(
        std::path::Path::new(exe)
            .file_name()
            .and_then(|name| name.to_str()),
        Some("prime-agent" | "pa-daemon")
    )
}

/// The unforgeable executable check for one pid: `/proc/<pid>/exe` names
/// the binary the kernel loaded (argv[0] is forgeable with `exec -a`).
#[cfg(target_os = "linux")]
#[must_use]
pub fn exe_is_product_binary(pid: u32) -> bool {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        // The kernel appends " (deleted)" to a replaced binary's exe link (in-place upgrade).
        .is_ok_and(|exe| {
            let name = exe.to_string_lossy();
            is_product_binary(name.trim_end_matches(" (deleted)"))
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn target(pid: u32) -> StopTarget {
        StopTarget {
            pid,
            start_id: crate::session::lease::get_process_start_id(pid),
        }
    }

    /// Kills and reaps the child on any exit path (a failed assertion in
    /// between would otherwise leak the child into the test machine: the
    /// std child kills nothing on drop).
    struct ReapOnDrop(Option<std::process::Child>);

    impl Drop for ReapOnDrop {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_real_process_stops_inside_the_term_grace() {
        let mut child = pa_types::session_scope::std_command("sleep")
            .arg("300")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(
            super::super::process::open_pidfd(pid).is_ok(),
            "the kernel-held handle opens"
        );
        let outcome = target(pid).stop(BOOT_REAP_BUDGETS).await;
        let _ = child.wait();
        assert_eq!(outcome, StopOutcome::Term, "sleep must exit on SIGTERM");
    }

    /// The `bash` records SIGTERM without exiting or spawning a child (a leaked
    /// grandchild would outlive the guard's kill); the trap marker is the readiness
    /// barrier and the TERM record the escalation must leave before its KILL.
    #[tokio::test]
    async fn a_term_ignoring_process_dies_to_the_stop_escalation() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let trap_armed = dir.path().join("trap-armed");
        let term_delivered = dir.path().join("term-delivered");
        let child = pa_types::session_scope::std_command("bash")
            .arg("-c")
            .arg("trap ': > \"$2\"' TERM; : > \"$1\"; while :; do :; done")
            .arg("bash")
            .arg(&trap_armed)
            .arg(&term_delivered)
            .spawn()
            .expect("spawn term-ignoring bash");
        let mut guard = ReapOnDrop(Some(child));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !trap_armed.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(trap_armed.exists(), "the trap never armed");
        let pid = guard.0.as_ref().expect("guard holds the child").id();
        let outcome = target(pid)
            .stop(StopBudgets {
                kill_verify: Duration::from_secs(10),
                ..BOOT_REAP_BUDGETS
            })
            .await;
        assert_eq!(
            outcome,
            StopOutcome::Kill,
            "the stop escalation must SIGKILL a term-ignoring process"
        );
        assert!(
            term_delivered.exists(),
            "the escalation must deliver its TERM before the KILL"
        );
        let mut child = guard.0.take().expect("guard holds the child");
        let status = child.wait().expect("reap the escalated child");
        assert_eq!(
            super::super::process::termination_signal(&status),
            Some(9),
            "the child's death must be the escalation's SIGKILL"
        );
    }

    #[tokio::test]
    async fn a_vanished_pid_reports_already_gone() {
        let mut child = pa_types::session_scope::std_command("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let _ = child.wait();
        assert_eq!(
            target(pid).stop(BOOT_REAP_BUDGETS).await,
            StopOutcome::AlreadyGone
        );
    }

    #[tokio::test]
    async fn a_recycled_pid_is_never_signaled() {
        let mut child = pa_types::session_scope::std_command("sleep")
            .arg("300")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let mut stale = target(pid);
        stale.start_id = stale.start_id.map(|id| id + "recycled");
        assert_eq!(
            stale.stop(BOOT_REAP_BUDGETS).await,
            StopOutcome::AlreadyGone
        );
        assert!(
            child.try_wait().expect("child alive").is_none(),
            "the recycled identity must not have been signaled"
        );
        let mut unobservable = target(pid);
        unobservable.start_id = None;
        assert_eq!(
            unobservable.stop(BOOT_REAP_BUDGETS).await,
            StopOutcome::AlreadyGone,
            "an unverifiable identity is never signaled"
        );
        assert!(
            child.try_wait().expect("child alive").is_none(),
            "the unverifiable identity must not have been signaled"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_exe_gate_reads_the_kernel_binary() {
        let mut child = pa_types::session_scope::std_command("sleep")
            .arg("2")
            .spawn()
            .expect("spawn sleep");
        assert!(
            !exe_is_product_binary(child.id()),
            "a foreign executable never passes the unforgeable gate"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn the_exe_gate_accepts_replaced_binaries() {
        let replaced = "/opt/prime-agent/bin/prime-agent (deleted)";
        assert!(
            is_product_binary(replaced.trim_end_matches(" (deleted)")),
            "the deleted-suffix spelling still identifies the product binary"
        );
    }
}
