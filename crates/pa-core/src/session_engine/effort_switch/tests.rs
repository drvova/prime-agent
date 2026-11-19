use std::sync::{Arc, Mutex};

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::types::ThinkingLevel;
use serde_json::{json, Value};

use super::{register_effort_handler, EffortGate, MODEL_APPLY_EFFORT_REQUEST_TYPE};
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::session::manager::SessionManager;
use crate::session_engine::turn_boundary::{ModelInfo, TurnBoundaryRequests, TurnBoundaryRuntime};

fn bound(root: &std::path::Path) -> (Arc<TurnBoundaryRequests>, Arc<Agent>) {
    let requests = Arc::new(TurnBoundaryRequests::new());
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("s".to_string()),
            model: None,
            thinking_level: Some(ThinkingLevel::Low),
            tools: None,
            messages: None,
        },
        stream_fn: None,
        ..Default::default()
    }));
    requests.bind(TurnBoundaryRuntime {
        agent: Arc::clone(&agent),
        session: Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(root))),
        context_window: None,
        model_info: ModelInfo {
            id: "faux-1".to_string(),
            provider: "faux".to_string(),
            input: Vec::new(),
        },
    });
    (requests, agent)
}

fn gate(root: &std::path::Path, allowed: Option<bool>) -> EffortGate {
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    if let Some(allowed) = allowed {
        std::fs::write(
            agent_dir.join("settings.json"),
            json!({"replCanChangeEffort": allowed}).to_string(),
        )
        .unwrap();
    }
    EffortGate {
        cwd: root.to_path_buf(),
        agent_dir,
    }
}

async fn set_effort(handlers: &HostRequestHandlers, data: Value) -> anyhow::Result<Value> {
    let handler = handlers.get("model.set_effort").unwrap().clone();
    handler(HostRequestPayload { data }).await
}

/// Off by default (and when only a project's settings try to turn it on): the switch fails
/// saying how to enable it, changes nothing, and counts as refused.
#[tokio::test]
async fn the_switch_is_refused_until_the_user_enables_it() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join(".prime").join("agent");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("settings.json"),
        r#"{"replCanChangeEffort": true}"#,
    )
    .unwrap();
    let (requests, agent) = bound(root.path());
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = HostRequestHandlers::default();
    register_effort_handler(
        &mut handlers,
        &requests,
        gate(root.path(), None),
        Some({
            let outcomes = Arc::clone(&outcomes);
            Arc::new(move |applied| outcomes.lock().unwrap().push(applied))
        }),
    );
    let error = set_effort(&handlers, json!({"level": "high"}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("\"replCanChangeEffort\": true"),
        "{error}"
    );
    assert_eq!(agent.state().await.thinking_level, ThinkingLevel::Low);
    assert_eq!(*outcomes.lock().unwrap(), vec![false]);
}

/// Enabled: the switch sets the agent's level; an embedding's `model.apply_effort` handler takes
/// the validated level instead.
#[tokio::test]
async fn an_enabled_switch_applies_the_level_or_hands_it_to_the_embedding() {
    let root = tempfile::tempdir().unwrap();
    let (requests, agent) = bound(root.path());
    let mut handlers = HostRequestHandlers::default();
    register_effort_handler(
        &mut handlers,
        &requests,
        gate(root.path(), Some(true)),
        None,
    );
    assert_eq!(
        set_effort(&handlers, json!({"level": "high"}))
            .await
            .unwrap(),
        json!({"thinkingLevel": "high"})
    );
    assert_eq!(agent.state().await.thinking_level, ThinkingLevel::High);
    let bad = set_effort(&handlers, json!({"level": "turbo"}))
        .await
        .unwrap_err();
    assert!(bad.to_string().contains("level must be one of"), "{bad}");

    let applied = Arc::new(Mutex::new(Vec::new()));
    let mut worker = HostRequestHandlers::default();
    worker.register(MODEL_APPLY_EFFORT_REQUEST_TYPE, {
        let applied = Arc::clone(&applied);
        host_handler(move |payload| {
            let applied = Arc::clone(&applied);
            async move {
                applied.lock().unwrap().push(payload.data);
                Ok(json!({"thinkingLevel": "medium"}))
            }
        })
    });
    register_effort_handler(&mut worker, &requests, gate(root.path(), Some(true)), None);
    assert_eq!(
        set_effort(&worker, json!({"level": "max"})).await.unwrap(),
        json!({"thinkingLevel": "medium"})
    );
    assert_eq!(*applied.lock().unwrap(), vec![json!({"level": "max"})]);
    assert_eq!(agent.state().await.thinking_level, ThinkingLevel::High);
}
