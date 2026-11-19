//! The live context reading on the turn's wire: every settled row's `message_end` carries the
//! session's context usage, and every flushed `message_update` carries the streaming estimate.
use super::*;

use pa_types::context_breakdown::RequestCache;
use pa_types::usage::{estimate_tokens, ContextUsage};

/// Run one turn over `engine` against an in-memory session store seeded with `seed` rows, and
/// collect the session events it broadcast.
async fn turn_events_over_store(engine: Arc<dyn SessionEngine>, seed: &[Value]) -> Vec<Value> {
    let (_tmp, runner) = burst_runner(Arc::clone(&engine));
    {
        let mut store = crate::session_store::SessionFile::create("/tmp", None, 0);
        for message in seed {
            store
                .persist_entry("message", json!({ "message": message }))
                .expect("seed row");
        }
        runner.core.lock().unwrap().store = Some(store);
    }
    let mut subscription = runner.events.subscribe();
    runner
        .run_turn(
            engine,
            Lane::Steering,
            vec![QueuedItem {
                priority: QueuePriority::Human,
                preview: None,
                message: "look it up".to_string(),
                custom_message: None,
                agent_message: None,
                queue_key: None,
                admission_id: None,
                images: Vec::new(),
                done: None,
                queue_visible: true,
                policy: TurnPolicy::Queued,
                forced_batch: false,
            }],
        )
        .await;
    let mut events = Vec::new();
    while let Ok(frame) = subscription.try_recv() {
        if frame.outbound_type == "session_event" {
            if let Ok(outbound) = serde_json::from_slice::<Value>(&frame.payload) {
                events.push(outbound["event"].clone());
            }
        }
    }
    events
}

/// One reading as a client reads it off the wire (serialized, then parsed back).
fn on_the_wire(usage: ContextUsage) -> Value {
    serde_json::from_str(&json!(usage).to_string()).expect("a reading round-trips")
}

/// The measured reading anchored on one assistant's usage.
fn measured(usage: &Value, context_window: u64) -> ContextUsage {
    ContextUsage {
        last_request: serde_json::from_value::<RequestCache>(usage.clone()).ok(),
        ..ContextUsage::of(
            pa_types::usage::calculate_context_tokens(usage),
            context_window,
        )
    }
}

/// Per model round: a faux tool loop (a tool call, its result, the closing reply) settles three
/// rows after the prompt, and each row's `message_end` carries the reading at that point — the
/// assistant rounds their real usage, the prompt and the tool result the last reading plus their
/// estimate — instead of one refresh at the turn's end.
#[tokio::test]
async fn every_settled_row_of_a_tool_loop_carries_the_live_context_reading() {
    const WINDOW: u64 = 100_000;
    let dir = tempfile::TempDir::new().unwrap();
    let script = json!({
        "engine": "faux",
        "contextWindow": WINDOW,
        "responses": [
            { "content": [
                { "type": "text", "text": "checking" },
                { "type": "toolCall", "id": "call-1", "name": "synthetic_missing_tool", "arguments": {} },
            ] },
            "all done",
        ],
    });
    let engine = Arc::new(
        AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: Some(script.to_string()),
            supervisor_link: None,
            telemetry_disabled: Some(true),
            cron_store: None,
            queued_steering_probe: None,
        })
        .unwrap(),
    );
    engine.register_arc();
    let events = turn_events_over_store(engine, &[]).await;

    let ends: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "message_end")
        .collect();
    let emitted: Vec<(String, Value)> = ends
        .iter()
        .map(|event| {
            (
                event["message"]["role"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                event["contextUsage"].clone(),
            )
        })
        .collect();
    // The expected readings, row by row: the faux usage anchors each assistant round, and the
    // rows after it add their chars/4 estimate.
    let (prompt, first, result, last) = (
        &ends[0]["message"],
        &ends[1]["message"],
        &ends[2]["message"],
        &ends[3]["message"],
    );
    let first_reading = measured(&first["usage"], WINDOW);
    let expected = vec![
        (
            "user".to_string(),
            on_the_wire(ContextUsage::of(estimate_tokens(prompt), WINDOW)),
        ),
        ("assistant".to_string(), on_the_wire(first_reading)),
        (
            "toolResult".to_string(),
            on_the_wire(ContextUsage {
                last_request: first_reading.last_request,
                ..ContextUsage::of(
                    first_reading.tokens.unwrap() + estimate_tokens(result),
                    WINDOW,
                )
            }),
        ),
        (
            "assistant".to_string(),
            on_the_wire(measured(&last["usage"], WINDOW)),
        ),
    ];
    assert_eq!(emitted, expected, "events: {events:?}");
}

/// One streamed reply, paced past the flush interval so every delta takes the leading edge
/// (one flushed update per delta, deterministic), over a session whose context reads 100 tokens.
struct PacedReplyEngine;

