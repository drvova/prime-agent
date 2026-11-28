//! The mock daemons' wire2 face: a listener with std's `UnixListener` API whose accepted
//! connections speak the runtime daemon's wire2 (`pa_types::wire2`) to the TUI, while the scenario
//! behind it reads and writes the old wire's JSON lines as before.
//!
//! [`UnixListener::bind`] declares its socket wire2 for the TUI's daemon client
//! (`daemon_client::use_transport`), so the client connects over its wire2 link, the transport the
//! interactive TUI runs on by default. Each accepted connection is bridged to the stream the
//! scenario gets back from `accept`:
//!
//! - **Handshake.** The client's `hello` is answered with `welcome` at once, before any later
//!   frame of the client's is read, as the runtime answers it; `daemon.identity` is answered with
//!   the fields of the scenario's `daemon_hello` (the runtime's identity) once it is written.
//! - **Requests.** Each publish, query and call reaches the scenario as the old command it stands
//!   for (`wire2_compat`'s mapping read backwards, checked by running it forwards again), under the
//!   request's id; the scenario's response is the reply, in the runtime's own terms where its call
//!   has them (`session.create`'s `{session, path, cwd}`, `schedules`' `{jobs}`). A
//!   `prompt_and_wait` (the publish that follows a fresh `agent/<id>/done` subscription) is
//!   answered with a task and that task's `done`. Subscriptions are acknowledged at once
//!   (`backfilled`, nothing replayed); dropping a feed's reaches the scenario as the old
//!   unsubscribe, whose response the TUI no longer waits for.
//! - **Events.** Each frame the scenario streams is published as the event the runtime publishes
//!   for it (`session_event` on `agent/<id>/stream` with its `meta.sequence`, `roster_update` on
//!   `runtime/roster`, and the rest of `wire2_compat`'s topic table) to the subscription that
//!   follows its topic; one nothing follows is dropped, as the runtime's live topics drop it.
//!   `session_binding` has no wire2 form (a wire2 session keeps its id) and is dropped.
//! - **End.** Either side's end ends the other's.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_tui::daemon_client::Transport;
use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameDecoder, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::daemon::DaemonCommand;
use pa_types::event_log::{
    AgentInput, Event, EventId, NewEvent, Priority, Topic, TopicFilter, Trust,
};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, SessionCreate};
use serde_json::{json, Map, Value};

/// A mock daemon's listener: std's `UnixListener`, its connections bridged to wire2.
pub struct UnixListener {
    inner: std::os::unix::net::UnixListener,
    path: PathBuf,
    /// Whether accepted connections are bridged to wire2 (every scenario's), or handed over as
    /// they are (the inbound goldens' replay, which serves each wire itself).
    bridged: bool,
}

impl UnixListener {
    /// Bind `path` and declare it a wire2 socket for the TUI's daemon client.
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let inner = std::os::unix::net::UnixListener::bind(path.as_ref())?;
        pa_tui::daemon_client::use_transport(path.as_ref(), Transport::Wire2);
        Ok(UnixListener {
            inner,
            path: path.as_ref().to_path_buf(),
            bridged: true,
        })
    }

    /// Bind `path` without the bridge: the caller declares the socket's transport and serves it.
    pub fn bind_unbridged(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(UnixListener {
            inner: std::os::unix::net::UnixListener::bind(path.as_ref())?,
            path: path.as_ref().to_path_buf(),
            bridged: false,
        })
    }

    /// Serve this socket on the old wire instead: connections are handed over as they are, and the
    /// client speaks the old wire to it. Only for a behaviour the old wire alone carries (the
    /// supervisor's `session_binding`; a wire2 session keeps its id), until the old wire goes.
    pub fn keep_old_wire(&mut self) {
        self.bridged = false;
        pa_tui::daemon_client::use_transport(&self.path, Transport::OldWire);
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    /// Accept the next client: the returned stream speaks the old wire, bridged to the client's
    /// wire2 connection.
    pub fn accept(&self) -> io::Result<(UnixStream, SocketAddr)> {
        let (client, address) = self.inner.accept()?;
        if !self.bridged {
            return Ok((client, address));
        }
        // macOS inherits O_NONBLOCK from the listener on accept; the bridge blocks.
        client.set_nonblocking(false)?;
        let (scenario, bridge) = UnixStream::pair()?;
        Bridge::start(client, bridge)?;
        Ok((scenario, address))
    }
}

