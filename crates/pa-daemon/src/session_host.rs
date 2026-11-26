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
//!   from a range no operating system hands out and a generation string
//!   (`pa_core::session::hosted`). Descriptors, leases and registrations
//!   record that pair where they recorded a worker's pid and start id;
//!   "alive" means the generation is registered as running in this daemon,
//!   never that some pid exists.
//! - **Containment.** The process panic hook
//!   (`pa_core::session::crash::install_panic_hook`)
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
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use pa_core::session::crash::{PanicSink, SessionCrash};
use pa_core::session::hosted;
use pa_types::session_scope::{self, SessionScope};

/// How long a session's runtime may take to drop its tasks once the session
/// has ended; threads still blocked after that are abandoned. Tasks drop at
/// their next await, so only a blocking call in flight waits this out, and
/// every stop of such a session pays it.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);

/// The hosted lifetimes running in this process: id -> the handle that ends
/// it. Their generations are registered with `pa_core::session::hosted`.
fn live() -> &'static Mutex<LiveSessions> {
    static LIVE: OnceLock<Mutex<LiveSessions>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Hosted id -> the handle that ends the lifetime.
type LiveSessions = HashMap<u32, Arc<HostControl>>;

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
        .map(Arc::clone);
    let Some(control) = control else {
        return false;
    };
    control.end.notify_one();
    // The host thread removes the lifetime when its runtime is down (bounded
    // by the runtime's shutdown budget).
    while hosted::hosted_alive(pid) {
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

impl HostControl {
    /// What the process panic hook hands this lifetime's panics to: the
    /// first ends the session. The panic's text belongs to the session it
    /// ended: its log gets what the default hook would have printed, and the
    /// daemon's own standard error stays free of it. A panic its caller
    /// catches ends the session too: the session's code is not trusted to
    /// go on after one.
    fn panic_sink(control: &Arc<Self>) -> PanicSink {
        let control = Arc::clone(control);
        PanicSink(Box::new(move |crash, _caught| {
            // The session is ended first, before anything is logged: the
            // unwinding that follows must find its session already going
            // down, so the turn's caller is answered with the crash, not with
            // what the unwound turn leaves behind.
            control.fault(crash.clone());
            crate::worker_stderr::line(format_args!(
                "thread '{}' panicked at {}:\n{}\n{}",
                crash.thread,
                crash.location,
                crash.message,
                std::backtrace::Backtrace::capture()
            ));
        }))
    }
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
        let (pid, generation) = hosted::new_lifetime();
        let control = Arc::new(HostControl {
            pid,
            end: tokio::sync::Notify::new(),
            crash: Mutex::new(None),
        });
        let (exit_tx, exit) = tokio::sync::watch::channel(None);
        live()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pid, Arc::clone(&control));
        hosted::register(pid, generation.clone());
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
                scope.set_slot(Arc::new(HostControl::panic_sink(&thread_control)));
                scope.set_slot(Arc::new(hosted::HostedId(pid)));
                // What the session's code logs outside this crate (the engine's own
                // `eprintln!` lines) goes to the session's log too.
                scope.set_slot(Arc::new(session_scope::LogSink(Box::new(
                    crate::worker_stderr::line,
                ))));
                // A cell whose host call outlives its interrupt ends the
                // session instead of wedging it.
                let stuck_control = Arc::clone(&thread_control);
                scope.set_slot(Arc::new(pa_core::kernel::StuckCellSink(Box::new(
                    move |detail| {
                        stuck_control.fault(SessionCrash::now("stuck", detail.to_string()));
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
                        // The lifetime ends while its runtimes still run
                        // (this one, and the engine's own, which is dropped
                        // with the worker's tasks by the shutdown below):
                        // whatever of it is left is cut off from the
                        // session's durable state before the next lifetime
                        // can exist, and a thread that waits on a runtime
                        // from outside it (the kernel's cell) is stopped and
                        // has left before that runtime goes. Shut down first,
                        // the runtime's timers fail under that thread's wait
                        // and it panics: a crash of a session that only ended.
                        scope.end();
                        runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
                        ended
                    }
                    Err(error) => {
                        scope.end();
                        Some(format!("build the session runtime: {error}"))
                    }
                };
                live()
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&pid);
                hosted::unregister(pid);
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
            hosted::unregister(pid);
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