impl PacedReplyEngine {
    const DELTAS: [&'static str; 3] = ["The answer ", "grows with ", "every delta"];

    fn reply(text: &str, usage: Option<Value>) -> Value {
        let mut message = json!({
            "role": "assistant",
            "provider": "faux",
            "model": "faux-1",
            "content": [{ "type": "text", "text": text }],
        });
        if let Some(usage) = usage {
            message["usage"] = usage;
        }
        message
    }
}

impl SessionEngine for PacedReplyEngine {
    fn model_context_window(&self) -> Option<u64> {
        Some(1_000)
    }

    fn run_prompt(
        &self,
        _prompt_index: usize,
        request: PromptRequest,
        _aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        if !emit(EngineEvent::UserMessage(
            json!({ "role": "user", "content": request.message }),
        )) {
            return;
        }
        let mut text = String::new();
        let start = EngineEvent::AssistantUpdate {
            message: crate::engine::AssistantSnapshot::Wire(Self::reply("", None)),
            stream_event: Some(json!({ "type": "start" })),
        };
        if !emit(start) {
            return;
        }
        for delta in Self::DELTAS {
            // Past the flush interval: the park broadcasts at once (the leading edge).
            std::thread::sleep(
                crate::streaming::UPDATE_FLUSH_INTERVAL + std::time::Duration::from_millis(10),
            );
            text.push_str(delta);
            let update = EngineEvent::AssistantUpdate {
                message: crate::engine::AssistantSnapshot::Wire(Self::reply(&text, None)),
                stream_event: Some(json!({ "type": "text_delta", "delta": delta })),
            };
            if !emit(update) {
                return;
            }
        }
        let usage = json!({
            "input": 40, "output": 9, "cacheRead": 120, "cacheWrite": 0, "totalTokens": 169,
        });
        if !emit(EngineEvent::AssistantMessage(Self::reply(
            &text,
            Some(usage),
        ))) {
            return;
        }
        emit(EngineEvent::Done(Ok(())));
    }

    fn run_side_question(
        &self,
        _request: SideQuestionRequest,
        _signal: &pa_agent::abort::AbortSignal,
        _sink: &pa_core::session_engine::side_question::SideQuestionSink,
    ) -> SideQuestionOutcome {
        SideQuestionOutcome::Failed {
            answer: String::new(),
            error: "unsupported".to_string(),
        }
    }

    fn run_compaction(
        &self,
        _request: CompactionRequest,
        _signal: &pa_agent::abort::AbortSignal,
    ) -> CompactionOutcome {
        CompactionOutcome::Skipped {
            message: "nothing to compact".to_string(),
        }
    }

    fn run_branch_summary(
        &self,
        _request: crate::engine::BranchSummaryRequest,
        _signal: &pa_agent::abort::AbortSignal,
    ) -> crate::engine::BranchSummaryOutcome {
        crate::engine::BranchSummaryOutcome::Failed {
            error: "unsupported".to_string(),
        }
    }

    fn rebuild_session_context(
        &self,
        _branch_entries: Vec<pa_types::session::FileEntry>,
        _goal_reload: pa_core::session_engine::goal_driver::GoalBranchReload,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// While the reply streams, each flushed update's estimate grows from the last reading (100
/// settled tokens plus the 12-char prompt) by the streamed text's chars/4; the reply's real usage
/// replaces it at `message_end`, its cache split with it.
#[tokio::test]
async fn the_streaming_estimate_rises_per_flush_and_settles_on_the_real_usage() {
    let seed = [
        json!({ "role": "user", "content": "earlier" }),
        PacedReplyEngine::reply(
            "earlier reply",
            Some(json!({ "input": 100, "output": 0, "cacheRead": 0, "cacheWrite": 0 })),
        ),
    ];
    let events = turn_events_over_store(Arc::new(PacedReplyEngine), &seed).await;
    let readings: Vec<(&str, &Value)> = events
        .iter()
        .filter(|event| event.get("contextUsage").is_some())
        .map(|event| {
            (
                event["type"].as_str().unwrap_or_default(),
                &event["contextUsage"],
            )
        })
        .collect();
    // "look it up" is 10 chars (3 tokens); the deltas reach 11, 22, then 33 chars (3, 6, 9).
    let settled_prompt = 100 + 3;
    let streamed = |tokens: u64| ContextUsage {
        estimated: true,
        last_request: Some(RequestCache {
            input: 100,
            cache_read: 0,
            cache_write: 0,
        }),
        ..ContextUsage::of(settled_prompt + tokens, 1_000)
    };
    let prompt_reading = ContextUsage {
        last_request: streamed(0).last_request,
        ..ContextUsage::of(settled_prompt, 1_000)
    };
    let real = ContextUsage {
        last_request: Some(RequestCache {
            input: 40,
            cache_read: 120,
            cache_write: 0,
        }),
        ..ContextUsage::of(169, 1_000)
    };
    let expected = [
        ("message_end", prompt_reading),
        ("message_update", streamed(3)),
        ("message_update", streamed(6)),
        ("message_update", streamed(9)),
        ("message_end", real),
    ]
    .map(|(kind, usage)| (kind, on_the_wire(usage)));
    assert_eq!(
        readings,
        expected
            .iter()
            .map(|(kind, usage)| (*kind, usage))
            .collect::<Vec<_>>()
    );
}
