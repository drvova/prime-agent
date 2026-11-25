//! Sessions running tasks as real turns, on engines built over pa-ai's
//! faux provider, through the whole runtime: input event, task, turn,
//! stream, `done`, transcript.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use pa_core::session::manager::SessionManager;
use pa_types::event_log::{
    AgentDone, AgentInput, EventId, NewEvent, Priority, TaskFailure, TaskState, Topic, Trust,
};
use serde_json::{json, Value};

use super::{BuiltSession, DriverEvents, Finished, SessionBuilder, SessionSpec, SettleAfter};
use crate::bus::{Heard, Listener};
use crate::runtime::{Runtime, RuntimeOptions};

/// The prompt whose turn panics inside the model call.
const PANIC_PROMPT: &str = "panic now";

/// Builds every session over one faux provider of the test's own, with its
/// transcript under `<cwd>/sessions`.
struct FauxBuilder {
    model: pa_types::ai::Model,
    agent_dir: PathBuf,
}

impl SessionBuilder for FauxBuilder {
    async fn build(&self, id: &str, spec: &SessionSpec) -> anyhow::Result<BuiltSession> {
        let sessions = spec.cwd.join("sessions");
        let inner =
            pa_core::session_engine::provider_adapter::real_stream_fn(None, self.model.clone());
        let stream_fn: pa_agent::stream::StreamFn = Arc::new(move |model, context, options| {
            assert!(
                !serde_json::to_string(&context.messages)
                    .unwrap_or_default()
                    .contains(PANIC_PROMPT),
                "the scripted turn panics"
            );
            inner(model, context, options)
        });
        let engine = Box::pin(pa_core::session_engine::engine::create_session(
            pa_core::session_engine::engine::SessionEngineConfig {
                on_late_sent_agent_message: None,
                semantic_edges: None,
                cron_store: None,
                steering_mode: None,
                follow_up_mode: None,
                image_model_router: None,
                telemetry: None,
                cwd: spec.cwd.clone(),
                agent_dir: self.agent_dir.clone(),
                mcp_manager: None,
                model: pa_core::session_engine::provider_adapter::json_round_trip(&self.model),
                thinking_level: None,
                stream_fn: Some(stream_fn),
                tools: Vec::new(),
                custom_system_prompt: None,
                prompt_guidelines: Vec::new(),
                generic_mcp_servers: Vec::new(),
                allow_recursion: None,
                session_manager: Some(SessionManager::open(
                    &spec.cwd,
                    &sessions,
                    &pa_core::session::manager::get_session_file_path(&sessions, id),
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
                model_info: Some(self.model.clone()),
                prewarm_kernel: Some(true),
                on_background_work_settled: None,
                bash_notice_wake: None,
                queued_goal_context_purge: None,
                queued_steering_probe: None,
                should_stop_after_turn: None,
            },
        ))
        .await?;
        Ok(BuiltSession {
            engine: Arc::new(engine),
            model: self.model.clone(),
            api_key: None,
            agent_dir: self.agent_dir.clone(),
            events: DriverEvents::Stream,
            autonomous: None,
            subagents: None,
        })
    }
}

/// A runtime over its own agent dir, with one open session whose faux model
/// answers `responses` in order.
struct Bed<B: SessionBuilder = FauxBuilder> {
    runtime: Runtime<B>,
    listener: Listener,
    session: String,
    cwd: PathBuf,
    _dir: tempfile::TempDir,
}

async fn bed(responses: Value) -> Bed {
    bed_with(
        responses,
        json!({ "compaction": { "enabled": false } }),
        |faux| faux,
    )
    .await
}

/// A bed whose agent dir has `settings`, and whose sessions `wrap` builds
/// over the faux builder.
async fn bed_with<B: SessionBuilder>(
    responses: Value,
    settings: Value,
    wrap: impl FnOnce(FauxBuilder) -> B,
) -> Bed<B> {
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    let parsed =
        pa_ai::faux::script::parse_faux_script(&json!({ "responses": responses })).unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![parsed.model]),
            ..Default::default()
        });
    registration.set_responses(parsed.responses);
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = Runtime::open(
        &agent_dir,
        RuntimeOptions {
            builder: wrap(FauxBuilder {
                model: registration.get_model(),
                agent_dir: agent_dir.clone(),
            }),
            spare_kernels: 1,
            max_concurrent_runs: NonZeroUsize::new(4).unwrap(),
            listener_capacity: NonZeroUsize::new(4096).unwrap(),
            cwd: cwd.clone(),
            sessions: Vec::new(),
        },
    )
    .await
    .unwrap();
    let listener = runtime.listen();
    let session = runtime
        .create_session(SessionSpec {
            cwd: cwd.clone(),
            ..SessionSpec::default()
        })
        .unwrap();
    Bed {
        runtime,
        listener,
        session,
        cwd,
        _dir: dir,
    }
}

