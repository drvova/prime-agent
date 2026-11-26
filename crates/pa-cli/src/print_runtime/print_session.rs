//! The print and json modes on the event-driven runtime (`pa_runtime`):
//! the session the runtime builds through the headless engine assembly,
//! each prompt published as the session's input with its stream printed
//! until the task's `done`, then the run's settle and finish, and this
//! mode's exit contract.

use std::collections::HashSet;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use pa_runtime::{
    BuiltSession, DriverEvents, Heard, Listener, SessionBuilder, SessionSpec, SettleAfter,
};
use pa_types::event_log::{AgentDone, AgentInput, NewEvent, TaskFailure, TaskState, Topic, Trust};
use tokio::sync::oneshot;

use crate::args::autonomous_runtime_config;
use crate::mode::{AppMode, RunOptions};

/// How many events the print run's listener may fall behind (a stalled
/// stdout) before it loses the oldest; a json run that loses one fails.
const LISTENER_CAPACITY: NonZeroUsize = NonZeroUsize::new(1 << 16).expect("non-zero");

/// The print run's engine builds: the headless assembly over the session
/// manager the CLI flags selected and the `--goal` seed for the print
/// session, and for each subagent it spawns, its model lane (a scripted
/// child's queue under `children.<spawn name>`, or the catalog model the
/// spawn names or the parent runs) over a transcript of its own under the
/// parent's. The first build reports its session header line (or its error)
/// to the run; every session is built once.
struct PrintSession {
    options: RunOptions,
    faux: Option<serde_json::Value>,
    first: Mutex<Option<FirstBuild>>,
    /// The subagents built so far.
    children: Mutex<HashSet<String>>,
}

/// What the first build takes, and where it reports.
struct FirstBuild {
    manager: pa_core::session::manager::SessionManager,
    opened: oneshot::Sender<Result<Option<String>, String>>,
}

impl SessionBuilder for PrintSession {
    fn build(
        &self,
        id: &str,
        spec: &SessionSpec,
        seams: pa_runtime::SessionSeams,
    ) -> impl Future<Output = anyhow::Result<BuiltSession>> + Send {
        let first = match &spec.subagent {
            Some(_) => None,
            None => self
                .first
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        };
        let first_child_build = spec.subagent.is_some()
            && self
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id.to_string());
        async move {
            if let Some(subagent) = &spec.subagent {
                anyhow::ensure!(first_child_build, "a print session's engine is built once");
                return build_subagent(
                    &self.options,
                    self.faux.as_ref(),
                    id,
                    spec,
                    subagent,
                    seams,
                )
                .await;
            }
            let Some(FirstBuild { manager, opened }) = first else {
                anyhow::bail!("a print session's engine is built once");
            };
            let built = async {
                let headless = super::build_engine(
                    &self.options,
                    self.faux.as_ref(),
                    Some(manager),
                    self.options.app_mode.as_str(),
                    &super::HeadlessEmbedding::Root { seams },
                )
                .await?;
                let engine = Arc::new(headless.engine);
                // The CLI `--goal` seed: a fresh root branch starts the goal; a
                // resumed branch keeps its persisted goal.
                if let Some(goal) = &self.options.config.initial_goal {
                    engine
                        .seed_initial_goal(&goal.objective, goal.token_budget.map(u64::from))
                        .await
                        .map_err(|error| format!("{error:#}"))?;
                }
                let header = super::session_header_json(&engine).await;
                let built = BuiltSession {
                    engine,
                    model: headless.model,
                    api_key: headless.api_key,
                    agent_dir: self.options.config.agent_dir.clone(),
                    events: if self.options.app_mode == AppMode::Json {
                        DriverEvents::Stream
                    } else {
                        DriverEvents::Quiet
                    },
                    autonomous: self
                        .options
                        .config
                        .autonomous
                        .as_ref()
                        .map(autonomous_runtime_config),
                    queue_modes: pa_runtime::QueueModes::Built,
                };
                Ok::<_, String>((built, header))
            }
            .await;
            match built {
                Ok((built, header)) => {
                    let _ = opened.send(Ok(header));
                    Ok(built)
                }
                Err(message) => {
                    let _ = opened.send(Err(message.clone()));
                    Err(anyhow::Error::msg(message))
                }
            }
        }
    }
}

