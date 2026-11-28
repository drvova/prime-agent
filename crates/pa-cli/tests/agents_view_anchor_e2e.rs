//! End-to-end verifier for the agents-view entry anchor: the agents-back
//! handoff opens the view anchored on the session just left (TS
//! `launchAgentsView`), and Enter opens the anchor's file, not the
//! first-listed session's.
#![cfg(unix)]

mod support;

use support::runtime_daemon::RuntimeDaemon;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pa_tui::agents_view::{AgentsHeadlessPlan, AgentsStep, AgentsViewOptions, AgentsViewUiMode};
use pa_tui::interactive::SessionSelection;

/// One saved-session fixture: header, display name, and an exchange.
/// Identical timestamps make the list order fall to the title tie-break.
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

/// Enter opens the anchor's file, not the first-listed session's.
#[tokio::test]
async fn entry_anchor_selects_the_left_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");

    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let daemon = RuntimeDaemon::spawn(dir.path());

    // The fixture roster: rows sort by last activity (mtime), so the cron session
    // lists first — the anchor is the second-listed gateway session.
    let gateway_path = write_fixture(
        &session_dir,
        "gateway-01",
        "gateway worker",
        &[("deploy the gateway", "gateway deployed")],
    );
    let cron_path = write_fixture(
        &session_dir,
        "cron-01",
        "cron keeper",
        &[("audit the cron jobs", "cron audit clean")],
    );

    let options = AgentsViewOptions {
        socket_path: daemon.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        anchor_session_id: Some("gateway-01".to_string()),
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
    };
    let plan = AgentsHeadlessPlan {
        steps: vec![
            // The roster snapshot precedes streaming pushes and the saved catalog lands right after
            // open; settle before the open key.
            AgentsStep::WaitSettle { timeout_ms: 1000 },
            AgentsStep::Key("enter".to_string()),
        ],
        width: 120,
        height: 36,
    };
    let outcome =
        pa_tui::agents_view::run_agents_view(options, AgentsViewUiMode::Headless(plan), None)
            .await
            .expect("agents view run")
            .outcome;
    assert!(!outcome.frames.is_empty(), "frames were captured");
    // Enter opened the anchor's file, not the first-listed cron session's.
    assert_eq!(
        outcome.selection,
        Some(SessionSelection::Resume(gateway_path.clone())),
        "the anchor row, not the first row, opens"
    );
    assert_ne!(
        outcome.selection,
        Some(SessionSelection::Resume(cron_path.clone())),
        "the first row is not the opened one"
    );
}

/// The `--continue` launch's view contract (P6 continue-recent safety): preselect the
/// newest saved session; Enter opens the candidate — the user confirms.
#[tokio::test]
async fn continue_recent_view_preselects_the_candidate_and_renders_the_notice() {
    let dir = tempfile::TempDir::new().expect("temp dir");

    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let daemon = RuntimeDaemon::spawn(dir.path());

    // Two saved sessions for the launch cwd; the second write is the newest, so
    // the continue candidate is the second one even though the first would list first.
    let older_path = write_fixture(
        &session_dir,
        "aaaa-candidate",
        "alpha session",
        &[("first turn", "first reply")],
    );
    let candidate_path = write_fixture(
        &session_dir,
        "bbbb-candidate",
        "beta session",
        &[("second turn", "second reply")],
    );
    // The newest file strictly newer (same-mtime granularity guard).
    let future = std::time::SystemTime::now() + std::time::Duration::from_mins(1);
    let handle = std::fs::File::options()
        .append(true)
        .open(&candidate_path)
        .expect("open candidate");
    handle.set_modified(future).expect("nudge mtime");

    let notice = format!(
        "Most recent session for this directory: {} — Enter continues it, or pick another session.",
        "bbbb-candidate"
    );
    let options = AgentsViewOptions {
        socket_path: daemon.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        anchor_session_id: Some("bbbb-candidate".to_string()),
        scope: None,
        folder_scope: None,
        query: None,
        expanded_ancestors: Vec::new(),
        selected_row_identity: None,
        selected_key: None,
        status_message: Some(notice.clone()),
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        show_hardware_cursor: false,
        incident_notice_state: None,
        create_config: serde_json::json!({}),
    };
    let plan = AgentsHeadlessPlan {
        steps: vec![
            // The saved catalog lands right after open; settle so the anchor preselection applies
            // before the open key.
            AgentsStep::WaitSettle { timeout_ms: 1000 },
            AgentsStep::Key("enter".to_string()),
        ],
        width: 120,
        height: 36,
    };
    let outcome =
        pa_tui::agents_view::run_agents_view(options, AgentsViewUiMode::Headless(plan), None)
            .await
            .expect("agents view run")
            .outcome;
    assert!(!outcome.frames.is_empty(), "frames were captured");
    let rendered = outcome.frames.join("\n");
    assert!(
        rendered.contains("Most recent session for this directory: bbbb-candidate"),
        "the continue notice rendered in the status line:\n{rendered}"
    );
    assert_eq!(
        outcome.selection,
        Some(SessionSelection::Resume(candidate_path.clone())),
        "the continue candidate opened"
    );
    assert_ne!(
        outcome.selection,
        Some(SessionSelection::Resume(older_path.clone())),
        "the first-listed row did not open"
    );
    drop(daemon);
}

/// Through the real daemon (`rename_saved_session` wire path, which a unit test cannot
/// exercise): the row renames in place, the file gains the name.
#[tokio::test]
async fn rename_saved_session_renames_the_row_and_the_file() {
    let dir = tempfile::TempDir::new().expect("temp dir");

    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let daemon = RuntimeDaemon::spawn(dir.path());
    let path = write_fixture(
        &session_dir,
        "ren-01",
        "gateway worker",
        &[("rename me", "renamed by the daemon")],
    );

    let options = AgentsViewOptions {
        socket_path: daemon.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
        theme: "prime".to_string(),
        version: "0.0.0".to_string(),
        anchor_session_id: Some("ren-01".to_string()),
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
    };
    let plan = AgentsHeadlessPlan {
        steps: vec![
            // The saved catalog lands right after open; settle so the fixture's row is selectable
            // before the rename key.
            AgentsStep::WaitSettle { timeout_ms: 1000 },
            AgentsStep::Key("ctrl+r".to_string()),
            AgentsStep::Key("ctrl+u".to_string()),
            AgentsStep::Type("renamed agent".to_string()),
            AgentsStep::Key("enter".to_string()),
            AgentsStep::WaitRender {
                needle: "Renamed to renamed agent".to_string(),
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
        rendered.contains("Renamed to renamed agent"),
        "the rename's TS status row rendered:\n{rendered}"
    );
    assert!(
        outcome
            .frames
            .last()
            .is_some_and(|frame| frame.contains("renamed agent")),
        "the final frame lists the new name:\n{rendered}"
    );
    // The old name left the row: the catalog loads once, never refetched, so
    // this pins the in-place saved-row patch in `rename_result`.
    assert!(
        outcome
            .frames
            .last()
            .is_some_and(|frame| !frame.contains("gateway worker")),
        "the final frame dropped the old name:\n{rendered}"
    );
    let content = support::saved_transcript(&agent_dir, &path);
    assert!(
        content.contains("\"name\":\"renamed agent\""),
        "the fixture gained the session_info name entry:\n{content}"
    );
    drop(daemon);
}
