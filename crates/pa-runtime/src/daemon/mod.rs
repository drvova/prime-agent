//! The runtime daemon: a [`Runtime`] over the agent dir, serving wire2 to
//! every client that connects to its socket, beside the old daemon until
//! the TUI moves onto it (phase 8).
//!
//! - **Endpoint.** [`socket_path`]: `daemon2.sock` beside the old daemon's
//!   socket (Unix), or the old daemon's pipe name with `2` appended
//!   (Windows). The embedder binds it through `pa_types::platform::transport`
//!   (the old daemon's transport, its stale-socket cleanup included) and
//!   hands [`run`] the listener; each connection is one
//!   [`crate::WireServer::serve`].
//! - **One per agent dir.** The embedder takes
//!   `pa_core::platform::RuntimeDaemonLock` with [`take_lock`] and holds it
//!   for the process's life (an operating-system file lock on
//!   `<agent dir>/runtime-daemon.lock`, its pid, socket and restart owner
//!   inside), so a second runtime daemon on the same agent dir refuses to
//!   start whatever socket it was given, and the old daemon's boot reap
//!   spares this one and its restart owner. A holder that does not answer on
//!   the same socket is a wedged predecessor: [`take_lock`] stops it first
//!   (the boot reap).
//! - **Sessions.** A session the daemon opens is held under its
//!   `SessionLease` (`pa_core::session::lease`) from `session.create` until
//!   `session.stop` or the daemon's end: the old daemon, the print mode and
//!   this daemon never hold the same session file. A change to a saved
//!   session takes its lease for the change.
//! - **Queries and calls.** [`RuntimeHandlers`](handlers): `sessions.list`,
//!   `session.snapshot` and `transcript.page` read the transcript store;
//!   `session.create`, `stop`, `rename`, `delete`, `set_model`, `set_effort`,
//!   `fork`, `label` and `export` change it through pa-core's session
//!   manager and store, or the open session's engine. An open session's
//!   turns and queue: `session.abort`, `abort_and_send_queued`,
//!   `resume_queue`, `withdraw`, `restore`, `compact` and the `tasks` query
//!   (`handlers/queue.rs`); its state: `session.context`, `usage`, `tree`,
//!   `context_tree` and `breakdown` (`handlers/state.rs`). For the CLI's
//!   daemon commands: the list's rows in the old daemon's summary shape
//!   (`handlers/summary.rs`), `session.send` (`handlers/messaging.rs`), and
//!   the `schedules` query with `schedule.add` and `schedule.cancel`
//!   (`handlers/jobs.rs`). For the ACP client: an input hold for its
//!   connection, one prompt's cancel (`handlers/input.rs`), the subagent
//!   roster and a child's cancel (`handlers/children.rs`), and the
//!   `session.model` query (`handlers/model.rs`).
//! - **Reliability.** The daemon's sessions are supervised
//!   (`crate::session` faults): a panic on a session's threads or a stuck
//!   cell ends that session's lifetime only; the crash is appended to
//!   `<agent dir>/runtime-daemon/session-crashes.jsonl` (the database's
//!   `crashes` table), published as `agent/<id>/crashed`, and the session is
//!   rebuilt from its transcript with the old daemon's backoff and give-up
//!   (`agent/<id>/recovered` or `/failed`; a given-up session is closed and
//!   its lease released), each fault also mirrored onto the session's
//!   stream as the old supervisor sends it. `session.snapshot` carries its
//!   `lastCrash`, and its stream's `lastEventSequence`. The
//!   sessions it holds are kept in the database, and a daemon that starts
//!   after one that died revives those with unfinished work ([`held`]).
//! - **Scheduled jobs.** The kernel's `schedule.*` and `rlm_heartbeat.*`
//!   are served in the daemon's sessions, on the old daemon's stored jobs,
//!   and fired by the runtime's timer sources while this daemon is the
//!   selected one ([`DaemonOptions::selected`]); a job due in an idle
//!   session the daemon holds down wakes it ([`schedules`]).
//! - **Boot imports.** The old daemon's, shared (`pa_core::session::boot_import`):
//!   a distribution build's history import, announced on the live-only
//!   `system/history_import` (`system.history_import` reads a running one),
//!   then the transcript and agent-state imports, on their own task once the
//!   daemon serves (`handlers/history_import.rs`).
//! - **Telemetry.** One `daemon event` (kind `catalog_refresh`,
//!   `daemon_kind: runtime`) once the startup catalog refresh settles, off
//!   the accept path; nothing the daemon serves waits on the network.

