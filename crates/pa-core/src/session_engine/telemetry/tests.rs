//! The telemetry unit battery: the scripted run state machine, the
//! outcome/provider/model/error categories, and the session-end finalize
//! surface.
use std::time::Duration;

use pa_agent::stream::AssistantMessageEvent;
use pa_agent::types::{
    AgentMessage, AssistantContent, Message as LoopMessage, StopReason, TextContent,
    ToolResultContent, Usage,
};
use pa_telemetry::{MockSink, TelemetryClient, TelemetryClientConfig};

use super::*;
use crate::session_engine::agent_messaging::AgentMessageRefusals;
use crate::session_engine::decision::DecideFact;

/// Tests that resolve the env-gated switch need the three override vars
/// cleared (the Cargo test config sets `DO_NOT_TRACK`); restore after.
/// The env is process-wide, so these tests (and the packages env tests)
/// serialize through the shared env lock while they hold it.
const TELEMETRY_ENV_VARS: [&str; 3] = ["DO_NOT_TRACK", "PI_OFFLINE", "PRIME_AGENT_TELEMETRY"];

struct CleanTelemetryEnv {
    saved: Vec<(&'static str, Option<String>)>,
    _env_lock: std::sync::MutexGuard<'static, ()>,
}

impl Default for CleanTelemetryEnv {
    fn default() -> Self {
        // Held first: the vars may not be touched while another env test
        // runs.
        let env_lock = crate::packages::test_support::ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = TELEMETRY_ENV_VARS
            .iter()
            .map(|var| {
                let value = std::env::var(var).ok();
                std::env::remove_var(var);
                (*var, value)
            })
            .collect();
        Self {
            saved,
            _env_lock: env_lock,
        }
    }
}

impl Drop for CleanTelemetryEnv {
    fn drop(&mut self) {
        // The manual drop runs before the field drops: the restore lands
        // while the env lock is still held.
        for (var, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
        }
    }
}

/// Controllable clock: tests move it between emits.
#[derive(Clone, Default)]
struct TestClock {
    millis: Arc<std::sync::atomic::AtomicU64>,
}

impl TestClock {
    fn set(&self, millis: u64) {
        self.millis
            .store(millis, std::sync::atomic::Ordering::Relaxed);
    }
}

fn client_for(mock: &std::sync::Arc<MockSink>) -> TelemetryClient {
    let mut config = TelemetryClientConfig::new("install-1");
    // Flush per event so assertions see every tracked event without an
    // explicit flush round-trip.
    config.batch_size = 1;
    config.flush_interval = Duration::from_mins(10);
    config.sinks = vec![mock.clone() as Arc<dyn pa_telemetry::TelemetrySink>];
    TelemetryClient::spawn(config).expect("spawn client")
}

struct Fixture {
    client: TelemetryClient,
    state: Arc<Mutex<TelemetryState>>,
    clock: TestClock,
    mock: std::sync::Arc<MockSink>,
}

/// A subscriber fed by scripted events — the state machine without a
/// live agent (same `handle_event` call the subscription uses).
fn fixture() -> Fixture {
    fixture_with_clock(TestClock::default())
}

fn fixture_with_clock(clock: TestClock) -> Fixture {
    let mock = std::sync::Arc::new(MockSink::new());
    let client = client_for(&mock);
    let now: Arc<dyn Fn() -> u64 + Send + Sync> = {
        let millis = clock.millis.clone();
        Arc::new(move || millis.load(std::sync::atomic::Ordering::Relaxed))
    };
    let state = Arc::new(Mutex::new(TelemetryState {
        session_id: "0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a".to_string(),
        started_at: 1_000,
        totals: SessionTotals::default(),
        active_run: None,
        tool_starts: HashMap::new(),
        telemetry_enabled: None,
        recording: true,
        now,
        auth_baseline: crate::auth::resolver::AuthMetrics::current(),
    }));
    Fixture {
        client,
        state,
        clock,
        mock,
    }
}

fn emit(fixture: &Fixture, event: AgentEvent) {
    handle_event(&fixture.client, "interactive", &fixture.state, event);
}

fn assistant_message() -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: "private assistant text".to_string(),
            text_signature: None,
        })],
        api: "test".to_string(),
        provider: "openai".to_string(),
        model: "gpt-test".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage {
            input: 100,
            output: 20,
            cache_read: 50,
            cache_write: 0,
            total_tokens: 170,
            cost: pa_agent::types::UsageCost::default(),
        },
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
    }
}

fn assistant_with_error(error: &str) -> AssistantMessage {
    let mut message = assistant_message();
    message.stop_reason = StopReason::Error;
    message.error_message = Some(error.to_string());
    message
}

fn user_message() -> AgentMessage {
    AgentMessage::user("private prompt")
}

fn text_delta_event(message: &AssistantMessage) -> AgentEvent {
    AgentEvent::MessageUpdate {
        message: std::sync::Arc::new(AgentMessage::Standard(LoopMessage::Assistant(
            message.clone(),
        ))),
        assistant_message_event: std::sync::Arc::new(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "private streamed text".to_string(),
            partial: message.clone(),
        }),
    }
}

fn message_end_event(message: AssistantMessage) -> AgentEvent {
    AgentEvent::MessageEnd {
        message: AgentMessage::Standard(LoopMessage::Assistant(message)),
    }
}

fn tool_execution_event(tool: &str, is_error: bool) -> (AgentEvent, AgentEvent) {
    (
        AgentEvent::ToolExecutionStart {
            tool_call_id: format!("{tool}-1"),
            tool_name: tool.to_string(),
            args: serde_json::json!({ "command": "private command" }),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: format!("{tool}-1"),
            tool_name: tool.to_string(),
            result: pa_agent::types::AgentToolResult {
                content: vec![ToolResultContent::text("private tool output")],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error,
        },
    )
}

/// Wait for the telemetry worker to drain tracked events, then read.
async fn event_properties(
    mock: &MockSink,
    name: &str,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    tokio::time::sleep(Duration::from_millis(10)).await;
    mock.events()
        .iter()
        .filter(|event| event.name == name)
        .map(|event| {
            serde_json::to_value(&event.properties)
                .expect("properties serialize")
                .as_object()
                .expect("properties are an object")
                .clone()
        })
        .collect()
}

/// TS "emits aggregate metrics without message or tool content": one run
/// through the full event sequence, exact counters, and no content leak.
#[tokio::test]
async fn emits_aggregate_metrics_without_content() {
    let fixture = fixture();
    let assistant = assistant_message();

    fixture.clock.set(1_000);
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    fixture.clock.set(1_010);
    emit(&fixture, AgentEvent::TurnStart);
    fixture.clock.set(1_035);
    emit(&fixture, text_delta_event(&assistant));
    fixture.clock.set(1_050);
    let (tool_start, tool_end) = tool_execution_event("bash", false);
    emit(&fixture, tool_start);
    emit(&fixture, tool_end);
    fixture.clock.set(1_100);
    emit(&fixture, message_end_event(assistant.clone()));
    fixture.clock.set(1_125);
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    // Deferred finalize: AgentEnd alone must not seal the run yet (the
    // post-run compaction window stays open).
    assert!(event_properties(&fixture.mock, "agent run completed")
        .await
        .is_empty());

    fixture.clock.set(1_200);
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.end().await.unwrap();

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run["outcome"], serde_json::json!("success"));
    assert_eq!(run["duration_ms"], serde_json::json!(125));
    assert_eq!(run["visible_ttft_ms"], serde_json::json!(25));
    assert_eq!(run["first_model_event_ms"], serde_json::json!(25));
    assert_eq!(run["model_latency_ms"], serde_json::json!(90));
    assert_eq!(run["turn_count"], serde_json::json!(1));
    assert_eq!(run["tool_call_count"], serde_json::json!(1));
    assert_eq!(run["tool_error_count"], serde_json::json!(0));
    assert_eq!(run["input_tokens"], serde_json::json!(100));
    assert_eq!(run["output_tokens"], serde_json::json!(20));
    assert_eq!(run["cache_read_tokens"], serde_json::json!(50));
    assert_eq!(run["total_tokens"], serde_json::json!(170));
    assert_eq!(run["retry_count"], serde_json::json!(0));
    assert_eq!(run["provider_category"], serde_json::json!("openai"));
    assert_eq!(run["model_category"], serde_json::json!("gpt"));
    assert_eq!(
        run["session_id"],
        serde_json::json!("0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a")
    );
    assert_eq!(run["execution_mode"], serde_json::json!("interactive"));
    assert_eq!(run["schema_version"], serde_json::json!(2));

    // Privacy: no private prompt/tool/assistant text anywhere.
    let all = serde_json::to_string(&fixture.mock.events()).unwrap();
    assert!(!all.contains("private"));
    assert!(!all.contains("session-1.jsonl"));

    let ended = event_properties(&fixture.mock, "agent session ended").await;
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["duration_ms"], serde_json::json!(200));
    assert_eq!(ended[0]["prompt_count"], serde_json::json!(1));
    assert_eq!(ended[0]["run_count"], serde_json::json!(1));
    assert_eq!(ended[0]["successful_run_count"], serde_json::json!(1));
    assert_eq!(ended[0]["total_tokens"], serde_json::json!(170));
}

