#![cfg_attr(
    unix,
    expect(
        clippy::large_futures,
        reason = "a session's futures are large by design, as in src/lib.rs"
    )
)]

//! Live kernel: one cell runs a whole retry loop at kernel speed and pays a model call only at
//! its decision points (`rlm.decide`), bounded per cell, each shown as one compact row in the
//! cell's output.
#![cfg(unix)]

use std::fmt::Write as _;

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
async fn one_cell_loop_pays_a_model_call_only_per_decision_up_to_the_bound() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");

    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));

    // One cell: a ten-step loop whose kernel work is free, with a decision at every step.
    let code = "(define (attempts step picked)
                  (if (= step 10)
                      picked
                      (let ([choice (with-handler
                                      (lambda (raised) (list (error-object-message raised)))
                                      (rlm-decide (string-append \"attempt \" (number->string step) \" failed; next?\")
                                                  (list \"stop\" \"retry\")
                                                  #:context \"1 failed\"))])
                        (if (pair? choice)
                            (cons (car choice) picked)
                            (attempts (+ step 1) (cons choice picked))))))
                (for-each displayln (reverse (attempts 0 '())))";
    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
        &model,
        None,
        vec![(
            "call-1",
            pa_types::ai::KERNEL_TOOL_NAME,
            json!({ "code": code }),
        )],
    )));
    // The eight decisions the bound allows; a ninth would exhaust the script.
    for _ in 0..8 {
        provider.push_text_turn("2");
    }
    provider.push_text_turn("done");

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
        bash_notice_wake: None,
        call_policy: None,
        queued_goal_context_purge: None,
        queued_steering_probe: None,
        should_stop_after_turn: None,
        image_model_router: None,
    })
    .await
    .expect("create_session");

    engine
        .session
        .prompt("loop it", pa_core::session_engine::PromptOptions::default())
        .await
        .expect("prompt");
    let entries = serde_json::to_value(engine.session.entries().await).expect("entries json");

    let mut expected = String::new();
    for attempt in 0..8 {
        writeln!(expected, "[decide] attempt {attempt} failed; next? → retry").unwrap();
    }
    for _ in 0..8 {
        writeln!(expected, "retry").unwrap();
    }
    writeln!(
        expected,
        " rlm.decide: this cell used its 8 decisions; decide the rest in code or end the cell and decide in the next turn"
    )
    .unwrap();
    let (status, text) = tool_result(&entries, "call-1");
    assert_eq!((status.as_str(), text.as_str()), ("ok", expected.as_str()));
    // The turn, eight decisions, and the closing turn: nothing else asked the model.
    assert_eq!(provider.calls().len(), 10);
}
