//! The Traffic pane never waits on the daemon: against a mock supervisor whose
//! `agent_traffic_subscribe` never answers, the scoped view's `m` still paints the open pane (its
//! connecting header) on the next frame, and the view's exit does not wait on the hanging
//! subscription; an entry the daemon pushes meanwhile paints in the open pane.
#![cfg(unix)]

mod support;

use std::io::BufReader;
use std::time::{Duration, Instant};

use pa_tui::agents_view::{AgentsHeadlessPlan, AgentsStep, AgentsViewScope, AgentsViewUiMode};
use serde_json::{json, Value};

use support::{read_line, respond, respond_failure, view_options, MockSupervisor};

fn summary(id: &str, parent: Option<&str>, name: &str) -> Value {
    let mut summary = json!({
        "sessionId": id,
        "lifecycle": "live",
        "activeSessionId": format!("{id}-live"),
        "sessionFile": format!("/tmp/{id}.jsonl"),
        "runtimeKind": if parent.is_some() { "subagent" } else { "top-level" },
        "sessionName": name,
        "messageCount": 1,
        "rlmDepth": u64::from(parent.is_some()),
    });
    if let Some(parent) = parent {
        summary["parentActiveSessionId"] = json!(format!("{parent}-live"));
        summary["parentSessionId"] = json!(parent);
        summary["parentSessionPath"] = json!(format!("/tmp/{parent}.jsonl"));
    }
    summary
}

/// Serve one view connection; `agent_traffic_subscribe` is read and never answered.
fn serve(mock: &MockSupervisor, subscribed: &std::sync::mpsc::Sender<()>) {
    let (stream, _) = mock.listener.accept().expect("accept view connection");
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("read timeout");
    let mut writer = stream.try_clone().expect("clone socket");
    let mut reader = BufReader::new(stream);
    support::write_json(
        &mut writer,
        &json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": [],
        }),
    );
    while let Some(line) = read_line(&mut reader) {
        let Ok(envelope) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = envelope["id"].as_str().unwrap_or_default().to_string();
        let command = envelope["command"]["type"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        match command.as_str() {
            "roster_subscribe" => {
                let roster = json!({ "roster": [
                    { "agentId": "p", "status": "idle", "summary": summary("p", None, "root") },
                    { "agentId": "c", "status": "running", "summary": summary("c", Some("p"), "kid") },
                ] });
                respond(&mut writer, &id, &command, &roster);
            }
            "list_saved_sessions" => {
                respond(&mut writer, &id, &command, &json!({ "sessions": [] }));
            }
            "heartbeats_list" => {
                respond(&mut writer, &id, &command, &json!({ "heartbeats": [] }));
            }
            // Never answered: only a pushed entry follows, which the open pane must paint.
            "agent_traffic_subscribe" => {
                let _ = subscribed.send(());
                support::write_json(
                    &mut writer,
                    &json!({ "type": "agent_traffic", "entry": {
                        "rootActiveSessionId": "p-live",
                        "from": "kid",
                        "fromActiveSessionId": "c-live",
                        "to": "root",
                        "toActiveSessionId": "p-live",
                        "relationship": "child",
                        "kind": "direct",
                        "outcome": "queued",
                        "chars": 9,
                        "preview": "lane done",
                        "timestampMs": 1,
                    } }),
                );
            }
            other => respond_failure(&mut writer, &id, other, "not handled by the mock"),
        }
    }
}

#[tokio::test]
async fn a_hanging_traffic_subscription_never_delays_the_pane_or_the_exit() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("agents-view.sock");
    let mock = MockSupervisor::bind(&socket);
    let (subscribed_tx, subscribed_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || serve(&mock, &subscribed_tx));

    let mut options = view_options(&socket);
    options.scope = Some(AgentsViewScope {
        session_id: Some("p".to_string()),
        active_session_id: Some("p-live".to_string()),
        session_name: Some("root".to_string()),
    });
    let plan = AgentsHeadlessPlan {
        steps: vec![
            AgentsStep::WaitRender {
                needle: "kid".to_string(),
                timeout_ms: 10_000,
            },
            AgentsStep::Key("m".to_string()),
            AgentsStep::WaitRender {
                needle: "Traffic \u{b7} connecting".to_string(),
                timeout_ms: 10_000,
            },
            AgentsStep::WaitRender {
                needle: "kid \u{2192} root".to_string(),
                timeout_ms: 10_000,
            },
        ],
        width: 120,
        height: 36,
    };
    let started = Instant::now();
    let outcome =
        pa_tui::agents_view::run_agents_view(options, AgentsViewUiMode::Headless(plan), None)
            .await
            .expect("the agents view run")
            .outcome;
    let elapsed = started.elapsed();

    assert!(
        outcome
            .frames
            .last()
            .is_some_and(|frame| frame.contains("Traffic \u{b7} connecting")
                && frame.contains(
                    "kid \u{2192} root \u{b7} child \u{b7} queued \u{b7} 9 chars  lane done"
                )),
        "the open pane painted, and painted a pushed entry, while its subscription hangs:\n{:?}",
        outcome.frames.last()
    );
    assert!(
        subscribed_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the open pane sent the subscribe"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "the exit never waits on the hanging subscription: {elapsed:?}"
    );
    drop(socket);
    let _ = server.join();
}