/// What a scenario's response to a forwarded request becomes.
enum Pending {
    /// The reply to the request, for the old `command` it stood for.
    Reply { command: String },
    /// A `prompt_and_wait` on `session`: a task, then that task's `done`.
    Task { session: String },
    /// The response to an old unsubscribe the bridge sent on its own: the TUI waits for none.
    Dropped,
}

/// One bridged connection's state, shared by its two directions.
struct State {
    /// The scenario's `daemon_hello`, once written.
    hello: Option<Value>,
    /// `daemon.identity` queries waiting for the scenario's `daemon_hello`.
    identity_requests: Vec<String>,
    subscriptions: Vec<(String, TopicFilter)>,
    /// Sessions whose `done` was just followed: their next publish is a `prompt_and_wait`.
    fresh_done: HashSet<String>,
    pending: HashMap<String, Pending>,
    /// The last stored event's sequence number (the stored topics' `seq`).
    stored_seq: u64,
    /// The bridge's own request ids.
    own_requests: u64,
}

struct Bridge {
    /// The client's end: wire2 frames are written here.
    client: Mutex<UnixStream>,
    /// The bridge's end of the scenario's stream: old JSON lines are written here.
    scenario: Mutex<UnixStream>,
    state: Mutex<State>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Bridge {
    fn start(client: UnixStream, scenario: UnixStream) -> io::Result<()> {
        let client_reader = client.try_clone()?;
        let scenario_reader = scenario.try_clone()?;
        let bridge = Arc::new(Bridge {
            client: Mutex::new(client),
            scenario: Mutex::new(scenario),
            state: Mutex::new(State {
                hello: None,
                identity_requests: Vec::new(),
                subscriptions: Vec::new(),
                fresh_done: HashSet::new(),
                pending: HashMap::new(),
                stored_seq: 0,
                own_requests: 0,
            }),
        });
        let inbound = Arc::clone(&bridge);
        std::thread::spawn(move || {
            inbound.pump_client(client_reader);
            inbound.end();
        });
        std::thread::spawn(move || {
            bridge.pump_scenario(scenario_reader);
            bridge.end();
        });
        Ok(())
    }

    /// Either side ended: so does the other.
    fn end(&self) {
        let _ = lock(&self.client).shutdown(Shutdown::Both);
        let _ = lock(&self.scenario).shutdown(Shutdown::Both);
    }

    fn send(&self, frame: &ServerFrame) {
        let bytes = encode_private_frame(
            &serde_json::to_value(frame).expect("a frame serializes"),
            &[],
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )
        .expect("a frame encodes");
        let _ = lock(&self.client).write_all(&bytes);
    }

    fn reply(&self, id: &str, outcome: Outcome) {
        self.send(&ServerFrame::Reply {
            id: id.to_string(),
            outcome,
        });
    }

    fn to_scenario(&self, id: &str, command: &Value) {
        let mut line = serde_json::to_string(&json!({ "id": id, "command": command }))
            .expect("serialize a command");
        line.push('\n');
        let _ = lock(&self.scenario).write_all(line.as_bytes());
    }

    fn pump_client(&self, mut reader: UnixStream) {
        let mut decoder = PrivateFrameDecoder::new(DEFAULT_PRIVATE_FRAME_LIMITS);
        let mut chunk = [0u8; 8192];
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            let Ok(frames) = decoder.push(&chunk[..read]) else {
                return;
            };
            for frame in frames {
                let Ok(frame) = serde_json::from_value::<ClientFrame>(frame.header) else {
                    continue;
                };
                if let Err(error) = self.client_frame(&frame) {
                    eprintln!("wire2 mock: {error}");
                    return;
                }
            }
        }
    }

