//! `SessionEngine` assembly: build a running agent session from a config.
//! The session subscribes persistence listeners on the caller's reactor, so
//! `create_session` is async.

use std::path::PathBuf;
use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::stream::StreamFn;
use pa_agent::types::{Model, ThinkingLevel};
use pa_types::ai::KERNEL_TOOL_NAME;

use crate::resources::{load_resources, ResourceLoaderOptions};
use crate::session::manager::SessionManager;
use crate::skills::PromptTemplate;

use super::{AgentSession, PromptOptions, PromptOutcome};

/// The background MCP settle's per-server open bound (the kernel's
/// `mcp_status` listing opens each not-yet-connected server bounded per
/// server; the settle is off every user-visible path, so the bound only
/// keeps the background task from outliving a wedged server forever).
const MCP_SETTLE_PER_SERVER_TIMEOUT_MS: u64 = 10_000;

/// Everything needed to assemble a session.
#[derive(Default)]
pub struct SessionEngineConfig {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub stream_fn: Option<StreamFn>,
    pub tools: Vec<Arc<dyn pa_agent::types::AgentTool>>,
    pub custom_system_prompt: Option<String>,
    /// Explicit append-system-prompt sources from the host; overrides file discovery.
    pub prompt_guidelines: Vec<String>,
    pub generic_mcp_servers: Vec<String>,
    pub allow_recursion: Option<bool>,
    /// In-memory when None.
    pub session_manager: Option<SessionManager>,
    /// Merged over the built-in goal/heartbeat registrations.
    pub extra_host_handlers: Option<crate::kernel::shared::HostRequestHandlers>,
    /// Conversation-log path for the system prompt when the caller owns
    /// persistence outside the session manager.
    pub conversation_log_path: Option<PathBuf>,
    pub additional_skill_paths: Vec<String>,
    pub additional_prompt_paths: Vec<String>,
    pub extra_builtin_skill_overrides: Vec<String>,
    pub rlm_subagent_host: Option<Arc<dyn super::rlm_host::RlmSubagentHost>>,
    pub rlm_usage_store: Option<Arc<dyn super::rlm_usage::RlmChildUsageStore>>,
    /// Announces every accepted `rlm.progress.note`; `None` keeps the store
    /// the only surface.
    pub progress_note_emit: Option<super::rlm_host::RlmProgressNoteEmit>,
    /// The embedding's todo/question hub; `None` keeps the session headless
    /// (`ask` answers `no_user` at once).
    pub interaction: Option<Arc<crate::interaction::InteractionHub>>,
    /// The session's depth in the RLM recursion tree (0 for top-level
    /// sessions); gates the `refine.*` host requests.
    pub rlm_depth: Option<u32>,
    /// Whether this session is a factory state's child (its spawn carried a
    /// factory run tag); selects the child prompt's factory line.
    pub factory_child: bool,
    /// Whether the session runs without a daemon: nothing outside the
    /// session itself (no scheduler, no bash-completion wake) starts a
    /// turn. Set explicitly by the embedding; print sessions are
    /// daemonless with or without an inline-subagent host, daemon
    /// workers are not.
    pub daemonless: bool,
    /// The full registry model (input modalities for `model.info`); the
    /// engine derives minimal facts from `model` when absent.
    pub model_info: Option<pa_types::ai::Model>,
    /// Session telemetry wiring (the telemetry client + execution mode). `None`
    /// (opt-out) installs nothing; non-depth-0 sessions never install.
    pub telemetry: Option<super::telemetry::TelemetryWiring>,
    /// Invoked at the goal pause/clear/start command sites and after a
    /// kernel `goal.complete` settles the goal.
    pub queued_goal_context_purge: Option<super::runtime::QueuedGoalContextPurge>,
    /// `true` while a steering-lane action is queued or mid-selection: the
    /// running turn stops at the next boundary and the queued steer delivers.
    pub queued_steering_probe: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Embedding-owned turn boundary, consulted after queued steering.
    /// Returning true yields the run; the embedding owns any continuation.
    pub should_stop_after_turn: Option<pa_agent::agent_loop::ShouldStopAfterTurnFn>,
    /// `None` keeps the TS default ("one-at-a-time").
    pub steering_mode: Option<pa_agent::agent::QueueMode>,
    pub follow_up_mode: Option<pa_agent::agent::QueueMode>,
    /// Start the kernel in the background at creation, at every RLM depth (a
    /// subagent's kernel starts while its first model call is in flight); a
    /// failed start surfaces on the next cell.
    pub prewarm_kernel: Option<bool>,
    /// Fires each time the kernel's last live background job (a shell
    /// command or a cell that outlived its call) settles; `None` installs
    /// nothing.
    pub on_background_work_settled: Option<crate::kernel::shared::BackgroundWorkSettledCallback>,
    /// The daemonless kernel-notice wake (TS `resumeIfIdle`): with it
    /// the engine registers the `bash.completed`/`bash.consumed`/
    /// `factory.progress` handlers whose in-process sinks steer the notice
    /// row onto the session's agent and fire the wake. `None` installs nothing —
    /// the daemon keeps its worker-lane sinks via the extra handlers.
    pub bash_notice_wake: Option<super::bash_notices::BashNoticeWake>,
    /// An externally owned MCP manager: the engine adopts it instead of
    /// building its own, so ACP-admitted servers reach the prompt's MCP gating.
    pub mcp_manager: Option<std::sync::Arc<std::sync::Mutex<crate::mcp::McpManager>>>,
    /// The kernel's `rlm_heartbeat.*` host requests write and read this
    /// embedding-owned store instead of the engine-private `cron-jobs.json`.
    pub cron_store: Option<super::runtime_wiring::KernelCronWiring>,
    /// The image-model routing host seam: the headless surfaces install
    /// theirs; the daemon worker stays `None` (its dispatch owns routing).
    pub image_model_router: Option<super::image_model_routing::ImageModelRouter>,
    /// The session's semantic-edge identity (TS
    /// `semanticEdgeLedgerPath` + `semanticParentSessionId` +
    /// `semanticSpawnedByRequestId`): the recorder's ledger location and
    /// spawn provenance. `None` keeps the session off the ledger (no
    /// request ids on the wire).
    pub semantic_edges: Option<super::semantic_edges::SemanticEdgeIdentity>,
    pub on_late_sent_agent_message: Option<crate::tools::kernel_cell::LateSentAgentMessageHandler>,
}

pub struct SessionEngine {
    pub session: AgentSession,
    pub skills: Vec<crate::skills::Skill>,
    /// Skill-loading diagnostics; the connection resource snapshot
    /// surfaces them.
    pub skill_diagnostics: Vec<crate::skills::ResourceDiagnostic>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub agents_files: Vec<crate::resources::ContextFile>,
    pub system_prompt: String,
    /// Byte length of the system prompt's cache-stable static prefix (the context window's
    /// static/dynamic split).
    pub system_prompt_static_len: usize,
    /// The request view's thinning settings (the context window's estimate replays them).
    pub context_thinning: super::context_thinning::ThinningSettings,
    /// The same instance the kernel `goal.*` host handlers reach, so `/goal`
    /// and `goal.complete()` observe one state machine.
    pub goal_driver: std::sync::Arc<tokio::sync::Mutex<super::goal_driver::GoalDriver>>,
    /// `/goal` pause/clear/start and the kernel's `goal.complete` withdraw
    /// queued goal-context turns through it; `None` when no queue.
    pub queued_goal_context_purge: Option<super::runtime::QueuedGoalContextPurge>,
    /// Auth gating and the source the `mcp.*` kernel host handlers resolve
    /// against; `replace_acp_mcp_servers` reaches it through this handle.
    pub mcp_manager: std::sync::Arc<std::sync::Mutex<crate::mcp::McpManager>>,
    pub turn_boundary: std::sync::Arc<super::turn_boundary::TurnBoundaryRequests>,
    /// `None` when telemetry is disabled or the session is not depth 0.
    pub telemetry: Option<std::sync::Arc<super::telemetry::SessionTelemetry>>,
    /// The kernel `worktree` capability's ledger: the worktrees this session made and has not
    /// removed, named by the session-end notice.
    pub worktrees: std::sync::Arc<crate::worktree_host::WorktreeLedger>,
    /// The kernel `rlm.spawn` handler registers spawn targets into it; the
    /// embedding wires the child-observation sink onto it after the build.
    pub rlm_usage: std::sync::Arc<super::rlm_usage::RlmChildUsageAttributions>,
    /// The factory host bridge (`/factory` view lane): the daemon/TUI
    /// request surface over the kernel's factory runs, built from the
    /// session facts captured in `create_session` (the #3184 capture
    /// pattern) and reached through [`SessionEngine::factory_activity`].
    pub factory_host: super::factory_host::FactoryHost,
    /// The progress-note store the `rlm.progress.note` handler throttles
    /// into; the embedding mirrors the Arc for sync reads.
    pub rlm_progress_notes: std::sync::Arc<super::rlm_host::RlmProgressNotes>,
    /// The session's kernel: the in-process Scheme engine with its shell
    /// commands, MCP connections, factory runs and background cells. The
    /// engine and the kernel tool on the agent own it; the compaction probe,
    /// the cache seam's pending-work probe and the MCP settle hold it
    /// weakly. Its host handlers reach back into the session, so a session
    /// that ends must call [`SessionEngine::dispose_kernel`]: that kills
    /// everything the kernel started.
    pub(crate) provisioner: std::sync::Arc<crate::kernel::scheme::SchemeKernelProvisioner>,
    /// The session's current resources and the `rlm.reload`/`/reload` re-read.
    pub resource_reload: std::sync::Arc<super::resource_reload::ResourceReload>,
    /// The `/better` runner the TUI command, the session command, and the kernel share.
    pub better: std::sync::Arc<super::better::BetterHost>,
}

