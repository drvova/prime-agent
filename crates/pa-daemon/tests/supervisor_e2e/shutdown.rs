//! The supervisor process-exit paths - the shutdown command and the SIGTERM
//! graceful drain - with the /proc process-tree helpers they lean on.

use super::*;

fn child_pids_of(ppid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    let entries = std::fs::read_dir("/proc").expect("read /proc");
    for entry in entries.flatten() {
        let Ok(entry_pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{entry_pid}/stat")) else {
            continue;
        };
        // `comm` can contain spaces and parens, so parse after the last ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        fields.next(); // process state
        let Ok(parent) = fields.next().unwrap_or_default().parse::<u32>() else {
            continue;
        };
        if parent == ppid {
            pids.push(entry_pid);
        }
    }
    pids
}

fn wait_child_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The shutdown must stop every session and exit the supervisor, cleaning
/// up its socket: a supervisor parked on its socket blocks replacement.
#[test]
fn shutdown_command_exits_the_supervisor_process() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).expect("sessions dir");
    let mut daemon = spawn_daemon(&socket, &agent_dir);
    let supervisor_pid = daemon.child.id();
    let (mut client, _hello) = Client::connect(&socket);

    let script_path = dir.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::json!({ "responses": [ { "text": "x" } ] }).to_string(),
    )
    .expect("write script");
    client.send_command(
        "c1",
        &serde_json::json!({
            "type": "create",
            "config": {
                "cwd": dir.path().to_string_lossy(),
                "sessionDir": agent_dir.join("sessions").to_string_lossy(),
                "script": script_path.to_string_lossy(),
            },
        }),
    );
    let created = client.read_response("c1");
    assert_eq!(created["success"], true, "create failed: {created}");
    // The session runs inside the supervisor: it serves its own socket and
    // holds its lease, and the supervisor has no child process for it.
    let session_id = created["data"]["id"].as_str().expect("session id");
    let descriptor: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            pa_daemon::descriptor::descriptor_dir(&agent_dir, &socket)
                .join(format!("{session_id}.json")),
        )
        .expect("the session's descriptor"),
    )
    .expect("descriptor json");
    let session_socket =
        std::path::PathBuf::from(descriptor["socketPath"].as_str().expect("socket path"));
    let held_leases = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(agent_dir.join("session-leases"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "lock")
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        (
            UnixStream::connect(&session_socket).is_ok(),
            held_leases().len(),
            child_pids_of(supervisor_pid),
        ),
        (true, 1, Vec::<u32>::new()),
        "one live session hosted inside the supervisor"
    );

    client.send_command("sd", &serde_json::json!({ "type": "shutdown" }));
    let shutdown = client.read_response("sd");
    assert_eq!(shutdown["success"], true, "shutdown failed: {shutdown}");

    let exit = wait_child_exit(&mut daemon.child, Duration::from_secs(10))
        .expect("the supervisor process exited after shutdown");
    assert!(exit.success(), "supervisor exit: {exit:?}");
    // Nothing of the session outlives the supervisor: its socket is not
    // served and its lease is released.
    assert_eq!(
        (
            socket.exists(),
            UnixStream::connect(&session_socket).is_ok(),
            held_leases(),
        ),
        (false, false, Vec::<std::path::PathBuf>::new()),
        "the supervisor's socket file is removed and its session is gone"
    );
}

/// SIGTERM to the live supervisor is the graceful drain, not the default
/// signal death: the process exits 0 and cleans its socket file up. A
/// connected client rides the drain.
#[test]
fn a_sigterm_exits_the_supervisor_through_the_graceful_drain() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let mut daemon = spawn_daemon(&socket, &agent_dir);
    let (_client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(daemon.child.id().to_string())
        .status()
        .expect("SIGTERM the supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Ok(Some(status)) = daemon.child.try_wait() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the drained supervisor must exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "the drain must end in a clean exit 0, not a signal death: {status}"
    );
    assert!(
        !socket.exists(),
        "the drained supervisor cleans its socket file up"
    );
}