    fn client_frame(&self, frame: &ClientFrame) -> Result<(), String> {
        match frame {
            // Written by this pump, before it reads the client's next frame: nothing the bridge
            // sends the client can come ahead of it (its handshake refuses a reply before it).
            ClientFrame::Hello { .. } => self.send(&ServerFrame::Welcome {
                version: pa_types::wire2::WIRE2_VERSION,
                runtime_version: "0.0.0".to_string(),
                last_seq: 0,
            }),
            ClientFrame::Subscribe { sub_id, filter, .. } => {
                {
                    let mut state = lock(&self.state);
                    state.subscriptions.push((sub_id.clone(), filter.clone()));
                    if let Some(session) = sub_id.strip_prefix("compat:done:") {
                        state.fresh_done.insert(session.to_string());
                    }
                }
                self.send(&ServerFrame::Backfilled {
                    sub_id: sub_id.clone(),
                    through_seq: 0,
                });
            }
            ClientFrame::Unsubscribe { sub_id } => {
                lock(&self.state)
                    .subscriptions
                    .retain(|(known, _)| known != sub_id);
                let old = match sub_id.as_str() {
                    "compat:roster" => "roster_unsubscribe",
                    "compat:traffic" => "agent_traffic_unsubscribe",
                    _ => return Ok(()),
                };
                let id = {
                    let mut state = lock(&self.state);
                    state.own_requests += 1;
                    let id = format!("wire2-mock:{}", state.own_requests);
                    state.pending.insert(id.clone(), Pending::Dropped);
                    id
                };
                self.to_scenario(&id, &json!({ "type": old }));
            }
            ClientFrame::Query { id, name, .. } if name == "daemon.identity" => {
                let hello = {
                    let mut state = lock(&self.state);
                    if state.hello.is_none() {
                        state.identity_requests.push(id.clone());
                    }
                    state.hello.clone()
                };
                if let Some(hello) = hello {
                    self.reply(
                        id,
                        Outcome::Ok {
                            result: identity(hello),
                        },
                    );
                }
            }
            // No history import runs in a mock.
            ClientFrame::Query { id, name, .. } if name == "system.history_import" => {
                self.reply(
                    id,
                    Outcome::Ok {
                        result: Value::Null,
                    },
                );
            }
            ClientFrame::Publish { id, .. }
            | ClientFrame::Query { id, .. }
            | ClientFrame::Call { id, .. } => {
                let (command, pending) = self.old_command(frame)?;
                check_round_trip(id, &command, frame)?;
                lock(&self.state).pending.insert(id.clone(), pending);
                self.to_scenario(id, &command);
            }
        }
        Ok(())
    }

    /// The old command `frame` stands for, and what its response becomes.
    fn old_command(&self, frame: &ClientFrame) -> Result<(Value, Pending), String> {
        match frame {
            ClientFrame::Publish { topic, payload, .. } => {
                let levels: Vec<&str> = topic.levels().collect();
                let ["agent", session, "input"] = levels.as_slice() else {
                    return Err(format!("a publish on {topic}"));
                };
                let input: AgentInput = serde_json::from_value(payload.clone())
                    .map_err(|error| format!("{topic}: {error}"))?;
                let waits = lock(&self.state).fresh_done.remove(*session);
                let mut command = json!({
                    "type": if waits { "prompt_and_wait" } else { "prompt" },
                    "activeSessionId": session,
                    "message": input.text,
                });
                if !input.images.is_empty() {
                    command["images"] =
                        serde_json::to_value(&input.images).map_err(|e| e.to_string())?;
                }
                if !waits {
                    match input.priority {
                        Some(Priority::Steer) => {
                            command["streamingBehavior"] = json!("steer");
                        }
                        Some(Priority::FollowUp) => {
                            command["streamingBehavior"] = json!("followUp");
                        }
                        Some(Priority::Prompt | Priority::Event | Priority::Background) | None => {}
                    }
                }
                let pending = if waits {
                    Pending::Task {
                        session: (*session).to_string(),
                    }
                } else {
                    Pending::Reply {
                        command: "prompt".to_string(),
                    }
                };
                Ok((command, pending))
            }
            ClientFrame::Query { name, args, .. }
            | ClientFrame::Call {
                method: name, args, ..
            } => {
                let command = old_request(name, args)?;
                let kind = command["type"].as_str().unwrap_or_default().to_string();
                Ok((command, Pending::Reply { command: kind }))
            }
            ClientFrame::Hello { .. }
            | ClientFrame::Subscribe { .. }
            | ClientFrame::Unsubscribe { .. } => Err(format!("not a request: {frame:?}")),
        }
    }

