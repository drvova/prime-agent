//! [`Wire2Link`]: the ACP client's [`DaemonTransport`] on wire2, to the
//! runtime daemon's `daemon2.sock` (`pa_types::wire2`).
//!
//! Each old-wire command the ACP surface sends becomes the wire2 frames
//! that do the same:
//!
//! - `create` is `session.create {path, cwd, sessionDir, launchEnv,
//!   options}` (the create config's session options and the command's
//!   telemetry opt-out; `--no-session` asks for an `ephemeral` session);
//!   `attach` and `detach` subscribe to and unsubscribe from the session's
//!   `agent/<id>/#` (an attach answers once the subscription's `backfilled`
//!   came); `list_saved_sessions` is `sessions.list`, `get_messages`
//!   `session.context`, `get_connection_state` `session.model`,
//!   `get_available_models` `models`, `set_model` `session.set_model`,
//!   `set_thinking_level` `session.set_effort`, `get_rlm_children`
//!   `session.children`, `cancel_rlm_child` `session.cancel_child`, the input
//!   pause `session.pause_input` / `session.release_input`,
//!   `replace_acp_mcp_servers` `session.set_mcp_servers`, and
//!   `complete_owned_session` `session.stop`.
//! - `prompt_and_wait` publishes the prompt on `agent/<id>/input` as a
//!   follow-up and answers when that task's `agent/<id>/done` comes (a failed
//!   task fails the command). The publish's reply names the task, which the
//!   link keeps under the prompt's admission id, so
//!   `cancel_prompt_admission` cancels it (`session.cancel_task`), also when
//!   the cancel came before the reply.
//! - `abort_and_clear_queue` is `session.abort`, `session.clear_queue` and
//!   `session.resume_queue`: the running turn aborted, the queue emptied (its
//!   agent messages bounced to their senders, as the old worker's clear does)
//!   and running again for the next prompt. `wait_for_idle` and
//!   `wait_for_headless_completion` are `session.wait_idle`, with
//!   `"subagents"` as the command's `waitForRlmQuiescence`.
//!
//! The frames come out in the order the daemon sent them: a session's
//! `agent/<id>/stream` events as old-wire `session_event` frames, its
//! `agent/<id>/schedules` as `heartbeats_changed`, and each reply, `done`
//! and `backfilled` as the response of the request waiting for it, after
//! the events sent ahead of it.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::daemon::{DaemonCommand, DaemonResponse};
use pa_types::event_log::{AgentInput, Priority, Topic, TopicFilter};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, Since, WIRE2_VERSION};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, Mutex};

use super::transport::{DaemonTransport, ResponseWait, TransportFrame};
use crate::lock;

/// Response timeout for one bounded wire2 request.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How many `done`s of tasks no request waits for (yet) the link keeps: a
/// prompt's `done` can come before its publish's reply.
const SEEN_DONES: usize = 256;

/// The session options a create config carries (the rest of it, `cwd`,
/// `sessionDir` and the verification scripts, is not an option).
const OPTION_KEYS: &[&str] = &[
    "provider",
    "model",
    "thinking",
    "systemPrompt",
    "appendSystemPrompt",
    "skills",
    "promptTemplates",
    "autonomous",
    "executionMode",
];

type Pending = Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<DaemonResponse>>>>;

/// The `done`s the link waits for, and the ones that came first.
#[derive(Default)]
struct Dones {
    /// Task id to the id of the request waiting for its `done`.
    waiting: HashMap<String, String>,
    /// `done` payloads by task, oldest first in `order`.
    seen: HashMap<String, Value>,
    order: VecDeque<String>,
}

/// One prompt the ACP surface sent, by its admission id.
struct Admission {
    session: String,
    /// Known once the publish's reply came.
    task: Option<String>,
    /// Cancelled before the task was known.
    cancelled: bool,
}

