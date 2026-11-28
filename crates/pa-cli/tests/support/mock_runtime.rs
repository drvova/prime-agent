//! A mock runtime daemon on wire2 (`pa_types::wire2`) for the pa-cli pty tests: each test re-executes
//! its binary as the product under test, whose interactive TUI connects over the daemon client's
//! wire2 link ([`use_wire2`], declared in the child), and the parent serves that connection here.
//!
//! The mock does what every scenario needs of the runtime: `welcome` for the client's `hello`,
//! `daemon.identity` with the scenario's capabilities, `backfilled` at once for every subscription
//! (nothing is replayed), no running history import. Each publish, query and call goes to the
//! scenario's `answer` under its wire2 name (a publish under [`PUBLISH`], its payload as `args`);
//! the old daemon's generic success, an empty result, is the default.
#![allow(
    dead_code,
    reason = "every pty test binary compiles this module but uses only the answers its scenario needs"
)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use pa_tui::daemon_client::Transport;
use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameDecoder, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, WIRE2_VERSION};
use serde_json::{json, Value};

/// The name a publish (an `agent/<id>/input` prompt) reaches the scenario's `answer` under.
pub const PUBLISH: &str = "publish";

/// One request the client sent: its wire2 query or call name ([`PUBLISH`] for a publish) and its
/// arguments (a publish's payload).
pub struct Request<'a> {
    pub name: &'a str,
    pub args: &'a Value,
}

/// How the mock answers one request.
pub enum Answer {
    /// The reply's result.
    Result(Value),
    /// A refusal: its message, and the old daemon's `errorInfo` a client can act on.
    Error {
        message: String,
        info: Option<Value>,
    },
    /// No reply at all: the client's bounded request hangs (a wedged daemon).
    Silence,
    /// The old daemon's generic success: an empty result.
    Default,
}

/// Declare `socket` a wire2 socket for this process's daemon client: the child calls it before it
/// runs the TUI.
pub fn use_wire2(socket: &Path) {
    pa_tui::daemon_client::use_transport(socket, Transport::Wire2);
}

/// The runtime's identity advertising `capabilities`.
pub fn identity(capabilities: &[&str]) -> Value {
    json!({ "serverCapabilities": capabilities, "clientId": "mock" })
}

/// The `session.create` result for the new session `s1`.
pub fn created() -> Value {
    json!({ "session": "s1", "path": "/tmp/sess-1.jsonl", "cwd": "/tmp" })
}

