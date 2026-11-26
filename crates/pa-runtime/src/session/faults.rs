//! What a session's crash does to it.
//!
//! Under [`SessionFaults::Contained`] (the print modes) a turn that panics
//! fails its task with the panic's text and the next task runs on a fresh
//! engine; nothing else is told.
//!
//! Under [`SessionFaults::Supervised`] (the runtime daemon) the session's
//! lifetimes are supervised as the old daemon supervises its hosted
//! sessions, with the same policy (`pa_core::session::crash`):
//!
//! - **Attribution.** Each lifetime's scope carries a
//!   [`pa_core::session::crash::PanicSink`]: the process panic hook hands it
//!   every panic raised on one of the session's threads. One nothing catches
//!   (a task on the session's runtime) ends the lifetime at once; one the
//!   host's own work loop catches ends it with the panic's location. A cell
//!   still on the engine after its interrupt's grace
//!   ([`pa_core::kernel::StuckCellSink`]) ends it as `stuck`: the thread it
//!   holds is abandoned with the lifetime's runtime.
//! - **Visibility.** The work that was running fails with the crash's reason
//!   ([`SessionCrash::reason`]) and is not run again; the crash is appended
//!   to the crash log (the `crashes` table), stored as `agent/<id>/crashed`,
//!   and the session's commands' programs it left running are stopped (its
//!   orphan journal).
//! - **Self-heal.** The session is rebuilt from its transcript in a new
//!   lifetime after 250 ms, doubling per crash in a row to 30 s; a lifetime
//!   that lived 30 s starts the count again. Before the engine adopts the
//!   transcript, the transcript keeps the fault (display-only rows) and each
//!   call the crash cut off is answered once as of unknown outcome; the new
//!   engine tells the model once that its kernel definitions are gone
//!   (`pa_core`'s resume notice). Once built, `agent/<id>/recovered`. After
//!   the sixth crash in a row the session is given up: `agent/<id>/failed`,
//!   and its work fails until it is closed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use pa_core::session::crash::{Caught, PanicSink, SessionCrash};
use pa_core::session::revival::{interrupted_call_results, LifetimeEnd};
use pa_types::daemon::{SessionCrashInfo, SessionFaultEvent, SESSION_FAULT_CUSTOM_TYPE};
use pa_types::event_log::{NewEvent, Trust};
use pa_types::session::FileEntry;
use pa_types::session_scope::SessionScope;
use tokio::sync::watch;

use super::{Outbox, SessionSpec, SessionTopics};
use crate::lock;

/// What a session's crash does to it.
#[derive(Clone)]
pub(crate) enum SessionFaults {
    /// The turn fails with the panic's text and the next task runs on a
    /// fresh engine.
    Contained,
    /// The runtime daemon's supervision.
    Supervised(Arc<Supervision>),
}

/// The runtime daemon's supervision of its sessions.
pub(crate) struct Supervision {
    /// The crash log every crash is appended to
    /// (`pa_core::agent_state::append_crash_record`).
    pub(crate) crash_log: PathBuf,
    /// Counts each crash, by its kind (the daemon's `session_crashed`
    /// telemetry, fire-and-forget).
    pub(crate) counted: Box<dyn Fn(&str) + Send + Sync>,
}

/// A session being rebuilt after a crash, and what the new lifetime owes
/// its transcript and clients.
#[derive(Debug, Clone)]
pub(crate) struct Rebuild {
    pub(crate) crash: SessionCrash,
    /// How many crashes in a row the session has had.
    pub(crate) failures: u32,
    /// How many programs its commands left running were stopped.
    pub(crate) reaped_programs: usize,
}

/// One lifetime's faults: the panics raised on its threads, and the fault
/// that ended it, once one has.
#[derive(Default)]
pub(crate) struct LifetimeFaults {
    /// The last panic each thread raised, for the work loop to read the one
    /// it caught.
    panics: Mutex<HashMap<ThreadId, SessionCrash>>,
    fault: watch::Sender<Option<SessionCrash>>,
}

