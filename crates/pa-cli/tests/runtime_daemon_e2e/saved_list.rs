//! The agents view's saved-session catalog on the runtime daemon (phase
//! 8c-3): `list_saved_sessions`, sent through pa-tui's `wire2_compat`
//! (`pa_tui::wire2_request_frames`) to the runtime daemon, streams the same
//! `session_list_item` and `session_list_progress` frames and ends with the
//! same response as the old daemon (the `pa-daemon` supervisor) sends for it
//! over one store: the pages a `limit` and its `nextCursor` walk, the
//! current-cwd scope, and the refusal of a list without a cwd. A subagent
//! only the spawn ledger keeps joins the catalog after the scan's rows, as
//! the old daemon's passive walk merges it (phase 8d-4c).
//!
//! Both sides' frames are read through `pa_types::daemon::DaemonOutbound`,
//! the shape the TUI's client takes them in; the request ids are the test's
//! own on both, and a `nextCursor` (the walk's snapshot id, a fresh UUID per
//! daemon) compares by its form. The old daemon's frames are frozen
//! ([`super::old_reference`]).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use pa_types::daemon::{DaemonCommand, DaemonOutbound};
use pa_types::wire2::{ClientFrame, ServerFrame};
use serde_json::{json, Value};

use super::old_reference::OldReference;
use super::{Bed, Client};

/// One assistant usage block that costs `cost`, billed as `billing`.
fn usage(cost: f64, billing: &Value) -> Value {
    json!({
        "input": 100, "output": 10, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 110,
        "cost": { "input": 0.0, "output": cost, "cacheRead": 0.0, "cacheWrite": 0.0,
            "total": cost },
        "billing": billing,
    })
}

/// Writes the transcript `<dir>/<id>.jsonl` (its `header`'s fields beside
/// the type, id, time and cwd, then `rows`, each given an id, a parent and a
/// time) and dates it `age_secs` ago, so the walk's newest-first order is
/// fixed.
pub(super) fn transcript(
    dir: &Path,
    id: &str,
    cwd: &str,
    header: &Value,
    rows: &[Value],
    age_secs: u64,
) {
    let mut lines = vec![json!({
        "type": "session", "version": 3, "id": id, "timestamp": "2026-09-23T00:00:00.000Z",
        "cwd": cwd,
    })];
    for (key, value) in header.as_object().unwrap() {
        lines[0][key] = value.clone();
    }
    let mut parent = Value::Null;
    for (index, row) in rows.iter().enumerate() {
        let mut row = row.clone();
        let entry = format!("{id}-{index}");
        row["id"] = json!(entry);
        row["parentId"] = parent;
        row["timestamp"] = json!("2026-09-23T00:00:01.000Z");
        parent = json!(entry);
        lines.push(row);
    }
    let path = dir.join(format!("{id}.jsonl"));
    let mut text = String::new();
    for line in &lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(age_secs))
        .unwrap();
}

/// A store of five files: two sessions in `/work/a` (one renamed, archived,
/// on a changed model and level, its spend split by billing mode), one in
/// `/work/b`, a subagent of the first, and a file whose first row is not a
/// header (walked, so counted, but no row).
fn store(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let user = |text: &str, at: u64| {
        json!({ "type": "message", "message": { "role": "user", "content": text,
            "timestamp": at } })
    };
    let assistant = |text: &str, at: u64, usage: Value| {
        json!({ "type": "message", "message": { "role": "assistant",
            "content": [{ "type": "text", "text": text }], "provider": "faux",
            "model": "faux-1", "timestamp": at, "usage": usage } })
    };
    let alpha = dir.join("alpha.jsonl");
    transcript(
        dir,
        "alpha",
        "/work/a",
        &json!({}),
        &[
            json!({ "type": "session_info", "name": "alpha work" }),
            json!({ "type": "model_change", "provider": "faux", "modelId": "faux-2" }),
            json!({ "type": "thinking_level_change", "thinkingLevel": "high" }),
            user("plan the alpha release", 1_790_110_000_000),
            assistant(
                "planned",
                1_790_110_001_000,
                usage(0.5, &json!({ "mode": "unknown" })),
            ),
            user("and ship it", 1_790_110_002_000),
            assistant(
                "shipped",
                1_790_110_003_000,
                usage(0.25, &json!({ "mode": "free" })),
            ),
            json!({ "type": "session_state", "state": { "status": "hidden" } }),
        ],
        400,
    );
    transcript(
        dir,
        "beta",
        "/work/b",
        &json!({}),
        &[
            user("beta question", 1_790_120_000_000),
            assistant(
                "beta answer",
                1_790_120_001_000,
                usage(0.1, &json!({ "mode": "free" })),
            ),
        ],
        300,
    );
    transcript(
        dir,
        "child",
        "/work/a",
        &json!({ "parentSession": alpha, "rlmDepth": 1 }),
        &[user("check the tests", 1_790_130_000_000)],
        200,
    );
    std::fs::write(
        dir.join("stray.jsonl"),
        "{\"type\":\"message\",\"id\":\"x\"}\n",
    )
    .unwrap();
    std::fs::File::open(dir.join("stray.jsonl"))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(100))
        .unwrap();
}

