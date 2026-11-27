//! The sessions the runtime daemon holds open ([`OpenSessions`]), kept in
//! the agent's database as they change, and their revival when the daemon
//! starts after one that died ([`revive`]).
//!
//! A held session is kept with its transcript, working directory and the
//! daemon process that held it (`kv` scope [`SCOPE`], key [`KEY`]). At
//! start, every session a dead daemon held has the programs its commands
//! left running stopped (its orphan journal, by that daemon's pid); one with
//! unfinished tasks is opened again under its lease, its death recorded as
//! its crash (kind `daemon_restart`) and the turn it was running classified
//! by how far its transcript shows it got
//! (`pa_core::session::revival`): run again, continued behind a notice, left
//! over, or, the second death in a row under it, settled as failed. One
//! with nothing unfinished stays down: when it keeps an active scheduled job
//! it is held dormant, its jobs armed (`super::schedules`), and opened
//! under its lease when one is due; with none it is let go. A session
//! stopped on purpose is not held, so its jobs wake nothing until it is
//! opened again. A dormant session is kept in the database too (`"dormant":
//! true`), so a daemon that dies before the wake arms it again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pa_core::session::crash::SessionCrash;
use pa_core::session::lease::{acquire_runtime_session_lease, SessionLease};
use pa_core::session::revival::{
    host_death, transcript_evidence, InterruptedTurn, RevivedTurn, TurnPrompt,
};
use pa_log::{Log, Origin};
use pa_types::event_log::{Task, TaskState};
use serde_json::{json, Value};

use crate::lock;
use crate::runtime::Runtime;
use crate::session::{Rebuild, SessionBuilder, SessionSpec};

use super::handlers::UserBashes;
use super::schedules::Schedules;

/// The `kv` scope and key the held sessions are kept under.
const SCOPE: &str = "runtime-daemon";
const KEY: &str = "sessions";

/// The directory under the agent dir the runtime daemon keeps its sessions'
/// orphan journals and its crash log in.
pub(super) const DIR: &str = "runtime-daemon";

/// The orphan journal of session `id` under `agent_dir`: its kernel records
/// each program its commands start there.
pub(super) fn orphan_journal(agent_dir: &Path, id: &str) -> PathBuf {
    agent_dir.join(DIR).join(format!("{id}.orphans.jsonl"))
}

/// One held session, as the database keeps it (`{"path", "cwd",
/// "daemonPid"}`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    path: PathBuf,
    cwd: PathBuf,
    daemon_pid: u32,
}

impl Held {
    fn of(value: &Value) -> Option<Self> {
        Some(Held {
            path: PathBuf::from(value["path"].as_str()?),
            cwd: PathBuf::from(value["cwd"].as_str()?),
            daemon_pid: u32::try_from(value["daemonPid"].as_u64()?).ok()?,
        })
    }
}

/// A session the daemon died under, as the next one opens it again
/// ([`Runtime::revive_session`]).
pub(crate) struct Revival {
    /// The death, as the session's crash.
    pub(crate) rebuild: Rebuild,
    /// The task that was running, how far its transcript shows it got (`None`
    /// when the transcript could not be read) and what becomes of it.
    pub(crate) running: Option<(Task, Option<RevivedTurn>, InterruptedTurn)>,
}

/// The sessions the daemon holds open, each with its transcript and the
/// lease it holds it by (a session's lease is released when it leaves), and
/// the dormant ones it holds down until a scheduled job of theirs is due.
pub(super) struct OpenSessions {
    log: std::sync::Arc<Log>,
    open: Mutex<HeldSessions>,
    /// The open sessions' `!` commands.
    pub(super) bashes: UserBashes,
}

#[derive(Default)]
struct HeldSessions {
    open: HashMap<String, OpenSession>,
    /// Down, without a lease: each one's transcript and working directory.
    dormant: HashMap<String, (PathBuf, PathBuf)>,
}

pub(super) struct OpenSession {
    pub(super) path: PathBuf,
    pub(super) cwd: PathBuf,
    /// `None` for an ephemeral session, which has no transcript to lease.
    pub(super) lease: Option<SessionLease>,
    /// The client (its `hello` name) an ephemeral session ends with; `None`
    /// for a session that keeps its transcript. An ephemeral session is not
    /// kept in the database: no daemon revives it.
    pub(super) owner: Option<String>,
    /// The scoped model list its `session.cycle_model` steps through
    /// (`session.set_scoped_models`), empty for the whole catalog; kept for
    /// the session's open life only, as the old worker kept it.
    pub(super) scoped_models: Vec<Value>,
}

impl OpenSessions {
    pub(super) fn new(log: std::sync::Arc<Log>) -> Self {
        OpenSessions {
            log,
            open: Mutex::new(HeldSessions::default()),
            bashes: UserBashes::default(),
        }
    }

    /// The transcript of open session `id`.
    pub(super) fn path(&self, id: &str) -> Option<PathBuf> {
        lock(&self.open)
            .open
            .get(id)
            .map(|session| session.path.clone())
    }

