//! The cache seam and its warming rule: request hints, the economic
//! verdict, arming and refreshing near the TTL, cancellation by a real
//! request, and the hanging-refresh guard (a refresh that never returns
//! must not delay the next real turn).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_agent::stream::{event_stream, AssistantMessageEvent, LlmContext, StreamFn};
use pa_agent::types::{AssistantMessage, CacheRetention, Model, StopReason, UsageCost};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::policy::{
    self, CachePrices, DueWarmer, StopReason as WarmStop, WarmPhase, WarmShape, WarmVerdict,
};
use super::{wrap_stream_fn, CacheWarmingConfig, PendingWorkProbe, SessionRole};
use crate::session_engine::telemetry::SessionCounters;
use crate::settings::{CacheRetentionSetting, CacheWarmingSetting};

const STATIC_PREFIX: &str = "static layers";
const SYSTEM_PROMPT: &str = "static layers\n\nsession tail";
/// Opus-class prices ($ per million tokens): reads 0.1x, 5-minute writes 1.25x.
const INPUT_PRICE: f64 = 5.0;

fn cost() -> UsageCost {
    UsageCost {
        input: INPUT_PRICE,
        output: 25.0,
        cache_read: 0.5,
        cache_write: 6.25,
        total: 0.0,
    }
}

fn model() -> Model {
    Model {
        cost: cost(),
        ..Model::unknown()
    }
}

fn message(stop_reason: StopReason, usage: &Value) -> AssistantMessage {
    serde_json::from_value(json!({
        "content": [],
        "usage": usage,
        "stopReason": stop_reason,
    }))
    .expect("assistant message fixture")
}

/// A provider payload carrying five-minute cache markers on the system and
/// the last message, as the Anthropic provider sends it.
fn marked_payload() -> Value {
    json!({
        "max_tokens": 32000,
        "system": [{ "type": "text", "text": "s", "cache_control": { "type": "ephemeral" } }],
        "messages": [{ "role": "user", "content": [
            { "type": "text", "text": "go", "cache_control": { "type": "ephemeral" } },
        ] }],
    })
}

/// What one fake provider call saw.
#[derive(Debug, Clone, PartialEq)]
struct SeenRequest {
    max_tokens: Option<u64>,
    cache_retention: Option<CacheRetention>,
    cached_system_prefix_len: Option<usize>,
}

/// A fake provider fn: records each call, runs the payload hook with
/// [`marked_payload`], and answers `answer` at once.
fn answering_stream_fn(
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    answer: Arc<Mutex<AssistantMessage>>,
) -> StreamFn {
    Arc::new(move |model, _context, options| {
        seen.lock().unwrap().push(SeenRequest {
            max_tokens: options.max_tokens,
            cache_retention: options.cache_retention,
            cached_system_prefix_len: options.cached_system_prefix_len,
        });
        if let Some(hook) = &options.on_payload {
            hook(marked_payload(), &model);
        }
        let message = answer.lock().unwrap().clone();
        Box::pin(async move {
            let (handle, stream) = event_stream();
            handle.push(AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message: message.clone(),
            });
            handle.end(Some(message));
            Ok(Box::new(stream) as Box<dyn pa_agent::stream::ModelStream>)
        })
    })
}

struct Harness {
    stream_fn: StreamFn,
    real: Arc<Mutex<Vec<SeenRequest>>>,
    real_answer: Arc<Mutex<AssistantMessage>>,
    warm_calls: mpsc::UnboundedReceiver<Instant>,
    counters: Arc<SessionCounters>,
}

fn harness(
    mode: CacheWarmingSetting,
    role: SessionRole,
    pending_work: bool,
    warm_stream_fn: impl FnOnce(mpsc::UnboundedSender<Instant>) -> StreamFn,
) -> Harness {
    let real = Arc::new(Mutex::new(Vec::new()));
    let real_answer = Arc::new(Mutex::new(message(
        StopReason::ToolUse,
        &json!({ "input": 10, "cacheWrite": 100_000 }),
    )));
    let (warm_tx, warm_calls) = mpsc::unbounded_channel();
    let counters = Arc::new(SessionCounters::default());
    let probe: PendingWorkProbe = Arc::new(move || Box::pin(async move { pending_work }));
    let stream_fn = wrap_stream_fn(
        CacheWarmingConfig {
            mode,
            retention: CacheRetentionSetting::Short,
            static_prefix: STATIC_PREFIX.into(),
            role,
            pending_work: probe,
            counters: Arc::clone(&counters),
            warm_stream_fn: warm_stream_fn(warm_tx),
        },
        answering_stream_fn(Arc::clone(&real), Arc::clone(&real_answer)),
    );
    Harness {
        stream_fn,
        real,
        real_answer,
        warm_calls,
        counters,
    }
}

