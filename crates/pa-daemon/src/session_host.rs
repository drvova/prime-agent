//! Hosting sessions inside the daemon process.
//!
//! A hosted session is one [`crate::worker::Worker`] running on its own OS
//! thread and its own tokio runtime, inside a [`SessionScope`] that carries
//! what used to be its process: launch environment, working directory,
//! identity. The supervisor talks to it over the same socket and frames a
//! worker process spoke, so routing, attach and the client wire are
//! unchanged.
//!
//! What replaces the process boundary:
//!
//! - **Identity and liveness.** Each hosted lifetime gets a pid-shaped id
//!   from a range no operating system hands out ([`is_hosted_pid`]) and a
//!   generation string. Descriptors, leases and registrations record that
//!   pair where they recorded a worker's pid and start id; "alive" means the
//!   generation is registered as running in this daemon ([`hosted_alive`]),
//!   never that some pid exists.
//! - **Containment.** The daemon's panic hook ([`install_panic_hook`])
//!   attributes a panic to the session whose thread raised it, records it,
//!   and ends that session's host thread, which drops the session's runtime
//!   and every task on it. Other sessions and the control plane keep running.
//! - **Stuck work.** A host call that never returns cannot be unwound: the
//!   kernel reports a cell that outlived its interrupt
//!   ([`pa_core::kernel::StuckCellSink`]), the session is ended the same way,
//!   and the stuck thread is abandoned (it keeps its memory and any file
//!   descriptors it holds until the daemon exits).
//!
//! What it does not replace: an abort-class fault (stack overflow, out of
//! memory, a fault in native code) ends the daemon and every session in it.
//! The daemon's owner restarts it and the boot adoption revives the sessions
//! that were mid-turn from their transcripts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use pa_types::session_scope::{self, SessionScope};
use serde::{Deserialize, Serialize};

/// Hosted ids start here: above every real pid (Linux caps pids at 2^22,
/// macOS at 99,998), and positive as an `i32` so a signal sent to one by
/// mistake names no process and no process group.
const HOSTED_PID_BASE: u32 = 0x4000_0000;

/// How long a session's runtime may take to drop its tasks once the session
/// has ended; threads still blocked after that are abandoned. Tasks drop at
/// their next await, so only a blocking call in flight waits this out, and
/// every stop of such a session pays it.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// The hosted lifetimes running in this process: id -> generation and the
/// handle that ends it.
fn live() -> &'static Mutex<LiveSessions> {
    static LIVE: OnceLock<Mutex<LiveSessions>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Hosted id -> the lifetime's generation and the handle that ends it.
type LiveSessions = HashMap<u32, (String, Arc<HostControl>)>;

/// End the hosted lifetime `pid` whatever it is doing, and wait until its
/// host thread has let go of it. Returns whether it was running. This is
/// the forced stop of a session that did not answer its shutdown: its tasks
/// are dropped, and a thread blocked in a call that never returns is
/// abandoned.
pub(crate) async fn stop_hosted(pid: u32) -> bool {
    let control = live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&pid)
        .map(|(_, control)| Arc::clone(control));
    let Some(control) = control else {
        return false;
    };
    control.end.notify_one();
    // The host thread removes the lifetime when its runtime is down (bounded
    // by the runtime's shutdown budget).
    while hosted_alive(pid) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// The fault each crashed lifetime ended with, kept while the supervisor
/// still holds that lifetime's handle.
fn crashes() -> &'static Mutex<HashMap<u32, SessionCrash>> {
    static CRASHES: OnceLock<Mutex<HashMap<u32, SessionCrash>>> = OnceLock::new();
    CRASHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The fault the hosted lifetime `pid` ended with, when it crashed: what a
/// request that was in flight on it failed for.
#[must_use]
pub fn recorded_crash(pid: u32) -> Option<SessionCrash> {
    crashes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&pid)
        .cloned()
}

impl SessionCrash {
    /// The one-line reason a caller is given for work this crash failed.
    #[must_use]
    pub fn reason(&self) -> String {
        if self.location.is_empty() {
            format!("Session crashed ({}): {}", self.kind, self.message)
        } else {
            format!(
                "Session crashed ({}): {} at {}",
                self.kind, self.message, self.location
            )
        }
    }
}

/// Whether `pid` is a hosted session's id rather than a process id.
#[must_use]
pub fn is_hosted_pid(pid: u32) -> bool {
    pid >= HOSTED_PID_BASE
}

/// Whether the hosted lifetime `pid` is running in this process.
#[must_use]
pub fn hosted_alive(pid: u32) -> bool {
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains_key(&pid)
}

/// The generation of the hosted lifetime `pid`, while it runs in this process.
#[must_use]
pub fn hosted_generation(pid: u32) -> Option<String> {
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&pid)
        .map(|(generation, _)| generation.clone())
}

