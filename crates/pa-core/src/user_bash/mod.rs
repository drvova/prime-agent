//! The user's `!` commands (moved here from pa-daemon so both daemons run
//! them alike): the one-at-a-time slot ([`UserBash`]), one run of a command
//! on a shell process ([`run_bash`]), the `bash_start`/`bash_output`/
//! `bash_end` events a run streams, and its durable `bashExecution` row. A
//! TS-stack port: sanitization, the 50KB window, the spill.
//!
//! The daemon owns the wire around it: the supervisor's worker answers
//! `execute_bash`, `execute_bash_and_wait` and `abort_bash`, the runtime
//! daemon `session.bash` and `session.abort_bash`.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use output::{truncate_tail, OutputSpill, OutputStream};

mod output;
mod streaming_utf8;
#[cfg(test)]
mod tests;

/// How long a run still reads output after its shell exits (TS `EXIT_STDIO_GRACE_MS`): a
/// background process holding the inherited pipes must not hold the run open.
const EXIT_STDIO_GRACE: std::time::Duration = std::time::Duration::from_millis(100);
/// How long a closing session waits for its aborted runs to settle (TS
/// `UPDATE_RESTART_ABORT_BASH_TIMEOUT_MS`).
const CLOSE_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Streaming window: chunks are retained (spilled to disk beyond this).
pub(crate) const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Streaming retention cap: the in-memory tail (TS `maxOutputBytes`).
pub(crate) const STREAM_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;
/// Tail truncation line budget (TS `DEFAULT_MAX_LINES`).
pub(crate) const DEFAULT_MAX_LINES: usize = 2000;
/// The TS spill prefix (temp file names in `$TMPDIR`).
pub(crate) const SPILL_PREFIX: &str = "pa-bash";

/// The user-bash slot: one command runs at a time (TS `_userBashRunning`),
/// with a kill switch `abort_bash` pulls.
pub struct UserBash {
    /// The user-bash claim (`execute_bash` only; TS `runUserBash` guard).
    running: AtomicBool,
    /// Awaited bash runs in flight (TS `_bashAbortControllers.size`):
    /// they count toward `isBashRunning` without claiming the slot.
    awaited: AtomicUsize,
    /// Each invocation owns its cancellation state; a later start cannot clear an earlier abort.
    controllers: std::sync::Mutex<Vec<std::sync::Weak<CancellationToken>>>,
    /// Signals each settle (the slot's release or an awaited run's end).
    settled: Notify,
}

impl Default for UserBash {
    fn default() -> Self {
        Self::new()
    }
}