/// Skill overrides for built-in integrations the user is not logged into,
/// plus the enabled persistent generic servers.
async fn mcp_gating(
    settings: &crate::settings::SettingsManager,
    agent_dir: std::path::PathBuf,
) -> anyhow::Result<(Vec<String>, Vec<String>, crate::mcp::McpManager)> {
    // MCP execution is user/global only, matching TS getGlobalMcpServers.
    let user_servers = settings
        .global_settings()
        .mcp_servers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(server, config)| {
            serde_json::from_value(config)
                .ok()
                .map(|parsed| (server, parsed))
        })
        .collect::<std::collections::HashMap<String, crate::mcp::McpServerConfig>>();
    // The MCP manager snapshots auth with a blocking lock; run it off the
    // async runtime.
    tokio::task::spawn_blocking(move || mcp_gating_blocking(user_servers, &agent_dir))
        .await
        .map_err(|error| anyhow::anyhow!("MCP gating task failed: {error}"))
}

fn mcp_gating_blocking(
    user_servers: std::collections::HashMap<String, crate::mcp::McpServerConfig>,
    agent_dir: &std::path::Path,
) -> (Vec<String>, Vec<String>, crate::mcp::McpManager) {
    crate::mcp::McpManager::prompt_gating(user_servers, agent_dir)
}