impl<B: SessionBuilder> Bed<B> {
    /// Publishes `text` as the session's input, at `priority`; the input
    /// event's id.
    async fn input(&self, text: &str, priority: Option<Priority>) -> EventId {
        self.runtime
            .publish(NewEvent {
                topic: Topic::new(format!("agent/{}/input", self.session)).unwrap(),
                source: "test".to_string(),
                session: Some(self.session.clone()),
                payload: serde_json::to_value(AgentInput {
                    text: text.to_string(),
                    images: Vec::new(),
                    priority,
                })
                .unwrap(),
                trust: Trust::Owner,
                cause: None,
                reply: None,
            })
            .await
            .unwrap()
            .event
            .id
    }

    /// The session's stream payloads up to its next `done`, and the `done`.
    async fn until_done(&mut self) -> (Vec<Value>, AgentDone) {
        let stream = format!("agent/{}/stream", self.session);
        let done = format!("agent/{}/done", self.session);
        let mut payloads = Vec::new();
        loop {
            let heard =
                tokio::time::timeout(std::time::Duration::from_secs(60), self.listener.recv())
                    .await
                    .expect("the session publishes")
                    .expect("the bus is open");
            let Heard::Event(event) = heard else {
                panic!("the listener lagged");
            };
            if event.event.topic.as_str() == stream {
                payloads.push(event.event.payload.clone());
            } else if event.event.topic.as_str() == done {
                return (
                    payloads,
                    serde_json::from_value(event.event.payload.clone()).unwrap(),
                );
            }
        }
    }

    /// Waits for the session's stream event of `kind`.
    async fn stream_event(&mut self, kind: &str) {
        let stream = format!("agent/{}/stream", self.session);
        loop {
            let heard =
                tokio::time::timeout(std::time::Duration::from_secs(60), self.listener.recv())
                    .await
                    .expect("the session publishes")
                    .expect("the bus is open");
            if let Heard::Event(event) = heard {
                if event.event.topic.as_str() == stream && event.event.payload["type"] == kind {
                    return;
                }
            }
        }
    }

    /// The (role, text) of each user and assistant message the session's
    /// transcript holds.
    fn transcript(&self) -> Vec<(String, String)> {
        let sessions = self.cwd.join("sessions");
        let manager = SessionManager::open(
            &self.cwd,
            &sessions,
            &pa_core::session::manager::get_session_file_path(&sessions, &self.session),
        );
        manager
            .active_context()
            .messages
            .iter()
            .filter_map(|message| {
                let value = serde_json::to_value(message).ok()?;
                let role = value["role"].as_str()?.to_string();
                if role != "user" && role != "assistant" {
                    return None;
                }
                let text = match &value["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>(),
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => {
                        String::new()
                    }
                };
                Some((role, text))
            })
            .collect()
    }
}

fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(role, text)| ((*role).to_string(), (*text).to_string()))
        .collect()
}

fn outcome(done: &AgentDone) -> (TaskState, Option<TaskFailure>, Option<String>) {
    (done.outcome, done.failure, done.error.clone())
}

