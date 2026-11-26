//! The runtime daemon's single-instance lock on an agent dir: an
//! operating-system file lock on `<agent dir>/runtime-daemon.lock`, held for
//! the daemon process's life (released with the process however it ends).
//! The holder is recorded inside ([`RecordedHolder`]): its pid, the socket it
//! serves and its restart owner. A second runtime daemon on the agent dir is
//! refused, unless the holder is a wedged runtime daemon on the same socket,
//! which [`RuntimeDaemonLock::reap_holder`] stops first. The supervisor's
//! boot reap reads the record so it never takes the runtime daemon or its
//! restart owner, whose command lines name the same socket, for a
//! predecessor of its own.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::process_stop::StopOutcome;

/// The lock file in the agent dir.
const LOCK_FILE: &str = "runtime-daemon.lock";

/// The held lock; dropping it clears the record and releases the lock.
#[derive(Debug)]
pub struct RuntimeDaemonLock {
    file: File,
}

/// What the lock file says about the runtime daemon that holds it (a holder
/// that died without clearing it leaves a stale record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedHolder {
    pub pid: u32,
    /// The socket it serves (`daemon2.sock`).
    pub socket: PathBuf,
    /// Its restart owner, when it is kept.
    pub keeper: Option<u32>,
}

/// What [`RuntimeDaemonLock::reap_holder`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderReap {
    /// The holder could not be shown to be a runtime daemon on this agent
    /// dir and socket (no record, another socket, another program, a process
    /// that does not hold the lock file, an unverifiable identity, a
    /// platform with no way to tell): it was not signalled.
    NotOurs,
    /// The verified holder was stopped, with this outcome.
    Stopped { pid: u32, outcome: StopOutcome },
}

impl RuntimeDaemonLock {
    /// Takes the agent dir's runtime-daemon lock and records this process
    /// as its holder, serving `socket` and kept by `keeper`. `None` when
    /// another process holds it.
    ///
    /// # Errors
    ///
    /// The lock file cannot be created, locked or written.
    pub fn try_acquire(
        agent_dir: &Path,
        socket: &Path,
        keeper: Option<u32>,
    ) -> anyhow::Result<Option<Self>> {
        std::fs::create_dir_all(agent_dir)
            .with_context(|| format!("create the agent dir {}", agent_dir.display()))?;
        let path = agent_dir.join(LOCK_FILE);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        if let Err(error) = super::try_lock_exclusive(&file) {
            return match error.kind() {
                std::io::ErrorKind::WouldBlock => Ok(None),
                _ => Err(anyhow::Error::new(error).context(format!("lock {}", path.display()))),
            };
        }
        let holder = RecordedHolder {
            pid: std::process::id(),
            socket: socket.to_path_buf(),
            keeper,
        };
        file.set_len(0)?;
        writeln!(file, "{}", serde_json::to_string(&holder)?)?;
        Ok(Some(RuntimeDaemonLock { file }))
    }

    /// The holder the agent dir's lock file records.
    #[must_use]
    pub fn recorded(agent_dir: &Path) -> Option<RecordedHolder> {
        serde_json::from_str(&std::fs::read_to_string(agent_dir.join(LOCK_FILE)).ok()?).ok()
    }

    /// Stops the lock's holder when it is a runtime daemon serving `socket`
    /// on this agent dir; the caller has found it wedged (it holds the lock
    /// and does not answer on its socket). The proof, taken before any
    /// signal and pinned by the process's start id: the record names
    /// `socket`, the process runs the product binary, and it has this agent
    /// dir's lock file open. Linux only (`/proc`); elsewhere nothing is
    /// signalled. The stop is the boot reap's: SIGTERM, a grace, SIGKILL,
    /// each wait on the process's observable exit.
    pub async fn reap_holder(agent_dir: &Path, socket: &Path) -> HolderReap {
        let Some(holder) = Self::recorded(agent_dir) else {
            return HolderReap::NotOurs;
        };
        let canonical = |path: &Path| {
            pa_types::platform::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        };
        if holder.pid == std::process::id() || canonical(&holder.socket) != canonical(socket) {
            return HolderReap::NotOurs;
        }
        let target = super::process_stop::StopTarget {
            pid: holder.pid,
            start_id: crate::session::lease::get_process_start_id(holder.pid),
        };
        #[cfg(target_os = "linux")]
        let verified = {
            let lock_file = pa_types::platform::canonicalize(agent_dir.join(LOCK_FILE)).ok();
            let holds_lock_file = std::fs::read_dir(format!("/proc/{}/fd", holder.pid))
                .into_iter()
                .flatten()
                .flatten()
                .any(|fd| std::fs::read_link(fd.path()).ok() == lock_file);
            super::process_stop::exe_is_product_binary(holder.pid)
                && holds_lock_file
                && crate::session::lease::get_process_start_id(holder.pid) == target.start_id
        };
        #[cfg(not(target_os = "linux"))]
        let verified = false;
        if !verified {
            return HolderReap::NotOurs;
        }
        HolderReap::Stopped {
            pid: holder.pid,
            outcome: target.stop(super::process_stop::BOOT_REAP_BUDGETS).await,
        }
    }
}

impl Drop for RuntimeDaemonLock {
    fn drop(&mut self) {
        // The lock itself goes with the file handle.
        let _ = self.file.set_len(0);
    }
}
