use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::types::ThinkingLevel;
use serde_json::{json, Value};

use super::{register_settings_handlers, ArmedSettings, HostFallback, SETTINGS_HOST_REQUEST_TYPE};
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::session::manager::SessionManager;
use crate::session_engine::turn_boundary::{ModelInfo, TurnBoundaryRequests, TurnBoundaryRuntime};

fn armed() -> ArmedSettings {
    ArmedSettings {
        cell_timeout_ms: Some(60_000),
        compaction_enabled: true,
        compaction_threshold_percent: 80.0,
        compaction_keep_recent_tokens: 20_000,
        compaction_agent_callable: true,
        image_model: Some("openai/gpt-image-1".to_string()),
        web_search_provider: "auto".to_string(),
        cache_retention: json!("short"),
        rlm_depth: 0,
        daemonless: false,
    }
}

fn fallback(root: &std::path::Path) -> HostFallback {
    HostFallback {
        session_file: None,
        cwd: root.to_path_buf(),
        agent_dir: root.to_path_buf(),
    }
}

async fn call(handlers: &HostRequestHandlers, request_type: &str) -> Value {
    let handler = handlers.get(request_type).unwrap().clone();
    handler(HostRequestPayload { data: json!({}) })
        .await
        .unwrap()
}

fn bound_requests(thinking: ThinkingLevel) -> Arc<TurnBoundaryRequests> {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: Some(pa_agent::types::Model {
                id: "faux-1".to_string(),
                name: "Faux".to_string(),
                api: "test".to_string(),
                provider: "faux".to_string(),
                base_url: "http://localhost".to_string(),
                reasoning: true,
                cost: pa_agent::types::UsageCost::default(),
                context_window: 200_000,
                max_tokens: 1_000,
                max_tokens_explicit: false,
            }),
            thinking_level: Some(thinking),
            tools: None,
            messages: None,
        },
        stream_fn: None,
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent,
        session: Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            std::path::Path::new("/tmp"),
        ))),
        context_window: Some(200_000),
        model_info: ModelInfo {
            id: "faux-1".to_string(),
            provider: "faux".to_string(),
            input: Vec::new(),
        },
    });
    requests
}

/// `settings.session` answers the live model facts (thinking level, context window) and the
/// armed limits, field by field, and counts the read.
#[tokio::test]
async fn settings_session_reports_the_live_model_and_the_armed_limits() {
    let root = tempfile::tempdir().unwrap();
    let requests = bound_requests(ThinkingLevel::High);
    let reads = Arc::new(AtomicUsize::new(0));
    let mut handlers = HostRequestHandlers::default();
    register_settings_handlers(
        &mut handlers,
        &requests,
        armed(),
        fallback(root.path()),
        Some({
            let reads = Arc::clone(&reads);
            Arc::new(move || {
                reads.fetch_add(1, Ordering::SeqCst);
            })
        }),
    );
    assert_eq!(
        call(&handlers, "settings.session").await,
        json!({
            "model": {
                "id": "faux-1",
                "provider": "faux",
                "contextWindow": 200_000,
                "thinkingLevel": "high",
            },
            "cellTimeoutMs": 60_000,
            "compaction": {
                "enabled": true,
                "thresholdPercent": 80.0,
                "keepRecentTokens": 20_000,
                "agentCallable": true,
            },
            "imageModel": "openai/gpt-image-1",
            "webSearchProvider": "auto",
            "cacheRetention": "short",
            "rlmDepth": 0,
            "daemonless": false,
        })
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

/// Without a daemon the host half resolves the depth bound from settings and has no autonomous
/// runner; a handler the embedding registered first (the daemon worker's) is kept.
#[tokio::test]
async fn the_host_fallback_never_replaces_the_embedding_handler() {
    let root = tempfile::tempdir().unwrap();
    let requests = bound_requests(ThinkingLevel::Off);
    let mut headless = HostRequestHandlers::default();
    register_settings_handlers(
        &mut headless,
        &requests,
        armed(),
        fallback(root.path()),
        None,
    );
    let reply = call(&headless, SETTINGS_HOST_REQUEST_TYPE).await;
    assert_eq!(reply["autonomous"], Value::Null);
    assert_eq!(
        reply["rlmMaxDepth"]["maxDepth"],
        json!(crate::session_engine::rlm_host::DEFAULT_RLM_MAX_DEPTH)
    );

    let mut worker = HostRequestHandlers::default();
    worker.register(
        SETTINGS_HOST_REQUEST_TYPE,
        host_handler(|_payload| async { Ok(json!({"worker": true})) }),
    );
    register_settings_handlers(&mut worker, &requests, armed(), fallback(root.path()), None);
    assert_eq!(
        call(&worker, SETTINGS_HOST_REQUEST_TYPE).await,
        json!({"worker": true})
    );
}