/// A wire2 client connection to the runtime daemon.
pub(crate) struct Wire2Link {
    writer: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    dones: Arc<std::sync::Mutex<Dones>>,
    admissions: std::sync::Mutex<HashMap<String, Admission>>,
    closed: AtomicBool,
    frames: Mutex<mpsc::UnboundedReceiver<TransportFrame>>,
    next_request_id: AtomicU64,
}

impl Wire2Link {
    /// Connects to the runtime daemon on `socket_path` and says `hello` as
    /// `acp:<pid>`, the old ACP client's id (an ephemeral session's owner).
    pub(crate) async fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            pa_types::platform::transport::connect_transport(socket_path),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to the runtime daemon socket"))??;
        let (reader_half, mut writer_half) = stream.split();
        let hello = ClientFrame::Hello {
            version: WIRE2_VERSION,
            client: format!("acp:{}", std::process::id()),
        };
        writer_half.write_all(&encode(&hello)?).await?;
        writer_half.flush().await?;
        let mut reader = PrivateFrameReader::new(reader_half, DEFAULT_PRIVATE_FRAME_LIMITS);
        let welcome = tokio::time::timeout(std::time::Duration::from_secs(3), reader.read_frame())
            .await
            .map_err(|_| anyhow::anyhow!("the runtime daemon did not send its welcome"))??
            .ok_or_else(|| anyhow::anyhow!("the runtime daemon closed before its welcome"))?;
        match serde_json::from_value::<ServerFrame>(welcome.header)? {
            ServerFrame::Welcome { .. } => {}
            ServerFrame::Closing { reason } => {
                anyhow::bail!("the runtime daemon refused: {reason}")
            }
            ServerFrame::Event { .. }
            | ServerFrame::Backfilled { .. }
            | ServerFrame::Reply { .. } => {
                anyhow::bail!("the runtime daemon did not start with its welcome")
            }
        }