#[tokio::test]
async fn post_run_compaction_counts_into_the_open_run() {
    let fixture = fixture();
    let assistant = assistant_message();

    emit(&fixture, AgentEvent::AgentStart);
    emit(&fixture, message_end_event(assistant.clone()));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    assert!(event_properties(&fixture.mock, "agent run completed")
        .await
        .is_empty());

    // The scheduled compaction drains between AgentEnd and the next run.
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.note_compaction(Some(45));
    emit(&fixture, AgentEvent::AgentStart);

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["compaction_count"], serde_json::json!(1));
}

#[tokio::test]
async fn compaction_between_runs_is_not_counted() {
    let fixture = fixture();
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    // No run finalized yet (one open, ended). Finalize it, then compact.
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    telemetry.note_compaction(Some(45));
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert!(runs
        .iter()
        .all(|run| run["compaction_count"] == serde_json::json!(0)));
}

#[tokio::test]
async fn error_and_aborted_outcomes() {
    let fixture = fixture();
    let failed = assistant_with_error("API Error: 429 rate limit exceeded");
    emit(&fixture, AgentEvent::AgentStart);
    emit(&fixture, message_end_event(failed));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    let mut aborted_message = assistant_message();
    aborted_message.stop_reason = StopReason::Aborted;
    emit(&fixture, message_end_event(aborted_message));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["outcome"], serde_json::json!("error"));
    assert_eq!(runs[0]["error_category"], serde_json::json!("rate_limit"));
    assert_eq!(runs[1]["outcome"], serde_json::json!("aborted"));
    assert_eq!(runs[1]["error_category"], serde_json::Value::Null);
}

#[test]
fn error_categories() {
    fn category(error: &str) -> String {
        let message = assistant_with_error(error);
        error_category(Some(&message))
            .as_str()
            .expect("category")
            .to_string()
    }
    assert_eq!(category("Unauthorized: invalid api key"), "authentication");
    assert_eq!(category("403 forbidden"), "authentication");
    assert_eq!(category("credential expired"), "authentication");
    assert_eq!(category("429 quota exceeded"), "rate_limit");
    assert_eq!(category("request timed out"), "timeout");
    assert_eq!(category("context length too long"), "context_limit");
    assert_eq!(category("maximum context length exceeded"), "context_limit");
    assert_eq!(category("network socket connection reset"), "network");
    assert_eq!(category("fetch failed"), "network");
    assert_eq!(
        category("503 overloaded, service unavailable"),
        "provider_unavailable"
    );
    assert_eq!(category("something unexpected happened"), "other");
    assert_eq!(
        error_category(Some(&assistant_message())),
        serde_json::Value::Null
    );
}

#[test]
fn provider_and_model_categories() {
    assert_eq!(provider_category(Some("prime")), "prime");
    assert_eq!(provider_category(Some("ANTHROPIC")), "anthropic");
    assert_eq!(provider_category(Some("custom-host")), "custom");
    assert_eq!(provider_category(None), "unknown");
    assert_eq!(model_category("glm-4.6"), "glm");
    assert_eq!(model_category("Claude-Sonnet-4"), "claude");
    assert_eq!(model_category("kimi-k2"), "kimi");
    assert_eq!(model_category("my-finetune"), "custom");
}

/// Tool calls fold into the run: built-in tools by name, MCP and custom
/// tools only as aggregates, and no raw tool name rides any event.
#[tokio::test]
async fn tool_calls_fold_into_run_aggregates() {
    let fixture = fixture();
    let assistant = assistant_message();
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    for (tool, is_error, start, end) in [
        ("bash", false, 1_000, 1_040),
        ("bash", true, 1_100, 1_110),
        ("edit", false, 1_200, 1_205),
        ("mcp__github__create_issue", false, 1_300, 1_350),
        ("private-extension-tool", false, 1_400, 1_401),
    ] {
        let (tool_start, tool_end) = tool_execution_event(tool, is_error);
        fixture.clock.set(start);
        emit(&fixture, tool_start);
        fixture.clock.set(end);
        emit(&fixture, tool_end);
    }
    emit(&fixture, message_end_event(assistant));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    let run = &runs[0];
    assert_eq!(run["tool_call_count"], serde_json::json!(5));
    assert_eq!(run["tool_error_count"], serde_json::json!(1));
    assert_eq!(run["tool_bash_call_count"], serde_json::json!(2));
    assert_eq!(run["tool_bash_error_count"], serde_json::json!(1));
    assert_eq!(run["tool_bash_duration_ms"], serde_json::json!(50));
    assert_eq!(run["tool_bash_max_duration_ms"], serde_json::json!(40));
    assert_eq!(run["tool_edit_call_count"], serde_json::json!(1));
    assert_eq!(run["mcp_tool_call_count"], serde_json::json!(1));
    assert_eq!(run["custom_tool_call_count"], serde_json::json!(1));
    assert!(
        run.get("tool_read_call_count").is_none(),
        "unused tools stay absent"
    );
    let names: Vec<String> = fixture.mock.event_names();
    assert_eq!(names, ["agent run completed"], "no per-call events");
    let all = serde_json::to_string(&fixture.mock.events()).unwrap();
    assert!(!all.contains("github"));
    assert!(!all.contains("private-extension-tool"));
}

/// Each `python_repl` result's `bashWaits` details fold into the run as two
/// primitive counters: awaits that finished inside their wait window vs
/// awaits that yielded a still-running snapshot.
#[tokio::test]
async fn bash_wait_outcomes_fold_into_run_counters() {
    let fixture = fixture();
    emit(&fixture, AgentEvent::AgentStart);
    for (completed, yielded) in [(1, 0), (2, 1)] {
        let (tool_start, mut tool_end) = tool_execution_event("python_repl", false);
        if let AgentEvent::ToolExecutionEnd { result, .. } = &mut tool_end {
            result.details =
                serde_json::json!({ "bashWaits": { "completed": completed, "yielded": yielded } });
        }
        emit(&fixture, tool_start);
        emit(&fixture, tool_end);
    }
    emit(&fixture, message_end_event(assistant_message()));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(
        (
            &runs[0]["bash_wait_completed_count"],
            &runs[0]["bash_wait_yielded_count"]
        ),
        (&serde_json::json!(3), &serde_json::json!(1))
    );
}

/// Skills, RLM child usage, MCP connector use, kernel boots, and feature
/// outcomes count into `agent session ended` instead of their own events.
#[tokio::test]
async fn session_counters_ride_session_ended() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    telemetry.note_skill_used();
    telemetry.note_skill_used();
    telemetry.note_child_usage_attributed(50_208, 2_929, 0, 0, 0.008_995_7);
    counters.note_mcp_connector_use();
    counters.note_kernel_bootstrap(true, true, 1_200);
    counters.note_kernel_bootstrap(false, false, 300);
    telemetry.note_feature_outcome("goal", "completed", Some("create"));
    telemetry.note_feature_outcome("not-a-feature", "completed", None);
    telemetry.end().await.unwrap();
    assert_eq!(fixture.mock.event_names(), ["agent session ended"]);
    let ended = &event_properties(&fixture.mock, "agent session ended").await[0];
    assert_eq!(ended["skill_use_count"], serde_json::json!(2));
    assert_eq!(ended["rlm_child_usage_count"], serde_json::json!(1));
    assert_eq!(ended["rlm_child_input_tokens"], serde_json::json!(50_208));
    assert_eq!(ended["rlm_child_output_tokens"], serde_json::json!(2_929));
    assert!((ended["rlm_child_cost"].as_f64().unwrap() - 0.008_995_7).abs() < 1e-9);
    assert_eq!(ended["mcp_connector_use_count"], serde_json::json!(1));
    assert_eq!(ended["kernel_bootstrap_count"], serde_json::json!(2));
    assert_eq!(ended["kernel_bootstrap_cold_count"], serde_json::json!(1));
    assert_eq!(ended["kernel_bootstrap_failed_count"], serde_json::json!(1));
    assert_eq!(ended["kernel_bootstrap_max_ms"], serde_json::json!(1_200));
    assert_eq!(ended["feature_goal_completed_count"], serde_json::json!(1));
    assert!(
        ended.get("web_search_count").is_none(),
        "a session that never searched carries no web search keys"
    );
}

