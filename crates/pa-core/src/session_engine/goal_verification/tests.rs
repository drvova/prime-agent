use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

use super::*;
use crate::autonomous::{ChildProcessResult, GateRunFuture, SnapshotFuture};
use crate::goals::GoalHostResponse;
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::session::manager::SessionManager;
use crate::session_engine::host_requests::SessionBinding;
use crate::session_engine::runtime::SessionRuntime;

/// A runner whose check exits with the scripted status over a fixed workspace snapshot, counting
/// the runs that actually happen.
struct ScriptedCheck {
    status: StdMutex<i32>,
    snapshot: StdMutex<String>,
    runs: AtomicUsize,
}

impl ScriptedCheck {
    fn new(status: i32) -> Self {
        Self {
            status: StdMutex::new(status),
            snapshot: StdMutex::new("tree-1".to_string()),
            runs: AtomicUsize::new(0),
        }
    }
}

impl GateCommandRunner for ScriptedCheck {
    fn run_gate(&self, _command: &str, _timeout_ms: u64) -> GateRunFuture<'_> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let status = *self.status.lock().unwrap();
        Box::pin(async move {
            Ok(ChildProcessResult {
                status: Some(status),
                stdout: "running 3 tests\ntest parse ... FAILED\ntest result: FAILED. 2 passed; 1 failed".to_string(),
                ..ChildProcessResult::default()
            })
        })
    }

    fn capture_snapshot(&self) -> SnapshotFuture<'_> {
        let status = self.snapshot.lock().unwrap().clone();
        Box::pin(async move {
            Some(GitWorktreeSnapshot {
                status,
                diff: String::new(),
                untracked_hash: String::new(),
            })
        })
    }
}

fn claim() -> GoalClaim {
    GoalClaim {
        goal_id: "g1".to_string(),
        objective: "fix the parser".to_string(),
        check: Some("cargo test -q".to_string()),
        verify: None,
    }
}

/// A failing check yields the command, exit, and output tail; over an unchanged workspace the
/// next claim is answered without rerunning it; a changed workspace reruns it and a pass passes.
#[tokio::test]
async fn a_failing_check_is_not_rerun_over_an_unchanged_snapshot() {
    let runner = Arc::new(ScriptedCheck::new(101));
    let verifier = GoalClaimVerifier::new(runner.clone(), GoalTelemetrySlot::default());

    let failed = verifier.verify(&claim()).await;
    assert_eq!(
        failed,
        GoalVerificationReport {
            passed: false,
            stage: GoalVerificationStage::Check,
            gap: Some("check `cargo test -q` exited with code 101; output tail:\nrunning 3 tests\ntest parse ... FAILED\ntest result: FAILED. 2 passed; 1 failed".to_string()),
        }
    );
    let skipped = verifier.verify(&claim()).await;
    assert_eq!(
        (runner.runs.load(Ordering::SeqCst), skipped),
        (
            1,
            GoalVerificationReport {
                passed: false,
                stage: GoalVerificationStage::Check,
                gap: Some(format!(
                    "check `cargo test -q` {GATE_NOT_RERUN_EXIT_TEXT}. {GOAL_CHECK_UNCHANGED_NOTE}"
                )),
            }
        )
    );

    *runner.snapshot.lock().unwrap() = "tree-2".to_string();
    *runner.status.lock().unwrap() = 0;
    let passed = verifier.verify(&claim()).await;
    assert_eq!(
        (runner.runs.load(Ordering::SeqCst), passed),
        (
            2,
            GoalVerificationReport {
                passed: true,
                stage: GoalVerificationStage::Check,
                gap: None,
            }
        )
    );
}

fn persisted_session() -> (tempfile::TempDir, SessionManager) {
    let dir = tempfile::TempDir::new().unwrap();
    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut session = SessionManager::in_memory(dir.path());
    session.materialize_session_file(Some(session_dir));
    (dir, session)
}

async fn call(handlers: &HostRequestHandlers, request_type: &str, data: Value) -> GoalHostResponse {
    let handler = handlers.get(request_type).unwrap().clone();
    let reply = handler(HostRequestPayload { data }).await.unwrap();
    serde_json::from_value(reply).unwrap()
}

use serde_json::{json, Value};

