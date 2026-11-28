//! The runtime daemon a pa-cli e2e test drives: the real `prime-agent --mode daemon` (the runtime
//! daemon, the default) on an isolated agent dir, listening with wire2 on `daemon2.sock` beside
//! the daemon socket it was given, and a blocking wire2 client for the test's own probes.
//!
//! [`RuntimeDaemon::spawn`] declares the runtime socket wire2 for this process's daemon client
//! (`pa_tui::daemon_client::use_transport`), as the composition root does for the window it
//! launches, so an in-process view or interactive run given [`RuntimeDaemon::socket`] speaks
//! wire2. Dropping the daemon stops it by protocol (`daemon.shutdown`), then waits for its exit,
//! killing it only when the stop is not answered in time (a failing test must not leak it).
#![allow(
    dead_code,
    reason = "every test binary that drives the runtime daemon compiles this module but uses only the parts its scenarios need"
)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameDecoder, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::event_log::TopicFilter;
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, Since, WIRE2_VERSION};
use serde_json::Value;

/// How long one frame may take to arrive before the test fails.
const FRAME_WAIT: Duration = Duration::from_mins(1);

/// How long a stopped daemon may take to exit before it is killed.
const EXIT_WAIT: Duration = Duration::from_secs(15);

/// Env keys a test's own environment may carry that must not reach the daemon: the old worker's
/// role and leases, the daemon selection, a package dir pointing outside the fixture, and a
/// sessions dir (fleet shells export one) that would put the daemon's default sessions dir, and
/// the subagent ledger resolved against it, outside the agent dir.
pub const SCRUB_ENV: [&str; 8] = [
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER",
    "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
    "PRIME_AGENT_INTERNAL_SESSION_LEASES",
    "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID",
    "PRIME_AGENT_RUNTIME_DAEMON",
    "PI_PACKAGE_DIR",
    "PRIME_AGENT_SESSION_DIR",
    "PRIME_AGENT_CODING_AGENT_SESSION_DIR",
];

/// The runtime socket beside daemon socket `daemon_socket` (`daemon2.sock` in its directory).
#[must_use]
pub fn runtime_socket_beside(daemon_socket: &Path) -> PathBuf {
    daemon_socket.with_file_name("daemon2.sock")
}

/// A running runtime daemon, stopped by protocol when dropped.
pub struct RuntimeDaemon {
    child: Child,
    /// The daemon socket it was started with (`<dir>/daemon.sock`, where nothing listens).
    pub daemon_socket: PathBuf,
    /// The wire2 socket it listens on (`<dir>/daemon2.sock`): the one a client connects to.
    pub socket: PathBuf,
}

impl RuntimeDaemon {
    /// The runtime daemon for `dir`, its agent dir `<dir>/agent` (created), listening.
    pub fn spawn(dir: &Path) -> RuntimeDaemon {
        RuntimeDaemon::spawn_with(dir, |_| {})
    }

    /// [`RuntimeDaemon::spawn`] whose sessions' turns the faux script at `script` answers
    /// (`PRIME_AGENT_FAUX_SCRIPT`, which the daemon reads as it starts: the file must exist).
    pub fn spawn_scripted(dir: &Path, script: &Path) -> RuntimeDaemon {
        RuntimeDaemon::spawn_with(dir, |command| {
            command.env("PRIME_AGENT_FAUX_SCRIPT", script);
        })
    }

    /// [`RuntimeDaemon::spawn`] with `configure` applied to the command last (extra env, a
    /// different agent dir, a log file for its standard error).
    ///
    /// The ambient provider credentials are scrubbed from this process first
    /// ([`super::scrub_ambient_provider_credentials`]): an in-process client hands its whole
    /// environment to the session it creates.
    pub fn spawn_with(dir: &Path, configure: impl FnOnce(&mut Command)) -> RuntimeDaemon {
        super::scrub_ambient_provider_credentials();
        let daemon_socket = dir.join("daemon.sock");
        let agent_dir = dir.join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
        command
            .args(["--mode", "daemon", "--daemon-socket"])
            .arg(&daemon_socket)
            .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for key in SCRUB_ENV {
            command.env_remove(key);
        }
        configure(&mut command);
        RuntimeDaemon::start(command, daemon_socket)
    }

    /// Runs `command` (a `prime-agent --mode daemon --daemon-socket <daemon_socket>` the caller
    /// built), waits for the runtime socket to listen, and declares it wire2.
    pub fn start(mut command: Command, daemon_socket: PathBuf) -> RuntimeDaemon {
        let socket = runtime_socket_beside(&daemon_socket);
        let mut child = command.spawn().expect("spawn prime-agent --mode daemon");
        super::await_listening(&socket, &mut child);
        pa_tui::daemon_client::use_transport(&socket, pa_tui::daemon_client::Transport::Wire2);
        RuntimeDaemon {
            child,
            daemon_socket,
            socket,
        }
    }

