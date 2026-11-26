//! The daemon's restart owner (the "keeper").
//!
//! Every session lives inside one daemon process, so a fault no panic hook
//! can contain (a native stack overflow, out of memory, a fault in native
//! code, a SIGKILL) ends all of them at once. The keeper is the process that
//! brings the daemon back: a parent whose whole job is to start the daemon,
//! wait for it, and start it again with backoff when it ends abnormally.
//!
//! It hosts no session, builds no engine and talks to no provider, so it
//! cannot share the daemon's fault: it runs one single-threaded runtime that
//! waits on a child and three signals, and between lifetimes it appends one
//! line to a log. The restarted daemon's own boot does the healing (the boot
//! reap, the adoption of on-disk sessions, the revival of interrupted turns).
//!
//! What the keeper decides, and from what:
//!
//! - **A deliberate end is not restarted.** The daemon exits with status 0
//!   on a `shutdown`, on a signal drain and on an update handoff; those all
//!   end the keeper too. A stop that escalates to a kill (a forced shutdown,
//!   the installer) leaves a stop intent first ([`record_stop_intent`]),
//!   which the keeper reads the same way.
//! - **One keeper per socket.** A keeper records itself as the socket's
//!   owner when it starts. A keeper that finds another recorded owner never
//!   restarts anything: a client that launched a fresh daemon while this one
//!   was backing off has taken the socket over.
//! - **A crash loop ends.** Deaths without a stable lifetime between them
//!   back off (doubling) and stop at a limit; every death is appended to
//!   [`DAEMON_DEATH_LOG`] and handed to the next daemon, whose
//!   `daemon_hello` shows it to every client.
//!
//! - **Either daemon.** A kept launch keeps whichever daemon its command line
//!   starts ([`KeptDaemon`]): the supervisor, or the runtime daemon when that
//!   is selected. The runtime daemon's keeper keeps its records under the
//!   runtime daemon's own socket (`daemon2.sock`), so the two daemons' keepers
//!   on one agent dir never take each other's socket over or read each other's
//!   deaths; the runtime daemon records its keeper in its lock file, so the
//!   supervisor's boot reap spares both.
//!
//! A kept launch is the ordinary daemon command line plus [`KEEPER_ENV`]. The
//! launched process splits in two first thing in `main` ([`enter`]): the copy
//! is the daemon, booting exactly as an unkept one, and the original becomes
//! the keeper, so a launch waits on nothing the keeper does (measured: the
//! time to the daemon's hello is the same with and without one). A later
//! lifetime is started as a new program with the same arguments. A binary
//! that predates the keeper ignores the variable and runs unkept, which is
//! what an update rollback relies on.
//!
//! Limits. The keeper itself is not kept: if it is killed, the daemon runs on
//! unkept until a client's next launch finds no daemon. A daemon that is
//! killed outright by something that recorded no stop intent is restarted,
//! whatever the killer meant. Unix only: on Windows nothing splits and the
//! daemon runs unkept, as before.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(any(unix, test))]
use std::time::Duration;

use pa_types::daemon::DaemonDeathRecord;

/// Set (to anything) on a daemon launch to make that process the keeper of
/// the daemon it would otherwise have been.
pub const KEEPER_ENV: &str = "PRIME_AGENT_DAEMON_KEEPER";

/// The keeper's pid, on the daemon it started.
const KEEPER_PID_ENV: &str = "PRIME_AGENT_DAEMON_KEEPER_PID";

/// The death the daemon is being started after (one JSON
/// [`DaemonDeathRecord`]), on the daemon the keeper starts next.
const DEATH_ENV: &str = "PRIME_AGENT_DAEMON_DEATH";

/// The append-only log of daemon deaths inside the worker-records directory:
/// one JSON [`DaemonDeathRecord`] per line.
pub const DAEMON_DEATH_LOG: &str = "daemon-deaths.jsonl";

/// Present while a stop of the daemon is deliberate even if it ends in a kill.
const STOP_INTENT_FILE: &str = "daemon-stop-intent";

/// Names the keeper that owns the socket's daemon. Not `.json`: the
/// worker-records directory's `.json` files are session descriptors.
#[cfg(unix)]
const OWNER_FILE: &str = "daemon-keeper.owner";

/// What this daemon process knows about its keeper.
struct HostEnv {
    keeper_pid: Option<u32>,
    /// The death the keeper started this daemon after.
    death: Option<DaemonDeathRecord>,
    /// Whether this daemon is the keeper's first, split off before the keeper read anything:
    /// a give-up a previous keeper recorded is then read from the log by the daemon itself.
    first_of_its_keeper: bool,
}

