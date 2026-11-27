//! Whether stored skills and memories earn their place by being reused.
//!
//! A skill sits in every session's skills inventory and a global harness
//! entry in every session's `[harness-digest]`, so an entry used only in
//! the session that made it costs at least what it saved. This module keeps
//! one small ledger, `<agent dir>/reuse/ledger.json`, of when each entry was
//! created, offered to a session and used, by which session and in which
//! project: counts, timestamps, session ids and a hash of the project
//! directory, never an entry's content.
//!
//! "Reused" means used in a session other than the one that created the
//! entry: the session an entry was built in overlaps its own task, so its
//! uses there say nothing about whether the entry helps elsewhere. When the
//! creating session was not observed (an entry older than the ledger, or
//! written by hand), the first session that saw the entry stands in for it,
//! which can only undercount reuse.
//!
//! What is recorded, and where:
//! - a skill is offered when a session's system prompt lists it (session
//!   start and `/reload`), and used when its `SKILL.md` is read through the
//!   kernel's `fs-read`, when a `/skill:<name>` command expands it, or when
//!   a cell names a function its Scheme source defines; it is created when
//!   a cell writes its `SKILL.md` and the ledger has no record of it.
//! - a global harness entry is created by `harness-put`, offered when a
//!   digest shows it, used when a `harness-query` search returns it, and
//!   forgotten when `harness-delete` removes it. Local entries belong to one session
//!   and are not tracked.
//!
//! Skills shipped with the product are not tracked: they are its surface,
//! not something a session or the user stored.
//!
//! The ledger only informs: nothing here deletes or demotes an entry. The
//! user prunes what they pick with `prime-agent reuse prune`.
//!
//! Writes take a process-wide lock and the file's cross-process lock
//! directory around a read-modify-write and replace the file atomically, so
//! concurrent sessions of one daemon and a CLI prune lose nothing. The
//! ledger is bounded: [`MAX_ENTRIES`] entries and [`MAX_RECENT_SESSIONS`]
//! sessions per entry; a session evicted from an entry's recent list and
//! seen again counts as a new one.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::refinement::RefinementKind;

mod report;
#[cfg(test)]
mod tests;

pub use report::{prune, report, skill_standings, Pruned, ReuseRow};

/// The ledger's directory under the agent dir.
pub const REUSE_DIR: &str = "reuse";
/// The ledger's file name in [`REUSE_DIR`].
pub const LEDGER_FILE: &str = "ledger.json";
/// The most entries the ledger keeps; the least recently active go first.
pub const MAX_ENTRIES: usize = 512;
/// The most sessions one entry remembers by id.
pub const MAX_RECENT_SESSIONS: usize = 16;
/// An entry offered to this many sessions after its creator's and used by
/// none of them is [`Standing::Unused`].
pub const UNUSED_AFTER_SESSIONS: u64 = 3;
/// A ledger file larger than this is not read (it is rebuilt from empty).
const MAX_LEDGER_BYTES: u64 = 4 * 1024 * 1024;

/// The skills a session's model sees, read when needed so a reload is seen.
pub type VisibleSkills = std::sync::Arc<dyn Fn() -> Vec<crate::skills::Skill> + Send + Sync>;

/// One tracked entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKey {
    /// An installed skill, by name.
    Skill(String),
    /// A global harness entry, by kind and id.
    Harness(RefinementKind, String),
}

impl EntryKey {
    /// The key as the ledger, the kernel and the CLI write it:
    /// `skill/<name>` or `harness/<kind>/<id>`.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            EntryKey::Skill(name) => format!("skill/{name}"),
            EntryKey::Harness(kind, id) => {
                format!("harness/{}/{id}", crate::refinement::kind_name(*kind))
            }
        }
    }

    /// The key [`EntryKey::text`] wrote, or `None` for text that is not one.
    #[must_use]
    pub fn parse(text: &str) -> Option<EntryKey> {
        match text.split_once('/')? {
            ("skill", name) if !name.is_empty() => Some(EntryKey::Skill(name.to_string())),
            ("harness", rest) => {
                let (kind, id) = rest.split_once('/')?;
                let kind = match kind {
                    "prompt" => RefinementKind::Prompt,
                    "memory" => RefinementKind::Memory,
                    "skill" => RefinementKind::Skill,
                    "subagent" => RefinementKind::Subagent,
                    "factory" => RefinementKind::Factory,
                    _ => return None,
                };
                (!id.is_empty()).then(|| EntryKey::Harness(kind, id.to_string()))
            }
            _ => None,
        }
    }
}

