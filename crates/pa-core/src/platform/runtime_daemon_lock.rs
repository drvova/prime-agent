//! The runtime daemon's single-instance lock on an agent dir: an
//! operating-system file lock on `<agent dir>/runtime-daemon.lock`, held for
//! the daemon process's life (released with the process however it ends),
//! with the holder's pid written inside. A second runtime daemon on the agent
//! dir is refused; the supervisor's boot reap reads the pid so it never takes
//! the runtime daemon, whose command line names the same socket, for a
//! predecessor of its own.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;

/// The lock file in the agent dir.
const LOCK_FILE: &str = "runtime-daemon.lock";

/// The held lock; dropping it clears the pid and releases the lock.
#[derive(Debug)]
pub struct RuntimeDaemonLock {
    file: File,
}

impl RuntimeDaemonLock {
    /// Takes the agent dir's runtime-daemon lock and writes this process's id
    /// into it.
    ///
    /// # Errors
    ///
    /// Another process holds it (a runtime daemon runs on this agent dir: the
    /// error names its pid), or the lock file cannot be created or locked.
    pub fn acquire(agent_dir: &Path) -> anyhow::Result<Self> {
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
            return Err(match error.kind() {
                std::io::ErrorKind::WouldBlock => anyhow::anyhow!(
                    "a runtime daemon is already running on {} (pid {})",
                    agent_dir.display(),
                    Self::recorded_pid(agent_dir)
                        .map_or_else(|| "unknown".to_string(), |pid| pid.to_string())
                ),
                _ => anyhow::Error::new(error).context(format!("lock {}", path.display())),
            });
        }
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(RuntimeDaemonLock { file })
    }

    /// The pid the agent dir's runtime daemon recorded, while one runs
    /// (a holder that died without clearing it may leave a stale one).
    #[must_use]
    pub fn recorded_pid(agent_dir: &Path) -> Option<u32> {
        std::fs::read_to_string(agent_dir.join(LOCK_FILE))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

impl Drop for RuntimeDaemonLock {
    fn drop(&mut self) {
        // The lock itself goes with the file handle.
        let _ = self.file.set_len(0);
    }
}