/// Native web searches fold into `agent session ended`: counts, the
/// fallback and keyless shares, results, and the slowest latency; the
/// query and the provider never ride.
#[tokio::test]
async fn web_searches_fold_into_session_ended_without_the_query() {
    use crate::web_search::{ProviderFailure, ProviderId, SearchError, SearchReport};
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    let source = crate::web_search::SearchSource {
        title: "private title".to_string(),
        url: "https://private.example/".to_string(),
        snippet: None,
        published_date: None,
        author: None,
    };
    let failure = ProviderFailure {
        provider: ProviderId::Serper,
        reason: "401".to_string(),
        status: Some(401),
    };
    counters.note_web_search(&Ok(SearchReport {
        provider: ProviderId::Duckduckgo,
        response: crate::web_search::SearchResponse {
            sources: vec![source.clone(), source],
            ..Default::default()
        },
        failures: vec![failure.clone()],
        notes: Vec::new(),
        elapsed: std::time::Duration::from_millis(900),
    }));
    counters.note_web_search(&Err(SearchError {
        message: "private failure".to_string(),
        failures: vec![failure],
        elapsed: std::time::Duration::from_millis(2_500),
    }));
    telemetry.end().await.unwrap();
    let ended = &event_properties(&fixture.mock, "agent session ended").await[0];
    let web_search: serde_json::Map<String, serde_json::Value> = ended
        .iter()
        .filter(|(key, _)| key.starts_with("web_search"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(web_search),
        serde_json::json!({
            "web_search_count": 2,
            "web_search_failed_count": 1,
            "web_search_fallback_count": 1,
            "web_search_keyless_count": 1,
            "web_search_result_count": 2,
            "web_search_max_ms": 2_500,
        })
    );
    let all = serde_json::to_string(&fixture.mock.events()).unwrap();
    assert!(!all.contains("private"));
}

/// Thinned request views fold into `agent session ended` as counts and
/// sizes only; the session's sum over every thinning request.
#[tokio::test]
async fn context_thinning_folds_into_session_ended() {
    use crate::session_engine::context_thinning::ThinningStats;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_context_thinning(&ThinningStats {
        thinned_results: 3,
        chars_saved: 40_001,
    });
    counters.note_context_thinning(&ThinningStats {
        thinned_results: 5,
        chars_saved: 60_000,
    });
    telemetry.end().await.unwrap();
    let ended = &event_properties(&fixture.mock, "agent session ended").await[0];
    let thinning: serde_json::Map<String, serde_json::Value> = ended
        .iter()
        .filter(|(key, _)| key.starts_with("context_thinned"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(thinning),
        serde_json::json!({
            "context_thinned_request_count": 2,
            "context_thinned_result_count": 8,
            "context_thinned_chars_saved": 100_001,
            "context_thinned_tokens_saved": 25_001,
        })
    );
}

/// `build_client` reuses the installation id a TS install wrote to
/// `telemetry.json` (one user across both products) and mirrors events to
/// the local JSONL file under that id.
#[tokio::test]
// This test's delivery task reads the same process-wide env as the assertion.
// The current-thread runtime's task never acquires this test-only mutex.
#[allow(clippy::await_holding_lock)]
async fn build_client_reuses_the_ts_installation_id_and_mirrors() {
    let _env_lock = crate::packages::test_support::ENV_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let ts_id = "6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b";
    std::fs::write(
        agent_dir.join("telemetry.json"),
        format!("{{\n  \"version\": 1,\n  \"installationId\": \"{ts_id}\"\n}}"),
    )
    .unwrap();
    let settings = crate::settings::SettingsManager::create(dir.path(), &agent_dir);
    let client = build_client(&settings, &agent_dir);
    assert_eq!(client.install_id(), ts_id);
    client.track("agent started", base_properties("interactive"));
    client.flush().await.unwrap();
    // The live switch gates every sink, the mirror included: the repo's
    // cargo env (`DO_NOT_TRACK=1`) keeps it off under `cargo test`.
    let mirror = std::fs::read_to_string(agent_dir.join("telemetry.jsonl")).ok();
    if telemetry_switch(&settings).enabled() {
        let mirror = mirror.expect("the mirror recorded the event");
        let line: serde_json::Value = serde_json::from_str(mirror.lines().next().unwrap()).unwrap();
        assert_eq!(line["distinct_id"], ts_id);
        assert_eq!(line["name"], "agent started");
    } else {
        assert!(
            mirror.is_none(),
            "nothing is recorded while telemetry is off"
        );
    }
}

#[tokio::test]
async fn multiple_runs_merge_into_session_totals() {
    let fixture = fixture();
    let assistant = assistant_message();
    for _ in 0..2 {
        emit(&fixture, AgentEvent::AgentStart);
        emit(
            &fixture,
            AgentEvent::MessageStart {
                message: user_message(),
            },
        );
        emit(&fixture, AgentEvent::TurnStart);
        emit(&fixture, message_end_event(assistant.clone()));
        emit(
            &fixture,
            AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
        );
    }
    emit(&fixture, AgentEvent::AgentStart);
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 2);
}

#[tokio::test]
async fn the_prompt_trigger_and_run_index_ride_the_completed_run() {
    let fixture = fixture();
    let assistant = assistant_message();
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    fixture.clock.set(1_000);
    emit(&fixture, AgentEvent::AgentStart);
    emit(&fixture, AgentEvent::TurnStart);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    emit(&fixture, message_end_event(assistant.clone()));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    fixture.clock.set(2_000);
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    assert!(
        event_properties(&fixture.mock, "agent run started")
            .await
            .is_empty(),
        "no run-start event: the trigger rides the completed run"
    );
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["trigger"], serde_json::json!("prompt"));
    assert_eq!(runs[0]["run_index"], serde_json::json!(1));
}

#[tokio::test]
async fn continuation_run_reports_continuation_trigger() {
    let fixture = fixture();
    let assistant = assistant_message();
    emit(&fixture, AgentEvent::AgentStart);
    emit(&fixture, AgentEvent::TurnStart);
    emit(&fixture, message_end_event(assistant));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["trigger"], serde_json::json!("continuation"));
}

fn retry_start(telemetry: &SessionTelemetry, attempt: u32, delay_ms: u64) {
    telemetry.note_auto_retry_event(&AutoRetryEvent::Start {
        attempt,
        max_attempts: 3,
        delay_ms,
        error_message: String::new(),
    });
}

/// One model attempt inside the current run: the attempt's start, the
/// model call, its end.
fn attempt(fixture: &Fixture, message: AssistantMessage) {
    emit(fixture, AgentEvent::AgentStart);
    emit(fixture, AgentEvent::TurnStart);
    emit(fixture, message_end_event(message));
    emit(
        fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
}

/// TS one-run-per-turn: a turn that retried twice and then succeeded is
/// exactly one `agent run completed`, with `retry_count` 2, the success
/// outcome of the final attempt, and the failed attempts folded into the
/// error counters.
#[tokio::test]
async fn retries_fold_into_one_run_that_succeeds() {
    let fixture = fixture();
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    let failed = assistant_with_error("API Error: 429 rate limit exceeded with /home/user/secret");
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    attempt(&fixture, failed.clone());
    retry_start(&telemetry, 1, 500);
    attempt(&fixture, failed);
    retry_start(&telemetry, 2, 1_000);
    let mut recovered = assistant_message();
    recovered.usage.cost.total = 0.012;
    attempt(&fixture, recovered);
    telemetry.note_auto_retry_event(&AutoRetryEvent::End {
        success: true,
        attempt: 2,
        final_error: None,
    });
    telemetry.end().await.unwrap();

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1, "one run per turn");
    let run = &runs[0];
    assert_eq!(run["outcome"], serde_json::json!("success"));
    assert_eq!(run["error_category"], serde_json::Value::Null);
    assert_eq!(run["retry_count"], serde_json::json!(2));
    assert_eq!(run["retry_wait_ms"], serde_json::json!(1_500));
    assert_eq!(run["model_call_count"], serde_json::json!(3));
    assert_eq!(run["turn_count"], serde_json::json!(3));
    assert_eq!(run["model_error_count"], serde_json::json!(2));
    assert_eq!(run["error_rate_limit_count"], serde_json::json!(2));
    // A recovered failure keeps the turn's cost.
    assert_eq!(run["usage_complete"], serde_json::json!(true));
    assert_eq!(run["estimated_cost_usd"], serde_json::json!(0.012));
    let ended = event_properties(&fixture.mock, "agent session ended").await;
    assert_eq!(ended[0]["run_count"], serde_json::json!(1));
    assert_eq!(ended[0]["successful_run_count"], serde_json::json!(1));
    assert_eq!(ended[0]["failed_run_count"], serde_json::json!(0));
    assert_eq!(ended[0]["retry_count"], serde_json::json!(2));
    // The privacy contract: no raw provider text anywhere.
    let all = serde_json::to_string(&fixture.mock.events()).unwrap();
    assert!(!all.contains("API Error"));
    assert!(!all.contains("/home/user/secret"));
}

/// The stream loop guard's interventions ride the run: a cut stream counts
/// a trip, the retry after it counts a retry after a trip, and each
/// repeated-tool-call nudge counts once.
#[tokio::test]
async fn loop_guard_interventions_ride_the_run() {
    let fixture = fixture();
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    let mut cut = assistant_with_error("Model output started repeating itself");
    cut.diagnostics = Some(vec![pa_agent::types::AssistantMessageDiagnostic {
        kind: "provider_stream_failure".to_string(),
        timestamp: 0,
        error: None,
        details: Some(serde_json::json!({ "kind": "repetition_loop" })),
    }]);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    attempt(&fixture, cut);
    retry_start(&telemetry, 1, 10);
    telemetry.note_tool_repeat_nudge();
    attempt(&fixture, assistant_message());
    telemetry.end().await.unwrap();

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    let counts: Vec<&serde_json::Value> = [
        "loop_guard_trip_count",
        "loop_guard_retry_count",
        "loop_guard_tool_repeat_nudge_count",
    ]
    .iter()
    .map(|key| &runs[0][*key])
    .collect();
    let one = serde_json::json!(1);
    assert_eq!(counts, vec![&one, &one, &one]);
}