impl LifetimeFaults {
    /// Puts the lifetime's panic sink and stuck-cell sink on `scope`.
    pub(crate) fn install(self: &Arc<Self>, scope: &SessionScope) {
        let faults = Arc::clone(self);
        scope.set_slot(Arc::new(PanicSink(Box::new(move |crash, caught| {
            lock(&faults.panics).insert(std::thread::current().id(), crash.clone());
            match caught {
                Caught::Caught => {}
                Caught::Uncaught => faults.end(crash),
            }
        }))));
        let faults = Arc::clone(self);
        scope.set_slot(Arc::new(pa_core::kernel::StuckCellSink(Box::new(
            move |detail| faults.end(SessionCrash::now("stuck", detail.to_string())),
        ))));
    }

    /// Ends the lifetime with `crash`, unless it has ended already.
    fn end(&self, crash: SessionCrash) {
        self.fault.send_if_modified(|fault| {
            let first = fault.is_none();
            if first {
                *fault = Some(crash);
            }
            first
        });
    }

    /// The fault that ended the lifetime, once one has.
    pub(crate) fn fault(&self) -> Option<SessionCrash> {
        self.fault.borrow().clone()
    }

    /// Resolves once a fault has ended the lifetime.
    pub(crate) async fn faulted(&self) -> SessionCrash {
        let mut fault = self.fault.subscribe();
        if let Ok(fault) = fault.wait_for(Option::is_some).await {
            if let Some(crash) = fault.clone() {
                return crash;
            }
        }
        // The sender lives in `self`: it never closes while this waits.
        std::future::pending().await
    }

    /// The crash of the panic the calling thread raised last, which its
    /// caller has caught.
    pub(crate) fn caught(&self, panic: &(dyn std::any::Any + Send)) -> SessionCrash {
        lock(&self.panics)
            .remove(&std::thread::current().id())
            .unwrap_or_else(|| SessionCrash::now("panic", panic_text(panic)))
    }
}

/// A panic payload's text.
pub(crate) fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with a non-text payload".to_string())
}