/// Through the kernel's host handlers: a claim whose check fails keeps the goal active and the
/// next continuation carries the gap (once); after the work lands, the claim completes the goal.
#[tokio::test]
async fn a_failing_check_keeps_the_goal_active_and_feeds_the_gap_forward() {
    let (_session_dir, session) = persisted_session();
    let workspace = tempfile::TempDir::new().unwrap();
    let runtime = SessionRuntime::new(
        &session,
        None,
        "live-1".to_string(),
        SessionBinding {
            session_id: "s1".to_string(),
            session_file: String::new(),
            cwd: workspace.path().display().to_string(),
        },
    );
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let mut handlers = HostRequestHandlers::default();
    runtime.register_host_handlers(Arc::clone(&session), &mut handlers);

    call(
        &handlers,
        "goal.create",
        json!({ "objective": "write done.txt", "check": "test -f done.txt" }),
    )
    .await;
    let refused = call(&handlers, "goal.complete", json!({})).await;
    let gap = "check `test -f done.txt` exited with code 1; output tail:\n(no output)";
    assert_eq!(
        (refused.goal.map(|goal| goal.status), refused.verification),
        (
            Some(GoalStatus::Active),
            Some(GoalVerificationReport {
                passed: false,
                stage: GoalVerificationStage::Check,
                gap: Some(gap.to_string()),
            })
        )
    );

    let continuation = {
        let mut driver = runtime.goal_driver().lock().await;
        let mut session = session.lock().await;
        let first = driver
            .next_continuation_message(&mut session, None)
            .unwrap()
            .unwrap();
        driver.continuation_consumed();
        let second = driver
            .next_continuation_message(&mut session, None)
            .unwrap()
            .unwrap();
        [first, second].map(|message| match message.content {
            pa_types::ai::UserContent::Text(text) => {
                text.contains(&format!("<verification_gap>\n{gap}\n</verification_gap>"))
            }
            pa_types::ai::UserContent::Blocks(_) => false,
        })
    };
    assert_eq!(
        continuation,
        [true, false],
        "the gap rides one continuation"
    );

    std::fs::write(workspace.path().join("done.txt"), "done").unwrap();
    let completed = call(&handlers, "goal.complete", json!({})).await;
    assert_eq!(
        (
            completed.goal.map(|goal| goal.status),
            completed.verification
        ),
        (
            Some(GoalStatus::Complete),
            Some(GoalVerificationReport {
                passed: true,
                stage: GoalVerificationStage::Check,
                gap: None,
            })
        )
    );
}

/// The scripted model of the measured run: it claims completion before doing the work, does the
/// work only when a claim is refused, then claims again and stops once a claim lands.
fn claim_first_model(
    provider: Arc<pa_agent::scripted::ScriptedProvider>,
) -> pa_agent::stream::StreamFn {
    let inner = provider.stream_fn();
    Arc::new(move |model, context, options| {
        let last_result = context
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                pa_agent::types::Message::ToolResult(result) => Some(result.clone()),
                pa_agent::types::Message::User(_) | pa_agent::types::Message::Assistant(_) => None,
            });
        match last_result {
            None => provider.push_tool_call_turn(None, vec![("c1", "claim_done", json!({}))]),
            Some(result) if result.tool_name == "write_done" => {
                provider.push_tool_call_turn(None, vec![("c3", "claim_done", json!({}))]);
            }
            Some(result)
                if serde_json::to_string(&result.content)
                    .unwrap()
                    .contains("\\\"passed\\\":false") =>
            {
                provider.push_tool_call_turn(None, vec![("c2", "write_done", json!({}))]);
            }
            Some(_) => provider.push_text_turn("The goal is complete."),
        }
        inner(model, context, options)
    })
}

/// What one scripted goal session ended with: the goal status, whether the work exists, and the
/// provider calls it took.
#[derive(Debug, PartialEq)]
struct MeasuredRun {
    status: GoalStatus,
    work_done: bool,
    provider_calls: usize,
}

