//! The print run's disposal drain on its failure path: the prompt loop
//! itself stays in pa-cli, so this verifier stays with it while the turn
//! boundary it drives lives in pa-runtime.

use std::sync::Arc;

use pa_core::session_engine::engine::SessionEngine;
use serde_json::json;

/// The refinement planner's reply, held by the faux clock for an hour: the
/// run must still be waiting on it when it returns.
const HELD_PLAN: &str = r#"{"summary":"background note","edits":[]}"#;

/// A faux-driven persisted engine over `dir` with auto-refine on and
/// compaction off: the seed turn's reply, then the held planner reply.
async fn faux_engine(dir: &std::path::Path) -> (SessionEngine, pa_types::ai::Model) {
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "compaction": {"enabled": false}, "autoRefine": {"enabled": true, "compact": true, "cooldownMs": 0} })
            .to_string(),
    )
    .unwrap();
    let parsed = pa_ai::faux::script::parse_faux_script(&json!({
        "responses": [{"text": "seed"}, {"text": HELD_PLAN, "delayMs": 3_600_000}],
    }))
    .unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![parsed.model]),
            ..Default::default()
        });
    registration.set_responses(parsed.responses);
    let model = registration.get_model();
    let engine = pa_core::session_engine::engine::create_session(
        pa_core::session_engine::engine::SessionEngineConfig {
            on_late_sent_agent_message: None,
            semantic_edges: None,
            cron_store: None,
            steering_mode: None,
            follow_up_mode: None,
            image_model_router: None,
            telemetry: None,
            cwd: dir.to_path_buf(),
            agent_dir,
            mcp_manager: None,
            model: Some(
                pa_core::session_engine::provider_adapter::json_round_trip(&model)
                    .expect("the faux model crosses the loop boundary"),
            ),
            thinking_level: None,
            stream_fn: Some(pa_core::session_engine::provider_adapter::real_stream_fn(
                None,
                model.clone(),
            )),
            tools: Vec::new(),
            custom_system_prompt: None,
            prompt_guidelines: Vec::new(),
            generic_mcp_servers: Vec::new(),
            allow_recursion: None,
            session_manager: Some(pa_core::session::manager::SessionManager::persisted(
                dir,
                &dir.join("sessions"),
            )),
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
            model_info: Some(model.clone()),
            prewarm_kernel: None,
            on_background_work_settled: None,
            bash_notice_wake: None,
            queued_goal_context_purge: None,
            queued_steering_probe: None,
            should_stop_after_turn: None,
        },
    )
    .await
    .expect("the faux session assembles");
    (engine, model)
}

/// Every print exit path runs the disposal drain: a `/refine` still planning
/// when a later prompt fails through `?` must land its outcome row before the
/// run returns — the session drop would abort it.
#[tokio::test(start_paused = true)]
async fn a_failed_print_run_still_drains_an_in_flight_refine() {
    let dir = tempfile::TempDir::new().unwrap();
    let (engine, model) = Box::pin(faux_engine(dir.path())).await;
    let engine = Arc::new(engine);
    engine
        .session
        .prompt("seed", pa_core::session_engine::PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    // The next run fails at its start: its prompt wait returns the error.
    let _failing = engine
        .session
        .agent()
        .subscribe(|event, _signal| {
            Box::pin(async move {
                if matches!(event, pa_agent::types::AgentEvent::AgentStart) {
                    anyhow::bail!("listener failed");
                }
                Ok(())
            })
        })
        .await;
    let options = crate::mode::RunOptions {
        app_mode: crate::mode::AppMode::Print,
        config: crate::mode::RuntimeConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            ..Default::default()
        },
        session: crate::mode::SessionOptions::default(),
        messages: vec!["/refine".to_string(), "next".to_string()],
        file_args: Vec::new(),
        daemon_socket: None,
        list_models: None,
        initial_message: None,
        initial_images: Vec::new(),
        verbose: false,
        offline: true,
        agents_view_requested: false,
        attach_agent: None,
    };
    let root_node = crate::print_rlm::root_node(
        &options,
        &pa_core::session::manager::SessionManager::in_memory(dir.path()),
        None,
    );
    let error = Box::pin(super::run_prompts_and_emit(
        &engine, &model, None, &options, &root_node,
    ))
    .await
    .expect_err("the failed prompt fails the run");
    assert!(error.contains("listener failed"), "{error}");
    let outcomes = engine
        .session
        .entries()
        .await
        .into_iter()
        .filter(|entry| {
            matches!(
                entry,
                pa_types::session::FileEntry::CustomMessage { payload, .. }
                    if payload.custom_type == "refinement_outcome"
            )
        })
        .count();
    assert_eq!(
        outcomes, 1,
        "the /refine outcome row landed before the run returned"
    );
}