/// The id the calling session records as its owner pid: its hosted id inside
/// a hosted session, the process id anywhere else.
#[must_use]
pub fn session_pid() -> u32 {
    session_scope::current()
        .and_then(|scope| scope.slot::<HostControl>())
        .map_or_else(std::process::id, |control| control.pid)
}

/// Why and where one hosted session crashed. Everything here is a primitive
/// about the fault itself; `message` is the panic's own text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCrash {
    /// `panic` (unwound and contained) or `stuck` (a host call outlived its
    /// interrupt and was abandoned).
    pub kind: String,
    pub message: String,
    /// `file:line:column` of the panic; empty for a stuck call.
    pub location: String,
    /// The name of the thread that raised it.
    pub thread: String,
    pub at: String,
}

/// How one hosted lifetime ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostedExit {
    /// The worker ended on its own (a shutdown, a failed boot) or was stopped
    /// by the supervisor; a failed boot carries its error.
    Ended(Option<String>),
    Crashed(SessionCrash),
}

/// The handle a hosted session's own code reaches its host through (a
/// [`SessionScope`] slot).
pub(crate) struct HostControl {
    pid: u32,
    /// The host thread parks on this; a stored permit ends the session.
    end: tokio::sync::Notify,
    crash: Mutex<Option<SessionCrash>>,
}

impl HostControl {
    /// Record the first fault and end the session.
    fn fault(&self, crash: SessionCrash) {
        crashes()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(self.pid)
            .or_insert_with(|| crash.clone());
        self.crash
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(crash);
        self.end.notify_one();
    }
}

/// End the calling session: its host thread stops serving it and drops its
/// runtime. Outside a hosted session this is the worker process's exit.
pub(crate) async fn end_session() -> ! {
    if let Some(control) = session_scope::current().and_then(|scope| scope.slot::<HostControl>()) {
        control.end.notify_one();
        // The host thread owns the session from the permit on; this task is
        // dropped with the runtime.
        match std::future::pending::<std::convert::Infallible>().await {}
    }
    std::process::exit(0)
}

/// Whether the calling code runs inside a hosted session.
pub(crate) fn is_hosted() -> bool {
    session_scope::current().is_some_and(|scope| scope.slot::<HostControl>().is_some())
}

/// Where the daemon's own panics are recorded, and those recorded so far.
fn daemon_crash_record() -> &'static Mutex<(Option<PathBuf>, Vec<SessionCrash>)> {
    static RECORD: OnceLock<Mutex<(Option<PathBuf>, Vec<SessionCrash>)>> = OnceLock::new();
    RECORD.get_or_init(|| Mutex::new((None, Vec::new())))
}

/// The panics raised in this process outside every session since it started
/// (kind `daemon`), oldest first: the control plane's own faults, which no
/// session's restart heals.
#[must_use]
pub fn daemon_crashes() -> Vec<SessionCrash> {
    daemon_crash_record()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .1
        .clone()
}

/// Account for every panic in this process. One raised on a hosted
/// session's thread is attributed to that session, which ends alone. One
/// raised anywhere else is the daemon's own: it is kept for
/// [`daemon_crashes`] and appended to `crash_log` as kind `daemon`. A panic
/// its caller catches and handles
/// ([`session_scope::catching_panics`]) is neither. The previous hook runs
/// after each, so the panic is printed as before. Installing again only
/// moves the log.
pub fn install_panic_hook(crash_log: PathBuf) {
    daemon_crash_record()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .0 = Some(crash_log);
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info.payload();
            let crash = |kind: &str| SessionCrash {
                kind: kind.to_string(),
                message: payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic with a non-text payload".to_string()),
                location: info.location().map_or_else(String::new, |location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                }),
                thread: std::thread::current()
                    .name()
                    .unwrap_or("unnamed")
                    .to_string(),
                at: crate::util::now_iso(),
            };
            match session_scope::current().and_then(|scope| scope.slot::<HostControl>()) {
                Some(control) => control.fault(crash("panic")),
                None if session_scope::panics_are_caught() => {}
                None => {
                    use std::io::Write as _;
                    let crash = crash("daemon");
                    let mut record = daemon_crash_record()
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    let appended = record.0.as_ref().map(|path| {
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                            .and_then(|mut log| {
                                let line = serde_json::json!({
                                    "kind": crash.kind,
                                    "message": crash.message,
                                    "location": crash.location,
                                    "thread": crash.thread,
                                    "at": crash.at,
                                });
                                log.write_all(format!("{line}\n").as_bytes())?;
                                log.sync_data()
                            })
                    });
                    if let Some(Err(error)) = appended {
                        // A hook must not panic: a closed stderr is ignored.
                        let _ = writeln!(
                            std::io::stderr(),
                            "the daemon's crash log could not be appended: {error}"
                        );
                    }
                    record.1.push(crash);
                }
            }
            previous(info);
        }));
    });
}