/// Builds a spawned subagent's engine: its model lane (a scripted child's
/// queue under `children.<spawn name>` of the faux script, or the catalog
/// model the spawn names or the parent runs) over a transcript of its own
/// under the parent's. The print run's and the runtime daemon's builders
/// both build their sessions' children this way.
pub(super) async fn build_subagent(
    options: &RunOptions,
    faux: Option<&serde_json::Value>,
    id: &str,
    spec: &SessionSpec,
    subagent: &pa_runtime::SubagentSpec,
    seams: pa_runtime::SessionSeams,
) -> anyhow::Result<BuiltSession> {
    let mut options = options.clone();
    options.config.cwd.clone_from(&spec.cwd);
    let config = &options.config;
    let name = spec
        .name
        .clone()
        .unwrap_or_else(|| subagent.child_id.clone());
    // The child's thinking: the requested level, else the
    // parent's; a real-provider child clamps it to what its model
    // supports.
    let thinking = subagent
        .thinking
        .as_deref()
        .and_then(pa_ai::models::thinking_level_from_str)
        .or(subagent.parent_thinking)
        .unwrap_or(pa_types::ai::ModelThinkingLevel::Medium);
    let (lane, thinking) = if let Some(script) = faux {
        let queue = script
            .get("children")
            .and_then(|children| children.get(&name))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "PRIME_AGENT_FAUX_SCRIPT has no \"children.{name}\" entry: a scripted print run \
                     must script every spawned child"
                )
            })?;
        let steps = super::faux_response_steps(
            queue
                .get("responses")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "PRIME_AGENT_FAUX_SCRIPT children.{name} requires a responses array"
                    )
                })?,
        )
        .map_err(anyhow::Error::msg)?;
        let repeat_last = queue
            .get("repeatLastResponse")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        (
            super::ChildLane::Faux(super::FauxChildQueue {
                child_id: subagent.child_id.clone(),
                steps,
                repeat_last,
            }),
            thinking,
        )
    } else {
        let selector = pa_daemon::rlm_child_model::resolve_child_model_unchecked(
            &config.agent_dir,
            subagent.model.as_deref(),
            subagent.parent_model.as_deref(),
            "subagent",
        )
        .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        pa_daemon::rlm_child_model::assert_thinking_supported(
            &config.agent_dir,
            subagent.thinking.as_deref(),
            &selector,
        )
        .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        let auth = pa_core::auth::AuthStorage::create(&config.agent_dir);
        let registry =
            pa_core::models::ModelRegistry::create(auth, config.agent_dir.join("models.json"));
        let all: Vec<pa_types::ai::Model> = registry.get_all().to_vec();
        let resolved =
            pa_core::models::resolve_cli_model(/*cli_provider*/ None, &selector, &all);
        if let Some(error) = resolved.error {
            return Err(anyhow::Error::msg(error));
        }
        let Some(model) = resolved.model else {
            return Err(anyhow::Error::msg("No matching model found."));
        };
        let thinking = pa_ai::models::clamp_thinking_level(&model, thinking);
        (super::ChildLane::Real(Box::new(model)), thinking)
    };
    // The child's session: its own directory under the parent's
    // artifacts tree; a persisted parent gives the child its own
    // file with the parent edge, a no-session parent keeps the
    // child in memory, still pinned to the child's dir for its
    // artifacts.
    // A child opened again (the runtime daemon's reopened family) keeps the
    // transcript its spawn wrote, in its own directory.
    let reopened = spec
        .transcript
        .as_ref()
        .filter(|path| pa_core::session::store::store().exists(path));
    let child_dir = match reopened.and_then(|path| path.parent()) {
        Some(dir) => dir.to_path_buf(),
        None => pa_core::session_engine::rlm_host::rlm_child_session_dir(
            &config.agent_dir,
            subagent
                .parent_session_file
                .is_some()
                .then_some(subagent.parent.as_str()),
            &subagent.child_id,
        )?,
    };
    let mut manager = if let Some(path) = reopened {
        pa_core::session::manager::SessionManager::open(&config.cwd, &child_dir, path)
    } else if let Some(parent_file) = &subagent.parent_session_file {
        let mut manager =
            pa_core::session::manager::SessionManager::persisted(&config.cwd, &child_dir);
        manager.new_session(&pa_core::session::manager::NewSessionOptions {
            id: Some(id.to_string()),
            parent_session: Some(parent_file.clone()),
            rlm_depth: Some(u64::from(subagent.depth)),
        });
        manager
    } else {
        let mut manager = pa_core::session::manager::SessionManager::in_memory_in_session_dir(
            &config.cwd,
            &child_dir,
        );
        manager.new_session(&pa_core::session::manager::NewSessionOptions {
            id: Some(id.to_string()),
            ..Default::default()
        });
        manager
    };
    if reopened.is_none() {
        manager
            .append_session_info(&name)
            .map_err(anyhow::Error::from)?;
    }
    let headless = super::build_engine(
        &options,
        faux,
        Some(manager),
        "subagent",
        &super::HeadlessEmbedding::Child {
            seams,
            overrides: Box::new(super::ChildEngineOverrides {
                lane,
                thinking,
                depth: subagent.depth,
                session_dir: child_dir,
                semantic_parent_session_id: Some(subagent.parent.clone()),
                spawned_by_request_id: subagent.spawned_by_request_id.clone(),
                factory_child: subagent.factory,
            }),
        },
    )
    .await
    .map_err(anyhow::Error::msg)?;
    Ok(BuiltSession {
        engine: Arc::new(headless.engine),
        model: headless.model,
        api_key: headless.api_key,
        agent_dir: config.agent_dir.clone(),
        events: DriverEvents::Quiet,
        autonomous: None,
        queue_modes: pa_runtime::QueueModes::Built,
    })
}