/// Assemble a session: load resources, build the system prompt, and start the
/// loop with persistence wiring.
///
/// # Errors
///
/// Returns an error when the MCP gating task fails, when session resources
/// cannot be loaded, or when the runtime bootstrap fails.
///
/// # Panics
///
/// Panics if the MCP manager mutex is poisoned while wiring telemetry
/// reporting.
pub async fn create_session(mut config: SessionEngineConfig) -> anyhow::Result<SessionEngine> {
    let cwd = config.cwd.clone();
    // Session persistence first: the conversation-log path and the resume
    // context both come from the session manager.
    let mut session_manager = config
        .session_manager
        .unwrap_or_else(|| SessionManager::in_memory(&cwd));
    let conversation_log = {
        let session = &session_manager;
        session
            .get_session_file()
            .map(|path| path.display().to_string())
            .or_else(|| {
                config
                    .conversation_log_path
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
    };
    let (settings, trace_consent) =
        crate::agent_traces::ContinuousTraceUpload::load_settings(&cwd, &config.agent_dir);
    // Resolve the kernel shell once, before any side effects: an invalid
    // `shellPath` must fail session creation with a clear config error
    // naming the setting, not every kernel `bash()` call later.
    let kernel_shell_path = crate::platform::shell::resolve_kernel_bash_shell(
        settings.settings().shell_path.as_deref(),
    )?
    .map(std::path::PathBuf::from);
    // TS injects the prefix only when truthy (an empty string stays unset).
    let kernel_command_prefix = settings
        .settings()
        .shell_command_prefix
        .clone()
        .filter(|prefix| !prefix.is_empty());
    let traces = session_manager
        .is_persisted()
        .then(|| {
            crate::agent_traces::ContinuousTraceUpload::install(
                &cwd,
                &config.agent_dir,
                session_manager.get_session_file(),
                trace_consent,
            )
        })
        .flatten();
    if let Some(traces) = traces {
        session_manager.on_persist(Box::new(move |path| traces.persisted(path)));
    }
    let wiring = super::runtime_wiring::wire_session_runtime(
        session_manager,
        &config.agent_dir,
        super::runtime_wiring::RlmWiring {
            model_registry: None,
            subagent_host: config.rlm_subagent_host.clone(),
            usage_store: config.rlm_usage_store.clone(),
            progress_note_emit: config.progress_note_emit.clone(),
            interaction: config.interaction.clone(),
        },
        config.queued_goal_context_purge.clone(),
        config.cron_store.clone(),
    );

    let refinement_settings = settings.reopen();
    let service_tier_preference = settings.get_default_service_tier();
    // Captured before `settings` moves into the resource loader: the
    // compaction budget and the auto-refine gates.
    let compaction_settings = settings.settings().compaction.clone().unwrap_or_default();
    let compaction_threshold_percent = settings.get_compaction_threshold_percent();
    let thinning_settings = super::context_thinning::ThinningSettings::resolve(
        settings.settings().context_thinning.as_ref(),
    );
    let auto_refine_gates =
        super::refine::AutoRefineGates::from_settings(settings.settings().auto_refine.as_ref());
    // Request timing: the settings half of the flag is read once here
    // (`settings` moves into the loader); the `PI_REQUEST_TIMING` half stays live.
    let request_timing_settings = settings.get_request_timing();
    // The stream loop guard's level (`loopGuard`), read once like the flag above.
    let loop_guard_level = settings.get_loop_guard();
    // Captured before `settings` moves into the resource loader: the
    // factory host bridge's preflight facts (the daemon `allowedModels`
    // pin), like the request-timing snapshot above.
    let factory_allowed_models = settings.get_allowed_models();
    // Captured before `settings` moves into the resource loader: the cell
    // yield window the user set (`0` disables; unset, it is learned).
    let cell_timeout_ms = settings.get_cell_timeout_ms();
    // The prompt-cache policy (`cacheRetention`, `cacheWarming`).
    let (cache_retention, cache_warming) =
        (settings.get_cache_retention(), settings.get_cache_warming());
    let web_search_provider = settings.get_web_search_provider();
    // The `settings.session` view's armed values, read before `settings` moves.
    let (compaction_enabled, image_model) = (
        settings.get_compaction_enabled(),
        settings.get_image_model(),
    );
    let (mcp_skill_overrides, mcp_generic_servers, built_manager) =
        mcp_gating(&settings, config.agent_dir.clone()).await?;
    let mcp_manager = config
        .mcp_manager
        .take()
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(built_manager)));
    let mut extra_builtin_skill_overrides = config.extra_builtin_skill_overrides.clone();
    extra_builtin_skill_overrides.extend(mcp_skill_overrides);
    let mut generic_mcp_servers = config.generic_mcp_servers.clone();
    for server in mcp_generic_servers {
        if !generic_mcp_servers.contains(&server) {
            generic_mcp_servers.push(server);
        }
    }
    let reload_skill_overrides = extra_builtin_skill_overrides.clone();
    let resources = load_resources(ResourceLoaderOptions {
        cwd: cwd.clone(),
        agent_dir: config.agent_dir.clone(),
        settings: Some(settings),
        extra_builtin_skill_overrides,
        additional_skill_paths: config.additional_skill_paths.clone(),
        additional_prompt_paths: config.additional_prompt_paths.clone(),
        no_skills: false,
        no_prompt_templates: false,
        no_context_files: false,
        system_prompt: config.custom_system_prompt.clone(),
        append_system_prompt: config.prompt_guidelines.clone(),
        ..Default::default()
    })?;

    let model = config
        .model
        .ok_or_else(|| anyhow::anyhow!("a resolved model is required"))?;
    let stream_fn = config
        .stream_fn
        .ok_or_else(|| anyhow::anyhow!("a provider stream_fn is required"))?;
    // `model.info` facts, captured before the model moves into the loop.
    let model_info = match config.model_info.clone() {
        Some(full) => super::turn_boundary::ModelInfo {
            id: full.id,
            provider: full.provider,
            input: full.input,
        },
        None => super::turn_boundary::ModelInfo {
            id: model.id.clone(),
            provider: model.provider.clone(),
            input: Vec::new(),
        },
    };
    let model_context_window = model.context_window;

    let session_id = wiring.session.lock().await.get_session_id().to_string();
    let mut handlers = wiring.handlers.clone();
    if let Some(extra) = config.extra_host_handlers.clone() {
        handlers.merge(extra);
    }
    // Per-session counters (MCP connector use, kernel boots, skills, RLM
    // child usage, feature outcomes) ride `agent session ended`; the seams
    // below count into them instead of emitting their own events.
    let session_counters = std::sync::Arc::new(super::telemetry::SessionCounters::default());
    // The counters gate from the very creation: the MCP and kernel setup
    // below starts counting (prewarm boots, connector use) long before
    // the first turn installs the session telemetry, so an off switch
    // must already be live — otherwise the pre-install window records
    // what a later enable would send.
    if let Some(telemetry_switch) = config
        .telemetry
        .as_ref()
        .and_then(|telemetry| telemetry.telemetry_enabled.as_ref())
    {
        session_counters.set_telemetry_enabled(telemetry_switch.enabled.clone());
    }
    if config.telemetry.is_some() {
        wiring
            .interaction
            .set_counters(std::sync::Arc::clone(&session_counters));
        // `agent_message.send` (registered by the embedding): telemetry counts the deliveries
        // the per-sender rate bucket or a receiver's wake budget refused (never a target or a body).
        let counters = std::sync::Arc::clone(&session_counters);
        super::agent_messaging::observe_agent_message_refusals(
            &mut handlers,
            std::sync::Arc::new(move |refusals| counters.note_agent_message_refusals(refusals)),
        );
        // `agent_message.wait`: telemetry counts how each wait ended (never a sender or a body).
        let counters = std::sync::Arc::clone(&session_counters);
        super::agent_messaging::observe_agent_message_waits(
            &mut handlers,
            std::sync::Arc::new(move |outcome| counters.note_agent_message_wait(outcome)),
        );
    }
    // The `mcp.*` host requests (config/refresh/begin_login) the kernel's
    // generic MCP registry sends while listing or calling generic servers.
    // Telemetry counts connector use (never the server name) when the
    // session is telemetry-enabled; set before the handlers register so
    // their closures capture the reporter.
    if config.telemetry.is_some() {
        let counters = std::sync::Arc::clone(&session_counters);
        mcp_manager
            .lock()
            .unwrap()
            .set_usage_report(Some(std::sync::Arc::new(
                move |action, _server| match action {
                    crate::mcp::LOGIN_ACTION => counters.note_kernel_mcp_login(Some(true)),
                    crate::mcp::LOGIN_FAILED_ACTION => counters.note_kernel_mcp_login(Some(false)),
                    crate::mcp::LOGIN_UNAVAILABLE_ACTION => counters.note_kernel_mcp_login(None),
                    _ => counters.note_mcp_connector_use(),
                },
            )));
    }
    // The registration takes the shared manager: the inventory handlers
    // serve live views per request.
    crate::mcp::McpManager::register_host_handlers(&mcp_manager, &mut handlers);
    // `web.search`: the kernel's `web` client and the websearch skill reach
    // the process-wide native engine; credentials resolve from the shared
    // auth store per search. Telemetry folds each outcome (never the query)
    // into the session counters.
    crate::web_search::WebSearchHost {
        engine: crate::web_search::WebSearchEngine::shared(),
        auth: mcp_manager.lock().unwrap().auth_storage_handle(),
        default_provider: web_search_provider,
        observer: config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |outcome: &_| counters.note_web_search(outcome))
                as crate::web_search::SearchObserver
        }),
    }
    .register(&mut handlers);
    // `web.read`: the kernel's `web.read(...)` reaches the process-wide
    // reader; telemetry folds each outcome (never the URL) into the counters.
    crate::web_read::WebReadHost {
        engine: crate::web_read::WebReadEngine::shared(),
        observer: config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |result: &_| {
                let (outcome, elapsed) = crate::web_read::ReadOutcome::of(result);
                counters.note_web_read(outcome, elapsed);
            }) as crate::web_read::ReadObserver
        }),
    }
    .register(&mut handlers);
    // `fs.grep`/`fs.glob`/`fs.find`: the kernel's native in-process search;
    // telemetry folds each call (never a pattern or path) into the counters.
    crate::fs_search::FsSearchHost {
        observer: config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_fs_search(fact))
                as crate::fs_search::SearchObserver
        }),
    }
    .register(&mut handlers);
    // `worktree.record`: the kernel `worktree` capability's report after each operation, kept in
    // the session's ledger (the session-end notice); telemetry counts each outcome (never a
    // name, a path, or a diff).
    let worktrees = std::sync::Arc::new(crate::worktree_host::WorktreeLedger::default());
    crate::worktree_host::register_worktree_host_handler(
        &mut handlers,
        std::sync::Arc::clone(&worktrees),
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_worktree(fact))
                as crate::worktree_host::WorktreeObserver
        }),
    );
    // `usage.get`: the session's spend (and the no-daemon quota fallback);
    // telemetry counts each read (never a number).
    crate::usage_host::register_usage_host_handlers(
        &mut handlers,
        &wiring.session,
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move || counters.note_usage_read())
                as crate::usage_host::UsageObserver
        }),
    );
    // `rizin.used`: one count per kernel `rz-*` call (never a target or a command).
    super::rizin_usage::register_rizin_usage(
        &mut handlers,
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_rizin(fact))
                as super::rizin_usage::RizinObserver
        }),
    );
    // `rlm.session.search`/`rlm.session.saved_messages`: read-only recall over this project's
    // saved sessions; telemetry folds each call (never a query or a session) into the counters.
    super::session_search::register(
        &mut handlers,
        &wiring.session,
        config
            .telemetry
            .is_some()
            .then(|| {
                let counters = std::sync::Arc::clone(&session_counters);
                std::sync::Arc::new(move |fact| counters.note_session_recall(fact))
                    as super::session_search::SessionRecallObserver
            })
            .as_ref(),
    );
    let turn_boundary = Arc::new(super::turn_boundary::TurnBoundaryRequests::new());
    // `rlm.reload` re-reads the skills, prompts, and context files; `settings.*`
    // reports the armed limits. Telemetry counts each (never a name or a value).
    let resource_reload = super::resource_reload::ResourceReload::new(
        &turn_boundary,
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |added| counters.note_repl_reload(added))
                as super::resource_reload::ReloadObserver
        }),
    );
    resource_reload.register(&mut handlers);
    // The cell yield window policy: none daemonless, else the user's setting,
    // else learned for this project. Telemetry counts each armed window's source.
    let cell_window = crate::kernel::cell_window::CellWindow::for_session(
        crate::kernel::cell_window::CellWindowConfig {
            daemonless: config.daemonless,
            configured_ms: cell_timeout_ms,
            agent_dir: &config.agent_dir,
            project_dir: &cwd,
            on_armed: config.telemetry.is_some().then(|| {
                let counters = std::sync::Arc::clone(&session_counters);
                std::sync::Arc::new(move |source| counters.note_cell_window(source))
                    as crate::kernel::cell_window::CellWindowObserver
            }),
        },
    );
    super::settings_view::register_settings_handlers(
        &mut handlers,
        &turn_boundary,
        super::settings_view::ArmedSettings {
            cell_window: cell_window.clone(),
            compaction_enabled,
            compaction_threshold_percent,
            compaction_keep_recent_tokens: compaction_settings
                .keep_recent_tokens
                .unwrap_or(super::compaction::DEFAULT_KEEP_RECENT_TOKENS),
            compaction_agent_callable: compaction_settings.agent_callable.unwrap_or(true),
            image_model,
            web_search_provider: serde_json::to_value(web_search_provider)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
            cache_retention: serde_json::to_value(cache_retention).unwrap_or_default(),
            rlm_depth: config.rlm_depth.unwrap_or(0),
            daemonless: config.daemonless,
        },
        super::settings_view::HostFallback {
            session_file: conversation_log.clone(),
            cwd: cwd.clone(),
            agent_dir: config.agent_dir.clone(),
        },
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move || counters.note_repl_settings_read())
                as super::settings_view::SettingsObserver
        }),
    );
    // `model.set_effort`: the opt-in thinking-level switch; telemetry counts
    // applied and refused switches (never the level).
    super::effort_switch::register_effort_handler(
        &mut handlers,
        &turn_boundary,
        super::effort_switch::EffortGate {
            cwd: cwd.clone(),
            agent_dir: config.agent_dir.clone(),
        },
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |applied| counters.note_effort_switch(applied))
                as super::effort_switch::EffortObserver
        }),
    );
    let _ = turn_boundary
        .refinement_settings
        .set(refinement_settings.reopen());
    turn_boundary.register_model_info_handler(&mut handlers, model_info.clone());
    let keep_recent_tokens = compaction_settings
        .keep_recent_tokens
        .unwrap_or(super::compaction::DEFAULT_KEEP_RECENT_TOKENS);
    if compaction_settings.agent_callable.unwrap_or(true) {
        turn_boundary.register_compact_handlers(&mut handlers, keep_recent_tokens);
    }
    // `rlm.decide`: one bounded decision by the session's model mid-cell; the decider binds
    // once the agent exists. Telemetry counts decisions, refusals at the bound, and failures.
    let decision_host = std::sync::Arc::new(super::decision::DecisionHost::new(
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_decision(fact))
                as super::decision::DecideObserver
        }),
    ));
    super::decision::register_decide_handler(&mut handlers, &decision_host);
    // `rlm.session.tool_result`: the recall path a thinned result's pointer names.
    super::context_thinning::register_tool_result_recall(&mut handlers, &wiring.session);
    // The daemon worker owns persistence outside the session manager, so
    // its conversation-log path implies the same artifact tree.
    let session_artifact_dir: Option<PathBuf> = wiring
        .session
        .lock()
        .await
        .get_session_artifact_dir()
        .or_else(|| {
            config
                .conversation_log_path
                .as_deref()
                .and_then(super::harness_digest::session_artifact_dir_for_log)
        });
    // `fs.read`/`fs.edit`: the kernel's anchored (hashline) reads and edits;
    // telemetry folds each call (never a path or content) into the counters.
    // Whole-file re-reads of files the model has seen shrink to "unchanged" or
    // a diff, with the full view in the spill store; the context probe binds
    // once the agent exists.
    let seen_files = std::sync::Arc::new(crate::fs_edit::SeenFiles::new(
        session_artifact_dir
            .as_deref()
            .map(crate::kernel::spill::SpillStore::for_session_dir),
        thinning_settings.keep_recent_rounds.max(1) as u64,
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_reread(fact))
                as crate::fs_edit::RereadObserver
        }),
    ));
    // A read file's likely next reads page out to the spill store (persistent
    // sessions only), from the project's one symbol index; telemetry counts the
    // handles issued and used.
    let prefetch = session_artifact_dir.as_deref().map(|session_dir| {
        crate::prefetch::Prefetcher::new(
            &cwd,
            Some(&config.agent_dir),
            crate::symbol_index::SymbolIndex::for_project(&cwd, Some(&config.agent_dir)),
            crate::kernel::spill::SpillStore::for_session_dir(session_dir),
            config.telemetry.is_some().then(|| {
                let counters = std::sync::Arc::clone(&session_counters);
                std::sync::Arc::new(move |fact| counters.note_prefetch(fact))
                    as crate::prefetch::PrefetchObserver
            }),
        )
    });
    crate::fs_edit::FsEditHost {
        observer: config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_fs_edit(fact))
                as crate::fs_edit::EditObserver
        }),
        seen: Some(std::sync::Arc::clone(&seen_files)),
        prefetch: prefetch.clone(),
    }
    .register(&mut handlers);
    // `ctx.*`: page a cut cell output back in by its handle, from the
    // session's spill store (an ephemeral session answers `unavailable`).
    let spill_store = session_artifact_dir
        .as_deref()
        .map(crate::kernel::spill::SpillStore::for_session_dir);
    // The cell output cap follows the context the session has left: the
    // thinning transform publishes each request's fill, the executor reads it.
    let output_budget = crate::kernel::spill::OutputBudget::default();
    // Telemetry counts spills and reads (never a handle, a pattern, or text).
    if let (Some(store), true) = (&spill_store, config.telemetry.is_some()) {
        let counters = std::sync::Arc::clone(&session_counters);
        store.set_observer(std::sync::Arc::new(move |fact| counters.note_spill(fact)));
    }
    // Previews collapse this project's learned noise patterns: the session
    // previews with the set known at its start (a short bounded read), and
    // what it learns is saved for the next session.
    if let Some(store) = &spill_store {
        let mut learner =
            crate::kernel::spill::noise::NoiseLearner::for_project(&config.agent_dir, &cwd);
        learner.load(crate::kernel::spill::noise::LOAD_BUDGET).await;
        store.set_previewer(
            learner,
            config.telemetry.is_some().then(|| {
                let counters = std::sync::Arc::clone(&session_counters);
                std::sync::Arc::new(move |fact| counters.note_preview(fact))
                    as crate::kernel::spill::PreviewObserver
            }),
        );
    }
    // Telemetry counts each `ctx.*` read; a read of a prefetched handle counts as its use.
    let ctx_counters = config
        .telemetry
        .is_some()
        .then(|| std::sync::Arc::clone(&session_counters));
    let ctx_observer = (ctx_counters.is_some() || prefetch.is_some()).then(|| {
        std::sync::Arc::new(move |read, id: &str| {
            if let Some(counters) = &ctx_counters {
                counters.note_ctx_read(read);
                counters.note_preview_followup();
            }
            if let Some(prefetch) = &prefetch {
                prefetch.note_handle_read(id);
            }
        }) as crate::kernel::spill::host::CtxObserver
    });
    crate::kernel::spill::host::register_ctx_handlers(
        &mut handlers,
        spill_store.as_ref(),
        ctx_observer.as_ref(),
    );
    // `fs.symbol`: the project's symbol index, built in the background from
    // its first query on; a definition's source pages out to the spill store.
    crate::symbol_index::register_symbol_handler(
        &mut handlers,
        crate::symbol_index::SymbolIndex::for_project(&cwd, Some(&config.agent_dir)),
        spill_store.clone(),
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_symbol_query(fact))
                as crate::symbol_index::SymbolObserver
        }),
    );
    // `/better`: its helper binds to the agent once it exists.
    let better = std::sync::Arc::new(super::better::BetterHost::new(
        cwd.clone(),
        Some(&config.agent_dir),
        config
            .telemetry
            .is_some()
            .then(|| std::sync::Arc::clone(&session_counters)),
        spill_store.clone(),
        std::sync::Arc::new(crate::autonomous::ShellGateRunner::new(cwd.clone())),
    ));
    // `hypotheses.record`: each finished `rlm.hypotheses` run (telemetry counts).
    crate::kernel::hypotheses::register_hypotheses_handler(
        &mut handlers,
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |run| counters.note_hypotheses(run))
                as crate::kernel::hypotheses::HypothesesObserver
        }),
    );
    // `command_cache.fact`: read-only cache lookups (telemetry counts).
    crate::kernel::command_cache::register_command_cache_handler(
        &mut handlers,
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |lookup| counters.note_cache_lookup(lookup))
                as crate::kernel::command_cache::CacheObserver
        }),
    );
    super::better::register_better_handler(&mut handlers, &better);
    // A delegation brief's depth follows the levels its subagent may still delegate.
    if let Some(depth) = config
        .rlm_subagent_host
        .as_ref()
        .and_then(|host| host.delegation_depth())
    {
        better.bind_delegation_depth(depth);
    }
    // `tests.summary`: an awaited test run's output and failures get handles
    // in the same store; telemetry counts each summarized run.
    crate::kernel::test_runs::register_test_summary_handler(
        &mut handlers,
        spill_store.as_ref(),
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_test_summary(fact))
                as crate::kernel::test_runs::TestRunObserver
        }),
    );
    // The project card's detail pages out to the same store.
    let card_spill = session_artifact_dir
        .as_deref()
        .map(crate::kernel::spill::SpillStore::for_session_dir);
    // `edit_check.fact`: how each check after an edit went (telemetry counts).
    crate::kernel::edit_checks::register_edit_check_handler(
        &mut handlers,
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |outcome| counters.note_edit_check(outcome))
                as crate::kernel::edit_checks::EditCheckObserver
        }),
    );
    let local_harness_dir =
        crate::refinement::get_local_harness_state_dir(session_artifact_dir.as_deref());
    // The only sessions whose `refine.*` host requests register and the
    // compact-trigger auto-refine may run for.
    let auto_refine_allowed = config.rlm_depth.unwrap_or(0) == 0 && local_harness_dir.is_some();
    if auto_refine_allowed {
        turn_boundary.register_refine_handlers(&mut handlers);
    }
    // A kernel start can settle before the AgentSession exists, so the rows it
    // has for the next turn park in a mailbox the session adopts once constructed.
    let boot_notice_rows = std::sync::Arc::new(std::sync::Mutex::new(Vec::<
        pa_types::session::CustomMessage,
    >::new()));
    // The daemonless bash-completion lane (TS registers the handlers in
    // every mode): the in-process sinks steer the notice row onto this
    // session's agent and fire the embedding's wake once the agent
    // exists (the slot binds below).
    let bash_notice_agent = super::bash_notices::BashNoticeAgentSlot::default();
    if let Some(wake) = config.bash_notice_wake.clone() {
        super::bash_notices::register_in_process_kernel_notice_handlers(
            &wake,
            &bash_notice_agent,
            &mut handlers,
        );
        // A failed acceptance-check round steers through the same in-process lane.
        better.bind_notice_sink(super::bash_notices::in_process_notice_sink(
            &wake,
            &bash_notice_agent,
        ));
    }
    // One model-visible skill list, computed now that the host-handler
    // registry is final (every registration, the refine handlers, and
    // the in-process bash notices are in): a controller-backed bundled
    // skill whose host request never registered drops out (TS
    // `_modelVisibleSkills`).
    let model_visible_skills =
        super::runtime_wiring::model_visible_skills(resources.skills.clone(), &handlers);
    let hidden_skills: Vec<String> = resources
        .skills
        .iter()
        .filter(|skill| !model_visible_skills.contains(skill))
        .map(|skill| skill.name.clone())
        .collect();
    let websearch_env =
        super::runtime_wiring::websearch_env(&resources.skills, web_search_provider);
    // The session's kernel gets the host handlers (its file tools count
    // through them), shell settings, directories and cell yield window, and
    // the telemetry counters behind its background cells (counts only).
    // A failed cell or awaited command gets its error locations' source
    // lines from this workspace; telemetry counts each enrichment.
    let error_context = crate::kernel::error_context::ErrorContext::for_workspace(
        cwd.clone(),
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |fact| counters.note_error_enrichment(fact))
                as crate::kernel::error_context::ErrorContextObserver
        }),
    );
    let scheme_options = crate::kernel::scheme::SchemeKernelOptions {
        error_context,
        host_handlers: handlers.clone(),
        cwd: cwd.clone(),
        shell_path: kernel_shell_path,
        command_prefix: kernel_command_prefix,
        agent_dir: Some(config.agent_dir.clone()),
        env: websearch_env,
        session_id: Some(session_id.clone()),
        artifact_dir: session_artifact_dir,
        cell_window: cell_window.clone(),
        on_background_cell: Some({
            let counters = config
                .telemetry
                .is_some()
                .then(|| std::sync::Arc::clone(&session_counters));
            let cell_window = cell_window.clone();
            std::sync::Arc::new(move |fact| {
                use crate::kernel::shared::BackgroundCellFact;
                if let Some(counters) = &counters {
                    counters.note_background_cell(fact);
                }
                match fact {
                    BackgroundCellFact::EndedOk {
                        ran_for: Some(ran_for),
                    }
                    | BackgroundCellFact::EndedWithError {
                        ran_for: Some(ran_for),
                    } => cell_window.observe(ran_for),
                    BackgroundCellFact::Backgrounded
                    | BackgroundCellFact::EndedOk { ran_for: None }
                    | BackgroundCellFact::EndedWithError { ran_for: None }
                    | BackgroundCellFact::Interrupted => {}
                }
            }) as crate::kernel::shared::BackgroundCellObserver
        }),
        // The skills' Scheme source loads at kernel start and after each
        // reload; a skill that fails to load is named on the next turn.
        // Only the skills the model sees: one whose host request never
        // registered in this embedding defines nothing in the kernel.
        skill_sources: Some({
            let reload = Arc::clone(&resource_reload);
            let wired = handlers;
            Arc::new(move || {
                crate::kernel::scheme::skills::SkillSource::from_skills(
                    &super::runtime_wiring::model_visible_skills(reload.skills(), &wired),
                )
            })
        }),
        on_unavailable_skills: Some({
            let boot_notice_rows = Arc::clone(&boot_notice_rows);
            Arc::new(
                move |errors: &crate::kernel::scheme::skills::UnavailableSchemeSkills| {
                    boot_notice_rows
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(crate::kernel::scheme::skills::notice_message(errors));
                },
            )
        }),
        on_background_work_settled: config.on_background_work_settled.clone(),
        // A kernel released while the session goes on took its definitions
        // with it: the model is told ahead of its next turn.
        on_state_lost: Some({
            let boot_notice_rows = Arc::clone(&boot_notice_rows);
            Arc::new(move || {
                boot_notice_rows
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(super::state_restore_notice::notice_message());
            })
        }),
        output_budget: output_budget.clone(),
        // Kernel starts (count, failures, the slowest) count into the session
        // counters. Every start is a cold one: an engine never revives state.
        on_started: config.telemetry.as_ref().map(|_| {
            let counters = std::sync::Arc::clone(&session_counters);
            Arc::new(move |start: crate::kernel::scheme::KernelStart| {
                counters.note_kernel_bootstrap(
                    /*cold*/ true,
                    start.succeeded,
                    start.took.as_millis() as u64,
                );
            }) as crate::kernel::scheme::KernelStartObserver
        }),
    };
    // The session's one kernel: the in-process Scheme engine. Its spare is
    // built here in the background, so the first cell does not pay for it.
    crate::kernel::scheme::prewarm_scheme_kernel();
    let provisioner =
        Arc::new(crate::kernel::scheme::SchemeKernelProvisioner::with_options(scheme_options));
    let mut tools = config.tools.clone();
    if !tools.iter().any(|tool| tool.name() == KERNEL_TOOL_NAME) {
        tools.push(Arc::new(
            crate::session_engine::tool_bridge::ToolDefinitionBridge::new(
                crate::tools::scheme::create_scheme_tool_definition(
                    crate::tools::kernel_cell::KernelToolOptions {
                        provisioner: Arc::clone(&provisioner)
                            as Arc<dyn crate::tools::kernel_cell::KernelProvisioner>,
                        on_late_sent_agent_message: config.on_late_sent_agent_message.clone(),
                    },
                ),
            ),
        ));
    }
    let active_tool_names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
    let has_kernel_tool = active_tool_names
        .iter()
        .any(|name| name == KERNEL_TOOL_NAME);

    // A resumed session starts on a new engine: what its earlier cells
    // defined is gone, and the model is told once, ahead of its next turn.
    if has_kernel_tool {
        let earlier = wiring.session.lock().await.active_context().messages;
        if let Some(row) = super::state_restore_notice::notice_for_resumed(&earlier) {
            boot_notice_rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(row);
        }
    }

    // A subagent starts its kernel early like a top-level session: its first
    // model call almost always ends in a cell, so the start overlaps that
    // call instead of following it.
    if config.prewarm_kernel.unwrap_or(false) && has_kernel_tool {
        provisioner.prewarm();
        // The MCP settle rides the same background posture (the
        // parallel-startup rule: nothing user-visible waits on the MCP
        // spawn/settle; the join point is first use): the configured generic
        // servers open in the background through the bounded listing the
        // connections view uses, so tool discovery is warm by the first
        // turn. A tool call or listing that arrives first is never raced to
        // an error: the kernel's registry serializes per-server opens and
        // then reuses the open connection, so the earliest user of a server
        // joins the settle's in-flight open and succeeds. The settle holds
        // the kernel weakly: a session dropped meanwhile is not kept alive.
        let settle_servers = generic_mcp_servers.clone();
        let settle_provisioner = Arc::downgrade(&provisioner);
        tokio::spawn(async move {
            let Some(provisioner) = settle_provisioner.upgrade() else {
                return;
            };
            provisioner
                .mcp_tool_listing(&settle_servers, MCP_SETTLE_PER_SERVER_TIMEOUT_MS)
                .await;
        });
    }

    let prompt_model_selector = format!("{}/{}", model_info.provider, model_info.id);
    let prompt_vision_capable = Some(model_info.input.contains(&pa_types::ai::ModelInput::Image));
    let model_prompts = crate::prompts::model_prompts::load_model_prompts(
        Some(&prompt_model_selector),
        &config.agent_dir,
    );
    if !model_prompts.errors.is_empty() {
        boot_notice_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(crate::prompts::model_prompts::model_prompt_error_message(
                &model_prompts.errors,
            ));
    }
    // The project card the previous session stored (a short bounded read,
    // never the derivation); a background refresh stores the next one.
    let project_card = crate::project_card::ProjectCard::for_project(&cwd, &config.agent_dir);
    project_card.load(crate::project_card::LOAD_BUDGET).await;
    project_card.refresh();
    let project_card_text = project_card.render(card_spill.as_ref());
    let system_prompt_breakdown = crate::prompts::system_prompt::system_prompt_breakdown(
        &crate::prompts::system_prompt::BuildSystemPromptOptions {
            custom_prompt: resources.system_prompt.clone(),
            model_prompt_extras: model_prompts.extras.as_deref(),
            vision_capable: prompt_vision_capable,
            cwd: cwd.display().to_string(),
            messages_path: conversation_log.clone(),
            context_files: resources
                .agents_files
                .iter()
                .map(|file| (file.path.display().to_string(), file.content.clone()))
                .collect(),
            skills: model_visible_skills.clone(),
            selected_tools: Some(
                active_tool_names
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            ),
            allow_recursion: config.allow_recursion,
            // A spawned child's prompt must read "depth: N (not root)" with
            // the child-agent reply doctrine, never the root identity.
            rlm_depth: config.rlm_depth,
            factory_child: config.factory_child,
            daemonless: config.daemonless,
            can_spawn: config.rlm_subagent_host.is_some(),
            generic_mcp_servers: generic_mcp_servers.clone(),
            project_card: project_card_text.clone(),
            append_system_prompt: Some(resources.append_system_prompt.join("\n\n")),
            ..Default::default()
        },
    );
    resource_reload.seed(
        super::resource_reload::ReloadInputs {
            cwd: cwd.clone(),
            agent_dir: config.agent_dir.clone(),
            extra_builtin_skill_overrides: reload_skill_overrides,
            additional_skill_paths: config.additional_skill_paths.clone(),
            additional_prompt_paths: config.additional_prompt_paths.clone(),
            custom_system_prompt: config.custom_system_prompt.clone(),
            prompt_guidelines: config.prompt_guidelines.clone(),
            hidden_skills,
            prompt: super::resource_reload::PromptInputs {
                model_prompt_extras: model_prompts.extras.clone(),
                messages_path: conversation_log.clone(),
                selected_tools: active_tool_names.clone(),
                allow_recursion: config.allow_recursion,
                rlm_depth: config.rlm_depth,
                factory_child: config.factory_child,
                daemonless: config.daemonless,
                can_spawn: config.rlm_subagent_host.is_some(),
                generic_mcp_servers,
                project_card: project_card_text,
            },
        },
        resources.skills.clone(),
        resources.prompts.clone(),
        system_prompt_breakdown.assembled.clone(),
    );
    // The static layers' byte span is the cache-stable prefix the cache
    // seam marks as its own block (shared by every session and subagent on
    // the same layers and model prompts).
    let system_prompt_static_len = system_prompt_breakdown.cached_prefix_len;
    let static_system_prefix =
        system_prompt_breakdown.assembled[..system_prompt_static_len].to_string();
    let system_prompt = system_prompt_breakdown.assembled;

    // Captured before the digest context moves the local harness dir and
    // the loop wiring moves the session model: the factory host bridge's
    // config (the #3184 capture pattern).
    let factory_local_harness_dir = local_harness_dir.clone();
    let factory_session_model = Some(model.clone());
    // Harness digest inputs: global state from the agent dir, local state
    // from the session artifacts (or the daemon-owned conversation log), and
    // the interfaces the digest may reference.
    let digest_context = super::harness_digest::HarnessDigestContext {
        global_dir: crate::refinement::get_global_harness_state_dir(&config.agent_dir),
        local_dir: local_harness_dir,
        include_kernel_tool: has_kernel_tool,
        include_shell_examples: active_tool_names.iter().any(|name| name == "bash"),
        include_refine: model_visible_skills.iter().any(|skill| {
            !skill.disable_model_invocation
                && skill.name == crate::prompts::system_prompt::REFINE_SKILL_NAME
        }),
    };
    let (existing_messages, has_thinking_entry, has_service_tier_entry) = {
        let session = wiring.session.lock().await;
        let messages = super::compact_session::rebuilt_context_after_compaction(&session);
        let messages = if super::refinement_gate::enabled(Some(&refinement_settings)) {
            messages
        } else {
            super::refinement_gate::without_refinements(messages)
        };
        let has_thinking_entry = session.has_thinking_level();
        let has_service_tier_entry = session.has_service_tier();
        (messages, has_thinking_entry, has_service_tier_entry)
    };
    let thinking_level = config.thinking_level.unwrap_or(ThinkingLevel::Off);
    {
        let mut session = wiring.session.lock().await;
        if existing_messages.is_empty() {
            session.append_model_change(&model.provider, &model.id)?;
            session.append_thinking_level_change(&format!("{thinking_level:?}").to_lowercase())?;
        } else if !has_thinking_entry {
            session.append_thinking_level_change(&format!("{thinking_level:?}").to_lowercase())?;
        }
        if existing_messages.is_empty() || !has_service_tier_entry {
            session.append_service_tier_change(Some(service_tier_preference))?;
        }
    }
    // Session entries cross through the shared wire shape (same conversion
    // the compaction rebuild uses).
    let initial_messages = if existing_messages.is_empty() {
        None
    } else {
        Some(
            existing_messages
                .into_iter()
                .filter_map(|message| {
                    let value = serde_json::to_value(&message).ok()?;
                    serde_json::from_value(value).ok()
                })
                .collect(),
        )
    };

    // One wiring per session owns the flag probe, the JSONL log, and the prompt-build
    // correlation state; the wrappers pass straight through while the flag is off.
    let request_timing_wiring = std::sync::Arc::new(
        super::request_timing::RequestTimingWiring::new(
            std::sync::Arc::new(move || {
                super::request_timing::is_request_timing_enabled(request_timing_settings)
            }),
            super::request_timing::RequestTimingLog::new(&config.agent_dir),
        )
        .with_payload_capture(super::request_timing::RequestPayloadCapture::new(
            &config.agent_dir,
        )),
    );
    // Semantic edges (TS `semantic-edges.ts`): the recorder opens this
    // session's request-id ledger, and its stream wrapper goes OUTERMOST
    // over the timing-instrumented fn (TS `sdk.ts` instruments first, the
    // `AgentSession` constructor wraps semantic edges over it). The side
    // question keeps the pre-semantic fn, so its calls carry no id.
    // Cache-warming refreshes go out on the raw provider fn: below request
    // timing and the semantic ledger, so a refresh is neither a timed turn
    // nor an edge.
    let warm_stream_fn = std::sync::Arc::clone(&stream_fn);
    let timing_stream_fn = super::request_timing::instrument_stream_fn(
        std::sync::Arc::clone(&request_timing_wiring),
        super::loop_guard::guard_stream_fn(loop_guard_level, stream_fn),
    );
    // The repeated-tool-call nudge counts into the session telemetry once
    // it is installed (after the agent exists).
    let repeat_nudge_telemetry = std::sync::Arc::new(std::sync::OnceLock::new());
    let side_question_stream_fn = std::sync::Arc::clone(&timing_stream_fn);
    let semantic_recorder = config.semantic_edges.take().map(|identity| {
        std::sync::Arc::new(super::semantic_edges::SemanticEdgeRecorder::open(identity))
    });
    let agent_stream_fn = match &semantic_recorder {
        Some(recorder) => {
            super::semantic_edges::wrap_stream_fn(std::sync::Arc::clone(recorder), timing_stream_fn)
        }
        None => timing_stream_fn,
    };
    // The thinning transform's model-window probe binds its agent late (the
    // transform is built into the agent); weak, so the seam pins nothing.
    let thinning_agent: std::sync::Arc<std::sync::OnceLock<std::sync::Weak<Agent>>> =
        std::sync::Arc::default();
    // The request view thins aged tool results under context pressure (the
    // stored transcript stays whole). One transform serves the loop and the
    // goal judge, so a judge fork sends the view the loop's cache entry holds.
    let thinning = super::context_thinning::thinning_transform(
        thinning_settings,
        {
            // The window of the model the request goes to: a run's routed
            // override, else the session model (in-memory reads; the slot
            // binds once the agent exists).
            let slot = std::sync::Arc::clone(&thinning_agent);
            std::sync::Arc::new(move || {
                let slot = std::sync::Arc::clone(&slot);
                Box::pin(async move {
                    let Some(agent) = slot.get().and_then(std::sync::Weak::upgrade) else {
                        return model_context_window;
                    };
                    match agent.model_override() {
                        Some(routed) => routed.model.context_window,
                        None => agent.state().await.model.context_window,
                    }
                })
            })
        },
        // A persistent session pages thinned results out to its spill store,
        // so the pointer is a handle `ctx.*` reads even after compaction.
        match spill_store.clone() {
            Some(store) => super::context_thinning::ThinnedRecall::Spill(store),
            None => super::context_thinning::ThinnedRecall::ToolResult,
        },
        output_budget,
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |stats: &_| counters.note_context_thinning(stats))
                as super::context_thinning::ThinningObserver
        }),
    );
    // The goal judge forks the session's requests outside the cache seam;
    // it takes the same cache hints so its system blocks match the loop's.
    let judge_stream_fn = super::cache_warming::with_cache_hints(
        cache_retention,
        static_system_prefix.clone(),
        std::sync::Arc::clone(&side_question_stream_fn),
    );
    // The prompt-cache seam goes outermost: it disarms the session's cache
    // warmer before any real request is dispatched. Its pending-work probe
    // holds the kernel and the subagent host weakly (the engine owns both).
    let agent_stream_fn = super::cache_warming::wrap_stream_fn(
        super::cache_warming::CacheWarmingConfig {
            mode: cache_warming,
            retention: cache_retention,
            static_prefix: static_system_prefix,
            role: if config.rlm_depth.unwrap_or(0) == 0 {
                super::cache_warming::SessionRole::Root
            } else {
                super::cache_warming::SessionRole::Subagent
            },
            pending_work: {
                let provisioner = Arc::downgrade(&provisioner);
                let subagent_host = config.rlm_subagent_host.as_ref().map(Arc::downgrade);
                Arc::new(move || {
                    let provisioner = provisioner.clone();
                    let subagent_host = subagent_host.clone();
                    Box::pin(async move {
                        let background_bash = provisioner
                            .upgrade()
                            .is_some_and(|provisioner| provisioner.has_background_work());
                        if background_bash {
                            return true;
                        }
                        let Some(host) = subagent_host.and_then(|host| host.upgrade()) else {
                            return false;
                        };
                        host.list_subagents().await.is_ok_and(|children| {
                            children.iter().any(|child| child.status == "running")
                        })
                    })
                })
            },
            counters: std::sync::Arc::clone(&session_counters),
            warm_stream_fn,
        },
        agent_stream_fn,
    );
    let agent = Agent::new(AgentOptions {
        // Every request in the session carries this identity: providers key prompt caching
        // (`prompt_cache_key`), session headers, and WebSocket/`previous_response_id` reuse on it
        // (TS `sdk.ts` passes `sessionManager.getSessionId()`).
        session_id: Some(session_id),
        initial_state: AgentInitialState {
            system_prompt: Some(system_prompt.clone()),
            model: Some(model),
            thinking_level: Some(thinking_level),
            tools: Some(tools),
            messages: initial_messages,
        },
        stream_fn: Some(agent_stream_fn),
        // The session conversion rules apply at the loop's LLM boundary
        // (TS `convertToLlm`): bookkeeping custom rows drop, everything
        // else (the harness digest included) becomes a user turn.
        convert_to_llm: Some(super::request_timing::instrument_convert_to_llm(
            std::sync::Arc::clone(&request_timing_wiring),
            super::refinement_gate::convert_to_llm(refinement_settings.reopen()),
        )),
        // The request view thins aged tool results under context pressure
        // (the stored transcript stays whole); the instrumented seam marks
        // the turn's dispatch moment around it.
        transform_context: Some(super::request_timing::instrument_transform_context(
            std::sync::Arc::clone(&request_timing_wiring),
            std::sync::Arc::clone(&thinning),
        )),
        should_stop_after_turn: {
            let steering = config.queued_steering_probe.take();
            let boundary = config.should_stop_after_turn.take();
            Some(std::sync::Arc::new(move |context| {
                let steering = steering.clone();
                let boundary = boundary.clone();
                Box::pin(async move {
                    if steering.as_ref().is_some_and(|probe| probe()) {
                        return Ok(true);
                    }
                    match boundary {
                        Some(hook) => hook(context).await,
                        None => Ok(false),
                    }
                })
            }))
        },
        should_stop_before_turn: config.queued_steering_probe.clone(),
        after_tool_call: (loop_guard_level != Some(pa_agent::types::LoopGuardLevel::Off)).then(
            || super::loop_guard::repeat_call_nudge(std::sync::Arc::clone(&repeat_nudge_telemetry)),
        ),
        steering_mode: config.steering_mode,
        follow_up_mode: config.follow_up_mode,
        ..Default::default()
    });

    let agent = Arc::new(agent);
    let _ = thinning_agent.set(Arc::downgrade(&agent));
    // The in-process bash-notice sinks bind their agent late: their
    // handlers registered before the agent existed.
    *bash_notice_agent
        .lock()
        .expect("bash notice agent slot lock") = Some(Arc::downgrade(&agent));
    // TS `_startRlmChildRun`'s spawn anchor: the `rlm.spawn` host handler
    // names the parent's in-flight turn through this weak seam (the
    // bridge is built before the agent exists, and a strong edge would
    // cycle the bridge -> agent -> kernel -> bridge graph).
    if let Some(recorder) = &semantic_recorder {
        let _ = wiring
            .rlm
            .semantic_spawn
            .set(super::rlm_host::SemanticSpawnAnchor {
                agent: Arc::downgrade(&agent),
                recorder: std::sync::Arc::clone(recorder),
            });
    }
    // Goal verification: facts count into the session counters, and `verify="judge"` forks this
    // agent's conversation (pre-semantic stream fn with the session's cache hints, the loop's own
    // thinning and conversion) so the judge reads the main loop's cached prefix. Weak agent: the
    // kernel's handlers reach the judge.
    let _ = wiring
        .runtime
        .goal_telemetry()
        .set(std::sync::Arc::clone(&session_counters));
    let _ = wiring
        .rlm
        .telemetry
        .set(std::sync::Arc::clone(&session_counters));
    wiring
        .runtime
        .goal_verifier()
        .bind_judge(super::goal_judge::GoalJudge::new(
            Arc::downgrade(&agent),
            judge_stream_fn,
            thinning,
            super::refinement_gate::convert_to_llm(refinement_settings.reopen()),
            wiring.rlm_usage.store(),
        ));
    // The seen-files probe: the assistant turns so far and the newest compaction, read from
    // the live context (weakly: the kernel's handlers hold the probe).
    let probe_agent = Arc::downgrade(&agent);
    seen_files.bind(std::sync::Arc::new(move || {
        let agent = probe_agent.clone();
        Box::pin(async move {
            let Some(agent) = agent.upgrade() else {
                return crate::fs_edit::ContextMark {
                    turn: 0,
                    compacted_at_ms: Some(u64::MAX),
                };
            };
            let messages = agent.state().await.messages;
            crate::fs_edit::ContextMark {
                turn: messages
                    .iter()
                    .filter(|message| {
                        matches!(
                            message,
                            pa_agent::types::AgentMessage::Standard(
                                pa_agent::types::Message::Assistant(_)
                            )
                        )
                    })
                    .count() as u64,
                compacted_at_ms: messages
                    .iter()
                    .filter_map(|message| match message {
                        pa_agent::types::AgentMessage::Custom(custom)
                            if custom.role == "compactionSummary" =>
                        {
                            custom.payload["timestamp"].as_u64()
                        }
                        pa_agent::types::AgentMessage::Custom(_)
                        | pa_agent::types::AgentMessage::Standard(_) => None,
                    })
                    .max(),
            }
        })
    }));
    decision_host.bind(super::decision::Decider::new(
        Arc::downgrade(&agent),
        std::sync::Arc::clone(&side_question_stream_fn),
        wiring.rlm_usage.store(),
    ));
    let telemetry_agent = std::sync::Arc::clone(&agent);
    let mut session = AgentSession::from_session_arc(
        agent.clone(),
        wiring.session.clone(),
        resources.prompts.clone(),
        super::refinement_gate::enabled(Some(&refinement_settings))
            .then_some(digest_context.clone()),
    )
    .await?;
    // Receipts that landed after their cell's tool result persist as
    // custom entries; the build patches them onto the rebuilt tool
    // results (TS `_restoreLateIpythonSentAgentMessages`).
    super::late_sent_messages::restore(&wiring.session, &agent).await;
    // Receipts that record while their cell's row has not landed yet
    // apply as the row lands (TS `_applyLateIpythonSentAgentMessages`):
    // the landing seam runs under the same lock as the state push, so
    // the live row and the persisted one both carry the receipt.
    agent.set_message_end_transform(Some(super::late_sent_messages::landing_transform(
        &wiring.session,
    )));
    session.set_auto_refine(auto_refine_allowed, auto_refine_gates);
    session.set_agent_dir(config.agent_dir.clone());
    session.arm_billing_stamp(
        config.agent_dir.clone(),
        config.telemetry.is_some().then(|| {
            let counters = std::sync::Arc::clone(&session_counters);
            std::sync::Arc::new(move |mode| counters.note_billing_mode(mode))
                as super::billing::BillingObserver
        }),
    );
    session.set_usage_store(wiring.rlm_usage.store());
    // Keep the context descriptor even while disabled, so re-enabling can load it.
    session.harness_digest = Some(digest_context);
    session.refinement_settings = Some(refinement_settings);
    // A disabled startup has never delivered its filtered digest. Defer that
    // first delivery without re-arming digests already consumed in a live session.
    if !session.harness_refinements_enabled() {
        session
            .digest_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    // Every compaction path reads the session's resolved compaction
    // settings (TS `getCompactionSettings`): `/compact` matches the
    // `compact.*` turn-boundary tool's `keepRecentTokens`/`reserveTokens`.
    session.set_compaction_settings(crate::session_engine::compaction::CompactionSettings {
        threshold_percent: compaction_threshold_percent,
        enabled: compaction_settings.enabled.unwrap_or(true),
        reserve_tokens: compaction_settings
            .reserve_tokens
            .unwrap_or(crate::session_engine::compaction::DEFAULT_RESERVE_TOKENS),
        keep_recent_tokens: compaction_settings
            .keep_recent_tokens
            .unwrap_or(crate::session_engine::compaction::DEFAULT_KEEP_RECENT_TOKENS),
    });
    // Summarizer passes resolve their model through the `auxiliaryModel` setting
    // with the session model as fallback, so one-off prompts stay off the prompt-cache prefix.
    session.set_auxiliary_model_context(
        crate::session_engine::auxiliary_model::AuxiliaryModelContext {
            cwd,
            agent_dir: config.agent_dir.clone(),
        },
    );
    // The probe holds a WEAK reference: the engine owns the provisioner,
    // and a strong edge here would loop the graph.
    session.set_kernel_state_probe(Some(std::sync::Arc::new(std::sync::Arc::downgrade(
        &provisioner,
    ))));
    // The admission path's `/skill:` expansion reads the FULL loader
    // inventory (TS `_expandSkillCommand` expands from the loader); the
    // model-visible list only shapes the prompt, pre-import, and counts.
    session.set_skills(resources.skills.clone());
    session.set_image_model_router(config.image_model_router.clone());
    // The semantic-edge handoff: the daemon's child registry and retry
    // park read the recorder; the side question keeps the pre-semantic
    // fn so its calls carry no id.
    session.set_semantic_edges(semantic_recorder);
    // The helper rides the side question's pre-semantic fn for the same reason.
    better.bind(
        &agent,
        std::sync::Arc::clone(&side_question_stream_fn),
        session.model_usage_recorder(),
    );
    // Acceptance checks a prompt armed run in the background when its runs end.
    better.watch_turn_ends(&agent).await;
    session.set_side_question_stream_fn(side_question_stream_fn);
    // The armed image route never outlives the run that armed it (TS
    // `_clearModelOverrideWhenIdle`: the override drops once the turn is
    // idle, so a picker switch between turns is live immediately — the
    // settle's still-routed guard leaves the switched slot). The settle
    // here is idempotent: an un-armed episode's swap restores the slot
    // it already holds, and the next admission's own settle re-reads
    // fresh state either way.
    if let Some(router) = config.image_model_router.clone() {
        // A weak agent reference: the listener lives ON the agent, so a
        // strong edge would cycle and pin a dropped session's agent.
        let agent_at_end = std::sync::Arc::downgrade(&agent);
        agent
            .subscribe(move |event, _signal| {
                let router = router.clone();
                let agent_at_end = agent_at_end.clone();
                Box::pin(async move {
                    if matches!(event, pa_agent::types::AgentEvent::AgentEnd { .. }) {
                        (router.swap_target)(None);
                        // A leftover override would leak into a later
                        // `continue_run`'s loop config.
                        if let Some(agent) = agent_at_end.upgrade() {
                            agent.set_model_override(None);
                        }
                    }
                    Ok(())
                })
            })
            .await;
    }
    // Rows parked by a boot that settled mid-build merge in; later boots
    // push straight into the live session's queue.
    session.adopt_next_turn_rows(boot_notice_rows);

    // `refine.status` answers from the session's own flight counter: the
    // same Arc every refinement run holds its flight guard on.
    turn_boundary.bind_refinements_in_flight(session.refinements_in_flight.clone());
    turn_boundary.bind(super::turn_boundary::TurnBoundaryRuntime {
        agent,
        session: wiring.session.clone(),
        context_window: (model_context_window > 0).then_some(model_context_window),
        model_info,
    });

    // Installed only for depth-0 sessions: subagents never double-report.
    let telemetry = match (config.telemetry.take(), config.rlm_depth.unwrap_or(0)) {
        (Some(wiring), 0) => {
            let skill_counts = super::telemetry::SkillCounts {
                skill_count: model_visible_skills.len(),
                scheme_skill_count: model_visible_skills
                    .iter()
                    .filter(|skill| skill.kind == crate::skills::SkillKind::Scheme)
                    .count(),
            };
            let installed = super::telemetry::install_session_telemetry(
                &telemetry_agent,
                &wiring,
                Some(skill_counts),
                std::sync::Arc::clone(&session_counters),
            )
            .await?;
            Some(std::sync::Arc::new(installed))
        }
        _ => None,
    };

    // The `skill_use_count` session counter counts through the session's
    // telemetry handle (installed once the telemetry composition decided
    // whether this session reports at all).
    if let Some(telemetry) = telemetry.as_ref() {
        let _ = repeat_nudge_telemetry.set(telemetry.clone());
        session.set_skill_telemetry(telemetry.clone());
        // Same lifetime for the `rlm_child_*` session counters: the
        // producer's flush counts through this handle.
        wiring.rlm_usage.set_telemetry(telemetry.clone());
    }
    let goal_driver = wiring.runtime.goal_driver().clone();
    // Each row the session's durable store appends that bills a model call
    // outside the agent loop charges the running goal and telemetry run.
    let charge_goal = goal_driver.lock().await.billed_row_recorder();
    let charge_run = telemetry.clone();
    wiring
        .rlm_usage
        .store()
        .observe_billed_rows(std::sync::Arc::new(
            move |row: &crate::session::billed_row::BilledRow| {
                charge_goal(&row.usage);
                if let Some(telemetry) = &charge_run {
                    telemetry.note_billed_call(&row.usage);
                }
            },
        ))
        .await;
    // The factory host bridge: registered from `create_session` (the #3184
    // pattern), so the daemon/TUI factory surface resolves against this
    // session's harness dirs, model registry, and the allowlist pin. The
    // kernel owns the runs; the bridge prefights and tunnels. Captured
    // before the loop wiring moves the session model and the digest moves
    // the local harness dir.
    let factory_host =
        super::factory_host::FactoryHost::new(super::factory_host::FactoryHostConfig {
            agent_dir: config.agent_dir.clone(),
            global_harness_dir: crate::refinement::get_global_harness_state_dir(&config.agent_dir),
            local_harness_dir: factory_local_harness_dir,
            session_model: factory_session_model,
            allowed_models: factory_allowed_models,
        });
    Ok(SessionEngine {
        session,
        skills: resources.skills,
        skill_diagnostics: resources.skill_diagnostics,
        prompt_templates: resources.prompts,
        agents_files: resources.agents_files,
        system_prompt,
        system_prompt_static_len,
        context_thinning: thinning_settings,
        goal_driver,
        queued_goal_context_purge: config.queued_goal_context_purge.clone(),
        mcp_manager,
        turn_boundary,
        telemetry,
        worktrees,
        rlm_usage: wiring.rlm_usage,
        factory_host,
        rlm_progress_notes: Arc::clone(&wiring.rlm.notes),
        provisioner,
        resource_reload,
        better,
    })
}