    /// The open session whose transcript is `path`.
    pub(super) fn id_of(&self, path: &Path) -> Option<String> {
        lock(&self.open)
            .open
            .iter()
            .find(|(_, session)| session.path == path)
            .map(|(id, _)| id.clone())
    }

    /// The working directory of open session `id`.
    pub(super) fn cwd(&self, id: &str) -> Option<PathBuf> {
        lock(&self.open)
            .open
            .get(id)
            .map(|session| session.cwd.clone())
    }

    pub(super) fn contains(&self, id: &str) -> bool {
        lock(&self.open).open.contains_key(id)
    }

    /// Open session `id`'s scoped model list (empty when none is set).
    pub(super) fn scoped_models(&self, id: &str) -> Vec<Value> {
        lock(&self.open)
            .open
            .get(id)
            .map_or_else(Vec::new, |session| session.scoped_models.clone())
    }

    /// Sets open session `id`'s scoped model list; `false` when it is not
    /// open.
    pub(super) fn set_scoped_models(&self, id: &str, scoped: Vec<Value>) -> bool {
        lock(&self.open)
            .open
            .get_mut(id)
            .map(|session| session.scoped_models = scoped)
            .is_some()
    }

    /// The ephemeral sessions client `owner` created.
    pub(super) fn owned_by(&self, owner: &str) -> Vec<String> {
        lock(&self.open)
            .open
            .iter()
            .filter(|(_, session)| session.owner.as_deref() == Some(owner))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The dormant sessions' transcripts.
    pub(super) fn dormant_paths(&self) -> Vec<PathBuf> {
        lock(&self.open)
            .dormant
            .values()
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Every open session's id and transcript.
    pub(super) fn paths(&self) -> HashMap<String, PathBuf> {
        lock(&self.open)
            .open
            .iter()
            .map(|(id, session)| (id.clone(), session.path.clone()))
            .collect()
    }

    /// Holds `session` as `id` (no longer dormant), kept in the database.
    pub(super) async fn insert(&self, id: String, session: OpenSession) {
        let kept = {
            let mut open = lock(&self.open);
            open.dormant.remove(&id);
            open.open.insert(id, session);
            self.keep(&open)
        };
        if let Err(error) = kept.wait().await {
            tracing::warn!("the runtime daemon's held sessions were not saved: {error:#}");
        }
    }

    /// Whether open session `id` is ephemeral: it keeps no transcript.
    pub(super) fn ephemeral(&self, id: &str) -> bool {
        lock(&self.open)
            .open
            .get(id)
            .is_some_and(|session| session.lease.is_none())
    }

    /// Open session `id` goes on from the transcript at `path` in `cwd`
    /// (a replacement), held by `lease` (`None` keeps the one it holds,
    /// for a transcript it holds already), kept in the database; the lease
    /// it held before is released. `false` when it is not open.
    pub(super) async fn replace(
        &self,
        id: &str,
        path: PathBuf,
        cwd: PathBuf,
        lease: Option<SessionLease>,
    ) -> bool {
        let (replaced, kept) = {
            let mut open = lock(&self.open);
            let Some(session) = open.open.get_mut(id) else {
                return false;
            };
            session.path = path;
            session.cwd = cwd;
            let replaced = lease.map(|lease| session.lease.replace(lease));
            (replaced, self.keep(&open))
        };
        drop(replaced);
        if let Err(error) = kept.wait().await {
            tracing::warn!("the runtime daemon's held sessions were not saved: {error:#}");
        }
        true
    }

    /// Holds session `id` dormant, down on `path` in `cwd`, kept in the
    /// database.
    pub(super) async fn insert_dormant(&self, id: String, path: PathBuf, cwd: PathBuf) {
        let kept = {
            let mut open = lock(&self.open);
            open.dormant.insert(id, (path, cwd));
            self.keep(&open)
        };
        if let Err(error) = kept.wait().await {
            tracing::warn!("the runtime daemon's held sessions were not saved: {error:#}");
        }
    }

    /// Lets session `id` go, open (its lease is released) or dormant, kept
    /// in the database.
    pub(super) async fn remove(&self, id: &str) {
        let kept = {
            let mut open = lock(&self.open);
            open.open.remove(id);
            open.dormant.remove(id);
            self.keep(&open)
        };
        if let Err(error) = kept.wait().await {
            tracing::warn!("the runtime daemon's held sessions were not saved: {error:#}");
        }
    }

    /// Saves `open` as the held sessions; called under the lock, so the
    /// writes land in the order the changes were made.
    fn keep(&self, held: &HeldSessions) -> pa_log::Receipt<bool> {
        let open = held
            .open
            .iter()
            .filter(|(_, session)| session.owner.is_none())
            .map(|(id, session)| {
                (
                    id.clone(),
                    json!({
                        "path": session.path,
                        "cwd": session.cwd,
                        "daemonPid": std::process::id(),
                    }),
                )
            });
        let dormant = held.dormant.iter().map(|(id, (path, cwd))| {
            (
                id.clone(),
                json!({
                    "path": path,
                    "cwd": cwd,
                    "daemonPid": std::process::id(),
                    "dormant": true,
                }),
            )
        });
        let held: serde_json::Map<String, Value> = open.chain(dormant).collect();
        self.log
            .put_value(SCOPE, KEY, Value::Object(held).to_string(), Origin::Written)
    }
}

/// Revives the sessions a daemon that died held (see the module
/// documentation); each one revived is held in `open`. The ids of those
/// opened again for unfinished tasks.
pub(super) async fn revive<B: SessionBuilder>(
    runtime: &Runtime<B>,
    open: &OpenSessions,
    schedules: &Schedules<B>,
    agent_dir: &Path,
) -> std::collections::HashSet<String> {
    let log = std::sync::Arc::clone(runtime.log());
    let held: BTreeMap<String, Held> = match log.value(SCOPE, KEY) {
        Ok(Some(held)) => serde_json::from_str::<serde_json::Map<String, Value>>(&held)
            .unwrap_or_default()
            .iter()
            .filter_map(|(id, held)| Some((id.clone(), Held::of(held)?)))
            .collect(),
        Ok(None) => BTreeMap::new(),
        Err(error) => {
            tracing::warn!("the runtime daemon's held sessions could not be read: {error:#}");
            BTreeMap::new()
        }
    };
    let mut revived = std::collections::HashSet::new();
    for (id, session) in held {
        match revive_one(runtime, open, schedules, agent_dir, &log, &id, session).await {
            Ok(true) => {
                revived.insert(id);
            }
            Ok(false) => {}
            Err(error) => tracing::warn!("session {id} was not revived: {error:#}"),
        }
    }
    // What is held now is what was revived.
    let kept = open.keep(&lock(&open.open));
    if let Err(error) = kept.wait().await {
        tracing::warn!("the runtime daemon's held sessions were not saved: {error:#}");
    }
    revived
}

/// Revives session `id`, which `session` says a dead daemon held: whether it
/// was opened again for its unfinished tasks.
async fn revive_one<B: SessionBuilder>(
    runtime: &Runtime<B>,
    open: &OpenSessions,
    schedules: &Schedules<B>,
    agent_dir: &Path,
    log: &std::sync::Arc<Log>,
    id: &str,
    session: Held,
) -> anyhow::Result<bool> {
    let journal = orphan_journal(agent_dir, id);
    let owner = session.daemon_pid;
    let reaped_programs =
        tokio::task::spawn_blocking(move || crate::session::reap_orphans(&journal, owner))
            .await
            .unwrap_or(0);
    let tasks = {
        let (log, id) = (std::sync::Arc::clone(log), id.to_string());
        tokio::task::spawn_blocking(move || log.unfinished_tasks(&id)).await??
    };
    if tasks.is_empty() {
        // Idle: down until a scheduled job of its is due, or let go.
        if schedules.arm_dormant(id, &session.path, &session.cwd) {
            open.insert_dormant(id.to_string(), session.path, session.cwd)
                .await;
        }
        return Ok(false);
    }
    let lease = {
        let (path, agent_dir) = (session.path.clone(), agent_dir.to_path_buf());
        tokio::task::spawn_blocking(move || acquire_runtime_session_lease(&path, &agent_dir))
            .await??
    };
    let running = tasks
        .into_iter()
        .find(|task| task.state == TaskState::Running);
    let spec = SessionSpec {
        id: Some(id.to_string()),
        name: None,
        cwd: session.cwd.clone(),
        env: super::session_env(agent_dir, id),
        subagent: None,
        transcript: Some(session.path.clone()),
        options: super::session_options(log, id),
        ephemeral: false,
    };
    match running {
        // Only queued work: the daemon died between turns.
        None => {
            runtime.create_session(spec).await?;
        }
        Some(task) => {
            let saved_at = log.task_saved_at(&task.id)?.unwrap_or(0);
            let evidence = transcript_evidence(
                &session.path,
                &pa_core::session::manager::format_iso(i64::try_from(saved_at)?),
                TurnPrompt::User,
            )
            .ok();
            // A task counts its runs: the second death in a row under it is
            // its second run's.
            let (message, turn) = host_death(evidence, u64::from(task.attempts.saturating_sub(1)));
            runtime
                .revive_session(
                    spec,
                    Revival {
                        rebuild: Rebuild {
                            crash: SessionCrash::now("daemon_restart", message.to_string()),
                            failures: 1,
                            reaped_programs,
                        },
                        running: Some((task, evidence, turn)),
                    },
                )
                .await?;
        }
    }
    open.insert(
        id.to_string(),
        OpenSession {
            path: session.path,
            cwd: session.cwd,
            lease: Some(lease),
            owner: None,
            scoped_models: Vec::new(),
        },
    )
    .await;
    Ok(true)
}