        let (writer, mut outbound) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(bytes) = outbound.recv().await {
                if writer_half.write_all(&bytes).await.is_err()
                    || writer_half.flush().await.is_err()
                {
                    break;
                }
            }
            let _ = writer_half.shutdown().await;
        });
        let (frame_tx, frame_rx) = mpsc::unbounded_channel::<TransportFrame>();
        let dones: Arc<std::sync::Mutex<Dones>> = Arc::default();
        {
            let dones = Arc::clone(&dones);
            tokio::spawn(async move {
                while let Ok(Some(frame)) = reader.read_frame().await {
                    let Ok(frame) = serde_json::from_value::<ServerFrame>(frame.header) else {
                        continue;
                    };
                    let Some(frame) = transport_frame(frame, &dones) else {
                        continue;
                    };
                    if frame_tx.send(frame).is_err() {
                        break;
                    }
                }
            });
        }
        Ok(Wire2Link {
            writer,
            pending: Arc::default(),
            dones,
            admissions: std::sync::Mutex::default(),
            closed: AtomicBool::new(false),
            frames: Mutex::new(frame_rx),
            next_request_id: AtomicU64::new(0),
        })
    }

    fn request_id(&self) -> String {
        format!(
            "acp-{}",
            self.next_request_id.fetch_add(1, Ordering::SeqCst) + 1
        )
    }

    /// Registers the response slot `id` (failing fast once closed).
    fn slot(&self, id: &str) -> anyhow::Result<oneshot::Receiver<DaemonResponse>> {
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id.to_string(), tx);
        // The flag is stored before the close's clear: a slot registered in
        // the race is either cleared or refused here.
        if self.closed.load(Ordering::SeqCst) {
            lock(&self.pending).remove(id);
            anyhow::bail!("the daemon connection is closed");
        }
        Ok(rx)
    }

    fn send(&self, id: &str, frame: &ClientFrame) -> anyhow::Result<()> {
        if self.writer.send(encode(frame)?).is_err() {
            lock(&self.pending).remove(id);
            anyhow::bail!("the daemon connection is closed");
        }
        Ok(())
    }

    /// Waits for the response in slot `id` as long as `wait` says.
    async fn answer(
        &self,
        id: &str,
        rx: oneshot::Receiver<DaemonResponse>,
        wait: ResponseWait,
    ) -> anyhow::Result<DaemonResponse> {
        match wait {
            ResponseWait::Bounded => {
                tokio::time::timeout(REQUEST_TIMEOUT, rx)
                    .await
                    .map_err(|_| {
                        lock(&self.pending).remove(id);
                        anyhow::anyhow!("timed out waiting for the runtime daemon response")
                    })?
            }
            ResponseWait::UntilClose => rx.await,
        }
        .map_err(|_| anyhow::anyhow!("the daemon connection closed mid-request"))
    }

    /// One query (`query`) or call: its result, or the daemon's refusal as
    /// the inner error.
    async fn request_wire(
        &self,
        kind: WireRequest,
        name: &str,
        args: Value,
        wait: ResponseWait,
    ) -> anyhow::Result<Result<Value, String>> {
        let id = self.request_id();
        let rx = self.slot(&id)?;
        let frame = match kind {
            WireRequest::Query => ClientFrame::Query {
                id: id.clone(),
                name: name.to_string(),
                args,
            },
            WireRequest::Call => ClientFrame::Call {
                id: id.clone(),
                method: name.to_string(),
                args,
            },
        };
        self.send(&id, &frame)?;
        let response = self.answer(&id, rx, wait).await?;
        Ok(outcome(response))
    }

    async fn query(&self, name: &str, args: Value) -> anyhow::Result<Result<Value, String>> {
        self.request_wire(WireRequest::Query, name, args, ResponseWait::Bounded)
            .await
    }

    async fn call(&self, name: &str, args: Value) -> anyhow::Result<Result<Value, String>> {
        self.request_wire(WireRequest::Call, name, args, ResponseWait::Bounded)
            .await
    }

    /// Subscribes to session `session`'s topics; answers once the
    /// subscription's `backfilled` came.
    async fn attach(&self, session: &str) -> anyhow::Result<Result<Value, String>> {
        let filter = match TopicFilter::new(format!("agent/{session}/#")) {
            Ok(filter) => filter,
            Err(error) => return Ok(Err(format!("{error:#}"))),
        };
        let sub_id = subscription_of(session);
        let id = backfill_slot(&sub_id);
        let rx = self.slot(&id)?;
        self.send(
            &id,
            &ClientFrame::Subscribe {
                sub_id,
                filter,
                since: Since::Now,
            },
        )?;
        self.answer(&id, rx, ResponseWait::Bounded).await?;
        Ok(Ok(Value::Null))
    }

    /// Publishes `input` on session `session`'s input and answers with its
    /// task's `done` (as long as `wait` says), the task kept under
    /// `admission` while it runs.
    async fn prompt(
        &self,
        session: &str,
        input: AgentInput,
        admission: Option<String>,
        wait: ResponseWait,
    ) -> anyhow::Result<Result<Value, String>> {
        let topic = match Topic::new(format!("agent/{session}/input")) {
            Ok(topic) => topic,
            Err(error) => return Ok(Err(format!("{error:#}"))),
        };
        if let Some(admission) = &admission {
            lock(&self.admissions).insert(
                admission.clone(),
                Admission {
                    session: session.to_string(),
                    task: None,
                    cancelled: false,
                },
            );
        }
        let answered = self.publish_and_wait(session, topic, input, admission.as_deref(), wait);
        let answered = answered.await;
        if let Some(admission) = &admission {
            lock(&self.admissions).remove(admission);
        }
        answered
    }

    async fn publish_and_wait(
        &self,
        session: &str,
        topic: Topic,
        input: AgentInput,
        admission: Option<&str>,
        wait: ResponseWait,
    ) -> anyhow::Result<Result<Value, String>> {
        let id = self.request_id();
        let rx = self.slot(&id)?;
        self.send(
            &id,
            &ClientFrame::Publish {
                id: id.clone(),
                topic,
                payload: serde_json::to_value(input)?,
            },
        )?;
        let published = match outcome(self.answer(&id, rx, ResponseWait::Bounded).await?) {
            Ok(published) => published,
            Err(error) => return Ok(Err(error)),
        };
        let Some(task) = published["task"].as_str().map(str::to_string) else {
            return Ok(Err(format!(
                "session {session}'s input was stored but no task was queued"
            )));
        };
        let cancelled = admission.is_some_and(|admission| {
            lock(&self.admissions)
                .get_mut(admission)
                .is_some_and(|known| {
                    known.task = Some(task.clone());
                    known.cancelled
                })
        });
        if cancelled {
            self.call(
                "session.cancel_task",
                json!({ "session": session, "task": task }),
            )
            .await?
            .ok();
        }
        let done = self.wait_done(&task, wait).await?;
        Ok(match done["outcome"].as_str() {
            Some("failed") => Err(done["error"]
                .as_str()
                .unwrap_or("the prompt failed")
                .to_string()),
            _ => Ok(done),
        })
    }

    /// Task `task`'s `done` payload, as long as `wait` says.
    async fn wait_done(&self, task: &str, wait: ResponseWait) -> anyhow::Result<Value> {
        let id = self.request_id();
        let rx = {
            let mut dones = lock(&self.dones);
            if let Some(done) = dones.seen.remove(task) {
                dones.order.retain(|seen| seen != task);
                return Ok(done);
            }
            let rx = self.slot(&id)?;
            dones.waiting.insert(task.to_string(), id.clone());
            rx
        };
        let answered = self.answer(&id, rx, wait).await;
        lock(&self.dones).waiting.remove(task);
        Ok(answered?.data.unwrap_or(Value::Null))
    }

    /// Cancels the prompt admitted as `admission`: its task once the
    /// publish's reply named it, else when it does.
    async fn cancel_admission(&self, admission: &str) -> anyhow::Result<Result<Value, String>> {
        let known = {
            let mut admissions = lock(&self.admissions);
            match admissions.get_mut(admission) {
                Some(known) => {
                    if known.task.is_none() {
                        known.cancelled = true;
                    }
                    known.task.clone().map(|task| (known.session.clone(), task))
                }
                None => None,
            }
        };
        match known {
            Some((session, task)) => {
                self.call(
                    "session.cancel_task",
                    json!({ "session": session, "task": task }),
                )
                .await
            }
            None => Ok(Ok(Value::Null)),
        }
    }

    /// `session.wait_idle`: the session at rest (`subagents`) or its own
    /// lane idle, answered as the old daemon's autonomous status with its
    /// `outstandingBackgroundJobs`.
    async fn wait_idle(
        &self,
        session: &str,
        subagents: bool,
        wait: ResponseWait,
    ) -> anyhow::Result<Result<Value, String>> {
        let answered = self
            .request_wire(
                WireRequest::Call,
                "session.wait_idle",
                json!({ "session": session, "subagents": subagents }),
                wait,
            )
            .await?;
        Ok(answered.map(|idle| {
            let mut status = idle["autonomous"].clone();
            if let Some(status) = status.as_object_mut() {
                status.insert(
                    "outstandingBackgroundJobs".to_string(),
                    idle["outstandingBackgroundJobs"].clone(),
                );
            }
            status
        }))
    }

    /// `session.abort`, `session.withdraw`, `session.resume_queue`.
    async fn abort_and_clear(&self, session: &str) -> anyhow::Result<Result<Value, String>> {
        let args = json!({ "session": session });
        for method in [
            "session.abort",
            "session.clear_queue",
            "session.resume_queue",
        ] {
            if let Err(error) = self.call(method, args.clone()).await? {
                return Ok(Err(error));
            }
        }
        Ok(Ok(Value::Null))
    }
}

