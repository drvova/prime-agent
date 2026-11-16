//! The shadow store's git runner: every command runs against the store's own
//! git directory and session index, never the workspace's repository, with the
//! user's global/system configuration shut out so no hook, filter, fsmonitor,
//! or textconv the user configured can start.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::CheckpointError;

/// How long one git command may run before it is killed. A capture past it
/// marks the workspace too large to checkpoint for this process.
pub(super) const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// The poll interval while a git command runs (the child is reaped as soon
/// as it exits; this only bounds how late the timeout check fires).
const WAIT_POLL: Duration = Duration::from_millis(5);

/// The paths one git invocation needs: the store's git dir, the workspace as
/// its work tree, and the session's own index (concurrent sessions in one
/// workspace never share an index).
pub(super) struct GitEnv {
    pub(super) git_dir: PathBuf,
    pub(super) work_tree: PathBuf,
    pub(super) index: PathBuf,
    /// An empty file standing in for the global config.
    pub(super) empty_config: PathBuf,
}

impl GitEnv {
    /// Run `git <args>` in the work tree, feeding `stdin` when given, and
    /// return stdout. A non-zero exit or the timeout is an error.
    pub(super) fn run(
        &self,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>, CheckpointError> {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.work_tree)
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work_tree)
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.empty_config)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_NAMESPACE")
            .env_remove("GIT_CEILING_DIRECTORIES")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_with_timeout(command, stdin, args)
    }

    /// Run a command whose stdout is one line (a tree id).
    pub(super) fn line(&self, args: &[&str]) -> Result<String, CheckpointError> {
        let stdout = self.run(args, None)?;
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }
}

/// Initialize a store's git directory (no work tree attached).
pub(super) fn init_git_dir(git_dir: &Path, empty_config: &Path) -> Result<(), CheckpointError> {
    let mut command = Command::new("git");
    command
        .args(["init", "--quiet", "--bare"])
        .arg(git_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_config)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_timeout(command, None, &["init"]).map(drop)
}

fn run_with_timeout(
    mut command: Command,
    stdin: Option<&[u8]>,
    args: &[&str],
) -> Result<Vec<u8>, CheckpointError> {
    let label = args.first().copied().unwrap_or("git");
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CheckpointError::GitMissing
        } else {
            CheckpointError::Git(format!("git {label}: {error}"))
        }
    })?;
    // Stdin, stdout and stderr each drain on their own thread so a full pipe
    // never stalls the child while the timeout is watched here.
    let writer = stdin.map(<[u8]>::to_vec).and_then(|bytes| {
        child.stdin.take().map(|mut pipe| {
            std::thread::spawn(move || {
                // A child that exits early closes the pipe; its exit status reports why.
                let _ = pipe.write_all(&bytes);
            })
        })
    });
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // Kill failure means it already exited; the wait below reaps either way.
                let _ = child.kill();
                let _ = child.wait();
                return Err(CheckpointError::TimedOut);
            }
            Ok(None) => std::thread::sleep(WAIT_POLL),
            Err(error) => return Err(CheckpointError::Git(format!("git {label}: {error}"))),
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let stdout = stdout
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        let detail = String::from_utf8_lossy(&stderr).trim().to_string();
        Err(CheckpointError::Git(format!(
            "git {label} failed: {detail}"
        )))
    }
}

fn drain<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        // A read error ends the capture; the exit status carries the failure.
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}