use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_types::event_log::{NewEvent, Topic, Trust, SYSTEM_CLOSING};
use pa_types::platform::transport::TransportListener;
use tokio::task::JoinSet;

use crate::bus::Heard;
use crate::runtime::{Runtime, RuntimeOptions};
use crate::session::{SessionBuilder, SessionFaults, Supervision};
use crate::wire::{Attached, WireConfig};

mod boot_reap;
mod handlers;
mod held;
mod model_scope;
mod schedules;

pub use boot_reap::take_lock;

use handlers::RuntimeHandlers;
use held::OpenSessions;
pub(crate) use held::Revival;

/// How long the drain waits for the connections to send what they hold
/// (`system/closing` last) before it closes them.
const CLOSE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The runtime daemon's socket's file name, beside the old daemon's.
#[cfg(unix)]
const SOCKET_NAME: &str = "daemon2.sock";

/// How many spare Scheme kernels the daemon keeps built: the old daemon's
/// count, so the first session a client creates takes one already built.
const SPARE_KERNELS: usize = 2;

/// How long the webhooks' deliveries are counted before one `daemon event`
/// reports them: a busy webhook sends one a minute, not one a request.
const WEBHOOK_TELEMETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// How many events a connection may fall behind before it loses the oldest
/// and catches up from the log.
const LISTENER_CAPACITY: NonZeroUsize = NonZeroUsize::new(1 << 14).expect("non-zero");

/// The runtime daemon's endpoint for the old daemon's `daemon_socket`:
/// `daemon2.sock` in the same directory (Unix), the pipe name with `2`
/// appended (Windows).
#[must_use]
pub fn socket_path(daemon_socket: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        daemon_socket.with_file_name(SOCKET_NAME)
    }
    #[cfg(not(unix))]
    {
        let mut name = daemon_socket.as_os_str().to_os_string();
        name.push("2");
        PathBuf::from(name)
    }
}

/// The environment a daemon session `id` runs with over the daemon's own:
/// the orphan journal its kernel records the programs its commands start
/// in, which the daemon reaps when the session or the daemon dies.
fn session_env(agent_dir: &Path, id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(
        pa_core::kernel::orphan_journal::ORPHAN_PROCESS_JOURNAL_ENV.to_string(),
        held::orphan_journal(agent_dir, id)
            .to_string_lossy()
            .into_owned(),
    )])
}

/// The `kv` scope the options a client created a daemon session with are
/// kept under, one key per session (`options:<id>`), so a session the daemon
/// opens again (its revival, a scheduled wake) is built as it was created.
const OPTIONS_SCOPE: &str = "runtime-daemon";

/// The options session `id` was created with; the default when it was
/// created with none, or they cannot be read (logged).
fn session_options(log: &pa_log::Log, id: &str) -> crate::SessionOptions {
    let read = || -> anyhow::Result<crate::SessionOptions> {
        match log.value(OPTIONS_SCOPE, &format!("options:{id}"))? {
            Some(text) => crate::SessionOptions::from_json(&serde_json::from_str(&text)?),
            None => Ok(crate::SessionOptions::default()),
        }
    };
    read().unwrap_or_else(|error| {
        tracing::warn!("session {id}'s options could not be read: {error:#}");
        crate::SessionOptions::default()
    })
}