    fn pump_scenario(&self, reader: UnixStream) {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            match frame["type"].as_str() {
                // The identity queries that waited for the scenario's hello are answered. They are
                // taken under the hold that records it: a query the client's pump reads after it
                // sees the hello and is answered there; one read before it is in the queue.
                Some("daemon_hello") => {
                    let identity = identity(frame.clone());
                    let waiting = {
                        let mut state = lock(&self.state);
                        state.hello = Some(frame);
                        std::mem::take(&mut state.identity_requests)
                    };
                    for id in waiting {
                        self.reply(
                            &id,
                            Outcome::Ok {
                                result: identity.clone(),
                            },
                        );
                    }
                }
                Some("response") => self.response(&frame),
                _ => self.publish(&frame),
            }
        }
    }

    /// The scenario's response, as the reply to the request it answers.
    fn response(&self, frame: &Value) {
        let id = frame["id"].as_str().unwrap_or_default();
        let Some(pending) = lock(&self.state).pending.remove(id) else {
            return;
        };
        let outcome = if frame["success"] == json!(true) {
            None
        } else {
            Some(Outcome::Error {
                message: frame["error"].as_str().unwrap_or_default().to_string(),
                info: frame.get("errorInfo").cloned(),
            })
        };
        let data = frame.get("data").cloned().unwrap_or(Value::Null);
        match pending {
            Pending::Dropped => {}
            Pending::Reply { command } => {
                let outcome = outcome.unwrap_or_else(|| Outcome::Ok {
                    result: wire2_result(&command, data),
                });
                self.reply(id, outcome);
            }
            Pending::Task { session } => {
                let task = format!("mock-task:{id}");
                self.reply(
                    id,
                    Outcome::Ok {
                        result: json!({ "task": task }),
                    },
                );
                let mut done = match data {
                    Value::Object(fields) => fields,
                    _ => Map::new(),
                };
                done.insert("task".to_string(), json!(task));
                match outcome {
                    Some(Outcome::Error { message, .. }) => {
                        done.insert("outcome".to_string(), json!("failed"));
                        done.insert("error".to_string(), json!(message));
                    }
                    Some(Outcome::Ok { .. }) | None => {
                        done.insert("outcome".to_string(), json!("completed"));
                    }
                }
                self.deliver(&format!("agent/{session}/done"), Value::Object(done), None);
            }
        }
    }

    /// A streamed old frame, published as the event the runtime publishes for it.
    fn publish(&self, frame: &Value) {
        let without_type = || {
            let mut payload = frame.clone();
            if let Some(fields) = payload.as_object_mut() {
                fields.remove("type");
            }
            payload
        };
        let session = frame["activeSessionId"].as_str().unwrap_or("mock");
        let id = frame["meta"]["id"].as_str().map(str::to_string);
        let (topic, payload) = match frame["type"].as_str().unwrap_or_default() {
            "session_event" => {
                let mut payload = frame["event"].clone();
                if let Some(sequence) = frame["meta"]["sequence"].as_u64() {
                    payload["meta"] = json!({ "sequence": sequence });
                }
                (format!("agent/{session}/stream"), payload)
            }
            "side_question_event" => (
                format!("agent/{session}/side_question"),
                frame["event"].clone(),
            ),
            "session_closed" => (
                format!("agent/{session}/closed"),
                json!({ "reason": frame["reason"] }),
            ),
            "roster_update" => ("runtime/roster".to_string(), without_type()),
            "agent_traffic" => ("runtime/traffic".to_string(), frame["entry"].clone()),
            "session_list_item" | "session_list_progress" => (
                format!(
                    "runtime/session_list/{}",
                    frame["id"].as_str().unwrap_or_default()
                ),
                without_type(),
            ),
            "model_catalog_changed" => ("runtime/models".to_string(), json!({})),
            "daemon_closing" => ("system/closing".to_string(), without_type()),
            "history_import" => ("system/history_import".to_string(), without_type()),
            "heartbeats_changed" => (format!("agent/{session}/schedules"), json!({})),
            other => {
                eprintln!("wire2 mock: the old `{other}` frame has no wire2 form; dropped");
                return;
            }
        };
        self.deliver(&topic, payload, id);
    }

    /// `payload` on `topic`, to the subscription that follows it (none: dropped, as live).
    fn deliver(&self, topic: &str, payload: Value, id: Option<String>) {
        let Ok(topic) = Topic::new(topic) else {
            eprintln!("wire2 mock: {topic:?} is not a topic; dropped");
            return;
        };
        let (sub_id, seq) = {
            let mut state = lock(&self.state);
            let Some(sub_id) = state
                .subscriptions
                .iter()
                .find(|(_, filter)| filter.matches(&topic))
                .map(|(sub_id, _)| sub_id.clone())
            else {
                return;
            };
            let seq = if topic.is_live_only() {
                0
            } else {
                state.stored_seq += 1;
                state.stored_seq
            };
            (sub_id, seq)
        };
        let id = id.unwrap_or_else(|| format!("mock-event-{seq}"));
        self.send(&ServerFrame::Event {
            sub_id,
            event: Event {
                id: EventId(id),
                seq,
                ts: 0,
                event: NewEvent {
                    topic,
                    source: "runtime".to_string(),
                    session: None,
                    payload,
                    trust: Trust::Owner,
                    cause: None,
                    reply: None,
                },
            },
        });
    }
}

