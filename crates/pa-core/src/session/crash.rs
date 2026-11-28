//! Session crashes in a process that hosts sessions on its own threads: what
//! one crash is ([`SessionCrash`]), the process panic hook that attributes a
//! panic to the session whose thread raised it ([`install_panic_hook`]), the
//! crash log both daemons append to ([`SESSION_CRASH_LOG`]), and the restart
//! policy a crashed session is healed under ([`restart_backoff_ms`]).
//!
//! The old daemon (`pa-daemon`'s session host) and the runtime daemon
//! (`pa-runtime`'s session host) share all of it: each sets a [`PanicSink`]
//! on the scope of every session it hosts, and the hook hands the sink the
//! crash of every panic raised on one of that session's threads.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

use pa_types::session_scope;
use serde::{Deserialize, Serialize};

/// The append-only crash log's file name: one JSON object per crash, never
/// rewritten. The agent database keeps it under its path
/// (`crate::agent_state`); with the file transcript store, the file holds
/// one record per line.
pub const SESSION_CRASH_LOG: &str = "session-crashes.jsonl";

/// How many crashes in a row a session is rebuilt after; the next one gives
/// it up.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// A session lifetime that lived at least this long proved healthy: its
/// crash starts a fresh count instead of adding to the give-up count.
pub const STABLE_LIFETIME_MS: u64 = 30_000;

/// The first rebuild's delay; each crash in a row doubles it, up to
/// [`MAX_BACKOFF_MS`].
const BASE_BACKOFF_MS: u64 = 250;
const MAX_BACKOFF_MS: u64 = 30_000;

/// How long to wait before rebuilding a session after its `failures`-th
/// crash in a row (counted from 1): 250 ms doubling to 30 s.
#[must_use]
pub fn restart_backoff_ms(failures: u32) -> u64 {
    (BASE_BACKOFF_MS << failures.saturating_sub(1).min(7)).min(MAX_BACKOFF_MS)
}

/// Why and where one hosted session crashed. Everything here is a primitive
/// about the fault itself; `message` is the panic's own text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCrash {
    /// `panic` (unwound and contained), `stuck` (a host call outlived its
    /// interrupt and was abandoned) or `daemon_restart` (the daemon died
    /// under the session's turn).
    pub kind: String,
    pub message: String,
    /// `file:line:column` of the panic; empty otherwise.
    pub location: String,
    /// The name of the thread that raised it.
    pub thread: String,
    pub at: String,
}

impl SessionCrash {
    /// A crash of `kind` with `message`, raised now on the calling thread at
    /// no source location (a stuck call, a daemon's death).
    #[must_use]
    pub fn now(kind: &str, message: String) -> Self {
        SessionCrash {
            kind: kind.to_string(),
            message,
            location: String::new(),
            thread: std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_string(),
            at: crate::session::manager::format_iso_now(),
        }
    }

    /// The crash of kind `kind` that the panic `info` describes.
    #[must_use]
    pub fn of_panic(kind: &str, info: &std::panic::PanicHookInfo<'_>) -> Self {
        let payload = info.payload();
        SessionCrash {
            location: info.location().map_or_else(String::new, |location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            }),
            ..SessionCrash::now(
                kind,
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic with a non-text payload".to_string()),
            )
        }
    }

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

/// Whether the code that raised a panic catches and handles it
/// ([`session_scope::catching_panics`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caught {
    /// A caller on the raising thread catches it.
    Caught,
    /// Nothing on the raising thread catches it: it unwinds the thread, or
    /// the task it ran (which its runtime reports as a join error).
    Uncaught,
}

/// The session scope slot through which the panic hook hands a session's
/// host each panic raised on one of the session's threads (its own, its
/// runtime's, and every thread that inherited its scope), with whether the
/// raising code catches it. The host decides what the panic does to the
/// session; the sink must not panic or block, since it runs inside the
/// panic hook.
pub struct PanicSink(pub Box<dyn Fn(SessionCrash, Caught) + Send + Sync>);

/// Where the process's own panics are recorded, and those recorded so far.
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

/// Accounts for every panic in this process. One raised on a thread whose
/// session scope carries a [`PanicSink`] is that session's: the sink gets
/// it, and nothing else does (its host logs it where the session's lines
/// go). One raised anywhere else is the process's own, unless its caller
/// catches and handles it ([`session_scope::catching_panics`]): it is kept
/// for [`daemon_crashes`] and appended to `crash_log` as kind `daemon`.
/// For every panic that is not a session's the previous hook runs, so it is
/// printed as before. Installing again only moves the log.
pub fn install_panic_hook(crash_log: PathBuf) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    daemon_crash_record()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .0 = Some(crash_log);
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            match session_scope::current().and_then(|scope| scope.slot::<PanicSink>()) {
                Some(sink) => {
                    let caught = if session_scope::panics_are_caught() {
                        Caught::Caught
                    } else {
                        Caught::Uncaught
                    };
                    (sink.0)(SessionCrash::of_panic("panic", info), caught);
                    return;
                }
                None if session_scope::panics_are_caught() => {}
                None => {
                    use std::io::Write as _;
                    let crash = SessionCrash::of_panic("daemon", info);
                    let mut record = daemon_crash_record()
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    let appended = record.0.as_ref().map(|path| {
                        crate::agent_state::append_crash_record(
                            path,
                            &serde_json::json!({
                                "kind": crash.kind,
                                "message": crash.message,
                                "location": crash.location,
                                "thread": crash.thread,
                                "at": crash.at,
                            }),
                        )
                    });
                    if let Some(Err(error)) = appended {
                        // A hook must not panic: a closed stderr is ignored.
                        let _ = writeln!(
                            std::io::stderr(),
                            "the daemon's crash log could not be appended: {error:#}"
                        );
                    }
                    record.1.push(crash);
                }
            }
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 250 ms doubling per crash in a row, capped at 30 s.
    #[test]
    fn the_rebuild_waits_250_ms_doubling_to_30_s() {
        assert_eq!(
            [1, 2, 3, 5, 8, 9, 40].map(restart_backoff_ms),
            [250, 500, 1_000, 4_000, 30_000, 30_000, 30_000]
        );
    }
}
