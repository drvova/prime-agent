//! The worker arms for `execute_bash`, `execute_bash_and_wait`, and
//! `abort_bash` over pa-core's user bash (`pa_core::user_bash`: the slot,
//! the run, its events), with the `bashExecution` durable row in the
//! worker's session store.

use std::sync::Arc;

use pa_core::user_bash::{
    bash_identity, run_bash, start_event, AwaitedRun, BashResult, ChunkSink, RunBash,
};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::Worker;

impl Worker {
    /// Rust-native kernel bash activity commands. These inspect the kernel's
    /// own shell commands, not the user-bash slot or a client-supplied PID.
    pub(crate) async fn handle_kernel_bash_activity(
        &self,
        command_type: &str,
        payload: &Value,
    ) -> DaemonResponse {
        if let Err(response) = self.require_created(command_type) {
            return response;
        }
        let Some(engine) = &self.agent_engine else {
            return response_failure(None, command_type, "Kernel is not running", None);
        };
        let action = match command_type {
            "list_kernel_bash" => "list",
            "tail_kernel_bash" => "tail",
            "kill_kernel_bash" => "kill",
            _ => return response_failure(None, command_type, "Unknown kernel bash command", None),
        };
        let activity_id = payload.get("activityId").and_then(Value::as_str);
        if action != "list" && activity_id.is_none_or(str::is_empty) {
            return response_failure(None, command_type, "activityId is required", None);
        }
        let lines = if action == "tail" {
            match payload.get("lines") {
                None => 50,
                Some(value) => match value.as_u64() {
                    Some(lines) if (1..=200).contains(&lines) => lines as usize,
                    _ => {
                        return response_failure(
                            None,
                            command_type,
                            "lines must be between 1 and 200",
                            None,
                        )
                    }
                },
            }
        } else {
            50
        };
        match engine.bash_activity(action, activity_id, lines).await {
            Ok(mut fields) => {
                if let Some(object) = fields.as_object_mut() {
                    object.remove("event");
                    object.remove("status");
                    object.remove("reason");
                    object.remove("id"); // kernel request id, not activity id
                    object.remove("activityId");
                    if let Some(activity_id) = activity_id {
                        object.insert("id".to_string(), json!(activity_id));
                    }
                }
                response_success(None, command_type, Some(fields))
            }
            Err(error) => response_failure(None, command_type, &error.to_string(), None),
        }
    }

    /// `execute_bash { command, excludeFromContext?, transient?, runId? }`
    /// (TS `runUserBash`): the running guard rejects a second command;
    /// the response goes out before completion, output streams as
    /// `bash_output`, and the settled `bash_end` (plus the durable row) follows.
    pub(crate) fn handle_execute_bash(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("execute_bash") {
            return response;
        }
        let Some(command) = payload.get("command").and_then(Value::as_str) else {
            return response_failure(
                None,
                "execute_bash",
                "execute_bash requires a command",
                None,
            );
        };
        // Claim the slot synchronously (TS claims before the async dispatch
        // could slip a second command through), through the admission gate:
        // the gate re-check and the claim share the lock, so the close's
        // abort cannot miss the new token (a claim its `abort_and_settle`
        // missed would leave the child running past the exit).
        let cancellation = match self.admit_streamed_bash() {
            Ok(cancellation) => cancellation,
            Err(response) => return response,
        };
        let exclude_from_context = payload
            .get("excludeFromContext")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let transient = payload
            .get("transient")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let run_id = payload.get("runId").and_then(Value::as_str);
        let identity = bash_identity(transient, run_id);
        self.emit_worker_event(start_event(command, exclude_from_context, &identity));
        // The run outlives the response (bash can exceed the client's
        // request timeout); completion streams via bash_end.
        let core = Arc::clone(&self.core);
        let events = self.events.clone();
        let user_bash = Arc::clone(&self.user_bash);
        let work_notify = Arc::clone(&self.work_notify);
        let command = command.to_string();
        let agent_dir = self.config.agent_dir.clone();
        tokio::spawn(async move {
            let cwd = {
                let core = core.lock().unwrap();
                core.cwd.clone()
            };
            let settings = {
                let core = core.lock().unwrap();
                pa_core::settings::SettingsManager::create(&core.cwd, &agent_dir)
            };
            let prefix = settings.settings().shell_command_prefix.clone();
            let shell_path = settings.settings().shell_path.clone();
            let on_chunk: ChunkSink = {
                let (core, events) = (Arc::clone(&core), Arc::clone(&events));
                Arc::new(move |event| emit_session_event_frame(&core, &events, event))
            };
            let end = run_bash(RunBash {
                command: &command,
                cwd: &cwd,
                prefix: prefix.as_deref(),
                shell_path: shell_path.as_deref(),
                cancellation: &cancellation,
                on_chunk: Some(on_chunk),
            })
            .await;
            // Persist the durable row (transient runs live only in their
            // pane; reloads and rebuilds cannot resurface them).
            if !transient {
                record_bash_result(&core, &command, &end.result(), exclude_from_context);
            }
            user_bash.release();
            emit_session_event_frame(&core, &events, end.end_event(&identity));
            // The queue drains after the slot is released (TS
            // `_drainQueuedMessagesAfterBash`).
            work_notify.notify_one();
        });
        response_success(None, "execute_bash", None)
    }