/// A settle on its way: how the run ended, or `None` when its task stopped
/// it.
type Settling =
    std::pin::Pin<Box<dyn Future<Output = anyhow::Result<Option<pa_runtime::Settled>>> + Send>>;

/// The print session's topics.
struct PrintTopics {
    input: Topic,
    stream: Topic,
    done: Topic,
}

/// What a print run follows the bus until.
enum Followed<T> {
    /// The session's next task ended.
    Done(AgentDone),
    /// The runtime answered.
    Answer(T),
}

/// Prints the session's stream (json mode) until its next `done` or
/// `answer`, whichever comes first. The listener is read first: an answer
/// sent after its events (a settle's, a finish's) is taken only once they
/// are printed.
async fn follow<T>(
    listener: &mut Listener,
    topics: &PrintTopics,
    json_mode: bool,
    answer: impl Future<Output = T>,
) -> Result<Followed<T>, String> {
    let mut answer = std::pin::pin!(answer);
    loop {
        tokio::select! {
            biased;
            heard = listener.recv() => match heard {
                Some(Heard::Event(event)) => {
                    if event.event.topic == topics.stream {
                        if json_mode {
                            println!("{}", event.event.payload);
                        }
                    } else if event.event.topic == topics.done {
                        return serde_json::from_value(event.event.payload.clone())
                            .map(Followed::Done)
                            .map_err(|error| format!("the session's done is malformed: {error}"));
                    }
                }
                Some(Heard::Lagged { missed }) => {
                    return Err(format!("the session's stream fell {missed} events behind"));
                }
                None => return Err("the runtime stopped".to_string()),
            },
            answer = &mut answer => return Ok(Followed::Answer(answer)),
        }
    }
}

