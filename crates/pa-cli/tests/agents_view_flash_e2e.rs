#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "the clean-first-render and stopped-session scenarios each read top to bottom"
    )
)]

//! End-to-end verifier for the agents-view flash (the operator's fleet box:
//! a departed root's registration-seeded rows stayed top-level until the
//! catalog re-parented them minutes later): the first render must show the
//! live roster alone; the catalog keeps every dead row resumable.
#![cfg(unix)]

mod support;

use support::runtime_daemon::RuntimeDaemon;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pa_core::session_engine::rlm_ledger::{RlmSpawnInput, RlmSpawnLedger};
use pa_tui::agents_view::{AgentsHeadlessPlan, AgentsStep, AgentsViewOptions, AgentsViewUiMode};
use serde_json::{json, Value};

/// One saved-session fixture: a version-3 session header (the parent
/// linkage for subagents), a display name, and a user/assistant exchange.
fn write_fixture(
    dir: &Path,
    id: &str,
    name: &str,
    parent: Option<&Path>,
    rlm_depth: u64,
    turns: &[(&str, &str)],
) -> PathBuf {
    let path = dir.join(format!("{id}.jsonl"));
    let mut content = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2024-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\""
    );
    if let Some(parent) = parent {
        let _ = write!(content, ",\"parentSession\":\"{}\"", parent.display());
    }
    let _ = write!(content, ",\"rlmDepth\":{rlm_depth}}}");
    content.push('\n');
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

/// The rows of one roster snapshot whose summary carries the dead family's
/// marker (a `rlmChildId` under the parent, or the parent's own session file).
fn family_rows(roster: &[Value], parent_file: &Path, child_prefix: &str) -> usize {
    roster
        .iter()
        .filter(|entry| {
            let summary = &entry["summary"];
            summary["rlmChildId"]
                .as_str()
                .is_some_and(|id| id.starts_with(child_prefix))
                || summary["sessionFile"].as_str() == Some(&parent_file.to_string_lossy())
        })
        .count()
}

fn roster_of(response: &Value) -> Vec<Value> {
    response["roster"].as_array().cloned().unwrap_or_default()
}

/// Whether a `runtime/roster` update changes or removes the row of session file `file`, or of
/// session `id`.
fn touches(update: &Value, file: &Path, id: Option<&str>) -> bool {
    let file = file.to_string_lossy();
    update["changed"].as_array().is_some_and(|changed| {
        changed
            .iter()
            .any(|entry| entry["summary"]["sessionFile"].as_str() == Some(file.as_ref()))
    }) || id.is_some_and(|id| {
        update["removed"]
            .as_array()
            .is_some_and(|removed| removed.iter().any(|row| row.as_str() == Some(id)))
    })
}

/// The create of the fixture session `file`, resumed on the battery's mock model.
fn create_args(file: &Path, cwd: &Path, sessions_dir: &Path) -> Value {
    json!({
        "path": file.to_string_lossy(),
        "cwd": cwd.to_string_lossy(),
        "sessionDir": sessions_dir.to_string_lossy(),
        "options": { "provider": "battery", "model": "mock-1", "thinking": "low" },
    })
}

/// The first frame of one headless view run (the mount draw precedes the saved-catalog fetch).
fn first_frame(frames: &[String]) -> String {
    frames
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("no frame rendered"))
}

fn frame_of(frames: &[String], marker: &str) -> String {
    frames
        .iter()
        .rev()
        .find(|frame| frame.contains(marker))
        .unwrap_or_else(|| {
            panic!(
                "no frame shows {marker:?}; frames:\n{}",
                frames.join("\n---frame---\n")
            )
        })
        .clone()
}

fn view_options(socket: &Path, session_dir: &Path) -> AgentsViewOptions {
    AgentsViewOptions {
        socket_path: socket.to_path_buf(),
        cwd: PathBuf::from("/tmp"),
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
        create_config: serde_json::json!({}),
    }
}

/// The dead family's children count: hundreds, like the operator's box.
const DEAD_CHILDREN: usize = 300;

