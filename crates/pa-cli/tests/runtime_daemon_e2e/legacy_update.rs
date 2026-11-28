//! A TypeScript updater's restart handoff (phase 8d-5b2): `prime-agent
//! update --internal-update-restart-coordinator`, the process a shipped TS
//! updater starts after it installed this release, restores the TS daemon's
//! sessions from its converted checkpoint on the runtime daemon, as the
//! supervisor of an earlier release did (`docs/legacy-upgrade-contract.md`).
//!
//! The TS daemon is gone already (its checkpoint's process is dead), so the
//! coordinator takes the recovery path: it boots the runtime daemon with the
//! checkpoint as its roster, waits for its greeting, polls its restore
//! status, and writes the TS status record. The runtime daemon replays the
//! row as the old daemon did: the update-complete notice for the session the update was
//! started from, the queued follow-up, the continuation of the cut-off turn
//! first, then the follow-up.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use pa_core::session::revival::UPDATE_RESTART_CONTINUATION_PROMPT;
use serde_json::{json, Value};

use super::old_reference::OldReference;
use super::turns::Attached;
use super::update::{compat_request, message_texts};
use super::{Bed, Client, FRAME_WAIT};

/// What the restored session answers: the continuation, then the follow-up.
const CONTINUED: &str = "continued after the update";
const FOLLOWED: &str = "followed up after the update";

/// The TS session's old active handle, the one the update was started from.
const TS_ACTIVE: &str = "ts-active-1";

/// The installed release's version, as the coordinator checks it.
fn installed_version() -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .arg("--version")
        .output()
        .expect("prime-agent --version");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A saved session in the bed: one turn on the runtime daemon, then the
/// daemon shut down; its id and transcript.
async fn saved_session(bed: &Bed) -> (String, PathBuf) {
    let daemon = bed.runtime_daemon(&json!({ "responses": ["first answer"] }));
    let mut session = Attached::open(bed, bed.dir.path()).await;
    let task = session.publish("hello", None).await;
    session.done(&task["task"]).await;
    let id = session.id.clone();
    drop(session);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let _ = client.call("daemon.shutdown", json!({})).await;
    super::lifecycle::exit_code(&mut { daemon });
    let transcript =
        pa_core::session::manager::get_session_file_path(&bed.agent_dir().join("sessions"), &id);
    (id, transcript)
}

/// The converted TS checkpoint for `transcript`, as the coordinator wrote it
/// before the TS daemon stopped: the turn was running, a follow-up queued.
fn checkpoint(bed: &Bed, id: &str, transcript: &Path, script: &Path) -> Value {
    json!({
        "format_version": 1,
        "update_id": "legacy-e2e",
        "socket_path": bed.old_socket(),
        "created_at": "2026-10-09T00:00:00Z",
        // A process that is gone: the predecessor has exited.
        "supervisor": { "pid": 4_000_000, "process_start_id": "gone", "generation": "ts" },
        "binary": { "from_version": "0.9.8", "to_version": installed_version() },
        "sessions": [{
            "session_id": id,
            "active_session_id": TS_ACTIVE,
            "session_file": transcript,
            "kind": "top-level",
            "rlm_depth": 0,
            "cwd": bed.dir.path(),
            "runtime_config": { "create": {
                "sessionPath": transcript, "cwd": bed.dir.path(),
                "config": { "script": script },
            } },
            "queue": { "next_turn": [], "actions": { "formatVersion": 1, "actions": [{
                "id": "follow", "delivery": "when_run_idle", "source": "interactive",
                "payload": { "kind": "turn", "text": "queued follow-up", "queueVisible": true },
            }] } },
            "in_flight": { "streaming": true, "compacting": false, "bash_running": false,
                           "rlm_children": false, "retrying": false, "prompt_in_flight": false },
            "should_resume": true,
        }],
        "legacy_ts_restart": true,
        "restart_origin_active_session_id": TS_ACTIVE,
    })
}

/// The status record, without its times, identities and request id.
fn status_view(status: &Value) -> Value {
    json!({
        "version": status["version"],
        "phase": status["phase"],
        "counts": status["counts"],
        "failures": status.get("failures").cloned().unwrap_or(json!([])),
        "message": status.get("message").cloned().unwrap_or(Value::Null),
    })
}