/// Whether a wire2 request is a query or a call.
#[derive(Debug, Clone, Copy)]
enum WireRequest {
    Query,
    Call,
}

impl DaemonTransport for Wire2Link {
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per old-wire command, each its wire2 frames"
    )]
    async fn exchange(
        &self,
        command: DaemonCommand,
        wait: ResponseWait,
    ) -> anyhow::Result<DaemonResponse> {
        let session = |id: &str| json!({ "session": id });
        let (name, answered) = match command {
            DaemonCommand::Create {
                session_path,
                no_session,
                config,
                telemetry_disabled,
                launch_env,
                ..
            } => {
                let config = config.unwrap_or(Value::Null);
                let text = |key: &str| config.get(key).and_then(Value::as_str).map(str::to_string);
                let mut options: serde_json::Map<String, Value> = OPTION_KEYS
                    .iter()
                    .filter_map(|key| Some(((*key).to_string(), config.get(*key)?.clone())))
                    .collect();
                if telemetry_disabled == Some(true) {
                    options.insert("telemetryDisabled".to_string(), json!(true));
                }
                let args = pa_types::wire2::SessionCreate {
                    path: session_path,
                    cwd: text("cwd"),
                    ephemeral: no_session == Some(true),
                    session_dir: text("sessionDir"),
                    launch_env,
                    options: Some(Value::Object(options)),
                    ..pa_types::wire2::SessionCreate::default()
                };
                let created = self
                    .call("session.create", serde_json::to_value(args)?)
                    .await?;
                (
                    "create",
                    created.map(|created| {
                        json!({
                            "activeSessionId": created["session"],
                            "sessionId": created["session"],
                            "cwd": created["cwd"],
                            "sessionFile": created["path"],
                        })
                    }),
                )
            }
            DaemonCommand::Attach {
                active_session_id, ..
            } => ("attach", self.attach(&active_session_id).await?),
            DaemonCommand::Detach {
                active_session_id, ..
            } => {
                if let Some(id) = active_session_id {
                    let sub_id = subscription_of(&id);
                    self.send(&sub_id, &ClientFrame::Unsubscribe { sub_id: sub_id.clone() })?;
                }
                ("detach", Ok(Value::Null))
            }
            DaemonCommand::ListSavedSessions { .. } => (
                "list_saved_sessions",
                self.query("sessions.list", json!({})).await?,
            ),
            DaemonCommand::GetMessages {
                active_session_id, ..
            } => (
                "get_messages",
                self.query("session.context", session(&active_session_id))
                    .await?,
            ),
            DaemonCommand::GetConnectionState {
                active_session_id, ..
            } => (
                "get_connection_state",
                self.query("session.model", session(&active_session_id))
                    .await?,
            ),
            DaemonCommand::GetAvailableModels { .. } => {
                ("get_available_models", self.query("models", json!({})).await?)
            }
            DaemonCommand::SetModel {
                active_session_id,
                provider,
                model_id,
                ..
            } => (
                "set_model",
                self.call(
                    "session.set_model",
                    json!({ "session": active_session_id, "provider": provider, "modelId": model_id }),
                )
                .await?,
            ),
            DaemonCommand::SetThinkingLevel {
                active_session_id,
                level,
                ..
            } => (
                "set_thinking_level",
                self.call(
                    "session.set_effort",
                    json!({ "session": active_session_id, "level": level }),
                )
                .await?,
            ),
            DaemonCommand::GetRlmChildren {
                active_session_id, ..
            } => (
                "get_rlm_children",
                self.query("session.children", session(&active_session_id))
                    .await?,
            ),
            DaemonCommand::CancelRlmChild {
                active_session_id,
                child_id,
                ..
            } => (
                "cancel_rlm_child",
                self.call(
                    "session.cancel_child",
                    json!({ "session": active_session_id, "child": child_id }),
                )
                .await?,
            ),
            DaemonCommand::AcquireSessionInputPause {
                active_session_id,
                lease_key,
                ..
            } => (
                "acquire_session_input_pause",
                self.call(
                    "session.pause_input",
                    json!({ "session": active_session_id, "key": lease_key }),
                )
                .await?,
            ),
            DaemonCommand::ReleaseSessionInputPause {
                active_session_id,
                pause_id,
                ..
            } => (
                "release_session_input_pause",
                self.call(
                    "session.release_input",
                    json!({ "session": active_session_id, "pauseId": pause_id }),
                )
                .await?,
            ),
            DaemonCommand::ReplaceAcpMcpServers {
                active_session_id,
                owner_id,
                servers,
                ..
            } => (
                "replace_acp_mcp_servers",
                self.call(
                    "session.set_mcp_servers",
                    json!({ "session": active_session_id, "owner": owner_id, "servers": servers }),
                )
                .await?,
            ),
            DaemonCommand::CompleteOwnedSession {
                active_session_id, ..
            } => (
                "complete_owned_session",
                self.call("session.stop", session(&active_session_id))
                    .await?,
            ),
            DaemonCommand::PromptAndWait {
                active_session_id,
                message,
                input,
                ..
            } => {
                let admission = input.admission_id;
                let images = match input.images {
                    Some(images) => serde_json::from_value(images)?,
                    None => Vec::new(),
                };
                // On wire2 an empty input wakes the session over the rows
                // delivered to it; the old worker refuses an empty prompt.
                if message.is_empty() && images.is_empty() {
                    return Ok(response(
                        "prompt_and_wait",
                        Err("Prompt cannot be empty".to_string()),
                    ));
                }
                // The old prompt's `followUp` streaming behavior.
                let input = AgentInput {
                    text: message,
                    images,
                    priority: Some(Priority::FollowUp),
                };
                (
                    "prompt_and_wait",
                    self.prompt(&active_session_id, input, admission, wait)
                        .await?,
                )
            }
            DaemonCommand::CancelPromptAdmission { admission_id, .. } => (
                "cancel_prompt_admission",
                self.cancel_admission(&admission_id).await?,
            ),
            DaemonCommand::AbortAndClearQueue {
                active_session_id, ..
            } => (
                "abort_and_clear_queue",
                self.abort_and_clear(&active_session_id).await?,
            ),
            DaemonCommand::WaitForIdle {
                active_session_id,
                wait_for_rlm_quiescence,
                ..
            } => (
                "wait_for_idle",
                self.wait_idle(
                    &active_session_id,
                    wait_for_rlm_quiescence == Some(true),
                    wait,
                )
                .await?,
            ),
            DaemonCommand::WaitForHeadlessCompletion {
                active_session_id,
                wait_for_rlm_quiescence,
                ..
            } => (
                "wait_for_headless_completion",
                self.wait_idle(
                    &active_session_id,
                    wait_for_rlm_quiescence == Some(true),
                    wait,
                )
                .await?,
            ),
            // The ACP surface sends no other command; the rest of the old
            // wire's 130-odd commands have no wire2 form here.
            other => {
                let name = serde_json::to_value(&other)?["type"]
                    .as_str()
                    .unwrap_or("command")
                    .to_string();
                return Ok(response(
                    &name,
                    Err(format!("the runtime daemon does not take `{name}` from ACP")),
                ));
            }
        };
        Ok(response(name, answered))
    }

    async fn next_frame(&self) -> Option<TransportFrame> {
        self.frames.lock().await.recv().await
    }

    fn deliver(&self, response: DaemonResponse) {
        let id = response.id.clone().unwrap_or_default();
        if let Some(tx) = lock(&self.pending).remove(&id) {
            let _ = tx.send(response);
        }
    }

    fn close(&self) {
        // The flag goes up before the clear; `slot` covers the race.
        self.closed.store(true, Ordering::SeqCst);
        lock(&self.pending).clear();
    }
}