/// A warm fn that answers like the provider (a cache read, one token).
fn answering_warm_fn(warm_tx: mpsc::UnboundedSender<Instant>) -> StreamFn {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let answer = Arc::new(Mutex::new(message(
        StopReason::Length,
        &json!({ "input": 10, "cacheRead": 100_000, "output": 1, "cost": { "total": 0.05 } }),
    )));
    let inner = answering_stream_fn(Arc::clone(&seen), answer);
    Arc::new(move |model, context, options| {
        assert_eq!(options.max_tokens, Some(1), "a refresh asks for one token");
        warm_tx.send(Instant::now()).unwrap();
        inner(model, context, options)
    })
}

async fn real_round(stream_fn: &StreamFn) {
    let context = LlmContext {
        system_prompt: Some(SYSTEM_PROMPT.into()),
        ..LlmContext::default()
    };
    let mut stream = stream_fn(
        model(),
        context,
        pa_agent::stream::StreamRequestOptions::default(),
    )
    .await
    .expect("real request");
    while stream.next_event().await.is_some() {}
    stream.result().await.expect("real result");
}

fn session_cache_properties(counters: &SessionCounters) -> Value {
    let mut properties = pa_telemetry::Properties::new();
    counters.write_into(&mut properties);
    let keys = [
        "cache_warm_sent_count",
        "cache_warm_skipped_count",
        "cache_warm_cost_usd",
        "cache_savings_usd",
    ];
    Value::Object(
        keys.iter()
            .filter_map(|key| Some(((*key).to_string(), properties.get(key)?.clone())))
            .collect(),
    )
}

#[test]
fn retention_precedence_is_env_then_setting_then_short() {
    assert_eq!(
        [
            policy::request_cache_retention(Some("long"), CacheRetentionSetting::Short),
            policy::request_cache_retention(Some("short"), CacheRetentionSetting::Long),
            policy::request_cache_retention(Some(""), CacheRetentionSetting::Long),
            policy::request_cache_retention(None, CacheRetentionSetting::Long),
            policy::request_cache_retention(None, CacheRetentionSetting::Short),
        ],
        // An env value leaves the option unset: the provider reads the env itself.
        [
            None,
            None,
            Some(CacheRetention::Long),
            Some(CacheRetention::Long),
            Some(CacheRetention::Short),
        ]
    );
}

#[test]
fn the_warm_shape_reads_marker_ttls_and_refuses_budget_thinking() {
    let mut one_hour = marked_payload();
    one_hour["system"][0]["cache_control"]["ttl"] = json!("1h");
    let mut budget = marked_payload();
    budget["thinking"] = json!({ "type": "enabled", "budget_tokens": 2048 });
    let mut adaptive = marked_payload();
    adaptive["thinking"] = json!({ "type": "adaptive" });
    let bedrock = json!({
        "system": [{ "text": "s" }, { "cachePoint": { "type": "default", "ttl": "1h" } }],
        "messages": [],
    });
    let five_minutes = Some(WarmShape {
        ttl: Duration::from_secs(300),
    });
    assert_eq!(
        [
            policy::warm_shape(&marked_payload()),
            policy::warm_shape(&one_hour),
            policy::warm_shape(&budget),
            policy::warm_shape(&adaptive),
            policy::warm_shape(&bedrock),
            policy::warm_shape(&json!({ "system": [{ "text": "s" }], "messages": [] })),
        ],
        [
            five_minutes,
            five_minutes,
            None,
            five_minutes,
            Some(WarmShape {
                ttl: Duration::from_secs(3600)
            }),
            None,
        ]
    );
}