/// How an entry's creation is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creation {
    /// A new entry: any record of an earlier one under its key is replaced.
    New,
    /// Possibly a new entry: recorded only when the ledger knows none.
    IfUnknown,
}

/// What happened to an entry in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Created(Creation),
    /// The session's prompt carried it.
    Offered,
    /// The session read or called it.
    Used,
}

/// How the creating session is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Observed {
    /// The session that created the entry was seen creating it.
    Created,
    /// The first session that saw the entry stands in for its creator.
    FirstSeen,
}

/// Where and when an entry was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    pub session: String,
    pub project: String,
    pub at: String,
    pub observed: Observed,
}

/// One session's dealings with an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMark {
    pub session: String,
    pub project: String,
    pub offered: bool,
    pub uses: u64,
    pub last_at: String,
}

/// What the ledger knows of one entry. The `*_elsewhere` counts leave out
/// the creating session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRecord {
    pub created: Origin,
    /// Distinct sessions that were offered it, its creator's included.
    pub offered_sessions: u64,
    pub offered_elsewhere: u64,
    /// Every use, its creator's included.
    pub uses: u64,
    pub uses_elsewhere: u64,
    /// Distinct sessions other than its creator that used it.
    pub used_elsewhere: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_offered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// The sessions seen most recently, at most [`MAX_RECENT_SESSIONS`].
    pub recent: Vec<SessionMark>,
}

/// Whether an entry has earned its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Standing {
    /// Too few sessions after its creator's have seen it to tell.
    New,
    /// Offered to [`UNUSED_AFTER_SESSIONS`] or more later sessions, used by
    /// none: it costs every session and saves none.
    Unused,
    /// Used by exactly one session other than its creator.
    UsedOnce,
    /// Used by two or more sessions other than its creator.
    Reused,
}

impl Standing {
    /// The label the prompt, the kernel and the CLI share.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Standing::New => "new",
            Standing::Unused => "unused",
            Standing::UsedOnce => "used-once",
            Standing::Reused => "reused",
        }
    }
}

impl EntryRecord {
    #[must_use]
    pub fn standing(&self) -> Standing {
        match self.used_elsewhere {
            0 if self.offered_elsewhere >= UNUSED_AFTER_SESSIONS => Standing::Unused,
            0 => Standing::New,
            1 => Standing::UsedOnce,
            _ => Standing::Reused,
        }
    }

    /// The latest time anything happened to it.
    fn last_active(&self) -> &str {
        [
            self.last_used_at.as_deref(),
            self.last_offered_at.as_deref(),
        ]
        .into_iter()
        .flatten()
        .fold(self.created.at.as_str(), std::cmp::max)
    }
}

/// The whole ledger.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub schema: u64,
    /// Records by [`EntryKey::text`].
    pub entries: BTreeMap<String, EntryRecord>,
}

impl Ledger {
    /// The record of `key`, when the ledger has one.
    #[must_use]
    pub fn record(&self, key: &EntryKey) -> Option<&EntryRecord> {
        self.entries.get(&key.text())
    }

