//! A distribution build's boot import of the official build's chats on the
//! runtime daemon (phase 8d-5c), compared with the old daemon (the
//! `pa-daemon` supervisor), each on a bed of its own: the official build's
//! chats under `<home>/.prime/agent`, the daemon on the distribution's own
//! agent dir `<home>/.prime/agent-fork`, offline on the faux provider. The
//! old daemon's `history_import` expectations (pa-core
//! `session::boot_import`'s `a_hanging_copy_never_blocks_the_boot`, moved
//! from pa-daemon) replayed over both wires:
//!
//! - a client connected as the daemon boots hears the started frame, then the
//!   finished one (on wire2 the live-only `system/history_import`, mapped back
//!   by pa-tui's `wire2_compat`);
//! - a client that connects while the copy is held reads the started frame
//!   after its hello (on wire2 the `system.history_import` query), then the
//!   finished one;
//! - the imported chats are the saved catalog's rows once it finished.
//!
//! The import is planned as the `fork` variant's build
//! (`PA_DAEMON_HISTORY_IMPORT_VARIANT`, debug builds only) and held before
//! its first file while a file exists (`PA_DAEMON_HISTORY_IMPORT_HOLD`), so
//! the mid-import client connects while the import provably runs.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::path::Path;

use pa_core::session::boot_import::{HISTORY_IMPORT_HOLD_ENV, HISTORY_IMPORT_VARIANT_ENV};
use pa_types::wire2::ServerFrame;
use serde_json::{json, Value};

use super::old_reference::OldReference;
use super::saved_list::{old_list, read_as_the_tui_does, runtime_list, transcript};
use super::side_questions::OldClient;
use super::{Bed, Client};

/// The distribution build's agent dir, under the bed's dir (its home).
const FORK_AGENT_DIR: &str = ".prime/agent-fork";

/// The saved catalog's request: every saved session.
fn catalog() -> Value {
    json!({ "type": "list_saved_sessions", "cwd": "/work/a" })
}

/// A bed on the distribution's agent dir, the official build's two chats
/// under its home, and the hold file in place.
fn seeded() -> Bed {
    let bed = Bed::with_agent_dir(Path::new(FORK_AGENT_DIR));
    let official = bed.dir.path().join(".prime/agent/sessions");
    std::fs::create_dir_all(&official).unwrap();
    let user = |text: &str, at: u64| {
        json!({ "type": "message", "message": { "role": "user", "content": text,
            "timestamp": at } })
    };
    transcript(
        &official,
        "alpha",
        "/work/a",
        &json!({}),
        &[
            json!({ "type": "session_info", "name": "alpha work" }),
            user("plan the alpha release", 1_790_110_000_000),
        ],
        400,
    );
    transcript(
        &official,
        "beta",
        "/work/b",
        &json!({}),
        &[user("beta question", 1_790_120_000_000)],
        300,
    );
    std::fs::write(hold(&bed), "").unwrap();
    bed
}

fn hold(bed: &Bed) -> std::path::PathBuf {
    bed.dir.path().join("hold-history-import")
}

/// The seams' environment over the bed's: offline, the `fork` build's
/// import, held.
fn env(bed: &Bed) -> Vec<(&'static str, String)> {
    vec![
        ("PI_OFFLINE", "1".to_string()),
        (HISTORY_IMPORT_VARIANT_ENV, "fork".to_string()),
        (HISTORY_IMPORT_HOLD_ENV, hold(bed).display().to_string()),
    ]
}

/// What the clients of one daemon saw of its boot import.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Imported {
    /// The client connected as the daemon booted: its `history_import`
    /// frames.
    booted: Vec<Value>,
    /// The client that connected mid-import: its `history_import` frames.
    joined: Vec<Value>,
    /// The saved catalog's rows once the import finished, by id, with the
    /// bed's dir as `<dir>` and their file times left out (each bed copied
    /// its chats at its own time).
    catalog: Vec<Value>,
}

/// The `history_import` frames among `frames`, a repeat of the one before
/// dropped: a client that connects as the import starts may read the started
/// frame both as a running import and as its broadcast (either daemon).
fn import_frames(frames: &[Value]) -> Vec<Value> {
    let mut imports: Vec<Value> = Vec::new();
    for frame in frames
        .iter()
        .filter(|frame| frame["type"] == "history_import")
    {
        if imports.last() != Some(frame) {
            imports.push(frame.clone());
        }
    }
    imports
}

/// Whether `frames` hold the import's finished frame.
fn finished(frames: &[Value]) -> bool {
    frames
        .iter()
        .any(|frame| frame["type"] == "history_import" && frame["phase"] == "finished")
}

/// The catalog's streamed rows (`session_list_item`'s `session`) as
/// [`Imported::catalog`] keeps them.
fn catalog_rows(bed: &Bed, frames: &[Value]) -> Vec<Value> {
    let dir = bed.dir.path().display().to_string();
    let mut rows: Vec<Value> = frames
        .iter()
        .map(read_as_the_tui_does)
        .filter(|frame| frame["type"] == "session_list_item")
        .map(|frame| {
            let mut row: Value =
                serde_json::from_str(&frame["session"].to_string().replace(&dir, "<dir>")).unwrap();
            for time in ["created", "modified"] {
                row[time] = Value::Null;
            }
            row
        })
        .collect();
    rows.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    rows
}

