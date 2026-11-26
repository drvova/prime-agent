//! The user's `!` commands on the runtime daemon (phase 7g-3): `session.bash`
//! streams `bash_start`, `bash_output` and `bash_end` on the session's
//! stream and saves the `bashExecution` row; `session.abort_bash` and
//! `session.stop` kill the running command.

use pa_types::wire2::{ClientFrame, ServerFrame, Since};
use serde_json::{json, Value};

use super::{ok, Bed, Client};

/// The `bash_*` payloads among `frames` on session `id`'s stream.
fn bash_events(frames: &[ServerFrame], id: &str) -> Vec<Value> {
    let stream = format!("agent/{id}/stream");
    frames
        .iter()
        .filter_map(|frame| match frame {
            ServerFrame::Event { event, .. } if event.event.topic.to_string() == stream => {
                Some(event.event.payload.clone())
            }
            _ => None,
        })
        .filter(|payload| {
            payload["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("bash_"))
        })
        .collect()
}

/// Sends `method` with `args` and reads frames until session `id`'s
/// `bash_end`; the run's `bash_*` events.
async fn until_bash_end(client: &mut Client, id: &str, method: &str, args: Value) -> Vec<Value> {
    let request = client.id();
    client
        .send(&ClientFrame::Call {
            id: request.clone(),
            method: method.to_string(),
            args,
        })
        .await;
    let mut frames = Vec::new();
    ok(client.reply(&request, &mut frames).await);
    while !bash_events(&frames, id)
        .iter()
        .any(|event| event["type"] == "bash_end")
    {
        frames.push(client.frame().await);
    }
    bash_events(&frames, id)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bash_command_streams_on_its_session_and_joins_the_transcript() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .subscribe(&format!("agent/{id}/stream"), Since::Now)
        .await;
    let events = until_bash_end(
        &mut client,
        &id,
        "session.bash",
        json!({ "session": id, "command": "echo hi; exit 3", "runId": "run-1" }),
    )
    .await;
    assert_eq!(
        events,
        [
            json!({ "type": "bash_start", "command": "echo hi; exit 3",
                    "excludeFromContext": false, "runId": "run-1" }),
            json!({ "type": "bash_output", "chunk": "hi\n" }),
            json!({ "type": "bash_end", "exitCode": 3, "cancelled": false,
                    "truncated": false, "runId": "run-1" }),
        ]
    );
    let page = ok(client
        .query("transcript.page", json!({ "session": id, "limit": 500 }))
        .await);
    let mut row = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["message"].clone())
        .find(|message| message["role"] == "bashExecution")
        .unwrap_or_else(|| panic!("the bashExecution row: {page}"));
    row.as_object_mut().unwrap().remove("timestamp");
    assert_eq!(
        row,
        json!({ "role": "bashExecution", "command": "echo hi; exit 3", "output": "hi\n",
                "exitCode": 3, "cancelled": false, "truncated": false })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_and_stop_kill_the_running_bash_command() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .subscribe(&format!("agent/{id}/stream"), Since::Now)
        .await;
    let ended = json!({ "type": "bash_end", "exitCode": null, "cancelled": true,
                        "truncated": false, "transient": true });
    let sleep = json!({ "session": id, "command": "sleep 600", "transient": true });
    ok(client.call("session.bash", sleep.clone()).await);
    let events = until_bash_end(
        &mut client,
        &id,
        "session.abort_bash",
        json!({ "session": id }),
    )
    .await;
    assert_eq!(events.last(), Some(&ended));
    ok(client.call("session.bash", sleep).await);
    let events = until_bash_end(&mut client, &id, "session.stop", json!({ "session": id })).await;
    assert_eq!(events.last(), Some(&ended));
}