static HOST_ENV: OnceLock<HostEnv> = OnceLock::new();

/// The daemon process the keeper split off at its own start (see [`enter`]).
#[cfg(unix)]
static SPLIT_DAEMON: OnceLock<u32> = OnceLock::new();

/// The death this daemon reports in its hello, resolved once.
static BOOT_DEATH: OnceLock<Option<DaemonDeathRecord>> = OnceLock::new();

/// Both binaries call this first thing in `main`, while the process is still
/// single-threaded. `daemon_launch` says whether the command line starts a
/// daemon.
///
/// A daemon launch asked to be kept ([`KEEPER_ENV`]) splits in two here, with
/// no second program start: the copy goes on to be the daemon exactly as an
/// unkept launch would, and the original goes on to be its keeper
/// ([`run_keeper`]). The launch therefore waits on nothing the keeper does.
///
/// Every other process takes the keeper's hand-off out of its environment:
/// the values are kept for [`keeper_pid`] and [`boot_death`], and nothing
/// this process starts (a session's commands, a nested daemon) inherits them.
pub fn enter(
    #[cfg_attr(
        not(unix),
        expect(unused_variables, reason = "only unix has a keeper to split into")
    )]
    daemon_launch: bool,
) {
    #[cfg(unix)]
    if daemon_launch && std::env::var_os(KEEPER_ENV).is_some() {
        use pa_core::platform::process::{parent_pid, split_process, Split};
        match split_process() {
            Ok(Split::Child) => {
                std::env::remove_var(KEEPER_ENV);
                HOST_ENV.get_or_init(|| HostEnv {
                    keeper_pid: Some(parent_pid()),
                    death: None,
                    first_of_its_keeper: true,
                });
            }
            Ok(Split::Parent { child }) => {
                SPLIT_DAEMON.get_or_init(|| child);
            }
            // No second process could be made: this one stays the keeper and
            // starts its first daemon the way it starts every later one.
            Err(_) => {}
        }
        return;
    }
    HOST_ENV.get_or_init(|| {
        let keeper_pid = std::env::var(KEEPER_PID_ENV)
            .ok()
            .and_then(|pid| pid.parse().ok());
        let death = std::env::var(DEATH_ENV)
            .ok()
            .and_then(|record| serde_json::from_str(&record).ok());
        std::env::remove_var(KEEPER_PID_ENV);
        std::env::remove_var(DEATH_ENV);
        HostEnv {
            keeper_pid,
            death,
            first_of_its_keeper: false,
        }
    });
}

/// The keeper that restarts this daemon, when one does.
#[must_use]
pub fn keeper_pid() -> Option<u32> {
    HOST_ENV.get().and_then(|env| env.keeper_pid)
}

/// The death this daemon was started after, when it was started because of
/// one: the death its keeper restarted it after, or, for a keeper's first
/// daemon, the give-up a previous keeper recorded in `descriptor_dir` and had
/// no daemon left to tell.
#[must_use]
pub fn boot_death(descriptor_dir: &Path) -> Option<DaemonDeathRecord> {
    let env = HOST_ENV.get()?;
    BOOT_DEATH
        .get_or_init(|| {
            env.death.clone().or_else(|| {
                env.first_of_its_keeper
                    .then(|| unreported_give_up(&descriptor_dir.join(DAEMON_DEATH_LOG)))
                    .flatten()
            })
        })
        .clone()
}

/// The death log's last record, when it is a give-up: the one death no
/// daemon was started after, so no hello has reported it yet.
fn unreported_give_up(death_log: &Path) -> Option<DaemonDeathRecord> {
    std::fs::read_to_string(death_log)
        .ok()
        .and_then(|text| {
            text.lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .and_then(|line| serde_json::from_str::<DaemonDeathRecord>(line).ok())
        })
        .filter(|record| record.outcome == pa_types::daemon::DaemonDeathOutcome::GaveUp)
}

/// Record that the daemon on `socket_path` is being stopped on purpose, so
/// its keeper does not restart it even when the stop ends in a kill. The
/// daemon records it when a shutdown starts; a caller about to kill a daemon
/// that never answered records it first.
///
/// # Errors
///
/// Returns an error when the record cannot be written.
pub fn record_stop_intent(agent_dir: &Path, socket_path: &Path) -> anyhow::Result<()> {
    crate::descriptor::write_file_atomic(
        &crate::descriptor::descriptor_dir(agent_dir, socket_path).join(STOP_INTENT_FILE),
        &crate::util::now_iso(),
    )
}