#[test]
fn refreshes_are_due_at_nine_tenths_of_the_ttl_from_the_last_read() {
    let five_minutes = Duration::from_secs(300);
    assert_eq!(
        [
            policy::refresh_delay(five_minutes, Duration::ZERO),
            policy::refresh_delay(five_minutes, Duration::from_secs(100)),
            policy::refresh_delay(five_minutes, Duration::from_secs(400)),
            policy::refresh_delay(Duration::from_secs(3600), Duration::ZERO),
        ],
        [
            Duration::from_secs(270),
            Duration::from_secs(170),
            Duration::ZERO,
            Duration::from_mins(54),
        ]
    );
}

/// The rule: refresh only when the write the next round avoids, weighted by
/// the phase's chance of a next round, minus the refresh's own read clears
/// five cents; `active` never warms an idle root; the caps stop it.
#[test]
fn the_verdict_follows_the_expected_saving_and_the_caps() {
    let prices = CachePrices::for_ttl(&cost(), Duration::from_secs(300));
    let due = |mode, phase, prompt_tokens, age_minutes: u64, refreshes| {
        policy::verdict(&DueWarmer {
            mode,
            phase,
            age: Duration::from_secs(age_minutes * 60),
            refreshes,
            prompt_tokens,
            prices,
        })
    };
    let refresh = |saving: f64| WarmVerdict::Refresh {
        expected_saving_usd: saving,
    };
    // 100k tokens: write 0.625, read 0.05 -> certain next round saves 0.525
    // (less the one output token, $0.000025).
    let certain = 0.575 - 0.05 - 0.000_025;
    assert_eq!(
        [
            due(
                CacheWarmingSetting::Active,
                WarmPhase::ToolRunning,
                100_000,
                4,
                0
            ),
            due(
                CacheWarmingSetting::Active,
                WarmPhase::AwaitingWork,
                100_000,
                4,
                0
            ),
            due(CacheWarmingSetting::Active, WarmPhase::Idle, 100_000, 4, 0),
            due(CacheWarmingSetting::Idle, WarmPhase::Idle, 100_000, 4, 0),
            due(
                CacheWarmingSetting::Active,
                WarmPhase::ToolRunning,
                5_000,
                4,
                0
            ),
            due(
                CacheWarmingSetting::Active,
                WarmPhase::ToolRunning,
                100_000,
                61,
                0
            ),
            due(
                CacheWarmingSetting::Active,
                WarmPhase::AwaitingParent,
                100_000,
                31,
                0
            ),
            due(
                CacheWarmingSetting::Active,
                WarmPhase::ToolRunning,
                100_000,
                4,
                12
            ),
            due(
                CacheWarmingSetting::Off,
                WarmPhase::ToolRunning,
                100_000,
                4,
                0
            ),
        ]
        .map(|verdict| match verdict {
            // Round the float so the table compares whole values.
            WarmVerdict::Refresh {
                expected_saving_usd,
            } => refresh((expected_saving_usd * 1e9).round() / 1e9),
            WarmVerdict::Stop(reason) => WarmVerdict::Stop(reason),
        }),
        [
            refresh((certain * 1e9_f64).round() / 1e9),
            refresh((certain * 1e9_f64).round() / 1e9),
            WarmVerdict::Stop(WarmStop::IdleNotWarmed),
            // 0.15 * 0.575 - 0.05 is under five cents.
            WarmVerdict::Stop(WarmStop::Unprofitable),
            WarmVerdict::Stop(WarmStop::Unprofitable),
            WarmVerdict::Stop(WarmStop::Capped),
            WarmVerdict::Stop(WarmStop::Capped),
            WarmVerdict::Stop(WarmStop::Capped),
            WarmVerdict::Stop(WarmStop::Disabled),
        ]
    );
}