/// The print run: the session on an in-process runtime, keeping `lease`
/// (the selected session file's) for the run.
pub(super) async fn print_mode_main(
    options: &RunOptions,
    lease: &mut Option<pa_core::session::lease::SessionLease>,
) -> Result<i32, String> {
    // `print` or `json`: the telemetry execution mode is the app mode (TS
    // main.ts `executionMode: appMode`).
    crate::telemetry_notice::print_if_due(&options.config);
    let (session_manager, opened_lease) = super::select_session_manager_with_lease(options)?;
    *lease = opened_lease;
    let session_manager = session_manager.unwrap_or_else(|| {
        pa_core::session::manager::SessionManager::in_memory(&options.config.cwd)
    });
    // The faux script (when scripted) scripts every spawned child too.
    let faux = super::faux_script_env()?;
    let id = session_manager.get_session_id().to_string();
    let topic = |kind: &str| {
        Topic::new(format!("agent/{id}/{kind}"))
            .map_err(|error| format!("session id {id:?} is not a topic level: {error}"))
    };
    let topics = PrintTopics {
        input: topic("input")?,
        stream: topic("stream")?,
        done: topic("done")?,
    };
    let (opened, built) = oneshot::channel();
    // `--no-session` keeps no record: its runtime's log lives in a scratch
    // dir removed after the run, never in the agent dir.
    let scratch = options
        .session
        .no_session
        .then(|| std::env::temp_dir().join(format!("prime-agent-print-{}", uuid::Uuid::now_v7())));
    let exit = async {
        let runtime = pa_runtime::Runtime::open(
            scratch.as_deref().unwrap_or(&options.config.agent_dir),
            pa_runtime::RuntimeOptions {
                builder: PrintSession {
                    options: options.clone(),
                    faux,
                    first: Mutex::new(Some(FirstBuild {
                        manager: session_manager,
                        opened,
                    })),
                    children: Mutex::new(HashSet::new()),
                },
                spare_kernels: 1,
                // The print session and every subagent it spawns run at
                // once: a parent's turn may wait on its children's.
                max_concurrent_runs: NonZeroUsize::MAX,
                listener_capacity: LISTENER_CAPACITY,
                cwd: options.config.cwd.clone(),
                // The print session's engine builds while the runtime's
                // database opens.
                sessions: vec![SessionSpec {
                    id: Some(id.clone()),
                    name: None,
                    cwd: options.config.cwd.clone(),
                    env: std::collections::BTreeMap::new(),
                    subagent: None,
                    transcript: None,
                    options: pa_runtime::SessionOptions::default(),
                    ephemeral: false,
                }],
            },
        )
        .await
        .map_err(|error| format!("{error:#}"))?;
        let mut listener = runtime.listen();
        let json_mode = options.app_mode == AppMode::Json;
        let ran = async {
            let header = built
                .await
                .map_err(|_| "the print session ended before its engine was built".to_string())??;
            if json_mode {
                if let Some(header) = header {
                    println!("{header}");
                }
            }
            // The prompts in order until one stops the run: the `@file` image
            // attachments ride the initial prompt only. The settle is armed
            // behind the last prompt's task, so it runs right after that turn.
            let prompts: Vec<(&String, Vec<pa_agent::types::ImageContent>)> = options
                .initial_message
                .iter()
                .map(|prompt| (prompt, options.initial_images.clone()))
                .chain(options.messages.iter().map(|prompt| (prompt, Vec::new())))
                .collect();
            let last = prompts.len().checked_sub(1);
            let mut settle: Option<Settling> = None;
            let mut stopped = None;
            for (index, (prompt, images)) in prompts.into_iter().enumerate() {
                let input = AgentInput {
                    text: prompt.clone(),
                    images: images
                        .into_iter()
                        .map(|image| pa_types::ai::ImageContent {
                            data: image.data,
                            mime_type: image.mime_type,
                            rest: serde_json::Map::new(),
                        })
                        .collect(),
                    priority: None,
                };
                let published = runtime
                    .publish(NewEvent {
                        topic: topics.input.clone(),
                        source: options.app_mode.as_str().to_string(),
                        session: Some(id.clone()),
                        payload: serde_json::to_value(input).map_err(|error| error.to_string())?,
                        trust: Trust::Owner,
                        cause: None,
                        reply: None,
                    })
                    .await;
                let followed = match published {
                    Ok(published) => {
                        if Some(index) == last {
                            settle = Some(Box::pin(
                                runtime.settle(&id, SettleAfter::Task(published.event.id)),
                            ));
                        }
                        // The task's `done` comes ahead of anything its settle
                        // streams and of the settle's answer.
                        follow(&mut listener, &topics, json_mode, async {
                            match settle.as_mut() {
                                Some(settling) => settling.await,
                                None => std::future::pending().await,
                            }
                        })
                        .await
                    }
                    Err(error) => Err(format!("{error:#}")),
                };
                let done = match followed {
                    Ok(Followed::Done(done)) => done,
                    Ok(Followed::Answer(_)) => {
                        stopped = Some(Err(
                            "the session settled before its last task's done".to_string()
                        ));
                        break;
                    }
                    Err(error) => {
                        stopped = Some(Err(error));
                        break;
                    }
                };
                let error = done.error.unwrap_or_default();
                match (done.outcome, done.failure) {
                    // A failed reply is the transcript's: the next prompt runs,
                    // and the exit contract reads it.
                    (TaskState::Done, _) | (TaskState::Failed, Some(TaskFailure::Reply)) => {}
                    // The rejected prompt wait: the raw command error on stderr,
                    // exit 1, and no later prompt runs.
                    (TaskState::Failed, Some(TaskFailure::Command)) => {
                        eprintln!("{error}");
                        stopped = Some(Ok(1));
                        break;
                    }
                    (TaskState::Failed, Some(TaskFailure::Turn) | None) => {
                        stopped = Some(Err(error));
                        break;
                    }
                    (TaskState::Cancelled, _) => {
                        stopped = Some(Err("the prompt was cancelled".to_string()));
                        break;
                    }
                    (TaskState::Queued | TaskState::Running, _) => {
                        stopped = Some(Err(format!(
                            "the session reported an unfinished task as done: {error}"
                        )));
                        break;
                    }
                }
            }
            let exit = match stopped {
                Some(exit) => exit,
                // The #2270 fix: print waits for RLM quiescence before the
                // terminal result is selected (json mode keeps streaming).
                None => match follow(
                    &mut listener,
                    &topics,
                    json_mode,
                    settle.unwrap_or_else(|| Box::pin(runtime.settle(&id, SettleAfter::Queued))),
                )
                .await
                {
                    // The print-mode exit contract: json mode never derives the
                    // exit code from the terminal selection (only the autonomous
                    // gates exit non-zero); text mode prints the goal cap, then
                    // the primary message (an error on stderr with exit 1, the
                    // settled answer on stdout), then the compaction outcomes.
                    Ok(Followed::Answer(Ok(Some(settled)))) => {
                        let mut exit_code = 0;
                        if !json_mode {
                            if let Some(reason) = &settled.goal_stopped {
                                eprintln!("{reason}");
                                exit_code = 1;
                            }
                            if let Some(primary) = settled.terminal.primary {
                                if let Some(stderr) = primary.stderr_text(&mut exit_code) {
                                    eprintln!("{stderr}");
                                }
                                if exit_code == 0 {
                                    if let Some(text) = primary.stdout_text() {
                                        println!("{text}");
                                    }
                                }
                            }
                            for outcome in settled.terminal.compaction_outcomes {
                                eprintln!("{}", outcome.content);
                                if outcome.outcome == "failed" {
                                    exit_code = 1;
                                }
                            }
                        }
                        // The autonomous contract applies to both output modes.
                        if let Some(stderr) = settled.autonomous_failure {
                            eprintln!("{stderr}");
                            exit_code = 1;
                        }
                        Ok(exit_code)
                    }
                    Ok(Followed::Answer(Ok(None))) => {
                        Err("the session's last task stopped the run".to_string())
                    }
                    Ok(Followed::Answer(Err(error))) => Err(format!("{error:#}")),
                    Ok(Followed::Done(done)) => Err(format!(
                        "the session ended task {} while it settled",
                        done.task
                    )),
                    Err(error) => Err(error),
                },
            };
            // The disposal drain and the subagents' end run on every exit path,
            // errors included; a failed `/refine` fails the run once awaited.
            let finished = match follow(&mut listener, &topics, json_mode, runtime.finish(&id))
                .await
            {
                Ok(Followed::Answer(finished)) => finished.map_err(|error| format!("{error:#}")),
                Ok(Followed::Done(done)) => Err(format!(
                    "the session ended task {} while it finished",
                    done.task
                )),
                Err(error) => Err(error),
            };
            let exit_code = exit?;
            Ok(if finished?.refine_command_failed {
                1
            } else {
                exit_code
            })
        }
        .await;
        runtime.shutdown().await;
        ran
    }
    .await;
    if let Some(scratch) = scratch {
        // A scratch dir left behind holds only this run's log in the system
        // temp dir; the run's outcome stands either way.
        let _ = std::fs::remove_dir_all(scratch);
    }
    exit
}
