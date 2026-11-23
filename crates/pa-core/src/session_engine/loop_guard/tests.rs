use super::*;

use pa_agent::stream::{LlmContext, StreamRequestOptions};
use pa_agent::types::{
    AfterToolCallContext, AgentContext, AgentToolResult, StopReason, ToolCall, Usage,
};

use crate::session_engine::provider_adapter::{json_round_trip, real_stream_fn};
use crate::session_engine::provider_retry::{
    complete_with_provider_retry, ProviderRetryPolicy, UNBOUNDED_BACKOFF_MS,
};

/// The scripted runaway through the real stream adapter and the shared retry policy: the guard
/// cuts the looping stream as a `repetition_loop` failure, the policy retries it exactly once,
/// and the retried request carries the nudge. The retry loops again, and the turn settles on
/// the failure instead of retrying in a loop (a third scripted reply is never requested).
#[tokio::test]
async fn a_looping_stream_is_cut_and_retried_once_with_the_nudge() {
    use pa_ai::faux::{
        faux_assistant_message, faux_text, register_faux_provider, FauxAssistantMessageOptions,
        FauxResponseStep, RegisterFauxProviderOptions,
    };
    let registration = register_faux_provider(RegisterFauxProviderOptions::default());
    let model = registration.get_model();
    let line = "I should double-check the configuration loader before continuing.\n";
    let runaway = faux_assistant_message(
        vec![faux_text(&line.repeat(1_000))],
        FauxAssistantMessageOptions::default(),
    );
    let retried_context = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&retried_context);
    let looping_again = runaway.clone();
    registration.set_responses(vec![
        FauxResponseStep::Message(runaway),
        FauxResponseStep::Factory(Arc::new(move |context, _, _, _| {
            *captured.lock().unwrap() = Some(context.clone());
            Ok(looping_again.clone())
        })),
        FauxResponseStep::Message(faux_assistant_message(
            vec![faux_text("A third attempt.")],
            FauxAssistantMessageOptions::default(),
        )),
    ]);

    let agent_model: pa_agent::types::Model = json_round_trip(&model).unwrap();
    // The session's `loopGuard.sensitivity: exact` (the faux provider is unguarded by default).
    let stream_fn = guard_stream_fn(
        Some(LoopGuardLevel::Exact),
        real_stream_fn(Some("test".to_string()), model),
    );
    let prompt = Message::User(UserMessage {
        content: UserContent::Text("Fix the loader.".to_string()),
        timestamp: 0,
    });
    let policy = ProviderRetryPolicy {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 1,
        max_retry_delay_ms: 50,
        max_delay_ms: UNBOUNDED_BACKOFF_MS,
    };
    let message = complete_with_provider_retry(
        &policy,
        None,
        |_| async { true },
        || {
            let stream_fn = Arc::clone(&stream_fn);
            let context = LlmContext {
                system_prompt: None,
                messages: vec![prompt.clone()],
                tools: Vec::new(),
            };
            let agent_model = agent_model.clone();
            async move {
                let mut stream =
                    stream_fn(agent_model, context, StreamRequestOptions::default()).await?;
                stream.result().await
            }
        },
    )
    .await
    .unwrap();
    let served = registration.call_count();
    let pending = registration.get_pending_response_count();
    registration.unregister();

    assert_eq!((served, pending), (2, 1));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        provider_stream_failure_kind(&message).as_deref(),
        Some(REPETITION_LOOP_KIND)
    );
    assert!(message.content.is_empty(), "the looping reply is voided");
    let retried = retried_context
        .lock()
        .unwrap()
        .take()
        .expect("the retry ran");
    assert_eq!(
        serde_json::to_value(&retried.messages).unwrap(),
        serde_json::json!([{
            "role": "user",
            "content": format!("Fix the loader.\n\n{REPETITION_NUDGE}"),
            "timestamp": 0,
        }])
    );
}

fn assistant_calling(arguments: &serde_json::Value) -> AgentMessage {
    AgentMessage::Standard(Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: "call".to_string(),
            name: "python_repl".to_string(),
            arguments: arguments.clone(),
            thought_signature: None,
        })],
        api: String::new(),
        provider: "test".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::zero(),
        stop_reason: StopReason::ToolUse,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
    }))
}

fn tool_result() -> AgentMessage {
    AgentMessage::Standard(Message::ToolResult(pa_agent::types::ToolResultMessage {
        tool_call_id: "call".to_string(),
        tool_name: "python_repl".to_string(),
        content: vec![ToolResultContent::text(
            "NameError: name 'loader' is not defined",
        )],
        details: None,
        is_error: true,
        timestamp: 0,
    }))
}

/// What the repeated-call hook returns for the newest round of `messages`.
async fn nudge_for(messages: Vec<AgentMessage>) -> Option<Vec<ToolResultContent>> {
    let AgentMessage::Standard(Message::Assistant(current)) = messages.last().unwrap().clone()
    else {
        panic!("the newest message is the calling round");
    };
    let call = current.tool_calls()[0].clone();
    let hook = repeat_call_guard(Arc::new(OnceLock::new()));
    hook(
        AfterToolCallContext {
            assistant_message: current,
            args: call.arguments.clone(),
            tool_call: call,
            result: AgentToolResult::text("NameError: name 'loader' is not defined"),
            is_error: true,
            context: AgentContext {
                system_prompt: String::new(),
                messages,
                tools: Vec::new(),
            },
        },
        pa_agent::abort::AbortSignal::never(),
    )
    .await
    .unwrap()
    .and_then(|result| result.content)
}

/// The agent re-running the same failing cell with the same code: the third identical round's
/// result carries the nudge; two rounds, or a changed cell in between, do not.
#[tokio::test]
async fn a_third_identical_tool_call_round_gets_the_nudge() {
    let same = serde_json::json!({"code": "loader.run()"});
    let changed = serde_json::json!({"code": "loader = Loader(); loader.run()"});
    let prompt = AgentMessage::Standard(Message::User(UserMessage {
        content: UserContent::Text("Run the loader.".to_string()),
        timestamp: 0,
    }));
    let three_rounds = vec![
        prompt.clone(),
        assistant_calling(&same),
        tool_result(),
        assistant_calling(&same),
        tool_result(),
        assistant_calling(&same),
    ];
    assert_eq!(
        nudge_for(three_rounds).await,
        Some(vec![
            ToolResultContent::text("NameError: name 'loader' is not defined"),
            ToolResultContent::text(
                "[loop guard] This is round 3 in a row calling `python_repl` with identical arguments. If its result is not changing, do not run it again unchanged: change the input or take a different approach."
            ),
        ])
    );
    let two_rounds = vec![
        prompt.clone(),
        assistant_calling(&same),
        tool_result(),
        assistant_calling(&same),
    ];
    assert_eq!(nudge_for(two_rounds).await, None);
    let changed_between = vec![
        prompt,
        assistant_calling(&same),
        tool_result(),
        assistant_calling(&changed),
        tool_result(),
        assistant_calling(&same),
    ];
    assert_eq!(nudge_for(changed_between).await, None);
}
