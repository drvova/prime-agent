//! A stdio MCP server: a child process that reads newline-delimited JSON-RPC
//! on its stdin and writes it on its stdout.
//!
//! The child leads its own session, so stopping it reaches every process it
//! started. One task writes frames, one reads them and hands each reply to
//! the request waiting for it, and one drains stderr. A request that is
//! dropped while it waits (its cell was stopped, or it timed out) leaves the
//! connection whole: its late reply is discarded and the server is told the
//! request was cancelled.
//!
//! The child never outlives its transport. [`StdioTransport::shutdown`]
//! closes its stdin, then signals it, and reaps it; a transport dropped any
//! other way kills the child's process group.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use super::{credentials, reply_outcome, server_request_answer, Failure, FailureKind};
use crate::platform;

/// Bytes of a starting server's stderr that are kept for a startup failure.
const STDERR_BYTES: usize = 8 * 1024;
/// Lines of it that are shown.
const STDERR_LINES: usize = 40;
/// Bytes of the handshake's own failure that are shown.
const REASON_BYTES: usize = 1024;
/// How long a server has to exit once its stdin is closed.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// How long it then has to exit on SIGTERM before SIGKILL.
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_millis(500);
/// How long a failed start waits for the server's last stderr output.
const STDERR_GRACE: Duration = Duration::from_millis(250);

/// The requests waiting for their replies, by request id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// A running stdio server and the tasks that carry its frames.
pub(super) struct StdioTransport {
    server: String,
    /// Frames for the server's stdin; `None` closes it.
    outgoing: mpsc::UnboundedSender<Option<String>>,
    pending: Pending,
    next_id: AtomicU64,
    /// Set once the server's stdout ends.
    closed: Arc<AtomicBool>,
    /// The child until [`StdioTransport::shutdown`] takes it to reap it.
    child: Option<tokio::process::Child>,
    pid: Option<u32>,
    /// The stderr kept while the server starts; `None` once it has started.
    stderr: Arc<Mutex<Option<Vec<u8>>>>,
    stderr_ended: watch::Receiver<bool>,
    /// Configured strings a startup diagnostic must not show.
    private: Vec<String>,
    /// False when a configured secret is too short to redact reliably: the
    /// diagnostic then shows no server output at all.
    disclosable: bool,
}

/// A request in flight. Dropped before its reply arrived, it stops waiting
/// and tells the server the request was cancelled.
struct InFlight<'a> {
    transport: &'a StdioTransport,
    id: u64,
    /// The protocol forbids cancelling `initialize`.
    cancellable: bool,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let waiting = self
            .transport
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id)
            .is_some();
        if waiting && self.cancellable {
            // A server that has gone takes no notice; nothing to report.
            let _ = self.transport.outgoing.send(Some(
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": self.id, "reason": "the caller stopped waiting"},
                })
                .to_string(),
            ));
        }
    }
}