    /// `require_created`'s shutdown gate re-checked with the streamed slot
    /// claim under the same lock: the claim cannot interleave with the gate's
    /// close, so the close's abort always sees — and cancels — the new token
    /// (a claim its `abort_and_settle` missed would leave the child running
    /// past the exit).
    #[expect(
        clippy::result_large_err,
        reason = "the error channel carries the whole wire response, which is wide by design"
    )]
    pub(crate) fn admit_streamed_bash(&self) -> Result<Arc<CancellationToken>, DaemonResponse> {
        let core = self.core.lock().unwrap();
        if core.shutdown_requested {
            return Err(response_failure(
                None,
                "execute_bash",
                "Session is shutting down",
                None,
            ));
        }
        self.user_bash.claim().ok_or_else(|| {
            response_failure(
                None,
                "execute_bash",
                "A bash command is already running",
                None,
            )
        })
    }

    /// `require_created`'s shutdown gate re-checked and the awaited run
    /// registered under the same lock: the registration cannot interleave
    /// with the gate's close, so the close's abort always sees — and cancels —
    /// the new token (a registration its `abort_and_settle` missed would let
    /// a child outlive the exit).
    #[expect(
        clippy::result_large_err,
        reason = "the error channel carries the whole wire response, which is wide by design"
    )]
    pub(crate) fn admit_awaited_bash(&self) -> Result<AwaitedRun<'_>, DaemonResponse> {
        let core = self.core.lock().unwrap();
        if core.shutdown_requested {
            return Err(response_failure(
                None,
                "execute_bash_and_wait",
                "Session is shutting down",
                None,
            ));
        }
        Ok(self.user_bash.begin_awaited())
    }

    /// `execute_bash_and_wait { command }` (TS `executeBash`): run to
    /// completion, record the row, answer the `BashResult` wire shape.
    pub(crate) async fn handle_execute_bash_and_wait(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("execute_bash_and_wait") {
            return response;
        }
        let Some(command) = payload.get("command").and_then(Value::as_str) else {
            return response_failure(
                None,
                "execute_bash_and_wait",
                "execute_bash_and_wait requires a command",
                None,
            );
        };
        let (cwd, prefix, shell_path) = {
            let core = self.core.lock().unwrap();
            let settings =
                pa_core::settings::SettingsManager::create(&core.cwd, &self.config.agent_dir);
            let settings = settings.settings();
            (
                core.cwd.clone(),
                settings.shell_command_prefix.clone(),
                settings.shell_path.clone(),
            )
        };
        let command = command.to_string();
        // The awaited run counts toward `isBashRunning` without
        // blocking a streamed user bash. The admission re-checks the
        // shutdown gate and registers under the same lock, so a close
        // cannot slip between them: it either sees the gate shut (and
        // the dispatch is rejected) or aborts a token that is already
        // registered (a registration after its abort would leave a
        // child the exit orphans).
        let awaited = match self.admit_awaited_bash() {
            Ok(awaited) => awaited,
            Err(response) => return response,
        };
        let end = run_bash(RunBash {
            command: &command,
            cwd: &cwd,
            prefix: prefix.as_deref(),
            shell_path: shell_path.as_deref(),
            cancellation: awaited.cancellation(),
            // The awaited path emits nothing (TS passes no `onChunk`).
            on_chunk: None,
        })
        .await;
        // The durable row lands while the run still counts: the orphan
        // monitor treats `!is_running` as exit permission, so an awaited
        // result released before its row (the streamed path persists,
        // then releases) could be lost to a racing orphan exit.
        self.roster_pushes.push();
        if let Some(error) = &end.error_message {
            drop(awaited);
            return response_failure(None, "execute_bash_and_wait", error, None);
        }
        let result = BashResult {
            output: end.output,
            exit_code: end.exit_code,
            cancelled: end.cancelled,
            truncated: end.truncated,
            full_output_path: end.full_output_path,
        };
        record_bash_result(&self.core, &command, &result, false);
        drop(awaited);
        response_success(
            None,
            "execute_bash_and_wait",
            Some(serde_json::to_value(&result).unwrap_or(Value::Null)),
        )
    }

    /// `abort_bash` (TS `abortBash`): kill the in-flight command; the
    /// settled run reports cancelled. Always a success.
    pub(crate) fn handle_abort_bash(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("abort_bash") {
            return response;
        }
        self.user_bash.abort();
        response_success(None, "abort_bash", None)
    }
}