impl SessionEngine {
    /// After a model switch, the `model.info` handler, the usage estimate's
    /// context window, and the system prompt's image-input line follow the
    /// model the session now runs (the line rides the dynamic tail, so the
    /// cache-stable static prefix is untouched).
    pub async fn update_model_facts(&self, model: &pa_types::ai::Model) {
        super::turn_boundary::TurnBoundaryRequests::rebind_model_facts(
            &self.turn_boundary,
            super::turn_boundary::ModelInfo {
                id: model.id.clone(),
                provider: model.provider.clone(),
                input: model.input.clone(),
            },
            (model.context_window > 0).then_some(model.context_window),
        );
        let vision_capable = model.input.contains(&pa_types::ai::ModelInput::Image);
        self.session
            .agent()
            .set_system_prompt(crate::prompts::system_prompt::with_image_input(
                &self.resource_reload.base_prompt(),
                vision_capable,
            ))
            .await;
    }

    /// A weak reference for kernel liveness probes without joining the
    /// strong ownership graph.
    pub fn kernel_provisioner_weak(
        &self,
    ) -> std::sync::Weak<crate::kernel::scheme::SchemeKernelProvisioner> {
        std::sync::Arc::downgrade(&self.provisioner)
    }

    /// Whether the session's kernel has live background work: a shell
    /// command whose completion notice may still be owed, or a cell that
    /// outlived its call (the same read the daemon's liveness probe uses).
    #[must_use]
    pub fn has_background_bash(&self) -> bool {
        self.provisioner.has_background_work()
    }