/// One goal session over the real engine: the model's tools call the kernel's `goal.complete`
/// handler as a cell would. `check` `None` is the pre-change baseline.
async fn measured_goal_run(check: Option<&str>) -> MeasuredRun {
    use crate::session_engine::engine::{create_session, SessionEngineConfig};
    use crate::session_engine::tool_bridge::bridge_tool;
    use crate::tools::tool_definition::{ExecutionMode, ToolDefinition, ToolExecutionResult};

    let model: pa_agent::types::Model = serde_json::from_value(json!({
        "id": "m", "name": "m", "api": "test", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 100_000, "maxTokens": 1000
    }))
    .unwrap();
    let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(model.clone()));
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let handlers: Arc<std::sync::OnceLock<HostRequestHandlers>> = Arc::default();
    let claim_handlers = Arc::clone(&handlers);
    let claim_done = ToolDefinition {
        name: "claim_done".to_string(),
        label: "Claim done".to_string(),
        description: "Runs `(goal-complete)`".to_string(),
        prompt_snippet: String::new(),
        parameters: json!({ "type": "object", "properties": {} }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(move |_id, _params, _signal, _on_update| {
            let handlers = Arc::clone(&claim_handlers);
            Box::pin(async move {
                let handler = handlers
                    .get()
                    .unwrap()
                    .get("goal.complete")
                    .unwrap()
                    .clone();
                let reply = handler(HostRequestPayload { data: json!({}) }).await?;
                Ok(ToolExecutionResult::text(reply.to_string()))
            })
        }),
    };
    let done_path = cwd.join("done.txt");
    let write_path = done_path.clone();
    let write_done = ToolDefinition {
        name: "write_done".to_string(),
        label: "Write done".to_string(),
        description: "Does the work".to_string(),
        prompt_snippet: String::new(),
        parameters: json!({ "type": "object", "properties": {} }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(move |_id, _params, _signal, _on_update| {
            let path = write_path.clone();
            Box::pin(async move {
                std::fs::write(&path, "done")?;
                Ok(ToolExecutionResult::text("wrote done.txt"))
            })
        }),
    };
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
        agent_dir: tmp.path().join("agent"),
        mcp_manager: None,
        model: Some(model),
        thinking_level: None,
        stream_fn: Some(claim_first_model(Arc::clone(&provider))),
        tools: vec![bridge_tool(claim_done), bridge_tool(write_done)],
        custom_system_prompt: None,
        prompt_guidelines: vec![],
        generic_mcp_servers: vec![],
        allow_recursion: None,
        session_manager: None,
        extra_host_handlers: None,
        conversation_log_path: None,
        additional_skill_paths: vec![],
        additional_prompt_paths: vec![],
        extra_builtin_skill_overrides: vec![],
        rlm_subagent_host: None,
        rlm_usage_store: None,
        progress_note_emit: None,
        interaction: None,
        rlm_depth: None,
        factory_child: false,
        daemonless: true,
        telemetry: None,
        model_info: None,
        on_background_work_settled: None,
        bash_notice_wake: None,
        prewarm_ipython_kernel: None,
        queued_goal_context_purge: None,
    })
    .await
    .unwrap();
    let _ = handlers.set(engine.provisioner.host_handlers().clone());
    call(
        handlers.get().unwrap(),
        "goal.create",
        json!({ "objective": "write done.txt", "check": check }),
    )
    .await;
    engine
        .prompt(
            "work on the goal",
            crate::session_engine::PromptOptions::default(),
        )
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    let status = engine.goal_driver.lock().await.state().status;
    MeasuredRun {
        status,
        work_done: done_path.exists(),
        provider_calls: provider.calls().len(),
    }
}

/// The measured run: a model that falsely declares done once. Without a check the false claim
/// completes the goal with no work done (2 provider calls). With `check="test -f done.txt"` the
/// claim is refused with the gap, the model does the work, and the second claim completes the
/// goal correctly (4 provider calls).
#[tokio::test]
async fn a_false_completion_claim_is_refused_until_the_check_passes() {
    assert_eq!(
        measured_goal_run(None).await,
        MeasuredRun {
            status: GoalStatus::Complete,
            work_done: false,
            provider_calls: 2,
        }
    );
    assert_eq!(
        measured_goal_run(Some("test -f done.txt")).await,
        MeasuredRun {
            status: GoalStatus::Complete,
            work_done: true,
            provider_calls: 4,
        }
    );
}
