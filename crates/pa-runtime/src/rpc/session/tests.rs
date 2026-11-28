//! A same-path reopen replaces the engine: the replaced session's kernel ends with
//! everything it started, and the reopened one starts clean. An in-memory fork rebuilds in
//! place: a refinement planned on the abandoned branch must not commit onto it.
#![expect(
    clippy::large_futures,
    reason = "the replacement beds hold whole engines on the stack; boxing every call site costs clarity"
)]

use std::path::Path;
use std::sync::Arc;

use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::PromptOptions;

use super::{RpcEngineFactory, RpcEngineHandle, RpcEngineRequest, RpcSession};
use crate::rpc::LineWriter;

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "reopen-test".into(),
        name: "Reopen test".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 100_000,
        max_tokens: 1_000,
        max_tokens_explicit: false,
    }
}

/// An engine over `manager` whose model is `provider`.
async fn engine_handle(
    cwd: &Path,
    agent_dir: &Path,
    manager: SessionManager,
    provider: &Arc<pa_agent::scripted::ScriptedProvider>,
) -> RpcEngineHandle {
    let model = model();
    let engine = create_session(SessionEngineConfig {
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        model: Some(model.clone()),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(manager),
        daemonless: true,
        ..Default::default()
    })
    .await
    .expect("the engine builds");
    RpcEngineHandle {
        engine: Arc::new(engine),
        model: pa_types::ai::Model {
            id: model.id,
            name: model.name,
            api: model.api,
            provider: model.provider,
            base_url: model.base_url,
            reasoning: false,
            thinking_level_map: None,
            input: vec![pa_types::ai::ModelInput::Text],
            cost: pa_types::ai::ModelCost {
                input: pa_types::JsNumber::from(0.0),
                output: pa_types::JsNumber::from(0.0),
                cache_read: pa_types::JsNumber::from(0.0),
                cache_write: pa_types::JsNumber::from(0.0),
            },
            context_window: 100_000,
            max_tokens: 1_000,
            max_tokens_explicit: false,
            featured: None,
            serving: None,
            headers: None,
            compat: None,
        },
        api_key: None,
        provider_target: Arc::default(),
        bash_wake: Arc::default(),
        session_lease: None,
    }
}

/// Run one turn in which the model evaluates `code`; the text of the cell's result.
async fn run_cell(
    handle: &RpcEngineHandle,
    provider: &pa_agent::scripted::ScriptedProvider,
    code: &str,
) -> String {
    provider.push_tool_call_turn(
        None,
        vec![("cell", "eval", serde_json::json!({ "code": code }))],
    );
    provider.push_text_turn("done");
    handle
        .engine
        .prompt("go", PromptOptions::default())
        .await
        .expect("the turn is admitted");
    handle.engine.session.agent().wait_for_idle().await;
    provider
        .calls()
        .last()
        .and_then(|call| call.messages.last().cloned())
        .and_then(|message| match message {
            pa_agent::types::Message::ToolResult(result) => Some(
                result
                    .content
                    .iter()
                    .map(|block| match block {
                        pa_agent::types::ToolResultContent::Text(text) => text.text.as_str(),
                        pa_agent::types::ToolResultContent::Image(_) => "",
                    })
                    .collect(),
            ),
            pa_agent::types::Message::User(_) | pa_agent::types::Message::Assistant(_) => None,
        })
        .expect("the model's last request ends with the cell's result")
}

/// Reopening the session file an RPC process already serves replaces the
/// engine: the replaced session's kernel ends with the command its cell left
/// running, and the reopened session starts on a clean kernel.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_same_path_reopen_ends_the_replaced_kernel_and_starts_a_clean_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().join("project");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions").join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();

    let mut manager = SessionManager::persisted(&cwd, &session_dir);
    manager.flush_now().expect("the session file exists");
    let session_path = manager
        .get_session_file()
        .expect("a persisted session has a file")
        .to_path_buf();
    let old_model = Arc::new(pa_agent::scripted::ScriptedProvider::new(model()));
    let mut old = engine_handle(&cwd, &agent_dir, manager, &old_model).await;
    old.session_lease = Some(
        pa_core::session::lease::acquire_runtime_session_lease(&session_path, &agent_dir)
            .expect("the session lease"),
    );
    let pid: u32 = run_cell(
        &old,
        &old_model,
        "(define kept 7) (display (hash-ref (bash \"exec sleep 600\" (hash 'background #t)) 'pid))",
    )
    .await
    .trim()
    .parse()
    .expect("the cell printed its command's pid");

    let new_model = Arc::new(pa_agent::scripted::ScriptedProvider::new(model()));
    let factory: RpcEngineFactory = Arc::new({
        let (cwd, agent_dir, session_dir) = (cwd.clone(), agent_dir.clone(), session_dir.clone());
        let new_model = Arc::clone(&new_model);
        move |request| {
            let (cwd, agent_dir, session_dir) =
                (cwd.clone(), agent_dir.clone(), session_dir.clone());
            let new_model = Arc::clone(&new_model);
            Box::pin(async move {
                let RpcEngineRequest::Open { session_path, .. } = request else {
                    return Err("only the reopen is expected".to_string());
                };
                let manager = SessionManager::open(&cwd, &session_dir, &session_path);
                Ok(engine_handle(&cwd, &agent_dir, manager, &new_model).await)
            })
        }
    });
    let session = RpcSession::adopt(old, Some(factory), LineWriter::spawn()).await;
    session
        .replace_locked(RpcEngineRequest::Open {
            session_path,
            reuse_lease: false,
        })
        .await
        .expect("the same-path reopen succeeds");
    let ended = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        pa_core::platform::process::wait_for_exit(pid),
    )
    .await
    .is_ok_and(|waited| waited.is_ok());
    let reopened = session.handle().await;
    let kept = run_cell(&reopened, &new_model, "(+ kept 1)").await;
    assert_eq!(
        (ended, kept.contains("kept"), kept.trim() == "8"),
        (true, true, false),
        "{kept}"
    );
    reopened.engine.dispose_kernel().await;
}

