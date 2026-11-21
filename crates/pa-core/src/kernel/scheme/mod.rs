//! The in-process Scheme kernel: one Steel engine per session, living in
//! this process instead of a child interpreter.
//!
//! Building an engine compiles Steel's whole prelude (about 200 ms), so the
//! process keeps one spare engine built ahead of time: a session takes the
//! spare and a replacement is built in the background. Every session's
//! engine is built on its own. Copies of one engine (`Engine::clone`) are not
//! used: they share Steel's parameters and compiler state, so two sessions
//! running at once read each other's output and lose their definitions.
//!
//! A cell runs on a blocking thread, never on the async runtime ([`cell`]
//! has its whole life). What it prints goes through the session's output
//! and error ports to the call's stream and result, with the value of its
//! last form.
//! Stopping a cell interrupts the engine at its next safepoint; the engine
//! cannot unwind native code, so a host function that blocks watches the
//! cell's signal and returns on its own.
//!
//! Scheme reaches the host through one primitive, `(host-request type
//! payload)`: the payload crosses as JSON to the session's
//! [`HostRequestHandlers`], the same handlers the Python kernel's host
//! requests reach, and the reply comes back as a Scheme value or raises.

mod api_help;
mod attach;
mod cell;
mod factory;
mod host_api;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::anyhow;
use serde_json::{json, Value};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use crate::kernel::cell_window::CellWindow;
use crate::kernel::shared::{
    BackgroundCellObserver, HostRequestHandlers, HostRequestPayload, END_TURN_REQUEST_TYPE,
};
use crate::tools::ipython::{
    BootstrapProgressHandler, ExecuteResult, IpythonKernelProvisioner, KernelExecuteOptions,
    KernelExecutor,
};
use crate::tools::tool_definition::AbortSignal;

mod mcp;
mod native;
pub mod skills;

pub(in crate::kernel::scheme) use factory::validate_factory_spec;
pub use factory::{factory_cli_dispatch, FactoryEnvironment};