/// How the keeper paces restarts and when it stops.
#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RestartPolicy {
    initial_backoff: Duration,
    max_backoff: Duration,
    /// The death that ends the keeper, counted in a row.
    give_up_after: u32,
    /// A lifetime at least this long ends a row of deaths.
    stable_lifetime: Duration,
}

/// The same pacing a hosted session's restart loop uses.
#[cfg(any(unix, test))]
const POLICY: RestartPolicy = RestartPolicy {
    initial_backoff: Duration::from_millis(250),
    max_backoff: Duration::from_secs(30),
    give_up_after: 5,
    stable_lifetime: Duration::from_secs(30),
};

/// What the keeper does about one abnormal death.
#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Restart { consecutive: u32, backoff: Duration },
    GiveUp { consecutive: u32 },
}

#[cfg(any(unix, test))]
impl RestartPolicy {
    /// Judge a death that followed `prior` deaths in a row and a lifetime of
    /// `uptime`.
    fn judge(&self, prior: u32, uptime: Duration) -> Verdict {
        let consecutive = if uptime >= self.stable_lifetime {
            1
        } else {
            prior + 1
        };
        if consecutive >= self.give_up_after {
            return Verdict::GiveUp { consecutive };
        }
        Verdict::Restart {
            consecutive,
            backoff: self
                .initial_backoff
                .saturating_mul(1 << (consecutive - 1).min(16))
                .min(self.max_backoff),
        }
    }
}

/// Which daemon a keeper keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptDaemon {
    /// The supervisor: its sessions are counted from its worker descriptors
    /// when it dies.
    Supervisor,
    /// The runtime daemon (`pa_runtime::daemon`): its sessions are held in
    /// its database, which the keeper does not open, so a death record
    /// leaves them uncounted.
    Runtime,
}

/// What a keeper is started with: the daemon it keeps.
pub struct KeeperOptions {
    /// The daemon executable (this one).
    pub exe: PathBuf,
    /// The daemon's arguments (this process's own).
    pub args: Vec<std::ffi::OsString>,
    /// The socket the kept daemon serves: the keeper's records (owner, stop
    /// intent, death log) are kept per socket, so the runtime daemon's
    /// keeper, keyed by `daemon2.sock`, never meets the supervisor's.
    pub socket_path: PathBuf,
    pub agent_dir: PathBuf,
    pub daemon: KeptDaemon,
}

/// Run as the keeper of the daemon `options` describes until that daemon
/// ends on purpose, another keeper takes the socket over, or the crash-loop
/// limit is reached. Returns the process exit code.
///
/// # Errors
///
/// Returns an error when the keeper cannot set itself up (its runtime, its
/// signal handlers, its owner record).
#[cfg(unix)]
pub fn run_keeper(options: &KeeperOptions) -> anyhow::Result<i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(keep(options))
}

/// One daemon lifetime's end, as the keeper waits for it.
#[cfg(unix)]
type Ended = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<pa_core::platform::process::ChildExit, String>>
            + Send,
    >,
>;

