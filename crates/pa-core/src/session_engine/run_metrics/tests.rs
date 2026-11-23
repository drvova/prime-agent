//! The run meter over scripted requests, the cache-prefix break rule, and
//! the offline feed over a synthetic session file.

use pa_agent::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, StopReason, TextContent,
    ToolCall, Usage, UsageCost,
};
use serde_json::json;

use super::*;

/// A settled request on the catalog's Anthropic model (a 1,000,000-token
/// window) with `input`/`cache_read`/`cache_write`/`output` tokens.
fn request(
    stop_reason: StopReason,
    tokens: [u64; 4],
    content: Vec<AssistantContent>,
) -> AssistantMessage {
    let [input, cache_read, cache_write, output] = tokens;
    AssistantMessage {
        content,
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-fable-5".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage {
            input,
            output,
            cache_read,
            cache_write,
            total_tokens: input + output + cache_read + cache_write,
            cost: UsageCost::default(),
        },
        stop_reason,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
    }
}

fn eval_call() -> Vec<AssistantContent> {
    vec![AssistantContent::ToolCall(ToolCall {
        id: "call-1".to_string(),
        name: "eval".to_string(),
        arguments: json!({ "code": "(fs-read \"a.rs\")" }),
        thought_signature: None,
    })]
}

fn text(text: &str) -> Vec<AssistantContent> {
    vec![AssistantContent::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    })]
}

/// A scripted run through every seam: cells with their durations and bash
/// waits, a failed tool call, a loop-guard cut, a spawn, and a compaction.
#[test]
fn a_scripted_run_folds_into_its_metrics() {
    let mut meter = MetricsMeter::new(SpawnVisibility::Observed);
    meter.request(&request(
        StopReason::ToolUse,
        [200, 0, 5_000, 100],
        eval_call(),
    ));
    meter.tool_result(
        "eval",
        false,
        &json!({ "durationMs": 1_500, "bashWaits": { "completed": 1, "yielded": 2 } }),
    );
    meter.subagent_spawn();
    meter.request(&request(
        StopReason::ToolUse,
        [50, 5_200, 400, 80],
        eval_call(),
    ));
    meter.tool_result("eval", true, &json!({ "durationMs": 500 }));
    meter.tool_result("custom_tool", false, &serde_json::Value::Null);
    meter.compaction();
    let mut cut = request(StopReason::Error, [0, 0, 0, 0], Vec::new());
    cut.diagnostics = Some(vec![AssistantMessageDiagnostic {
        kind: "provider_stream_failure".to_string(),
        timestamp: 0,
        error: None,
        details: Some(json!({ "kind": "repetition_loop" })),
    }]);
    meter.request(&cut);

    assert_eq!(
        meter.finish_run(9_000),
        RunMetrics {
            model_requests: 3,
            failed_model_requests: 1,
            eval_cells: 2,
            eval_cell_ms: 2_000,
            tool_calls: 3,
            tool_errors: 1,
            tokens: TokenSplit {
                uncached_input: 250,
                cache_read: 5_200,
                cache_write: 5_400,
                output: 180,
            },
            cache_hit_rate: Some(5_200.0 / 10_850.0),
            cache_breaks: Some(0),
            bash_waits_completed: 1,
            bash_waits_yielded: 2,
            subagent_spawns: Some(1),
            compactions: vec![CompactionPoint {
                context_fraction: Some(5_730.0 / 1_000_000.0),
            }],
            end_reason: TurnEndReason::LoopGuard,
            wall_ms: 9_000,
        }
    );
}

/// A request that re-reads less than the previous prompt is a break; one
/// that reads the whole previous prompt and appends is not; a provider
/// that never reports cache tokens is never compared.
#[test]
fn the_cache_break_counter_counts_a_break_and_not_an_append() {
    let mut meter = MetricsMeter::new(SpawnVisibility::Observed);
    meter.request(&request(
        StopReason::ToolUse,
        [10, 0, 20_000, 50],
        eval_call(),
    ));
    // Append: reads all 20,010 prompt tokens back.
    meter.request(&request(
        StopReason::ToolUse,
        [10, 20_010, 900, 50],
        eval_call(),
    ));
    // Break: the provider re-read only the system prompt.
    meter.request(&request(
        StopReason::Stop,
        [18_000, 3_000, 0, 50],
        text("done"),
    ));
    let breaks = meter.finish_run(0).cache_breaks;

    let mut silent = MetricsMeter::new(SpawnVisibility::Observed);
    silent.request(&request(
        StopReason::ToolUse,
        [20_000, 0, 0, 50],
        eval_call(),
    ));
    silent.request(&request(StopReason::Stop, [21_000, 0, 0, 50], text("done")));

    assert_eq!((breaks, silent.finish_run(0).cache_breaks), (Some(1), None));
}

/// The offline feed over a synthetic session file (hand-built: the faux
/// provider counts a request's new suffix in both its input and its cache
/// write, so it is no model of a provider's cache): two runs, a steering
/// message inside the first, a compaction after it, and an empty final
/// answer ending the second.
#[test]
fn a_session_file_yields_the_metrics_of_each_run() {
    let runs = transcript_run_metrics(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/run_metrics/synthetic-two-runs.jsonl"
    )));

    assert_eq!(
        runs,
        vec![
            RunMetrics {
                model_requests: 3,
                failed_model_requests: 0,
                eval_cells: 2,
                eval_cell_ms: 2_000,
                tool_calls: 2,
                tool_errors: 1,
                tokens: TokenSplit {
                    uncached_input: 3_250,
                    cache_read: 7_200,
                    cache_write: 6_400,
                    output: 220,
                },
                cache_hit_rate: Some(7_200.0 / 16_850.0),
                cache_breaks: Some(1),
                bash_waits_completed: 1,
                bash_waits_yielded: 0,
                subagent_spawns: None,
                compactions: vec![CompactionPoint {
                    context_fraction: Some(6_040.0 / 1_000_000.0),
                }],
                end_reason: TurnEndReason::Stop,
                wall_ms: 10_000,
            },
            RunMetrics {
                model_requests: 1,
                failed_model_requests: 0,
                eval_cells: 0,
                eval_cell_ms: 0,
                tool_calls: 0,
                tool_errors: 0,
                tokens: TokenSplit {
                    uncached_input: 100,
                    cache_read: 0,
                    cache_write: 3_000,
                    output: 0,
                },
                cache_hit_rate: Some(0.0),
                cache_breaks: None,
                bash_waits_completed: 0,
                bash_waits_yielded: 0,
                subagent_spawns: None,
                compactions: Vec::new(),
                end_reason: TurnEndReason::Empty,
                wall_ms: 5_000,
            },
        ]
    );
}