/// The `session.attach` result for the idle session `s1` named `session_name`: `messages` as its
/// transcript and `model` as its model. Scenarios patch the returned value for anything else.
pub fn attached(session_name: &str, model: Option<&str>, messages: &[Value]) -> Value {
    json!({
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "activeSessionId": "s1",
        "snapshot": {
            "activeSessionId": "s1",
            "summary": { "id": "s1", "cwd": "/tmp" },
            "state": {
                "activeSessionId": "s1",
                "cwd": "/tmp",
                "sessionId": "s1",
                "sessionName": session_name,
                "model": model,
                "isStreaming": false,
                "isCompacting": false,
                "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
            },
            "messages": messages,
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
        "client": { "id": "mock", "capabilities": [] },
        "lastEventSequence": 0,
        "lastEventCursor": null,
    })
}

/// The answers of one attached session: `session.create` makes `s1`, `session.attach` answers
/// `attach` (an [`attached`] result), and everything else the generic success.
pub fn session(attach: Value) -> impl Fn(&Request<'_>) -> Answer + Clone + Send + 'static {
    move |request| match request.name {
        "session.create" => Answer::Result(created()),
        "session.attach" => Answer::Result(attach.clone()),
        _ => Answer::Default,
    }
}

/// A mock runtime daemon listening on a test's socket.
pub struct MockRuntime {
    listener: UnixListener,
    identity: Value,
    /// Hears each connection's accept and every frame in and out, for a scenario's diagnostics.
    log: Option<fn(&str)>,
}

impl MockRuntime {
    /// Bind `socket`, advertising no capabilities.
    pub fn bind(socket: &Path) -> Self {
        MockRuntime {
            listener: UnixListener::bind(socket).expect("bind mock socket"),
            identity: identity(&[]),
            log: None,
        }
    }

    /// Hand the accept and every frame in and out (`[accept]`, `[req]`, `[res]`) to `log`.
    #[must_use]
    pub fn with_wire_log(mut self, log: fn(&str)) -> Self {
        self.log = Some(log);
        self
    }

    /// Advertise `capabilities` in the runtime's identity.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: &[&str]) -> Self {
        self.identity = identity(capabilities);
        self
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    /// Serve the first client to connect, until it hangs up.
    pub fn serve_first(self, answer: impl FnMut(&Request<'_>) -> Answer) {
        if let Ok((stream, _)) = self.listener.accept() {
            serve_connection(stream, &self.identity, self.log, answer);
        }
    }

    /// Serve every client that connects, each on its own thread, until the listener fails.
    pub fn serve_all<A>(self, answer: A)
    where
        A: Fn(&Request<'_>) -> Answer + Clone + Send + 'static,
    {
        serve_all(&self.listener, &self.identity, self.log, answer);
    }
}

/// Serve every client `listener` accepts, each on its own thread, until the listener fails;
/// `log` hears each connection's accept and frames.
pub fn serve_all<A>(listener: &UnixListener, identity: &Value, log: Option<fn(&str)>, answer: A)
where
    A: Fn(&Request<'_>) -> Answer + Clone + Send + 'static,
{
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let identity = identity.clone();
        let answer = answer.clone();
        std::thread::spawn(move || serve_connection(stream, &identity, log, answer));
    }
}

/// Serve one client connection until it hangs up; `log` hears its accept and frames.
pub fn serve_connection(
    stream: UnixStream,
    identity: &Value,
    log: Option<fn(&str)>,
    mut answer: impl FnMut(&Request<'_>) -> Answer,
) {
    let mut out = Out {
        stream: stream.try_clone().expect("clone mock socket"),
        log,
    };
    out.log("[accept] the child connected");
    // macOS inherits O_NONBLOCK from the listener on accept; the serve loop blocks.
    stream.set_nonblocking(false).expect("blocking mock stream");
    let mut reader = stream;
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
            out.log(&format!("[req] {}", frame.header));
            let Ok(frame) = serde_json::from_value::<ClientFrame>(frame.header) else {
                continue;
            };
            let reply = |id: &str, result: Value| ServerFrame::Reply {
                id: id.to_string(),
                outcome: Outcome::Ok { result },
            };
            let sent = match &frame {
                ClientFrame::Hello { .. } => out.send(&ServerFrame::Welcome {
                    version: WIRE2_VERSION,
                    runtime_version: "0.0.0".to_string(),
                    last_seq: 0,
                }),
                ClientFrame::Subscribe { sub_id, .. } => out.send(&ServerFrame::Backfilled {
                    sub_id: sub_id.clone(),
                    through_seq: 0,
                }),
                ClientFrame::Unsubscribe { .. } => true,
                ClientFrame::Query { id, name, .. } if name == "daemon.identity" => {
                    out.send(&reply(id, identity.clone()))
                }
                // No history import runs in a mock.
                ClientFrame::Query { id, name, .. } if name == "system.history_import" => {
                    out.send(&reply(id, Value::Null))
                }
                ClientFrame::Publish { id, payload, .. } => out.answer(
                    id,
                    answer(&Request {
                        name: PUBLISH,
                        args: payload,
                    }),
                ),
                ClientFrame::Query { id, name, args }
                | ClientFrame::Call {
                    id,
                    method: name,
                    args,
                } => out.answer(id, answer(&Request { name, args })),
            };
            if !sent {
                return;
            }
        }
    }
}

/// The connection's writing half, and the scenario's wire log.
struct Out {
    stream: UnixStream,
    log: Option<fn(&str)>,
}

impl Out {
    fn log(&self, line: &str) {
        if let Some(log) = self.log {
            log(line);
        }
    }

    /// Write `answer` as the reply to request `id`; `false` once the client is gone.
    fn answer(&mut self, id: &str, answer: Answer) -> bool {
        let outcome = match answer {
            Answer::Result(result) => Outcome::Ok { result },
            Answer::Error { message, info } => Outcome::Error { message, info },
            Answer::Silence => return true,
            Answer::Default => Outcome::Ok { result: json!({}) },
        };
        self.send(&ServerFrame::Reply {
            id: id.to_string(),
            outcome,
        })
    }

    /// Write one frame; `false` once the client is gone.
    fn send(&mut self, frame: &ServerFrame) -> bool {
        let header = serde_json::to_value(frame).expect("a frame serializes");
        self.log(&format!("[res] {header}"));
        let bytes = encode_private_frame(&header, &[], DEFAULT_PRIVATE_FRAME_LIMITS)
            .expect("a frame encodes");
        self.stream.write_all(&bytes).is_ok()
    }
}
