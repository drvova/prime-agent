//! The inbound goldens' replay oracle: each recorded stream under `tests/goldens/inbound/` is
//! served back to the real TUI (its daemon client's decode and its event pump), and the frame the
//! scenario ends on must match the one committed beside the stream. A transport that hands the TUI
//! the same frames - the wire2 compat layer at the cutover - renders the same screens.
//!
//! # How the streams were recorded
//!
//! A debug-build TUI with `PA_TUI_RECORD_INBOUND` set (`src/inbound_recording.rs`) ran each
//! scenario's headless plan against the real daemon (`prime-agent --mode daemon`) on the faux
//! engine (`"engine": "faux"` scripts), so every line is one frame the client received,
//! `message_update` deltas already restored. The recordings were then normalized: the temporary
//! directories read `/tmp/pa-golden`, the daemon's ephemeral tokens are zeroed, and the model
//! catalog reply lists no models (the host's ambient providers are not part of any scenario).
//!
//! # How the replay serves them
//!
//! The mock answers each request with the next recorded reply to the same command (the last one
//! again once they run out; a plain success for a command the recording never answered), and it
//! advertises the recorded `daemon_hello` without the direct worker link, so everything arrives on
//! the one connection. The non-reply frames keep their recorded order and are released in blocks:
//! the frames recorded after a turn-starting reply (`attach`, `prompt`) and before the next one are
//! written right after the replay answers that reply, and those recorded between the previous
//! reply and a turn-starting one (the request's own frames, such as a queued prompt's
//! `session_action_update`) right before it. In the compared frame, times of day read `##:##`
//! and elapsed times `#` (the TUI reads them off its own clock), and the context tray row is
//! elided (it settles on whichever `get_session_stats` reply wins its race with the stream).
//!
//! # Both transports
//!
//! Every scenario is replayed twice: on the old wire, as recorded, and on wire2 (the daemon
//! client's wire2 link, `Transport::Wire2`), where a small wire2 mock serves the same recording
//! as the runtime daemon would: `welcome` and `daemon.identity` (the recorded hello's fields) for
//! the hello, each request's reply as the recorded reply to the old command it stands for (in the
//! runtime's own terms where its call has them: `session.create`'s `{session, path, cwd}`,
//! `schedules`' `{jobs}`), and each streamed frame as the event the runtime publishes for it
//! (`agent/<id>/stream` with the frame's `meta.sequence`, `runtime/roster`), to the subscription
//! that follows its topic, released at the same replies. Both must end on the committed frame.
#![cfg(unix)]

mod support;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_tui::daemon_client::Transport;
use pa_tui::interactive::{HeadlessPlan, HeadlessStep, SessionSelection};
use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameDecoder, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::event_log::{Event, EventId, NewEvent, Topic, TopicFilter, Trust};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame};
use serde_json::{json, Value};

use support::{accept_within_idle_window, run_headless, write_json, UnixListener};

/// The replies that start a block of streamed frames.
const TURN_STARTS: [&str; 2] = ["attach", "prompt"];

/// One recorded stream, split the way the replay serves it.
struct Recording {
    hello: Value,
    /// The recorded replies per command, in arrival order.
    replies: HashMap<String, Vec<Value>>,
    /// The streamed frames around each turn-starting reply, in order.
    blocks: Vec<Block>,
}

/// The streamed frames one turn-starting request releases.
#[derive(Default)]
struct Block {
    /// Recorded after the previous reply and before this one: the request's own frames (a queued
    /// prompt's `session_action_update`), written before the reply.
    before: Vec<Value>,
    /// Recorded after the reply, up to the next turn-starting request's own frames.
    after: Vec<Value>,
}

