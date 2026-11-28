#![cfg_attr(
    unix,
    expect(
        clippy::large_futures,
        reason = "a session's futures are large by design, as in src/lib.rs"
    )
)]
#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "the verifier is one scripted scenario, end to end"
    )
)]
#![cfg_attr(
    unix,
    expect(
        clippy::cast_possible_truncation,
        reason = "the whole floats Scheme writes back are the host's integer counts"
    )
)]

//! Verifier: the turn-boundary host-request contract (`model.info`, `compact.*`, `refine.*`) over a
//! REAL kernel: one turn's `eval` cell calls `(%host-request ...)` directly; the raw
//! handler responses are asserted, then the scheduled refinement reaches the boundary seam.
#![cfg(unix)]

use std::path::Path;

use pa_agent::scripted::{tool_call_turn_steps, ScriptStep, ScriptedProvider, ScriptedTurn};
use pa_agent::stream::AssistantMessageEvent;
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::turn_boundary::PendingRefine;
use pa_core::session_engine::PromptOptions;
use serde_json::{json, Value};

/// `value` with each float that holds a whole number as that integer.
fn whole_numbers(value: Value) -> Value {
    match value {
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() && float.fract() == 0.0 => json!(float as i64),
            _ => Value::Number(number),
        },
        Value::Array(items) => items.into_iter().map(whole_numbers).collect(),
        Value::Object(fields) => fields
            .into_iter()
            .map(|(key, field)| (key, whole_numbers(field)))
            .collect(),
        other @ (Value::Null | Value::Bool(_) | Value::String(_)) => other,
    }
}

