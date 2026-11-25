#![cfg_attr(
    unix,
    expect(
        clippy::large_futures,
        reason = "a session's futures are large by design, as in src/lib.rs"
    )
)]

//! Live kernel: a cell prints far past the transcript cap, its result carries the
//! spill's handle, and a later cell pages the middle back in with `ctx.page`
//! instead of re-running the command.
#![cfg(unix)]

use std::fmt::Write as _;

use std::path::PathBuf;

use pa_agent::scripted::{tool_call_turn_steps, ScriptedProvider, ScriptedTurn};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use serde_json::{json, Value};
use sha2::Digest as _;

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

/// A persistent session on `provider`'s script (no telemetry, no tools beyond the kernel's).
async fn engine_for(
    session: SessionManager,
    agent_dir: PathBuf,
    cwd: PathBuf,
    model: pa_agent::types::Model,
    provider: &std::sync::Arc<ScriptedProvider>,
) -> pa_core::session_engine::engine::SessionEngine {
    create_session(SessionEngineConfig {
        on_late_sent_agent_message: None,
        semantic_edges: None,
        cron_store: None,
        steering_mode: None,
        follow_up_mode: None,
        factory_child: false,
        cwd,
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
    .expect("create_session")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_cell_pages_the_middle_of_a_huge_output_back_in() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");

    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));

    // What the first cell prints: 20,000 lines, 240,000 characters (a cell keeps at most
    // 262,144 bytes of what it printed, and its result shows 65,536 characters).
    let printed: String = (0..20_000).fold(String::new(), |mut out, n| {
        let _ = writeln!(out, "line {n:06}");
        out
    });
    let handle = format!(
        "out:{}",
        &format!("{:x}", sha2::Sha256::digest(printed.as_bytes()))[..10]
    );
    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    for (call_id, args) in [
        (
            "call-1",
            json!({ "code": "(let count ([n 0])
                               (when (< n 20000)
                                 (let ([digits (number->string n)])
                                   (displayln (string-append \"line \" (make-string (- 6 (string-length digits)) #\\0) digits)))
                                 (count (+ n 1))))" }),
        ),
        (
            "call-2",
            json!({ "code": format!("(displayln (hash-ref (ctx-page {handle:?} #:start 10000 #:lines 3) 'text))") }),
        ),
    ] {
        provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
            &model,
            None,
            vec![(call_id, pa_types::ai::KERNEL_TOOL_NAME, args)],
        )));
    }
    provider.push_text_turn("done");

    let engine = engine_for(session, agent_dir, cwd, model, &provider).await;

    engine
        .session
        .prompt("page it", pa_core::session_engine::PromptOptions::default())
        .await
        .expect("prompt");
    let entries = serde_json::to_value(engine.session.entries().await).expect("entries json");

    // The cut output shows its handle instead of a truncation marker.
    let (status, text) = tool_result(&entries, "call-1");
    assert_eq!(status, "ok", "{text}");
    assert!(
        text.contains(&format!(
            "[{handle} · 240,000 chars · 20,000 lines · 211,699 chars not shown]"
        )),
        "{}",
        &text[text.len().saturating_sub(400)..]
    );
    // The later cell reads the middle back, exactly as printed.
    let (status, text) = tool_result(&entries, "call-2");
    assert_eq!(
        (status.as_str(), text.as_str()),
        ("ok", "line 010000\nline 010001\nline 010002\n\n")
    );
}

/// The `(role, text)` of each tool result a provider request carried, by tool call id.
fn request_results(context: &pa_agent::stream::LlmContext) -> Vec<(String, String)> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            pa_agent::types::Message::ToolResult(result) => Some((
                result.tool_call_id.clone(),
                result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        pa_agent::types::ToolResultContent::Text(text) => Some(text.text.as_str()),
                        pa_agent::types::ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            pa_agent::types::Message::User(_) | pa_agent::types::Message::Assistant(_) => None,
        })
        .collect()
}

/// Live kernel: past the thinning threshold, the request replays the oldest
/// rounds as a short preview and their spill handle, and a later cell
/// pages a thinned result back in whole with `ctx.page`, byte for byte.
#[tokio::test(flavor = "multi_thread")]
async fn a_thinned_result_pages_back_in_whole_from_a_live_kernel() {
    const ROUNDS: usize = 16;
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));
    // Thinning is off by default.
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"contextThinning": {"enabled": true}}"#,
    )
    .expect("settings");

    // 16 rounds of ~7.5k chars each in a 40k-token window: the view passes
    // the 65% thinning threshold during them, so later requests thin the
    // oldest rounds.
    let mut model = scripted_model();
    model.context_window = 40_000;
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    for round in 0..ROUNDS {
        provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
            &model,
            None,
            vec![(
                format!("call-{round:02}").as_str(),
                pa_types::ai::KERNEL_TOOL_NAME,
                json!({ "code": format!("(let count ([n 0])
                               (when (< n 470)
                                 (let ([digits (number->string n)])
                                   (displayln (string-append \"r{round:02} line \" (make-string (- 4 (string-length digits)) #\\0) digits)))
                                 (count (+ n 1))))") }),
            )],
        )));
    }
    provider.push_text_turn("printed");
    let engine = engine_for(session, agent_dir, cwd, model.clone(), &provider).await;
    engine
        .session
        .prompt("print", pa_core::session_engine::PromptOptions::default())
        .await
        .expect("prompt");
    let entries = serde_json::to_value(engine.session.entries().await).expect("entries json");
    let (status, original) = tool_result(&entries, "call-00");
    assert_eq!(status, "ok", "{original}");

    // The last request carried call-00 as a preview of its lines, then its
    // handle. The preview ends with one unbroken tail block in at most half
    // its 360 characters (the last twelve 14-char lines), and every earlier
    // line is as informative as the next, so the rest fills in order from the
    // top: five lines, the gap, and the last twelve.
    let id = &format!("{:x}", sha2::Sha256::digest(original.as_bytes()))[..10];
    let chars: Vec<char> = original.chars().collect();
    let all: Vec<&str> = original.lines().collect();
    let lines = all.len();
    let preview = format!(
        "{}\n[… {} lines …]\n{}",
        all[..5].join("\n"),
        lines - 17,
        all[lines - 12..].join("\n")
    );
    let last = provider.calls().pop().expect("a request");
    let thinned = request_results(&last)
        .into_iter()
        .find(|(call, _)| call == "call-00")
        .expect("call-00 in the request");
    assert_eq!(
        thinned,
        (
            "call-00".to_string(),
            format!(
                // Grouped digits; the result is a few thousand chars.
                "{preview}\n[out:{id} · {},{:03} chars · {lines} lines · thinned from this request; ctx-page/ctx-grep read it whole]",
                chars.len() / 1000,
                chars.len() % 1000,
            )
        )
    );

    // A later cell pages the whole result back in, byte for byte.
    provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
        &model,
        None,
        vec![(
            "call-page",
            pa_types::ai::KERNEL_TOOL_NAME,
            json!({ "code": format!("(display (hash-ref (ctx-page \"out:{id}\" #:lines {lines}) 'text))") }),
        )],
    )));
    provider.push_text_turn("paged");
    engine
        .session
        .prompt("page", pa_core::session_engine::PromptOptions::default())
        .await
        .expect("prompt");
    let entries = serde_json::to_value(engine.session.entries().await).expect("entries json");
    assert_eq!(
        tool_result(&entries, "call-page"),
        ("ok".to_string(), original)
    );
}