    /// The daemon's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// A wire2 client connected to the daemon.
    #[must_use]
    pub fn client(&self) -> Wire2 {
        Wire2::connect(&self.socket)
    }

    /// Stops the daemon with `daemon.shutdown` and waits for it to exit; kills it when the stop
    /// is not answered or the exit does not come in time.
    pub fn stop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            if let Ok(mut client) = Wire2::try_connect(&self.socket) {
                let _ = client.try_call("daemon.shutdown", &serde_json::json!({}));
            }
            let deadline = Instant::now() + EXIT_WAIT;
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for RuntimeDaemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A blocking wire2 client: hello, calls, queries, publishes and subscriptions, each frame read
/// within [`FRAME_WAIT`]. Events that arrive while it waits for a reply are kept for
/// [`Wire2::event`].
pub struct Wire2 {
    stream: UnixStream,
    decoder: PrivateFrameDecoder,
    frames: std::collections::VecDeque<ServerFrame>,
    events: std::collections::VecDeque<pa_types::event_log::Event>,
    next_id: u64,
    /// The welcome's last stored sequence number.
    pub last_seq: u64,
}

impl Wire2 {
    /// Connects to `socket` and says hello.
    ///
    /// # Panics
    ///
    /// The connection or the hello fails.
    #[must_use]
    pub fn connect(socket: &Path) -> Wire2 {
        Wire2::try_connect(socket)
            .unwrap_or_else(|error| panic!("wire2 connect to {}: {error}", socket.display()))
    }

    /// [`Wire2::connect`], the failure returned.
    pub fn try_connect(socket: &Path) -> Result<Wire2, String> {
        let stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(FRAME_WAIT))
            .map_err(|error| error.to_string())?;
        let mut client = Wire2 {
            stream,
            decoder: PrivateFrameDecoder::new(DEFAULT_PRIVATE_FRAME_LIMITS),
            frames: std::collections::VecDeque::new(),
            events: std::collections::VecDeque::new(),
            next_id: 0,
            last_seq: 0,
        };
        client.send(&ClientFrame::Hello {
            version: WIRE2_VERSION,
            client: "pa-cli-e2e".to_string(),
        })?;
        match client.read_frame()? {
            ServerFrame::Welcome { last_seq, .. } => {
                client.last_seq = last_seq;
                Ok(client)
            }
            other => Err(format!("expected welcome, got {other:?}")),
        }
    }

    fn send(&mut self, frame: &ClientFrame) -> Result<(), String> {
        let header = serde_json::to_value(frame).map_err(|error| error.to_string())?;
        let bytes = encode_private_frame(&header, &[], DEFAULT_PRIVATE_FRAME_LIMITS)
            .map_err(|error| error.to_string())?;
        self.stream
            .write_all(&bytes)
            .map_err(|error| error.to_string())
    }

    /// The next frame, within [`FRAME_WAIT`].
    fn read_frame(&mut self) -> Result<ServerFrame, String> {
        let mut chunk = vec![0_u8; 16 * 1024];
        loop {
            if let Some(frame) = self.frames.pop_front() {
                return Ok(frame);
            }
            let read = self
                .stream
                .read(&mut chunk)
                .map_err(|error| format!("no wire2 frame within the wait: {error}"))?;
            if read == 0 {
                return Err("the runtime daemon closed the connection".to_string());
            }
            for frame in self
                .decoder
                .push(&chunk[..read])
                .map_err(|error| error.to_string())?
            {
                let parsed: ServerFrame =
                    serde_json::from_value(frame.header).map_err(|error| error.to_string())?;
                self.frames.push_back(parsed);
            }
        }
    }

    fn next_id(&mut self) -> String {
        self.next_id += 1;
        format!("e2e-{}", self.next_id)
    }

    /// Waits for the reply to `id`, keeping the events that arrive first.
    fn reply(&mut self, id: &str) -> Result<Outcome, String> {
        loop {
            match self.read_frame()? {
                ServerFrame::Reply {
                    id: replied,
                    outcome,
                } if replied == id => return Ok(outcome),
                ServerFrame::Event { event, .. } => self.events.push_back(event),
                ServerFrame::Closing { reason } => {
                    return Err(format!("the runtime daemon is closing: {reason}"))
                }
                ServerFrame::Welcome { .. }
                | ServerFrame::Backfilled { .. }
                | ServerFrame::Reply { .. } => {}
            }
        }
    }

    fn try_call(&mut self, method: &str, args: &Value) -> Result<Outcome, String> {
        let id = self.next_id();
        self.send(&ClientFrame::Call {
            id: id.clone(),
            method: method.to_string(),
            args: args.clone(),
        })?;
        self.reply(&id)
    }