#[cfg(unix)]
async fn keep(options: &KeeperOptions) -> anyhow::Result<i32> {
    use pa_core::platform::process::ChildExit;
    use pa_types::daemon::DaemonDeathOutcome;
    use std::os::unix::process::ExitStatusExt;
    use tokio::signal::unix::{signal, SignalKind};

    let dir = crate::descriptor::descriptor_dir(&options.agent_dir, &options.socket_path);
    let log = crate::paths::RotatingLog::new(crate::paths::daemon_log_path(
        &options.socket_path,
        &options.agent_dir,
    ));
    let owner = format!("{} {}", std::process::id(), uuid::Uuid::new_v4());
    let owner_path = dir.join(OWNER_FILE);
    let intent_path = dir.join(STOP_INTENT_FILE);
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let death_log = dir.join(DAEMON_DEATH_LOG);
    // The daemon split off at this process's start is the first lifetime; without one the
    // keeper starts its first daemon itself, and tells it of a give-up a previous keeper
    // recorded (a split daemon reads that from the log on its own).
    let mut split_daemon = SPLIT_DAEMON.get().copied();
    let mut pending: Option<DaemonDeathRecord> = match split_daemon {
        Some(_) => None,
        None => unreported_give_up(&death_log),
    };
    let mut consecutive = 0;
    let mut first_start = true;
    loop {
        let started = tokio::time::Instant::now();
        let mut stopping = false;
        // The lifetime to wait on: the daemon split off at start, or a daemon started here.
        let (pid, ended): (u32, Result<Ended, String>) = if let Some(pid) = split_daemon.take() {
            (
                pid,
                Ok(Box::pin(async move {
                    tokio::task::spawn_blocking(move || pa_core::platform::process::wait_child(pid))
                        .await
                        .map_err(|error| format!("wait for the daemon: {error}"))?
                        .map_err(|error| format!("wait for the daemon: {error}"))
                })),
            )
        } else {
            {
                let mut command = tokio::process::Command::new(&options.exe);
                command
                    .args(&options.args)
                    .env_remove(KEEPER_ENV)
                    .env(KEEPER_PID_ENV, std::process::id().to_string());
                match &pending {
                    Some(record) => command.env(DEATH_ENV, serde_json::to_string(record)?),
                    None => command.env_remove(DEATH_ENV),
                };
                // Every daemon lifetime owns its session, as a daemon launched unkept does.
                pa_core::platform::process::set_new_session(command.as_std_mut());
                // An update's roster is restored by the daemon the update started, once; a
                // restart after a death boots from the on-disk state alone.
                if !first_start {
                    command.env_remove(pa_types::daemon::update_flow::UPDATE_ROSTER_ENV);
                }
                match command.spawn() {
                    Ok(mut child) => (
                        child.id().unwrap_or(0),
                        Ok(Box::pin(async move {
                            let status = child
                                .wait()
                                .await
                                .map_err(|error| format!("wait for the daemon: {error}"))?;
                            Ok(match status.signal() {
                                Some(signal) => ChildExit::Signal(signal),
                                None => ChildExit::Code(status.code().unwrap_or(-1)),
                            })
                        })),
                    ),
                    Err(error) => (
                        0,
                        Err(format!(
                            "start the daemon {}: {error}",
                            options.exe.display()
                        )),
                    ),
                }
            }
        };
        // The keeper's own records are written once the first daemon is already starting: a
        // launch waits on that daemon's socket, and nothing below is on its way. They are
        // read only when a daemon ends. An intent nobody consumed belongs to a daemon that is
        // gone. A keeper that cannot record itself as the owner restarts nothing (the owner
        // check below fails), which the log says here.
        if first_start {
            let recorded = crate::paths::ensure_dir(&dir)
                .and_then(|()| crate::descriptor::write_file_atomic_unsynced(&owner_path, &owner))
                .and_then(|()| match std::fs::remove_file(&intent_path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error.into()),
                });
            if let Err(error) = recorded {
                log.append(&format!(
                    "{} restart owner {}: could not record itself ({error:#}); this daemon will not be restarted if it dies",
                    crate::util::now_iso(),
                    std::process::id()
                ));
            }
        }
        first_start = false;
        let exit = match ended {
            Ok(mut ended) => loop {
                tokio::select! {
                    exit = &mut ended => break exit,
                    _ = terminate.recv() => {}
                    _ = interrupt.recv() => {}
                    _ = hangup.recv() => {}
                }
                // A signal to the keeper stops the daemon it keeps, for good. A failed
                // delivery means the daemon is already gone, which the wait reports next.
                stopping = true;
                if let (Ok(raw), true) = (i32::try_from(pid), pid != 0) {
                    let _ = pa_core::platform::process::kill_pid(
                        raw,
                        pa_core::platform::process::Signal::Term,
                    );
                }
            },
            Err(error) => Err(error),
        };
        let superseded =
            std::fs::read_to_string(&owner_path).ok().as_deref() != Some(owner.as_str());
        if superseded {
            log.append(&format!(
                "{} restart owner {}: another restart owner holds this socket; leaving",
                crate::util::now_iso(),
                std::process::id()
            ));
            return Ok(0);
        }
        let deliberate = stopping || exit == Ok(ChildExit::Code(0)) || intent_path.exists();
        if deliberate {
            // Best effort: a leftover owner or intent record is replaced or
            // cleared by the next keeper's start.
            let _ = std::fs::remove_file(&intent_path);
            let _ = std::fs::remove_file(&owner_path);
            return Ok(0);
        }
        let uptime = started.elapsed();
        let verdict = POLICY.judge(consecutive, uptime);
        // Reads of the dead daemon's own records, kept off this thread so a
        // fault in them cannot end the keeper.
        let counted = match options.daemon {
            KeptDaemon::Runtime => None,
            KeptDaemon::Supervisor => {
                let dir = dir.clone();
                let socket_path = options.socket_path.clone();
                tokio::task::spawn_blocking(move || {
                    let hosted: Vec<_> = crate::descriptor::load_descriptors(&dir, &socket_path)
                        .into_iter()
                        .filter(|(_, descriptor)| {
                            descriptor.lifecycle != pa_types::daemon::DaemonWorkerLifecycle::Failed
                        })
                        .collect();
                    let busy = hosted
                        .iter()
                        .filter(|(_, descriptor)| {
                            crate::journal::WorkerRecoveryJournal::read_interrupted(Path::new(
                                &descriptor.recovery_journal_path,
                            ))
                        })
                        .count();
                    (hosted.len() as u64, busy as u64)
                })
                .await
                .ok()
            }
        };
        let record = DaemonDeathRecord {
            at: crate::util::now_iso(),
            pid,
            exit_code: match exit {
                Ok(ChildExit::Code(code)) => Some(code),
                Ok(ChildExit::Signal(_)) | Err(_) => None,
            },
            signal: match exit {
                Ok(ChildExit::Signal(signal)) => Some(signal),
                Ok(ChildExit::Code(_)) | Err(_) => None,
            },
            error: exit.as_ref().err().cloned(),
            uptime_ms: uptime.as_millis() as u64,
            live_sessions: counted.map(|(live, _)| live),
            busy_sessions: counted.map(|(_, busy)| busy),
            consecutive: match verdict {
                Verdict::Restart { consecutive, .. } | Verdict::GiveUp { consecutive } => {
                    consecutive
                }
            },
            outcome: match verdict {
                Verdict::Restart { .. } => DaemonDeathOutcome::Restarted,
                Verdict::GiveUp { .. } => DaemonDeathOutcome::GaveUp,
            },
        };
        let line = format!("{}\n", serde_json::to_string(&record)?);
        let appended = {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&death_log)
                .and_then(|mut file| {
                    file.write_all(line.as_bytes())?;
                    file.sync_data()
                })
        };
        // The restart matters more than its record: a log that cannot be
        // written is reported in the daemon log and the daemon still returns.
        if let Err(error) = appended {
            log.append(&format!(
                "{} restart owner {}: the daemon death log could not be appended: {error}",
                crate::util::now_iso(),
                std::process::id()
            ));
        }
        let how = match (&record.signal, &record.exit_code, &record.error) {
            (Some(signal), _, _) => format!("was killed by signal {signal}"),
            (None, Some(code), _) => format!("exited with status {code}"),
            (None, None, Some(error)) => format!("did not run ({error})"),
            (None, None, None) => "ended without a status".to_string(),
        };
        match verdict {
            Verdict::GiveUp { consecutive } => {
                log.append(&format!(
                    "{} restart owner {}: daemon pid {pid} {how} after {}ms; {consecutive} deaths in a row, giving up (the next client launch starts a daemon again)",
                    record.at,
                    std::process::id(),
                    record.uptime_ms
                ));
                // Best effort, as above.
                let _ = std::fs::remove_file(&owner_path);
                return Ok(1);
            }
            Verdict::Restart {
                consecutive: row,
                backoff,
            } => {
                log.append(&format!(
                    "{} restart owner {}: daemon pid {pid} {how} after {}ms; restarting in {}ms (death {row}/{} in a row)",
                    record.at,
                    std::process::id(),
                    record.uptime_ms,
                    backoff.as_millis(),
                    POLICY.give_up_after
                ));
                consecutive = row;
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    _ = terminate.recv() => return Ok(0),
                    _ = interrupt.recv() => return Ok(0),
                    _ = hangup.recv() => return Ok(0),
                }
            }
        }
        pending = Some(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deaths_in_a_row_back_off_then_give_up_and_a_stable_lifetime_starts_the_row_again() {
        let quick = Duration::from_millis(10);
        let verdicts: Vec<Verdict> = (0..5).map(|prior| POLICY.judge(prior, quick)).collect();
        let restart = |consecutive, millis| Verdict::Restart {
            consecutive,
            backoff: Duration::from_millis(millis),
        };
        assert_eq!(
            (verdicts, POLICY.judge(4, POLICY.stable_lifetime)),
            (
                vec![
                    restart(1, 250),
                    restart(2, 500),
                    restart(3, 1000),
                    restart(4, 2000),
                    Verdict::GiveUp { consecutive: 5 },
                ],
                restart(1, 250)
            )
        );
    }
}