    /// Apply one event of `session` (in `project`, at `at`) to `key`.
    fn apply(&mut self, event: Event, key: &EntryKey, session: &str, project: &str, at: &str) {
        let origin = |observed| Origin {
            session: session.to_string(),
            project: project.to_string(),
            at: at.to_string(),
            observed,
        };
        let fresh = |observed| EntryRecord {
            created: origin(observed),
            offered_sessions: 0,
            offered_elsewhere: 0,
            uses: 0,
            uses_elsewhere: 0,
            used_elsewhere: 0,
            last_offered_at: None,
            last_used_at: None,
            recent: Vec::new(),
        };
        let text = key.text();
        match event {
            Event::Created(Creation::New) => {
                self.entries.insert(text, fresh(Observed::Created));
                return;
            }
            Event::Created(Creation::IfUnknown) => {
                self.entries
                    .entry(text)
                    .or_insert_with(|| fresh(Observed::Created));
                return;
            }
            Event::Offered | Event::Used => {}
        }
        let record = self
            .entries
            .entry(text)
            .or_insert_with(|| fresh(Observed::FirstSeen));
        let elsewhere = record.created.session != session;
        let known = record
            .recent
            .iter()
            .position(|mark| mark.session == session);
        let position = known.unwrap_or_else(|| {
            if record.recent.len() >= MAX_RECENT_SESSIONS {
                if let Some(oldest) = record
                    .recent
                    .iter()
                    .enumerate()
                    .min_by(|(_, left), (_, right)| left.last_at.cmp(&right.last_at))
                    .map(|(position, _)| position)
                {
                    record.recent.remove(oldest);
                }
            }
            record.recent.push(SessionMark {
                session: session.to_string(),
                project: project.to_string(),
                offered: false,
                uses: 0,
                last_at: at.to_string(),
            });
            record.recent.len() - 1
        });
        let mark = &mut record.recent[position];
        mark.last_at = at.to_string();
        match event {
            Event::Offered => {
                if !mark.offered {
                    mark.offered = true;
                    record.offered_sessions += 1;
                    if elsewhere {
                        record.offered_elsewhere += 1;
                    }
                }
                record.last_offered_at = Some(at.to_string());
            }
            Event::Used => {
                mark.uses += 1;
                record.uses += 1;
                if elsewhere {
                    record.uses_elsewhere += 1;
                    if mark.uses == 1 {
                        record.used_elsewhere += 1;
                    }
                }
                record.last_used_at = Some(at.to_string());
            }
            Event::Created(_) => {}
        }
    }

    /// Drop the least recently active records past [`MAX_ENTRIES`].
    fn bound(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by(|(_, left), (_, right)| left.last_active().cmp(right.last_active()))
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            self.entries.remove(&oldest);
        }
    }
}

/// Whether the ledger tracks `skill`: every skill but those shipped with
/// the product, which are its surface rather than something a session or
/// the user stored.
#[must_use]
pub fn tracked(skill: &crate::skills::Skill) -> bool {
    skill.source_info.source != SHIPPED_SKILL_SOURCE
}

/// The `source` of a skill shipped with the product.
const SHIPPED_SKILL_SOURCE: &str = "builtin";

/// The ledger's path under `agent_dir`.
#[must_use]
pub fn ledger_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(REUSE_DIR).join(LEDGER_FILE)
}

/// The ledger under `agent_dir`; empty when there is none or it cannot be
/// read (it only informs, so a damaged one starts over).
#[must_use]
pub fn load(agent_dir: &Path) -> Ledger {
    let path = ledger_path(agent_dir);
    let log = crate::agent_state::process_log();
    let held = log.is_some_and(|log| {
        log.partition(&crate::agent_state::StateFile::ReuseLedger.scope(&path))
            .is_ok_and(|partition| partition.is_some())
    });
    // A file is checked for its size before it is read.
    let readable =
        held || std::fs::metadata(&path).is_ok_and(|meta| meta.len() <= MAX_LEDGER_BYTES);
    readable
        .then(|| {
            crate::agent_state::read_document(
                log,
                crate::agent_state::StateFile::ReuseLedger,
                &path,
            )
            .ok()
            .flatten()
        })
        .flatten()
        .filter(|raw| raw.len() as u64 <= MAX_LEDGER_BYTES)
        .and_then(|raw| serde_json::from_str::<Ledger>(&raw).ok())
        .unwrap_or_default()
}