/// Record one bash outcome as the durable `bashExecution` row (TS
/// `recordBashResult`); it joins the model context unless excluded.
fn record_bash_result(
    core: &Arc<std::sync::Mutex<crate::worker::SessionCore>>,
    command: &str,
    result: &BashResult,
    exclude_from_context: bool,
) {
    let mut core = core.lock().unwrap();
    let Some(store) = core.store.as_mut() else {
        return;
    };
    let mut row = json!({
        "role": "bashExecution",
        "command": command,
        "output": result.output,
        "cancelled": result.cancelled,
        "truncated": result.truncated,
        "timestamp": crate::util::now_ms(),
    });
    if let Some(exit_code) = result.exit_code {
        row["exitCode"] = json!(exit_code);
    }
    if let Some(path) = &result.full_output_path {
        row["fullOutputPath"] = json!(path);
    }
    if exclude_from_context {
        row["excludeFromContext"] = json!(true);
    }
    let _ = store.persist_entry("message", json!({ "message": row }));
}

/// Sequence and broadcast one `session_event` frame (the free-standing
/// form of the worker's emitter the spawned bash task uses).
pub(crate) fn emit_session_event_frame(
    core: &Arc<std::sync::Mutex<crate::worker::SessionCore>>,
    events: &Arc<crate::worker::EventPump>,
    event: Value,
) {
    use crate::protocol::create_daemon_event_meta;
    use crate::protocol::DaemonOutbound;
    use crate::worker::OutboundFrame;
    let mut core = core.lock().unwrap();
    let sequence = core.last_event_sequence + 1;
    core.last_event_sequence = sequence;
    let meta = create_daemon_event_meta(
        &core.active_session_id,
        sequence,
        None,
        Some(&core.generation),
    );
    let active_session_id = core.active_session_id.clone();
    let outbound = DaemonOutbound::SessionEvent {
        active_session_id,
        event,
        meta: Some(meta),
        rest: Map::default(),
    };
    let payload = serde_json::to_vec(&outbound).unwrap_or_default();
    drop(core);
    events.send(OutboundFrame::session_event(payload));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    async fn created_worker(cwd: &std::path::Path) -> (tempfile::TempDir, Arc<Worker>) {
        std::fs::create_dir_all(cwd).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let config = crate::worker::WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "bash-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": cwd.to_string_lossy(), "name": "bash" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        (tmp, worker)
    }

    fn bash_rows(worker: &Worker) -> Vec<Value> {
        let core = worker.core.lock().unwrap();
        core.store.as_ref().map_or_default(|store| {
            store
                .entries()
                .iter()
                .filter(|entry| entry.type_ == "message")
                .filter_map(|entry| entry.fields.get("message").cloned())
                .filter(|message| {
                    message.get("role").and_then(Value::as_str) == Some("bashExecution")
                })
                .collect()
        })
    }

    #[tokio::test]
    async fn execute_bash_and_wait_matches_the_ts_result_shape() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        let response = worker
            .dispatch(
                "execute_bash_and_wait",
                &json!({ "activeSessionId": "bash-session", "command": "echo hello" }),
            )
            .await;
        assert!(response.success, "failed: {response:?}");
        let data = response.data.expect("data");
        assert_eq!(data["output"], json!("hello\n"));
        assert_eq!(data["exitCode"], json!(0));
        assert_eq!(data["cancelled"], json!(false));
        assert_eq!(data["truncated"], json!(false));
        assert!(data.get("fullOutputPath").is_none(), "no spill: {data}");

        let rows = bash_rows(&worker);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["command"], json!("echo hello"));
        assert_eq!(rows[0]["output"], json!("hello\n"));
        assert_eq!(rows[0]["exitCode"], json!(0));
        assert!(rows[0].get("excludeFromContext").is_none());

        // Failures carry the exit status and the merged stderr.
        let response = worker
            .dispatch(
                "execute_bash_and_wait",
                &json!({ "activeSessionId": "bash-session", "command": "echo boom >&2; exit 3" }),
            )
            .await;
        assert!(response.success);
        let data = response.data.expect("data");
        assert_eq!(data["exitCode"], json!(3));
        assert_eq!(data["output"], json!("boom\n"));

        let response = worker
            .dispatch(
                "execute_bash_and_wait",
                &json!({ "activeSessionId": "bash-session" }),
            )
            .await;
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("execute_bash_and_wait requires a command")
        );
    }

    /// Multi-byte characters that straddle a pipe read decode intact: a per-read decode replaced
    /// both halves with U+FFFD in the stream and the row the model reads.
    #[cfg(unix)]
    #[tokio::test]
    async fn characters_split_across_reads_decode_intact() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        // 1,500 seven-byte lines cross an 8 KiB read without reaching either output cap.
        let response = worker
            .dispatch(
                "execute_bash_and_wait",
                &json!({
                    "activeSessionId": "bash-session",
                    "command": "yes '\u{20ac}\u{20ac}' | head -n 1500",
                }),
            )
            .await;
        assert!(response.success, "failed: {response:?}");
        let data = response.data.expect("data");
        let output = data["output"].as_str().expect("output");
        assert_eq!(output, "\u{20ac}\u{20ac}\n".repeat(1500));
    }

    /// A command whose shell dies by a signal reports 128 + the signal number, like a shell's
    /// `$?` (TS `normalizedExitCode`): with no exit code the model would read it as a success.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_signal_death_reports_128_plus_the_signal() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        for (command, code) in [
            ("kill -TERM $$", 143),
            ("kill -KILL $$", 137),
            ("kill -SEGV $$", 139),
        ] {
            let response = worker
                .dispatch(
                    "execute_bash_and_wait",
                    &json!({ "activeSessionId": "bash-session", "command": command }),
                )
                .await;
            assert!(response.success, "failed: {response:?}");
            assert_eq!(
                response.data,
                Some(json!({
                    "output": "",
                    "exitCode": code,
                    "cancelled": false,
                    "truncated": false,
                })),
                "{command}"
            );
        }
    }

    #[tokio::test]
    async fn an_awaited_bash_run_reports_running_to_the_connection_state() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        let runner = std::sync::Arc::clone(&worker);
        let run = tokio::spawn(async move {
            runner
                .dispatch(
                    "execute_bash_and_wait",
                    &json!({ "activeSessionId": "bash-session", "command": "sleep 1" }),
                )
                .await
        });
        let mut saw_running = false;
        for _ in 0..100 {
            let state = worker
                .dispatch(
                    "get_connection_state",
                    &json!({ "activeSessionId": "bash-session" }),
                )
                .await;
            if state.data.expect("state data")["isBashRunning"] == json!(true) {
                saw_running = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            saw_running,
            "the awaited run never reported isBashRunning to the connection state"
        );
        let response = run.await.expect("the awaited run task panicked");
        assert!(response.success, "failed: {response:?}");
        let state = worker
            .dispatch(
                "get_connection_state",
                &json!({ "activeSessionId": "bash-session" }),
            )
            .await;
        assert_eq!(
            state.data.expect("state data")["isBashRunning"],
            json!(false),
            "the settled run left the flag on"
        );
    }

    #[tokio::test]
    async fn execute_bash_claims_the_slot_and_aborts() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        let response = worker
            .dispatch(
                "execute_bash",
                &json!({
                    "activeSessionId": "bash-session",
                    "command": "sleep 2",
                    "excludeFromContext": true,
                }),
            )
            .await;
        assert!(response.success, "failed: {response:?}");
        assert!(response.data.is_none(), "responds before completion");

        let state = worker
            .dispatch(
                "get_connection_state",
                &json!({ "activeSessionId": "bash-session" }),
            )
            .await;
        assert_eq!(state.data.expect("data")["isBashRunning"], json!(true));
        let refusal = worker
            .dispatch(
                "execute_bash",
                &json!({ "activeSessionId": "bash-session", "command": "echo nope" }),
            )
            .await;
        assert!(!refusal.success);
        assert_eq!(
            refusal.error.as_deref(),
            Some("A bash command is already running")
        );

        let aborted = worker
            .dispatch("abort_bash", &json!({ "activeSessionId": "bash-session" }))
            .await;
        assert!(aborted.success, "failed: {aborted:?}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = worker
                .dispatch(
                    "get_connection_state",
                    &json!({ "activeSessionId": "bash-session" }),
                )
                .await;
            if state.data.expect("data")["isBashRunning"] == json!(false) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "bash slot never released"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let rows = bash_rows(&worker);
        assert_eq!(rows.len(), 1, "the cancelled run still records");
        assert_eq!(rows[0]["cancelled"], json!(true));
        assert_eq!(rows[0]["excludeFromContext"], json!(true));
        assert!(
            rows[0].get("exitCode").is_none(),
            "killed runs carry no exit code"
        );
    }

    /// The next `session_event` of `event_type` (bounded: a missing event fails the test).
    #[cfg(unix)]
    async fn next_session_event(
        events: &mut tokio::sync::broadcast::Receiver<Arc<crate::worker::OutboundFrame>>,
        event_type: &str,
    ) -> Value {
        let next = async {
            loop {
                let frame = events.recv().await.expect("the event stream stays live");
                if frame.outbound_type != "session_event" {
                    continue;
                }
                let outbound: Value = serde_json::from_slice(&frame.payload).expect("event JSON");
                if outbound["event"]["type"] == json!(event_type) {
                    return outbound["event"].clone();
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), next)
            .await
            .unwrap_or_else(|_| panic!("no {event_type} event arrived"))
    }

    /// The child a running user bash command forked (the shell runs `sh`, which execs `sleep`).
    /// The child holds a FIFO's write end, so the FIFO reader's EOF is the child's death.
    #[cfg(unix)]
    struct ForkedChild {
        /// Killed on drop until [`Self::assert_killed`] saw the death: a failing run leaks nothing.
        pid: Option<i32>,
        gone: Option<tokio::task::JoinHandle<()>>,
    }

    #[cfg(unix)]
    impl ForkedChild {
        async fn start(
            worker: &Worker,
            cwd: &std::path::Path,
            events: &mut tokio::sync::broadcast::Receiver<Arc<crate::worker::OutboundFrame>>,
        ) -> Self {
            let fifo = cwd.join("child-alive");
            let made = pa_types::session_scope::std_command("mkfifo")
                .arg(&fifo)
                .status()
                .expect("run mkfifo");
            assert!(made.success(), "mkfifo failed: {made}");
            let response = worker
                .dispatch(
                    "execute_bash",
                    &json!({
                        "activeSessionId": "bash-session",
                        "command": r#"sh -c 'exec 3<>child-alive; echo "ready $$"; exec sleep 60'; echo after"#,
                    }),
                )
                .await;
            assert!(response.success, "failed: {response:?}");
            let ready = next_session_event(events, "bash_output").await;
            let pid = ready["chunk"]
                .as_str()
                .and_then(|chunk| chunk.trim().strip_prefix("ready "))
                .and_then(|pid| pid.parse().ok());
            assert!(pid.is_some(), "the child reports its pid: {ready}");
            // The child opened its end before `ready`, so this open returns at once.
            let mut reader = std::fs::File::open(&fifo).expect("open the fifo");
            let gone = tokio::task::spawn_blocking(move || {
                std::io::copy(&mut reader, &mut std::io::sink()).expect("drain the fifo");
            });
            ForkedChild {
                pid,
                gone: Some(gone),
            }
        }

        async fn assert_killed(&mut self) {
            let gone = self.gone.take().expect("awaited once");
            tokio::time::timeout(std::time::Duration::from_secs(10), gone)
                .await
                .expect("the command's child is still running")
                .expect("the fifo reader joins");
            self.pid = None;
        }
    }

    #[cfg(unix)]
    impl Drop for ForkedChild {
        fn drop(&mut self) {
            if let Some(pid) = self.pid {
                let _ = pa_core::platform::process::kill_pid(
                    pid,
                    pa_core::platform::process::Signal::Kill,
                );
            }
        }
    }

    /// `abort_bash` kills every process the command started, not just the shell: a surviving
    /// child would keep running and hold the output pipes open.
    #[cfg(unix)]
    #[tokio::test]
    async fn abort_bash_kills_the_commands_child_processes() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        let mut events = worker.events.subscribe();
        let mut child = ForkedChild::start(&worker, cwd.path(), &mut events).await;
        let aborted = worker
            .dispatch("abort_bash", &json!({ "activeSessionId": "bash-session" }))
            .await;
        assert!(aborted.success, "failed: {aborted:?}");
        child.assert_killed().await;
        assert_eq!(
            next_session_event(&mut events, "bash_end").await,
            json!({
                "type": "bash_end",
                "exitCode": null,
                "cancelled": true,
                "truncated": false,
            })
        );
    }

    /// The graceful stop's abort reaches the command's children too: the worker's exit never
    /// leaves the user's processes running.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_the_running_commands_child_processes() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        let mut events = worker.events.subscribe();
        let mut child = ForkedChild::start(&worker, cwd.path(), &mut events).await;
        let stopped = worker.dispatch("shutdown", &json!({})).await;
        assert!(stopped.success, "failed: {stopped:?}");
        assert!(
            !worker.user_bash.is_running(),
            "the stop answered before the aborted run settled"
        );
        child.assert_killed().await;
    }

    /// A command that backgrounds a process settles when its shell exits: the background process
    /// holding the inherited output pipes does not hold the run open (TS `waitForChildProcess`).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_backgrounded_process_does_not_hold_the_run_open() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        let settled = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            worker.dispatch(
                "execute_bash_and_wait",
                &json!({
                    "activeSessionId": "bash-session",
                    "command": "sleep 60 & echo $! > background.pid; echo started",
                }),
            ),
        )
        .await;
        // The background process outlives the run (TS leaves it running too).
        if let Some(pid) = std::fs::read_to_string(cwd.path().join("background.pid"))
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
        {
            let _ =
                pa_core::platform::process::kill_pid(pid, pa_core::platform::process::Signal::Kill);
        }
        let response = settled.expect("the run waited on the backgrounded process");
        assert!(response.success, "failed: {response:?}");
        assert_eq!(
            response.data,
            Some(json!({
                "output": "started\n",
                "exitCode": 0,
                "cancelled": false,
                "truncated": false,
            }))
        );
    }

    #[tokio::test]
    async fn transient_execute_bash_records_nothing() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        let response = worker
            .dispatch(
                "execute_bash",
                &json!({
                    "activeSessionId": "bash-session",
                    "command": "echo transient",
                    "transient": true,
                }),
            )
            .await;
        assert!(response.success, "failed: {response:?}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = worker
                .dispatch(
                    "get_connection_state",
                    &json!({ "activeSessionId": "bash-session" }),
                )
                .await;
            if state.data.expect("data")["isBashRunning"] == json!(false) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "bash slot never released"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(bash_rows(&worker).len(), 0, "transient runs record nothing");
    }

    #[tokio::test]
    async fn execute_bash_and_wait_refuses_a_missing_cwd() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        {
            let mut core = worker.core.lock().unwrap();
            core.cwd = "/nonexistent-bash-cwd".to_string();
        }
        let response = worker
            .dispatch(
                "execute_bash_and_wait",
                &json!({ "activeSessionId": "bash-session", "command": "pwd" }),
            )
            .await;
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some(
                "Working directory does not exist: /nonexistent-bash-cwd\nCannot execute bash commands."
            )
        );
    }

    /// Readiness wait for a racing thread's entered signal: the signal is
    /// what the caller proceeds on — the deadline only bounds failure, it can
    /// never make an assertion pass.
    fn wait_for(signal: &AtomicBool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !signal.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "the racing admission never started"
            );
            std::hint::spin_loop();
            std::thread::yield_now();
        }
    }

    #[tokio::test]
    async fn the_shutdown_gate_cannot_close_between_a_streamed_claim_and_its_registration() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;

        // Gate shut: the admission's own re-check rejects, nothing claims.
        worker.core.lock().unwrap().shutdown_requested = true;
        assert!(worker.admit_streamed_bash().is_err());
        assert!(!worker.user_bash.is_running());

        // A gate that closes while a claim waits on the admission lock: the
        // gate check and the claim are one critical section, so the claim
        // cannot slip past the close (a claim the close's abort missed would
        // leave the child running past the exit).
        {
            let mut core = worker.core.lock().unwrap();
            core.shutdown_requested = false;
        }
        let mut gate = worker.core.lock().unwrap();
        // Readiness is the racing admission's own signal, not a fixed sleep:
        // once the thread is inside its call, the held gate pins it to enter
        // the critical section only after the close.
        let entered = Arc::new(AtomicBool::new(false));
        let racing = std::thread::spawn({
            let worker = Arc::clone(&worker);
            let entered = Arc::clone(&entered);
            move || {
                entered.store(true, Ordering::SeqCst);
                worker.admit_streamed_bash().is_ok()
            }
        });
        wait_for(&entered);
        assert!(
            !worker.user_bash.is_running(),
            "the gated claim must not register"
        );
        gate.shutdown_requested = true;
        drop(gate);
        let claimed = racing.join().expect("claim thread");
        assert!(!claimed, "the closed gate must reject it");
        assert!(!worker.user_bash.is_running(), "nothing may register");
    }

    #[tokio::test]
    async fn the_shutdown_gate_cannot_close_between_an_awaited_admission_and_its_registration() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;

        // Gate shut: the admission's own re-check rejects, nothing registers.
        worker.core.lock().unwrap().shutdown_requested = true;
        assert!(worker.admit_awaited_bash().is_err());
        assert!(!worker.user_bash.is_running());

        // A gate that closes while an admission waits on the admission
        // lock: the gate check and the registration are one critical
        // section, so the admission cannot slip past the close (a
        // registration the close's abort missed would leave the child
        // running past the exit).
        {
            let mut core = worker.core.lock().unwrap();
            core.shutdown_requested = false;
        }
        let mut gate = worker.core.lock().unwrap();
        // Readiness is the racing admission's own signal, not a fixed sleep
        // (same as the streamed claim's gate test).
        let entered = Arc::new(AtomicBool::new(false));
        let racing = std::thread::spawn({
            let worker = Arc::clone(&worker);
            let entered = Arc::clone(&entered);
            move || {
                entered.store(true, Ordering::SeqCst);
                worker.admit_awaited_bash().is_ok()
            }
        });
        wait_for(&entered);
        assert!(
            !worker.user_bash.is_running(),
            "the gated admission must not register"
        );
        gate.shutdown_requested = true;
        drop(gate);
        let admitted = racing.join().expect("admission thread");
        assert!(!admitted, "the closed gate must reject it");
        assert!(!worker.user_bash.is_running(), "nothing may register");
    }

    #[tokio::test]
    async fn the_close_abort_cancels_an_admitted_awaited_run_and_settles_after_it() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        let awaited = worker.admit_awaited_bash().expect("gate open");
        assert!(worker.user_bash.is_running());
        let settle = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.user_bash.abort_and_settle().await }
        });
        // The close's abort cancels the admitted run's token: the token's
        // own cancellation is the observable signal, and the bound only
        // bounds failure.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            awaited.cancellation().cancelled(),
        )
        .await
        .expect("the abort never reached the admitted token");
        // The settle still waits: the run counts until it settles.
        assert!(!settle.is_finished());
        drop(awaited);
        tokio::time::timeout(std::time::Duration::from_secs(2), settle)
            .await
            .expect("the settle completes once the run settles")
            .expect("the settle task joins");
    }

    #[tokio::test]
    async fn an_awaited_dispatch_against_a_closing_gate_records_no_row() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let (_tmp, worker) = created_worker(cwd.path()).await;
        worker.core.lock().unwrap().shutdown_requested = true;
        let response = worker
            .dispatch(
                "execute_bash_and_wait",
                &json!({ "activeSessionId": "bash-session", "command": "echo no" }),
            )
            .await;
        assert!(!response.success);
        assert_eq!(response.error.as_deref(), Some("Session is shutting down"));
        assert_eq!(
            bash_rows(&worker).len(),
            0,
            "a rejected run records nothing"
        );
        assert!(!worker.user_bash.is_running());
    }
}
