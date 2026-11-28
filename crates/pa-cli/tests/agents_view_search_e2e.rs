#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "`search_matches_names_ids_and_cwd_never_transcripts` is one scenario, top to bottom"
    )
)]

//! End-to-end verifier for the agents-view session search (Kevin's
//! 2026-09-23 directive): queries match the session NAME, the durable
//! session ID, and the CWD — never first messages, transcript text, or
//! file paths — and hits render as one flat, relevance-ranked list.
#![cfg(unix)]

mod support;

use support::runtime_daemon::RuntimeDaemon;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pa_tui::agents_view::{AgentsHeadlessPlan, AgentsStep, AgentsViewOptions, AgentsViewUiMode};
use pa_tui::interactive::SessionSelection;

/// One saved-session fixture: header, display name, and a user/assistant
/// exchange whose texts feed the name/first-message/transcript search.
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

/// Writes every captured frame under `PA_SEARCH_FRAMES_DIR` when set: the
/// before/after evidence capture for the search redesign.
fn dump_frames(label: &str, frames: &[String]) {
    let Some(dir) = std::env::var("PA_SEARCH_FRAMES_DIR")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    std::fs::create_dir_all(&dir).expect("frames dir");
    for (index, frame) in frames.iter().enumerate() {
        std::fs::write(
            std::path::Path::new(&dir).join(format!("{label}-{index:03}.txt")),
            frame,
        )
        .expect("frame dump");
    }
}

#[tokio::test]
async fn search_matches_names_ids_and_cwd_never_transcripts() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let daemon = RuntimeDaemon::spawn(dir.path());

    // The fixture roster: a named session whose transcript ALSO mentions the query word,
    // siblings whose transcripts mention it but names do not, an id-only target.
    let fast_path = write_fixture(
        &session_dir,
        "fast-01",
        "fast lane refactor",
        &[("make the suite fast", "the gateway deploy is fast")],
    );
    let gateway_path = write_fixture(
        &session_dir,
        "gateway-01",
        "gateway worker",
        &[(
            "deploy the gateway",
            "done; bumped the backoff ceiling to 30s",
        )],
    );
    let migration_path = write_fixture(
        &session_dir,
        "migration-01",
        "migration runner",
        &[(
            "run the migrations now",
            "migration 001 applied; it was fast",
        )],
    );
    write_fixture(
        &session_dir,
        "probe-01",
        "alpha probe",
        &[("probe the endpoint", "alpha probe complete")],
    );

    let options = AgentsViewOptions {
        socket_path: daemon.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
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
    };
    let plan = AgentsHeadlessPlan {
        steps: vec![
            // The full-roster gate: all four fixtures sit in the row model before
            // any query filters them, so every later settle rides the render cadence.
            AgentsStep::WaitRender {
                needle: "4 inactive".to_string(),
                timeout_ms: 10_000,
            },
            // A noisy word: transcripts mention it, names mostly do not.
            AgentsStep::Type("fast".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
            AgentsStep::Key("escape".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
            // Transcript-only text: no identity field carries it.
            AgentsStep::Type("backoff".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
            AgentsStep::Key("escape".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
            // Fuzzy name fragment ranks the gateway session first.
            AgentsStep::Type("gtwy".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
            AgentsStep::Key("escape".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
            // Durable-id targeting: the id fragment lives nowhere else.
            AgentsStep::Type("probe-0".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
            AgentsStep::Key("escape".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
            AgentsStep::Type("gateway".to_string()),
            AgentsStep::WaitSettle { timeout_ms: 300 },
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
    dump_frames("lane", &outcome.frames);

    // Name query: only the named hit renders — the sibling transcripts that mention "fast" stay
    // hidden.
    let fast_frame = frame_of(&outcome.frames, " >  fast");
    assert!(
        fast_frame.contains("fast lane refactor"),
        "the named hit renders:\n{fast_frame}"
    );
    for absent in [
        "gateway worker",
        "migration runner",
        "alpha probe",
        "Inactive (",
    ] {
        assert!(
            !fast_frame.contains(absent),
            "{absent:?} hides under the query (flat ranked list):\n{fast_frame}"
        );
    }

    // Transcript-only text never matches: content is not a picker filter.
    let backoff_frame = frame_of(&outcome.frames, " >  backoff");
    assert!(
        backoff_frame.contains("No sessions match"),
        "transcript-only queries match nothing:\n{backoff_frame}"
    );

    // Fuzzy name fragment: the gateway session is the sole hit.
    let gtwy_frame = frame_of(&outcome.frames, " >  gtwy");
    assert!(
        gtwy_frame.contains("gateway worker"),
        "the fuzzy name fragment matches:\n{gtwy_frame}"
    );
    assert!(
        !gtwy_frame.contains("migration runner"),
        "unrelated names hide under the fuzzy query:\n{gtwy_frame}"
    );

    // Id targeting: "probe-0" hits only through the session id.
    let id_frame = frame_of(&outcome.frames, " >  probe-0");
    assert!(
        id_frame.contains("alpha probe"),
        "the id target renders:\n{id_frame}"
    );
    assert!(
        !id_frame.contains("gateway worker"),
        "other rows hide under the id query:\n{id_frame}"
    );

    assert_eq!(
        outcome.selection,
        Some(SessionSelection::Resume(gateway_path)),
        "Enter opened the filtered match"
    );
    drop((fast_path, migration_path));
    drop(daemon);
}

#[tokio::test]
async fn ranked_hits_sort_by_relevance_then_recency() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let daemon = RuntimeDaemon::spawn(dir.path());

    // Three name-prefix hits for "run": two same-length prefixes (tie on tier
    // quality, title breaks the tie) and one longer prefix ranking below.
    write_fixture(
        &session_dir,
        "run-books-01",
        "run books",
        &[("start", "done")],
    );
    write_fixture(
        &session_dir,
        "run-dials-01",
        "run dials",
        &[("start", "done")],
    );
    let runway_path = write_fixture(
        &session_dir,
        "runway-01",
        "runway cleanup",
        &[("start", "done")],
    );

    let options = AgentsViewOptions {
        socket_path: daemon.socket.clone(),
        cwd: dir.path().to_path_buf(),
        session_dir: Some(session_dir.clone()),
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
    };
    let plan = AgentsHeadlessPlan {
        steps: vec![
            AgentsStep::Type("run".to_string()),
            // The full-catalog count gate: the plan cannot reach Done before the scan's rows
            // rendered under the query (red-agentsview-search-ranked-hits-20260926-1).
            AgentsStep::WaitRender {
                needle: "3 inactive".to_string(),
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
    dump_frames("ranked", &outcome.frames);
    let frame = frame_of(&outcome.frames, " >  run");
    let positions: Vec<usize> = ["run books", "run dials", "runway cleanup"]
        .iter()
        .map(|needle| {
            frame
                .find(needle)
                .unwrap_or_else(|| panic!("row {needle:?} missing from:\n{frame}"))
        })
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "same-length prefixes tie by title, the longer prefix ranks last:\n{frame}"
    );
    drop(runway_path);
    drop(daemon);
}