    /// Expand a `/skill:<name>` submission into its `<skill>` block for
    /// the accepted-turn row (TS `_expandSkillCommand`; the row the daemon
    /// emits before admission must match the text the model turn
    /// receives). Non-skill inputs pass through unchanged; the admitted
    /// turn's own expansion is idempotent over the block. The
    /// `skill_use_count` counter counts at the admission, not here.
    pub fn expand_skill_submission(&self, text: &str) -> String {
        crate::skills::expand_skill_command(text, &self.resource_reload.skills()).0
    }

    /// Record one late (fire-and-forget) `agent_message.send` receipt (TS
    /// `_recordLateIpythonSentAgentMessage`): the receipt persists as a
    /// custom entry and the newest matching `python_repl` tool result
    /// carries it in its `sentAgentMessages` details.
    ///
    /// # Errors
    ///
    /// Returns the persistence failure so the embedding cannot report successful quiescence.
    pub async fn record_late_sent_agent_message(
        &self,
        tool_call_id: &str,
        message: crate::kernel::shared::KernelSentAgentMessage,
    ) -> anyhow::Result<()> {
        super::late_sent_messages::record(
            self.session.session_handle(),
            self.session.agent(),
            tool_call_id,
            &message,
        )
        .await
    }

    /// Withdraw the queued goal-context turns: a minted continuation waiting
    /// in the embedding's queue never runs behind a paused/cleared/replaced goal.
    pub fn purge_queued_goal_contexts(&self) {
        if let Some(purge) = &self.queued_goal_context_purge {
            purge();
        }
    }