impl Recording {
    fn load(name: &str) -> Self {
        let path = format!(
            "{}/tests/goldens/inbound/{name}.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        let mut hello = Value::Null;
        let mut replies: HashMap<String, Vec<Value>> = HashMap::new();
        let mut blocks: Vec<Block> = Vec::new();
        // How many streamed frames arrived since the last reply of any command.
        let mut since_reply = 0;
        for line in text.lines() {
            let mut recorded: Value = serde_json::from_str(line).expect("a recorded line is JSON");
            let frame = recorded["frame"].take();
            match frame["type"].as_str() {
                Some("daemon_hello") => hello = frame,
                Some("response") => {
                    let command = frame["command"].as_str().unwrap_or_default().to_string();
                    if TURN_STARTS.contains(&command.as_str()) {
                        let before = blocks.last_mut().map_or_else(Vec::new, |block| {
                            block.after.split_off(block.after.len() - since_reply)
                        });
                        blocks.push(Block {
                            before,
                            after: Vec::new(),
                        });
                    }
                    since_reply = 0;
                    replies.entry(command).or_default().push(frame);
                }
                _ => {
                    blocks
                        .last_mut()
                        .unwrap_or_else(|| panic!("{name}: a streamed frame before the attach"))
                        .after
                        .push(frame);
                    since_reply += 1;
                }
            }
        }
        let capabilities = hello["serverCapabilities"]
            .as_array_mut()
            .expect("the recorded hello's capabilities");
        capabilities.retain(|capability| capability != "direct_peer_transport");
        Recording {
            hello,
            replies,
            blocks,
        }
    }
}

/// The replay's position in a recording, shared by every connection the TUI opens.
struct Replay {
    recording: Recording,
    answered: HashMap<String, usize>,
    released: usize,
}

impl Replay {
    /// Answer one request on the old wire and release the frames its reply starts.
    fn answer(&mut self, writer: &mut UnixStream, id: &str, command: &str) {
        self.answer_with(id, command, |frame| write_json(writer, frame));
    }

    /// Answer one request: `write` gets the frames its reply starts and the reply, in order.
    fn answer_with(&mut self, id: &str, command: &str, mut write: impl FnMut(&Value)) {
        let index = self.answered.entry(command.to_string()).or_default();
        let reply = self
            .recording
            .replies
            .get(command)
            .and_then(|replies| replies.get(*index).or_else(|| replies.last()).cloned());
        *index += 1;
        let mut reply = reply.unwrap_or_else(
            || json!({ "type": "response", "command": command, "success": true, "data": {} }),
        );
        reply["id"] = json!(id);
        if !TURN_STARTS.contains(&command) {
            write(&reply);
            return;
        }
        self.released += 1;
        let block = self.recording.blocks.get(self.released - 1);
        let empty = Block::default();
        let block = block.unwrap_or(&empty);
        for frame in &block.before {
            write(frame);
        }
        write(&reply);
        for frame in &block.after {
            write(frame);
        }
    }
}

fn serve(stream: UnixStream, replay: &Mutex<Replay>) {
    let mut writer = stream.try_clone().expect("clone mock socket");
    let hello = replay.lock().unwrap().recording.hello.clone();
    write_json(&mut writer, &hello);
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(envelope) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let id = envelope["id"].as_str().unwrap_or_default();
        let command = envelope["command"]["type"].as_str().unwrap_or_default();
        replay.lock().unwrap().answer(&mut writer, id, command);
    }
}

/// The old command a wire2 request stands for: the compat layer's mapping read backwards, for the
/// requests the scenarios send.
fn old_command(frame: &ClientFrame) -> Option<&'static str> {
    Some(match frame {
        ClientFrame::Publish { .. } => "prompt",
        ClientFrame::Query { name, args, .. } => match (name.as_str(), args["shape"].as_str()) {
            ("runtime.roster", _) => "roster_subscribe",
            ("schedules", _) => "heartbeats_list",
            ("session.kernel_bash", _) => "list_kernel_bash",
            ("session.model_catalog", _) => "get_model_catalog",
            ("session.snapshot", Some("state")) => "get_state",
            ("session.commands", _) => "get_commands",
            ("transcript.page", _) => "get_messages",
            ("session.usage", _) => "get_session_stats",
            _ => return None,
        },
        ClientFrame::Call { method, .. } => match method.as_str() {
            "session.create" => "create",
            "session.attach" => "attach",
            "session.detach" => "detach",
            _ => return None,
        },
        ClientFrame::Hello { .. }
        | ClientFrame::Subscribe { .. }
        | ClientFrame::Unsubscribe { .. } => return None,
    })
}

/// A recorded reply as the runtime daemon's: its `data` as the result (in the runtime's terms
/// where its call has them), a failure as the error with its `errorInfo`.
fn wire2_outcome(command: &str, reply: &Value) -> Outcome {
    if reply["success"] != json!(true) {
        return Outcome::Error {
            message: reply["error"].as_str().unwrap_or_default().to_string(),
            info: reply.get("errorInfo").cloned(),
        };
    }
    let data = reply.get("data").cloned().unwrap_or(Value::Null);
    let result = match command {
        "create" => json!({ "session": data["activeSessionId"], "path": data["sessionFile"],
                            "cwd": data["cwd"] }),
        "heartbeats_list" => json!({
            "jobs": data["heartbeats"].as_array().into_iter().flatten()
                .map(|row| row["job"].clone()).collect::<Vec<_>>(),
        }),
        _ => data,
    };
    Outcome::Ok { result }
}

