//! E2E verifier for the agents-view reply composer: the space-armed composer sends a
//! live session's reply through the UNATTACHED roster client (the view never attaches),
//! and a saved row resumes into a fresh session whose file carries the reply.
#![cfg(unix)]

mod support;

use support::runtime_daemon::RuntimeDaemon;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pa_tui::agents_view::{AgentsHeadlessPlan, AgentsStep, AgentsViewOptions, AgentsViewUiMode};

fn write_fixture(dir: &Path, id: &str, name: &str, turns: &[(&str, &str)]) -> PathBuf {
    let path = dir.join(format!("{id}.jsonl"));
    let mut content = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2024-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}}\n"
    );
    let _ = writeln!(content,
        "{{\"type\":\"session_info\",\"id\":\"{id}-info\",\"timestamp\":\"2024-01-01T00:00:00.000Z\",\"name\":\"{name}\"}}"
    );
    for (index, (user, assistant)) in turns.iter().enumerate() {
        let _ = writeln!(content,
            "{{\"type\":\"message\",\"id\":\"{id}-m{index}u\",\"timestamp\":\"2024-01-01T00:00:0{index}.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"{user}\",\"timestamp\":{}}}}}",
            index * 1000
        );
        let _ = writeln!(content,
            "{{\"type\":\"message\",\"id\":\"{id}-m{index}a\",\"timestamp\":\"2024-01-01T00:00:0{index}.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{assistant}\"}}],\"timestamp\":{}}}}}",
            index * 1000 + 1
        );
    }
    std::fs::write(&path, content).expect("write fixture");
    path
}

/// The faux engine's script, written under `dir`: the runtime daemon started with it in
/// `PRIME_AGENT_FAUX_SCRIPT` answers each session's turns from it, so the send path runs through
/// the REAL agent engine with real session persistence. Returns the script's path.
fn faux_engine_script(dir: &Path, responses: &[&str]) -> PathBuf {
    let script = dir.join("faux-engine.json");
    std::fs::write(
        &script,
        serde_json::json!({
            "engine": "faux",
            "responses": responses
                .iter()
                .map(|text| serde_json::json!({ "text": text }))
                .collect::<Vec<_>>(),
        })
        .to_string(),
    )
    .expect("write faux script");
    script
}

/// The runtime daemon for `dir`, its sessions' turns answered by the faux `script`.
fn spawn_scripted(dir: &Path, script: &Path) -> RuntimeDaemon {
    RuntimeDaemon::spawn_with(dir, |command| {
        command.env("PRIME_AGENT_FAUX_SCRIPT", script);
    })
}

fn view_options(socket: &Path, session_dir: &Path, config: serde_json::Value) -> AgentsViewOptions {
    AgentsViewOptions {
        socket_path: socket.to_path_buf(),
        cwd: std::env::temp_dir(),
        session_dir: Some(session_dir.to_path_buf()),
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        anchor_session_id: None,
        scope: None,
        folder_scope: None,
        query: None,
        expanded_ancestors: Vec::new(),
        selected_row_identity: None,
        selected_key: None,
        status_message: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        show_hardware_cursor: false,
        incident_notice_state: None,
        create_config: config,
    }
}

