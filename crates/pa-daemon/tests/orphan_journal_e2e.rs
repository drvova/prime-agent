//! Orphan-process journal e2e: a session ending without a teardown of its
//! own must not leak the processes its `bash` commands started: its
//! lifetime's end and the supervisor's reap of the journal take them out
//! when the session is seen to have crashed.
#![cfg(unix)]

mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill as signal_pid, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

fn spawn_daemon(socket: &Path, agent_dir: &Path) -> Daemon {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pa-daemon"));
    command
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        // Worker sockets land in `<TMPDIR>/prime-agent-<uid>/`, which a killed
        // worker never cleans: keep them in the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .arg("--agent-dir")
        .arg(agent_dir)
        .env_remove("PRIME_API_KEY")
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "20000",
        );
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pa-daemon supervisor");
    common::await_listening(socket, &mut child);
    Daemon { child }
}

/// The orphaning cell: a background command that starts the program
/// `/bin/sleep`, which outlives the turn, and writes its pid for the test to
/// probe by. A program, by its path: under the built-in shell a bare `sleep`
/// is the shell's own, and the job's `pid` a job number, so neither names a
/// process.
fn orphaning_cell() -> String {
    let code = "(bash \"/bin/sleep 600 & echo $! > orphan.pid; wait\" (hash 'background #t))";
    serde_json::to_string(&json!({ "code": code })).expect("serialize cell")
}

/// The file the wedging cell's native call writes once it is inside it (the
/// kernel's test seam): from then on no interrupt reaches the cell.
static WEDGE_MARKER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The sequential mock: one SSE answer per request in order.
fn spawn_mock(next: &'static AtomicUsize) -> PathBuf {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || {
                let _ = serve(stream, next);
            });
        }
    });
    PathBuf::from(url)
}

fn chunk(delta: &Value, finish_reason: Option<&str>) -> String {
    json!({
        "id": "chatcmpl-orphan",
        "object": "chat.completion.chunk",
        "created": 1_750_000_000,
        "model": "mock-1",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    })
    .to_string()
}

fn serve(mut stream: TcpStream, next: &AtomicUsize) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or_default();
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    // The scripted sequence: request 1 answers the eval tool call that
    // starts the orphaning sleep, request 2 settles the turn, request 3 is
    // the cell that crashes the session.
    let request = next.fetch_add(1, Ordering::SeqCst);
    let mut payload = String::new();
    let data = match request {
        0 => [
            chunk(
                &json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-orphan-1",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            "arguments": orphaning_cell(),
                        }
                    }]
                }),
                None,
            ),
            chunk(&json!({}), Some("tool_calls")),
        ],
        1 => [
            chunk(
                &json!({"role": "assistant", "content": "the orphan sleeps"}),
                None,
            ),
            chunk(&json!({}), Some("stop")),
        ],
        2 => [
            chunk(
                &json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-wedge-1",
                        "type": "function",
                        "function": {
                            "name": "eval",
                            // The cell sits in a native call no interrupt reaches.
                            "arguments": json!({
                                "code": format!(
                                    "(%test-block {})",
                                    serde_json::to_string(
                                        &WEDGE_MARKER
                                            .get()
                                            .expect("the test set the marker")
                                            .to_string_lossy()
                                    )
                                    .expect("a Scheme string literal")
                                ),
                            })
                            .to_string(),
                        }
                    }]
                }),
                None,
            ),
            chunk(&json!({}), Some("tool_calls")),
        ],
        _ => [
            chunk(
                &json!({"role": "assistant", "content": "nothing to do"}),
                None,
            ),
            chunk(&json!({}), Some("stop")),
        ],
    };
    for data in data {
        write!(payload, "data: {data}\n\n").expect("write to String");
    }
    payload.push_str("data: [DONE]\n\n");
    stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{payload}"
        )
        .as_bytes(),
    )
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        let stream = UnixStream::connect(socket).expect("connect");
        let writer = stream.try_clone().expect("clone");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
        };
        let hello = client.read_line();
        assert_eq!(hello["type"], "daemon_hello");
        client
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(120);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse line"),
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for supervisor line: {error}"
                    );
                }
            }
        }
    }

    fn request(&mut self, id: &str, command: &Value) -> Value {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .unwrap_or_else(|error| panic!("write command {id}: {error}"));
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

fn wait_until<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + deadline;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "condition never became true");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn create_session(client: &mut Client, id: &str, dir: &Path, agent_dir: &Path) -> String {
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let created = client.request(
        id,
        &json!({
            "type": "create",
            "name": "orphan-lane",
            // An aborted cell still on the engine after this long ends its session.
            // And its kernel has the one native call no interrupt reaches.
            "launchEnv": {
                "PRIME_AGENT_STUCK_CELL_GRACE_MS": "300",
                "PRIME_AGENT_TEST_BLOCKING_CELL_SEAM": "1",
            },
            "config": {
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions.to_string_lossy(),
                "provider": "prime-inference",
                "model": "mock-1",
            },
        }),
    );
    assert_eq!(created["success"], true, "create failed: {created}");
    created["data"]["id"]
        .as_str()
        .or_else(|| created["data"]["activeSessionId"].as_str())
        .expect("active session id")
        .to_string()
}

