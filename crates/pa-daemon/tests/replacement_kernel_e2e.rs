//! Replacement-flow kernel lifecycle e2e (TS parity ruling per flow): every
//! whole-runtime replacement flow (`new_session`/`switch_session`/`fork`) drops
//! the old session's kernel and gives the fresh session a COLD one (its
//! definitions are gone); tree moves (`navigate_tree`) are NOT replacements: the
//! kernel stays WARM. A missing switch target fails at the prepare, never tearing
//! the live session down.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        common::reap_tree(&mut self.child);
    }
}

fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Daemon {
    std::fs::create_dir_all(agent_dir).expect("agent dir");
    let child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        // Worker sockets land in `<TMPDIR>/prime-agent-<uid>/`, which a killed
        // worker never cleans: keep them in the test's own dir.
        .env("TMPDIR", socket.parent().expect("socket dir"))
        .arg("--agent-dir")
        .arg(agent_dir)
        // Hermetic agent dir: the ambient environment may export a real
        // agent dir; point every fallback at the test sandbox instead.
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env_remove("PRIME_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        )
        .spawn()
        .expect("spawn pa-daemon supervisor");
    Daemon { child }
}

fn wait_socket_ready(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "supervisor socket never came up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// JSONL supervisor client (command envelopes, id-matched responses).
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> (Self, Value) {
        let stream = UnixStream::connect(socket).expect("connect supervisor");
        let writer = stream.try_clone().expect("clone socket");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
        };
        let hello = client.read_line();
        (client, hello)
    }

    fn send_command(&mut self, id: &str, command: &Value) {
        self.write_line(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        }));
    }

    fn write_line(&mut self, value: &Value) {
        let mut line = serde_json::to_string(value).expect("serialize line");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        self.writer.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_mins(2);
        self.reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set timeout");
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

    fn read_response(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_mins(4);
        loop {
            assert!(Instant::now() < deadline, "no response for id {id}");
            let line = self.read_line();
            if line.get("id").and_then(Value::as_str) == Some(id) {
                return line;
            }
        }
    }
}

/// Where the cell named `name` writes its verdict.
fn receipt_path(dir: &Path, name: &str) -> PathBuf {
    let receipts = dir.join("receipts");
    std::fs::create_dir_all(&receipts).expect("receipts dir");
    receipts.join(format!("{name}.txt"))
}

/// A cell that evaluates `verdict` (a string) and writes it to the receipt `name`.
fn receipt_cell(dir: &Path, name: &str, verdict: &str) -> Value {
    let receipt = receipt_path(dir, name);
    let code = format!(
        "(define verdict {verdict})\n(fs-write {receipt:?} verdict)\n(displayln verdict)",
        receipt = receipt.to_string_lossy(),
    );
    json!({ "content": [
        { "type": "toolCall", "name": "eval", "arguments": { "code": code } },
    ] })
}

/// A faux script whose turns run one cell each: turn 1 defines `marker` and
/// writes the `seed` receipt, and each later turn writes the receipt named in
/// `probes` with whether `marker` is still defined ("warm") or not ("cold"),
/// on whatever kernel the session then owns.
fn write_faux_script(dir: &Path, probes: &[&str]) -> PathBuf {
    let script = dir.join("faux.json");
    let mut responses = vec![
        receipt_cell(dir, "seed", r#"(begin (define marker "warm") "seeded")"#),
        json!({ "text": "done" }),
    ];
    for probe in probes {
        responses.push(receipt_cell(
            dir,
            probe,
            r#"(with-handler (lambda (e) "cold") (eval 'marker))"#,
        ));
        responses.push(json!({ "text": "done" }));
    }
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": responses }).to_string(),
    )
    .expect("write faux script");
    script
}

