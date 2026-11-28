//! The kill -9 restart regression: sessions die with the daemon that hosts
//! them and the next boot revives the ones that were mid-turn.
use super::*;

#[test]
fn supervisor_kill9_restart_revives_the_sessions_that_were_mid_turn() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

    let mut daemon = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);
    let supervisor_pid = daemon.child.id();
    let (mut client, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    // Each session has a slow first turn (in flight when the daemon dies)
    // and a second turn for the post-restart prompt.
    let mut sessions = Vec::new();
    for index in 0..3 {
        let script_path = dir.path().join(format!("script-{index}.json"));
        std::fs::write(
            &script_path,
            json!({ "responses": [
                { "text": format!("turn-1-{index}"), "delayMs": 1200 },
                { "text": format!("turn-2-{index}"), "delayMs": 10 },
            ] })
            .to_string(),
        )
        .expect("write script");
        client.send_command(
            &format!("c{index}"),
            &json!({
                "type": "create",
                "config": {
                    "cwd": dir.path().to_string_lossy(),
                    "sessionDir": sessions_dir.to_string_lossy(),
                    "script": script_path.to_string_lossy(),
                },
            }),
        );
        let created = client.read_response(&format!("c{index}"));
        assert_eq!(created["success"], true, "create {index} failed: {created}");
        let session_id = created["data"]["id"]
            .as_str()
            .or_else(|| created["data"]["sessionId"].as_str())
            .expect("session id")
            .to_string();
        client.send_command(
            &format!("a{index}"),
            &json!({ "type": "attach", "activeSessionId": session_id }),
        );
        let attached = client.read_response(&format!("a{index}"));
        assert_eq!(attached["success"], true, "attach {index} failed");
        sessions.push(session_id);
    }
    // Sessions run inside the daemon: it has no child process to outlive it.
    assert_eq!(
        child_pids_of(supervisor_pid),
        Vec::<u32>::new(),
        "three live sessions and the daemon has no child process"
    );

    for (index, session_id) in sessions.iter().enumerate() {
        client.send_command(
            &format!("p{index}"),
            &json!({
                "type": "prompt",
                "activeSessionId": session_id,
                "message": "go",
            }),
        );
        let ack = client.read_response(&format!("p{index}"));
        assert_eq!(ack["success"], true, "prompt {index} failed: {ack}");
    }
    // Every turn is durably in flight before the daemon dies.
    for session_id in &sessions {
        wait_for_busy_journal_evidence(&agent_dir, &socket, session_id);
    }
    let session_sockets: Vec<PathBuf> = sessions
        .iter()
        .map(|session_id| session_socket(&agent_dir, &socket, session_id))
        .collect();

    // kill -9 the daemon: every session dies with it, mid-turn.
    daemon.child.kill().expect("kill -9 supervisor");
    let _ = daemon.child.wait();
    for session_socket in &session_sockets {
        assert!(
            UnixStream::connect(session_socket).is_err(),
            "nothing serves {} once the daemon is dead",
            session_socket.display()
        );
    }

    let restart_before = pa_daemon::util::now_iso();
    let mut daemon2 = spawn_supervisor(&socket, &agent_dir);
    wait_socket_ready(&socket);

    // The boot revives all three from their journals, under the same ids.
    let log_path = pa_core::platform::daemon_endpoint::daemon_log_path(&socket, &agent_dir);
    let deadline = Instant::now() + Duration::from_secs(30);
    let registered = loop {
        let registered = distinct(workers_registered_since(&log_path, &restart_before));
        if registered.len() == 3 {
            break registered;
        }
        assert!(
            Instant::now() < deadline,
            "sessions were not revived after restart ({restart_before}): {registered:?}; log: {}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut expected = sessions.clone();
    expected.sort();
    assert_eq!(registered, expected, "the same three identities came back");

    let (mut client2, _hello) = Client::connect(&socket);
    // Each revived session starts its interrupted turn again and finishes it;
    // the death is on its row.
    let mut revived = Vec::new();
    for (index, session_id) in sessions.iter().enumerate() {
        client2.send_command(
            &format!("idle{index}"),
            &json!({ "type": "wait_for_idle", "activeSessionId": session_id }),
        );
        let idle = client2.read_response(&format!("idle{index}"));
        assert_eq!(
            idle["success"], true,
            "wait_for_idle {index} failed: {idle}"
        );
        client2.send_command(
            &format!("text{index}"),
            &json!({ "type": "get_last_assistant_text", "activeSessionId": session_id }),
        );
        let text = client2.read_response(&format!("text{index}"));
        revived.push(text["data"]["text"].clone());
    }
    client2.send_command("list1", &json!({ "type": "list" }));
    let list = client2.read_response("list1");
    assert_eq!(list["success"], true, "list failed: {list}");
    let mut rows: Vec<(String, Value)> = list["data"]["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|row| {
            (
                row["id"].as_str().expect("id").to_string(),
                row["lastCrash"]["kind"].clone(),
            )
        })
        .collect();
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        (rows, revived),
        (
            expected
                .iter()
                .map(|session_id| (session_id.clone(), json!("daemon_restart")))
                .collect::<Vec<_>>(),
            vec![json!("turn-1-0"), json!("turn-1-1"), json!("turn-1-2")],
        ),
        "the roster is rebuilt from the journals and every interrupted turn ran again: {list}"
    );

    let target = &sessions[1];
    client2.send_command(
        "p1",
        &json!({
            "type": "prompt_and_wait",
            "activeSessionId": target,
            "message": "second turn",
        }),
    );
    let done = client2.read_response("p1");
    assert_eq!(done["success"], true, "post-restart prompt failed: {done}");
    client2.send_command(
        "answer",
        &json!({ "type": "get_last_assistant_text", "activeSessionId": target }),
    );
    let answer = client2.read_response("answer");
    assert_eq!(
        answer["data"]["text"], "turn-2-1",
        "scripted turn completed post-restart: {answer}"
    );

    client2.send_command("sd", &json!({ "type": "shutdown" }));
    let shutdown = client2.read_response("sd");
    assert_eq!(shutdown["success"], true, "shutdown failed: {shutdown}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon2.child.try_wait().expect("try wait").is_none() {
        assert!(Instant::now() < deadline, "restarted supervisor exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_sessions_gone(&agent_dir, &session_sockets);
}
