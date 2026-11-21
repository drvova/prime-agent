// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Live kernel: a later cell re-reads a file the model already saw; the transcript shows one
//! "unchanged since turn N" line with a handle instead of the file again.
#![cfg(unix)]

use std::path::PathBuf;

use pa_agent::scripted::{tool_call_turn_steps, ScriptedProvider, ScriptedTurn};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use serde_json::{json, Value};
use sha2::Digest as _;

/// The kernel Python with prime-agent-runtime installed (the interpreter the
/// TS product's kernel venv bootstraps). Skipped (with a note) on machines
/// without a live install; `PA_CORE_KERNEL_PYTHON` points at an explicit one.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_CORE_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string(),
        |home| format!("{home}/.prime/agent/kernel-venv/bin/python"),
    ));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel test",
        candidate.display()
    );
    None
}

/// Scoped process-env overrides: applied on construction, restored on drop.
struct EnvOverride {
    saved: Vec<(String, Option<String>)>,
}

impl EnvOverride {
    fn apply(pairs: &[(&str, Option<String>)]) -> Self {
        let saved = pairs
            .iter()
            .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
            .collect();
        for (key, value) in pairs {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        EnvOverride { saved }
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
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

#[tokio::test]
async fn a_reread_of_an_unchanged_file_is_one_line_in_the_transcript() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    std::fs::write(cwd.join("a.txt"), "alpha\nbeta\ngamma\n").expect("file");

    let _env = EnvOverride::apply(&[
        (
            "PRIME_AGENT_KERNEL_PYTHON",
            Some(kernel_python.display().to_string()),
        ),
        ("PRIME_AGENT_CODING_AGENT_DIR", None),
        ("PRIME_API_KEY", None),
    ]);

    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));
    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    for call_id in ["call-1", "call-2"] {
        provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
            &model,
            None,
            vec![(
                call_id,
                "python_repl",
                json!({ "code": "print(await fs.read('a.txt'))" }),
            )],
        )));
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
        prewarm_ipython_kernel: None,
        on_background_work_settled: None,
        bash_notice_wake: None,
        queued_goal_context_purge: None,
        queued_steering_probe: None,
        should_stop_after_turn: None,
        image_model_router: None,
    })
    .await
    .expect("create_session");

    engine
        .session
        .prompt(
            "read it twice",
            pa_core::session_engine::PromptOptions::default(),
        )
        .await
        .expect("prompt");
    let entries = serde_json::to_value(engine.session.entries().await).expect("entries json");
    let (_, first) = tool_result(&entries, "call-1");
    let full = first.trim_end_matches('\n');
    assert!(full.starts_with("a.txt (lines 1-3 of 3)\n"), "{full}");
    let handle = format!(
        "out:{}",
        &format!("{:x}", sha2::Sha256::digest(full.as_bytes()))[..10]
    );
    let (status, again) = tool_result(&entries, "call-2");
    assert_eq!(
        (status.as_str(), again.as_str()),
        (
            "ok",
            format!("a.txt (unchanged since turn 1, 3 lines; whole view: {handle})\n").as_str()
        )
    );
}