/// One hosted lifetime, held by the supervisor.
#[derive(Debug)]
pub(crate) struct HostedSession {
    pid: u32,
    generation: String,
    control: Arc<HostControl>,
    exit: tokio::sync::watch::Receiver<Option<HostedExit>>,
}

impl std::fmt::Debug for HostControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostControl")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl Drop for HostedSession {
    fn drop(&mut self) {
        crashes()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.pid);
    }
}

/// What a hosted session is launched with.
pub(crate) struct HostedLaunch {
    pub(crate) worker_id: String,
    pub(crate) cwd: PathBuf,
    /// The session's environment over the daemon's own.
    pub(crate) env: BTreeMap<String, String>,
    /// Daemon variables the session must not see.
    pub(crate) removed_env: BTreeSet<String>,
}

impl HostedSession {
    /// Start a hosted session: its thread, scope and runtime. The worker
    /// reads its configuration from the scope's environment exactly as a
    /// worker process read its own.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system refuses the thread.
    pub(crate) fn start(launch: HostedLaunch) -> std::io::Result<Self> {
        static NEXT: OnceLock<AtomicU32> = OnceLock::new();
        // A random start keeps a restarted daemon from reissuing the ids its
        // predecessor's descriptors still name.
        let next = NEXT.get_or_init(|| AtomicU32::new(uuid::Uuid::new_v4().as_fields().0));
        let pid = HOSTED_PID_BASE | (next.fetch_add(1, Ordering::Relaxed) & (HOSTED_PID_BASE - 1));
        let generation = format!("hosted:{}:{}", std::process::id(), uuid::Uuid::new_v4());
        let control = Arc::new(HostControl {
            pid,
            end: tokio::sync::Notify::new(),
            crash: Mutex::new(None),
        });
        let (exit_tx, exit) = tokio::sync::watch::channel(None);
        live()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pid, (generation.clone(), Arc::clone(&control)));
        let thread_control = Arc::clone(&control);
        let thread_name = format!(
            "session-{}",
            launch.worker_id.chars().take(8).collect::<String>()
        );
        let spawned = std::thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || {
                let scope =
                    SessionScope::new(launch.worker_id, launch.cwd, launch.env, launch.removed_env);
                scope.set_slot(Arc::clone(&thread_control));
                // A cell whose host call outlives its interrupt ends the
                // session instead of wedging it.
                let stuck_control = Arc::clone(&thread_control);
                scope.set_slot(Arc::new(pa_core::kernel::StuckCellSink(Box::new(
                    move |detail| {
                        stuck_control.fault(SessionCrash {
                            kind: "stuck".to_string(),
                            message: detail.to_string(),
                            location: String::new(),
                            thread: std::thread::current()
                                .name()
                                .unwrap_or("unnamed")
                                .to_string(),
                            at: crate::util::now_iso(),
                        });
                    },
                ))));
                let _scope = session_scope::enter(Some(Arc::clone(&scope)));
                let ended = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name(thread_name)
                    .on_thread_start(session_scope::inherit())
                    .build()
                {
                    Ok(runtime) => {
                        let ended = runtime.block_on(async {
                            tokio::select! {
                                served = crate::worker::serve_configured_worker() => {
                                    served.err().map(|error| format!("{error:#}"))
                                }
                                () = thread_control.end.notified() => None,
                            }
                        });
                        runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
                        ended
                    }
                    Err(error) => Some(format!("build the session runtime: {error}")),
                };
                // Whatever of this lifetime is still running (a stuck call,
                // a blocking thread the shutdown did not wait for) is cut
                // off from the session's durable state before the next
                // lifetime can exist.
                scope.end();
                live()
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&pid);
                let crash = thread_control
                    .crash
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                exit_tx.send_replace(Some(match crash {
                    Some(crash) => HostedExit::Crashed(crash),
                    None => HostedExit::Ended(ended),
                }));
            });
        if let Err(error) = spawned {
            live()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&pid);
            return Err(error);
        }
        Ok(Self {
            pid,
            generation,
            control,
            exit,
        })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn generation(&self) -> &str {
        &self.generation
    }

    /// How the lifetime ended, once it has.
    pub(crate) fn ended(&self) -> Option<HostedExit> {
        self.exit.borrow().clone()
    }

    /// Wait for the lifetime to end.
    pub(crate) async fn wait(&mut self) -> HostedExit {
        match self.exit.wait_for(Option::is_some).await {
            Ok(exit) => exit.clone().unwrap_or(HostedExit::Ended(None)),
            // The host thread died without reporting: only a panic of the
            // host thread itself drops the sender unsent.
            Err(_) => HostedExit::Ended(Some("the session's host thread ended".to_string())),
        }
    }

    /// Stop the session now and wait until its host thread has let go.
    pub(crate) async fn kill(&mut self) {
        self.control.end.notify_one();
        self.wait().await;
    }
}

#[cfg(test)]
mod tests;