#[tokio::test]
async fn the_first_agents_view_render_is_clean_behind_hundreds_of_dead_subagents() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    // The saved catalog reports canonical parent paths; macOS temp dirs sit behind the
    // /var -> /private/var symlink.
    let root = pa_types::platform::canonicalize(dir.path()).expect("canonical temp dir");
    let agent_dir = root.join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("session dir");

    // The mock provider registers without any prompt ever running (the create and the stop are
    // turn-free paths).
    std::fs::write(
        agent_dir.join("models.json"),
        json!({
            "providers": {
                "battery": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "apiKey": "sk-battery",
                    "models": [
                        {
                            "id": "mock-1",
                            "name": "Mock 1",
                            "api": "openai-completions",
                            "reasoning": true,
                            "contextWindow": 128_000,
                            "maxTokens": 4096
                        }
                    ]
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");

    // The dead family: one parent in the sessions dir and hundreds of subagent
    // transcripts under session-artifacts (the RLM layout the scan never visits).
    let parent_file = write_fixture(
        &sessions_dir,
        "flash-parent",
        "flash parent",
        None,
        0,
        &[("run the fleet drill", "the drill ran")],
    );
    let artifacts_dir = agent_dir.join("session-artifacts").join("flash-parent");
    let ledger = RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
    for index in 0..DEAD_CHILDREN {
        let child_dir = artifacts_dir.join(format!("sub-{index}"));
        std::fs::create_dir_all(&child_dir).expect("child dir");
        let child_file = write_fixture(
            &child_dir,
            &format!("flash-child-{index:03}"),
            &format!("flash worker {index:03}"),
            Some(&parent_file),
            1,
            &[("do the work", "work complete")],
        );
        ledger
            .append_spawn(&RlmSpawnInput {
                child_id: format!("sub-{index}"),
                parent: parent_file.to_string_lossy().to_string(),
                child: child_file.to_string_lossy().to_string(),
                depth: 1,
                name: format!("flash worker {index:03}"),
                factory: None,
            })
            .expect("append spawn edge");
    }

    let daemon = RuntimeDaemon::spawn(&root);
    let mut client = daemon.client();

    // Before the session opens, the roster is clean of the dead family.
    let before = client
        .query("runtime.roster", &json!({}))
        .expect("runtime.roster");
    assert_eq!(
        family_rows(&roster_of(&before), &parent_file, "sub-"),
        0,
        "an unopened family never reaches the roster: {before}"
    );

    // The parent opens (the create resumes the fixture file): the registration seed walks the
    // live ledger family — TS parity while resident.
    client.subscribe("runtime/roster");
    let created = client
        .call(
            "session.create",
            &create_args(&parent_file, &root, &sessions_dir),
        )
        .expect("create");
    let active_id = created["session"]
        .as_str()
        .expect("the active session id")
        .to_string();
    client.event_until("runtime/roster", |update| {
        touches(update, &parent_file, None)
    });
    let resident = client
        .query("runtime.roster", &json!({}))
        .expect("runtime.roster");
    assert_eq!(
        family_rows(&roster_of(&resident), &parent_file, "sub-"),
        DEAD_CHILDREN + 1,
        "a resident root's own row plus its seeded family serves to subscribers: {resident}"
    );

    // The session stops (the operator's stale-worker stop): the SEEDED rows
    // settle out of the roster with the stop, while the parent's own TOP-LEVEL
    // row PASSIVATES and stays (TS `flipWorkerRosterEntriesInactive` keeps every
    // stopped non-ephemeral row visible).
    client
        .call("session.stop", &json!({ "session": active_id }))
        .expect("stop");
    client.event_until("runtime/roster", |update| {
        touches(update, &parent_file, Some(&active_id))
    });
    let departed = client
        .query("runtime.roster", &json!({}))
        .expect("runtime.roster");
    let departed_roster = roster_of(&departed);
    let seeded_children = departed_roster
        .iter()
        .filter(|entry| {
            entry["summary"]["rlmChildId"]
                .as_str()
                .is_some_and(|id| id.starts_with("sub-"))
        })
        .count();
    assert_eq!(
        seeded_children, 0,
        "the departed family's seeded rows left the roster: {departed}"
    );
    let passivated_parent = departed_roster
        .iter()
        .find(|entry| {
            entry["summary"]["sessionFile"].as_str() == Some(parent_file.to_string_lossy().as_ref())
        })
        .unwrap_or_else(|| {
            panic!("the stopped parent's row stays in the roster (passivated): {departed}")
        });
    assert_eq!(
        passivated_parent["status"], "inactive",
        "the stopped parent's row passivated: {passivated_parent}"
    );

    // The saved catalog keeps every dead row resumable (the passive ledger walk).
    let saved = client
        .query(
            "sessions.list",
            &json!({ "shape": "saved", "cwd": root.to_string_lossy() }),
        )
        .expect("the saved catalog");
    let parent_path = parent_file.to_string_lossy().to_string();
    let children = saved["sessions"].as_array().map_or_default(|rows| {
        rows.iter()
            .filter(|row| row["parentSessionPath"].as_str() == Some(parent_path.as_str()))
            .count()
    });
    assert_eq!(children, DEAD_CHILDREN, "the dead rows stay resumable");

    // The first frame renders the live roster the moment the surface mounts
    // (the saved-catalog fetch has not spawned yet) — never the dead family
    // as top-level entries. The settled frame carries the family through
    // the parent's collapsed tree.
    let plan = AgentsHeadlessPlan {
        steps: vec![
            AgentsStep::WaitSettle { timeout_ms: 2_000 },
            AgentsStep::Key("alt+right".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 500 },
        ],
        width: 120,
        height: 36,
    };
    let view = pa_tui::agents_view::run_agents_view(
        view_options(&daemon.socket, &sessions_dir),
        AgentsViewUiMode::Headless(plan),
        None,
    )
    .await
    .expect("agents view run")
    .outcome;

    let first = first_frame(&view.frames);
    assert!(
        !first.contains("flash worker"),
        "the first render shows no dead family rows:\n{first}"
    );
    let settled = frame_of(&view.frames, "flash parent");
    assert!(
        settled.contains("300 subagents · 0 running"),
        "the settled frame carries the dead family behind the parent's ONE collapsed line:\n{settled}"
    );
    let expanded = frame_of(&view.frames, "flash worker 007");
    assert!(
        expanded.contains("▾ 300 subagents · 0 running"),
        "the expansion opens the parent's one group and flips its marker:\n{expanded}"
    );
}

/// A stopped session's row STAYS visible (TS `flipWorkerRosterEntriesInactive`; the Rust
/// stop used to delete it — the operator's rows-disappear report).
#[tokio::test]
async fn a_stopped_session_stays_visible_in_the_view() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("session dir");

    std::fs::write(
        agent_dir.join("models.json"),
        json!({
            "providers": {
                "battery": {
                    "api": "openai-completions",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "apiKey": "sk-battery",
                    "models": [
                        {
                            "id": "mock-1",
                            "name": "Mock 1",
                            "api": "openai-completions",
                            "reasoning": true,
                            "contextWindow": 128_000,
                            "maxTokens": 4096
                        }
                    ]
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");

    let stopped_file = write_fixture(
        &sessions_dir,
        "kept-session",
        "kept session",
        None,
        0,
        &[("run the drill", "the drill ran")],
    );

    let daemon = RuntimeDaemon::spawn(dir.path());
    let mut client = daemon.client();

    // The session opens, then stops: the stop's roster update must PASSIVATE the row (a
    // `changed` update), never remove it.
    client.subscribe("runtime/roster");
    let created = client
        .call(
            "session.create",
            &create_args(&stopped_file, dir.path(), &sessions_dir),
        )
        .expect("create");
    let active_id = created["session"]
        .as_str()
        .expect("the active session id")
        .to_string();
    client.event_until("runtime/roster", |update| {
        touches(update, &stopped_file, None)
    });
    client
        .call("session.stop", &json!({ "session": active_id }))
        .expect("stop");
    client.event_until("runtime/roster", |update| {
        touches(update, &stopped_file, Some(&active_id))
    });

    // The snapshot serves the passivated row (the view's roster half).
    let roster = client
        .query("runtime.roster", &json!({}))
        .expect("runtime.roster");
    let entries = roster["roster"].as_array().cloned().unwrap_or_default();
    let entry = entries
        .iter()
        .find(|entry| entry["summary"]["sessionName"] == json!("kept session"))
        .unwrap_or_else(|| {
            panic!("the stopped session's passivated row is in the snapshot: {entries:?}")
        });
    assert_eq!(
        entry["status"], "inactive",
        "the row is passivated: {entry}"
    );
    assert_eq!(
        entry["summary"]["lifecycle"], "live",
        "lifecycle stays live (TS passivation): {entry}"
    );

    // The view's FIRST frame shows the stopped session in the Inactive section — from the roster
    // alone, before the catalog loads.
    let plan = AgentsHeadlessPlan {
        steps: vec![AgentsStep::WaitSettle { timeout_ms: 2_000 }],
        width: 120,
        height: 36,
    };
    let view = pa_tui::agents_view::run_agents_view(
        view_options(&daemon.socket, &sessions_dir),
        AgentsViewUiMode::Headless(plan),
        None,
    )
    .await
    .expect("agents view run")
    .outcome;
    let first = first_frame(&view.frames);
    assert!(
        first.contains("kept session"),
        "the stopped session's row paints on the FIRST frame (TS keeps stopped rows visible):
{first}"
    );
    assert!(
        first.contains("Inactive"),
        "the stopped session renders in the Inactive section:
{first}"
    );
}
