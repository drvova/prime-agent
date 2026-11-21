//! `(bash command [options])`: one shell command run to its end in the
//! session's working directory, in the foreground of its cell.
//!
//! The command runs under the kernel's shell with no stdin and the
//! non-interactive environment the Python kernel's `bash()` used. It leads its
//! own process group (its own session on Unix), so stopping it reaches every
//! process it started. It is stopped when its own timeout passes or when its
//! cell is aborted or timed out: the engine cannot interrupt a blocking host
//! function, so this one watches the cell's signal itself.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};

use crate::platform;
use crate::tools::tool_definition::AbortSignal;

/// Bytes of the output's start that are always kept.
const HEAD_CAP: usize = 512 * 1024;
/// Bytes of the output's end that are kept; what falls between is dropped.
const TAIL_CAP: usize = 3 * 512 * 1024;
/// How long a command may run when the call names no `timeout-ms`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a stopped command has to end on SIGTERM before SIGKILL.
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_millis(500);
/// How long output is still read after the shell exits: a background process
/// holding the inherited pipe must not hold the call open.
const EXIT_PIPE_GRACE: Duration = Duration::from_millis(500);

/// The environment every command gets on top of the process's own: nothing
/// may open an editor, a pager, or a prompt, since no one can answer it.
const CHILD_ENV: [(&str, &str); 14] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("CLICOLOR", "0"),
    ("FORCE_COLOR", "0"),
    ("GIT_EDITOR", "true"),
    ("GIT_SEQUENCE_EDITOR", "true"),
    ("GIT_TERMINAL_PROMPTS", "0"),
    ("GIT_ASKPASS", "true"),
    ("SSH_ASKPASS_REQUIRE", "never"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];

/// A command's captured output: its first [`HEAD_CAP`] bytes and a rolling
/// [`TAIL_CAP`]-byte tail, with the middle dropped and counted.
#[derive(Default)]
struct BoundedBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped: usize,
}

impl BoundedBuffer {
    fn write(&mut self, chunk: &[u8]) {
        let take = (HEAD_CAP - self.head.len()).min(chunk.len());
        self.head.extend_from_slice(&chunk[..take]);
        self.tail.extend(&chunk[take..]);
        let excess = self.tail.len().saturating_sub(TAIL_CAP);
        self.tail.drain(..excess);
        self.dropped += excess;
    }

    fn text(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.dropped == 0 {
            // Decoded as one run so a character split at the head's end survives.
            return String::from_utf8_lossy(&[self.head.as_slice(), &tail].concat()).into_owned();
        }
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(&tail);
        format!("{head}\n... [{} bytes dropped] ...\n{tail}", self.dropped)
    }
}

/// How a command's run ended.
enum End {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    CellStopped,
}