/// A real request carries the static-prefix length (only when its system
/// prompt starts with the static layers) and the resolved retention.
#[tokio::test]
async fn real_requests_carry_the_static_prefix_and_retention() {
    let harness = harness(
        CacheWarmingSetting::Off,
        SessionRole::Root,
        false,
        answering_warm_fn,
    );
    real_round(&harness.stream_fn).await;
    let mut other = (harness.stream_fn)(
        model(),
        LlmContext {
            system_prompt: Some("a different prompt".into()),
            ..LlmContext::default()
        },
        pa_agent::stream::StreamRequestOptions::default(),
    )
    .await
    .expect("request");
    while other.next_event().await.is_some() {}
    let retention = policy::request_cache_retention(
        std::env::var(policy::CACHE_RETENTION_ENV).ok().as_deref(),
        CacheRetentionSetting::Short,
    );
    assert_eq!(
        *harness.real.lock().unwrap(),
        vec![
            SeenRequest {
                max_tokens: None,
                cache_retention: retention,
                cached_system_prefix_len: Some(STATIC_PREFIX.len()),
            },
            SeenRequest {
                max_tokens: None,
                cache_retention: retention,
                cached_system_prefix_len: None,
            },
        ]
    );
}

/// While the round's tool runs, the warmer refreshes at 270 s (nine tenths
/// of the five-minute TTL), then every 270 s after each refresh's read; the
/// session counters carry the refreshes and the cache saving.
#[tokio::test(start_paused = true)]
async fn a_running_tool_is_warmed_at_nine_tenths_of_the_ttl() {
    let mut harness = harness(
        CacheWarmingSetting::Active,
        SessionRole::Root,
        false,
        answering_warm_fn,
    );
    let start = Instant::now();
    real_round(&harness.stream_fn).await;
    let first = harness.warm_calls.recv().await.expect("first refresh");
    let second = harness.warm_calls.recv().await.expect("second refresh");
    assert_eq!(
        (first - start, second - first),
        (Duration::from_secs(270), Duration::from_secs(270))
    );
    // The real round wrote 100k tokens: -(100k * (6.25 - 5)) / 1e6 = -0.125;
    // its savings accrue from later reads, so the ended event floors it at zero.
    tokio::task::yield_now().await;
    let properties = session_cache_properties(&harness.counters);
    assert_eq!(properties["cache_warm_sent_count"], json!(2));
    assert_eq!(properties["cache_savings_usd"], json!(0.0));
}

/// A settled root turn is not warmed under `active`; the same turn with a
/// background job or a running child is, and so is a subagent waiting on
/// its parent.
#[tokio::test(start_paused = true)]
async fn a_settled_turn_warms_only_while_work_or_a_parent_is_pending() {
    let cases = [
        (SessionRole::Root, false, false),
        (SessionRole::Root, true, true),
        (SessionRole::Subagent, false, true),
    ];
    for (role, pending_work, warms) in cases {
        let mut harness = harness(
            CacheWarmingSetting::Active,
            role,
            pending_work,
            answering_warm_fn,
        );
        *harness.real_answer.lock().unwrap() = message(
            StopReason::Stop,
            &json!({ "input": 10, "cacheWrite": 100_000 }),
        );
        real_round(&harness.stream_fn).await;
        let warmed = tokio::time::timeout(Duration::from_secs(600), harness.warm_calls.recv())
            .await
            .is_ok();
        assert_eq!(warmed, warms, "{role:?} pending_work={pending_work}");
        if !warms {
            assert_eq!(
                session_cache_properties(&harness.counters)["cache_warm_skipped_count"],
                json!(1)
            );
        }
    }
}

/// A real request takes the armed warmer down: nothing is sent afterwards.
#[tokio::test(start_paused = true)]
async fn a_real_request_cancels_the_armed_warmer() {
    let mut harness = harness(
        CacheWarmingSetting::Active,
        SessionRole::Root,
        false,
        answering_warm_fn,
    );
    real_round(&harness.stream_fn).await;
    tokio::time::sleep(Duration::from_secs(200)).await;
    // The next round errors, so it arms nothing of its own.
    *harness.real_answer.lock().unwrap() = message(StopReason::Error, &json!({}));
    real_round(&harness.stream_fn).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(3600), harness.warm_calls.recv())
            .await
            .is_err(),
        "no refresh after a real request started"
    );
}