impl UserBash {
    #[must_use]
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            awaited: AtomicUsize::new(0),
            controllers: std::sync::Mutex::new(Vec::new()),
            settled: Notify::new(),
        }
    }

    /// Claim and register synchronously, before the spawned task can race an abort.
    /// `None` while a claimed command still runs.
    ///
    /// # Panics
    ///
    /// The run registry's lock was poisoned.
    pub fn claim(&self) -> Option<Arc<CancellationToken>> {
        let mut controllers = self.controllers.lock().unwrap();
        if self.running.swap(true, Ordering::SeqCst) {
            return None;
        }
        let cancellation = Arc::new(CancellationToken::new());
        controllers.retain(|controller| controller.strong_count() > 0);
        controllers.push(Arc::downgrade(&cancellation));
        Some(cancellation)
    }

    /// Release the slot [`Self::claim`] took, once the run settled.
    pub fn release(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.settled.notify_waiters();
    }

    /// Whether a user bash or an awaited bash run is in flight (TS `isBashRunning`).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst) || self.awaited.load(Ordering::SeqCst) > 0
    }

    /// Count one awaited run (`execute_bash_and_wait`) toward [`Self::is_running`]; the returned
    /// bracket decrements on drop, so a dropped future cannot leave the count stuck on.
    ///
    /// # Panics
    ///
    /// The run registry's lock was poisoned.
    pub fn begin_awaited(&self) -> AwaitedRun<'_> {
        let mut controllers = self.controllers.lock().unwrap();
        let cancellation = Arc::new(CancellationToken::new());
        controllers.retain(|controller| controller.strong_count() > 0);
        controllers.push(Arc::downgrade(&cancellation));
        self.awaited.fetch_add(1, Ordering::SeqCst);
        AwaitedRun {
            user_bash: self,
            cancellation,
        }
    }

    /// Notify every in-flight run to kill its process tree (TS `abortBash`
    /// aborts every controller); the settled runs report cancelled.
    ///
    /// # Panics
    ///
    /// The run registry's lock was poisoned.
    pub fn abort(&self) {
        let mut controllers = self.controllers.lock().unwrap();
        controllers.retain(|controller| {
            if let Some(controller) = controller.upgrade() {
                controller.cancel();
                true
            } else {
                false
            }
        });
    }

    /// [`Self::abort`], then wait (bounded) until no run is in flight: each run kills its tree
    /// in its own task, so a close must not exit ahead of the kills (TS `abortBashForClose`).
    pub async fn abort_and_settle(&self) {
        self.abort();
        let settled = async {
            loop {
                // Armed before the check, so a settle in between still wakes this wait.
                let notified = self.settled.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.is_running() {
                    return;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(CLOSE_SETTLE_TIMEOUT, settled).await;
    }
}

/// The wire identity echoed on `bash_start`/`bash_end` (TS `identity`).
#[must_use]
pub fn bash_identity(transient: bool, run_id: Option<&str>) -> Value {
    let mut identity = serde_json::Map::new();
    if transient {
        identity.insert("transient".to_string(), json!(true));
    }
    if let Some(run_id) = run_id {
        identity.insert("runId".to_string(), json!(run_id));
    }
    Value::Object(identity)
}

/// A run's `bash_start` event, with the run's `identity`.
#[must_use]
pub fn start_event(command: &str, exclude_from_context: bool, identity: &Value) -> Value {
    merge_identity(
        json!({
            "type": "bash_start",
            "command": command,
            "excludeFromContext": exclude_from_context,
        }),
        identity,
    )
}

/// Records one settled run as the `bashExecution` row of `engine`'s session
/// (TS `recordBashResult`), joining the model's context unless excluded:
/// with no turn running it is saved and appended to the agent's messages at
/// once, under the agent's idle reservation so no turn starts between the
/// two; a running turn takes it as a steer, read (and saved) at its next
/// step, so it never lands between a tool call and its result.
///
/// # Errors
///
/// The transcript write failed.
pub async fn record_in_engine(
    engine: &crate::session_engine::engine::SessionEngine,
    command: &str,
    result: &BashResult,
    exclude_from_context: bool,
) -> std::io::Result<()> {
    let row =
        pa_types::session::AgentMessage::BashExecution(pa_types::session::BashExecutionMessage {
            command: command.to_string(),
            output: result.output.clone(),
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result.full_output_path.clone(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as u64),
            exclude_from_context: exclude_from_context.then_some(true),
        });
    let agent = engine.session.agent();
    let loop_row = crate::session_engine::session_message_to_loop(&row);
    if agent.signal().is_some() {
        if let Some(loop_row) = loop_row {
            agent.steer(loop_row);
            return Ok(());
        }
    }
    let _idle = agent.reserve_idle().await;
    {
        let mut session = engine.session.session_handle().lock().await;
        session.append_message(row)?;
        session.flush_now()?;
    }
    if let Some(loop_row) = loop_row {
        agent.append_messages(vec![loop_row]).await;
    }
    Ok(())
}

/// `event` with `identity`'s fields added.
#[must_use]
pub fn merge_identity(mut event: Value, identity: &Value) -> Value {
    if let (Some(event), Some(identity)) = (event.as_object_mut(), identity.as_object()) {
        for (key, value) in identity {
            event.insert(key.clone(), value.clone());
        }
    }
    event
}

/// The drop-bracket [`UserBash::begin_awaited`] returns: one awaited run's contribution to the
/// `isBashRunning` flag, released even when the run's future is dropped mid-await.
pub struct AwaitedRun<'a> {
    user_bash: &'a UserBash,
    cancellation: Arc<CancellationToken>,
}

impl AwaitedRun<'_> {
    /// The run's cancellation: [`UserBash::abort`] cancels it.
    #[must_use]
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

impl Drop for AwaitedRun<'_> {
    fn drop(&mut self) {
        self.user_bash.awaited.fetch_sub(1, Ordering::SeqCst);
        self.user_bash.settled.notify_waiters();
    }
}