/// The kernel cell: the raw host requests the `refine`/`compact` skill modules
/// issue, recorded on disk for the verifier (and printed so the tool result
/// carries the same proof into the session transcript).
fn harness_cell(receipt_path: &Path) -> String {
    format!(
        "(define model-info (%host-request \"model.info\" (hash)))
         (define compact-status (%host-request \"compact.status\" (hash)))
         (define refine-before (%host-request \"refine.status\" (hash)))
         (define refine-run (%host-request \"refine.run\"
                                          (hash 'instructions \"persist the kernel round-trip contract observation\"
                                                'global #t)))
         (define refine-after (%host-request \"refine.status\" (hash)))
         (define compact-run (%host-request \"compact.run\" (hash 'instructions \"keep the observation\")))
         (define written
           (fs-write {receipt_path:?}
                     (value->jsexpr-string
                       (hash 'model_info model-info
                             'compact_status compact-status
                             'refine_status_before refine-before
                             'refine_run refine-run
                             'refine_status_after refine-after
                             'compact_run compact-run))))
         (displayln \"HARNESS_CELL_OK\")",
        receipt_path = receipt_path.display().to_string(),
    )
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

/// The registry model `model.info` reports from: input modalities included.
fn registry_model() -> pa_types::ai::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "test", "provider": "faux",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4_000
    }))
    .expect("faux registry model")
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_boundary_host_requests_round_trip_through_a_real_kernel() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");
    let receipt_path = dir.path().join("harness-receipt.json");

    // A persisted session: the refine handlers register only for depth-0
    // sessions with a local harness state dir (TS `_autoRefineAllowedForSession`).
    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(sessions_dir));

    let model = scripted_model();
    let provider = std::sync::Arc::new(ScriptedProvider::new(model.clone()));
    // One turn: the model calls the kernel cell. Then the turn settles. The scripted provider
    // reports zero usage, but `compact.status` anchors its estimate on the last assistant usage (TS
    // `getContextUsage`), so the tool-call turn carries a real one: 120 tokens.
    let mut steps = tool_call_turn_steps(
        &model,
        Some("running the harness cell"),
        vec![(
            "call-1",
            pa_types::ai::KERNEL_TOOL_NAME,
            json!({ "code": harness_cell(&receipt_path) }),
        )],
    );
    for step in &mut steps {
        if let ScriptStep::Event(event) = step {
            if let AssistantMessageEvent::Done { message, .. } = &mut **event {
                message.usage = pa_agent::types::Usage {
                    input: 100,
                    output: 20,
                    cache_read: 0,
                    cache_write: 0,
                    total_tokens: 120,
                    cost: pa_agent::types::UsageCost::default(),
                };
            }
        }
    }
    provider.push_turn(ScriptedTurn::Events(steps));
    provider.push_text_turn("boundary reached");

    let engine = create_session(SessionEngineConfig {
        on_late_sent_agent_message: None,
        semantic_edges: None,
        cron_store: None,
        queued_steering_probe: None,
        should_stop_after_turn: None,
        image_model_router: None,
        steering_mode: None,
        follow_up_mode: None,
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
        factory_child: false,
        daemonless: true,
        telemetry: None,
        model_info: Some(registry_model()),
        mcp_manager: None,
        prewarm_kernel: None,
        on_background_work_settled: None,
        bash_notice_wake: None,
        kernel_notice_sink: None,
        call_policy: None,
        queued_goal_context_purge: None,
    })
    .await
    .expect("create_session");

    let outcome = engine
        .prompt("run the harness cell", PromptOptions::default())
        .await
        .expect("prompt");
    assert_eq!(
        outcome,
        pa_core::session_engine::PromptOutcome::Prompt,
        "the prompt must reach the model loop"
    );

    let raw = std::fs::read_to_string(&receipt_path)
        .unwrap_or_else(|_| panic!("kernel cell wrote no receipt at {}", receipt_path.display()));
    // Scheme writes every number as a float; the host's counts read as integers again.
    let payload: Value = whole_numbers(serde_json::from_str(&raw).expect("receipt json"));

    assert_eq!(
        payload["model_info"],
        json!({ "id": "faux-1", "provider": "faux", "input": ["text"], "contextWindow": 200_000, "thinkingLevel": "off" }),
        "model_info: {}",
        payload["model_info"]
    );
    let compact_status = &payload["compact_status"];
    assert_eq!(compact_status["scheduled"], false, "{payload}");
    assert_eq!(compact_status["context_window"], 200_000, "{payload}");
    // The usage anchor (the faux turn's 120 tokens) with nothing trailing it.
    assert_eq!(compact_status["tokens"], 120, "{payload}");
    assert_eq!(compact_status["percent"], 0.06, "{payload}");
    assert_eq!(
        payload["refine_status_before"],
        json!({ "pending": false, "in_flight": false }),
        "{payload}"
    );
    assert_eq!(payload["refine_run"]["scheduled"], true, "{payload}");
    assert_eq!(
        payload["refine_run"]["note"],
        "Refinement runs when the current turn ends; applied edits are appended to your context as a refinement notice and you resume automatically. Continue working normally."
    );
    assert_eq!(
        payload["refine_status_after"],
        json!({ "pending": true, "in_flight": false }),
        "{payload}"
    );
    assert_eq!(payload["compact_run"]["scheduled"], false, "{payload}");
    assert_eq!(
        payload["compact_run"]["reason"],
        "session is too short to compact"
    );

    // The cell output entered the session transcript (the model saw the
    // same proof the receipt carries).
    let serialized = {
        let entries = engine.session.entries().await;
        serde_json::to_string(&entries).expect("entries json")
    };
    assert!(
        serialized.contains("HARNESS_CELL_OK"),
        "no kernel cell output in the session rows"
    );

    // The scheduled refinement reached the turn-boundary seam: the runtime
    // consumes exactly this request after the settled turn (the daemon
    // worker's boundary pass; unit-tested at the registry level).
    assert_eq!(
        engine.turn_boundary.take_refine().await,
        Some(PendingRefine {
            instructions: Some("persist the kernel round-trip contract observation".to_string()),
            global: true,
        })
    );
    assert!(
        engine.turn_boundary.take_refine().await.is_none(),
        "the refinement request must be consumed exactly once"
    );
    assert!(
        engine.turn_boundary.take_compaction().await.is_none(),
        "the skipped compact.run must not schedule anything"
    );
}