/// The runtime's `daemon.identity` for the scenario's `daemon_hello`: its fields without the
/// frame's `type` and the old protocol.
fn identity(mut hello: Value) -> Value {
    if let Some(fields) = hello.as_object_mut() {
        fields.remove("type");
        fields.remove("protocol");
    }
    hello
}

/// The scenario's response `data` to the old `command` as the runtime's reply result: verbatim,
/// except where the runtime's own call answers in its own terms.
fn wire2_result(command: &str, data: Value) -> Value {
    match command {
        "create" => {
            let mut result = match data {
                Value::Object(fields) => fields,
                _ => Map::new(),
            };
            let session = result
                .get("activeSessionId")
                .cloned()
                .unwrap_or(Value::Null);
            let path = result.get("sessionFile").cloned().unwrap_or(Value::Null);
            let cwd = result.get("cwd").cloned().unwrap_or(Value::Null);
            result.insert("session".to_string(), session);
            result.insert("path".to_string(), path);
            result.insert("cwd".to_string(), cwd);
            Value::Object(result)
        }
        "heartbeats_list" => json!({
            "jobs": data["heartbeats"].as_array().into_iter().flatten()
                .map(|row| row["job"].clone()).collect::<Vec<_>>(),
        }),
        _ => data,
    }
}

