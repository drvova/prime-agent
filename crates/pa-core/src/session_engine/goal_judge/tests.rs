use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{
    AgentMessage, AssistantContent, AssistantMessage, Message, StopReason, TextContent, ToolCall,
    ToolResultContent, ToolResultMessage,
};

use super::*;
use crate::session::manager::SessionManager;
use crate::session_engine::rlm_usage::SessionUsageStore;

fn test_model() -> pa_agent::types::Model {
    serde_json::from_value(serde_json::json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 100_000, "maxTokens": 8000
    }))
    .unwrap()
}

fn assistant(content: Vec<AssistantContent>, stop_reason: StopReason) -> AgentMessage {
    AgentMessage::Standard(Message::Assistant(AssistantMessage {
        content,
        api: "openai-completions".to_string(),
        provider: "test".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_agent::types::Usage::zero(),
        stop_reason,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
    }))
}

fn tool_call(id: &str, name: &str) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    })
}

/// The main conversation at the moment a kernel cell calls `goal.complete()`: a user ask, a test
/// run with its output, and the in-flight assistant turn whose cell is making the claim.
fn conversation() -> Vec<AgentMessage> {
    vec![
        AgentMessage::Standard(Message::User(pa_agent::types::UserMessage {
            content: pa_agent::types::UserContent::Text(
                "fix the parser so the suite passes".to_string(),
            ),
            timestamp: 0,
        })),
        assistant(vec![tool_call("call-1", "bash")], StopReason::ToolUse),
        AgentMessage::Standard(Message::ToolResult(ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            content: vec![ToolResultContent::Text(TextContent {
                text: "running 3 tests\ntest result: ok. 3 passed; 0 failed".to_string(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            timestamp: 0,
        })),
        assistant(
            vec![
                AssistantContent::Text(TextContent {
                    text: "All done, everything works.".to_string(),
                    text_signature: None,
                }),
                tool_call("call-2", "python_repl"),
            ],
            StopReason::ToolUse,
        ),
    ]
}

struct Fixture {
    provider: Arc<ScriptedProvider>,
    _agent: Arc<Agent>,
    judge: GoalJudge,
    _dir: tempfile::TempDir,
}

fn fixture(judge_reply: &str) -> Fixture {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn(judge_reply);
    let agent = Arc::new(Agent::new(AgentOptions {
        session_id: Some("session-1".to_string()),
        initial_state: AgentInitialState {
            system_prompt: Some("main system prompt".to_string()),
            model: Some(test_model()),
            thinking_level: None,
            tools: Some(Vec::new()),
            messages: Some(conversation()),
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    let dir = tempfile::TempDir::new().unwrap();
    let session = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
        dir.path(),
    )));
    let convert: ConvertToLlmFn = Arc::new(|messages| {
        Box::pin(async move {
            Ok(crate::session_engine::messages::loop_convert_to_llm(
                messages,
            ))
        })
    });
    let judge = GoalJudge::new(
        Arc::downgrade(&agent),
        provider.stream_fn(),
        Arc::new(|messages, _signal| Box::pin(async move { Ok(messages) })),
        convert,
        Arc::new(SessionUsageStore(session)),
    );
    Fixture {
        provider,
        _agent: agent,
        judge,
        _dir: dir,
    }
}

/// A judge that answers MET but cites nothing the conversation contains does not pass: a bare
/// assertion, and a quote that only the judge made up, both count as NOT MET.
#[tokio::test]
async fn a_met_verdict_without_verifiable_evidence_is_not_met() {
    let uncited_gap = "the judge answered MET without citing evidence found in the conversation (a file:line or verbatim command output); run the verifying command so its output is in context, then claim completion again".to_string();
    for reply in [
        "VERDICT: MET\nEVIDENCE:\n- the agent fixed the parser and all tests pass",
        "VERDICT: MET\nEVIDENCE:\n- `test result: ok. 4 passed; 0 failed` supports the suite",
    ] {
        let fixture = fixture(reply);
        assert_eq!(
            fixture.judge.judge("fix the parser").await,
            JudgeVerdict::NotMet {
                gap: uncited_gap.clone()
            },
            "{reply}"
        );
    }
}

/// A MET citing tool output verbatim passes, and the judge request forks the conversation as the
/// main loop last sent it (same system prompt and messages up to the in-flight claim turn), so
/// the provider serves it from the cached prefix; only the rubric is new.
#[tokio::test]
async fn a_cited_met_passes_over_the_forked_prefix() {
    let fixture = fixture(
        "VERDICT: MET\nEVIDENCE:\n- `test result: ok. 3 passed; 0 failed` supports the suite passing",
    );
    assert_eq!(
        fixture.judge.judge("fix the parser").await,
        JudgeVerdict::Met
    );
    let calls = fixture.provider.calls();
    assert_eq!(calls.len(), 1, "exactly one judge call");
    let request = &calls[0];
    let parent_prefix: Vec<Message> = conversation()
        .into_iter()
        .take(3)
        .filter_map(|message| match message {
            AgentMessage::Standard(message) => Some(message),
            AgentMessage::Custom(_) => None,
        })
        .collect();
    assert_eq!(
        (
            request.system_prompt.as_deref(),
            serde_json::to_value(&request.messages[..3]).unwrap()
        ),
        (
            Some("main system prompt"),
            serde_json::to_value(&parent_prefix).unwrap()
        )
    );
    assert_eq!(request.messages.len(), 4, "prefix + the rubric");
}

/// NOT MET carries the judge's gap forward.
#[tokio::test]
async fn a_not_met_verdict_carries_its_gap() {
    let fixture = fixture(
        "VERDICT: NOT MET\nEVIDENCE:\n- `running 3 tests` only\nGAP: the README example still fails",
    );
    assert_eq!(
        fixture.judge.judge("fix the parser and the README").await,
        JudgeVerdict::NotMet {
            gap: "the README example still fails".to_string()
        }
    );
}