/// What a daemon restored: the status record and the session's texts.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Restored {
    status: Value,
    texts: Vec<String>,
}

async fn legacy_restore(bed: &Bed) -> Restored {
    let (id, transcript) = saved_session(bed).await;
    let script = bed.dir.path().join("successor-script.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "modelId": "faux-1", "modelName": "Faux Model",
                "reasoning": false, "contextWindow": 128_000,
                "responses": [CONTINUED, FOLLOWED] })
        .to_string(),
    )
    .unwrap();
    let recovery = bed.agent_dir().join("legacy-update-recovery");
    std::fs::create_dir_all(&recovery).unwrap();
    std::fs::write(
        recovery.join(format!(
            "{}.json",
            pa_core::update::restart::socket_hash(&bed.old_socket())
        )),
        checkpoint(bed, &id, &transcript, &script).to_string(),
    )
    .unwrap();
    let status_path = bed
        .agent_dir()
        .join("update-restarts")
        .join("legacy-e2e.json");
    std::fs::create_dir_all(status_path.parent().unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    bed.env(&mut command, "coordinator.log");
    command
        .args([
            "update",
            "--internal-update-restart-coordinator",
            "--daemon-socket",
        ])
        .arg(bed.old_socket())
        .arg("--internal-update-restart-status")
        .arg(&status_path)
        .env("PI_OFFLINE", "1")
        .env(
            "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_REGISTRY_DIR",
            bed.dir.path().join("supervisor-owners"),
        )
        .env(
            "PRIME_AGENT_FAUX_SCRIPT",
            std::fs::read_to_string(&script).unwrap(),
        );
    let finished = command.status().expect("run the coordinator");
    let status: Value =
        serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
    assert!(
        finished.success(),
        "the coordinator failed: {status}\n{}",
        std::fs::read_to_string(bed.dir.path().join("coordinator.log")).unwrap_or_default()
    );
    let texts = restored_texts(bed, &id).await;
    stop_successor(bed).await;
    Restored {
        status: status_view(&status),
        texts,
    }
}

/// The restored session's texts once its follow-up was answered.
async fn restored_texts(bed: &Bed, id: &str) -> Vec<String> {
    let deadline = Instant::now() + FRAME_WAIT;
    loop {
        let messages = json!({ "type": "get_messages", "activeSessionId": id });
        let response = {
            let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
            compat_request(&mut client, "get_messages", messages).await
        };
        let texts = message_texts(&response["data"]);
        if texts.last().map(String::as_str) == Some(FOLLOWED) {
            return texts;
        }
        assert!(
            Instant::now() < deadline,
            "the restored session never answered its follow-up: {response}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stops the successor (and with it its restart owner) and waits until its
/// socket is gone.
async fn stop_successor(bed: &Bed) {
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let _ = client.call("daemon.shutdown", json!({})).await;
    let socket = bed.runtime_socket();
    let deadline = Instant::now() + FRAME_WAIT;
    while std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        assert!(Instant::now() < deadline, "the successor did not stop");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The supervisor's restore of a TS checkpoint, replayed on the runtime
/// daemon: the same status record and the same restored turns.
#[tokio::test(flavor = "multi_thread")]
async fn a_ts_checkpoint_restores_on_the_runtime_daemon_as_on_the_old_daemon() {
    let old: Restored = OldReference::open("ts_checkpoint_restore", &Bed::new()).take("restore");
    let runtime = legacy_restore(&Bed::new()).await;
    assert_eq!(
        old,
        Restored {
            status: json!({
                "version": 1,
                "phase": "complete",
                "counts": { "total": 1, "restored": 1, "resumed": 1, "failed": 0 },
                "failures": [],
                "message": null,
            }),
            texts: vec![
                "hello".to_string(),
                "first answer".to_string(),
                UPDATE_RESTART_CONTINUATION_PROMPT.to_string(),
                CONTINUED.to_string(),
                "queued follow-up".to_string(),
                FOLLOWED.to_string(),
            ],
        },
        "the old daemon's own expectations"
    );
    assert_eq!(runtime, old);
}