/// A turn whose retries are exhausted is one run with the error outcome
/// of its final attempt.
#[tokio::test]
async fn exhausted_retries_are_one_failed_run() {
    let fixture = fixture();
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    let failed = assistant_with_error("network connection reset");
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    attempt(&fixture, failed.clone());
    retry_start(&telemetry, 1, 500);
    attempt(&fixture, failed.clone());
    retry_start(&telemetry, 2, 500);
    attempt(&fixture, failed);
    telemetry.note_auto_retry_event(&AutoRetryEvent::End {
        success: false,
        attempt: 2,
        final_error: Some("network".to_string()),
    });
    telemetry.end().await.unwrap();

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1, "one run per turn");
    assert_eq!(runs[0]["outcome"], serde_json::json!("error"));
    assert_eq!(runs[0]["error_category"], serde_json::json!("network"));
    assert_eq!(runs[0]["retry_count"], serde_json::json!(2));
    assert_eq!(runs[0]["model_error_count"], serde_json::json!(3));
    assert_eq!(runs[0]["error_network_count"], serde_json::json!(3));
    assert_eq!(runs[0]["stop_reason"], serde_json::json!("error"));
    let ended = event_properties(&fixture.mock, "agent session ended").await;
    assert_eq!(ended[0]["failed_run_count"], serde_json::json!(1));
    assert_eq!(ended[0]["run_count"], serde_json::json!(1));
}

/// A retry whose wait is cancelled closes its run: the next user turn is
/// a run of its own, not a continuation of the abandoned retry.
#[tokio::test]
async fn a_cancelled_retry_never_absorbs_the_next_turn() {
    let fixture = fixture();
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    attempt(&fixture, assistant_with_error("network connection reset"));
    retry_start(&telemetry, 1, 500);
    telemetry.note_auto_retry_event(&AutoRetryEvent::End {
        success: false,
        attempt: 1,
        final_error: Some("Retry cancelled".to_string()),
    });
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    attempt(&fixture, assistant_message());
    telemetry.end().await.unwrap();

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 2, "the next turn is its own run");
    assert_eq!(runs[0]["outcome"], serde_json::json!("error"));
    assert_eq!(runs[0]["retry_count"], serde_json::json!(1));
    assert_eq!(runs[1]["outcome"], serde_json::json!("success"));
    assert_eq!(runs[1]["retry_count"], serde_json::json!(0));
}

#[tokio::test]
async fn run_completed_v2_enrichment() {
    let fixture = fixture();
    let mut assistant = assistant_message();
    assistant.usage.cost.total = 0.012;
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    emit(&fixture, AgentEvent::TurnStart);
    emit(&fixture, message_end_event(assistant));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["stop_reason"], serde_json::json!("stop"));
    assert_eq!(runs[0]["terminal_outcome"], serde_json::json!("success"));
    assert_eq!(runs[0]["successful_model_call_count"], serde_json::json!(1));
    assert_eq!(runs[0]["usage_complete"], serde_json::json!(true));
    assert_eq!(runs[0]["estimated_cost_usd"], serde_json::json!(0.012));
    let session = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    session.end().await.unwrap();
    let ended = event_properties(&fixture.mock, "agent session ended").await;
    assert_eq!(ended[0]["terminal_outcome"], serde_json::json!("success"));
}

/// The stream-gap timing: the largest quiet stretch between model
/// events reports on the run's timing stage.
#[tokio::test]
async fn stream_gap_tracks_the_largest_quiet_stretch() {
    let fixture = fixture();
    let assistant = assistant_message();
    fixture.clock.set(1_000);
    emit(&fixture, AgentEvent::AgentStart);
    emit(&fixture, AgentEvent::TurnStart);
    emit(&fixture, text_delta_event(&assistant));
    fixture.clock.set(1_050);
    emit(&fixture, text_delta_event(&assistant));
    fixture.clock.set(1_200);
    emit(&fixture, text_delta_event(&assistant));
    emit(&fixture, message_end_event(assistant));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs[0]["max_stream_gap_ms"], serde_json::json!(150));
    assert_eq!(runs[0]["run_to_first_text_ms"], serde_json::json!(0));
}

/// The bot-found edges, pinned: the trigger lands on run completed (the catalog lists it),
/// an aborted run's terminal outcome is the #2117 `cancelled`, a tool failure never touches
/// the model-failure chain, and a retry give-up never double-counts the chain.
#[tokio::test]
async fn bot_edges_the_trigger_lands_and_terminal_outcome_maps() {
    let fixture = fixture();
    let mut aborted = assistant_message();
    aborted.stop_reason = StopReason::Aborted;
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    emit(&fixture, AgentEvent::TurnStart);
    emit(&fixture, message_end_event(aborted));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    emit(&fixture, AgentEvent::AgentStart);
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["trigger"], serde_json::json!("prompt"));
    assert_eq!(runs[0]["terminal_outcome"], serde_json::json!("cancelled"));
}

/// End to end through the real client and the real analytics sink to a
/// local stub endpoint: a scripted interactive session (start, one run
/// with a tool call, end) posts TS-shaped bodies, and every legacy event
/// carries the full TS property set in the TS vocabulary.
#[tokio::test]
async fn legacy_events_reach_the_analytics_endpoint_in_the_ts_shape() {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/api/v1/agent-analytics/events",
        listener.local_addr().unwrap()
    );
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        let body: serde_json::Value =
                            serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
                        let accepted = body["events"].as_array().map_or(0, Vec::len);
                        let reply = format!("{{\"accepted\":{accepted}}}");
                        // Hand the body over before answering: the flush
                        // returns once the reply is read, and the test
                        // collects right after it.
                        let _ = tx.send(body);
                        let _ = write!(
                            stream,
                            "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                            reply.len()
                        );
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
        }
    });

    let install_id = "6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b";
    let mut config = TelemetryClientConfig::new(install_id);
    config.flush_interval = Duration::from_mins(10);
    config.sinks = vec![
        Arc::new(pa_telemetry::AnalyticsSink::new(url)) as Arc<dyn pa_telemetry::TelemetrySink>
    ];
    let client = TelemetryClient::spawn(config).unwrap();
    let mut fixture = fixture();
    fixture.client = client.clone();

    let mut started = base_properties("interactive");
    started.set(
        "session_id",
        Value::from("0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a"),
    );
    client.track("agent started", started);
    let assistant = assistant_message();
    emit(&fixture, AgentEvent::AgentStart);
    emit(
        &fixture,
        AgentEvent::MessageStart {
            message: user_message(),
        },
    );
    emit(&fixture, AgentEvent::TurnStart);
    emit(&fixture, text_delta_event(&assistant));
    let (tool_start, tool_end) = tool_execution_event("bash", false);
    emit(&fixture, tool_start);
    emit(&fixture, tool_end);
    emit(&fixture, message_end_event(assistant));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    let telemetry =
        SessionTelemetry::detached(client, fixture.state.clone(), "interactive".to_string());
    telemetry.end().await.unwrap();

    let bodies: Vec<serde_json::Value> = rx.try_iter().collect();
    assert!(!bodies.is_empty(), "the stub received the batches");
    for body in &bodies {
        println!("{}", serde_json::to_string_pretty(body).unwrap());
        let mut keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["events", "installation_id"],
            "the TS envelope, nothing else"
        );
        assert_eq!(body["installation_id"], install_id);
        for event in body["events"].as_array().unwrap() {
            let mut keys: Vec<&String> = event.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, ["id", "name", "properties", "timestamp"]);
        }
    }
    let events: Vec<&serde_json::Value> = bodies
        .iter()
        .flat_map(|body| body["events"].as_array().unwrap())
        .collect();
    let base = [
        "version",
        "os_family",
        "architecture",
        "install_method",
        "execution_mode",
        "libc",
        "libc_version",
        "cpu_baseline",
        "os_release",
        "os_product_version",
    ];
    let ts_keys: [(&str, &[&str]); 3] = [
        ("agent started", &["session_id"]),
        (
            "agent run completed",
            &[
                "session_id",
                "outcome",
                "duration_ms",
                "visible_ttft_ms",
                "first_model_event_ms",
                "model_latency_ms",
                "max_model_latency_ms",
                "model_call_count",
                "turn_count",
                "tool_call_count",
                "tool_error_count",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "total_tokens",
                "compaction_count",
                "retry_count",
                "provider_category",
                "model_category",
                "error_category",
            ],
        ),
        (
            "agent session ended",
            &[
                "session_id",
                "duration_ms",
                "prompt_count",
                "run_count",
                "successful_run_count",
                "failed_run_count",
                "aborted_run_count",
                "tool_call_count",
                "compaction_count",
                "model_call_count",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "total_tokens",
            ],
        ),
    ];
    for (name, keys) in ts_keys {
        let event = events
            .iter()
            .find(|event| event["name"] == name)
            .unwrap_or_else(|| panic!("{name} was sent"));
        let properties = event["properties"].as_object().unwrap();
        for key in base.iter().chain(keys.iter()) {
            assert!(
                properties.contains_key(*key),
                "{name} carries the TS key {key}"
            );
        }
        assert_eq!(properties["execution_mode"], "interactive");
        assert!(["linux", "darwin", "win32", "freebsd", "android"]
            .contains(&properties["os_family"].as_str().unwrap()));
        assert!(
            ["x64", "arm64", "ia32", "arm", "s390x", "ppc64", "riscv64", "loong64"]
                .contains(&properties["architecture"].as_str().unwrap())
        );
    }
    let run = events
        .iter()
        .find(|event| event["name"] == "agent run completed")
        .unwrap();
    assert_eq!(run["properties"]["outcome"], "success");
    assert_eq!(run["properties"]["provider_category"], "openai");
    assert_eq!(run["properties"]["model_category"], "gpt");
    assert_eq!(run["properties"]["error_category"], serde_json::Value::Null);
    // TS sums the session's tool calls from its runs (one call, counted once).
    let ended = events
        .iter()
        .find(|event| event["name"] == "agent session ended")
        .unwrap();
    assert_eq!(ended["properties"]["tool_call_count"], 1);
}