/// The AGENTS.md hanging-sink guard: a refresh whose provider call never
/// returns must not delay the next real turn, and the turn drops it.
#[tokio::test(start_paused = true)]
async fn a_hanging_refresh_never_delays_the_next_real_turn() {
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&dropped);
    let polled = Arc::clone(&polls);
    let mut harness = harness(
        CacheWarmingSetting::Active,
        SessionRole::Root,
        false,
        move |warm_tx| -> StreamFn {
            Arc::new(move |_model, _context, _options| {
                warm_tx.send(Instant::now()).unwrap();
                let guard = DropFlag(Arc::clone(&flag));
                let polled = Arc::clone(&polled);
                Box::pin(async move {
                    let _guard = guard;
                    polled.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<anyhow::Result<Box<dyn pa_agent::stream::ModelStream>>>()
                        .await
                })
            })
        },
    );
    real_round(&harness.stream_fn).await;
    harness
        .warm_calls
        .recv()
        .await
        .expect("the refresh went out");
    // Let the warmer reach its hanging provider call.
    while polls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    // With paused time, any await on the hanging refresh would let the clock
    // run past this bound.
    let before = Instant::now();
    tokio::time::timeout(Duration::from_millis(1), real_round(&harness.stream_fn))
        .await
        .expect("the real turn does not wait on the refresh");
    assert_eq!(Instant::now(), before);
    tokio::task::yield_now().await;
    assert!(
        dropped.load(Ordering::SeqCst),
        "the hanging refresh was dropped"
    );
    assert_eq!(harness.real.lock().unwrap().len(), 2);
}

/// One request body's cacheable prefix, in Anthropic's render order (tools,
/// system, messages): each block's bytes (its `cache_control` stripped) and
/// whether a marker closes it.
fn render_blocks(payload: &Value) -> Vec<(String, bool)> {
    let mut blocks = Vec::new();
    let mut push = |block: &Value| {
        let mut block = block.clone();
        let marked = block
            .as_object_mut()
            .and_then(|map| map.remove("cache_control"))
            .is_some();
        blocks.push((block.to_string(), marked));
    };
    for tool in payload["tools"].as_array().into_iter().flatten() {
        push(tool);
    }
    for block in payload["system"].as_array().into_iter().flatten() {
        push(block);
    }
    for message in payload["messages"].as_array().into_iter().flatten() {
        match &message["content"] {
            Value::Array(content) => content.iter().for_each(&mut push),
            other => push(other),
        }
    }
    blocks
}

/// A prompt-cache model of Anthropic's: an entry per marker keyed on the
/// exact prefix before it; a request reads its longest cached marker prefix
/// and writes everything up to its last marker past that. Tokens are
/// estimated as bytes / 4 (inferred, not tokenizer counts).
#[derive(Default)]
struct SimulatedCache {
    entries: std::collections::HashSet<String>,
}

impl SimulatedCache {
    /// `(read, written)` estimated tokens for one request body.
    fn send(&mut self, payload: &Value) -> (usize, usize) {
        let mut prefix = String::new();
        let mut marker_prefixes = Vec::new();
        for (block, marked) in render_blocks(payload) {
            prefix.push_str(&block);
            if marked {
                marker_prefixes.push(prefix.clone());
            }
        }
        let read = marker_prefixes
            .iter()
            .rev()
            .find(|prefix| self.entries.contains(*prefix))
            .map_or(0, String::len);
        let last = marker_prefixes.last().map_or(0, String::len);
        self.entries.extend(marker_prefixes);
        (read / 4, last.saturating_sub(read) / 4)
    }
}

