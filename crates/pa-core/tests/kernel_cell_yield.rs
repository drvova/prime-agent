// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! A cell still running when its `timeout` (the yield window) passes continues in
//! the background, as Nodecode's evaluations do: the tool result says so with its
//! output so far, nothing interrupts it, a cell sent meanwhile is refused without
//! running (the session's one engine is held), and its end arrives as a `[cell-done]`
//! notice that wakes the session.
#![cfg(unix)]

use pa_agent::scripted::{tool_call_turn_steps, ScriptedProvider, ScriptedTurn};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use serde_json::{json, Value};

fn scripted_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "test", "provider": "faux",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4_000
    }))
    .expect("faux loop model")
}

/// The `(details.status, text)` of the tool result for `call_id` in the session rows.
fn tool_result(entries: &Value, call_id: &str) -> (String, String) {
    let message = entries
        .as_array()
        .expect("entries array")
        .iter()
        .filter_map(|entry| entry.get("message"))
        .find(|message| message["role"] == "toolResult" && message["toolCallId"] == call_id)
        .unwrap_or_else(|| panic!("no tool result for {call_id}: {entries}"));
    let text = message["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let status = message["details"]["status"]
        .as_str()
        .unwrap_or_else(|| panic!("no details.status for {call_id}: {message}"))
        .to_string();
    (status, text)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cell_past_its_window_continues_in_the_background_and_its_end_wakes_the_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let flag = dir.path().join("go");

    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));

    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    let long_cell = format!(
        "(displayln \"before\")
         (let wait ()
           (when (null? (hash-ref (fs-glob \"go\" (hash 'path {dir:?})) 'entries))
             (time/sleep-ms 20)
             (wait)))
         (displayln \"after\")
         \"finished\"",
        dir = dir.path().display().to_string(),
    );
    for (call_id, args) in [
        ("call-1", json!({ "code": "(define x 41)" })),
        ("call-2", json!({ "code": long_cell, "timeout": 1 })),
        (
            "call-3",
            json!({ "code": "(display (+ x 1))", "timeout": 1 }),
        ),
    ] {
        provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
            &model,
            None,
            vec![(call_id, pa_types::ai::KERNEL_TOOL_NAME, args)],
        )));
    }
    provider.push_text_turn("done");

    // The notice's wake is the observable event: the test awaits it.
    let woken = std::sync::Arc::new(tokio::sync::Notify::new());
    let wake = std::sync::Arc::clone(&woken);
    let engine = create_session(SessionEngineConfig {
        on_late_sent_agent_message: None,
        semantic_edges: None,
        cron_store: None,
        steering_mode: None,
        follow_up_mode: None,
        factory_child: false,
        cwd: cwd.clone(),
        agent_dir,
        model: Some(model),
        thinking_level: None,
        stream_fn: Some(provider.stream_fn()),
        tools: Vec::new(),
        custom_system_prompt: None,
        prompt_guidelines: Vec::new(),
        generic_mcp_servers: Vec::new(),
        allow_recursion: None,
        session_manager: Some(session),
        extra_host_handlers: None,
        conversation_log_path: None,
        additional_skill_paths: Vec::new(),
        additional_prompt_paths: Vec::new(),
        extra_builtin_skill_overrides: Vec::new(),
        rlm_subagent_host: None,
        rlm_usage_store: None,
        progress_note_emit: None,
        interaction: None,
        rlm_depth: None,
        daemonless: false,
        telemetry: None,
        model_info: None,
        mcp_manager: None,
        prewarm_kernel: None,
        on_background_work_settled: None,
        // Registers the in-process notice handlers (bash and cell) with this wake.
        bash_notice_wake: Some(std::sync::Arc::new(move || wake.notify_one())),
        queued_goal_context_purge: None,
        queued_steering_probe: None,
        should_stop_after_turn: None,
        image_model_router: None,
    })
    .await
    .expect("create_session");

    let notified = woken.notified();
    engine
        .session
        .prompt(
            "run the cells",
            pa_core::session_engine::PromptOptions::default(),
        )
        .await
        .expect("prompt");
    let entries = serde_json::to_value(engine.session.entries().await).expect("entries json");

    // The over-window cell was not stopped: it ended the call at its window with
    // its output so far and the running envelope.
    let (status, text) = tool_result(&entries, "call-2");
    assert_eq!(status, "backgrounded", "{text}");
    assert!(text.starts_with("before\n\n[cell "), "{text}");
    assert!(text.contains("continues in the background"), "{text}");

    // The next cell did not run while it held the engine, and says which cell does.
    let (status, text) = tool_result(&entries, "call-3");
    assert_eq!(
        (status.as_str(), text.starts_with("This cell did not run. ")),
        ("error", true),
        "{text}"
    );

    // Released after the run ended, its end wakes the session with the
    // [cell-done] row queued on the agent.
    std::fs::write(&flag, "").expect("release the cell");
    tokio::time::timeout(std::time::Duration::from_secs(30), notified)
        .await
        .expect("the cell's end must wake the session");
    let queued = engine.session.agent().remove_queued_messages(|_| true);
    let [pa_agent::types::AgentMessage::Custom(custom)] = queued.as_slice() else {
        panic!("exactly the cell's notice row queued: {queued:?}");
    };
    let details = &custom.payload["details"];
    let cell = details["cell"].as_str().expect("the row names its cell");
    assert_eq!(
        (
            &custom.payload["customType"],
            &custom.payload["content"],
            details
        ),
        (
            &json!("async_cell_completion"),
            &json!(format!(
                "[cell-done cell:{cell} status:ok]\n\nValue:\n```\n\"finished\"\n```\n\nOutput:\n```\nafter\n```"
            )),
            &json!({ "cell": cell, "status": "ok" }),
        )
    );
}