#[tokio::test]
async fn an_input_runs_a_turn_that_streams_then_reports_done() {
    let mut bed = bed(json!([{ "text": "hello back" }])).await;
    bed.input("hello", None).await;
    let (stream, done) = bed.until_done().await;
    let kinds: Vec<&str> = stream
        .iter()
        .filter_map(|payload| payload["type"].as_str())
        .collect();
    // The harness digest's message pair, the prompt's, then the reply's.
    assert_eq!(
        kinds,
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "message_start",
            "message_update",
            "message_update",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
    assert_eq!(outcome(&done), (TaskState::Done, None, None));
    assert_eq!(
        bed.transcript(),
        pairs(&[("user", "hello"), ("assistant", "hello back")])
    );
    bed.runtime.shutdown().await;
}

#[tokio::test]
async fn a_steer_during_a_turn_is_read_at_its_next_step() {
    // The first reply is held long enough for the steer to arrive while the
    // turn runs.
    let mut bed = bed(json!([
        { "text": "first reply", "delayMs": 1000 },
        { "text": "steered reply" },
    ]))
    .await;
    bed.input("start", None).await;
    bed.stream_event("turn_start").await;
    bed.input("change course", Some(Priority::Steer)).await;
    let (_, done) = bed.until_done().await;
    bed.runtime.idle(&bed.session).await;
    // Taken by the running turn, the steer has no run of its own: its
    // session reports no other task. A task's `done` is published before
    // the lane goes idle, so any other is already heard.
    let done_topic = format!("agent/{}/done", bed.session);
    let mut other_dones = 0;
    while let Ok(Some(Heard::Event(event))) =
        tokio::time::timeout(std::time::Duration::ZERO, bed.listener.recv()).await
    {
        other_dones += usize::from(event.event.topic.as_str() == done_topic);
    }
    assert_eq!(
        (outcome(&done), other_dones),
        ((TaskState::Done, None, None), 0)
    );
    assert_eq!(
        bed.transcript(),
        pairs(&[
            ("user", "start"),
            ("assistant", "first reply"),
            ("user", "change course"),
            ("assistant", "steered reply"),
        ])
    );
    bed.runtime.shutdown().await;
}

#[tokio::test]
async fn cancelling_a_running_task_aborts_its_turn() {
    // Held far past the test: only the abort ends the turn.
    let mut bed = bed(json!([{ "text": "never sent", "delayMs": 600_000 }])).await;
    bed.input("wait", None).await;
    bed.stream_event("turn_start").await;
    bed.runtime.cancel(&bed.session).await.unwrap();
    let (_, done) = bed.until_done().await;
    assert_eq!(outcome(&done), (TaskState::Cancelled, None, None));
    bed.runtime.idle(&bed.session).await;
    bed.runtime.shutdown().await;
}

#[tokio::test]
async fn a_panicking_turn_fails_its_task_and_the_next_one_runs() {
    let mut bed = bed(json!([{ "text": "after the panic" }])).await;
    bed.input(PANIC_PROMPT, None).await;
    let (_, panicked) = bed.until_done().await;
    bed.input("go on", None).await;
    let (_, next) = bed.until_done().await;
    assert_eq!(
        [outcome(&panicked), outcome(&next)],
        [
            (
                TaskState::Failed,
                Some(TaskFailure::Turn),
                Some("the turn panicked: the scripted turn panics".to_string())
            ),
            (TaskState::Done, None, None),
        ]
    );
    bed.runtime.shutdown().await;
}

#[tokio::test]
async fn a_failed_model_reply_fails_its_task_as_a_reply() {
    // No response queued: the faux provider answers with an error stop.
    let mut bed = bed(json!([])).await;
    bed.input("hello", None).await;
    let (_, done) = bed.until_done().await;
    assert_eq!(
        (done.outcome, done.failure, done.error.is_some()),
        (TaskState::Failed, Some(TaskFailure::Reply), true)
    );
    bed.runtime.shutdown().await;
}

#[tokio::test]
async fn settling_reports_how_the_run_ended() {
    let mut bed = bed(json!([{ "text": "hello back" }])).await;
    bed.input("hello", None).await;
    bed.until_done().await;
    let settled = bed
        .runtime
        .settle(&bed.session, SettleAfter::Queued)
        .await
        .unwrap()
        .expect("nothing stopped the run");
    assert_eq!(
        (
            settled
                .terminal
                .primary
                .and_then(|primary| primary.stdout_text()),
            settled.terminal.compaction_outcomes,
            settled.goal_stopped,
            settled.autonomous_failure,
        ),
        (Some("hello back".to_string()), Vec::new(), None, None)
    );
    bed.runtime.shutdown().await;
}

/// Builds over the faux builder, runs a seed turn, then makes every later
/// run fail at its start.
struct FailingAfterSeed(FauxBuilder);

impl SessionBuilder for FailingAfterSeed {
    async fn build(&self, id: &str, spec: &SessionSpec) -> anyhow::Result<BuiltSession> {
        let built = self.0.build(id, spec).await?;
        built
            .engine
            .session
            .prompt("seed", pa_core::session_engine::PromptOptions::default())
            .await?;
        built.engine.session.agent().wait_for_idle().await;
        let _failing = built
            .engine
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
        Ok(built)
    }
}

/// A run's end drains what its turns left: a `/refine` still planning when a
/// later task failed lands its outcome before the finish answers.
#[tokio::test]
async fn finishing_after_a_failed_task_drains_an_in_flight_refine() {
    // The planner's reply is held past the failed task: only the drain waits
    // for it.
    let mut bed = bed_with(
        json!([
            { "text": "seed" },
            { "text": r#"{"summary":"background note","edits":[]}"#, "delayMs": 3000 },
        ]),
        json!({
            "compaction": { "enabled": false },
            "autoRefine": { "enabled": true, "compact": true, "cooldownMs": 0 },
        }),
        FailingAfterSeed,
    )
    .await;
    bed.input("/refine", None).await;
    let (_, refine) = bed.until_done().await;
    let next_input = bed.input("next", None).await;
    // Armed behind a task that stops the run, the settle has nothing to do.
    let settled = bed
        .runtime
        .settle(&bed.session, SettleAfter::Task(next_input));
    let (_, next) = bed.until_done().await;
    let settled = settled.await.unwrap();
    let finished = bed.runtime.finish(&bed.session).await.unwrap();
    // The finish answers once its events are on the bus: the listener holds
    // them all already.
    let stream = format!("agent/{}/stream", bed.session);
    let mut refined = false;
    while let Ok(Some(Heard::Event(event))) =
        tokio::time::timeout(std::time::Duration::ZERO, bed.listener.recv()).await
    {
        refined |= event.event.topic.as_str() == stream
            && event.event.payload["type"] == "refine_complete";
    }
    assert_eq!(
        (
            outcome(&refine),
            (next.outcome, next.failure),
            settled,
            finished,
            refined
        ),
        (
            (TaskState::Done, None, None),
            (TaskState::Failed, Some(TaskFailure::Turn)),
            None,
            Finished {
                refine_command_failed: false
            },
            true
        )
    );
    bed.runtime.shutdown().await;
}

/// Fails every build.
struct FailingBuild;

impl SessionBuilder for FailingBuild {
    fn build(
        &self,
        _id: &str,
        _spec: &SessionSpec,
    ) -> impl std::future::Future<Output = anyhow::Result<BuiltSession>> + Send {
        std::future::ready(Err(anyhow::anyhow!("no engine here")))
    }
}

#[tokio::test]
async fn a_task_whose_engine_cannot_be_built_reports_its_failure() {
    let mut bed = bed_with(json!([]), json!({}), |_| FailingBuild).await;
    bed.input("hello", None).await;
    let (_, done) = bed.until_done().await;
    assert_eq!(
        outcome(&done),
        (
            TaskState::Failed,
            Some(TaskFailure::Turn),
            Some("build the session: no engine here".to_string())
        )
    );
    bed.runtime.shutdown().await;
}