#[tokio::test]
async fn reply_to_live_session_delivers_the_prompt() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    // Two scripted responses: the warm-up turn consumes the first, the view's reply the second.
    let script = faux_engine_script(dir.path(), &["warm reply", "the reply landed"]);
    let daemon = spawn_scripted(dir.path(), &script);
    let mut client = daemon.client();
    // Create through the daemon: the roster row the view lists must exist.
    client.subscribe("runtime/roster");
    let created = client
        .call("session.create", &serde_json::json!({}))
        .expect("the create answered");
    let active = created["session"]
        .as_str()
        .expect("the created session is live")
        .to_string();
    let lists = |update: &serde_json::Value, lifecycle: Option<&str>| {
        update["changed"].as_array().is_some_and(|changed| {
            changed.iter().any(|entry| {
                entry["summary"]["activeSessionId"].as_str() == Some(active.as_str())
                    && lifecycle.is_none_or(|lifecycle| {
                        entry["summary"]["lifecycle"].as_str() == Some(lifecycle)
                    })
            })
        })
    };
    // The roster feed carries the fresh session before the view runs.
    client.event_until("runtime/roster", |update| lists(update, None));
    // A message-less draft never surfaces as a roster row (TS
    // `shouldShowAgentsViewSession`), so the reply target needs a live session with
    // a first exchange; the warm-up must complete, and the roster show it live, before the view
    // runs.
    client.run_turn(&active, "warm up");
    client.event_until("runtime/roster", |update| lists(update, Some("live")));
    let config = serde_json::json!({});
    let options = view_options(&daemon.socket, &session_dir, config);
    let plan = AgentsHeadlessPlan {
        steps: vec![
            AgentsStep::WaitSettle { timeout_ms: 1000 },
            // The live row sits at the top of the Idle section.
            AgentsStep::Key("space".to_string()),
            AgentsStep::Type("ping".to_string()),
            AgentsStep::Key("enter".to_string()),
            AgentsStep::WaitRender {
                needle: "Reply sent".to_string(),
                timeout_ms: 10_000,
            },
        ],
        width: 120,
        height: 36,
    };
    let outcome =
        pa_tui::agents_view::run_agents_view(options, AgentsViewUiMode::Headless(plan), None)
            .await
            .expect("agents view run")
            .outcome;
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("Reply sent"),
        "the send's status rendered ({} frames):\n{rendered}",
        outcome.frames.len()
    );
    // The reply's turn runs behind the send's ack: wait for the session to settle.
    client
        .call(
            "session.wait_idle",
            &serde_json::json!({ "session": active, "subagents": false }),
        )
        .expect("the session settles");
    let last = client
        .query(
            "session.last_assistant_text",
            &serde_json::json!({ "session": active }),
        )
        .expect("the last assistant text");
    assert_eq!(
        last["text"], "the reply landed",
        "the reply delivered to the live session"
    );
}

#[tokio::test]
async fn reply_to_saved_session_resumes_and_sends() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let script = faux_engine_script(dir.path(), &["resumed and heard"]);
    let daemon = spawn_scripted(dir.path(), &script);
    let config = serde_json::json!({});

    let fixture = write_fixture(
        &session_dir,
        "resume-01",
        "resume target",
        &[("first turn", "first reply")],
    );

    let options = view_options(&daemon.socket, &session_dir, config);
    let plan = AgentsHeadlessPlan {
        steps: vec![
            AgentsStep::WaitSettle { timeout_ms: 1000 },
            // The saved row is the Inactive section's only row.
            AgentsStep::Key("space".to_string()),
            AgentsStep::WaitRender {
                needle: "Write a prompt to resume this session".to_string(),
                timeout_ms: 5000,
            },
            AgentsStep::Type("resume me".to_string()),
            AgentsStep::Key("enter".to_string()),
            AgentsStep::WaitRender {
                needle: "Reply sent".to_string(),
                timeout_ms: 15_000,
            },
        ],
        width: 120,
        height: 36,
    };
    let outcome =
        pa_tui::agents_view::run_agents_view(options, AgentsViewUiMode::Headless(plan), None)
            .await
            .expect("agents view run")
            .outcome;
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("Reply sent"),
        "the resume's status rendered:\n{rendered}"
    );
    // The fixture resumed into a live session; its reply's turn runs behind the send's ack, and
    // the session appends to the SAME durable file: wait for it to settle, then read the file.
    let mut client = daemon.client();
    let open = client
        .query("sessions.list", &serde_json::json!({ "open": true }))
        .expect("the list answered");
    let session = open["sessions"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["path"].as_str() == Some(fixture.to_string_lossy().as_ref()))
        })
        .and_then(|row| row["id"].as_str())
        .unwrap_or_else(|| panic!("the fixture resumed into a live session: {open}"))
        .to_string();
    client
        .call(
            "session.wait_idle",
            &serde_json::json!({ "session": session, "subagents": false }),
        )
        .expect("the session settles");
    let content = support::saved_transcript(&agent_dir, &fixture);
    assert!(
        content.contains("resume me") && content.contains("resumed and heard"),
        "the resumed session's file holds the reply and its answer:\n{content}"
    );
}
