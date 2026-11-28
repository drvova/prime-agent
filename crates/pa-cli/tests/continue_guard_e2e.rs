//! End-to-end verifier for the print-mode active-session guard (B-11): a
//! headless `-c`/`-r` must refuse to open a session file that a live daemon
//! daemon already hosts (the runtime daemon, the default), with the exact TS
//! `SessionAlreadyActiveError` message.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use support::runtime_daemon::RuntimeDaemon;

/// Open a session file on the live runtime daemon (`session.create` naming its
/// transcript), so the print path's guard has an active session to find.
/// Returns the daemon's session id, the lease's holder.
fn make_session_active(daemon: &RuntimeDaemon, session_path: &Path, cwd: &Path) -> String {
    let created = daemon
        .client()
        .call(
            "session.create",
            &json!({
                "path": session_path.to_string_lossy(),
                "cwd": cwd.to_string_lossy(),
            }),
        )
        .unwrap_or_else(|error| panic!("create failed: {error}"));
    created["session"]
        .as_str()
        .expect("the created session's id")
        .to_string()
}

fn run_print(args: &[&str], env: &[(String, String)]) -> (String, String, i32) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("binary present");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// `-c` refuses an active session with the exact TS error; a fresh daemon
/// still continues the most recent session.
#[test]
fn print_continue_refuses_an_active_daemon_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let socket = dir.path().join("daemon.sock");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).expect("work dir");
    let env = vec![
        ("HOME".to_string(), dir.path().to_string_lossy().to_string()),
        (
            "PRIME_AGENT_CODING_AGENT_DIR".to_string(),
            agent_dir.to_string_lossy().to_string(),
        ),
        (
            "PRIME_AGENT_FAUX_SCRIPT".to_string(),
            json!({"responses": [{"text": "first turn"}]}).to_string(),
        ),
    ];

    // One persisted print session for the work cwd.
    let (stdout, stderr, code) = run_print(
        &[
            "-p",
            "--daemon-socket",
            &socket.to_string_lossy(),
            "seed the session",
        ],
        &env,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "first turn\n");

    let session_files: Vec<PathBuf> = support::saved_transcripts_in(&agent_dir, &sessions)
        .into_iter()
        .map(|(locator, _)| locator)
        .collect();
    assert_eq!(session_files.len(), 1, "one persisted session");
    let session_path = session_files[0].clone();

    // With no daemon running, -c continues the saved session.
    let (stdout, stderr, code) = run_print(
        &[
            "-p",
            "--daemon-socket",
            &socket.to_string_lossy(),
            "-c",
            "continue offline",
        ],
        &env,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "first turn\n");

    // A live daemon worker hosting the file makes -c refuse it with the
    // exact TS `SessionAlreadyActiveError` message.
    let daemon = RuntimeDaemon::spawn(dir.path());
    let active_session_id = make_session_active(&daemon, &session_path, &work);
    let (stdout, stderr, code) = run_print(
        &[
            "-p",
            "--daemon-socket",
            &socket.to_string_lossy(),
            "-c",
            "continue me",
        ],
        &env,
    );
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "stdout: {stdout}");
    // The descriptive refusal (operator-directed): the TS-identical first
    // line, then the holder's identity and the next steps.
    let first_line = format!(
        "Error: Session is already active in {active_session_id}: {}\n",
        pa_core::session::lease::canonical_session_path(&session_path).display()
    );
    assert!(
        stderr.starts_with(&first_line),
        "stderr keeps the TS refusal first line: {stderr}"
    );
    assert!(
        stderr.contains("Holder: session "),
        "the refusal identifies the holder: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "Attach to it instead: prime-agent --resume '{active_session_id}'\n"
        )),
        "the refusal suggests attaching to the live session (the holder id rides shell-quoted): {stderr}"
    );
    assert!(
        stderr.contains("the file unlocks when that session exits"),
        "the refusal names the unlock condition: {stderr}"
    );

    // The same guard applies to --resume of the active file.
    let (stdout, stderr, code) = run_print(
        &[
            "-p",
            "--daemon-socket",
            &socket.to_string_lossy(),
            "--resume",
            &session_path.to_string_lossy(),
            "resume me",
        ],
        &env,
    );
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(
        stderr.starts_with("Error: Session is already active in "),
        "stderr: {stderr}"
    );
}