    /// Prompt the session (delegates to `AgentSession::prompt`).
    ///
    /// # Errors
    ///
    /// Returns the underlying turn admission error: an invalid prompt, an
    /// already-busy session under its admission rule, or the turn's own failure.
    pub async fn prompt(
        &self,
        text: &str,
        options: PromptOptions,
    ) -> anyhow::Result<PromptOutcome> {
        self.session.prompt(text, options).await
    }

    /// Tear the kernel down now, for a host that ends the session but keeps the engine
    /// alive: its running cell is interrupted, and its shell commands (all but the persistent
    /// shells), MCP servers and factory runs end with it. What its cells defined is lost.
    /// The session's cached Responses sockets close with it (TS `cleanupSessionResources` on
    /// dispose), so a worker that replaces its session does not hold the old socket until the
    /// idle expiry.
    ///
    /// # Panics
    ///
    /// Panics if the refinement-cancellation mutex is poisoned.
    pub async fn dispose_kernel(&self) {
        use crate::tools::kernel_cell::KernelProvisioner as _;
        self.session
            .refinement_cancel
            .lock()
            .expect("refinement cancellation lock")
            .abort();
        if let Some(session_id) = self.session.agent().session_id() {
            pa_ai::responses_transport::close_session_websockets(session_id);
        }
        self.provisioner.kill().await;
        // The session ends: name the worktrees it made and never removed (they stay on disk).
        if let Some(notice) = self.worktrees.unremoved_notice() {
            eprintln!("prime-agent: {notice}");
        }
    }