/// The frame the ACP surface reads for one wire2 server frame, if any: a
/// `done` a request waits for is that request's response, and one no
/// request waits for yet is kept for the request that will.
fn transport_frame(frame: ServerFrame, dones: &std::sync::Mutex<Dones>) -> Option<TransportFrame> {
    match frame {
        ServerFrame::Reply { id, outcome } => Some(TransportFrame::Response(match outcome {
            Outcome::Ok { result } => DaemonResponse {
                id: Some(id),
                command: String::new(),
                success: true,
                data: Some(result),
                error: None,
                error_info: None,
            },
            Outcome::Error { message, info } => DaemonResponse {
                id: Some(id),
                command: String::new(),
                success: false,
                data: None,
                error: Some(message),
                error_info: info.and_then(|info| serde_json::from_value(info).ok()),
            },
        })),
        ServerFrame::Backfilled { sub_id, .. } => Some(TransportFrame::Response(DaemonResponse {
            id: Some(backfill_slot(&sub_id)),
            command: String::new(),
            success: true,
            data: None,
            error: None,
            error_info: None,
        })),
        ServerFrame::Event { event, .. } => {
            let mut levels = event.event.topic.levels();
            let (Some("agent"), Some(session), Some(kind), None) =
                (levels.next(), levels.next(), levels.next(), levels.next())
            else {
                return None;
            };
            match kind {
                // The payload is the old `session_event.event`; its
                // `meta.sequence` is the runtime's frame number, which the
                // ACP surface does not read.
                "stream" => {
                    let mut payload = event.event.payload;
                    if let Some(payload) = payload.as_object_mut() {
                        payload.remove("meta");
                    }
                    Some(TransportFrame::Event(json!({
                        "type": "session_event",
                        "activeSessionId": session,
                        "event": payload,
                    })))
                }
                "schedules" => Some(TransportFrame::HeartbeatsChanged),
                "done" => {
                    let payload = event.event.payload;
                    let task = payload["task"].as_str()?.to_string();
                    let mut dones = lock(dones);
                    if let Some(waiter) = dones.waiting.remove(&task) {
                        return Some(TransportFrame::Response(DaemonResponse {
                            id: Some(waiter),
                            command: String::new(),
                            success: true,
                            data: Some(payload),
                            error: None,
                            error_info: None,
                        }));
                    }
                    dones.order.push_back(task.clone());
                    dones.seen.insert(task, payload);
                    while dones.order.len() > SEEN_DONES {
                        if let Some(oldest) = dones.order.pop_front() {
                            dones.seen.remove(&oldest);
                        }
                    }
                    None
                }
                _ => None,
            }
        }
        // The welcome was read at the connect; a closing ends the frames
        // with the connection.
        ServerFrame::Welcome { .. } | ServerFrame::Closing { .. } => None,
    }
}

/// A wire2 response's result, or its refusal.
fn outcome(response: DaemonResponse) -> Result<Value, String> {
    if response.success {
        Ok(response.data.unwrap_or(Value::Null))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "unknown error".to_string()))
    }
}

/// The old-wire response for command `name`.
fn response(name: &str, answered: Result<Value, String>) -> DaemonResponse {
    let (success, data, error) = match answered {
        Ok(data) => (true, Some(data), None),
        Err(error) => (false, None, Some(error)),
    };
    DaemonResponse {
        id: None,
        command: name.to_string(),
        success,
        data,
        error,
        error_info: None,
    }
}

/// The subscription a session's attach is.
fn subscription_of(session: &str) -> String {
    format!("acp:{session}")
}

/// The response slot of subscription `sub_id`'s `backfilled`.
fn backfill_slot(sub_id: &str) -> String {
    format!("backfilled:{sub_id}")
}

fn encode(frame: &ClientFrame) -> anyhow::Result<Vec<u8>> {
    encode_private_frame(
        &serde_json::to_value(frame)?,
        &[],
        DEFAULT_PRIVATE_FRAME_LIMITS,
    )
}