impl StdioTransport {
    /// Start the server `config` describes. Must be called from within a
    /// tokio runtime.
    ///
    /// # Errors
    ///
    /// The configuration is malformed, or the command could not be started.
    pub(super) fn start(server: &str, config: &Map<String, Value>) -> Result<Self, Failure> {
        let invalid = |reason: String| Failure::new(FailureKind::Invalid, reason);
        let program = config
            .get("command")
            .and_then(Value::as_str)
            .filter(|program| !program.is_empty());
        let arguments: Option<Vec<&str>> = match config.get("args") {
            None | Some(Value::Null) => Some(Vec::new()),
            Some(arguments) => arguments
                .as_array()
                .and_then(|arguments| arguments.iter().map(Value::as_str).collect()),
        };
        let (Some(program), Some(arguments)) = (program, arguments) else {
            return Err(invalid(format!(
                "MCP server '{server}' requires a command and string args"
            )));
        };
        let cwd =
            match config.get("cwd") {
                None | Some(Value::Null) => None,
                Some(cwd) => Some(cwd.as_str().ok_or_else(|| {
                    invalid(format!("MCP server '{server}' cwd must be a string"))
                })?),
            };
        let (env, added) = credentials::stdio_env(config)?;
        let mut private: Vec<String> = added
            .iter()
            .map(String::as_str)
            .chain([program])
            .chain(arguments.iter().copied())
            .chain(cwd)
            .filter(|value| value.len() >= 4)
            .map(str::to_string)
            .collect();
        // Longest first, so a value that contains another is redacted whole.
        private.sort_by_key(|value| std::cmp::Reverse(value.len()));
        let mut command = tokio::process::Command::new(program);
        command
            .args(&arguments)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        platform::process::set_new_session(command.as_std_mut());
        #[cfg(not(unix))]
        {
            platform::set_new_process_group(command.as_std_mut());
            platform::set_no_window(command.as_std_mut());
        }
        let mut child = command.spawn().map_err(|error| {
            Failure::new(
                FailureKind::Startup,
                format!("MCP stdio server '{server}' could not be started: {error}"),
            )
        })?;
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(Failure::new(
                FailureKind::Startup,
                format!("MCP stdio server '{server}' started without its pipes"),
            ));
        };
        let (outgoing, mut frames) = mpsc::unbounded_channel::<Option<String>>();
        let pending = Pending::default();
        let closed = Arc::new(AtomicBool::new(false));
        tokio::spawn(async move {
            while let Some(Some(frame)) = frames.recv().await {
                let written = async {
                    stdin.write_all(frame.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                };
                // A server that stopped reading has ended or will be ended:
                // the reader below reports it to whoever is waiting.
                if written.await.is_err() {
                    break;
                }
            }
            // Dropping stdin here is what tells the server to exit.
        });
        let (routed, ended, answers) =
            (Arc::clone(&pending), Arc::clone(&closed), outgoing.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            // A line that is not JSON is a server logging to the wrong
            // stream; it is skipped.
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(answer) = server_request_answer(&message) {
                    // The writer has ended only when the server has.
                    let _ = answers.send(Some(answer.to_string()));
                } else if let Some(waiter) =
                    message.get("id").and_then(Value::as_u64).and_then(|id| {
                        routed
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&id)
                    })
                {
                    // The request stopped waiting between the lookup and here.
                    let _ = waiter.send(message);
                }
            }
            ended.store(true, Ordering::SeqCst);
            // Dropping the waiters tells every request the server has gone.
            routed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        });
        let kept = Arc::new(Mutex::new(Some(Vec::new())));
        let (stderr_end, stderr_ended) = watch::channel(false);
        let keeping = Arc::clone(&kept);
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            // Read to the end even when nothing is kept, so the server never
            // blocks on a full pipe.
            while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
                if let Some(tail) = keeping
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
                {
                    tail.extend_from_slice(&chunk[..read]);
                    let excess = tail.len().saturating_sub(STDERR_BYTES);
                    tail.drain(..excess);
                }
            }
            // No one waits once the server has started.
            let _ = stderr_end.send(true);
        });
        Ok(StdioTransport {
            server: server.to_string(),
            outgoing,
            pending,
            next_id: AtomicU64::new(1),
            closed,
            pid: child.id(),
            child: Some(child),
            stderr: kept,
            stderr_ended,
            private,
            disclosable: added
                .iter()
                .all(|value| value.is_empty() || value.len() >= 4),
        })
    }

    /// Whether the server's stdout has ended: it can answer nothing more.
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send one request and wait up to `limit` for its reply.
    ///
    /// # Errors
    ///
    /// The server answered with an error, did not answer in time, or closed
    /// its connection.
    pub(super) async fn request(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, Failure> {
        let gone = || {
            Failure::new(
                FailureKind::Protocol,
                format!("MCP server '{}' closed its connection", self.server),
            )
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply, replied) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, reply);
        let _in_flight = InFlight {
            transport: self,
            id,
            cancellable: method != "initialize",
        };
        // The reader marks the server gone before it drops the waiters: a
        // request registered after that would wait for nothing.
        if self.is_closed() {
            return Err(gone());
        }
        self.outgoing
            .send(Some(
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
            ))
            .map_err(|_| gone())?;
        match tokio::time::timeout(limit, replied).await {
            Ok(Ok(reply)) => reply_outcome(reply),
            Ok(Err(_)) => Err(gone()),
            Err(_) => Err(Failure::new(
                FailureKind::TimedOut,
                format!(
                    "MCP server '{}' did not answer {method} within {} ms",
                    self.server,
                    limit.as_millis()
                ),
            )),
        }
    }

    /// Send one notification.
    ///
    /// # Errors
    ///
    /// The server has closed its connection.
    pub(super) fn notify(&self, method: &str) -> Result<(), Failure> {
        self.outgoing
            .send(Some(
                json!({"jsonrpc": "2.0", "method": method}).to_string(),
            ))
            .map_err(|_| {
                Failure::new(
                    FailureKind::Protocol,
                    format!("MCP server '{}' closed its connection", self.server),
                )
            })
    }

    /// The server has started: its stderr is no longer kept.
    pub(super) fn started(&self) {
        *self.stderr.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The failure of a start that `cause` ended, with the server's last
    /// stderr output. Configured values are redacted from both, and neither
    /// is shown when a configured secret is too short to redact.
    pub(super) async fn startup_failure(&self, cause: &Failure) -> Failure {
        // A server that has just died may not have been read to its end yet.
        let mut ended = self.stderr_ended.clone();
        let _ = tokio::time::timeout(STDERR_GRACE, ended.wait_for(|ended| *ended)).await;
        let tail = self
            .stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_default();
        // Printable, redacted, and cut to its last lines and bytes.
        let shown = |text: &str, bytes: usize| {
            let mut plain = String::with_capacity(text.len());
            let mut letters = text.chars().peekable();
            while let Some(letter) = letters.next() {
                match letter {
                    // A terminal escape: a CSI sequence ends at its final
                    // byte, an OSC sequence at its terminator.
                    '\u{1b}' => match letters.peek() {
                        Some('[') => {
                            letters.next();
                            while letters
                                .next()
                                .is_some_and(|next| !('@'..='~').contains(&next))
                            {
                            }
                        }
                        Some(']') => {
                            while letters
                                .next()
                                .is_some_and(|next| next != '\u{7}' && next != '\\')
                            {
                            }
                        }
                        Some(_) | None => {}
                    },
                    '\r' | '\n' => plain.push('\n'),
                    '\t' => plain.push(' '),
                    control if control.is_control() => {}
                    printable => plain.push(printable),
                }
            }
            for value in &self.private {
                plain = plain.replace(value, "[REDACTED]");
            }
            let lines: Vec<&str> = plain
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            let kept = lines[lines.len().saturating_sub(STDERR_LINES)..].join("\n");
            let mut start = kept.len().saturating_sub(bytes);
            while !kept.is_char_boundary(start) {
                start += 1;
            }
            kept[start..].to_string()
        };
        let (reason, output) = if self.disclosable {
            (
                shown(&cause.message, REASON_BYTES),
                shown(&String::from_utf8_lossy(&tail), STDERR_BYTES),
            )
        } else {
            (
                "details omitted for safe redaction".to_string(),
                String::new(),
            )
        };
        let detail = if output.is_empty() {
            String::new()
        } else {
            format!(" Stderr tail:\n{output}")
        };
        Failure::new(
            FailureKind::Startup,
            format!(
                "MCP stdio server '{}' failed during startup ({reason}).{detail}",
                self.server
            ),
        )
    }

    /// End the server and reap it: close its stdin, and signal it if it does
    /// not exit on that.
    pub(super) async fn shutdown(mut self) {
        // The writer has ended only when the server has.
        let _ = self.outgoing.send(None);
        let Some(mut child) = self.child.take() else {
            return;
        };
        if tokio::time::timeout(CLOSE_GRACE, child.wait())
            .await
            .is_ok()
        {
            return;
        }
        // The child is not reaped yet, so its pid still names its own group.
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            let _ = platform::process::signal_process_group(pid as i32, platform::Signal::Term);
            if tokio::time::timeout(TERM_GRACE, child.wait()).await.is_ok() {
                return;
            }
            let _ = platform::process::signal_process_group(pid as i32, platform::Signal::Kill);
        }
        #[cfg(not(unix))]
        if let Some(pid) = self.pid {
            let _ = platform::kill_process_group_or_pid(pid as i32);
        }
        // Already gone is the only way this fails.
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

impl Drop for StdioTransport {
    /// A transport dropped without [`StdioTransport::shutdown`] (a start that
    /// failed or was abandoned) kills the server's whole group; the child
    /// itself is then killed and reaped by its own drop.
    fn drop(&mut self) {
        // Only while the child is unreaped does its pid still name its group.
        if let (Some(_), Some(pid)) = (&self.child, self.pid) {
            #[cfg(unix)]
            let _ = platform::process::signal_process_group(pid as i32, platform::Signal::Kill);
            #[cfg(not(unix))]
            let _ = platform::kill_process_group_or_pid(pid as i32);
        }
    }
}