/// A fixture whose recording seams consult a live switch the test flips:
/// the same shape [`install_session_telemetry`] installs from the wiring
/// (minus the write-path epoch tracking, which the production switch
/// registers and the zero-event-flap test exercises separately).
fn fixture_with_switch(telemetry_enabled: Arc<dyn Fn() -> bool + Send + Sync>) -> Fixture {
    let mock = std::sync::Arc::new(MockSink::new());
    let client = client_for(&mock);
    let clock = TestClock::default();
    let now: Arc<dyn Fn() -> u64 + Send + Sync> = {
        let millis = clock.millis.clone();
        Arc::new(move || millis.load(std::sync::atomic::Ordering::Relaxed))
    };
    let state = Arc::new(Mutex::new(TelemetryState {
        session_id: "0197d0a0-8f5c-7f2a-b0e3-2d7e0d2b3b1a".to_string(),
        started_at: 1_000,
        totals: SessionTotals::default(),
        active_run: None,
        tool_starts: HashMap::new(),
        telemetry_enabled: Some(RecordingSwitch::test(telemetry_enabled)),
        recording: true,
        now,
        auth_baseline: crate::auth::resolver::AuthMetrics::current(),
    }));
    Fixture {
        client,
        state,
        clock,
        mock,
    }
}

/// The TUI counters' rule, ported to the session surface: while telemetry
/// is off nothing counts, so turning it on later never sends what
/// happened while it was off.
#[tokio::test]
async fn off_period_session_counters_never_count() {
    let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switch: Arc<dyn Fn() -> bool + Send + Sync> = {
        let on = Arc::clone(&on);
        Arc::new(move || on.load(std::sync::atomic::Ordering::Relaxed))
    };
    let fixture = fixture_with_switch(Arc::clone(&switch));
    let counters = Arc::new(SessionCounters::default());
    counters.set_telemetry_enabled(Arc::clone(&switch));
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);

    // Off: a skill use, a connector use, a kernel boot, a feature
    // outcome, and a child usage row all record nothing — the counters'
    // switch is the same live gate the engine installs at creation, so
    // the pre-install MCP/kernel window records nothing either.
    telemetry.note_skill_used();
    counters.note_mcp_connector_use();
    counters.note_kernel_bootstrap(true, true, 1_200);
    telemetry.note_feature_outcome("goal", "completed", Some("create"));
    telemetry.note_child_usage_attributed(50_208, 2_929, 0, 0, 0.008_995_7);

    // On: the same seams record again.
    on.store(true, std::sync::atomic::Ordering::Relaxed);
    telemetry.note_skill_used();
    telemetry.note_skill_used();

    telemetry.end().await.unwrap();
    let ended = &event_properties(&fixture.mock, "agent session ended").await[0];
    assert_eq!(ended["skill_use_count"], serde_json::json!(2));
    assert_eq!(
        ended["mcp_connector_use_count"],
        serde_json::json!(0),
        "the off-period connector use never counted"
    );
    assert_eq!(
        ended["rlm_child_usage_count"],
        serde_json::json!(0),
        "the off-period child usage never counted"
    );
    assert_eq!(
        ended["kernel_bootstrap_count"],
        serde_json::json!(0),
        "the off-period kernel boot never counted"
    );
    assert_eq!(
        ended["rlm_child_input_tokens"],
        serde_json::json!(0),
        "the off-period child tokens never counted"
    );
    let feature_keys: Vec<_> = ended
        .keys()
        .filter(|key| key.starts_with("feature_"))
        .collect();
    assert!(
        feature_keys.is_empty(),
        "the off-period feature outcome never counted"
    );
}

/// The run state machine stops recording while telemetry is off: a run
/// that happened in the off period never reports, and a later enable
/// sends only what the on period recorded.
#[tokio::test]
async fn off_period_run_facts_never_send() {
    let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switch: Arc<dyn Fn() -> bool + Send + Sync> = {
        let on = Arc::clone(&on);
        Arc::new(move || on.load(std::sync::atomic::Ordering::Relaxed))
    };
    let fixture = fixture_with_switch(switch);
    let assistant = assistant_message();

    // A whole run while off: start, a turn, a tool call, and its end.
    fixture.clock.set(1_000);
    emit(&fixture, AgentEvent::AgentStart);
    emit(&fixture, AgentEvent::TurnStart);
    let (tool_start, tool_end) = tool_execution_event("bash", false);
    emit(&fixture, tool_start);
    emit(&fixture, tool_end);
    fixture.clock.set(1_100);
    emit(&fixture, message_end_event(assistant.clone()));
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );

    // On again: the next run records normally.
    on.store(true, std::sync::atomic::Ordering::Relaxed);
    fixture.clock.set(2_000);
    emit(&fixture, AgentEvent::AgentStart);
    fixture.clock.set(2_050);
    emit(&fixture, AgentEvent::TurnStart);
    fixture.clock.set(2_100);
    emit(&fixture, message_end_event(assistant.clone()));
    fixture.clock.set(2_150);
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );

    fixture.clock.set(2_200);
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.end().await.unwrap();

    // One run completed: the on-period run. The off-period run never
    // existed, and its tool call never entered the session totals.
    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1, "only the on-period run reports");
    assert_eq!(runs[0]["turn_count"], serde_json::json!(1));
    assert_eq!(runs[0]["input_tokens"], serde_json::json!(100));
    let ended = &event_properties(&fixture.mock, "agent session ended").await[0];
    assert_eq!(ended["run_count"], serde_json::json!(1));
    assert_eq!(ended["tool_call_count"], serde_json::json!(0));
}

/// The recording switch resolves live: a fresh invocation reads the
/// settings as they are now, so the turn-boundary asks (and the
/// client's flush gate) observe a mid-session opt-out or re-enable the
/// moment it is saved. The events between boundaries deliberately use
/// the boundary cache; this proves only the raw switch has no cache of
/// its own.
#[test]
fn recording_switch_flips_immediately_with_the_settings() {
    let _env = CleanTelemetryEnv::default();
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let mut settings = crate::settings::SettingsManager::create(dir.path(), &agent_dir);
    settings.set_telemetry_enabled(true).unwrap();
    let switch = telemetry_enabled_switch(dir.path(), &agent_dir);
    assert!((switch.enabled)(), "on in settings records");

    // The off lands without a cache window: nothing the seams ask
    // after the flip may still see the pre-flip answer.
    settings.set_telemetry_enabled(false).unwrap();
    assert!(!(switch.enabled)(), "the off applies immediately");

    // The re-enable lands the same way.
    settings.set_telemetry_enabled(true).unwrap();
    assert!((switch.enabled)(), "the on applies immediately too");
}

/// The opt-out is the settings write itself: no telemetry state can
/// block it. A squatted (unwritable) install-id state — which used to
/// fail the command — never holds the disable hostage, and the opt-out
/// never touches (or mints) the state file.
#[test]
fn a_disable_lands_even_when_the_telemetry_state_is_unusable() {
    let _env = CleanTelemetryEnv::default();
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let mut settings = crate::settings::SettingsManager::create(dir.path(), &agent_dir);
    settings.set_telemetry_enabled(true).unwrap();
    // The state path is unusable: a directory squats it (no create or
    // rename could ever land there, for any user).
    std::fs::create_dir_all(agent_dir.join("telemetry.json")).unwrap();

    super::set_telemetry_enabled_text(&mut settings, &agent_dir, false).unwrap();
    assert!(
        !crate::settings::SettingsManager::create(dir.path(), &agent_dir).get_telemetry_enabled(),
        "the disable lands regardless of the telemetry state"
    );
    super::set_telemetry_enabled_text(&mut settings, &agent_dir, true).unwrap();
    assert!(
        crate::settings::SettingsManager::create(dir.path(), &agent_dir).get_telemetry_enabled(),
        "the re-enable lands the same way"
    );
}

/// Opting out never mints an identity: a fresh installation with no
/// install-id state disables without creating `telemetry.json`.
#[test]
fn a_disable_on_a_fresh_install_never_mints_the_install_id() {
    let _env = CleanTelemetryEnv::default();
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let mut settings = crate::settings::SettingsManager::create(dir.path(), &agent_dir);

    super::set_telemetry_enabled_text(&mut settings, &agent_dir, false).unwrap();

    assert!(
        !crate::settings::SettingsManager::create(dir.path(), &agent_dir).get_telemetry_enabled(),
        "the opt-out lands"
    );
    assert!(
        !agent_dir.join("telemetry.json").exists(),
        "the opt-out never creates the install-id state"
    );
}