/// The queries and calls that carry their old command's own members (plus `session`): each wire2
/// name and the old command it stands for.
const PLAIN_REQUESTS: [(&str, &str); 71] = [
    ("session.stop", "kill"),
    ("session.rename", "rename"),
    ("session.set_model", "set_model"),
    ("session.set_effort", "set_thinking_level"),
    ("session.bash", "execute_bash"),
    ("session.abort_bash", "abort_bash"),
    ("session.abort", "abort"),
    ("session.abort_and_send_queued", "abort_and_send_queued"),
    ("session.model", "get_connection_state"),
    ("session.children", "get_rlm_children"),
    ("session.usage", "get_session_stats"),
    ("session.context_tree", "get_context_tree"),
    ("session.breakdown", "get_context_breakdown"),
    ("session.tree", "get_session_tree"),
    ("session.system_prompt", "get_system_prompt"),
    ("schedules", "heartbeats_list"),
    ("runtime.roster", "roster_subscribe"),
    ("runtime.traffic", "agent_traffic_subscribe"),
    ("session.attach", "attach"),
    ("session.detach", "detach"),
    ("session.answer_question", "answer_question"),
    ("session.cycle_model", "cycle_model"),
    ("session.set_scoped_models", "set_scoped_models"),
    ("session.cycle_effort", "cycle_thinking_level"),
    ("session.set_service_tier", "set_service_tier"),
    ("session.set_transport", "set_transport"),
    ("session.set_steering_mode", "set_steering_mode"),
    ("session.set_follow_up_mode", "set_follow_up_mode"),
    ("session.set_auto_compaction", "set_auto_compaction"),
    ("session.set_auto_retry", "set_auto_retry"),
    ("session.reload", "reload"),
    (
        "session.mark_anthropic_warning_shown",
        "mark_anthropic_warning_shown",
    ),
    ("session.commands", "get_commands"),
    ("session.mcp_connections", "get_mcp_connections"),
    ("session.model_catalog", "get_model_catalog"),
    ("session.fork_messages", "get_user_messages_for_forking"),
    ("session.last_assistant_text", "get_last_assistant_text"),
    ("session.navigate_tree", "navigate_tree"),
    ("session.abort_branch_summary", "abort_branch_summary"),
    ("session.export_html", "export_html"),
    ("session.fork_entry", "fork"),
    ("session.import_jsonl", "import_jsonl"),
    ("session.mutate_queued_message", "mutate_queued_message"),
    ("session.abort_compaction", "abort_compaction"),
    ("session.abort_retry", "abort_retry"),
    ("session.start_side_question", "start_side_question"),
    ("session.abort_side_question", "abort_side_question"),
    ("session.record_better_outcome", "record_better_outcome"),
    ("usage", "get_usage"),
    ("session.kernel_bash", "list_kernel_bash"),
    ("session.kernel_bash_tail", "tail_kernel_bash"),
    ("session.kill_kernel_bash", "kill_kernel_bash"),
    ("session.factory_activity", "factory_activity"),
    ("persistent_shells", "list_persistent_shells"),
    ("persistent_shells.tail", "tail_persistent_shell"),
    ("persistent_shells.kill", "kill_persistent_shell"),
    ("agent_messages", "agent_messages_status"),
    ("agent_messages.pause", "agent_messages_pause"),
    ("agent_messages.resume", "agent_messages_resume"),
    ("agent_messages.clear", "agent_messages_clear"),
    ("session.rlm_max_depth", "get_rlm_max_depth_status"),
    ("session.set_rlm_max_depth", "set_rlm_max_depth"),
    ("session.heartbeat", "heartbeat_get"),
    ("session.set_heartbeat", "heartbeat_set"),
    ("session.update_heartbeat", "heartbeat_update"),
    ("session.manage_heartbeat", "heartbeat_manage"),
    ("daemon.shutdown", "shutdown"),
    ("daemon.restart", "restart"),
    ("daemon.prepare_update_restart", "prepare_update_restart"),
    ("daemon.update_restore_status", "update_restore_status"),
    ("daemon.commit_update_restart", "commit_update_restart"),
];