/// What a runtime daemon is started with.
pub struct DaemonOptions<B> {
    /// The agent dir: its database, settings and session leases.
    pub agent_dir: PathBuf,
    /// The socket the daemon serves ([`socket_path`]): an update's prepared
    /// roster is kept under its update dir (`daemon.prepare_update_restart`).
    pub socket: PathBuf,
    /// What an update's boot is started with (`handlers/restore.rs`).
    pub update: UpdateBoot,
    /// Where new sessions' transcripts go (`<sessions dir>/<id>.jsonl`).
    pub session_dir: PathBuf,
    /// Where a session created without a `cwd` runs.
    pub cwd: PathBuf,
    /// Builds each session's engine.
    pub builder: B,
    /// Whether this daemon is the selected one now: asked before each
    /// scheduled job runs, which is skipped while it is not, so a job never
    /// fires in both daemons.
    pub selected: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Called once when the daemon begins to stop on purpose (its stop
    /// signal, `daemon.shutdown`, `daemon.restart`), before anything stops:
    /// the embedder records the stop intent its restart owner reads, so a
    /// stop that ends in a kill is not restarted either.
    pub stopping: Box<dyn FnOnce() + Send>,
}

/// What the runtime daemon's update boot is started with.
pub struct UpdateBoot {
    /// The update roster the coordinator spawned the daemon with
    /// (`pa_core::update::restart::restore::consume_roster_env`, read before
    /// the boot sweep deletes its file): its sessions are restored, and the
    /// restore reported on `daemon.update_restore_status`.
    pub roster: Option<pa_types::daemon::UpdateRoster>,
    /// The restart owner keeping the daemon, if any.
    pub keeper_pid: Option<u32>,
    /// The release the daemon runs, as `daemon.identity` and the roster name it.
    pub app_version: String,
}

/// Runs the runtime daemon on `listener` until `shutdown` resolves or a
/// client asks it to stop (`daemon.shutdown`, `daemon.restart`): opens the
/// runtime over the agent dir and serves each connection on its own task.
/// Then the drain, the old supervisor's: [`DaemonOptions::stopping`], every
/// query and call refused, `system/closing` to every connection (which then
/// closes), every session's running turn cut off with its continuation
/// queued for the next daemon, the connections closed (each given
/// [`CLOSE_BUDGET`] to send what it holds), then the sessions (and their
/// leases) and the runtime.
///
/// # Errors
///
/// The runtime could not be opened (its database).
///
/// # Panics
///
/// Called outside a Tokio runtime.
#[expect(
    clippy::too_many_lines,
    reason = "the daemon's start, serving and shutdown read in the order they run"
)]
pub async fn run<B: SessionBuilder>(
    listener: Box<dyn TransportListener>,
    mut options: DaemonOptions<B>,
    shutdown: impl Future<Output = ()> + Send,
) -> anyhow::Result<()> {
    // No session may block on whatever this process was given as standard
    // input, and every session's kernel inherits the raised file limit.
    if let Err(error) = pa_core::platform::process::close_standard_input() {
        tracing::warn!("standard input was not closed: {error}");
    }
    let _ = pa_core::platform::process::raise_open_file_limit();
    // The telemetry client (none when the environment opts out) and the
    // launch event: once the startup catalog refresh settles, the served
    // model count with `daemon_kind: runtime`, on its own task. The
    // catalog's disk caches are kept warm as the old daemon keeps them.
    let telemetry = {
        use pa_core::session_engine::telemetry;
        let settings = pa_core::settings::SettingsManager::create(&options.cwd, &options.agent_dir);
        let client = (!matches!(
            telemetry::telemetry_switch(&settings),
            telemetry::TelemetrySwitch::Env { enabled: false, .. }
        ))
        .then(|| telemetry::build_client(&settings, &options.agent_dir));
        let _ = pa_core::models::startup_refresh(&options.agent_dir);
        pa_core::models::spawn_hourly_refresh(&options.agent_dir);
        if let Some(client) = client.clone() {
            let agent_dir = options.agent_dir.clone();
            tokio::spawn(async move {
                let catalog = pa_core::models::catalog_for(Some(&agent_dir.join("models.json")));
                let credentials = pa_core::models::prime_credentials_for_dir(&agent_dir);
                catalog
                    .refresh_with_credentials(false, credentials.as_ref())
                    .await;
                let count = catalog.resolve(credentials.as_ref()).len();
                telemetry::track_catalog_refresh(&client, count, telemetry::DaemonKind::Runtime);
            });
        }
        client
    };
    let daemon_dir = options.agent_dir.join(held::DIR);
    if let Err(error) = std::fs::create_dir_all(&daemon_dir) {
        tracing::warn!("{} was not created: {error}", daemon_dir.display());
    }
    let crash_log = daemon_dir.join(pa_core::session::crash::SESSION_CRASH_LOG);
    // Every panic is accounted for: a session's is that session's crash, any
    // other the daemon's own, appended to the crash log.
    pa_core::session::crash::install_panic_hook(crash_log.clone());
    let boot_crash_log = crash_log.clone();
    let runtime = Arc::new(
        Runtime::open_with(
            &options.agent_dir,
            RuntimeOptions {
                builder: options.builder,
                spare_kernels: SPARE_KERNELS,
                // Every session and the subagents they spawn run at once: a
                // parent's turn may wait on its children's.
                max_concurrent_runs: NonZeroUsize::MAX,
                listener_capacity: LISTENER_CAPACITY,
                cwd: options.cwd.clone(),
                sessions: Vec::new(),
            },
            SessionFaults::Supervised(Arc::new(Supervision {
                crash_log,
                counted: {
                    let client = telemetry.clone();
                    Box::new(move |kind: &str| {
                        if let Some(client) = &client {
                            pa_core::session_engine::telemetry::track_session_crashed(client, kind);
                        }
                    })
                },
            })),
        )
        .await?,
    );
    let open = Arc::new(OpenSessions::new(Arc::clone(runtime.log())));
    let schedules = schedules::Schedules::new(
        &runtime,
        Arc::clone(&open),
        options.agent_dir.clone(),
        options.selected,
    );
    runtime.attach_jobs(Arc::clone(&schedules) as Arc<dyn crate::session::SessionJobs>);
    // Its sessions' timers, watches and webhooks outlive it, as their
    // subscriptions do.
    runtime.keep_kernel_sources();
    // Adoption: the webhooks' deliveries, counted without blocking a
    // delivery and sent at most once a window, by the telemetry worker.
    if let Some(client) = telemetry.clone() {
        let pending = Arc::new(std::sync::atomic::AtomicU64::new(0));
        runtime.observe_webhook_deliveries(Box::new(move || {
            use std::sync::atomic::Ordering;
            if pending.fetch_add(1, Ordering::Relaxed) == 0 {
                let (client, pending) = (client.clone(), Arc::clone(&pending));
                tokio::spawn(async move {
                    tokio::time::sleep(WEBHOOK_TELEMETRY_WINDOW).await;
                    pa_core::session_engine::telemetry::track_webhooks_delivered(
                        &client,
                        pending.swap(0, Ordering::Relaxed),
                    );
                });
            }
        }));
    }
    // Its sessions' families are recorded in the old daemon's ledger, wait
    // on each other's messages and observe each other.
    runtime
        .subagents()
        .attach_daemon(crate::subagents::DaemonFamily::new(
            &options.agent_dir,
            &options.session_dir,
        ));
    let feeds = Arc::new(handlers::Feeds::default());
    // A session given up on is closed, and its lease released.
    let given_up = {
        let mut listener = runtime.listen();
        let (runtime, open, schedules, feeds) = (
            Arc::clone(&runtime),
            Arc::clone(&open),
            Arc::clone(&schedules),
            Arc::clone(&feeds),
        );
        tokio::spawn(async move {
            while let Some(heard) = listener.recv().await {
                let event = match heard {
                    Heard::Event(event) => event,
                    Heard::Lagged { .. } => continue,
                };
                let mut levels = event.event.topic.levels();
                let (Some("agent"), Some(id), Some("failed"), None) =
                    (levels.next(), levels.next(), levels.next(), levels.next())
                else {
                    continue;
                };
                if open.contains(id) {
                    let closing = open.bashes.close(id).await;
                    if let Err(error) = runtime.close_session(id).await {
                        tracing::warn!(
                            "the given-up session {id} did not close cleanly: {error:#}"
                        );
                    }
                    open.remove(id).await;
                    drop(closing);
                    schedules.disarm(id);
                    feeds.due(id);
                }
            }
        })
    };
    // An update's boot (spec §6): this socket's update scratch and the stale
    // TS names are swept (the roster was read before), then the sessions the
    // revival did not bring up are restored from the roster once serving.
    {
        let (agent_dir, socket) = (options.agent_dir.clone(), options.socket.clone());
        let _ = tokio::task::spawn_blocking(move || {
            pa_core::update::restart::restore::boot_sweep(&agent_dir, &socket);
        })
        .await;
    }
    let restore = Arc::new(pa_core::update::restart::restore::RestoreProgress::new());
    let roster = options.update.roster.take();
    restore.begin(roster.as_ref());
    let revived = held::revive(&runtime, &open, &schedules, &options.agent_dir).await;
    let attached = Arc::new(Attached::default());
    let stop = Arc::new(handlers::DaemonStop::default());
    let update = Arc::new(pa_core::update::restart::UpdateRestart::new(
        options.agent_dir.clone(),
        options.socket,
        pa_types::daemon::UpdateTimeoutBudget::from_env(),
        Box::new(|line: &str| tracing::info!("{line}")),
    ));
    // Aborts an update transaction whose budget passes with no call to see it.
    let update_watchdog = {
        let update = Arc::clone(&update);
        tokio::spawn(async move { update.watchdog().await })
    };
    let handlers = Arc::new(RuntimeHandlers::new(
        Arc::clone(&runtime),
        Arc::clone(&open),
        Arc::clone(&schedules),
        Arc::clone(&attached),
        feeds,
        Arc::clone(&stop),
        update,
        Arc::clone(&restore),
        options.update,
        options.agent_dir.clone(),
        options.session_dir,
        options.cwd,
    ));
    let feed_tasks = handlers.spawn_feeds();
    let restore_pass = {
        let host = handlers::RuntimeRestore {
            handlers: Arc::clone(&handlers),
            revived,
        };
        tokio::spawn(async move {
            pa_core::update::restart::restore::restore_pass(&host, &restore, roster).await;
        })
    };
    // The boot's imports, off every path a client waits on.
    let import_tasks = {
        let client = telemetry.clone();
        handlers.spawn_boot_import(boot_crash_log, Box::new(move || client.clone()))
    };
    let server = runtime.wire_server(Arc::clone(&handlers), WireConfig::default(), attached);
    let mut connections = JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    while connections.try_join_next().is_some() {}
                    let server = server.clone();
                    connections.spawn(async move {
                        let (reader, writer) = stream.split();
                        if let Err(error) = server.serve(reader, writer).await {
                            tracing::debug!("a wire2 connection ended: {error:#}");
                        }
                    });
                }
                Err(error) => tracing::warn!("the runtime daemon's accept failed: {error}"),
            },
            () = &mut shutdown => break,
            () = stop.requested() => break,
        }
    }
    drop(listener);
    // A restore still running ends with the daemon: its rows are the next
    // boot's to revive.
    restore_pass.abort();
    let _ = restore_pass.await;
    stop.begin();
    (options.stopping)();
    let closing = NewEvent {
        topic: Topic::new(SYSTEM_CLOSING)?,
        source: "runtime".to_string(),
        session: None,
        payload: stop.closing(),
        trust: Trust::Owner,
        cause: None,
        reply: None,
    };
    if let Err(error) = runtime.publish(closing).await {
        tracing::warn!("the runtime daemon's end was not announced: {error:#}");
    }
    handlers.cut_off_turns().await;
    let closed = tokio::time::timeout(CLOSE_BUDGET, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if closed.is_err() {
        tracing::warn!(
            "the runtime daemon closes connections that did not end in {CLOSE_BUDGET:?}"
        );
    }
    connections.shutdown().await;
    drop(server);
    // An import still copying resumes on the next boot.
    for task in feed_tasks.into_iter().chain(import_tasks) {
        task.abort();
        let _ = task.await;
    }
    drop(handlers);
    // The `!` commands are killed before their sessions close.
    open.bashes.close_all().await;
    given_up.abort();
    let _ = given_up.await;
    update_watchdog.abort();
    let _ = update_watchdog.await;

    schedules.stop();
    drop(schedules);
    if let Ok(runtime) = Arc::try_unwrap(runtime) {
        runtime.shutdown().await;
    } else {
        tracing::warn!(
            "the runtime is still in use at shutdown; its sessions end with the process"
        );
    }
    // The sessions are closed: their leases go now.
    drop(open);
    if let Some(client) = telemetry {
        let _ = client.shutdown().await;
    }
    Ok(())
}