const HELD_PLANNER_API: &str = "test-held-rpc-fork-planner";

/// A planning provider whose request parks until released.
struct HeldPlanner {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl pa_ai::registry::Provider for HeldPlanner {
    fn api(&self) -> &str {
        HELD_PLANNER_API
    }

    fn stream(
        &self,
        _model: &pa_types::ai::Model,
        _context: &pa_ai::types::Context,
        _options: Option<&pa_ai::types::StreamOptions>,
    ) -> pa_ai::AssistantMessageEventStream {
        let (writer, stream) = pa_ai::AssistantMessageEventStream::new();
        let (started, release) = (self.started.clone(), self.release.clone());
        tokio::spawn(async move {
            started.notify_one();
            release.notified().await;
            let message = pa_ai::faux::faux_assistant_text_message(
                r#"{"summary":"planned on the abandoned branch","edits":[]}"#,
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            );
            writer.push(pa_types::ai::AssistantMessageEvent::Done {
                reason: pa_types::ai::DoneStopReason::Stop,
                message: message.clone(),
            });
            writer.end(Some(message));
        });
        stream
    }

    fn stream_simple(
        &self,
        model: &pa_types::ai::Model,
        context: &pa_ai::types::Context,
        _options: Option<&pa_ai::types::SimpleStreamOptions>,
    ) -> pa_ai::AssistantMessageEventStream {
        self.stream(model, context, None)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_in_memory_fork_cancels_a_refinement_planned_on_the_abandoned_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().join("project");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    pa_ai::registry::register_api_provider(
        Arc::new(HeldPlanner {
            started: started.clone(),
            release: release.clone(),
        }),
        Some(HELD_PLANNER_API),
    );
    let mut manager = SessionManager::in_memory(&cwd);
    let forked_entry = manager
        .append_message(pa_types::session::AgentMessage::User(
            pa_types::ai::UserMessage {
                content: pa_types::ai::UserContent::Text("the abandoned branch".to_string()),
                timestamp: 0,
                rest: serde_json::Map::default(),
            },
        ))
        .unwrap();
    let mut handle = engine_handle(
        &cwd,
        &agent_dir,
        manager,
        &Arc::new(pa_agent::scripted::ScriptedProvider::new(model())),
    )
    .await;
    handle.model.api = HELD_PLANNER_API.to_string();
    handle.model.context_window = 128_000;
    let engine = Arc::clone(&handle.engine);
    let writer = LineWriter::spawn();
    let state = Arc::new(crate::rpc::commands::RpcState {
        session: Arc::new(RpcSession::adopt(handle, None, writer.clone()).await),
        writer,
        agent_dir,
        compacting: Arc::default(),
        autonomous: Arc::new(tokio::sync::Mutex::new(
            pa_core::autonomous::create_autonomous_runtime_state(None, None),
        )),
        last_goal: Arc::new(tokio::sync::Mutex::new(engine.goal_state().await)),
        queue_pump: Arc::default(),
        pump_suspended: Arc::default(),
        model_ops: Arc::default(),
        session_ops: Arc::default(),
    });
    let command = |command: &str, payload: serde_json::Value| crate::rpc::protocol::RpcCommand {
        id: None,
        command: command.to_string(),
        payload,
    };
    let mut refine = tokio::spawn({
        let state = Arc::clone(&state);
        let refine = command("refine", serde_json::json!({ "global": true }));
        async move { crate::rpc::commands::handle_command(&state, refine).await }
    });
    tokio::select! {
        () = started.notified() => {}
        response = &mut refine => panic!("refine settled before planning: {response:?}"),
    }
    let fork = crate::rpc::commands::handle_command(
        &state,
        command("fork", serde_json::json!({ "entryId": forked_entry })),
    )
    .await;
    assert_eq!(fork["success"], true, "{fork}");
    release.notify_one();
    let refined = refine.await.unwrap();
    assert_eq!(refined["success"], false, "{refined}");
    let persistence = engine.session.shared_persistence();
    assert!(!persistence.lock().await.get_all_entries().iter().any(|entry| matches!(
        entry,
        pa_types::session::FileEntry::CustomMessage { payload, .. }
            if payload.custom_type == pa_core::session_engine::refine::REFINEMENT_OUTCOME_CUSTOM_TYPE
    )));
    pa_ai::registry::unregister_api_providers(HELD_PLANNER_API);
}