/// The TS `BashResult` wire shape.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// The settled user-bash run (TS `UserBashEndDetails` plus the output it
/// carries for the durable row).
pub struct BashEnd {
    pub output: String,
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
    /// The command could not start (a missing working directory, no shell).
    pub error_message: Option<String>,
}

impl BashEnd {
    /// The `BashResult` the run settled with.
    #[must_use]
    pub fn result(&self) -> BashResult {
        BashResult {
            output: self.output.clone(),
            exit_code: self.exit_code,
            cancelled: self.cancelled,
            truncated: self.truncated,
            full_output_path: self.full_output_path.clone(),
        }
    }

    /// The settled run's `bash_end` event, with the run's `identity`.
    #[must_use]
    pub fn end_event(&self, identity: &Value) -> Value {
        let mut event = json!({
            "type": "bash_end",
            "exitCode": self.exit_code,
            "cancelled": self.cancelled,
            "truncated": self.truncated,
        });
        if let Some(path) = &self.full_output_path {
            event["fullOutputPath"] = json!(path);
        }
        if let Some(error) = &self.error_message {
            event["errorMessage"] = json!(error);
        }
        merge_identity(event, identity)
    }
}

/// Where a streamed run's `bash_output` events go, one per sanitized chunk.
pub type ChunkSink = Arc<dyn Fn(Value) + Send + Sync>;

/// One process run (TS `executeBashWithOperations` over the local bash
/// operations): the streaming window, the spill, and the tail truncation.
pub struct RunBash<'a> {
    pub command: &'a str,
    pub cwd: &'a str,
    /// The `shellCommandPrefix` setting, run ahead of the command.
    pub prefix: Option<&'a str>,
    /// The `shellPath` setting.
    pub shell_path: Option<&'a str>,
    pub cancellation: &'a CancellationToken,
    /// The streaming emit target for each sanitized chunk; `None` on
    /// the awaited path, which emits nothing.
    pub on_chunk: Option<ChunkSink>,
}