/// The old daemon's import, and how many frames its catalog streamed
/// before its response.
fn old_import(bed: &Bed) -> (Imported, usize) {
    let env = env(bed);
    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let _daemon = bed.old_daemon_with(&env);
    let mut booted = OldClient::connect(&bed.old_socket());
    let started = |seen: &[Value]| seen.iter().any(|frame| frame["type"] == "history_import");
    booted.until(started);
    // The copy is held before its first file: this client joins mid-import.
    let mut joined = OldClient::connect(&bed.old_socket());
    joined.until(|seen| seen.iter().any(|frame| frame["type"] == "daemon_hello") && started(seen));
    std::fs::remove_file(hold(bed)).unwrap();
    booted.until(finished);
    joined.until(finished);
    let catalog = old_list(&bed.old_socket(), "catalog", &catalog());
    let imported = Imported {
        booted: import_frames(booted.seen()),
        joined: import_frames(joined.seen()),
        catalog: catalog_rows(bed, &catalog),
    };
    (imported, catalog.len() - 1)
}

/// A compat client of the runtime daemon: it follows `system/history_import`
/// and asks for a running import as the compat layer's transport does once
/// connected; the frames it hands the TUI so far.
async fn import_follower(socket: &Path) -> (Client, Vec<Value>) {
    let (mut client, _) = Client::connect(socket).await;
    let request = client.id();
    let mut frames = pa_tui::wire2_history_import_request(request.clone())
        .unwrap()
        .into_iter();
    let (Some(subscribe), Some(query), None) = (frames.next(), frames.next(), frames.next()) else {
        panic!("the history import is a subscription and a query");
    };
    client.send(&subscribe).await;
    let mut early = Vec::new();
    loop {
        match client.frame().await {
            ServerFrame::Backfilled { .. } => break,
            other => early.push(other),
        }
    }
    client.send(&query).await;
    let outcome = client.reply(&request, &mut early).await;
    let mut seen = compat_frames(&early);
    seen.extend(pa_tui::wire2_history_import_reply_frame(outcome).unwrap());
    (client, seen)
}

/// The old frames the compat layer hands the TUI for the events among
/// `frames`.
fn compat_frames(frames: &[ServerFrame]) -> Vec<Value> {
    let events: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            ServerFrame::Event { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect();
    pa_tui::wire2_live_frames(&events).unwrap()
}

/// Reads `client`'s frames into `seen` until `done` holds for them.
async fn until(client: &mut Client, seen: &mut Vec<Value>, done: impl Fn(&[Value]) -> bool) {
    while !done(seen) {
        let frame = client.frame().await;
        seen.extend(compat_frames(&[frame]));
    }
}

/// The runtime daemon's import; its catalog is read once `streamed` frames
/// arrived beside the response, as many as the old daemon's.
async fn runtime_import(bed: &Bed, streamed: usize) -> Imported {
    let env = env(bed);
    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let _daemon = bed.runtime_daemon_with(&json!({ "responses": [] }), &env);
    let started = |seen: &[Value]| seen.iter().any(|frame| frame["type"] == "history_import");
    let (mut booted, mut booted_seen) = import_follower(&bed.runtime_socket()).await;
    until(&mut booted, &mut booted_seen, started).await;
    // The copy is held before its first file: this client joins mid-import.
    let (mut joined, mut joined_seen) = import_follower(&bed.runtime_socket()).await;
    assert!(
        started(&joined_seen),
        "a client that connects mid-import reads the running import at once: {joined_seen:?}"
    );
    std::fs::remove_file(hold(bed)).unwrap();
    until(&mut booted, &mut booted_seen, finished).await;
    until(&mut joined, &mut joined_seen, finished).await;
    let listed = {
        let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
        runtime_list(&mut client, "catalog", &catalog(), streamed).await
    };
    Imported {
        booted: import_frames(&booted_seen),
        joined: import_frames(&joined_seen),
        catalog: catalog_rows(bed, &listed),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_boot_imports_the_official_chats_as_on_the_old_daemon() {
    let old_bed = seeded();
    let mut reference = OldReference::open("history_import", &old_bed);
    let (old, streamed): (Imported, usize) = reference.take("import", || old_import(&old_bed));
    let runtime = runtime_import(&seeded(), streamed).await;
    let started = json!({ "type": "history_import", "phase": "started", "sessions": 2 });
    let finished =
        json!({ "type": "history_import", "phase": "finished", "sessions": 2, "kept": 0 });
    let row = |id: &str, cwd: &str, first: &str| {
        json!({ "id": id, "path": format!("<dir>/{FORK_AGENT_DIR}/sessions/{id}.jsonl"),
                "cwd": cwd, "firstMessage": first })
    };
    // The old daemon's frames and rows show the import did what the
    // fixture asks of it: both chats announced, copied and listed.
    assert_eq!(
        (
            &old.booted,
            &old.joined,
            old.catalog
                .iter()
                .map(
                    |row| json!({ "id": row["id"], "path": row["path"], "cwd": row["cwd"],
                                   "firstMessage": row["firstMessage"] })
                )
                .collect::<Vec<_>>(),
        ),
        (
            &vec![started.clone(), finished.clone()],
            &vec![started, finished],
            vec![
                row("alpha", "/work/a", "plan the alpha release"),
                row("beta", "/work/b", "beta question"),
            ],
        ),
        "the old daemon: {old:#?}"
    );
    assert_eq!(runtime, old);
}
