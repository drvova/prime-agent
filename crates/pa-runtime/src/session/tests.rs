//! Sessions running tasks as real turns, on engines built over pa-ai's
//! faux provider, through the whole runtime: input event, task, turn,
//! stream, `done`, transcript.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use pa_core::session::manager::SessionManager;
use pa_types::event_log::{AgentDone, AgentInput, NewEvent, Priority, TaskState, Topic, Trust};
use serde_json::{json, Value};

use super::{BuiltSession, SessionBuilder, SessionSpec};
use crate::bus::{Heard, Listener};
use crate::runtime::{Runtime, RuntimeOptions};

/// The prompt whose turn panics inside the model call.
const PANIC_PROMPT: &str = "panic now";

/// The prompt whose model call waits for a permit of the builder's `hold`.
const HOLD_PROMPT: &str = "hold on";

/// Builds every session over one faux provider of the test's own, with its
/// transcript under `<cwd>/sessions`.
struct FauxBuilder {
    model: pa_types::ai::Model,
    agent_dir: PathBuf,
    hold: Arc<tokio::sync::Semaphore>,
}

impl SessionBuilder for FauxBuilder {
    async fn build(&self, id: &str, spec: &SessionSpec) -> anyhow::Result<BuiltSession> {
        let sessions = spec.cwd.join("sessions");
        let inner =
            pa_core::session_engine::provider_adapter::real_stream_fn(None, self.model.clone());
        let hold = Arc::clone(&self.hold);
        let stream_fn: pa_agent::stream::StreamFn = Arc::new(move |model, context, options| {
            let messages = serde_json::to_string(&context.messages).unwrap_or_default();
            assert!(!messages.contains(PANIC_PROMPT), "the scripted turn panics");
            let gate = messages.contains(HOLD_PROMPT).then(|| Arc::clone(&hold));
            let inner = Arc::clone(&inner);
            Box::pin(async move {
                if let Some(gate) = gate {
                    let _permit = gate.acquire().await?;
                }
                inner(model, context, options).await
            })
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
            engine,
            model: self.model.clone(),
            api_key: None,
            agent_dir: self.agent_dir.clone(),
        })
    }
}

/// A runtime over its own agent dir, with one open session whose faux model
/// answers `responses` in order.
struct Bed {
    runtime: Runtime<FauxBuilder>,
    listener: Listener,
    session: String,
    cwd: PathBuf,
    agent_dir: PathBuf,
    model: pa_types::ai::Model,
    hold: Arc<tokio::sync::Semaphore>,
    _dir: tempfile::TempDir,
}

/// A runtime over `agent_dir` whose sessions run on `model`.
async fn open_runtime(
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
    model: pa_types::ai::Model,
    hold: &Arc<tokio::sync::Semaphore>,
) -> Runtime<FauxBuilder> {
    Runtime::open(
        agent_dir,
        RuntimeOptions {
            builder: FauxBuilder {
                model,
                agent_dir: agent_dir.to_path_buf(),
                hold: Arc::clone(hold),
            },
            spare_kernels: 1,
            max_concurrent_runs: NonZeroUsize::new(4).unwrap(),
            listener_capacity: NonZeroUsize::new(4096).unwrap(),
            cwd: cwd.to_path_buf(),
        },
    )
    .await
    .unwrap()
}

async fn bed(responses: Value) -> Bed {
    let dir = tempfile::TempDir::new().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "compaction": { "enabled": false } }).to_string(),
    )
    .unwrap();
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
    let model = registration.get_model();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let runtime = open_runtime(&agent_dir, &cwd, model.clone(), &hold).await;
    let listener = runtime.listen();
    let session = runtime
        .create_session(SessionSpec {
            cwd: cwd.clone(),
            ..SessionSpec::default()
        })
        .await
        .unwrap();
    Bed {
        runtime,
        listener,
        session,
        cwd,
        agent_dir,
        model,
        hold,
        _dir: dir,
    }
}

impl Bed {
    /// Publishes `text` as the session's input, at `priority`.
    async fn input(&self, text: &str, priority: Option<Priority>) {
        self.runtime
            .publish(NewEvent {
                topic: Topic::new(format!("agent/{}/input", self.session)).unwrap(),
                source: "test".to_string(),
                session: Some(self.session.clone()),
                payload: serde_json::to_value(AgentInput {
                    text: text.to_string(),
                    priority,
                })
                .unwrap(),
                trust: Trust::Owner,
                cause: None,
                reply: None,
            })
            .await
            .unwrap();
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

fn outcome(done: &AgentDone) -> (TaskState, Option<String>) {
    (done.outcome, done.error.clone())
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
    assert_eq!(outcome(&done), (TaskState::Done, None));
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
    assert_eq!((outcome(&done), other_dones), ((TaskState::Done, None), 0));
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
    assert_eq!(outcome(&done), (TaskState::Cancelled, None));
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
                Some("the turn panicked: the scripted turn panics".to_string())
            ),
            (TaskState::Done, None),
        ]
    );
    bed.runtime.shutdown().await;
}

/// A second runtime on the same database (a second headless process)
/// restores only the sessions it opens: the first runtime's running task
/// stays Running through the second's open, and ends Done.
#[tokio::test]
async fn a_second_runtime_on_the_database_leaves_other_sessions_tasks_alone() {
    let mut bed = bed(json!([{ "text": "held reply" }])).await;
    bed.input(HOLD_PROMPT, None).await;
    bed.stream_event("turn_start").await;
    let second = open_runtime(&bed.agent_dir, &bed.cwd, bed.model.clone(), &bed.hold).await;
    second
        .create_session(SessionSpec {
            cwd: bed.cwd.clone(),
            ..SessionSpec::default()
        })
        .await
        .unwrap();
    let log = pa_log::Log::open(
        pa_core::session::store::database_path(&bed.agent_dir),
        pa_log::DEFAULT_EVENT_RETENTION,
    )
    .unwrap();
    let stored: Vec<(TaskState, String, Option<String>)> = log
        .unfinished_tasks(&bed.session)
        .unwrap()
        .into_iter()
        .map(|task| (task.state, task.prompt, task.error))
        .collect();
    bed.hold.add_permits(1);
    let (_, done) = bed.until_done().await;
    assert_eq!(
        (stored, outcome(&done)),
        (
            vec![(TaskState::Running, HOLD_PROMPT.to_string(), None)],
            (TaskState::Done, None)
        )
    );
    second.shutdown().await;
    bed.runtime.shutdown().await;
}
