//! The trust gate: what a session's kernel may do while it runs an untrusted
//! task.
//!
//! A task carries the trust of the event that queued it (`Trust::Untrusted`
//! for anything carrying outside content, and everything such an event
//! causes). The session's host sets [`TaskTrust`] as a task starts and clears
//! it as the task ends; the session's engine is built with a [`TrustGate`]
//! over the same cell as its kernel's `CallPolicy`, so every host request a
//! cell sends and every side-effecting native it calls is asked about first.
//! An owner's task, and the time between tasks (a kernel prewarm, a
//! background command's report), is admitted everywhere. An untrusted task
//! is admitted only for the calls [`classes`] marks as reading the session's
//! working directory or touching the session's own context and todo list, and for the kernel's reports about its own work; every
//! other call, and any request the table does not list, is refused with
//! "refused for an untrusted turn: <call>".

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_core::kernel::{CallKind, CallPolicy, CallRefused};
use pa_types::event_log::{Cause, Trust};
use serde_json::Value;

use crate::lock;

mod classes;
#[cfg(test)]
mod tests;

use classes::{native_class, CallClass, HOST_REQUESTS};

/// The trust of the task a session runs now, and the event that caused it:
/// set by its host as a task starts, cleared as it ends, and read by the
/// session's [`TrustGate`], its stream (whose events carry the trust) and
/// whatever its turn publishes for others (whose cause the trigger is).
#[derive(Clone, Default)]
pub(crate) struct TaskTrust(Arc<Mutex<Option<RunningTask>>>);

/// What a running task's events carry: its trust, and its trigger as their
/// cause (so they sit one hop past it in its chain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningTask {
    pub(crate) trust: Trust,
    pub(crate) cause: Cause,
}

impl TaskTrust {
    /// Sets the running task; `None` once no task runs.
    pub(crate) fn set(&self, task: Option<RunningTask>) {
        *lock(&self.0) = task;
    }

    /// The running task's trust, `Owner` when no task runs.
    pub(crate) fn current(&self) -> Trust {
        lock(&self.0)
            .as_ref()
            .map_or(Trust::Owner, |task| task.trust)
    }

    /// The running task's trigger, as the cause of what it publishes;
    /// `None` when no task runs.
    pub(crate) fn cause(&self) -> Option<Cause> {
        lock(&self.0).as_ref().map(|task| task.cause.clone())
    }
}

/// A session's call policy by the trust of the task it runs.
pub(crate) struct TrustGate {
    trust: TaskTrust,
    dir: WorkingDir,
}

impl TrustGate {
    /// The gate of a session working in `cwd` that runs its tasks with
    /// `trust`.
    pub(crate) fn new(trust: TaskTrust, cwd: &Path) -> Self {
        TrustGate {
            trust,
            dir: WorkingDir::new(cwd),
        }
    }
}

/// A session's working directory: what its untrusted reads, and the folders
/// its kernel watches, stay under.
#[derive(Debug, Clone)]
pub(crate) struct WorkingDir {
    /// As given and lexically normalized.
    cwd: PathBuf,
    /// The same directory with its links resolved.
    real_cwd: PathBuf,
}

impl WorkingDir {
    pub(crate) fn new(cwd: &Path) -> Self {
        let cwd = normalized(cwd);
        WorkingDir {
            real_cwd: pa_types::platform::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone()),
            cwd,
        }
    }

    /// `path`, resolved against the directory, when it is under it: one
    /// that exists followed through its links (and answered so), one that
    /// does not with its `.` and `..` folded away. A path that climbs out,
    /// or links out, is `None`.
    pub(crate) fn under(&self, path: &str) -> Option<PathBuf> {
        let path = normalized(&self.cwd.join(path));
        match pa_types::platform::canonicalize(&path) {
            Ok(real) => real.starts_with(&self.real_cwd).then_some(real),
            Err(_) => path.starts_with(&self.cwd).then_some(path),
        }
    }
}

/// `path` with its `.` and `..` components folded away.
fn normalized(path: &Path) -> PathBuf {
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                folded.pop();
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                folded.push(component);
            }
        }
    }
    folded
}

impl CallPolicy for TrustGate {
    fn admit(&self, call: CallKind<'_>) -> Result<(), CallRefused> {
        match self.trust.current() {
            Trust::Owner => return Ok(()),
            Trust::Untrusted => {}
        }
        let (name, admitted) = match call {
            CallKind::HostRequest { kind, payload } => {
                let class = HOST_REQUESTS
                    .iter()
                    .find(|(listed, _)| *listed == kind)
                    .map_or(CallClass::Privileged, |(_, class)| *class);
                let admitted = match class {
                    CallClass::WorkspaceRead => {
                        // A path resolves against the session's directory,
                        // never the payload's own `cwd`, which must be
                        // under it too; one that exists is followed through
                        // its links.
                        let under_cwd = |path: &str| self.dir.under(path).is_some();
                        let named = |key: &str| payload.get(key).and_then(Value::as_str);
                        let target = if kind == "fs.read" { "path" } else { "root" };
                        // A glob's pattern is joined to its root: it may not
                        // climb out of it or start anywhere else.
                        // (`..` anywhere, braces included, is refused).
                        let pattern_stays = named("pattern").is_none_or(|pattern| {
                            kind != "fs.glob"
                                || (Path::new(pattern).is_relative() && !pattern.contains(".."))
                        });
                        named(target).is_some_and(under_cwd)
                            && named("cwd").is_none_or(under_cwd)
                            && pattern_stays
                    }
                    CallClass::OwnSession | CallClass::KernelReport => true,
                    CallClass::Privileged => false,
                };
                (kind, admitted)
            }
            CallKind::Native { family, name } => {
                let admitted = match native_class(family) {
                    // The file reader's own `fs.*` request is checked with
                    // its paths.
                    CallClass::WorkspaceRead | CallClass::OwnSession | CallClass::KernelReport => {
                        true
                    }
                    CallClass::Privileged => false,
                };
                (name, admitted)
            }
        };
        if admitted {
            Ok(())
        } else {
            Err(CallRefused {
                call: name.to_string(),
            })
        }
    }
}