/// Run one command and report its exit code, output, and duration.
///
/// # Errors
///
/// Bad options, no usable shell or working directory, a failed spawn, or the
/// cell being stopped while the command ran (the command is killed first).
pub(super) fn run(
    runtime: &tokio::runtime::Handle,
    cwd: &Path,
    cell: &AbortSignal,
    options: &Value,
) -> anyhow::Result<Value> {
    let fields = options
        .as_object()
        .ok_or_else(|| anyhow!("options must be a hash"))?;
    if let Some(unknown) = fields
        .keys()
        .find(|key| !matches!(key.as_str(), "command" | "timeout-ms"))
    {
        bail!("unknown option '{unknown}' (accepted: timeout-ms)");
    }
    let script = fields
        .get("command")
        .and_then(Value::as_str)
        .filter(|script| !script.trim().is_empty())
        .ok_or_else(|| anyhow!("the command must be a non-empty string"))?;
    let timeout = match fields.get("timeout-ms") {
        None => Some(DEFAULT_TIMEOUT),
        Some(Value::Bool(false)) => None,
        Some(value) => Some(Duration::from_millis(
            value
                .as_u64()
                .filter(|limit| *limit > 0)
                .ok_or_else(|| anyhow!("timeout-ms must be a positive integer, or #f for none"))?,
        )),
    };
    if cell.is_cancelled() {
        bail!("the cell was stopped before the command started");
    }
    if !cwd.is_dir() {
        bail!("the working directory does not exist: {}", cwd.display());
    }
    let shell = platform::resolve_kernel_bash_shell(None)?.ok_or_else(|| {
        anyhow!("no POSIX shell was found (on Windows, install Git Bash in its default location)")
    })?;
    // One pipe carries stdout and stderr, so their order is the command's own.
    let (mut output, output_end) = std::io::pipe().context("could not open the output pipe")?;
    let mut command = tokio::process::Command::new(&shell);
    command
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .envs(CHILD_ENV)
        .stdin(Stdio::null())
        .stdout(
            output_end
                .try_clone()
                .context("could not open the output pipe")?,
        )
        .stderr(output_end)
        .kill_on_drop(true);
    #[cfg(unix)]
    platform::process::set_new_session(command.as_std_mut());
    #[cfg(not(unix))]
    {
        platform::set_new_process_group(command.as_std_mut());
        platform::set_no_window(command.as_std_mut());
    }
    runtime.block_on(async move {
        let started = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("could not start {shell}"))?;
        // `command` holds this process's copies of the pipe's writing end: the
        // output can only end once they are closed.
        drop(command);
        let pid = child.id();
        let captured = Arc::new(Mutex::new(BoundedBuffer::default()));
        let (output_ended, mut output_done) = tokio::sync::oneshot::channel::<()>();
        // Reading blocks, and a background process the command leaves behind
        // can hold the pipe open long after the call: a thread of its own
        // reads on, so that process never blocks on a full pipe.
        let reading = Arc::clone(&captured);
        std::thread::Builder::new()
            .name("scheme-bash-output".to_string())
            .spawn(move || {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    match output.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => reading
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .write(&buffer[..read]),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                // No one waits once the call has returned.
                let _ = output_ended.send(());
            })
            .context("could not start the output reader")?;
        let deadline = async {
            match timeout {
                Some(limit) => tokio::time::sleep(limit).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(deadline);
        // The call settles on the shell's exit, not on its pipe closing.
        let end = tokio::select! {
            biased;
            () = cell.cancelled() => End::CellStopped,
            () = &mut deadline => End::TimedOut,
            status = child.wait() => End::Exited(status),
        };
        let (status, timed_out) = match end {
            End::Exited(status) => (status, false),
            End::TimedOut | End::CellStopped => {
                // Only an unreaped child still pins its pid and its group.
                if let Some(pid) = pid {
                    #[cfg(unix)]
                    {
                        let _ = platform::process::signal_process_group(
                            pid as i32,
                            platform::Signal::Term,
                        );
                        let _ = tokio::time::timeout(TERM_GRACE, child.wait()).await;
                        // Whatever outlived SIGTERM, the shell or a member that ignored it.
                        let _ = platform::process::signal_process_group(
                            pid as i32,
                            platform::Signal::Kill,
                        );
                    }
                    #[cfg(not(unix))]
                    if !platform::kill_process_group_or_pid(pid as i32) {
                        let _ = child.start_kill();
                    }
                }
                let status = child.wait().await;
                if matches!(end, End::CellStopped) {
                    bail!("the command was killed: its cell was stopped");
                }
                (status, true)
            }
        };
        // Ended or not within the grace, what was read by then is the output.
        let _ = tokio::time::timeout(EXIT_PIPE_GRACE, &mut output_done).await;
        let status = status.context("could not wait for the command")?;
        let captured = captured.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(json!({
            "exit-code": platform::normalized_exit_code(&status),
            "output": captured.text(),
            "truncated": captured.dropped > 0,
            "timed-out": timed_out,
            "duration-ms": started.elapsed().as_millis() as u64,
        }))
    })
}
