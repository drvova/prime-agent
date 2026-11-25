#![expect(
    clippy::large_futures,
    reason = "a session's futures are large by design, as in src/lib.rs"
)]

//! A reply cut off at the output limit runs none of its tool calls, through a
//! real session and kernel: the `eval` call it carries may be incomplete, so
//! its result says so and the turn goes on for the model to issue it again.
//! No model call: the `ScriptedProvider` answers from the pushed turns.

use std::sync::Arc;

use pa_agent::scripted::{tool_call_turn_steps, ScriptStep, ScriptedProvider, ScriptedTurn};
use pa_agent::stream::AssistantMessageEvent;
use pa_agent::types::{AgentMessage, Message, StopReason, ToolResultContent};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::PromptOptions;
use serde_json::json;

fn scripted_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "test", "provider": "faux",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4_000
    }))
    .expect("faux loop model")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_in_a_reply_cut_off_at_the_output_limit_is_not_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let written = cwd.join("written.txt");
    let model = scripted_model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    let mut steps = tool_call_turn_steps(
        &model,
        None,
        vec![(
            "call-1",
            pa_types::ai::KERNEL_TOOL_NAME,
            json!({ "code": format!("(fs-write {:?} \"ran\")", written.to_string_lossy()) }),
        )],
    );
    if let Some(ScriptStep::Event(event)) = steps.last_mut() {
        if let AssistantMessageEvent::Done { reason, message } = event.as_mut() {
            *reason = StopReason::Length;
            message.stop_reason = StopReason::Length;
        }
    }
    provider.push_turn(ScriptedTurn::Events(steps));
    provider.push_text_turn("done");
    let engine = create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir: dir.path().join("agent"),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(SessionManager::in_memory(&cwd)),
        daemonless: true,
        ..SessionEngineConfig::default()
    })
    .await
    .expect("session");
    engine
        .prompt("go", PromptOptions::default())
        .await
        .expect("prompt");
    let results: Vec<(String, bool)> = engine
        .session
        .agent()
        .state()
        .await
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some((
                result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultContent::Text(text) => Some(text.text.clone()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<String>(),
                result.is_error,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        (results, written.exists(), provider.calls().len()),
        (
            vec![(
                format!(
                    "Tool call \"{}\" was not run: the reply was cut off at the output limit, so \
its arguments may be incomplete. Issue it again with complete arguments; split a long cell or \
write long text to a file in several calls.",
                    pa_types::ai::KERNEL_TOOL_NAME
                ),
                true
            )],
            false,
            2
        )
    );
}
