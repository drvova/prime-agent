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
//! - **One per agent dir.** The embedder holds
//!   `pa_core::platform::RuntimeDaemonLock` for the process's life (an
//!   operating-system file lock on `<agent dir>/runtime-daemon.lock`, its pid
//!   inside), so a second runtime daemon on the same agent dir refuses to
//!   start whatever socket it was given, and the old daemon's boot reap
//!   spares this one.
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
//!   `context_tree` and `breakdown` (`handlers/state.rs`). `settings`,
//!   `models`, `sources` and `subscriptions` are refused: they arrive later.
//! - **Telemetry.** One `daemon event` (kind `catalog_refresh`,
//!   `daemon_kind: runtime`) once the startup catalog refresh settles, off
//!   the accept path; nothing the daemon serves waits on the network.

use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_types::platform::transport::TransportListener;
use tokio::task::JoinSet;

use crate::runtime::{Runtime, RuntimeOptions};
use crate::session::SessionBuilder;
use crate::wire::WireConfig;

mod handlers;

use handlers::{OpenSessions, RuntimeHandlers};

/// The runtime daemon's socket's file name, beside the old daemon's.
#[cfg(unix)]
const SOCKET_NAME: &str = "daemon2.sock";

/// How many spare Scheme kernels the daemon keeps built: the old daemon's
/// count, so the first session a client creates takes one already built.
const SPARE_KERNELS: usize = 2;

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

/// What a runtime daemon is started with.
pub struct DaemonOptions<B> {
    /// The agent dir: its database, settings and session leases.
    pub agent_dir: PathBuf,
    /// Where new sessions' transcripts go (`<sessions dir>/<id>.jsonl`).
    pub session_dir: PathBuf,
    /// Where a session created without a `cwd` runs.
    pub cwd: PathBuf,
    /// Builds each session's engine.
    pub builder: B,
}

/// Runs the runtime daemon on `listener` until `shutdown` resolves: opens
/// the runtime over the agent dir, serves each connection on its own task,
/// then closes the connections, the sessions (and their leases) and the
/// runtime.
///
/// # Errors
///
/// The runtime could not be opened (its database).
///
/// # Panics
///
/// Called outside a Tokio runtime.
pub async fn run<B: SessionBuilder>(
    listener: Box<dyn TransportListener>,
    options: DaemonOptions<B>,
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
    let runtime = Arc::new(
        Runtime::open(
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
        )
        .await?,
    );
    let open = Arc::new(OpenSessions::default());
    let server = runtime.wire_server(
        RuntimeHandlers::new(
            Arc::clone(&runtime),
            Arc::clone(&open),
            options.agent_dir.clone(),
            options.session_dir,
            options.cwd,
        ),
        WireConfig::default(),
    );
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
        }
    }
    drop(listener);
    connections.shutdown().await;
    drop(server);
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