/// The orphan journal `spec`'s kernel records the programs its commands
/// start in, when its environment names one.
pub(crate) fn orphan_journal(spec: &SessionSpec) -> Option<PathBuf> {
    spec.env
        .get(pa_core::kernel::orphan_journal::ORPHAN_PROCESS_JOURNAL_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// Stops the programs the journal at `path` lists as started by process
/// `owner` and still running, and clears it; how many it stopped.
pub(crate) fn reap_orphans(path: &Path, owner: u32) -> usize {
    i32::try_from(owner).map_or(0, |owner| {
        pa_core::kernel::orphan_journal::reap_worker_orphan_processes(path, owner)
    })
}

/// Makes `crash` of session `id` visible: appended to the crash log, logged,
/// and stored as `agent/<id>/crashed` (the crash's own fields, a
/// `session_crashed` fault event).
pub(crate) fn record_crash(
    supervision: &Supervision,
    outbox: &Outbox,
    topics: &SessionTopics,
    id: &str,
    rebuild: &Rebuild,
) {
    let crash = &rebuild.crash;
    let record = serde_json::json!({
        "sessionId": id,
        "kind": crash.kind,
        "message": crash.message,
        "location": crash.location,
        "thread": crash.thread,
        "at": crash.at,
        "failures": rebuild.failures,
        "reapedPrograms": rebuild.reaped_programs,
    });
    if let Err(error) = pa_core::agent_state::append_crash_record(&supervision.crash_log, &record) {
        tracing::error!("session {id}'s crash was not appended to the crash log: {error:#}");
    }
    (supervision.counted)(&crash.kind);
    tracing::error!(
        "session {id} crashed ({}): {} at {} on thread {}",
        crash.kind,
        crash.message,
        crash.location,
        crash.thread
    );
    publish_fault(
        outbox,
        &topics.crashed,
        id,
        &SessionFaultEvent::SessionCrashed(SessionCrashInfo {
            kind: crash.kind.clone(),
            message: crash.message.clone(),
            location: crash.location.clone(),
            thread: crash.thread.clone(),
            at: crash.at.clone(),
            state: None,
        }),
    );
}

/// Stores `fault` on `topic`, a system event of session `id`.
pub(crate) fn publish_fault(
    outbox: &Outbox,
    topic: &pa_types::event_log::Topic,
    id: &str,
    fault: &SessionFaultEvent,
) {
    outbox.send(NewEvent {
        topic: topic.clone(),
        source: id.to_string(),
        session: Some(id.to_string()),
        payload: serde_json::to_value(fault).unwrap_or_default(),
        trust: Trust::Owner,
        cause: None,
        reply: None,
    });
}

/// What the transcript keeps of a session's fault, and what the crash owes
/// it, written before the next engine adopts it: the fault's display-only
/// rows (the crash, then `then`: its recovery or its give-up), once per
/// crash; and, when the session is rebuilt, a result for each call the crash
/// cut off. A session without a saved transcript keeps nothing.
pub(crate) fn keep_fault(spec: &SessionSpec, rebuild: &Rebuild, then: &SessionFaultEvent) {
    let Some(path) = spec.transcript.as_deref() else {
        return;
    };
    if !pa_core::session::store::store().exists(path) {
        return;
    }
    let crash = &rebuild.crash;
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut manager = pa_core::session::manager::SessionManager::open(&spec.cwd, dir, path);
    let kept = manager.get_all_entries().iter().any(|entry| {
        let FileEntry::CustomMessage { payload, .. } = entry else {
            return false;
        };
        payload.custom_type == SESSION_FAULT_CUSTOM_TYPE
            && payload
                .details
                .as_ref()
                .is_some_and(|details| details["at"] == crash.at.as_str())
    });
    if !kept {
        let crashed = SessionFaultEvent::SessionCrashed(SessionCrashInfo {
            kind: crash.kind.clone(),
            message: crash.message.clone(),
            location: crash.location.clone(),
            thread: crash.thread.clone(),
            at: crash.at.clone(),
            state: None,
        });
        let then_text = match then {
            SessionFaultEvent::SessionRecovered { .. } => {
                "Session recovered: it was rebuilt from its transcript.".to_string()
            }
            SessionFaultEvent::SessionFailed { failures } => format!(
                "Session failed: it crashed {failures} times in a row and was not rebuilt again."
            ),
            SessionFaultEvent::SessionCrashed(_) => crash.reason(),
        };
        for (event, content) in [(&crashed, crash.reason()), (then, then_text)] {
            if let Err(error) = manager.append_custom_message_entry(
                SESSION_FAULT_CUSTOM_TYPE,
                pa_types::ai::UserContent::Text(content),
                /*display*/ true,
                serde_json::to_value(event).ok(),
            ) {
                tracing::error!("the session's fault row was not saved: {error:#}");
            }
        }
    }
    match then {
        SessionFaultEvent::SessionRecovered { .. } => {}
        SessionFaultEvent::SessionFailed { .. } | SessionFaultEvent::SessionCrashed(_) => return,
    }
    let branch: Vec<serde_json::Value> = manager
        .get_branch(None)
        .into_iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    let (results, _) = interrupted_call_results(
        branch
            .iter()
            .map(|row| (row["type"].as_str().unwrap_or_default(), row)),
        LifetimeEnd::of_crash_kind(Some(&crash.kind)),
        u64::try_from(rebuild.reaped_programs).unwrap_or(u64::MAX),
    );
    for result in results {
        let saved = serde_json::from_value(result)
            .map_err(std::io::Error::other)
            .and_then(|message| manager.append_message(message));
        if let Err(error) = saved {
            tracing::error!("an interrupted tool call was not answered: {error:#}");
        }
    }
}
