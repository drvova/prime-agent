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

use super::{
    BuiltSession, DriverEvents, Finished, SessionBuilder, SessionSeams, SessionSpec, SettleAfter,
};
use crate::bus::{Heard, Listener};
use crate::runtime::{Runtime, RuntimeOptions};

/// The prompt whose turn panics inside the model call.
const PANIC_PROMPT: &str = "panic now";

/// The prompt whose model calls wait at the builder's [`Hold`].
const HOLD_PROMPT: &str = "hold on";

/// A host request every bed's sessions answer that the trust table does not
/// list.
const UNLISTED_REQUEST: &str = "bed.unlisted";

/// Where a model call of a turn holding [`HOLD_PROMPT`] waits: it says it
/// has started, then waits for one permit, which it gives back.
struct Hold {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

/// Builds every session over one faux provider of the test's own, with its
/// transcript under `<cwd>/sessions`.
struct FauxBuilder {
    model: pa_types::ai::Model,
    agent_dir: PathBuf,
    hold: Arc<Hold>,
}

impl SessionBuilder for FauxBuilder {
    async fn build(
        &self,
        id: &str,
        spec: &SessionSpec,
        seams: SessionSeams,
    ) -> anyhow::Result<BuiltSession> {
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
                    gate.entered.notify_one();
                    let _permit = gate.release.acquire().await?;
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
                extra_host_handlers: Some({
                    // The runtime's, then a request the trust table does not list.
                    let mut handlers = seams.host_handlers;
                    handlers.register(
                        UNLISTED_REQUEST,
                        pa_core::kernel::shared::host_handler(|_payload| async {
                            Ok(json!("answered"))
                        }),
                    );
                    handlers
                }),
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
                call_policy: Some(seams.call_policy),
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
    agent_dir: PathBuf,
    model: pa_types::ai::Model,
    hold: Arc<Hold>,
    _dir: tempfile::TempDir,
}

/// A runtime over `agent_dir` whose sessions `builder` builds.
async fn open_runtime<B: SessionBuilder>(
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
    builder: B,
) -> Runtime<B> {
    Runtime::open(
        agent_dir,
        RuntimeOptions {
            builder,
            spare_kernels: 1,
            max_concurrent_runs: NonZeroUsize::new(4).unwrap(),
            listener_capacity: NonZeroUsize::new(4096).unwrap(),
            cwd: cwd.to_path_buf(),
            sessions: Vec::new(),
        },
    )
    .await
    .unwrap()
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
    let model = registration.get_model();
    let hold = Arc::new(Hold {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let runtime = open_runtime(
        &agent_dir,
        &cwd,
        wrap(FauxBuilder {
            model: model.clone(),
            agent_dir: agent_dir.clone(),
            hold: Arc::clone(&hold),
        }),
    )
    .await;
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

impl<B: SessionBuilder> Bed<B> {
    /// Publishes `text` as the session's input, at `priority`; the input
    /// event's id.
    async fn input(&self, text: &str, priority: Option<Priority>) -> EventId {
        self.input_with(text, priority, Trust::Owner).await
    }

    /// Publishes `text` as the session's input, at `priority`, with
    /// `trust`; the input event's id.
    async fn input_with(&self, text: &str, priority: Option<Priority>, trust: Trust) -> EventId {
        self.input_to(&self.session, text, priority, trust).await
    }

    /// Publishes `text` as `session`'s input, at `priority`, with `trust`;
    /// the input event's id.
    async fn input_to(
        &self,
        session: &str,
        text: &str,
        priority: Option<Priority>,
        trust: Trust,
    ) -> EventId {
        self.runtime
            .publish(NewEvent {
                topic: Topic::new(format!("agent/{session}/input")).unwrap(),
                source: "test".to_string(),
                session: Some(session.to_string()),
                payload: serde_json::to_value(AgentInput {
                    text: text.to_string(),
                    images: Vec::new(),
                    priority,
                })
                .unwrap(),
                trust,
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
        let session = self.session.clone();
        self.until_done_of(&session).await
    }

    /// `session`'s stream payloads up to its next `done`, and the `done`;
    /// other sessions' events are passed over.
    async fn until_done_of(&mut self, session: &str) -> (Vec<Value>, AgentDone) {
        let stream = format!("agent/{session}/stream");
        let done = format!("agent/{session}/done");
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
        self.transcript_of(&self.session)
    }

    /// The (role, text) of each user and assistant message `session`'s
    /// transcript holds.
    fn transcript_of(&self, session: &str) -> Vec<(String, String)> {
        let sessions = self.cwd.join("sessions");
        let manager = SessionManager::open(
            &self.cwd,
            &sessions,
            &pa_core::session::manager::get_session_file_path(&sessions, session),
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
    // The first model call is held until the steer is queued: the turn is
    // past its first read of the steering queue.
    let mut bed = bed(json!([
        { "text": "first reply" },
        { "text": "steered reply" },
    ]))
    .await;
    bed.input(HOLD_PROMPT, None).await;
    bed.hold.entered.notified().await;
    bed.input("change course", Some(Priority::Steer)).await;
    bed.hold.release.add_permits(1);
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
            ("user", HOLD_PROMPT),
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
    async fn build(
        &self,
        id: &str,
        spec: &SessionSpec,
        seams: SessionSeams,
    ) -> anyhow::Result<BuiltSession> {
        let built = self.0.build(id, spec, seams).await?;
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
        _seams: SessionSeams,
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

/// A second runtime on the same database (a second headless process)
/// restores only the sessions it opens: the first runtime's running task
/// stays Running through the second's open, and ends Done.
#[tokio::test]
async fn a_second_runtime_on_the_database_leaves_other_sessions_tasks_alone() {
    let mut bed = bed(json!([{ "text": "held reply" }])).await;
    bed.input(HOLD_PROMPT, None).await;
    bed.stream_event("turn_start").await;
    let second = open_runtime(
        &bed.agent_dir,
        &bed.cwd,
        FauxBuilder {
            model: bed.model.clone(),
            agent_dir: bed.agent_dir.clone(),
            hold: Arc::clone(&bed.hold),
        },
    )
    .await;
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
    bed.hold.release.add_permits(1);
    let (_, done) = bed.until_done().await;
    assert_eq!(
        (stored, outcome(&done)),
        (
            vec![(TaskState::Running, HOLD_PROMPT.to_string(), None)],
            (TaskState::Done, None, None)
        )
    );
    second.shutdown().await;
    bed.runtime.shutdown().await;
}

/// A faux reply that runs `code` in one `eval` cell.
fn eval_cell(code: &str) -> Value {
    json!({ "content": [{ "type": "toolCall", "id": "cell", "name": "eval", "arguments": { "code": code } }] })
}

/// What each cell of a task's stream answered, as text.
fn cell_answers(stream: &[Value]) -> Vec<String> {
    stream
        .iter()
        .filter(|payload| payload["type"] == "tool_execution_end")
        .map(|payload| payload["result"].to_string())
        .collect()
}

/// An untrusted task's prompt reaches the model marked as outside content,
/// and its `bash` cell is refused before the command starts; the owner's
/// next task runs the same command.
#[tokio::test]
async fn an_untrusted_tasks_bash_is_refused_while_an_owners_runs() {
    let mut bed = bed(json!([
        eval_cell("(bash \"echo untrusted > untrusted-made\")"),
        { "text": "refused" },
        eval_cell("(bash \"echo owner > owner-made\")"),
        { "text": "ran" },
    ]))
    .await;
    bed.input_with("run it", None, Trust::Untrusted).await;
    let (untrusted, _) = bed.until_done().await;
    bed.input("run it", None).await;
    let (owner, _) = bed.until_done().await;
    let refused = cell_answers(&untrusted);
    let ran = cell_answers(&owner);
    assert_eq!(
        (
            bed.transcript().first().cloned(),
            refused.len(),
            refused[0].contains("refused for an untrusted turn: bash"),
            bed.cwd.join("untrusted-made").exists(),
            ran.iter().any(|answer| answer.contains("refused")),
            std::fs::read_to_string(bed.cwd.join("owner-made")).ok(),
        ),
        (
            Some((
                "user".to_string(),
                "<untrusted-event source=\"test\">run it</untrusted-event>".to_string()
            )),
            1,
            true,
            false,
            false,
            Some("owner\n".to_string()),
        ),
        "untrusted: {refused:?}, owner: {ran:?}"
    );
    bed.runtime.shutdown().await;
}

/// An untrusted task's cell writes no file.
#[tokio::test]
async fn an_untrusted_tasks_file_write_is_refused() {
    let mut bed = bed(json!([
        eval_cell("(fs-write \"written.txt\" \"outside content\")"),
        { "text": "refused" },
    ]))
    .await;
    bed.input_with("write it", None, Trust::Untrusted).await;
    let (stream, _) = bed.until_done().await;
    let answers = cell_answers(&stream);
    assert_eq!(
        (
            answers.len(),
            answers[0].contains("refused for an untrusted turn: fs-write"),
            bed.cwd.join("written.txt").exists(),
        ),
        (1, true, false),
        "{answers:?}"
    );
    bed.runtime.shutdown().await;
}

/// An untrusted task reads files under its session's working directory and
/// none outside it.
#[tokio::test]
async fn an_untrusted_tasks_reads_stay_under_the_sessions_directory() {
    let dir = tempfile::TempDir::new().unwrap();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "the outside secret\n").unwrap();
    let mut bed = bed(json!([
        eval_cell("(hash-ref (fs-read \"inside.txt\") 'text)"),
        eval_cell(&format!("(fs-read {:?})", outside.to_string_lossy())),
        { "text": "read" },
    ]))
    .await;
    std::fs::write(bed.cwd.join("inside.txt"), "the inside text\n").unwrap();
    bed.input_with("read them", None, Trust::Untrusted).await;
    let (stream, _) = bed.until_done().await;
    let answers = cell_answers(&stream);
    assert_eq!(
        (
            answers.len(),
            answers[0].contains("the inside text"),
            answers[1].contains("refused for an untrusted turn: fs.read"),
            answers[1].contains("the outside secret"),
        ),
        (2, true, true, false),
        "{answers:?}"
    );
    bed.runtime.shutdown().await;
}

/// A host request the trust table does not list is refused to an untrusted
/// task, though the session answers it.
#[tokio::test]
async fn an_unlisted_host_request_is_refused_to_an_untrusted_task() {
    let mut bed = bed(json!([
        eval_cell(&format!("(host-request {UNLISTED_REQUEST:?} (hash))")),
        { "text": "refused" },
    ]))
    .await;
    bed.input_with("ask it", None, Trust::Untrusted).await;
    let (stream, _) = bed.until_done().await;
    let answers = cell_answers(&stream);
    assert_eq!(
        (
            answers.len(),
            answers[0].contains(&format!(
                "refused for an untrusted turn: {UNLISTED_REQUEST}"
            )),
            answers[0].contains("answered"),
        ),
        (1, true, false),
        "{answers:?}"
    );
    bed.runtime.shutdown().await;
}

/// An untrusted steer is not taken into a running owner's turn, where its
/// cells would run with the owner's reach: it runs as its own task after.
#[tokio::test]
async fn an_untrusted_steer_runs_as_its_own_task() {
    let mut bed = bed(json!([
        { "text": "first reply" },
        { "text": "steer reply" },
    ]))
    .await;
    bed.input(HOLD_PROMPT, None).await;
    bed.hold.entered.notified().await;
    bed.input_with("change course", Some(Priority::Steer), Trust::Untrusted)
        .await;
    bed.hold.release.add_permits(1);
    let (_, first) = bed.until_done().await;
    let (_, steer) = bed.until_done().await;
    assert_eq!(
        (outcome(&first), outcome(&steer), bed.transcript()),
        (
            (TaskState::Done, None, None),
            (TaskState::Done, None, None),
            pairs(&[
                ("user", HOLD_PROMPT),
                ("assistant", "first reply"),
                (
                    "user",
                    "<untrusted-event source=\"test\">change course</untrusted-event>"
                ),
                ("assistant", "steer reply"),
            ])
        )
    );
    bed.runtime.shutdown().await;
}

/// An untrusted task reaches no web: a URL could carry out what it read.
#[tokio::test]
async fn an_untrusted_tasks_web_read_is_refused() {
    let mut bed = bed(json!([
        eval_cell("(web-read \"https://example.invalid/\")"),
        { "text": "refused" },
    ]))
    .await;
    bed.input_with("fetch it", None, Trust::Untrusted).await;
    let (stream, _) = bed.until_done().await;
    let answers = cell_answers(&stream);
    assert_eq!(
        (
            answers.len(),
            answers[0].contains("refused for an untrusted turn: web.read"),
        ),
        (1, true),
        "{answers:?}"
    );
    bed.runtime.shutdown().await;
}

mod events;