/// The Scheme side of the host bridge, defined in every session engine after
/// its `%host-request-json` primitive is registered.
const HOST_BRIDGE: &str = r"
(define (host-request type payload)
  (let ([reply (string->jsexpr (%host-request-json type (value->jsexpr-string payload)))])
    (if (hash-contains? reply 'error)
        (error (hash-ref reply 'error))
        (hash-ref reply 'value))))

;; The session's working directory.
(define (session-cwd) %session-cwd)

;; The session's id, or #f when the host gave none.
(define (session-id) %session-id)

;; The saved session's artifact directory, or #f.
(define (session-artifact-dir) %session-artifact-dir)
";

/// Keep Steel's own state inside the agent directory. Unless `STEEL_HOME` is
/// set, the first engine built in a process creates a `steel` directory in
/// the user's data home and prints to stderr when it cannot, which breaks
/// every mode whose stderr is part of its output. Sets `STEEL_HOME` to
/// `<agent dir>/steel`, so it must run while the process is still
/// single-threaded: at the top of `main`, before a runtime exists. An
/// explicit `STEEL_HOME` or an unresolvable agent directory is left alone.
pub fn pin_scheme_home() {
    if std::env::var_os("STEEL_HOME").is_some() {
        return;
    }
    if let Some(home) = pa_types::platform::agent_dir().map(|dir| dir.join("steel")) {
        // Steel only reads the variable; a directory it later needs and
        // cannot create is its own reported error.
        let _ = std::fs::create_dir_all(&home);
        std::env::set_var("STEEL_HOME", home);
    }
}

/// The process's spare engine, taken by the next session to start.
static SPARE: Mutex<Option<Engine>> = Mutex::new(None);

/// Build the spare engine in the background when there is none, so the next
/// session's kernel starts without paying for the build. Must be called from
/// within a tokio runtime.
pub fn prewarm_scheme_kernel() {
    tokio::task::spawn_blocking(|| {
        if SPARE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
        {
            return;
        }
        // Built outside the lock: a session starting meanwhile builds its own.
        let engine = Engine::new();
        SPARE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(engine);
    });
}

/// What a session gives its Scheme kernel: the same wiring its Python
/// kernel's provisioner got. The default is a kernel with no host requests,
/// no shell settings, and cells that run to their end.
#[derive(Default)]
pub struct SchemeKernelOptions {
    /// Answers `(host-request type payload)`.
    pub host_handlers: HostRequestHandlers,
    /// The session's working directory: where relative paths in host
    /// functions resolve and commands run.
    pub cwd: PathBuf,
    /// The session's resolved shell for `(bash ...)`; `None` uses the
    /// platform's default one.
    pub shell_path: Option<PathBuf>,
    /// Shell text run before every `(bash ...)` command (the
    /// `shellCommandPrefix` setting).
    pub command_prefix: Option<String>,
    /// The session's agent directory: exported to commands as
    /// `PRIME_AGENT_CODING_AGENT_DIR`, where MCP servers' stored credentials
    /// are read (`auth.json`), and where the post-edit checks keep what they
    /// learn. Without it those credentials are unavailable and the checks
    /// keep nothing.
    pub agent_dir: Option<PathBuf>,
    /// More environment for commands, on top of the session's own.
    pub env: Vec<(String, String)>,
    /// The session's id, as `(session-id)` returns it.
    pub session_id: Option<String>,
    /// A saved session's artifact directory: its name is the durable session
    /// id commands see as `RLM_SESSION_ID`, and the directory itself is
    /// `RLM_SESSION_DIR`. `None` for a session that is not saved.
    pub artifact_dir: Option<PathBuf>,
    /// When a long cell moves to the background instead of holding its call
    /// (see [`cell`]). The default runs every cell to its end.
    pub cell_window: CellWindow,
    /// Hears each cell that moves to the background, and how it ends.
    pub on_background_cell: Option<BackgroundCellObserver>,
    /// The source lines a failed command a cell waited for gets on the
    /// cell's result. The default attaches none.
    pub error_context: crate::kernel::error_context::ErrorContext,
    /// Names the session's skills, whose Scheme source the kernel loads at
    /// its start and after each reload (see [`skills`]).
    pub skill_sources: Option<skills::SkillSourcesProvider>,
    /// Told which skills' Scheme source failed to load.
    pub on_unavailable_skills: Option<skills::UnavailableSchemeSkillsCallback>,
}

/// A session's settings as its cells and host functions read them.
pub(crate) struct SessionEnvironment {
    pub(crate) cwd: PathBuf,
    pub(crate) shell_path: Option<PathBuf>,
    pub(crate) command_prefix: Option<String>,
    /// What every command gets on top of the process's own environment.
    pub(crate) env: Vec<(String, String)>,
    pub(crate) session_id: Option<String>,
    pub(crate) artifact_dir: Option<PathBuf>,
    pub(crate) cell_window: CellWindow,
    pub(crate) on_background_cell: Option<BackgroundCellObserver>,
    pub(crate) agent_dir: Option<PathBuf>,
    pub(crate) error_context: crate::kernel::error_context::ErrorContext,
}

/// One session's engine and the cells it runs.
struct SessionEngine {
    engine: Mutex<Engine>,
    /// The running cell's stop signal, for host functions that block.
    cell: native::CellSignal,
    /// Held by the cell on the engine, from its start to its end: the engine
    /// runs one cell at a time.
    turn: Arc<tokio::sync::Mutex<()>>,
    cells: Arc<cell::Cells>,
    /// The session's shell commands: told of each cell on the engine, and
    /// killed with the kernel.
    jobs: native::BashJobs,
    /// The session's factory runs, which progress between cells.
    factory: factory::Executor,
    /// The session's skill sources, loaded again after a reload.
    skills: skills::SkillLoader,
}

/// A session's running Scheme kernel.
pub struct SchemeKernel {
    session: Arc<SessionEngine>,
}

impl KernelExecutor for SchemeKernel {
    fn execute<'a>(
        &self,
        code: &str,
        options: KernelExecuteOptions<'a>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ExecuteResult>> + Send + 'a>> {
        Box::pin(cell::execute(
            Arc::clone(&self.session),
            code.to_string(),
            options,
        ))
    }
}

struct ProvisionerState {
    handlers: HostRequestHandlers,
    environment: Arc<SessionEnvironment>,
    /// The session's MCP client: its connections outlive a cell and end
    /// with `kill`.
    mcp: Arc<mcp::Registry>,
    skill_sources: Option<skills::SkillSourcesProvider>,
    on_unavailable_skills: Option<skills::UnavailableSchemeSkillsCallback>,
    /// The session's engine once started; the async lock holds concurrent
    /// `ensure` calls to one startup.
    session: tokio::sync::Mutex<Option<Arc<SessionEngine>>>,
}

/// Owns one session's Scheme kernel: started on the first `ensure` from the
/// process's spare engine (or built on the spot), reused afterwards, and
/// dropped by `kill`.
pub struct SchemeKernelProvisioner {
    state: Arc<ProvisionerState>,
}

impl SchemeKernelProvisioner {
    /// A provisioner whose kernel answers `(host-request ...)` from `handlers`
    /// and works in `cwd`, with every other option at its default.
    #[must_use]
    pub fn new(handlers: HostRequestHandlers, cwd: impl Into<PathBuf>) -> Self {
        Self::with_options(SchemeKernelOptions {
            host_handlers: handlers,
            cwd: cwd.into(),
            ..SchemeKernelOptions::default()
        })
    }

    /// A provisioner for a session's kernel, wired as `options` say.
    #[must_use]
    pub fn with_options(options: SchemeKernelOptions) -> Self {
        // As the Python kernel's environment: the agent directory, and the
        // saved session's id and directories. The id is always set, so a
        // session that is not saved never inherits the one of a session it
        // was started from.
        let mut env = vec![(
            "RLM_SESSION_ID".to_string(),
            options
                .artifact_dir
                .as_deref()
                .and_then(std::path::Path::file_name)
                .map(|id| id.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )];
        for (name, path) in [
            ("PRIME_AGENT_CODING_AGENT_DIR", options.agent_dir.clone()),
            ("RLM_SESSION_DIR", options.artifact_dir.clone()),
            (
                "RLM_HARNESS_STATE_DIR",
                crate::refinement::get_local_harness_state_dir(options.artifact_dir.as_deref()),
            ),
        ] {
            if let Some(path) = path {
                env.push((name.to_string(), path.to_string_lossy().into_owned()));
            }
        }
        env.extend(options.env);
        let mcp = Arc::new(mcp::Registry::new(options.host_handlers.clone()));
        if let Some(agent_dir) = &options.agent_dir {
            mcp.set_agent_dir(agent_dir.clone());
        }
        SchemeKernelProvisioner {
            state: Arc::new(ProvisionerState {
                mcp,
                skill_sources: options.skill_sources,
                on_unavailable_skills: options.on_unavailable_skills,
                handlers: options.host_handlers,
                environment: Arc::new(SessionEnvironment {
                    cwd: options.cwd,
                    shell_path: options.shell_path,
                    command_prefix: options.command_prefix,
                    env,
                    session_id: options.session_id,
                    artifact_dir: options.artifact_dir,
                    cell_window: options.cell_window,
                    on_background_cell: options.on_background_cell,
                    agent_dir: options.agent_dir,
                    error_context: options.error_context,
                }),
                session: tokio::sync::Mutex::new(None),
            }),
        }
    }
}

impl SchemeKernelProvisioner {
    /// Read stored MCP credentials from `agent_dir`'s `auth.json`, for a
    /// provisioner made without [`SchemeKernelOptions::agent_dir`]. Without
    /// either, an MCP server that needs a stored credential reports it as
    /// unavailable.
    #[must_use]
    pub fn with_agent_dir(self, agent_dir: impl Into<PathBuf>) -> Self {
        self.state.mcp.set_agent_dir(agent_dir.into());
        self
    }

    /// Open each of `servers` and list its tools, each bounded by
    /// `per_server_timeout_ms`: one `{server, tools, error}` per server, as
    /// the Python kernel manager's `mcp_tool_listing` answered. It needs no
    /// started kernel, and a cell that calls a server meanwhile joins the
    /// open in flight: this is what settles a session's MCP servers in the
    /// background.
    pub async fn mcp_tool_listing(
        &self,
        servers: &[String],
        per_server_timeout_ms: u64,
    ) -> Vec<Value> {
        self.state
            .mcp
            .tool_listing(servers, per_server_timeout_ms)
            .await
    }

    /// Answer one factory request of the daemon and TUI lane (`{id, action,
    /// runId, specId, timeoutMs}`) from the running kernel's factory with
    /// its reply frame (`{event, id, status, result | reason}`).
    ///
    /// # Errors
    ///
    /// The session's kernel is not running: it holds the runs.
    pub async fn factory_activity(&self, request: &Value) -> anyhow::Result<Value> {
        let session = self.state.session.lock().await.clone();
        match session {
            Some(session) => Ok(session.factory.activity_reply(request).await),
            None => Err(anyhow!(
                "the Scheme kernel is not running: it has no factory runs"
            )),
        }
    }
}

impl IpythonKernelProvisioner for SchemeKernelProvisioner {
    fn ensure(
        &self,
        _on_progress: Option<BootstrapProgressHandler>,
        signal: Option<AbortSignal>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn KernelExecutor>>> + Send>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            if signal.as_ref().is_some_and(AbortSignal::is_cancelled) {
                return Err(anyhow!("Kernel start aborted"));
            }
            let mut slot = state.session.lock().await;
            if let Some(session) = slot.as_ref() {
                return Ok(Box::new(SchemeKernel {
                    session: Arc::clone(session),
                }) as Box<dyn KernelExecutor>);
            }
            // The learned window's stored state loads while the engine starts.
            state.environment.cell_window.prime();
            let mut handlers = state.handlers.clone();
            let skills = skills::SkillLoader::new(
                state.skill_sources.clone(),
                state.on_unavailable_skills.clone(),
                &mut handlers,
            );
            let factory = factory::Executor::new(
                state.handlers.clone(),
                state.environment.agent_dir.clone().map_or_else(
                    FactoryEnvironment::default,
                    |agent_dir| {
                        FactoryEnvironment::for_session(
                            agent_dir,
                            crate::refinement::get_local_harness_state_dir(
                                state.environment.artifact_dir.as_deref(),
                            ),
                        )
                    },
                ),
                state.environment.cwd.clone(),
            );
            let native_handlers = state.handlers.clone();
            let mcp_client = Arc::clone(&state.mcp);
            let environment = Arc::clone(&state.environment);
            let cwd = state.environment.cwd.to_string_lossy().into_owned();
            let native_cwd = state.environment.cwd.clone();
            let runtime = tokio::runtime::Handle::current();
            let session = tokio::task::spawn_blocking(move || {
                let spare = SPARE.lock().unwrap_or_else(PoisonError::into_inner).take();
                let mut engine = spare.unwrap_or_default();
                let cells = cell::Cells::new(
                    Arc::clone(&environment),
                    handlers.clone(),
                    engine.get_thread_state_controller(),
                );
                engine.register_fn(
                    "%host-request-json",
                    move |request_type: String, payload: String| -> String {
                        let running = cell::current();
                        let reply = match (
                            request_type == END_TURN_REQUEST_TYPE,
                            handlers.get(&request_type),
                            serde_json::from_str::<Value>(&payload),
                        ) {
                            // The kernel's own answer, as the Python kernel's
                            // manager gave it: the running cell is marked,
                            // and its result ends the turn.
                            (true, _, _) => match running
                                .ok_or_else(|| {
                                    anyhow!("rlm.end_turn has no running cell to end the turn for")
                                })
                                .and_then(|cell| cell.end_turn())
                            {
                                Ok(value) => json!({ "value": value }),
                                Err(error) => json!({ "error": format!("{error:#}") }),
                            },
                            (false, None, _) => {
                                json!({ "error": format!("unknown host request: {request_type}") })
                            }
                            (false, Some(_), Err(error)) => {
                                json!({ "error": format!("{request_type}: payload is not JSON: {error}") })
                            }
                            (false, Some(handler), Ok(data)) => {
                                // The engine cannot interrupt this wait: it
                                // ends with the cell's signal instead, and
                                // dropping the request withdraws it.
                                let stopped = running.map(|cell| cell.signal());
                                let question = (request_type == "question.ask")
                                    .then(|| data.get("id").cloned())
                                    .flatten();
                                let answered = runtime.block_on(async {
                                    let request = handler(HostRequestPayload { data });
                                    match &stopped {
                                        Some(stopped) => tokio::select! {
                                            answer = request => Some(answer),
                                            () = stopped.cancelled() => None,
                                        },
                                        None => Some(request.await),
                                    }
                                });
                                match answered {
                                    Some(Ok(value)) => json!({ "value": value }),
                                    Some(Err(error)) => json!({ "error": format!("{error:#}") }),
                                    None => {
                                        // An open question must leave the
                                        // user's terminal with its cell.
                                        if let (Some(id), Some(cancel)) =
                                            (question, handlers.get("question.cancel"))
                                        {
                                            let withdrawn = runtime.block_on(cancel(HostRequestPayload {
                                                data: json!({ "type": "question.cancel", "id": id }),
                                            }));
                                            if let Err(error) = withdrawn {
                                                tracing::warn!("the host refused question.cancel: {error:#}");
                                            }
                                        }
                                        json!({ "error": format!("{request_type}: the cell was stopped while it waited") })
                                    }
                                }
                            }
                        };
                        reply.to_string()
                    },
                );
                engine.register_value("%session-cwd", SteelVal::StringV(cwd.into()));
                for (name, value) in [
                    ("%session-id", environment.session_id.clone()),
                    (
                        "%session-artifact-dir",
                        environment
                            .artifact_dir
                            .as_deref()
                            .map(|dir| dir.to_string_lossy().into_owned()),
                    ),
                ] {
                    engine.register_value(
                        name,
                        value.map_or(SteelVal::BoolV(false), |text| SteelVal::StringV(text.into())),
                    );
                }
                engine
                    .run(HOST_BRIDGE)
                    .map_err(|error| anyhow!("the Scheme host bridge failed to load: {error}"))?;
                cell::register(&mut engine, &cells)?;
                native::register_session_tools(&mut engine, &native_handlers, &environment);
                host_api::load(&mut engine)?;
                api_help::register(&mut engine)?;
                let cell = native::CellSignal::default();
                let jobs = native::register(
                    &mut engine,
                    native_cwd,
                    Arc::clone(&cell),
                    native_handlers,
                    environment.agent_dir.clone(),
                    &environment,
                )?;
                mcp::register(&mut engine, mcp_client, Arc::clone(&cell))?;
                factory::register(&mut engine, factory.clone(), Arc::clone(&cell))?;
                skills.start(&mut engine)?;
                anyhow::Ok(Arc::new(SessionEngine {
                    engine: Mutex::new(engine),
                    cell,
                    turn: Arc::new(tokio::sync::Mutex::new(())),
                    cells,
                    jobs,
                    factory,
                    skills,
                }))
            })
            .await
            .map_err(|error| anyhow!("the Scheme kernel's start thread failed: {error}"))??;
            *slot = Some(Arc::clone(&session));
            // The spare is spent: build the next session's.
            prewarm_scheme_kernel();
            Ok(Box::new(SchemeKernel { session }) as Box<dyn KernelExecutor>)
        })
    }

    fn kill(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            // A cell still running holds its own reference: interrupt it so
            // the dropped engine stops instead of running on unobserved.
            if let Some(session) = state.session.lock().await.take() {
                session.cells.kill();
                session.jobs.kill_all();
                // The runs lived in this kernel: their loops end with it.
                session.factory.shutdown();
            }
            state.mcp.shutdown().await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::kernel::shared::host_handler;
    use crate::tools::ipython::ExecuteStatus;

    fn options(
        signal: Option<AbortSignal>,
        timeout_ms: Option<u64>,
    ) -> KernelExecuteOptions<'static> {
        KernelExecuteOptions {
            signal,
            timeout_ms,
            on_stream: None,
            on_late_sent_agent_message: None,
        }
    }

    /// What a cell answered, for whole-value comparison.
    fn answer(result: ExecuteResult) -> (ExecuteStatus, String, Option<String>, Option<String>) {
        (
            result.status,
            result.stdout,
            result.result,
            result.error.map(|error| error.ename),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn definitions_persist_in_their_session_and_output_is_captured() {
        let first = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/");
        let other = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/");
        let kernel = first.ensure(None, None).await.unwrap();
        kernel
            .execute(
                "(define (area-of-square side) (* side side))",
                options(None, None),
            )
            .await
            .unwrap();
        let again = first.ensure(None, None).await.unwrap();
        let reused = again
            .execute(
                "(displayln \"squaring\") (area-of-square 12)",
                options(None, None),
            )
            .await
            .unwrap();
        let elsewhere = other
            .ensure(None, None)
            .await
            .unwrap()
            .execute("(area-of-square 12)", options(None, None))
            .await
            .unwrap();
        assert_eq!(
            (answer(reused), answer(elsewhere).0),
            (
                (
                    ExecuteStatus::Ok,
                    "squaring\n".to_string(),
                    Some("144".to_string()),
                    None
                ),
                ExecuteStatus::Error
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn host_requests_reach_the_session_handlers_and_their_errors_raise() {
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "echo-upper",
            host_handler(|payload: HostRequestPayload| async move {
                let text = payload.data["text"].as_str().unwrap_or_default();
                Ok(json!({ "text": text.to_uppercase() }))
            }),
        );
        handlers.register(
            "always-fails",
            host_handler(
                |_payload: HostRequestPayload| async move { Err(anyhow!("the host refused")) },
            ),
        );
        let kernel = SchemeKernelProvisioner::new(handlers, "/")
            .ensure(None, None)
            .await
            .unwrap();
        let echoed = kernel
            .execute(
                "(hash-ref (host-request \"echo-upper\" (hash 'text \"quiet\")) 'text)",
                options(None, None),
            )
            .await
            .unwrap();
        let refused = kernel
            .execute(
                "(host-request \"always-fails\" (hash))",
                options(None, None),
            )
            .await
            .unwrap();
        assert_eq!(
            (
                answer(echoed),
                refused.status,
                refused
                    .error
                    .is_some_and(|error| error.evalue.contains("the host refused"))
            ),
            (
                (
                    ExecuteStatus::Ok,
                    String::new(),
                    Some("\"QUIET\"".to_string()),
                    None
                ),
                ExecuteStatus::Error,
                true
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_abort_or_timeout_stops_a_runaway_cell_and_the_kernel_stays_usable() {
        let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
            .ensure(None, None)
            .await
            .unwrap();
        let spin = "(define (spin n) (spin (+ n 1))) (displayln \"spinning\") (spin 0)";
        let signal = AbortSignal::new();
        let abort = signal.clone();
        let aborting = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            abort.cancel();
        });
        let aborted = kernel
            .execute(spin, options(Some(signal), None))
            .await
            .unwrap();
        aborting.await.unwrap();
        let timed_out = kernel
            .execute(spin, options(None, Some(100)))
            .await
            .unwrap();
        let after = kernel
            .execute("(+ 1 2)", options(None, None))
            .await
            .unwrap();
        assert_eq!(
            (answer(aborted), answer(timed_out), answer(after)),
            (
                (ExecuteStatus::Aborted, "spinning\n".to_string(), None, None),
                (
                    ExecuteStatus::Error,
                    "spinning\n".to_string(),
                    None,
                    Some("Timeout".to_string())
                ),
                (
                    ExecuteStatus::Ok,
                    String::new(),
                    Some("3".to_string()),
                    None
                )
            )
        );
    }

    /// Sessions running at the same time keep their own output and
    /// definitions: the failure mode of engines copied from one template.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_sessions_never_see_each_others_output_or_definitions() {
        let sessions = ["alpha", "beta", "gamma", "delta"].map(|name| {
            tokio::spawn(async move {
                let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
                    .ensure(None, None)
                    .await
                    .unwrap();
                let mut seen = Vec::new();
                for round in 0..50 {
                    let cell = format!("(define mine \"{name}{round}\") (display mine)");
                    seen.push(
                        kernel
                            .execute(&cell, options(None, None))
                            .await
                            .unwrap()
                            .stdout,
                    );
                }
                (name, seen)
            })
        });
        for session in sessions {
            let (name, seen) = session.await.unwrap();
            let own: Vec<String> = (0..50).map(|round| format!("{name}{round}")).collect();
            assert_eq!(seen, own);
        }
    }
}