/// A session file held by a live FOREIGN lease holder (this test process
/// stands in for the other product's holder) refuses `--resume`. No daemon
/// runs: the guard's lease probe must catch a holder no roster of this
/// product's daemon can see; the guard runs before the file is opened.
#[test]
fn print_resume_refuses_a_foreign_lease_holder() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let socket = dir.path().join("daemon.sock");
    let env = vec![
        ("HOME".to_string(), dir.path().to_string_lossy().to_string()),
        (
            "PRIME_AGENT_CODING_AGENT_DIR".to_string(),
            agent_dir.to_string_lossy().to_string(),
        ),
        (
            "PRIME_AGENT_FAUX_SCRIPT".to_string(),
            json!({"responses": [{"text": "first turn"}]}).to_string(),
        ),
    ];
    let session_path = sessions.join("foreign-held.jsonl");
    std::fs::write(&session_path, "{}\n").expect("session file");

    // The foreign holder: this test process takes the runtime lease, in
    // exactly the role the other product's daemon worker plays. The
    // lease-enable env is consumed at the acquire itself, so it leaves this
    // test's window immediately.
    std::env::set_var(pa_core::session::lease::SESSION_LEASES_ENABLED_ENV, "1");
    std::env::set_var(
        pa_core::session::lease::SESSION_LEASE_OWNER_ID_ENV,
        "foreign01ab3c",
    );
    let holder = pa_core::session::lease::acquire_session_lease(Some(&session_path), &agent_dir)
        .expect("lease acquire probe")
        .expect("the lease must be held");
    std::env::remove_var(pa_core::session::lease::SESSION_LEASE_OWNER_ID_ENV);
    std::env::remove_var(pa_core::session::lease::SESSION_LEASES_ENABLED_ENV);

    // The refusal: this test process runs a build of this product, so the
    // holder classification reads as another Rust build, never the TypeScript
    // product and never an unnamed process. The refusal names what the kill
    // would hit (here this test binary), and the session footer carries no
    // paren (the `{}\n` fixture file has no name).
    let holder_image = std::env::current_exe()
        .expect("own exe")
        .file_name()
        .map_or_default(|name| name.to_string_lossy().into_owned());
    let expected = format!(
        concat!(
            "Error: This session is currently open in another Rust build of Prime Agent ",
            "(active in foreign01ab3c) — another daemon or window of this product holds the file's ",
            "runtime lease.\n",
            "\n",
            "• Continue where you left off:\n",
            "  prime-agent --daemon-socket <socket> --resume 'foreign01ab3c'\n",
            "  (<socket> is that instance's daemon socket, from the shell where you started ",
            "it — that daemon owns this session)\n",
            "\n",
            "• Take over on this daemon:\n",
            "  kill {} # the holder is {}\n",
            "  Then retry — the file unlocks when the holder exits.\n",
            "\n",
            "Session: foreign01ab3c\n"
        ),
        std::process::id(),
        holder_image
    );
    let (stdout, stderr, code) = run_print(
        &[
            "-p",
            "--daemon-socket",
            &socket.to_string_lossy(),
            "--resume",
            &session_path.to_string_lossy(),
            "resume me",
        ],
        &env,
    );
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert_eq!(stderr, expected);

    holder.release();

    // With the holder gone the same open proceeds: the guard's lease
    // probe answers None for a released lease (the stale record is not
    // live ownership).
    let (_stdout, stderr, code) = run_print(
        &[
            "-p",
            "--daemon-socket",
            &socket.to_string_lossy(),
            "--resume",
            &session_path.to_string_lossy(),
            "resume me now",
        ],
        &env,
    );
    assert_eq!(
        code, 0,
        "the released holder must let the open proceed: stderr {stderr}"
    );
    assert!(!stderr.contains("This session is currently open"));
}