    /// One factory activity over this session's live kernel: the `/factory`
    /// view's bridge lane (graph/status/watch/run/stop/resume). A `run`
    /// prefights the spec's declared models first (allowlist pin, request
    /// auth) so a doomed run fails before any child spawns; then the
    /// out-of-band frame carries the request into the kernel's executor,
    /// which owns the run registry.
    ///
    /// # Errors
    ///
    /// Returns an error when the arguments are invalid, the preflight
    /// fails, the session has no running kernel, or the kernel request
    /// fails or does not settle.
    pub async fn factory_activity(
        &self,
        action: &str,
        run_id: Option<&str>,
        spec_id: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<serde_json::Value> {
        let request = super::factory_host::FactoryActivityRequest::parse(
            action, run_id, spec_id, timeout_ms,
        )?;
        if request.action == "run" {
            let spec_id = request
                .spec_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("factory activity run requires specId"))?;
            // The preflight reads the harness states, the model catalog,
            // and the auth caches from disk — blocking work off the
            // executor (the daemon's established settings-read posture:
            // `spawn_blocking`, never the async lane), so a stalled
            // filesystem can never stall the worker's other activity.
            let host = self.factory_host.clone();
            let spec_id = spec_id.to_string();
            let preflight = tokio::task::spawn_blocking(move || host.preflight_run(&spec_id))
                .await
                .map_err(|join| anyhow::anyhow!("factory run preflight join failed: {join}"))?;
            preflight?;
        }
        let mut frame = self
            .provisioner
            .factory_activity(&serde_json::json!({
                "id": "factory-activity",
                "action": request.action,
                "runId": request.run_id,
                "specId": request.spec_id,
                "timeoutMs": request.timeout_ms,
            }))
            .await?;
        match frame["status"].as_str() {
            Some("ok") => Ok(frame["result"].take()),
            Some(_) | None => Err(anyhow::anyhow!(
                "{}",
                frame["reason"]
                    .as_str()
                    .unwrap_or("Kernel factory activity failed")
            )),
        }
    }

    /// Out-of-band kernel bash activity, scoped to this session's live kernel.
    ///
    /// # Errors
    ///
    /// Returns an error when the session has no running kernel, or the
    /// kernel's bash-activity validation or request fails.
    pub async fn bash_activity(
        &self,
        action: &str,
        activity_id: Option<&str>,
        lines: usize,
    ) -> anyhow::Result<serde_json::Value> {
        self.provisioner
            .bash_activity(action, activity_id, lines)
            .await
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod kernel_tests;

#[cfg(test)]
mod append_system_tests;

#[cfg(test)]
mod billed_row_tests;