/// A streamed old frame as the event the runtime daemon publishes for it, if it publishes one.
fn wire2_event(frame: &Value) -> Option<Event> {
    let (topic, payload) = match frame["type"].as_str()? {
        "session_event" => {
            let mut payload = frame["event"].clone();
            if let Some(sequence) = frame["meta"]["sequence"].as_u64() {
                payload["meta"] = json!({ "sequence": sequence });
            }
            (
                format!("agent/{}/stream", frame["activeSessionId"].as_str()?),
                payload,
            )
        }
        "roster_update" => {
            let mut payload = frame.clone();
            payload.as_object_mut()?.remove("type");
            ("runtime/roster".to_string(), payload)
        }
        _ => return None,
    };
    Some(Event {
        id: EventId(frame["meta"]["id"].as_str().unwrap_or("live-0").to_string()),
        seq: 0,
        ts: 0,
        event: NewEvent {
            topic: Topic::new(topic).ok()?,
            source: "runtime".to_string(),
            session: None,
            payload,
            trust: Trust::Owner,
            cause: None,
            reply: None,
        },
    })
}

fn write_frame(writer: &mut UnixStream, frame: &ServerFrame) {
    let bytes = encode_private_frame(
        &serde_json::to_value(frame).expect("a frame serializes"),
        &[],
        DEFAULT_PRIVATE_FRAME_LIMITS,
    )
    .expect("a frame encodes");
    writer.write_all(&bytes).expect("write mock frame");
}

/// Serve one wire2 connection from the replay: the recorded hello as the runtime's identity, each
/// request answered as [`Replay::answer_with`] answers its old command.
fn serve_wire2(stream: UnixStream, replay: &Mutex<Replay>) {
    let mut writer = stream.try_clone().expect("clone mock socket");
    let mut reader = stream;
    let mut decoder = PrivateFrameDecoder::new(DEFAULT_PRIVATE_FRAME_LIMITS);
    let mut subscriptions: Vec<(String, TopicFilter)> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let Ok(frames) = decoder.push(&chunk[..read]) else {
            break;
        };
        for frame in frames {
            let Ok(frame) = serde_json::from_value::<ClientFrame>(frame.header) else {
                continue;
            };
            let answer_frame = |id: &str, outcome: Outcome| ServerFrame::Reply {
                id: id.to_string(),
                outcome,
            };
            match &frame {
                ClientFrame::Hello { .. } => write_frame(
                    &mut writer,
                    &ServerFrame::Welcome {
                        version: pa_types::wire2::WIRE2_VERSION,
                        runtime_version: "0.0.0".to_string(),
                        last_seq: 0,
                    },
                ),
                ClientFrame::Subscribe { sub_id, filter, .. } => {
                    subscriptions.push((sub_id.clone(), filter.clone()));
                    write_frame(
                        &mut writer,
                        &ServerFrame::Backfilled {
                            sub_id: sub_id.clone(),
                            through_seq: 0,
                        },
                    );
                }
                ClientFrame::Unsubscribe { sub_id } => {
                    subscriptions.retain(|(known, _)| known != sub_id);
                }
                ClientFrame::Query { id, name, .. } if name == "daemon.identity" => {
                    let mut identity = replay.lock().unwrap().recording.hello.clone();
                    if let Some(fields) = identity.as_object_mut() {
                        fields.remove("type");
                        fields.remove("protocol");
                    }
                    write_frame(
                        &mut writer,
                        &answer_frame(id, Outcome::Ok { result: identity }),
                    );
                }
                ClientFrame::Query { id, name, .. } if name == "system.history_import" => {
                    write_frame(
                        &mut writer,
                        &answer_frame(
                            id,
                            Outcome::Ok {
                                result: Value::Null,
                            },
                        ),
                    );
                }
                ClientFrame::Publish { id, .. }
                | ClientFrame::Query { id, .. }
                | ClientFrame::Call { id, .. } => {
                    let command = old_command(&frame).unwrap_or("unmapped");
                    replay.lock().unwrap().answer_with(id, command, |old| {
                        if old["type"] == "response" {
                            write_frame(
                                &mut writer,
                                &answer_frame(id, wire2_outcome(command, old)),
                            );
                            return;
                        }
                        let Some(event) = wire2_event(old) else {
                            return;
                        };
                        if let Some((sub_id, _)) = subscriptions
                            .iter()
                            .find(|(_, filter)| filter.matches(&event.event.topic))
                        {
                            write_frame(
                                &mut writer,
                                &ServerFrame::Event {
                                    sub_id: sub_id.clone(),
                                    event,
                                },
                            );
                        }
                    });
                }
            }
        }
    }
}