/// The measured win of the split, end to end through the session's cache
/// seam and the real Anthropic request builder: a root session and two
/// spawned subagents share the static layers, so with the split the
/// subagents read those layers from the cache instead of writing them.
/// Request bodies are recorded from the provider's payload hook (the
/// loopback endpoint refuses the connection after the body is built).
#[tokio::test]
async fn a_session_and_two_subagents_write_the_static_layers_once() {
    use crate::prompts::system_prompt::{system_prompt_breakdown, BuildSystemPromptOptions};

    let prompt = |depth: u32, log: &str| {
        system_prompt_breakdown(&BuildSystemPromptOptions {
            cwd: "/work/project".into(),
            messages_path: Some(format!("/work/sessions/{log}.jsonl")),
            selected_tools: Some(vec![pa_types::ai::PYTHON_TOOL_NAME, "bash", "edit"]),
            allow_recursion: Some(true),
            rlm_depth: Some(depth),
            rlm_parent_agent: (depth > 0).then_some("root"),
            can_spawn: true,
            ..Default::default()
        })
    };
    let sessions = [
        prompt(0, "root"),
        prompt(1, "child-a"),
        prompt(1, "child-b"),
    ];
    let static_prefix = sessions[0].assembled[..sessions[0].cached_prefix_len].to_string();
    assert!(sessions
        .iter()
        .all(|breakdown| breakdown.assembled.starts_with(&static_prefix)));

    let model: pa_types::ai::Model = serde_json::from_value(json!({
        "id": "claude-opus-5-5",
        "name": "Claude Opus 5.5",
        "api": "anthropic-messages",
        "provider": "anthropic",
        "baseUrl": "http://127.0.0.1:9",
        "reasoning": false,
        "input": ["text"],
        "cost": { "input": 4.0, "output": 20.0, "cacheRead": 0.2, "cacheWrite": 5.0 },
        "contextWindow": 1_000_000,
        "maxTokens": 128_000,
    }))
    .expect("model fixture");
    let tools = [pa_types::ai::PYTHON_TOOL_NAME, "bash", "edit"].map(|name| {
        pa_agent::stream::ToolDefinition {
            name: name.into(),
            description: format!("The {name} tool."),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    });

    let mut totals = Vec::new();
    for static_prefix in [String::new(), static_prefix.clone()] {
        let mut cache = SimulatedCache::default();
        let mut writes = Vec::new();
        for breakdown in &sessions {
            let (body_tx, mut body_rx) = mpsc::unbounded_channel::<Value>();
            let stream_fn = wrap_stream_fn(
                CacheWarmingConfig {
                    mode: CacheWarmingSetting::Off,
                    retention: CacheRetentionSetting::Short,
                    static_prefix: static_prefix.clone(),
                    role: SessionRole::Root,
                    pending_work: Arc::new(|| Box::pin(async { false })),
                    counters: Arc::new(SessionCounters::default()),
                    warm_stream_fn: Arc::new(|_, _, _| {
                        Box::pin(async { Err(anyhow::anyhow!("no refresh in this test")) })
                    }),
                },
                crate::session_engine::provider_adapter::real_stream_fn(
                    Some("sk-ant-api-loopback".into()),
                    model.clone(),
                ),
            );
            let context = LlmContext {
                system_prompt: Some(breakdown.assembled.clone()),
                messages: vec![serde_json::from_value(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "Summarize the repository." }],
                    "timestamp": 1,
                }))
                .expect("user message")],
                tools: tools.to_vec(),
            };
            let options = pa_agent::stream::StreamRequestOptions {
                on_payload: Some(Arc::new(move |payload: Value, _model: &Model| {
                    body_tx.send(payload).ok();
                    None
                })),
                ..Default::default()
            };
            let agent_model: Model =
                crate::session_engine::provider_adapter::json_round_trip(&model)
                    .expect("model round trip");
            let stream = stream_fn(agent_model, context, options).await;
            let body = body_rx.recv().await.expect("the request body was built");
            drop(stream);
            writes.push(cache.send(&body).1);
        }
        totals.push(writes);
    }
    let static_tokens = static_prefix.len() / 4;
    println!(
        "static prefix: {} bytes (~{static_tokens} tokens, inferred bytes/4); \
         cache-write tokens per request [root, child A, child B]: before {:?}, after {:?}",
        static_prefix.len(),
        totals[0],
        totals[1],
    );
    let (before, after) = (&totals[0], &totals[1]);
    // The root writes the same either way (one more block wrapper aside);
    // each subagent stops re-writing the static layers.
    assert!(before[0].abs_diff(after[0]) <= 16);
    for child in 1..3 {
        assert!(
            before[child] - after[child] + 16 >= static_tokens,
            "child {child}: before {} after {}",
            before[child],
            after[child]
        );
    }
}

/// The bodies the real Anthropic request builder produced for one engine
/// session, and a signal per one-token refresh.
struct RecordedSession {
    engine: crate::session_engine::engine::SessionEngine,
    bodies: Arc<Mutex<Vec<Value>>>,
    refreshes: mpsc::UnboundedReceiver<()>,
    _dir: tempfile::TempDir,
}

