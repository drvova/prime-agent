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
mod confinement;
mod contracts;
mod factory;
mod host_api;
#[cfg(test)]
mod interleavings;
mod limits;

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
use crate::tools::kernel_cell::{
    BootstrapProgressHandler, ExecuteResult, KernelExecuteOptions, KernelExecutor,
    KernelProvisioner,
};
use crate::tools::tool_definition::AbortSignal;

mod mcp;
mod native;
mod rizin;
mod session;
pub mod skills;

pub use cell::StuckCellSink;
pub(in crate::kernel::scheme) use factory::validate_factory_spec;
pub use factory::{factory_cli_dispatch, FactoryEnvironment};
pub(crate) use native::FileReads;

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

/// How long a stopped cell waits for the host to take back the question it
/// had open.
const WITHDRAW_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the end of a session's lifetime waits for the cell on the engine
/// to leave it, before the host shuts the session's runtime down. Longer than
/// every bounded wait a stopped cell may still make on that runtime (a
/// command's kill, a withdrawn question), so only a cell inside a call that
/// does not observe its stop is given up on, and every end of such a session
/// pays this.
const CELL_LEAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// The switches Steel's compiler reads from the process's environment: the
/// first five on every compile (inlining, closure lifting, a dump of each
/// syntax tree to standard output), the last once, as directories to find
/// modules in.
const STEEL_COMPILER_SWITCHES: [&str; 6] = [
    "STEEL_DEBUG_AST",
    "STEEL_INLINE",
    "STEEL_INLINE_RECURSIVE",
    "STEEL_MODULE_INLINE",
    "STEEL_CLOSURE_LIFTING",
    "STEEL_SEARCH_PATHS",
];

/// Keep Steel's own state inside the agent directory, and its compiler as
/// it was built. Unless `STEEL_HOME` is
/// set, the first engine built in a process creates a `steel` directory in
/// the user's data home and prints to stderr when it cannot, which breaks
/// every mode whose stderr is part of its output. Sets `STEEL_HOME` to
/// `<agent dir>/steel`, so it must run while the process is still
/// single-threaded: at the top of `main`, before a runtime exists. An
/// explicit `STEEL_HOME` or an unresolvable agent directory is left alone.
///
/// The directory is named, never created: with the variable set Steel only
/// reads it, and a distribution build's first-run import takes an agent
/// directory that already exists to mean the first run is over.
///
/// [`STEEL_COMPILER_SWITCHES`] are removed from the process's environment:
/// set in the environment the agent was started from (for a Steel of the
/// user's own), they would change how every session's cells compile, or
/// print each cell's syntax tree into a stream that is a protocol.
pub fn pin_scheme_home() {
    for switch in STEEL_COMPILER_SWITCHES {
        std::env::remove_var(switch);
    }
    if pa_types::session_scope::var_os("STEEL_HOME").is_some() {
        return;
    }
    if let Some(home) = pa_types::platform::agent_dir().map(|dir| dir.join("steel")) {
        std::env::set_var("STEEL_HOME", home);
    }
}

/// The process's spare engines, taken by the next sessions to start.
static SPARES: Mutex<Vec<Engine>> = Mutex::new(Vec::new());

/// How many spare engines the process keeps: one for a process that runs a
/// single session, more for one that hosts many
/// ([`keep_spare_scheme_kernels`]).
static SPARE_TARGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// Whether a session has taken an engine yet. Until one has, the process
/// keeps a single spare whatever its target: a session created right after
/// the process started must not compete with a pool being built.
static FIRST_TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How many spares the process holds right now: its target once a session
/// has started, one before that.
fn spare_goal() -> usize {
    use std::sync::atomic::Ordering;
    let target = SPARE_TARGET.load(Ordering::SeqCst);
    if FIRST_TAKEN.load(Ordering::SeqCst) {
        target
    } else {
        target.min(1)
    }
}