/// Replay `name` under `steps` on `transport` and return its final frame, clock readings masked.
fn replay(
    name: &str,
    session: SessionSelection,
    steps: Vec<HeadlessStep>,
    transport: Transport,
) -> String {
    let replay = Arc::new(Mutex::new(Replay {
        recording: Recording::load(name),
        answered: HashMap::new(),
        released: 0,
    }));
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        },
        |socket| pa_tui::interactive::InteractiveOptions {
            session,
            ..support::options(socket)
        },
        |socket| {
            let listener = UnixListener::bind_unbridged(socket).expect("bind the replay socket");
            pa_tui::daemon_client::use_transport(socket, transport);
            move || {
                let mut connections = Vec::new();
                while let Some(stream) = accept_within_idle_window(&listener) {
                    let replay = Arc::clone(&replay);
                    connections.push(std::thread::spawn(move || match transport {
                        Transport::OldWire => serve(stream, &replay),
                        Transport::Wire2 => serve_wire2(stream, &replay),
                    }));
                }
                for connection in connections {
                    connection.join().expect("replay connection");
                }
            }
        },
    );
    let clock = regex::Regex::new(r"\b\d{2}:\d{2}\b").expect("the clock pattern");
    let elapsed = regex::Regex::new(r"\b\d+(?:ms|s)\b").expect("the elapsed pattern");
    let frame = outcome.frames.last().expect("a rendered frame");
    let mut compared = String::new();
    for row in frame.lines() {
        // The context tray reads `get_session_stats` replies, which race the turn's own
        // context readings: what it ends on is the query's timing, not the stream's.
        if row.starts_with("\u{2190} manage") {
            compared.push_str("\u{2190} manage (context tray)\n");
            continue;
        }
        let row = clock.replace_all(row, "##:##");
        let row = elapsed.replace_all(&row, "#");
        compared.push_str(row.trim_end());
        compared.push('\n');
    }
    compared
}

fn expected(name: &str) -> String {
    let path = format!(
        "{}/tests/goldens/inbound/{name}.frame.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn idle() -> HeadlessStep {
    HeadlessStep::WaitIdle { timeout_ms: 30_000 }
}

fn render(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 30_000,
    }
}

fn submit(text: &str) -> HeadlessStep {
    HeadlessStep::Submit(text.to_string())
}

#[test]
fn recorded_streams_render_their_golden_frames() {
    let scenarios = [
        (
            "prompt_stream",
            SessionSelection::New,
            // The title's `$—` (an unpriced call) comes from the roster's first usage reading,
            // the `roster_update` the daemon sends after `agent_end`: the turn reads idle
            // before that frame lands, so the replay waits for the marker it draws.
            vec![submit("hello"), idle(), render("$\u{2014}")],
        ),
        (
            "eval_cell",
            SessionSelection::New,
            vec![submit("compute it"), idle()],
        ),
        (
            "compaction",
            SessionSelection::New,
            vec![
                submit("hi"),
                idle(),
                submit("and again"),
                idle(),
                submit("/compact"),
                render("Context compacted"),
                idle(),
            ],
        ),
        (
            "subagent_roster",
            SessionSelection::New,
            vec![submit("spawn a helper"), idle()],
        ),
        (
            "attach_paging",
            SessionSelection::Resume(PathBuf::from(
                "/tmp/pa-golden/seed/01a21012-ef57-724d-ac04-3e4bd2df1794.jsonl",
            )),
            vec![
                render("q260 asked"),
                HeadlessStep::ScrollTop,
                render("q001 asked"),
            ],
        ),
    ];
    let mut rendered = Vec::new();
    let mut goldens = Vec::new();
    for transport in [Transport::OldWire, Transport::Wire2] {
        for (name, session, steps) in scenarios.clone() {
            rendered.push((transport, name, replay(name, session, steps, transport)));
            goldens.push((transport, name, expected(name)));
        }
    }
    assert_eq!(rendered, goldens);
}
