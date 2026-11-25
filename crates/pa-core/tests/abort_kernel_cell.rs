#![expect(
    clippy::large_futures,
    reason = "a session's futures are large by design, as in src/lib.rs"
)]

//! Verifier: an abort during a running kernel cell must settle the turn at once: the abort
//! interrupts the cell and returns the loop to ready. The cell writes `started` and `finished`
//! markers around a long wait, so the abort lands mid-cell.

use std::path::Path;
use std::time::Duration;

use pa_agent::scripted::{tool_call_turn_steps, ScriptedProvider, ScriptedTurn};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use serde_json::json;

/// The wedge cell: writes the `started` marker (the test aborts strictly after it), waits far
/// beyond the abort budget, then writes `finished` (must never exist after the abort settled).
fn wedge_cell(started: &Path, finished: &Path) -> String {
    format!(
        "(fs-write {started:?} \"started\")
         (let wait ([left 1200])
           (when (> left 0) (time/sleep-ms 100) (wait (- left 1))))
         (fs-write {finished:?} \"finished\")
         (displayln \"cell completed\")",
        started = started.display().to_string(),
        finished = finished.display().to_string(),
    )
}

fn scripted_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "test", "provider": "faux",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4_000
    }))
    .expect("faux loop model")
}

/// The abort budget is generous for CI but far below the cell's 120s wait: a wedge waits the
/// cell out (or forever).
#[tokio::test(flavor = "multi_thread")]
async fn abort_during_a_kernel_cell_settles_the_turn_immediately() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let started = dir.path().join("cell-started");
    let finished = dir.path().join("cell-finished");

    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));

    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
        &model,
        Some("running the wedge cell"),
        vec![(
            "call-1",
            pa_types::ai::KERNEL_TOOL_NAME,
            json!({ "code": wedge_cell(&started, &finished) }),
        )],
    )));
    provider.push_text_turn("the cell completed");

    let engine = create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir,
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(session),
        daemonless: true,
        ..Default::default()
    })
    .await
    .expect("create_session");

    // Run the prompt on a task so the test can abort mid-cell: the engine moves into the task
    // and the abort goes through the agent handle kept here.
    let agent = engine.session.agent().clone();
    let kernel = engine.kernel_provisioner_weak();
    let prompt = tokio::spawn(async move {
        engine
            .session
            .prompt(
                "run the wedge cell",
                pa_core::session_engine::PromptOptions::default(),
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(60), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the cell wrote its started marker");
    // Abort strictly mid-cell.
    agent.abort();

    // The turn settles at once: an abort wedge hangs here for the rest of the cell (or forever).
    // Either the abort surfaces as an error or the turn outcome settles; both mean the loop
    // returned.
    let _settled = tokio::time::timeout(Duration::from_secs(30), prompt)
        .await
        .expect("the aborted turn must settle within 30s")
        .expect("prompt task join");
    // The cell itself stopped: its engine is free again (a cell that ran on would hold it for
    // the rest of its wait), and it never reached its last form.
    let kernel = kernel.upgrade().expect("the session's kernel");
    tokio::time::timeout(Duration::from_secs(30), async {
        while kernel.defined_names().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the aborted cell frees the engine");
    assert_eq!(
        (finished.exists(), agent.state().await.is_streaming),
        (false, false),
        "the interrupted cell must not run on, and the agent must be idle after the aborted turn"
    );
}