/// The old command a wire2 query or call stands for: `wire2_compat`'s mapping read backwards.
fn old_request(name: &str, args: &Value) -> Result<Value, String> {
    let mut fields = match args {
        Value::Object(fields) => fields.clone(),
        Value::Null => Map::new(),
        other => return Err(format!("{name}: arguments {other}")),
    };
    let session = fields.remove("session");
    let rename = |fields: &mut Map<String, Value>, from: &str, to: &str| {
        if let Some(value) = fields.remove(from) {
            fields.insert(to.to_string(), value);
        }
    };
    let old = match name {
        "session.create" => return old_create(args),
        "session.snapshot" => {
            fields.remove("shape");
            "get_state"
        }
        "transcript.page" => {
            fields.remove("shape");
            "get_messages"
        }
        "sessions.list" if fields.get("shape") == Some(&json!("saved")) => {
            fields.remove("shape");
            fields.remove("stream");
            "list_saved_sessions"
        }
        "sessions.list" => {
            if fields.remove("open") == Some(json!(false)) {
                fields.insert("all".to_string(), json!(true));
            }
            "list"
        }
        "session.label" => {
            rename(&mut fields, "entry", "entryId");
            "set_session_entry_label"
        }
        "session.export" => {
            rename(&mut fields, "out", "outputPath");
            "export_jsonl"
        }
        "session.cancel_child" => {
            rename(&mut fields, "child", "childId");
            "cancel_rlm_child"
        }
        "session.delete_child" => {
            rename(&mut fields, "child", "childId");
            "delete_rlm_subagent"
        }
        other => match PLAIN_REQUESTS.iter().find(|(wire2, _)| *wire2 == other) {
            Some((_, old)) => old,
            None => return Err(format!("no old command stands for `{other}`")),
        },
    };
    fields.insert("type".to_string(), json!(old));
    if let Some(session) = session {
        fields.insert("activeSessionId".to_string(), session);
    }
    Ok(Value::Object(fields))
}

/// The old `create` a `session.create` stands for: its options back in the create config.
fn old_create(args: &Value) -> Result<Value, String> {
    let create: SessionCreate =
        serde_json::from_value(args.clone()).map_err(|error| format!("session.create: {error}"))?;
    let mut config = match create.options {
        Some(Value::Object(options)) => options,
        _ => Map::new(),
    };
    if let Some(parent) = config.remove("parent") {
        config.insert("parentSessionPath".to_string(), parent["path"].clone());
        config.insert("rlmDepth".to_string(), parent["rlmDepth"].clone());
    }
    if let Some(cwd) = create.cwd {
        config.insert("cwd".to_string(), json!(cwd));
    }
    if let Some(session_dir) = create.session_dir {
        config.insert("sessionDir".to_string(), json!(session_dir));
    }
    let mut command = json!({ "type": "create", "config": config });
    if let Some(path) = create.path {
        command["sessionPath"] = json!(path);
    }
    if create.ephemeral {
        command["noSession"] = json!(true);
    }
    if let Some(launch_env) = create.launch_env {
        command["launchEnv"] = json!(launch_env);
    }
    Ok(command)
}

/// The reverse mapping's check: the old command, carried forwards by the TUI's own compat layer,
/// is the frame the client sent.
fn check_round_trip(id: &str, command: &Value, frame: &ClientFrame) -> Result<(), String> {
    let parsed: DaemonCommand = serde_json::from_value(command.clone())
        .map_err(|error| format!("{command} is no old command: {error}"))?;
    let forwards = pa_tui::wire2_request_frames(id.to_string(), parsed)
        .map_err(|error| format!("{command}: {error}"))?;
    match forwards.last() {
        Some(last) if last == frame => Ok(()),
        other => Err(format!(
            "the reverse mapping is not exact: {frame:?} read back as {command}, which is sent as {other:?}"
        )),
    }
}