/// A run active when telemetry goes off is severed, not merged: its
/// `AgentEnd` and the next run's `AgentStart` happen in the off period,
/// and the next on-period run starts clean with only its own facts.
#[tokio::test]
async fn a_run_active_when_telemetry_turns_off_is_severed_not_merged() {
    let on = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let switch: Arc<dyn Fn() -> bool + Send + Sync> = {
        let on = Arc::clone(&on);
        Arc::new(move || on.load(std::sync::atomic::Ordering::Relaxed))
    };
    let fixture = fixture_with_switch(switch);
    let assistant = assistant_message();

    // Run 1 starts while on: one turn, one tool call in flight.
    fixture.clock.set(1_000);
    emit(&fixture, AgentEvent::AgentStart);
    fixture.clock.set(1_050);
    emit(&fixture, AgentEvent::TurnStart);
    let (tool_start, _tool_end) = tool_execution_event("bash", false);
    emit(&fixture, tool_start);
    fixture.clock.set(1_100);
    emit(&fixture, message_end_event(assistant.clone()));

    // Telemetry goes off: run 1's tool end, its end, and the next
    // run's start all happen in the off period.
    on.store(false, std::sync::atomic::Ordering::Relaxed);
    fixture.clock.set(1_200);
    let (_tool_start2, tool_end) = tool_execution_event("bash", false);
    emit(&fixture, tool_end);
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );
    fixture.clock.set(1_300);
    emit(&fixture, AgentEvent::AgentStart);

    // Back on: the next run reports only its own window.
    on.store(true, std::sync::atomic::Ordering::Relaxed);
    fixture.clock.set(2_000);
    emit(&fixture, AgentEvent::AgentStart);
    fixture.clock.set(2_050);
    emit(&fixture, AgentEvent::TurnStart);
    fixture.clock.set(2_100);
    emit(&fixture, message_end_event(assistant.clone()));
    fixture.clock.set(2_150);
    emit(
        &fixture,
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    );

    fixture.clock.set(2_200);
    let telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.end().await.unwrap();

    let runs = event_properties(&fixture.mock, "agent run completed").await;
    assert_eq!(runs.len(), 1, "only the on-period run reports");
    assert_eq!(runs[0]["turn_count"], serde_json::json!(1));
    assert_eq!(
        runs[0]["duration_ms"],
        serde_json::json!(150),
        "the severed run never merges: the duration stays in run 2's window"
    );
    let ended = &event_properties(&fixture.mock, "agent session ended").await[0];
    assert_eq!(ended["run_count"], serde_json::json!(1));
    assert_eq!(
        ended["tool_call_count"],
        serde_json::json!(0),
        "the off-period tool end never counts"
    );
}

/// Goal verification and budget facts ride their own `agent goal verification` event at session
/// end (never new keys on `agent session ended`, which sits at the 128-key cap).
#[tokio::test]
async fn goal_verification_facts_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        GoalTelemetryFact::CheckRun,
        GoalTelemetryFact::CheckFailed,
        GoalTelemetryFact::CheckSkipped,
        GoalTelemetryFact::CheckRun,
        GoalTelemetryFact::JudgeCall,
        GoalTelemetryFact::JudgeNotMet,
        GoalTelemetryFact::CostBudgetStop,
    ] {
        counters.note_goal(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent goal verification"]
    );
    let goal = &event_properties(&fixture.mock, "agent goal verification").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = goal
        .iter()
        .filter(|(key, _)| key.starts_with("goal_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "goal_check_run_count": 2,
            "goal_check_failed_count": 1,
            "goal_check_skipped_count": 1,
            "goal_judge_call_count": 1,
            "goal_judge_not_met_count": 1,
            "goal_token_budget_stop_count": 0,
            "goal_cost_budget_stop_count": 1,
        })
    );
    let ended = &event_properties(&fixture.mock, "agent session ended").await[0];
    assert!(ended.keys().all(|key| !key.starts_with("goal_")));
}

/// The provider layer's Responses transport counts ride their own `agent responses transport`
/// event at session end (counts only), and the session ended event carries none of them.
#[tokio::test]
async fn responses_transport_counts_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_responses_transport(pa_ai::responses_transport::ResponsesTransportCounts {
        websocket_requests: 3,
        delta_requests: 2,
        full_context_requests: 1,
        stale_continuation_retries: 1,
        auto_websocket: 2,
        auto_sse_one_shot: 1,
        probes: 1,
        ..Default::default()
    });
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent responses transport"]
    );
    let transport = &event_properties(&fixture.mock, "agent responses transport").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = transport
        .iter()
        .filter(|(key, _)| key.starts_with("responses_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "responses_websocket_request_count": 3,
            "responses_delta_request_count": 2,
            "responses_full_context_request_count": 1,
            "responses_sse_fallback_count": 0,
            "responses_stale_continuation_retry_count": 1,
            "responses_auto_websocket_count": 2,
            "responses_auto_sse_one_shot_count": 1,
            "responses_auto_sse_no_websocket_wire_count": 0,
            "responses_auto_sse_unsupported_count": 0,
            "responses_auto_sse_demoted_count": 0,
            "responses_probe_count": 1,
            "responses_probe_failed_count": 0,
            "responses_websocket_without_wire_count": 0,
        })
    );
    let ended = &event_properties(&fixture.mock, "agent session ended").await[0];
    assert!(ended.keys().all(|key| !key.starts_with("responses_")));
}

/// The worker's endpoint discovery counts ride their own `agent endpoint discovery` event at
/// session end (counts only).
#[tokio::test]
async fn endpoint_discovery_counts_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_endpoint_discovery(pa_ai::responses_transport::DiscoveryCounts {
        probes: 3,
        found_responses: 1,
        found_websocket: 1,
        upgrades: 1,
        demotions: 0,
    });
    telemetry.end().await.unwrap();
    let discovery = &event_properties(&fixture.mock, "agent endpoint discovery").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = discovery
        .iter()
        .filter(|(key, _)| key.starts_with("endpoint_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "endpoint_probe_count": 3,
            "endpoint_found_responses_count": 1,
            "endpoint_found_websocket_count": 1,
            "endpoint_upgrade_count": 1,
            "endpoint_demotion_count": 0,
        })
    );
}

/// Kernel `web.read` outcomes ride their own `agent web reads` event at session end (counts by
/// kind, never a URL), and a session that read nothing sends none.
#[tokio::test]
async fn web_reads_ride_their_own_event() {
    use crate::web_read::{ReadKind, ReadOutcome};
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for (outcome, ms) in [
        (
            ReadOutcome::Read {
                kind: ReadKind::Site,
                truncated: false,
            },
            120,
        ),
        (
            ReadOutcome::Read {
                kind: ReadKind::Pdf,
                truncated: true,
            },
            900,
        ),
        (
            ReadOutcome::Read {
                kind: ReadKind::Page,
                truncated: false,
            },
            40,
        ),
        (ReadOutcome::Failed, 300),
    ] {
        counters.note_web_read(outcome, std::time::Duration::from_millis(ms));
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent web reads"]
    );
    let reads = &event_properties(&fixture.mock, "agent web reads").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = reads
        .iter()
        .filter(|(key, _)| key.starts_with("web_read_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "web_read_count": 4,
            "web_read_failed_count": 1,
            "web_read_site_count": 1,
            "web_read_pdf_count": 1,
            "web_read_page_count": 1,
            "web_read_truncated_count": 1,
            "web_read_max_ms": 900,
        })
    );
}

/// Kernel `fs.*` searches ride their own `agent fs search` event at session end (counts by
/// operation, never a pattern or path), and a session that never searched sends none.
#[tokio::test]
async fn fs_searches_ride_their_own_event() {
    use crate::fs_search::{FsOperation, SearchFact};
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for (operation, results, truncated, ms) in [
        (FsOperation::Grep, Some(12), false, 30),
        (FsOperation::Grep, Some(100), true, 250),
        (FsOperation::Glob, Some(7), false, 5),
        (FsOperation::Find, None, false, 1),
    ] {
        counters.note_fs_search(SearchFact {
            operation,
            results,
            truncated,
            elapsed: std::time::Duration::from_millis(ms),
        });
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent fs search"]
    );
    let search = &event_properties(&fixture.mock, "agent fs search").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = search
        .iter()
        .filter(|(key, _)| key.starts_with("fs_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "fs_grep_count": 2,
            "fs_glob_count": 1,
            "fs_find_count": 1,
            "fs_search_failed_count": 1,
            "fs_search_truncated_count": 1,
            "fs_search_result_count": 119,
            "fs_search_max_ms": 250,
        })
    );
}

/// Kernel `fs.read`/`fs.edit` calls ride their own `agent fs edit` event at session end (counts
/// by outcome, never a path or content), and a session that never used them sends none.
#[tokio::test]
async fn fs_edits_ride_their_own_event() {
    use crate::fs_edit::{EditFact, FsEditOperation};
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        EditFact::Read { lines: 40 },
        EditFact::Read { lines: 2 },
        EditFact::Failed {
            operation: FsEditOperation::Read,
        },
        EditFact::Applied { edits: 3 },
        EditFact::Rejected { stale: 2 },
        EditFact::Failed {
            operation: FsEditOperation::Edit,
        },
    ] {
        counters.note_fs_edit(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent fs edit"]
    );
    let edits = &event_properties(&fixture.mock, "agent fs edit").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = edits
        .iter()
        .filter(|(key, _)| key.starts_with("fs_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "fs_read_count": 2,
            "fs_read_line_count": 42,
            "fs_read_failed_count": 1,
            "fs_edit_count": 1,
            "fs_edit_op_count": 3,
            "fs_edit_rejected_count": 1,
            "fs_edit_stale_anchor_count": 2,
            "fs_edit_failed_count": 1,
        })
    );
}