/// The pid of the program the orphaning cell started, once the command has
/// written it into the session's working directory.
fn orphan_pid(dir: &Path) -> i32 {
    wait_until(Duration::from_secs(30), || {
        std::fs::read_to_string(dir.join("orphan.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .filter(|pid| *pid > 0)
    })
}

fn orphan_alive(pid: i32) -> bool {
    signal_pid(Pid::from_raw(pid), None).is_ok()
}

/// Wait out the one liveness signal the feature produces: the program's
/// death. The message names the kill that failed to reap it, so a red
/// run says exactly which half of the contract leaked.
fn expect_orphan_dead(pid: i32, cause: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while orphan_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the command's program (pid {pid}) survived {cause}: the reap never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A red run must not leak `sleep 600` past the test: the program dies with
/// the guard whether or not the assertions passed.
struct OrphanGuard(i32);

impl Drop for OrphanGuard {
    fn drop(&mut self) {
        let _ = signal_pid(Pid::from_raw(self.0), Signal::SIGKILL);
    }
}

/// A crashed session runs no teardown of its own. Its lifetime's end kills
/// each command's whole group all the same (the watcher of each job is
/// dropped with the session's runtime, and the host ends the lifetime from
/// outside the stuck thread), and records it in the journal, so the
/// supervisor's reap that follows finds nothing left alive.
#[test]
fn a_crashed_session_has_the_programs_of_its_commands_reaped_by_the_supervisor() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let socket = agent_dir.join("orphan.sock");
    let url = spawn_mock(&NEXT);
    std::fs::write(
        agent_dir.join("models.json"),
        json!({
            "providers": {
                "prime-inference": {
                    "api": "openai-completions",
                    "baseUrl": url.to_string_lossy(),
                    "apiKey": "sk-orphan",
                    "models": [
                        {
                            "id": "mock-1",
                            "name": "Mock 1",
                            "api": "openai-completions",
                            "contextWindow": 128_000,
                            "maxTokens": 4096
                        }
                    ]
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
    let _daemon = spawn_daemon(&socket, &agent_dir);
    let mut client = Client::connect(&socket);
    let active_session_id = create_session(&mut client, "c1", root.path(), &agent_dir);
    let started = client.request(
        "p1",
        &json!({
            "type": "prompt_and_wait",
            "activeSessionId": active_session_id,
            "message": "start the orphan and stop",
        }),
    );
    assert_eq!(started["success"], true, "first turn failed: {started}");
    let pid = orphan_pid(root.path());
    let _guard = OrphanGuard(pid);
    assert!(
        orphan_alive(pid),
        "the orphaning sleep died before the crash: pid {pid}"
    );

    // The session ends with no teardown of its own: its next cell wedges in a
    // native call, the abort finds it still on the engine after the session's
    // grace, and the supervisor ends the session as crashed.
    let marker = WEDGE_MARKER.get_or_init(|| root.path().join("cell-blocked"));
    let wedged = client.request(
        "p2",
        &json!({
            "type": "prompt",
            "activeSessionId": active_session_id,
            "message": "wedge the cell",
        }),
    );
    assert_eq!(wedged["success"], true, "second prompt failed: {wedged}");
    wait_until(Duration::from_secs(30), || marker.exists().then_some(()));
    let aborted = client.request(
        "ab",
        &json!({ "type": "abort", "activeSessionId": active_session_id }),
    );
    assert_eq!(aborted["success"], true, "abort failed: {aborted}");
    wait_until(Duration::from_secs(30), || {
        let listed = client.request("ls", &json!({ "type": "list" }));
        listed["data"]["sessions"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|row| {
                    row["id"] == active_session_id.as_str() && row["lastCrash"]["kind"] == "stuck"
                })
            })
            .then_some(())
    });
    // The supervisor's crash handling owns the only reap: the program dies
    // when it runs, not on any timeout of ours.
    expect_orphan_dead(pid, "the session crash");
}
