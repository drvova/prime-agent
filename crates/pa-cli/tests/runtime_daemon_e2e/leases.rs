//! Whatever ends a session without its own release leaves its transcript
//! openable on the runtime daemon (the supervisor's
//! `stale_lease_release_e2e`): the daemon that held it killed outright, mid
//! turn or idle, or its give-up after the crash cap. A session the next boot
//! revived is supervised in the lifetime that boot started: a crash there
//! heals, and opening its file answers with it. (The supervisor's fourth
//! case, a leftover worker process of the abandoned id, has no runtime
//! counterpart: the runtime daemon runs no worker processes.)

use std::path::PathBuf;

use serde_json::{json, Value};

use super::turns::Attached;
use super::{ok, Bed, Client};

/// A session's script whose prompt `crash now` panics.
fn crashable(responses: &Value) -> Value {
    json!({
        "responses": responses,
        "repeatLastResponse": true,
        "panicOn": { "message": "crash now", "panic": "injected fault" },
    })
}

/// Creates a session in the bed's work dir: its id and its transcript.
async fn create(bed: &Bed) -> (String, PathBuf) {
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client.call("session.create", json!({ "cwd": cwd })).await);
    (
        created["session"]
            .as_str()
            .expect("a session id")
            .to_string(),
        PathBuf::from(created["path"].as_str().expect("a transcript path")),
    )
}

/// Opens the saved session at `path`: the answer.
async fn open(bed: &Bed, path: &std::path::Path) -> Value {
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    ok(client.call("session.create", json!({ "path": path })).await)
}

/// The session leases held under the bed's agent dir.
fn held_leases(bed: &Bed) -> usize {
    std::fs::read_dir(bed.agent_dir().join("session-leases")).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lock"))
            .count()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_revived_session_stays_supervised_and_its_file_reopens() {
    let bed = Bed::new();
    // The turn outlasts the daemon: the kill lands mid-turn.
    let mut first = bed.runtime_daemon(&crashable(
        &json!([{ "text": "turn-1", "delayMs": 600_000 }]),
    ));
    let (id, path) = create(&bed).await;
    let mut session = Attached::attach(&bed, &id).await;
    let cut_off = session.publish("go", None).await;
    session.until_streamed("agent_start").await;
    first.0.kill().expect("kill the runtime daemon");
    first.0.wait().expect("reap the runtime daemon");
    drop(session);

    let _second = bed.runtime_daemon(&crashable(&json!(["continued"])));
    let mut session = Attached::attach(&bed, &id).await;
    session.done(&cut_off["task"]).await;
    session
        .until(|session| !session.payloads("recovered").is_empty())
        .await;
    let reopened = open(&bed, &path).await;
    let crashed = session.publish("crash now", None).await;
    session.done(&crashed["task"]).await;
    session
        .until(|session| session.payloads("recovered").len() == 2)
        .await;
    assert_eq!(
        (
            reopened["session"].clone(),
            reopened["reused"].clone(),
            session.payloads("failed"),
            session.payloads("recovered")[1]["failures"].clone(),
        ),
        (json!(id), json!(true), Vec::<Value>::new(), json!(1)),
        "the file opens onto the revived session, whose first crash is failure 1 of a \
         lifetime the daemon watches"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_held_by_a_killed_daemons_idle_session_is_released_and_its_file_opens() {
    let bed = Bed::new();
    let mut first = bed.runtime_daemon(&json!({ "responses": ["turn-1"] }));
    let (id, path) = create(&bed).await;
    let mut session = Attached::attach(&bed, &id).await;
    let turn = session.publish("go", None).await;
    session.done(&turn["task"]).await;
    drop(session);
    first.0.kill().expect("kill the runtime daemon");
    first.0.wait().expect("reap the runtime daemon");
    let left_behind = held_leases(&bed);

    let _second = bed.runtime_daemon(&json!({ "responses": [] }));
    let reopened = open(&bed, &path).await;
    assert_eq!(
        (left_behind, reopened["session"].clone()),
        (1, json!(id)),
        "the killed daemon left its lease and the saved session opens after the boot"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_given_up_on_releases_its_lease_and_its_file_opens() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&crashable(&json!(["turn-1"])));
    let (id, path) = create(&bed).await;
    let mut session = Attached::attach(&bed, &id).await;
    // A first turn saves the transcript.
    let first = session.publish("hello", None).await;
    session.done(&first["task"]).await;
    while session.payloads("failed").is_empty() {
        let crashed = session.publish("crash now", None).await;
        session.done(&crashed["task"]).await;
        let faults = session.payloads("crashed").len();
        session
            .until(|session| {
                session.payloads("recovered").len() + session.payloads("failed").len() == faults
            })
            .await;
    }
    let reopened = open(&bed, &path).await;
    assert_eq!(
        (reopened["session"].clone(), reopened["reused"].clone()),
        (json!(id), json!(false)),
        "a given-up session leaves its file openable, as a session of its own"
    );
}