    /// Calls `method`: its result, or its refusal's message.
    ///
    /// # Panics
    ///
    /// The connection fails.
    pub fn call(&mut self, method: &str, args: &Value) -> Result<Value, String> {
        outcome(
            self.try_call(method, args)
                .unwrap_or_else(|error| panic!("{method}: {error}")),
        )
    }

    /// Queries `name`: its result, or its refusal's message.
    ///
    /// # Panics
    ///
    /// The connection fails.
    pub fn query(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        let id = self.next_id();
        let reply = self
            .send(&ClientFrame::Query {
                id: id.clone(),
                name: name.to_string(),
                args: args.clone(),
            })
            .and_then(|()| self.reply(&id))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        outcome(reply)
    }

    /// Publishes `payload` on `topic` with the client's (owner) trust: its reply's result (an
    /// input's names the task it became), or its refusal's message.
    ///
    /// # Panics
    ///
    /// The topic is malformed or the connection fails.
    pub fn publish(&mut self, topic: &str, payload: &Value) -> Result<Value, String> {
        let id = self.next_id();
        let reply = self
            .send(&ClientFrame::Publish {
                id: id.clone(),
                topic: pa_types::event_log::Topic::new(topic.to_string()).expect("a topic"),
                payload: payload.clone(),
            })
            .and_then(|()| self.reply(&id))
            .unwrap_or_else(|error| panic!("publish on {topic}: {error}"));
        outcome(reply)
    }

    /// Runs one prompt `text` as session `session`'s turn: publishes it on the session's input
    /// and waits for its task's `agent/<id>/done`.
    ///
    /// # Panics
    ///
    /// The publish is refused, or the turn does not finish within the wait.
    pub fn run_turn(&mut self, session: &str, text: &str) -> Value {
        let done = format!("agent/{session}/done");
        self.subscribe(&done);
        let accepted = self
            .publish(
                &format!("agent/{session}/input"),
                &serde_json::json!({ "text": text }),
            )
            .unwrap_or_else(|error| panic!("the prompt {text:?} was refused: {error}"));
        let task = accepted["task"].clone();
        self.event_until(&done, |payload| task.is_null() || payload["task"] == task)
    }

    /// Subscribes `filter` live (from now); returns once the subscription is in place.
    ///
    /// # Panics
    ///
    /// The filter is malformed or the connection fails.
    pub fn subscribe(&mut self, filter: &str) {
        let sub_id = self.next_id();
        self.send(&ClientFrame::Subscribe {
            sub_id: sub_id.clone(),
            filter: TopicFilter::new(filter).expect("a topic filter"),
            since: Since::Now,
        })
        .unwrap_or_else(|error| panic!("subscribe {filter}: {error}"));
        loop {
            match self
                .read_frame()
                .unwrap_or_else(|error| panic!("subscribe {filter}: {error}"))
            {
                ServerFrame::Backfilled { sub_id: done, .. } if done == sub_id => return,
                ServerFrame::Event { event, .. } => self.events.push_back(event),
                other => panic!("subscribe {filter}: unexpected {other:?}"),
            }
        }
    }

    /// The next event on a subscribed topic, kept or live, within [`FRAME_WAIT`].
    ///
    /// # Panics
    ///
    /// None arrives in time, or the connection fails.
    pub fn event(&mut self) -> pa_types::event_log::Event {
        loop {
            if let Some(event) = self.events.pop_front() {
                return event;
            }
            match self
                .read_frame()
                .unwrap_or_else(|error| panic!("waiting for an event: {error}"))
            {
                ServerFrame::Event { event, .. } => return event,
                ServerFrame::Closing { reason } => {
                    panic!("the runtime daemon closed while an event was awaited: {reason}")
                }
                ServerFrame::Welcome { .. }
                | ServerFrame::Backfilled { .. }
                | ServerFrame::Reply { .. } => {}
            }
        }
    }

    /// Reads events until one on `topic` satisfies `done`; returns its payload. The events on
    /// `topic` before it are dropped; those on other topics are kept, in order, for the next read.
    pub fn event_until(&mut self, topic: &str, mut done: impl FnMut(&Value) -> bool) -> Value {
        let mut other = std::collections::VecDeque::new();
        loop {
            let event = self.event();
            if event.event.topic.as_str() != topic {
                other.push_back(event);
            } else if done(&event.event.payload) {
                other.append(&mut self.events);
                self.events = other;
                return event.event.payload;
            }
        }
    }
}

/// An outcome's result, or its refusal's message.
fn outcome(outcome: Outcome) -> Result<Value, String> {
    match outcome {
        Outcome::Ok { result } => Ok(result),
        Outcome::Error { message, .. } => Err(message),
    }
}