/// Kernel `schedule.*` calls ride their own `agent repl schedule` event at session end (counts by
/// kind, never a prompt or schedule), and a session that never scheduled sends none.
#[tokio::test]
async fn repl_schedules_ride_their_own_event() {
    use crate::session_engine::runtime::ScheduleFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        ScheduleFact::AddedOnce,
        ScheduleFact::AddedRecurring,
        ScheduleFact::AddedRecurring,
        ScheduleFact::Listed,
        ScheduleFact::Cancelled,
        ScheduleFact::Rejected,
    ] {
        counters.note_repl_schedule(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent repl schedule"]
    );
    let schedule = &event_properties(&fixture.mock, "agent repl schedule").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = schedule
        .iter()
        .filter(|(key, _)| key.starts_with("schedule_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "schedule_once_added_count": 1,
            "schedule_recurring_added_count": 2,
            "schedule_list_count": 1,
            "schedule_cancelled_count": 1,
            "schedule_rejected_count": 1,
        })
    );
}

/// Kernel `checkpoint.*` calls ride their own `agent repl checkpoint` event at session end
/// (counts by outcome, never a label or path), and a session that never checkpointed sends none.
#[tokio::test]
async fn repl_checkpoints_ride_their_own_event() {
    use crate::session_engine::checkpoint_host::CheckpointFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        CheckpointFact::Created,
        CheckpointFact::Created,
        CheckpointFact::Listed,
        CheckpointFact::Restored,
        CheckpointFact::RestoreDeclined,
        CheckpointFact::RestoreDisabled,
        CheckpointFact::Failed,
    ] {
        counters.note_repl_checkpoint(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent repl checkpoint"]
    );
    let checkpoint = &event_properties(&fixture.mock, "agent repl checkpoint").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = checkpoint
        .iter()
        .filter(|(key, _)| key.starts_with("checkpoint_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "checkpoint_created_count": 2,
            "checkpoint_list_count": 1,
            "checkpoint_restored_count": 1,
            "checkpoint_restore_declined_count": 1,
            "checkpoint_restore_disabled_count": 1,
            "checkpoint_failed_count": 1,
        })
    );
}

/// Kernel `usage.get()` reads ride their own `agent usage reads` event at session end (a count,
/// never a cost or a token number), and a session that never read its usage sends none.
#[tokio::test]
async fn usage_reads_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_usage_read();
    counters.note_usage_read();
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent usage reads"]
    );
    let reads = &event_properties(&fixture.mock, "agent usage reads").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = reads
        .iter()
        .filter(|(key, _)| key.starts_with("usage_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({"usage_read_count": 2})
    );
}

/// Spills and kernel `ctx.*` reads ride their own `agent context paging` event at session end
/// (counts and a size bucket, never a handle or text).
#[tokio::test]
async fn context_paging_rides_its_own_event() {
    use crate::kernel::spill::host::CtxRead;
    use crate::kernel::spill::SpillFact;

    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_spill(SpillFact::Queued { bytes: 2_000_000 });
    counters.note_spill(SpillFact::Queued { bytes: 48_211 });
    counters.note_spill(SpillFact::NotStored);
    counters.note_spill(SpillFact::Evicted);
    counters.note_ctx_read(CtxRead::Page);
    counters.note_ctx_read(CtxRead::Page);
    counters.note_ctx_read(CtxRead::Grep);
    counters.note_ctx_read(CtxRead::Info);
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent context paging"]
    );
    let paging = &event_properties(&fixture.mock, "agent context paging").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = paging
        .iter()
        .filter(|(key, _)| key.starts_with("spill_") || key.starts_with("ctx_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "spill_count": 2,
            "spill_bytes_bucket": "lt_16mb",
            "spill_not_stored_count": 1,
            "spill_evicted_count": 1,
            "ctx_page_count": 2,
            "ctx_grep_count": 1,
            "ctx_tail_count": 0,
            "ctx_info_count": 1,
        })
    );
}

/// Kernel `worktree` operations ride their own `agent worktrees` event at session end (counts by
/// outcome, never a name or a path).
#[tokio::test]
async fn worktree_operations_ride_their_own_event() {
    use crate::worktree_host::WorktreeFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        WorktreeFact::Created,
        WorktreeFact::Created,
        WorktreeFact::Conflicted,
        WorktreeFact::Merged,
        WorktreeFact::Removed,
    ] {
        counters.note_worktree(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent worktrees"]
    );
    let worktrees = &event_properties(&fixture.mock, "agent worktrees").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = worktrees
        .iter()
        .filter(|(key, _)| key.starts_with("worktree_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "worktree_created_count": 2,
            "worktree_diff_count": 0,
            "worktree_merged_count": 1,
            "worktree_branched_count": 0,
            "worktree_conflict_count": 1,
            "worktree_removed_count": 1,
            "worktree_failed_count": 0,
        })
    );
}

/// Rate-limited `agent_message.send` deliveries ride their own `agent messages` event at session
/// end (a count, never a target or a body), and a session never throttled sends none.
#[tokio::test]
async fn agent_message_throttles_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_agent_message_refusals(AgentMessageRefusals {
        throttled: 1,
        wake_refused: 0,
    });
    counters.note_agent_message_refusals(AgentMessageRefusals {
        throttled: 3,
        wake_refused: 2,
    });
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent messages"]
    );
    let messages = &event_properties(&fixture.mock, "agent messages").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = messages
        .iter()
        .filter(|(key, _)| key.starts_with("agent_message_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "agent_message_throttled_count": 4,
            "agent_message_wake_refused_count": 2,
        })
    );
}

/// `agent_message.wait` outcomes ride their own `agent message waits` event at session end
/// (counts by outcome, never a sender).
#[tokio::test]
async fn agent_message_waits_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for outcome in ["message", "timeout", "timeout", "cycle"] {
        counters.note_agent_message_wait(outcome);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent message waits"]
    );
    let waits = &event_properties(&fixture.mock, "agent message waits").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = waits
        .iter()
        .filter(|(key, _)| key.starts_with("agent_message_wait_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "agent_message_wait_message_count": 1,
            "agent_message_wait_timeout_count": 2,
            "agent_message_wait_cycle_count": 1,
            "agent_message_wait_no_sender_count": 0,
        })
    );
}

/// Kernel `rlm.abort` calls ride their own `agent rlm child abort` event at session end (counts
/// by outcome, never a child id), and a session that never aborted sends none.
#[tokio::test]
async fn rlm_child_aborts_ride_their_own_event() {
    use crate::session_engine::rlm_host::RlmAbortOutcome;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for outcome in [
        Some(RlmAbortOutcome::Aborted),
        Some(RlmAbortOutcome::Aborted),
        Some(RlmAbortOutcome::Idle),
        None,
    ] {
        counters.note_rlm_abort(outcome);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent rlm child abort"]
    );
    let abort = &event_properties(&fixture.mock, "agent rlm child abort").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = abort
        .iter()
        .filter(|(key, _)| key.starts_with("rlm_abort_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "rlm_abort_aborted_count": 2,
            "rlm_abort_idle_count": 1,
            "rlm_abort_failed_count": 1,
        })
    );
}

/// Cells that continued in the background, and the source of each armed cell window, ride their
/// own `agent repl background cells` event at session end (counts only: never code, output, a
/// cell id, or a duration).
#[tokio::test]
async fn background_cells_ride_their_own_event() {
    use crate::kernel::cell_window::CellWindowSource;
    use crate::kernel::shared::BackgroundCellFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        BackgroundCellFact::Backgrounded,
        BackgroundCellFact::Backgrounded,
        BackgroundCellFact::Backgrounded,
        BackgroundCellFact::EndedOk {
            ran_for: Some(std::time::Duration::from_secs(90)),
        },
        BackgroundCellFact::Interrupted,
    ] {
        counters.note_background_cell(fact);
    }
    for source in [
        CellWindowSource::Learned,
        CellWindowSource::Learned,
        CellWindowSource::ModelTimeout,
        CellWindowSource::ColdDefault,
    ] {
        counters.note_cell_window(source);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent repl background cells"]
    );
    let cells = &event_properties(&fixture.mock, "agent repl background cells").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = cells
        .iter()
        .filter(|(key, _)| key.starts_with("cell_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "cell_backgrounded_count": 3,
            "cell_background_ok_count": 1,
            "cell_background_error_count": 0,
            "cell_background_interrupted_count": 1,
            "cell_window_learned_count": 2,
            "cell_window_timeout_count": 1,
            "cell_window_setting_count": 0,
            "cell_window_default_count": 1,
        })
    );
}

/// Kernel `notify(...)` pings and `model.set_effort(...)` calls ride their own `agent repl
/// notify` event at session end (counts only, never a message or a level).
#[tokio::test]
async fn repl_notifications_and_effort_switches_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_notification(true);
    counters.note_notification(false);
    counters.note_effort_switch(true);
    counters.note_effort_switch(false);
    counters.note_effort_switch(false);
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent repl notify"]
    );
    let notify = &event_properties(&fixture.mock, "agent repl notify").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = notify
        .iter()
        .filter(|(key, _)| key.starts_with("notify_") || key.starts_with("effort_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "notify_count": 2,
            "notify_undelivered_count": 1,
            "effort_set_count": 1,
            "effort_refused_count": 2,
        })
    );
}

