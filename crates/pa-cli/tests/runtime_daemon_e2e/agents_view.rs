//! What the TUI's agents view follows on the runtime daemon (phase 8c-2):
//! the roster feed lists a session from its open to its stop, in the old
//! supervisor's entry shape, and `runtime.roster` reads the same entry. The
//! traffic feed is checked where a family sends a message
//! ([`super::family`]), and both feeds against the recordings in
//! [`super::goldens`].

use pa_types::wire2::{ServerFrame, Since};
use serde_json::{json, Value};

use super::{ok, Bed, Client};

/// The roster updates among `frames`.
fn roster_updates(frames: &[ServerFrame]) -> Vec<Value> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            ServerFrame::Event { event, .. } if event.event.topic.as_str() == "runtime/roster" => {
                Some(event.event.payload.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_roster_lists_a_session_from_its_open_to_its_stop() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"] }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    client.subscribe("runtime/roster", Since::Now).await;
    let mut frames = Vec::new();
    let created = ok(client
        .call_keeping("session.create", json!({}), &mut frames)
        .await);
    let id = created["session"].as_str().unwrap().to_string();
    while roster_updates(&frames).is_empty() {
        frames.push(client.frame().await);
    }
    let opened = roster_updates(&frames).remove(0);
    let entry = &opened["changed"][0];
    let read = ok(client.query("runtime.roster", json!({})).await);
    let mut stopped = Vec::new();
    ok(client
        .call_keeping("session.stop", json!({ "session": id }), &mut stopped)
        .await);
    while roster_updates(&stopped).is_empty() {
        stopped.push(client.frame().await);
    }
    let after = ok(client.query("runtime.roster", json!({})).await);
    assert_eq!(
        (
            [&entry["agentId"], &entry["workerId"], &entry["status"]],
            [
                &entry["summary"]["activeSessionId"],
                &entry["summary"]["lifecycle"],
                &entry["summary"]["runtimeKind"],
            ],
            entry["summary"].get("sessionActions"),
            read,
            roster_updates(&stopped),
            after,
        ),
        (
            [&json!(id), &json!(id), &json!("idle")],
            [&json!(id), &json!("draft"), &json!("top-level")],
            None,
            json!({ "roster": [entry] }),
            vec![json!({ "changed": [], "removed": [id] })],
            json!({ "roster": [] }),
        )
    );
}