/// `frame` in the shape the TUI's client reads it in, its `nextCursor` by
/// its form.
pub(super) fn read_as_the_tui_does(frame: &Value) -> Value {
    let typed: DaemonOutbound = serde_json::from_value(frame.clone())
        .unwrap_or_else(|error| panic!("{frame} is not an old frame: {error}"));
    let mut frame = serde_json::to_value(typed).unwrap();
    if let Some(cursor) = frame["data"]["nextCursor"].as_str() {
        let offset = cursor.rsplit(':').next().unwrap().to_string();
        frame["data"]["nextCursor"] = json!(format!("snapshot:<id>:{offset}"));
    }
    if let Some(frame) = frame.as_object_mut() {
        // The compat layer's response carries no request id (its caller
        // pairs it); the streamed frames keep theirs.
        if frame.get("type") == Some(&json!("response")) {
            frame.remove("id");
        }
    }
    frame
}

/// The old daemon's frames for `command` (request `id`), as it sent them:
/// its streamed rows and progress, then its response.
pub(super) fn old_list(socket: &Path, id: &str, command: &Value) -> Vec<Value> {
    let stream = UnixStream::connect(socket).expect("connect to the old daemon");
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let envelope = json!({
        "type": "command",
        "id": id,
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": command,
    });
    writer
        .write_all(format!("{envelope}\n").as_bytes())
        .unwrap();
    let deadline = Instant::now() + Duration::from_mins(1);
    let mut frames = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("the old daemon closed the connection"),
            Ok(_) => {
                let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if frame["id"] != id {
                    continue;
                }
                let response = frame["type"] == "response";
                frames.push(frame);
                if response {
                    return frames;
                }
            }
            Err(_) => assert!(Instant::now() < deadline, "the old daemon never replied"),
        }
    }
}

/// The frames a client of the compat layer receives for `command` (request
/// `id`) from the runtime daemon: the subscription, then the query once it
/// is backfilled; the `streamed` frames the old daemon sent before its
/// response (the rows travel apart from the reply, so they are waited for),
/// then the response.
pub(super) async fn runtime_list(
    client: &mut Client,
    id: &str,
    command: &Value,
    streamed: usize,
) -> Vec<Value> {
    let command: DaemonCommand = serde_json::from_value(command.clone()).unwrap();
    let mut frames = pa_tui::wire2_request_frames(id.to_string(), command)
        .expect("a mapped command")
        .into_iter();
    let (Some(subscribe), Some(query), None) = (frames.next(), frames.next(), frames.next()) else {
        panic!("list_saved_sessions is a subscription and a query");
    };
    client.send(&subscribe).await;
    while !matches!(client.frame().await, ServerFrame::Backfilled { .. }) {}
    client.send(&query).await;
    let ClientFrame::Query { id: query_id, .. } = &query else {
        panic!("the second frame is the query");
    };
    let mut received = Vec::new();
    let outcome = client.reply(query_id, &mut received).await;
    let events = |received: &[ServerFrame]| {
        let events: Vec<_> = received
            .iter()
            .filter_map(|frame| match frame {
                ServerFrame::Event { event, .. } => Some(event.clone()),
                _ => None,
            })
            .collect();
        pa_tui::wire2_live_frames(&events).expect("mapped frames")
    };
    while events(&received).len() < streamed {
        received.push(client.frame().await);
    }
    let mut frames = events(&received);
    frames.push(pa_tui::wire2_response_frame("list_saved_sessions", outcome).expect("a response"));
    frames
}