/// Whether the refill thread is running.
static REFILLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Keep `target` spare engines once the first session has started (one
/// until then), and start building. A
/// daemon that hosts sessions calls this once at boot: several sessions
/// starting together (a parent's fan-out of children) each take a built
/// engine instead of building their own.
pub fn keep_spare_scheme_kernels(target: usize) {
    SPARE_TARGET.store(target, std::sync::atomic::Ordering::SeqCst);
    prewarm_scheme_kernel();
}

/// How many spare engines are built and waiting: the observable "a new
/// session's kernel will not pay for an engine build" signal.
#[must_use]
pub fn spare_scheme_kernels() -> usize {
    SPARES.lock().unwrap_or_else(PoisonError::into_inner).len()
}

/// Build spare engines in the background until the process holds its target,
/// so the next session's kernel starts without paying for the build. The
/// builds run on their own thread, which belongs to the process: no session
/// waits on it, and a session that ends does not take it down.
pub fn prewarm_scheme_kernel() {
    use std::sync::atomic::Ordering;
    if REFILLING.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("scheme-spare".to_string())
        .spawn(|| {
            while spare_scheme_kernels() < spare_goal() {
                // Built outside the lock: a session starting meanwhile takes
                // what is there, or builds its own.
                match confinement::engine() {
                    Ok(engine) => SPARES
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(engine),
                    Err(error) => {
                        // Each session's own start reports the same failure
                        // to its caller; building more spares would only
                        // repeat it.
                        tracing::error!("a spare Scheme engine could not be built: {error:#}");
                        REFILLING.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
            REFILLING.store(false, Ordering::SeqCst);
            // A session that took an engine after the last check found this
            // thread still marked as running: its refill is started here.
            if spare_scheme_kernels() < spare_goal() {
                prewarm_scheme_kernel();
            }
        });
    if spawned.is_err() {
        // No thread, no spares: sessions build their own engines.
        REFILLING.store(false, Ordering::SeqCst);
    }
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
    /// Whether `(bash ...)` commands run on the built-in shell or on a
    /// shell process (the `shell` setting).
    pub shell: crate::platform::KernelShell,
    /// The session's resolved shell process for `(bash ...)`; `None` uses
    /// the platform's default one.
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
    /// Fires each time the kernel's last live background job settles (a
    /// shell command or a cell that outlived its call), so the session can
    /// run what was waiting for it.
    pub on_background_work_settled: Option<crate::kernel::shared::BackgroundWorkSettledCallback>,
    /// The session's cap on a cell's output, which follows the context the
    /// session has left. It applies only where cut output is kept (a saved
    /// session's spill store); the default never tightens the cap.
    pub output_budget: crate::kernel::spill::OutputBudget,
    /// Hears how each start of the kernel went.
    pub on_started: Option<KernelStartObserver>,
    /// Called when a kernel that stays in use is released and its cells'
    /// definitions go with it (see [`SchemeKernelProvisioner::release`]), so
    /// the session can tell the model.
    pub on_state_lost: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// How one start of a session's kernel went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelStart {
    pub succeeded: bool,
    /// From the first `ensure` that found no kernel to the engine being ready.
    pub took: std::time::Duration,
}

/// Hears each start of a session's kernel (the session's start counters).
pub type KernelStartObserver = Arc<dyn Fn(KernelStart) + Send + Sync>;

/// A session's settings as its cells and host functions read them.
pub(crate) struct SessionEnvironment {
    pub(crate) cwd: PathBuf,
    pub(crate) shell: crate::platform::KernelShell,
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
    pub(crate) background: session::BackgroundWork,
    pub(crate) output_budget: crate::kernel::spill::OutputBudget,
}

/// One session's engine and the cells it runs.
struct SessionEngine {
    engine: Mutex<Engine>,
    /// The global names the engine had when the kernel was ready: any other
    /// is one the session's cells defined. (Their position says nothing: the
    /// engine gives a new name the slot of a shadowed one.)
    own_globals: std::collections::HashSet<String>,
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
    /// The kernel's rizin sessions: they outlive a cell and end with `kill`
    /// and with the session's lifetime.
    rizin: Arc<rizin::Registry>,
    skill_sources: Option<skills::SkillSourcesProvider>,
    on_unavailable_skills: Option<skills::UnavailableSchemeSkillsCallback>,
    on_started: Option<KernelStartObserver>,
    on_state_lost: Option<Arc<dyn Fn() + Send + Sync>>,
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
        let environment = Arc::new(SessionEnvironment {
            cwd: options.cwd,
            shell: options.shell,
            shell_path: options.shell_path,
            command_prefix: options.command_prefix,
            env,
            session_id: options.session_id,
            artifact_dir: options.artifact_dir,
            cell_window: options.cell_window,
            on_background_cell: options.on_background_cell,
            agent_dir: options.agent_dir,
            error_context: options.error_context,
            background: session::BackgroundWork::new(options.on_background_work_settled),
            output_budget: options.output_budget,
        });
        SchemeKernelProvisioner {
            state: Arc::new(ProvisionerState {
                mcp,
                rizin: Arc::new(rizin::Registry::new(
                    options.host_handlers.clone(),
                    &environment,
                )),
                skill_sources: options.skill_sources,
                on_unavailable_skills: options.on_unavailable_skills,
                on_started: options.on_started,
                on_state_lost: options.on_state_lost,
                handlers: options.host_handlers,
                environment,
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

impl KernelProvisioner for SchemeKernelProvisioner {
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
            let rizin_sessions = Arc::clone(&state.rizin);
            let environment = Arc::clone(&state.environment);
            let cwd = state.environment.cwd.to_string_lossy().into_owned();
            let native_cwd = state.environment.cwd.clone();
            let runtime = tokio::runtime::Handle::current();
            // Read here, on the session's own thread.
            // Debug builds only: a session's launch environment is its user's, and no
            // shipped binary may hold a function a cell cannot be stopped in.
            #[cfg(debug_assertions)]
            let blocking_seam =
                pa_types::session_scope::var(limits::BLOCKING_SEAM_ENV).is_ok_and(|set| set == "1");
            let starting = std::time::Instant::now();
            let started = tokio::task::spawn_blocking(move || {
                let spare = SPARES.lock().unwrap_or_else(PoisonError::into_inner).pop();
                FIRST_TAKEN.store(true, std::sync::atomic::Ordering::SeqCst);
                let mut engine = spare.map_or_else(confinement::engine, Ok)?;
                let controllers = confinement::controllers(&mut engine);
                let cells =
                    cell::Cells::new(Arc::clone(&environment), handlers.clone(), controllers);
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
                                            // Bounded: the cell is being
                                            // stopped and must leave the
                                            // engine whatever the host does.
                                            let withdrawn = runtime.block_on(tokio::time::timeout(
                                                WITHDRAW_WAIT,
                                                cancel(HostRequestPayload {
                                                    data: json!({ "type": "question.cancel", "id": id }),
                                                }),
                                            ));
                                            match withdrawn {
                                                Ok(Ok(_)) => {}
                                                Ok(Err(error)) => {
                                                    tracing::warn!("the host refused question.cancel: {error:#}");
                                                }
                                                Err(_) => {
                                                    tracing::warn!("the host did not answer question.cancel in time");
                                                }
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
                #[cfg(debug_assertions)]
                if blocking_seam {
                    engine.register_fn("%test-block", |marker: String| {
                        let marker = std::path::PathBuf::from(marker);
                        // The marker appears from inside the native call: whoever sees it
                        // knows no interrupt can reach this cell any more.
                        if std::fs::write(&marker, "blocked").is_ok() {
                            while marker.exists() {
                                std::thread::sleep(std::time::Duration::from_millis(20));
                            }
                        }
                    });
                }
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
                let refusals = contracts::register(&mut engine, &native_handlers)?;
                contracts::load(&mut engine, HOST_BRIDGE)
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
                    refusals.clone(),
                )?;
                mcp::register(&mut engine, mcp_client, Arc::clone(&cell), refusals)?;
                rizin::register(&mut engine, rizin_sessions, Arc::clone(&cell))?;
                factory::register(&mut engine, factory.clone(), Arc::clone(&cell))?;
                skills.start(&mut engine)?;
                let own_globals = engine
                    .globals()
                    .iter()
                    .map(|name| name.resolve().to_string())
                    .collect();
                anyhow::Ok(Arc::new(SessionEngine {
                    own_globals,
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
            .map_err(|error| anyhow!("the Scheme kernel's start thread failed: {error}"))
            .and_then(std::convert::identity);
            if let Some(on_started) = &state.on_started {
                on_started(KernelStart {
                    succeeded: started.is_ok(),
                    took: starting.elapsed(),
                });
            }
            let session = started?;
            // A hosted session's lifetime can end without `kill` (its host
            // stops serving it and shuts its runtime down): the cell on the
            // engine and the session's commands stop with it all the same.
            if let Some(scope) = pa_types::session_scope::current() {
                // Weakly: a kernel released before then is not kept for it
                // (a cell still on its engine keeps it, and is waited for).
                let ending = Arc::downgrade(&session);
                let disassembling = Arc::downgrade(&state.rizin);
                scope.at_end(Box::new(move || {
                    let ending = ending.upgrade();
                    let leaving = ending.as_ref().is_some_and(|ending| {
                        let leaving = ending.cells.end();
                        ending.jobs.kill_all();
                        leaving
                    });
                    // No rizin, and nothing it debugs, outlives the session.
                    if let Some(disassembling) = disassembling.upgrade() {
                        disassembling.shutdown();
                    }
                    // The stopped cell's thread still waits on the session's
                    // runtime (for the command it kills, for a question it
                    // withdraws), and a wait on a runtime that is shut down
                    // under it panics. The host shuts the runtime down when
                    // this returns: not before the cell's thread is done with
                    // it. The engine's turn is held by exactly that thread.
                    if let (true, Some(ending)) = (leaving, ending) {
                        let until = std::time::Instant::now() + CELL_LEAVE_WAIT;
                        while ending.turn.try_lock().is_err() && std::time::Instant::now() < until {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                }));
            }
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
            state.rizin.shutdown();
        })
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::kernel::shared::host_handler;
    use crate::tools::kernel_cell::ExecuteStatus;

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

    /// Set in the child process of the test below.
    const SWITCHES_CHILD_ENV: &str = "PRIME_AGENT_TEST_STEEL_SWITCHES_CHILD";

    /// A process started with every switch Steel's compiler reads from the
    /// environment compiles as one started with none: with
    /// `STEEL_DEBUG_AST` it printed the syntax tree of everything an engine
    /// compiled to standard output. The switches are set for a child
    /// process, so nothing else in this one compiles under them.
    #[test]
    fn steels_compiler_switches_in_the_environment_do_not_reach_the_kernel() {
        if std::env::var_os(SWITCHES_CHILD_ENV).is_some() {
            pin_scheme_home();
            confinement::engine()
                .unwrap()
                .run("(define (compiled-in-the-child x) (+ x 1))")
                .unwrap();
            let left: Vec<&str> = STEEL_COMPILER_SWITCHES
                .into_iter()
                .filter(|switch| std::env::var_os(switch).is_some())
                .collect();
            println!("switches left: {left:?}");
            return;
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kernel::scheme::tests::steels_compiler_switches_in_the_environment_do_not_reach_the_kernel",
                "--nocapture",
            ])
            .env(SWITCHES_CHILD_ENV, "1")
            .envs(STEEL_COMPILER_SWITCHES.map(|switch| (switch, "1")))
            .output()
            .unwrap();
        let printed = String::from_utf8_lossy(&child.stdout);
        assert_eq!(
            (
                child.status.success(),
                printed.contains("switches left: []"),
                printed.contains("compiled-in-the-child"),
            ),
            (true, true, false),
            "the child printed: {printed}"
        );
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
        // Aborted once the cell's whole line has reached its stream: the
        // cell is spinning by then, however slow the machine is.
        let printed = Mutex::new(String::new());
        let on_stream = move |text: &str, _stream: &'static str| {
            let mut printed = printed.lock().unwrap();
            printed.push_str(text);
            if printed.ends_with('\n') {
                abort.cancel();
            }
        };
        let aborted = kernel
            .execute(
                spin,
                KernelExecuteOptions {
                    signal: Some(signal),
                    timeout_ms: None,
                    on_stream: Some(&on_stream),
                    on_late_sent_agent_message: None,
                },
            )
            .await
            .unwrap();
        // This one prints nothing: what a cell printed before its limit
        // depends on how far it got in that time.
        let timed_out = kernel
            .execute("(spin 0)", options(None, Some(100)))
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
                    String::new(),
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