/// Create a session on the supervisor and run one scripted turn (prompt + idle wait).
fn create_session(client: &mut Client, dir: &Path, script: &Path, id: &str) -> String {
    let sessions_dir = dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    client.send_command(
        id,
        &json!({
            "type": "create",
            "config": {
                "cwd": dir.to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response(id);
    assert_eq!(created["success"], true, "create failed: {created}");
    created["data"]["activeSessionId"]
        .as_str()
        .or_else(|| created["data"]["id"].as_str())
        .expect("active session id")
        .to_string()
}

/// Run one turn (prompt + idle wait) on the session.
fn run_turn(client: &mut Client, session_id: &str, message: &str, id: &str) {
    client.send_command(
        id,
        &json!({ "type": "prompt", "activeSessionId": session_id, "message": message }),
    );
    let prompted = client.read_response(id);
    assert_eq!(prompted["success"], true, "prompt failed: {prompted}");
    let idle_id = format!("{id}-idle");
    client.send_command(
        &idle_id,
        &json!({ "type": "wait_for_idle", "activeSessionId": session_id }),
    );
    let idle = client.read_response(&idle_id);
    assert_eq!(idle["success"], true, "wait_for_idle failed: {idle}");
}

/// A cell's receipt proves the kernel executed it; the content is the
/// cell's verdict (`seeded` for the seed, `warm`/`cold` for the probe).
fn await_receipt(dir: &Path, name: &str) -> String {
    await_receipt_text(&receipt_path(dir, name))
}

/// Poll for a kernel cell's receipt content.
fn await_receipt_text(receipt: &Path) -> String {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        if let Ok(content) = std::fs::read_to_string(receipt) {
            return content;
        }
        assert!(
            Instant::now() < deadline,
            "kernel cell receipt never appeared at {}",
            receipt.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The first user message's entry id (a fork/navigation target).
fn first_user_entry_id(client: &mut Client, session_id: &str, id: &str) -> String {
    client.send_command(
        id,
        &json!({ "type": "get_user_messages_for_forking", "activeSessionId": session_id }),
    );
    let messages = client.read_response(id);
    assert_eq!(messages["success"], true, "fork points failed: {messages}");
    messages["data"]["messages"][0]["entryId"]
        .as_str()
        .expect("first user entry id")
        .to_string()
}

/// `new_session` (TS `teardownForReplacement`): the fresh session's kernel is COLD.
#[test]
fn new_session_disposes_the_kernel_and_starts_cold() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let socket = dir.path().join("supervisor.sock");
    let script = write_faux_script(dir.path(), &["probe"]);

    let _daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let session_id = create_session(&mut client, dir.path(), &script, "c1");
    run_turn(&mut client, &session_id, "seed the marker", "t1");
    assert_eq!(await_receipt(dir.path(), "seed"), "seeded");

    // The replacement: TS disposes the old runtime first.
    client.send_command(
        "n1",
        &json!({ "type": "new_session", "activeSessionId": session_id }),
    );
    let replaced = client.read_response("n1");
    assert_eq!(replaced["success"], true, "new_session failed: {replaced}");

    // The fresh session's kernel has none of the old one's definitions.
    run_turn(&mut client, &session_id, "probe the marker", "t2");
    assert_eq!(
        await_receipt(dir.path(), "probe"),
        "cold",
        "the new session's kernel kept the old session's definitions"
    );
}

/// `switch_session` (TS `teardownForReplacement`): a prepared target replaces the runtime
/// (cold); a MISSING target fails at the prepare, leaving the live kernel untouched.
#[test]
fn switch_session_disposes_the_kernel_and_a_failed_target_keeps_it() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let socket = dir.path().join("supervisor.sock");
    let script = write_faux_script(dir.path(), &["kept", "probe"]);

    let _daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let session_id = create_session(&mut client, dir.path(), &script, "c1");
    run_turn(&mut client, &session_id, "seed the marker", "t1");
    assert_eq!(await_receipt(dir.path(), "seed"), "seeded");

    // A missing switch target fails at the prepare: the live session stays untouched.
    client.send_command(
        "s1",
        &json!({
            "type": "switch_session",
            "activeSessionId": session_id,
            "sessionPath": "/tmp/definitely-missing-replacement.jsonl",
        }),
    );
    let failed = client.read_response("s1");
    assert_eq!(
        failed["success"], false,
        "missing switch target succeeded: {failed}"
    );
    assert!(
        failed["error"]
            .as_str()
            .unwrap_or_default()
            .contains("/tmp/definitely-missing-replacement.jsonl"),
        "missing switch target error: {failed:?}"
    );
    run_turn(&mut client, &session_id, "probe the marker", "t-kept");
    assert_eq!(
        await_receipt(dir.path(), "kept"),
        "warm",
        "a failed switch tore the live kernel down"
    );

    // A prepared target replaces the runtime: a cold kernel.
    let target = dir.path().join("sessions").join("switch-target.jsonl");
    std::fs::write(
        &target,
        format!(
            "{}\n",
            json!({
                "type": "session",
                "id": "switch-target",
                "timestamp": "2026-09-21T00:00:00.000Z",
                "cwd": dir.path().to_string_lossy(),
            })
        ),
    )
    .expect("write switch target");
    client.send_command(
        "s2",
        &json!({
            "type": "switch_session",
            "activeSessionId": session_id,
            "sessionPath": target.to_string_lossy(),
        }),
    );
    let switched = client.read_response("s2");
    assert_eq!(
        switched["success"], true,
        "switch_session failed: {switched}"
    );
    run_turn(&mut client, &session_id, "probe the marker", "t2");
    assert_eq!(
        await_receipt(dir.path(), "probe"),
        "cold",
        "the switched-to session's kernel kept the old session's definitions"
    );
}

/// `fork` (TS `teardownForReplacement`): a whole-runtime replacement; the fork's kernel boots cold.
#[test]
fn fork_disposes_the_kernel_and_starts_cold() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let socket = dir.path().join("supervisor.sock");
    let script = write_faux_script(dir.path(), &["probe"]);

    let _daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let session_id = create_session(&mut client, dir.path(), &script, "c1");
    run_turn(&mut client, &session_id, "seed the marker", "t1");
    assert_eq!(await_receipt(dir.path(), "seed"), "seeded");

    // Fork before the first user message: the fork's branch is empty.
    let entry_id = first_user_entry_id(&mut client, &session_id, "f0");
    client.send_command(
        "f1",
        &json!({
            "type": "fork",
            "activeSessionId": session_id,
            "entryId": entry_id,
            "position": "before",
        }),
    );
    let forked = client.read_response("f1");
    assert_eq!(forked["success"], true, "fork failed: {forked}");
    assert_eq!(
        forked["data"]["cancelled"], false,
        "fork cancelled: {forked}"
    );

    // The fork's kernel has none of the source's definitions.
    run_turn(&mut client, &session_id, "probe the marker", "t2");
    assert_eq!(
        await_receipt(dir.path(), "probe"),
        "cold",
        "the forked session's kernel kept the source session's definitions"
    );
}

/// The switch cwd rebind (TS `createRuntime({ cwd: sessionManager.getCwd() })`): the
/// post-switch kernel's `(session-info "cwd")`, where its commands run, is the switched-to
/// session's working directory.
#[test]
fn switch_session_rebinds_the_kernel_cwd_onto_the_target_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let socket = dir.path().join("supervisor.sock");
    let alpha = dir.path().join("alpha");
    let beta = dir.path().join("beta");
    std::fs::create_dir_all(&alpha).expect("alpha dir");
    std::fs::create_dir_all(&beta).expect("beta dir");
    // One scripted turn: a cell has a command write its working directory to the receipt
    // and fails unless `(session-info "cwd")` names the same directory.
    let receipt = dir.path().join("receipts").join("cwd.txt");
    std::fs::create_dir_all(receipt.parent().expect("receipts dir")).expect("receipts dir");
    let script = dir.path().join("faux.json");
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "responses": [
                { "content": [
                    { "type": "toolCall", "name": "eval", "arguments": {
                        "code": format!(
                            "(define ran (bash {command:?}))\n(unless (equal? (trim (hash-ref ran 'output)) (session-info \"cwd\"))\n  (error \"the command ran outside the session cwd\"))\n(displayln \"cwd\")",
                            command = format!("pwd | tee '{}'", receipt.to_string_lossy()),
                        ),
                    } },
                ] },
                { "text": "done" },
            ],
        })
        .to_string(),
    )
    .expect("write faux script");

    let _daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    client.send_command(
        "c1",
        &json!({
            "type": "create",
            "config": {
                "cwd": alpha.to_string_lossy(),
                "sessionDir": sessions_dir.to_string_lossy(),
                "script": script.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    let session_id = created["data"]["activeSessionId"]
        .as_str()
        .or_else(|| created["data"]["id"].as_str())
        .expect("active session id")
        .to_string();

    // A target session file recording the OTHER directory as its cwd.
    let target = sessions_dir.join("switch-target.jsonl");
    std::fs::write(
        &target,
        format!(
            "{}\n",
            json!({
                "type": "session",
                "id": "switch-target",
                "timestamp": "2026-09-21T00:00:00.000Z",
                "cwd": beta.to_string_lossy(),
            })
        ),
    )
    .expect("write switch target");
    client.send_command(
        "s1",
        &json!({
            "type": "switch_session",
            "activeSessionId": session_id,
            "sessionPath": target.to_string_lossy(),
        }),
    );
    let switched = client.read_response("s1");
    assert_eq!(
        switched["success"], true,
        "switch_session failed: {switched}"
    );

    // The post-switch turn runs the cell on the rebuilt kernel: the cwd is the target's.
    run_turn(&mut client, &session_id, "print the cwd", "t1");
    let observed = await_receipt_text(&receipt);
    assert_eq!(
        observed.trim_end(),
        beta.to_string_lossy(),
        "the switched-to session's kernel kept the worker's original cwd"
    );
}

/// `navigate_tree` (TS `AgentSession.navigateTree`): a tree move is NOT a runtime replacement;
/// the kernel stays WARM (the marker defined before the move survives it).
#[test]
fn navigate_tree_keeps_the_kernel_warm() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let socket = dir.path().join("supervisor.sock");
    let script = write_faux_script(dir.path(), &["probe"]);

    let _daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let session_id = create_session(&mut client, dir.path(), &script, "c1");
    run_turn(&mut client, &session_id, "seed the marker", "t1");
    assert_eq!(await_receipt(dir.path(), "seed"), "seeded");

    // A branch move to the first user message: in-place context rebuild,
    // same session, same kernel.
    let entry_id = first_user_entry_id(&mut client, &session_id, "m0");
    client.send_command(
        "m1",
        &json!({
            "type": "navigate_tree",
            "activeSessionId": session_id,
            "targetId": entry_id,
        }),
    );
    let moved = client.read_response("m1");
    assert_eq!(moved["success"], true, "navigate_tree failed: {moved}");
    assert_eq!(
        moved["data"]["cancelled"], false,
        "navigate_tree cancelled: {moved}"
    );

    // The same kernel's definitions survived the move.
    run_turn(&mut client, &session_id, "probe the marker", "t2");
    assert_eq!(
        await_receipt(dir.path(), "probe"),
        "warm",
        "a tree move lost the live kernel's definitions"
    );
}