/// The `nextCursor` a first page's frames end with.
fn cursor(frames: &[Value]) -> String {
    frames.last().unwrap()["data"]["nextCursor"]
        .as_str()
        .expect("the first page leaves files")
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_saved_session_list_streams_the_old_daemons_rows() {
    let bed = Bed::new();
    store(&bed.agent_dir().join("sessions"));
    let page = |cursor: Option<&str>| {
        let mut command = json!({ "type": "list_saved_sessions", "cwd": "/work/a", "limit": 2 });
        if let Some(cursor) = cursor {
            command["cursor"] = json!(cursor);
        }
        command
    };
    let scoped = json!({ "type": "list_saved_sessions", "cwd": "/work/a", "scope": "current" });
    let no_cwd = json!({ "type": "list_saved_sessions" });
    let mut reference = OldReference::open("saved_session_list", &bed);
    let old: [Vec<Value>; 4] = reference.take("lists", || {
        let _daemon = bed.old_daemon();
        let socket = bed.old_socket();
        let first = old_list(&socket, "l1", &page(None));
        let second = old_list(&socket, "l2", &page(Some(&cursor(&first))));
        [
            first,
            second,
            old_list(&socket, "l3", &scoped),
            old_list(&socket, "l4", &no_cwd),
        ]
    });
    let _daemon = bed.runtime_daemon(&json!([]));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let streamed = |frames: &[Value]| frames.len() - 1;
    let first = runtime_list(&mut client, "l1", &page(None), streamed(&old[0])).await;
    let next = page(Some(&cursor(&first)));
    let runtime = [
        first,
        runtime_list(&mut client, "l2", &next, streamed(&old[1])).await,
        runtime_list(&mut client, "l3", &scoped, streamed(&old[2])).await,
        runtime_list(&mut client, "l4", &no_cwd, streamed(&old[3])).await,
    ];
    let read = |lists: &[Vec<Value>; 4]| {
        lists
            .iter()
            .map(|frames| frames.iter().map(read_as_the_tui_does).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    // The old daemon's frames show the walk did what the fixture asks of it:
    // two files a page, the stray file walked but listed nowhere.
    assert_eq!(
        old.iter().map(Vec::len).collect::<Vec<_>>(),
        [4, 6, 6, 1],
        "the old daemon's frames: {old:#?}"
    );
    assert_eq!(read(&runtime), read(&old));
}

/// A saved parent and its subagent, whose transcript only the spawn ledger
/// leads to (under `session-artifacts`, which the catalog scan never
/// visits), and the subagent's own subagent below it.
fn passive_family(agent_dir: &Path) {
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let user = |text: &str, at: u64| {
        json!({ "type": "message", "message": { "role": "user", "content": text,
            "timestamp": at } })
    };
    transcript(
        &sessions,
        "parent",
        "/work/a",
        &json!({}),
        &[user("delegate the audit", 1_790_110_000_000)],
        300,
    );
    let parent = sessions.join("parent.jsonl");
    let kid_dir = agent_dir.join("session-artifacts/parent/sub-kid");
    std::fs::create_dir_all(&kid_dir).unwrap();
    transcript(
        &kid_dir,
        "kid",
        "/work/a",
        &json!({ "parentSession": parent }),
        &[user("audit the tests", 1_790_120_000_000)],
        200,
    );
    let kid = kid_dir.join("kid.jsonl");
    let grandkid_dir = agent_dir.join("session-artifacts/kid/sub-grandkid");
    std::fs::create_dir_all(&grandkid_dir).unwrap();
    transcript(
        &grandkid_dir,
        "grandkid",
        "/work/b",
        &json!({ "parentSession": kid }),
        &[user("audit one test", 1_790_130_000_000)],
        100,
    );
    let ledger =
        pa_core::session_engine::rlm_ledger::RlmSpawnLedger::new(agent_dir, &sessions, |_| {});
    for (child_id, parent, child, depth, name) in [
        ("sub-kid", &parent, &kid, 1, "kid"),
        (
            "sub-grandkid",
            &kid,
            &grandkid_dir.join("grandkid.jsonl"),
            2,
            "grandkid",
        ),
    ] {
        ledger
            .append_spawn(&pa_core::session_engine::rlm_ledger::RlmSpawnInput {
                child_id: child_id.to_string(),
                parent: parent.display().to_string(),
                child: child.display().to_string(),
                depth,
                name: name.to_string(),
                factory: None,
            })
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn passive_subagents_join_the_saved_catalog_as_on_the_old_daemon() {
    let bed = Bed::new();
    passive_family(&bed.agent_dir());
    let all = json!({ "type": "list_saved_sessions", "cwd": "/work/a" });
    let scoped = json!({ "type": "list_saved_sessions", "cwd": "/work/a", "scope": "current" });
    let mut reference = OldReference::open("passive_subagents", &bed);
    let old: [Vec<Value>; 2] = reference.take("lists", || {
        let _daemon = bed.old_daemon();
        let socket = bed.old_socket();
        [
            old_list(&socket, "l1", &all),
            old_list(&socket, "l2", &scoped),
        ]
    });
    let _daemon = bed.runtime_daemon(&json!([]));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let streamed = |frames: &[Value]| frames.len() - 1;
    let runtime = [
        runtime_list(&mut client, "l1", &all, streamed(&old[0])).await,
        runtime_list(&mut client, "l2", &scoped, streamed(&old[1])).await,
    ];
    let read = |lists: &[Vec<Value>; 2]| {
        lists
            .iter()
            .map(|frames| frames.iter().map(read_as_the_tui_does).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    let rows = |frames: &[Value]| -> Vec<(Value, Value, Value)> {
        frames.last().unwrap()["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["id"].clone(),
                    row["parentSessionPath"].clone(),
                    row["rlmDepth"].clone(),
                )
            })
            .collect()
    };
    let parent = bed.agent_dir().join("sessions/parent.jsonl");
    let kid = bed
        .agent_dir()
        .join("session-artifacts/parent/sub-kid/kid.jsonl");
    // The old daemon merges both descendants after the scanned parent, the
    // ledger edge their topology, and the current-cwd scope keeps the kid's
    // cwd only.
    assert_eq!(
        (rows(&old[0]), rows(&old[1])),
        (
            vec![
                (json!("parent"), Value::Null, json!(0)),
                (json!("kid"), json!(parent.display().to_string()), json!(1)),
                (
                    json!("grandkid"),
                    json!(kid.display().to_string()),
                    json!(2)
                ),
            ],
            vec![
                (json!("parent"), Value::Null, json!(0)),
                (json!("kid"), json!(parent.display().to_string()), json!(1)),
            ],
        ),
        "the old daemon's frames: {old:#?}"
    );
    assert_eq!(read(&runtime), read(&old));
}

/// `sessions.list` lists the subagents only the spawn ledger keeps below a
/// listed session as the old supervisor's `list --all` walk did: each a
/// passive row with its handle, depth and parent transcript.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_list_lists_the_passive_subagents() {
    let bed = Bed::new();
    passive_family(&bed.agent_dir());
    let _daemon = bed.runtime_daemon(&json!([]));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let listed = super::ok(client.query("sessions.list", json!({})).await);
    let passive: Vec<Value> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["rlmChildId"].is_string())
        .map(|row| {
            json!([
                row["session"],
                row["rlmChildId"],
                row["rlmDepth"],
                row["runtimeKind"]
            ])
        })
        .collect();
    assert_eq!(
        passive,
        vec![
            json!(["kid", "sub-kid", 1, "subagent"]),
            json!(["grandkid", "sub-grandkid", 2, "subagent"])
        ]
    );
}

/// `session.send` to a session an older release archived
/// (`sessions-archive/`) restores it and wakes it for the message, as the
/// old supervisor's saved-session wake does.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_wakes_an_archived_session() {
    let bed = Bed::new();
    let archive = bed.agent_dir().join("sessions-archive");
    std::fs::create_dir_all(&archive).unwrap();
    let cwd = bed.dir.path().display().to_string();
    transcript(&archive, "archived", &cwd, &json!({}), &[], 100);
    let _daemon =
        bed.runtime_daemon(&json!({ "responses": ["read it"], "repeatLastResponse": true }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let receipt = super::ok(
        client
            .call(
                "session.send",
                json!({ "session": "archived", "message": "wake up" }),
            )
            .await,
    );
    assert_eq!(receipt["woken"], json!(true));
}