/// Kernel `rlm.decide(...)` outcomes ride their own `agent repl decide` event at session end
/// (counts only, never a question or a choice).
#[tokio::test]
async fn repl_decisions_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        DecideFact::Decided,
        DecideFact::Decided,
        DecideFact::Limited,
        DecideFact::Failed,
        DecideFact::Decided,
    ] {
        counters.note_decision(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent repl decide"]
    );
    let decide = &event_properties(&fixture.mock, "agent repl decide").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = decide
        .iter()
        .filter(|(key, _)| key.starts_with("decide_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "decide_count": 3,
            "decide_limited_count": 1,
            "decide_failed_count": 1,
        })
    );
}

/// Kernel `rlm.reload()` and `settings.get()` calls ride their own `agent repl reload` event at
/// session end (counts only, never a skill name or a setting value).
#[tokio::test]
async fn repl_reloads_and_settings_reads_ride_their_own_event() {
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_repl_reload(2);
    counters.note_repl_reload(0);
    counters.note_repl_settings_read();
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent repl reload"]
    );
    let reload = &event_properties(&fixture.mock, "agent repl reload").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = reload
        .iter()
        .filter(|(key, _)| key.starts_with("repl_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "repl_reload_count": 2,
            "repl_reload_skills_added_count": 2,
            "repl_settings_get_count": 1,
        })
    );
}

/// Kernel `mcp.login` outcomes and past-session recall ride their own `agent kernel login and
/// recall` event at session end (counts only), and a session that used neither sends none.
#[tokio::test]
async fn kernel_login_and_recall_ride_their_own_event() {
    use crate::session_engine::session_search::SessionRecallFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    counters.note_kernel_mcp_login(Some(true));
    counters.note_kernel_mcp_login(Some(false));
    counters.note_kernel_mcp_login(None);
    for fact in [
        SessionRecallFact::Searched { hits: 3 },
        SessionRecallFact::Searched { hits: 0 },
        SessionRecallFact::Read,
        SessionRecallFact::Failed,
    ] {
        counters.note_session_recall(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent kernel login and recall"]
    );
    let recall = &event_properties(&fixture.mock, "agent kernel login and recall").await[0];
    let counts: serde_json::Map<String, serde_json::Value> = recall
        .iter()
        .filter(|(key, _)| key.starts_with("kernel_mcp_") || key.starts_with("session_search_"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(
        serde_json::Value::Object(counts),
        serde_json::json!({
            "kernel_mcp_login_count": 1,
            "kernel_mcp_login_failed_count": 1,
            "kernel_mcp_login_unavailable_count": 1,
            "session_search_count": 2,
            "session_search_hit_count": 3,
            "session_search_read_count": 1,
            "session_search_failed_count": 1,
        })
    );
}

/// Summarized test runs ride their own `agent test summaries` event at session end (counts only: never a test name or output).
#[tokio::test]
async fn test_summaries_ride_their_own_event() {
    use crate::kernel::test_runs::TestRunFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        TestRunFact {
            failed: 2,
            failures: 2,
            stored: true,
        },
        TestRunFact {
            failed: 0,
            failures: 0,
            stored: false,
        },
    ] {
        counters.note_test_summary(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent test summaries"]
    );
    let properties: serde_json::Map<String, serde_json::Value> =
        event_properties(&fixture.mock, "agent test summaries").await[0]
            .iter()
            .filter(|(key, _)| key.starts_with("test_summary_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    assert_eq!(
        serde_json::Value::Object(properties),
        serde_json::json!({
            "test_summary_count": 2,
            "test_summary_failing_count": 1,
            "test_summary_failed_test_count": 2,
            "test_summary_failure_handle_count": 2,
            "test_summary_stored_count": 1,
        })
    );
}

/// Checks after edits ride their own `agent edit checks` event at session end (counts only: never a path or output).
#[tokio::test]
async fn edit_checks_ride_their_own_event() {
    use crate::kernel::edit_checks::EditCheckOutcome;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for outcome in [
        EditCheckOutcome::Late,
        EditCheckOutcome::Found,
        EditCheckOutcome::Clean,
        EditCheckOutcome::Skipped,
        EditCheckOutcome::Skipped,
    ] {
        counters.note_edit_check(outcome);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent edit checks"]
    );
    let properties: serde_json::Map<String, serde_json::Value> =
        event_properties(&fixture.mock, "agent edit checks").await[0]
            .iter()
            .filter(|(key, _)| key.starts_with("edit_check_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    assert_eq!(
        serde_json::Value::Object(properties),
        serde_json::json!({
            "edit_check_run_count": 2,
            "edit_check_found_count": 1,
            "edit_check_late_count": 1,
            "edit_check_skipped_count": 2,
            "edit_check_failed_count": 0,
        })
    );
}

/// Read-only cache lookups ride their own `agent readonly cache` event at session end (counts only: never a command or a path).
#[tokio::test]
async fn readonly_cache_ride_their_own_event() {
    use crate::kernel::command_cache::CacheLookup;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for (file_read, hit) in [(false, true), (false, true), (false, false), (true, true)] {
        counters.note_cache_lookup(CacheLookup { file_read, hit });
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent readonly cache"]
    );
    let properties: serde_json::Map<String, serde_json::Value> =
        event_properties(&fixture.mock, "agent readonly cache").await[0]
            .iter()
            .filter(|(key, _)| key.starts_with("readonly_cache_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    assert_eq!(
        serde_json::Value::Object(properties),
        serde_json::json!({
            "readonly_cache_hit_count": 2,
            "readonly_cache_miss_count": 1,
            "readonly_cache_fs_read_hit_count": 1,
            "readonly_cache_fs_read_miss_count": 0,
        })
    );
}

/// Error enrichments ride their own `agent error enrichment` event at session end (counts only: never a path, a line, or output).
#[tokio::test]
async fn error_enrichment_ride_their_own_event() {
    use crate::kernel::error_context::EnrichmentFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        EnrichmentFact {
            failed_commands: 1,
            locations: 2,
            skipped: 1,
            timed_out: false,
        },
        EnrichmentFact {
            failed_commands: 0,
            locations: 0,
            skipped: 3,
            timed_out: true,
        },
    ] {
        counters.note_error_enrichment(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent error enrichment"]
    );
    let properties: serde_json::Map<String, serde_json::Value> =
        event_properties(&fixture.mock, "agent error enrichment").await[0]
            .iter()
            .filter(|(key, _)| key.starts_with("error_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    assert_eq!(
        serde_json::Value::Object(properties),
        serde_json::json!({
            "error_enrichment_count": 1,
            "error_location_count": 2,
            "error_location_skipped_count": 4,
            "error_enrichment_timeout_count": 1,
            "error_failed_command_count": 1,
        })
    );
}

/// `fs.symbol` queries ride their own `agent symbol index` event at session end (counts only: never a name or a path).
#[tokio::test]
async fn symbol_index_ride_their_own_event() {
    use crate::symbol_index::SymbolFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        SymbolFact {
            definitions: 1,
            references: 4,
            partial: true,
        },
        SymbolFact {
            definitions: 2,
            references: 0,
            partial: false,
        },
    ] {
        counters.note_symbol_query(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent symbol index"]
    );
    let properties: serde_json::Map<String, serde_json::Value> =
        event_properties(&fixture.mock, "agent symbol index").await[0]
            .iter()
            .filter(|(key, _)| key.starts_with("symbol_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    assert_eq!(
        serde_json::Value::Object(properties),
        serde_json::json!({
            "symbol_query_count": 2,
            "symbol_query_partial_count": 1,
            "symbol_definition_count": 3,
            "symbol_reference_count": 4,
        })
    );
}

/// Neighbour prefetches ride their own `agent prefetch` event at session end (counts only: never a path).
#[tokio::test]
async fn prefetch_ride_their_own_event() {
    use crate::prefetch::PrefetchFact;
    let fixture = fixture();
    let counters = Arc::new(SessionCounters::default());
    let mut telemetry = SessionTelemetry::detached(
        fixture.client.clone(),
        fixture.state.clone(),
        "interactive".to_string(),
    );
    telemetry.counters = Arc::clone(&counters);
    for fact in [
        PrefetchFact {
            issued: 3,
            used: 0,
            unused: 0,
            skipped: false,
        },
        PrefetchFact {
            issued: 0,
            used: 1,
            unused: 2,
            skipped: true,
        },
    ] {
        counters.note_prefetch(fact);
    }
    telemetry.end().await.unwrap();
    assert_eq!(
        fixture.mock.event_names(),
        ["agent session ended", "agent prefetch"]
    );
    let properties: serde_json::Map<String, serde_json::Value> =
        event_properties(&fixture.mock, "agent prefetch").await[0]
            .iter()
            .filter(|(key, _)| key.starts_with("prefetch_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
    assert_eq!(
        serde_json::Value::Object(properties),
        serde_json::json!({
            "prefetch_read_count": 2,
            "prefetch_issued_count": 3,
            "prefetch_used_count": 1,
            "prefetch_unused_count": 2,
            "prefetch_skipped_count": 1,
        })
    );
}
