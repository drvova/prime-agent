use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pa_types::ai::{AssistantMessage, StopReason, Usage, UsageCost};
use pa_types::session::AgentMessage;
use pa_types::JsNumber;
use serde_json::{json, Value};

use super::{register_usage_host_handlers, USAGE_QUOTAS_REQUEST_TYPE};
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::session::manager::SessionManager;

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost {
            total: JsNumber(cost),
            ..UsageCost::default()
        },
        billing: None,
    }
}

fn assistant(usage: Usage) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        content: vec![],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage,
        stop_reason: StopReason::ToolUse,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        rest: serde_json::Map::default(),
    })
}

async fn call(handlers: &HostRequestHandlers, request_type: &str) -> Value {
    let handler = handlers.get(request_type).unwrap().clone();
    handler(HostRequestPayload { data: json!({}) })
        .await
        .unwrap()
}

/// `usage.session` answers the branch's spend with the subagents' attributed share split out,
/// cache tokens included, and counts the read without seeing a number.
#[tokio::test]
async fn usage_session_reports_cost_with_the_subagent_share_and_cache_tokens() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut session = SessionManager::in_memory(dir.path());
    let first = session
        .append_message(assistant(usage(1_000, 200, 4_000, 500, 0.25)))
        .unwrap();
    session
        .append_message(assistant(usage(100, 50, 5_000, 0, 0.125)))
        .unwrap();
    // A subagent's spend folds into the assistant row that spawned it.
    session
        .append_child_usage_attribution(
            "attr-1",
            &first,
            usage(300, 30, 0, 0, 0.5),
            // The target's usage after the fold.
            usage(1_300, 230, 4_000, 500, 0.75),
            None,
        )
        .unwrap();
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let reads = Arc::new(AtomicU64::new(0));
    let mut handlers = HostRequestHandlers::default();
    register_usage_host_handlers(
        &mut handlers,
        &session,
        Some({
            let reads = Arc::clone(&reads);
            Arc::new(move || {
                reads.fetch_add(1, Ordering::SeqCst);
            })
        }),
    );

    assert_eq!(
        call(&handlers, "usage.session").await,
        json!({
            "cost": {"total": 0.875, "own": 0.375, "subagents": 0.5},
            "tokens": {
                "input": 1_400,
                "output": 280,
                "cacheRead": 9_000,
                "cacheWrite": 500,
                "total": 11_180,
            },
        })
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

/// Without a daemon the quota request answers why there are no cards; a quota handler the
/// embedding registered first (the daemon worker's) is kept.
#[tokio::test]
async fn the_quota_fallback_never_replaces_the_embedding_handler() {
    let dir = tempfile::TempDir::new().unwrap();
    let session = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
        dir.path(),
    )));
    let mut headless = HostRequestHandlers::default();
    register_usage_host_handlers(&mut headless, &session, None);
    assert_eq!(
        call(&headless, USAGE_QUOTAS_REQUEST_TYPE).await,
        json!({
            "providers": [],
            "unavailable": "provider quotas are read by the prime-agent daemon; this session runs without one",
        })
    );

    let mut worker = HostRequestHandlers::default();
    worker.register(
        USAGE_QUOTAS_REQUEST_TYPE,
        host_handler(|_payload| async { Ok(json!({"providers": ["worker"]})) }),
    );
    register_usage_host_handlers(&mut worker, &session, None);
    assert_eq!(
        call(&worker, USAGE_QUOTAS_REQUEST_TYPE).await,
        json!({"providers": ["worker"]})
    );
}