/// Locked read-modify-write of the ledger under `agent_dir`.
fn update<R>(agent_dir: &Path, change: impl FnOnce(&mut Ledger) -> R) -> anyhow::Result<R> {
    // One writer at a time in this process; the lock directory keeps out
    // the others (a CLI prune).
    static WRITING: Mutex<()> = Mutex::new(());
    let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    let path = ledger_path(agent_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = crate::platform::lock_dir::LockDir::acquire_owned_retrying(
        &path,
        std::time::Duration::from_secs(10),
        50,
        std::time::Duration::from_millis(20),
    )?;
    let mut ledger = load(agent_dir);
    let result = change(&mut ledger);
    ledger.schema = 1;
    ledger.bound();
    lock.ensure_owned()?;
    crate::agent_state::write_document(
        crate::agent_state::StateFile::ReuseLedger,
        &path,
        serde_json::to_string(&ledger)?,
        // Advisory counts: losing the last write to a power cut is cheaper
        // than a disk sync on every use.
        crate::settings::storage::AtomicWriteOptions { fsync: false },
    )?;
    Ok(result)
}

/// Remove the records of `keys`.
///
/// # Errors
///
/// The ledger could not be locked or written.
pub fn forget(agent_dir: &Path, keys: &[EntryKey]) -> anyhow::Result<()> {
    update(agent_dir, |ledger| {
        for key in keys {
            ledger.entries.remove(&key.text());
        }
    })
}

/// The ledger's name for a project directory: a hash, never the path.
#[must_use]
pub fn project_id(project: &Path) -> String {
    Sha256::digest(project.to_string_lossy().as_bytes())[..8]
        .iter()
        .fold(String::with_capacity(16), |mut name, byte| {
            let _ = write!(name, "{byte:02x}");
            name
        })
}

/// Records one session's events in the ledger of its agent dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorder {
    agent_dir: PathBuf,
    session: String,
    project: String,
}

impl Recorder {
    /// A recorder for `session`, working in `project`.
    #[must_use]
    pub fn new(agent_dir: &Path, session: &str, project: &Path) -> Self {
        Recorder {
            agent_dir: agent_dir.to_path_buf(),
            session: session.to_string(),
            project: project_id(project),
        }
    }

    /// The agent dir whose ledger this writes.
    #[must_use]
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// Record `event` for each of `keys`, now.
    ///
    /// # Errors
    ///
    /// The session's lifetime has ended, or the ledger could not be locked
    /// or written.
    pub fn record(&self, event: Event, keys: &[EntryKey]) -> anyhow::Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        pa_types::session_scope::ensure_live()?;
        let at = crate::session::manager::format_iso_now();
        update(&self.agent_dir, |ledger| {
            for key in keys {
                ledger.apply(event, key, &self.session, &self.project, &at);
            }
        })
    }

    /// [`Recorder::record`] on a thread of its own, for callers that must
    /// not wait on the disk (a session's start, a turn's admission). A
    /// failure is logged.
    pub fn record_detached(&self, event: Event, keys: Vec<EntryKey>) {
        if keys.is_empty() {
            return;
        }
        let recorder = self.clone();
        let spawned = std::thread::Builder::new()
            .name("reuse-ledger".to_string())
            .spawn(pa_types::session_scope::propagate(move || {
                if let Err(error) = recorder.record(event, &keys) {
                    tracing::warn!("the reuse ledger was not updated: {error:#}");
                }
            }));
        if let Err(error) = spawned {
            tracing::warn!("the reuse ledger was not updated: {error}");
        }
    }

    /// Record a use of the skill whose file `path` is, if it is one of
    /// `skills`' files.
    ///
    /// # Errors
    ///
    /// As [`Recorder::record`].
    pub fn note_read(&self, path: &Path, skills: &[crate::skills::Skill]) -> anyhow::Result<()> {
        let read = crate::skills::loader::canonicalize_path(path);
        let keys: Vec<EntryKey> = skills
            .iter()
            .filter(|skill| tracked(skill))
            .filter(|skill| {
                skill.file_path == path
                    || crate::skills::loader::canonicalize_path(&skill.file_path) == read
            })
            .map(|skill| EntryKey::Skill(skill.name.clone()))
            .collect();
        self.record(Event::Used, &keys)
    }

    /// Record the creation of the skill whose `SKILL.md` `path` is, when the
    /// ledger knows none: a file named `SKILL.md` in a directory under one
    /// named `skills`, the skill named after its directory.
    ///
    /// # Errors
    ///
    /// As [`Recorder::record`].
    pub fn note_write(&self, path: &Path) -> anyhow::Result<()> {
        let skill_dir = path
            .parent()
            .filter(|_| path.file_name().is_some_and(|name| name == "SKILL.md"));
        let name = skill_dir
            .filter(|dir| {
                dir.parent()
                    .and_then(Path::file_name)
                    .is_some_and(|parent| parent == "skills")
            })
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned());
        match name {
            Some(name) => self.record(
                Event::Created(Creation::IfUnknown),
                &[EntryKey::Skill(name)],
            ),
            None => Ok(()),
        }
    }
}