/// Runs one command to its settle: stdout and stderr merged, sanitized and
/// tail-truncated, spilled to a private temp file past the window; a
/// cancelled run kills its process tree and reports no exit code.
///
/// # Panics
///
/// A pipe reader panicked holding the output's lock while the other one
/// still reads.
pub async fn run_bash(run: RunBash<'_>) -> BashEnd {
    let resolved = match run.prefix.filter(|prefix| !prefix.is_empty()) {
        Some(prefix) => format!("{prefix}\n{}", run.command),
        None => run.command.to_string(),
    };
    let spawn_failure = |message: String| BashEnd {
        output: String::new(),
        exit_code: None,
        cancelled: false,
        truncated: false,
        full_output_path: None,
        error_message: Some(message),
    };
    if !std::path::Path::new(run.cwd).exists() {
        // TS local operations reject before spawning.
        return spawn_failure(format!(
            "Working directory does not exist: {}\nCannot execute bash commands.",
            run.cwd
        ));
    }
    let shell = match crate::platform::shell::get_shell_config(run.shell_path) {
        Ok(shell) => shell,
        Err(error) => return spawn_failure(error.to_string()),
    };
    let mut command = pa_types::session_scope::tokio_command(&shell.shell);
    command
        .args(&shell.args)
        .arg(&resolved)
        .current_dir(run.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group on POSIX, so a kill reaches every process the
    // command started (TS `detached: true`); Windows tree kills go through
    // `taskkill /T`.
    #[cfg(unix)]
    crate::platform::process::set_new_process_group(command.as_std_mut());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return spawn_failure(error.to_string()),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Only this run kills its tree, and only before it reaps the child: a
    // reaped pid can be reused. A failed tree kill still stops the shell.
    let kill_tree = |child: &mut tokio::process::Child| {
        let killed = child
            .id()
            .is_some_and(|pid| crate::platform::process::kill_process_group_or_pid(pid as i32));
        if !killed {
            let _ = child.start_kill();
        }
    };
    let abort = run.cancellation.cancelled();
    tokio::pin!(abort);
    // Cancellation remains sticky even if another command starts before this task runs.
    let mut killed = run.cancellation.is_cancelled();
    if killed {
        kill_tree(&mut child);
    }
    let stream = std::sync::Arc::new(std::sync::Mutex::new(OutputStream {
        chunks: Vec::new(),
        retained_bytes: 0,
        total_bytes: 0,
        spill: OutputSpill::new(),
        on_chunk: run.on_chunk,
    }));
    // stdout and stderr merge into one stream (TS wires both pipes to the
    // same `onData`); the shared accumulator keeps the arrival order.
    // A read can end inside a multi-byte character; each pipe's decoder holds its leading
    // bytes for the next read (TS `TextDecoder`, `stream: true`).
    let pump = |mut reader: Box<dyn AsyncRead + Unpin + Send>| {
        let stream = Arc::clone(&stream);
        async move {
            let mut decoder = streaming_utf8::StreamingUtf8::default();
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let text = decoder.decode_chunk(&buffer[..read]);
                        stream.lock().unwrap().push(&text);
                    }
                }
            }
            let rest = decoder.flush();
            stream.lock().unwrap().push(&rest);
        }
    };
    let stdout_pump = stdout.map(|pipe| pump(Box::new(pipe)));
    let stderr_pump = stderr.map(|pipe| pump(Box::new(pipe)));
    let mut pumps = Box::pin(async move {
        match (stdout_pump, stderr_pump) {
            (Some(stdout_pump), Some(stderr_pump)) => {
                futures::future::join(stdout_pump, stderr_pump).await;
            }
            (Some(pump), None) | (None, Some(pump)) => pump.await,
            (None, None) => {}
        }
    });
    let mut pumps_done = false;
    // The run settles on the shell's exit, not on its pipes closing
    // (abort_bash may kill it meanwhile; a cancelled run reports no exit code).
    let status = loop {
        tokio::select! {
            // If both are ready, kill while the unreaped child still pins its PID.
            biased;
            () = &mut abort, if !killed => {
                killed = true;
                kill_tree(&mut child);
            }
            status = child.wait() => break status,
            () = &mut pumps, if !pumps_done => pumps_done = true,
        }
    };
    // Descendants still holding the pipes (a `cmd &`) get the grace, then the
    // pipes close (TS `waitForChildProcess`).
    if !pumps_done {
        let _ = tokio::time::timeout(EXIT_STDIO_GRACE, &mut pumps).await;
    }
    drop(pumps);
    // A signal death reports 128 + the signal number, like a shell's `$?` (TS
    // `normalizedExitCode`); a cancelled run still reports none below.
    let exit_code = status
        .ok()
        .and_then(|status| crate::platform::process::normalized_exit_code(&status))
        .map(i64::from);
    let cancelled = run.cancellation.is_cancelled();
    // The pumps are dropped, so this is the last reference; a poisoned
    // lock (a reader panicked) keeps whatever it collected.
    let mut stream = match Arc::try_unwrap(stream) {
        Ok(mutex) => mutex
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Err(stream) => stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone_stream(),
    };
    let full_output = stream.chunks.join("");
    let (output, truncated) = truncate_tail(&full_output);
    // A line-truncated run spills at settle even under the byte budget,
    // so the truncation notice can always name a full-output file.
    if truncated {
        stream.spill.open(&stream.chunks);
    }
    let full_output_path = stream.spill.finalize();
    BashEnd {
        output,
        exit_code: if cancelled { None } else { exit_code },
        cancelled,
        truncated,
        full_output_path,
        error_message: None,
    }
}