/// An engine subagent session (its settled turns warm while it waits on the
/// parent) resumed over 17 tool rounds of 4,000 chars in a 20k window, so
/// context thinning applies. Its provider records each request body built by
/// the real Anthropic builder (the loopback endpoint refuses the connection
/// afterwards) and answers from a script: a refresh reads the cache, every
/// other call answers `NOT MET` text (a plain reply to the main loop).
async fn recorded_thinning_session() -> RecordedSession {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "compaction": { "enabled": false } }).to_string(),
    )
    .unwrap();
    let ai_model: pa_types::ai::Model = serde_json::from_value(json!({
        "id": "claude-opus-5-5", "name": "Claude Opus 5.5", "api": "anthropic-messages",
        "provider": "anthropic", "baseUrl": "http://127.0.0.1:9", "reasoning": false,
        "input": ["text"],
        "cost": { "input": 5000.0, "output": 25.0, "cacheRead": 500.0, "cacheWrite": 6250.0 },
        "contextWindow": 20_000, "maxTokens": 4096,
    }))
    .expect("model fixture");
    let model: Model = crate::session_engine::provider_adapter::json_round_trip(&ai_model)
        .expect("model round trip");
    let mut manager = crate::session::manager::SessionManager::in_memory(dir.path());
    let mut history =
        vec![json!({ "role": "user", "content": "fix the failing build", "timestamp": 1 })];
    for round in 0..17 {
        let call = format!("call-{round}");
        let output: String = (0..)
            .map(|line| format!("[{round}] line {line}: value={}\n", line * 7 + round))
            .take(400)
            .collect::<String>()
            .chars()
            .take(4_000)
            .collect();
        history.push(json!({
            "role": "assistant",
            "content": [{ "type": "toolCall", "id": call, "name": "python_repl", "arguments": { "code": "run()" } }],
            "api": "anthropic-messages", "provider": "anthropic", "model": "claude-opus-5-5",
            "usage": {}, "stopReason": "toolUse", "timestamp": 2,
        }));
        history.push(json!({
            "role": "toolResult", "toolCallId": call, "toolName": "python_repl",
            "content": [{ "type": "text", "text": output }], "isError": false, "timestamp": 3,
        }));
    }
    for message in history {
        // Through the loop's message type, which fills the usage defaults.
        let message: pa_agent::types::AgentMessage =
            serde_json::from_value(message).expect("history row");
        manager
            .append_message(serde_json::from_value(serde_json::to_value(message).unwrap()).unwrap())
            .unwrap();
    }

    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (refresh_tx, refreshes) = mpsc::unbounded_channel();
    let builder = crate::session_engine::provider_adapter::real_stream_fn(
        Some("sk-ant-api-loopback".into()),
        ai_model,
    );
    let recorded = Arc::clone(&bodies);
    let provider: StreamFn = Arc::new(move |model, context, mut options| {
        let builder = Arc::clone(&builder);
        let recorded = Arc::clone(&recorded);
        let refresh_tx = refresh_tx.clone();
        Box::pin(async move {
            let refresh = options.max_tokens == Some(1);
            let (body_tx, mut body_rx) = mpsc::unbounded_channel::<Value>();
            let seam = options.on_payload.take();
            options.on_payload = Some(Arc::new(move |payload: Value, model: &Model| {
                let next = seam.as_ref().and_then(|hook| hook(payload.clone(), model));
                body_tx.send(next.clone().unwrap_or(payload)).ok();
                next
            }));
            drop(builder(model, context, options).await);
            let body = body_rx.recv().await.expect("the request body was built");
            recorded.lock().unwrap().push(body);
            let reply: AssistantMessage = serde_json::from_value(if refresh {
                json!({ "content": [], "usage": { "cacheRead": 1_000, "output": 1 }, "stopReason": "length" })
            } else {
                json!({
                    "content": [{ "type": "text", "text": "VERDICT: NOT MET\nGAP: more work" }],
                    "usage": { "cacheWrite": 1_000 }, "stopReason": "stop",
                })
            })
            .expect("scripted reply");
            if refresh {
                refresh_tx.send(()).ok();
            }
            let (handle, stream) = event_stream();
            handle.push(AssistantMessageEvent::Done {
                reason: reply.stop_reason,
                message: reply.clone(),
            });
            handle.end(Some(reply));
            Ok(Box::new(stream) as Box<dyn pa_agent::stream::ModelStream>)
        })
    });
    let engine = crate::session_engine::engine::create_session(
        crate::session_engine::engine::SessionEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir,
            model: Some(model),
            stream_fn: Some(provider),
            session_manager: Some(manager),
            daemonless: true,
            rlm_depth: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    RecordedSession {
        engine,
        bodies,
        refreshes,
        _dir: dir,
    }
}

