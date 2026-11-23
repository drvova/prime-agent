//! The harness metrics (`session_engine::run_metrics`) on `agent run
//! completed`: every live seam feeds the run's meter.

use super::*;

/// One run on the catalog's Anthropic model (a 1,000,000-token window):
/// an eval cell, an admitted spawn, a request that re-read less than the
/// previous prompt, and a post-run compaction.
#[tokio::test]
async fn the_live_seams_feed_the_harness_metrics() {
    let fixture = fixture();
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    let request = |tokens: [u64; 4], stop_reason| {
        let [input, cache_read, cache_write, output] = tokens;
        let mut message = assistant_message();
        message.provider = "anthropic".to_string();
        message.model = "claude-fable-5".to_string();
        message.usage = Usage {
            input,
            output,
            cache_read,
            cache_write,
            total_tokens: input + output + cache_read + cache_write,
            cost: pa_agent::types::UsageCost::default(),
        };
        message.stop_reason = stop_reason;
        message
    };
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    emit(&fixture, AgentEvent::TurnStart);
    emit(
        &fixture,
        message_end_event(request([100, 0, 4_000, 20], StopReason::ToolUse)),
    );
    let (tool_start, mut tool_end) = tool_execution_event("eval", false);
    if let AgentEvent::ToolExecutionEnd { result, .. } = &mut tool_end {
        result.details = serde_json::json!({ "status": "ok", "durationMs": 700 });
    }
    emit(&fixture, tool_start);
    emit(&fixture, tool_end);
    telemetry.note_subagent_spawn();
    emit(&fixture, AgentEvent::TurnStart);
    emit(
        &fixture,
        message_end_event(request([3_000, 1_000, 100, 30], StopReason::Stop)),
    );
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    telemetry.note_compaction(Some(10));
    emit(&fixture, AgentEvent::AgentStart);

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    let keys = [
        "end_reason",
        "eval_cell_count",
        "eval_cell_duration_ms",
        "subagent_spawn_count",
        "cache_read_break_count",
        "cache_hit_percent",
        "compaction_context_percent_min",
        "compaction_context_percent_max",
    ];
    let metrics: serde_json::Map<String, serde_json::Value> = keys
        .iter()
        .filter_map(|key| Some(((*key).to_string(), runs.first()?.get(*key)?.clone())))
        .collect();
    let compaction_percent = 4_130.0 / 1_000_000.0 * 100.0;
    assert_eq!(
        serde_json::Value::Object(metrics),
        serde_json::json!({
            "end_reason": "stop",
            "eval_cell_count": 1,
            "eval_cell_duration_ms": 700,
            "subagent_spawn_count": 1,
            "cache_read_break_count": 1,
            "cache_hit_percent": 1_000.0 / 8_200.0 * 100.0,
            "compaction_context_percent_min": compaction_percent,
            "compaction_context_percent_max": compaction_percent,
        })
    );
}