impl RecordedSession {
    async fn turn(&self, text: &str) {
        self.engine
            .session
            .prompt(text, crate::session_engine::PromptOptions::default())
            .await
            .unwrap();
        self.engine.session.agent().wait_for_idle().await;
    }

    /// The recorded body whose last message carries `marker`.
    fn body_ending_with(&self, marker: &str) -> Value {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .find(|body| {
                body["messages"]
                    .as_array()
                    .and_then(|messages| messages.last())
                    .is_some_and(|last| last.to_string().contains(marker))
            })
            .cloned()
            .unwrap_or_else(|| panic!("no request ends with {marker:?}"))
    }
}

/// A body's cacheable blocks (tools, system, messages) without their markers.
fn prefix_blocks(body: &Value) -> Vec<String> {
    render_blocks(body)
        .into_iter()
        .map(|(block, _marked)| block)
        .collect()
}

/// Thinning x cache warming: the refresh re-sends the thinned request the
/// last real turn sent, byte for byte but for its one-token cap, and the next
/// real turn's request extends it, so the refresh keeps alive the entry that
/// turn reads.
#[tokio::test]
async fn a_refresh_resends_the_thinned_request_the_next_turn_extends() {
    let mut session = recorded_thinning_session().await;
    session.turn("continue").await;
    tokio::time::pause();
    tokio::time::timeout(Duration::from_secs(600), session.refreshes.recv())
        .await
        .expect("a refresh while the subagent waits on its parent")
        .expect("refresh signal");
    tokio::time::resume();
    session.turn("next").await;

    let sent = session.body_ending_with("continue");
    let mut refresh = session
        .bodies
        .lock()
        .unwrap()
        .iter()
        .find(|body| body["max_tokens"] == json!(1))
        .cloned()
        .expect("the refresh body");
    assert!(
        sent.to_string().contains("thinned from the request"),
        "the real request was thinned"
    );
    // Thinning rewrites message bodies only: the split system blocks and the
    // last user turn carry the markers, within Anthropic's four.
    assert_eq!(
        render_blocks(&sent)
            .iter()
            .filter(|(_block, marked)| *marked)
            .count(),
        3
    );
    refresh["max_tokens"] = sent["max_tokens"].clone();
    assert_eq!(refresh, sent);
    let (sent, next) = (
        prefix_blocks(&sent),
        prefix_blocks(&session.body_ending_with("next")),
    );
    assert_eq!(next[..sent.len()], sent[..]);
}

/// Thinning x split cache x goal judge: the judge forks the request the main
/// loop last sent as the provider received it (the thinned view and the
/// split static/session system blocks), so it reads that request's cache
/// entry and only the rubric is new.
#[tokio::test]
async fn the_goal_judge_extends_the_main_loops_thinned_split_request() {
    let session = recorded_thinning_session().await;
    session.turn("continue").await;
    let handlers = session.engine.provisioner.host_handlers().clone();
    for (request_type, data) in [
        (
            "goal.create",
            json!({ "objective": "fix the build", "verify": "judge" }),
        ),
        ("goal.complete", json!({})),
    ] {
        let handler = handlers.get(request_type).unwrap().clone();
        handler(crate::kernel::shared::HostRequestPayload { data })
            .await
            .unwrap();
    }

    let (sent, judge) = (
        prefix_blocks(&session.body_ending_with("continue")),
        prefix_blocks(&session.body_ending_with("<goal_judge>")),
    );
    assert_eq!(judge[..sent.len()], sent[..]);
}
