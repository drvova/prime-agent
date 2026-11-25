//! Append-only recovery journals: the worker journal records the latest
//! busy/operation state and queue snapshots.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub(crate) const RECOVERY_JOURNAL_SUFFIX: &str = ".recovery.jsonl";

/// The worker's orphan-process journal file suffix (TS
/// `${workerId}.orphans.jsonl`, beside the recovery journal).
pub(crate) const ORPHAN_JOURNAL_SUFFIX: &str = ".orphans.jsonl";

/// The worker's orphan-process journal path under its descriptor dir.
#[must_use]
pub(crate) fn orphan_journal_path(descriptor_dir: &Path, worker_id: &str) -> std::path::PathBuf {
    descriptor_dir.join(format!("{worker_id}{ORPHAN_JOURNAL_SUFFIX}"))
}

pub(crate) fn append_record(path: &Path, record: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open journal {}", path.display()))?;
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    pa_core::platform::fsync(&file)?;
    Ok(())
}

/// Append several records as ONE durable write: the batch is
/// all-or-nothing; the on-disk bytes match the records appended one by one.
///
/// # Errors
///
/// Returns an error when the open, serialization, write, or sync fails;
/// the loader skips a partial write's truncated trailing lines.
pub(crate) fn append_records(path: &Path, records: &[Value]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open journal {}", path.display()))?;
    let mut lines = Vec::new();
    for record in records {
        serde_json::to_writer(&mut lines, record)?;
        lines.push(b'\n');
    }
    file.write_all(&lines)?;
    pa_core::platform::fsync(&file)?;
    Ok(())
}

/// Whether the temp journal's data rides a full sync before the swap.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Finalize {
    /// Bare rename, temp synced before the swap: every failure surfaces
    /// immediately.
    Synced,
    /// Bare rename with an UNSYNCED temp: durability is owned by the
    /// append path — a lost compact falls back to the append-only history.
    Bare,
}

pub(crate) fn rewrite_records(path: &Path, records: &[Value], finalize: Finalize) -> Result<()> {
    use pa_types::platform::{Durability, FileMode, ReplaceOptions, Symlinks};
    let options = ReplaceOptions {
        durability: match finalize {
            Finalize::Synced => Durability::SyncFile,
            Finalize::Bare => Durability::Unsynced,
        },
        mode: FileMode::KeepTarget,
        symlinks: Symlinks::ReplaceLink,
    };
    let mut file = pa_types::platform::AtomicFile::create(path, options)
        .with_context(|| format!("create a temp file beside {}", path.display()))?;
    {
        let mut writer = BufWriter::new(&mut file);
        for record in records {
            let mut line = serde_json::to_string(record)?;
            line.push('\n');
            writer.write_all(line.as_bytes())?;
        }
        writer.flush()?;
    }
    file.commit()
        .with_context(|| format!("persist {}", path.display()))?;
    Ok(())
}

// Complete records must decode exactly before recovery can replace their history.
// Only an unterminated final JSON fragment may end with incomplete UTF-8.
pub(crate) fn decode_journal_lines<'a>(bytes: &'a [u8], path: &Path) -> Result<Vec<&'a str>> {
    let mut lines = Vec::new();
    let mut records = bytes.split(|byte| *byte == b'\n').peekable();
    while let Some(raw) = records.next() {
        match std::str::from_utf8(raw) {
            Ok(line) => lines.push(line),
            Err(error) => {
                let incomplete_final = records.peek().is_none()
                    && !bytes.ends_with(b"\n")
                    && serde_json::from_slice::<Value>(raw).is_err_and(|error| error.is_eof());
                if !incomplete_final {
                    return Err(error)
                        .with_context(|| format!("invalid UTF-8 in journal {}", path.display()));
                }
            }
        }
    }
    Ok(lines)
}

/// Exclusive write ownership of one journal: an advisory lock on its
/// `.writer-lock` sidecar, held by this process.
#[derive(Debug)]
pub(crate) struct JournalWriteOwner {
    file: std::sync::Arc<File>,
    ownership_path: std::path::PathBuf,
    /// The owner's id: see [`crate::session_host::session_pid`].
    owner: u32,
    /// The journal's one writer at a time, and which ownership that is: a
    /// later lifetime's takeover changes it, and from then on this one's
    /// writes are refused ([`JournalWriteOwner::write`]).
    gate: std::sync::Arc<std::sync::Mutex<u64>>,
    /// This ownership, as the gate names it.
    generation: u64,
}

impl JournalWriteOwner {
    /// Run one durable write of the journal, with no other of its writers
    /// inside one. A hosted session that was ended while a thread of it was
    /// stuck leaves that thread holding its journal; if the thread wakes
    /// after the rebuilt session took the journal over, its writes stop
    /// here instead of landing among the new lifetime's records.
    ///
    /// # Errors
    ///
    /// A later lifetime of the session owns the journal now, or `write`
    /// failed.
    fn write<T>(&self, write: impl FnOnce() -> Result<T>) -> Result<T> {
        pa_types::session_scope::ensure_live()?;
        let current = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            *current == self.generation,
            "the journal {} belongs to a later lifetime of its session: an ended lifetime does not write to it",
            self.ownership_path.display()
        );
        write()
    }
}

/// The journal locks this process holds: sidecar path -> owner and locked
/// file. The operating system's lock excludes other processes; this table
/// is what lets a rebuilt hosted session take over the lock of a lifetime
/// that ended without letting go (a stuck thread still owns its handle).
fn held_ownerships() -> &'static std::sync::Mutex<HeldOwnerships> {
    static HELD: std::sync::OnceLock<std::sync::Mutex<HeldOwnerships>> = std::sync::OnceLock::new();
    HELD.get_or_init(Default::default)
}

/// Sidecar path -> the owner's id, the locked file, and the journal's
/// write gate.
type HeldOwnerships = std::collections::HashMap<
    std::path::PathBuf,
    (
        u32,
        std::sync::Arc<File>,
        std::sync::Arc<std::sync::Mutex<u64>>,
    ),
>;

impl Drop for JournalWriteOwner {
    fn drop(&mut self) {
        let mut held = held_ownerships()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A successor that took the lock over keeps it locked.
        if !held
            .get(&self.ownership_path)
            .is_some_and(|(owner, file, gate)| {
                *owner == self.owner
                    && std::sync::Arc::ptr_eq(file, &self.file)
                    && *gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        == self.generation
            })
        {
            return;
        }
        held.remove(&self.ownership_path);
        // Closing one descriptor can leave the lock held by an inherited copy.
        if let Err(error) = pa_core::platform::unlock_file(&self.file) {
            // Drop has no error channel; report without panicking if stderr is unavailable.
            let _ = writeln!(
                std::io::stderr(),
                "Prime Agent: failed to release journal ownership: {error}"
            );
        }
    }
}

// Keep the sidecar inode stable: unlinking can admit a second ownership inode.
// Empty sidecars remain after retirement; directory entries/inodes can accumulate.
// Explicit unlock is attempted on guard drop; a failure leaves close as fallback.
pub(crate) fn journal_write_owner(path: &Path) -> Result<JournalWriteOwner> {
    static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut ownership_path = path.as_os_str().to_os_string();
    ownership_path.push(".writer-lock");
    let ownership_path = std::path::PathBuf::from(ownership_path);
    let owner = crate::session_host::session_pid();
    let mut held = held_ownerships()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The holder is a hosted lifetime of this process that has ended: its
    // lock passes to the caller. Any other holder keeps it.
    let generation = GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some((file, gate)) = held
        .get(&ownership_path)
        .filter(|(holder, _, _)| {
            crate::session_host::is_hosted_pid(*holder)
                && !crate::session_host::hosted_alive(*holder)
        })
        .map(|(_, file, gate)| (std::sync::Arc::clone(file), std::sync::Arc::clone(gate)))
    {
        // From here the ended lifetime's writes are refused. One of them
        // that is inside its write right now cannot be taken back, and
        // would land after this lifetime's first records: the journal is
        // not taken over under it.
        match gate.try_lock() {
            Ok(mut current) => *current = generation,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                *poisoned.into_inner() = generation;
            }
            Err(std::sync::TryLockError::WouldBlock) => anyhow::bail!(
                "an ended lifetime of this session is still inside a write to journal {}",
                path.display()
            ),
        }
        held.insert(
            ownership_path.clone(),
            (
                owner,
                std::sync::Arc::clone(&file),
                std::sync::Arc::clone(&gate),
            ),
        );
        return Ok(JournalWriteOwner {
            file,
            ownership_path,
            owner,
            gate,
            generation,
        });
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&ownership_path)
        .with_context(|| format!("open journal ownership {}", path.display()))?;
    pa_core::platform::try_lock_exclusive(&file)
        .with_context(|| format!("acquire journal ownership {}", path.display()))?;
    let file = std::sync::Arc::new(file);
    let gate = std::sync::Arc::new(std::sync::Mutex::new(generation));
    held.insert(
        ownership_path.clone(),
        (
            owner,
            std::sync::Arc::clone(&file),
            std::sync::Arc::clone(&gate),
        ),
    );
    Ok(JournalWriteOwner {
        file,
        ownership_path,
        owner,
        gate,
        generation,
    })
}

pub(crate) fn tail_is_torn(path: &Path) -> Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("open journal tail {}", path.display()));
        }
    };
    if file
        .metadata()
        .with_context(|| format!("stat journal tail {}", path.display()))?
        .len()
        == 0
    {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))
        .with_context(|| format!("seek journal tail {}", path.display()))?;
    let mut tail = [0u8];
    file.read_exact(&mut tail)
        .with_context(|| format!("read journal tail {}", path.display()))?;
    Ok(tail[0] != b'\n')
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRecoveryRecord {
    pub active_session_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub busy: bool,
    pub operation: String,
    pub recorded_at: String,
}

fn parse_worker_records(path: &Path) -> Result<HashMap<String, WorkerRecoveryRecord>> {
    let mut latest = HashMap::new();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(latest),
        Err(error) => {
            return Err(error).with_context(|| format!("read journal {}", path.display()));
        }
    };
    for line in decode_journal_lines(&bytes, path)? {
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<WorkerRecoveryRecord>(line) else {
            continue;
        };
        latest.insert(record.active_session_id.clone(), record);
    }
    Ok(latest)
}

/// One parked queue row in a worker queue snapshot: a restored queued
/// heartbeat still delivers as the `heartbeat_prompt` component instead of
/// a plain user message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerQueueItemRecord {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<crate::worker::QueuePriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_message: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_key: Option<String>,
    #[serde(default = "queue_visible_default")]
    pub queue_visible: bool,
    /// The item's turn-execution class ("queued"/"injected"/"direct"):
    /// the batch gathering's compatibility gate. A pre-field record
    /// restores as "queued" — the only class a fresh snapshot can batch.
    #[serde(default = "queue_policy_default")]
    pub policy: String,
    /// The forced-steering batch flag: a restored forced item keeps its
    /// one-turn batch semantics.
    #[serde(default)]
    pub forced_batch: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<pa_agent::types::ImageContent>,
}

fn queue_visible_default() -> bool {
    true
}

fn queue_policy_default() -> String {
    "queued".to_string()
}

impl WorkerQueueItemRecord {
    /// The record's turn-execution class; an unknown value restores as
    /// the dominant "queued" class.
    pub(crate) fn policy(&self) -> crate::worker::TurnPolicy {
        match self.policy.as_str() {
            "injected" => crate::worker::TurnPolicy::Injected,
            "direct" => crate::worker::TurnPolicy::Direct,
            _ => crate::worker::TurnPolicy::Queued,
        }
    }
}

/// A worker queue snapshot record: the pending steering/follow-up lanes so
/// a respawned worker restores its queues. Version 2 lanes carry the full
/// item records; a version-1 lane is a bare message-text array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerQueueSnapshotRecord {
    pub version: u32,
    pub r#type: String,
    pub active_session_id: String,
    pub steering: Vec<WorkerQueueItemRecord>,
    pub follow_up: Vec<WorkerQueueItemRecord>,
    /// The queued-input suspension at snapshot time: a restored parked
    /// queue stays parked until an explicit resume site clears it.
    #[serde(default)]
    pub suspended: bool,
    pub recorded_at: String,
    /// The rows of the turn that had just been picked up when this snapshot
    /// was taken (they have left their lane). A session revived after its
    /// host died replays them ahead of the lane they came from; a session
    /// rebuilt after its own crash does not, because the supervisor drops
    /// them first ([`WorkerRecoveryJournal::drop_running_turns`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub running: Vec<WorkerQueueItemRecord>,
    /// Whether `running` came off the steering lane (else the follow-up lane).
    #[serde(default)]
    pub running_steering: bool,
}

impl WorkerQueueSnapshotRecord {
    /// The lanes a restore redelivers: the picked-up turn's rows first.
    fn restored_lanes(&self) -> QueueSnapshotLanes {
        let (mut steering, mut follow_up) = (self.steering.clone(), self.follow_up.clone());
        let lane = if self.running_steering {
            &mut steering
        } else {
            &mut follow_up
        };
        lane.splice(0..0, self.running.iter().cloned());
        (steering, follow_up, self.suspended)
    }
}

/// Whether a queue checkpoint is taken while a picked-up turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickedUpTurn {
    /// A turn is in flight: the checkpoint keeps the rows its pickup
    /// recorded, and the pickup's time (a revival reads the transcript from
    /// it), so a revive after the daemon dies still redelivers what is owed
    /// of that turn. Rows queued meanwhile do not replace them.
    Running,
    /// No turn is in flight: the checkpoint carries no picked-up rows.
    Ended,
}

/// One session's latest queue snapshot as read back for a restore: the
/// steering and follow-up lanes, and whether queued input was suspended
/// when they were recorded (a parked queue revives still parked).
pub type QueueSnapshotLanes = (Vec<WorkerQueueItemRecord>, Vec<WorkerQueueItemRecord>, bool);

/// Latest busy/operation per active session, plus the latest queue
/// snapshot per session.
pub struct WorkerRecoveryJournal {
    write_owner: JournalWriteOwner,
    path: std::path::PathBuf,
    latest: HashMap<String, WorkerRecoveryRecord>,
    queue_snapshots: HashMap<String, WorkerQueueSnapshotRecord>,
}

impl WorkerRecoveryJournal {
    /// Open the worker journal at `path`, creating the parent directory as needed.
    ///
    /// # Errors
    ///
    /// Returns an error when write ownership cannot be acquired, the parent directory cannot
    /// be created, either
    /// record pass cannot read an existing journal, or the
    /// torn-tail heal cannot rewrite it.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let write_owner = journal_write_owner(path)?;
        let queue_snapshots = parse_queue_snapshot_records(path)?;
        let journal = WorkerRecoveryJournal {
            write_owner,
            path: path.to_path_buf(),
            latest: parse_worker_records(path)?,
            queue_snapshots,
        };
        if tail_is_torn(path)? {
            // Recovery can still contain busy verdicts and queued prompts.
            // Sync the replacement before it takes over their durable history.
            journal.compact(Finalize::Synced)?;
        }
        Ok(journal)
    }

    /// Read the latest worker record per active session straight from a
    /// journal file.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing journal cannot be read; a missing
    /// journal reads as an empty set.
    pub fn read_latest(path: &Path) -> Result<Vec<WorkerRecoveryRecord>> {
        Ok(parse_worker_records(path)?.into_values().collect())
    }

    /// Does the journal prove live work at the worker's last exit? A
    /// restart must not mass-revive historical sessions: a latest `busy`
    /// record marks in-flight work; an unreadable journal proves nothing.
    #[must_use]
    pub fn read_interrupted(path: &Path) -> bool {
        Self::read_latest(path).is_ok_and(|records| records.iter().any(|record| record.busy))
    }

    /// The newest `busy` record's `recorded_at`, when the journal proves
    /// live work: the timestamp the boot-revival gate ages the evidence against.
    #[must_use]
    pub fn latest_busy_recorded_at(path: &Path) -> Option<String> {
        Self::read_latest(path)
            .ok()?
            .iter()
            .filter(|record| record.busy)
            .map(|record| record.recorded_at.clone())
            .max()
    }

    /// Settle every busy session to idle with `operation` (the give-up
    /// belt): stale busy evidence must not outlive the give-up that
    /// superseded it, or every boot re-storms the slot.
    ///
    /// # Errors
    ///
    /// Returns an error when the journal cannot be opened or a settle record cannot be appended.
    pub fn settle_busy_records(path: &Path, operation: &str) -> Result<()> {
        let mut journal = Self::open(path)?;
        let busy: Vec<WorkerRecoveryRecord> = journal
            .get_latest()
            .into_iter()
            .filter(|record| record.busy)
            .collect();
        for record in busy {
            journal.record(
                &record.active_session_id,
                &record.session_id,
                record.session_file.as_deref(),
                false,
                operation,
            )?;
        }
        Ok(())
    }

    /// Record the latest busy/operation state for an active session; an
    /// unchanged record is skipped.
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be serialized or appended,
    /// or the all-idle compaction fails.
    pub fn record(
        &mut self,
        active_session_id: &str,
        session_id: &str,
        session_file: Option<&str>,
        busy: bool,
        operation: &str,
    ) -> Result<()> {
        if let Some(previous) = self.latest.get(active_session_id) {
            if previous.busy == busy
                && previous.operation == operation
                && previous.session_file.as_deref() == session_file
            {
                return Ok(());
            }
        }
        let record = WorkerRecoveryRecord {
            active_session_id: active_session_id.to_string(),
            session_id: session_id.to_string(),
            session_file: session_file.map(str::to_string),
            busy,
            operation: operation.to_string(),
            recorded_at: crate::util::now_iso(),
        };
        let line = serde_json::to_value(&record)?;
        self.write_owner
            .write(|| append_record(&self.path, &line))?;
        self.latest.insert(active_session_id.to_string(), record);
        // TS parity: the all-idle check runs AFTER the insert (TS checks
        // after `set`); checking before it, a single-session journal never
        // compacted.
        if self.latest.values().all(|entry| !entry.busy) {
            self.compact(Finalize::Bare)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn get_latest(&self) -> Vec<WorkerRecoveryRecord> {
        self.latest.values().cloned().collect()
    }

    /// Persist the pending queue lanes; latest record wins per session.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot record cannot be serialized or appended.
    pub fn record_queue_snapshot(
        &mut self,
        active_session_id: &str,
        steering: &[WorkerQueueItemRecord],
        follow_up: &[WorkerQueueItemRecord],
        suspended: bool,
    ) -> Result<()> {
        let record = WorkerQueueSnapshotRecord {
            version: QUEUE_SNAPSHOT_VERSION,
            r#type: QUEUE_SNAPSHOT_RECORD_TYPE.to_string(),
            active_session_id: active_session_id.to_string(),
            steering: steering.to_vec(),
            follow_up: follow_up.to_vec(),
            suspended,
            recorded_at: crate::util::now_iso(),
            running: Vec::new(),
            running_steering: false,
        };
        let line = serde_json::to_value(&record)?;
        self.write_owner
            .write(|| append_record(&self.path, &line))?;
        self.queue_snapshots
            .insert(active_session_id.to_string(), record);
        Ok(())
    }

    /// Record the queue snapshot and the busy/operation verdict in ONE
    /// durable append: the verdict never publishes over a snapshot that
    /// did not persist. `picked_up` says whether the turn the last pickup
    /// recorded is still running, and so whether its rows ride along.
    ///
    /// # Errors
    ///
    /// Returns an error when either record cannot be serialized, the
    /// batched append fails, or the all-idle compaction fails.
    #[expect(
        clippy::too_many_arguments,
        reason = "one checkpoint records the queue, the turn and the active action together"
    )]
    pub fn record_queue_checkpoint(
        &mut self,
        active_session_id: &str,
        session_id: &str,
        session_file: Option<&str>,
        busy: bool,
        operation: &str,
        steering: &[WorkerQueueItemRecord],
        follow_up: &[WorkerQueueItemRecord],
        suspended: bool,
        picked_up: PickedUpTurn,
    ) -> Result<()> {
        let pickup = match picked_up {
            PickedUpTurn::Running => self
                .queue_snapshots
                .get(active_session_id)
                .filter(|previous| !previous.running.is_empty()),
            PickedUpTurn::Ended => None,
        };
        let snapshot = WorkerQueueSnapshotRecord {
            version: QUEUE_SNAPSHOT_VERSION,
            r#type: QUEUE_SNAPSHOT_RECORD_TYPE.to_string(),
            active_session_id: active_session_id.to_string(),
            steering: steering.to_vec(),
            follow_up: follow_up.to_vec(),
            suspended,
            recorded_at: pickup.map_or_else(crate::util::now_iso, |previous| {
                previous.recorded_at.clone()
            }),
            running: pickup.map_or_default(|previous| previous.running.clone()),
            running_steering: pickup.is_some_and(|previous| previous.running_steering),
        };
        let verdict_unchanged = self.latest.get(active_session_id).is_some_and(|previous| {
            previous.busy == busy
                && previous.operation == operation
                && previous.session_file.as_deref() == session_file
        });
        let record = if verdict_unchanged {
            None
        } else {
            Some(WorkerRecoveryRecord {
                active_session_id: active_session_id.to_string(),
                session_id: session_id.to_string(),
                session_file: session_file.map(str::to_string),
                busy,
                operation: operation.to_string(),
                recorded_at: crate::util::now_iso(),
            })
        };
        let mut batch = Vec::with_capacity(2);
        batch.push(serde_json::to_value(&snapshot)?);
        if let Some(record) = &record {
            batch.push(serde_json::to_value(record)?);
        }
        self.write_owner
            .write(|| append_records(&self.path, &batch))?;
        self.queue_snapshots
            .insert(active_session_id.to_string(), snapshot);
        if let Some(record) = record {
            self.latest.insert(active_session_id.to_string(), record);
            // TS parity (same post-insert check as `record`): the
            // compaction fires on the all-idle map including the verdict.
            if self.latest.values().all(|entry| !entry.busy) {
                self.compact(Finalize::Bare)?;
            }
        }
        Ok(())
    }

    /// The latest persisted queue rows for `active_session_id`.
    #[must_use]
    pub fn latest_queue_snapshot(&self, active_session_id: &str) -> Option<QueueSnapshotLanes> {
        self.queue_snapshots
            .get(active_session_id)
            .map(WorkerQueueSnapshotRecord::restored_lanes)
    }

    /// Record a turn's pickup: the lanes as they are now, and the rows the
    /// turn took off one of them. Until the next snapshot, a restore
    /// redelivers those rows first.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot record cannot be serialized or appended.
    pub(crate) fn record_turn_pickup(
        &mut self,
        active_session_id: &str,
        running: Vec<WorkerQueueItemRecord>,
        running_steering: bool,
        lanes: crate::worker::QueueLanes,
        suspended: bool,
    ) -> Result<()> {
        let record = WorkerQueueSnapshotRecord {
            version: QUEUE_SNAPSHOT_VERSION,
            r#type: QUEUE_SNAPSHOT_RECORD_TYPE.to_string(),
            active_session_id: active_session_id.to_string(),
            steering: lanes.steering,
            follow_up: lanes.follow_up,
            suspended,
            recorded_at: crate::util::now_iso(),
            running,
            running_steering,
        };
        let line = serde_json::to_value(&record)?;
        self.write_owner
            .write(|| append_record(&self.path, &line))?;
        self.queue_snapshots
            .insert(active_session_id.to_string(), record);
        Ok(())
    }

    /// Forget the turns that were running when the journal's writer went
    /// down, keeping every queued row behind them: a session rebuilt after
    /// its own crash must not run the turn that crashed it again.
    ///
    /// # Errors
    ///
    /// Returns an error when the journal cannot be opened or appended.
    pub fn drop_running_turns(path: &Path) -> Result<()> {
        let mut journal = Self::open(path)?;
        for record in journal.queue_snapshots.values_mut() {
            if !record.running.is_empty() {
                record.running.clear();
                record.recorded_at = crate::util::now_iso();
                let line = serde_json::to_value(&*record)?;
                journal
                    .write_owner
                    .write(|| append_record(&journal.path, &line))?;
            }
        }
        Ok(())
    }

    /// Replace the rows of each turn that was running when the journal's
    /// writer went down: `replace` is shown every snapshot that holds such
    /// rows and answers what a restore redelivers in their place (`None`
    /// keeps them).
    ///
    /// # Errors
    ///
    /// Returns an error when the journal cannot be opened or appended.
    pub(crate) fn replace_running_turns(
        path: &Path,
        mut replace: impl FnMut(&WorkerQueueSnapshotRecord) -> Option<Vec<WorkerQueueItemRecord>>,
    ) -> Result<()> {
        let mut journal = Self::open(path)?;
        for record in journal.queue_snapshots.values_mut() {
            if record.running.is_empty() {
                continue;
            }
            if let Some(running) = replace(record) {
                record.running = running;
                record.recorded_at = crate::util::now_iso();
                let line = serde_json::to_value(&*record)?;
                journal
                    .write_owner
                    .write(|| append_record(&journal.path, &line))?;
            }
        }
        Ok(())
    }

    /// Read the latest queue snapshot for a session straight from a journal
    /// file (worker restore on a fresh process).
    ///
    /// # Errors
    ///
    /// Returns an error when the journal exists but cannot be read (a
    /// missing journal answers `Ok(None)`).
    pub fn read_queue_snapshot(
        path: &Path,
        active_session_id: &str,
    ) -> Result<Option<QueueSnapshotLanes>> {
        Ok(parse_queue_snapshot_records(path)?
            .get(active_session_id)
            .map(WorkerQueueSnapshotRecord::restored_lanes))
    }

    fn compact(&self, finalize: Finalize) -> Result<()> {
        let mut records: Vec<Value> = self
            .latest
            .values()
            .map(serde_json::to_value)
            .collect::<std::result::Result<_, _>>()?;
        let snapshots: Vec<Value> = self
            .queue_snapshots
            .values()
            .map(serde_json::to_value)
            .collect::<std::result::Result<_, _>>()?;
        records.extend(snapshots);
        self.write_owner
            .write(|| rewrite_records(&self.path, &records, finalize))
    }
}

const QUEUE_SNAPSHOT_RECORD_TYPE: &str = "queue_snapshot";
/// The current queue-snapshot record version: the lanes carry the full
/// item records.
const QUEUE_SNAPSHOT_VERSION: u32 = 2;

fn parse_queue_snapshot_records(path: &Path) -> Result<HashMap<String, WorkerQueueSnapshotRecord>> {
    let mut latest: HashMap<String, WorkerQueueSnapshotRecord> = HashMap::new();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(latest),
        Err(error) => {
            return Err(error).with_context(|| format!("read journal {}", path.display()))
        }
    };
    for line in decode_journal_lines(&bytes, path)? {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) != Some(QUEUE_SNAPSHOT_RECORD_TYPE) {
            continue;
        }
        let version = record.get("version").and_then(Value::as_u64);
        if version != Some(1) && version != Some(u64::from(QUEUE_SNAPSHOT_VERSION)) {
            continue;
        }
        let Some(active_session_id) = record.get("active_session_id").and_then(Value::as_str)
        else {
            continue;
        };
        let entry = WorkerQueueSnapshotRecord {
            version: QUEUE_SNAPSHOT_VERSION,
            r#type: QUEUE_SNAPSHOT_RECORD_TYPE.to_string(),
            active_session_id: active_session_id.to_string(),
            steering: parse_snapshot_lane(record.get("steering")),
            follow_up: parse_snapshot_lane(record.get("follow_up")),
            suspended: record
                .get("suspended")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            recorded_at: record
                .get("recorded_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            running: parse_snapshot_lane(record.get("running")),
            running_steering: record
                .get("running_steering")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        latest.insert(active_session_id.to_string(), entry);
    }
    Ok(latest)
}

/// One snapshot lane: a version-2 entry is the full item record; a
/// version-1 entry is the bare message text and restores as a plain row.
fn parse_snapshot_lane(value: Option<&Value>) -> Vec<WorkerQueueItemRecord> {
    value.and_then(Value::as_array).map_or_default(|entries| {
        entries
            .iter()
            .filter_map(|entry| match entry {
                Value::String(message) => Some(WorkerQueueItemRecord {
                    message: message.clone(),
                    priority: None,
                    preview: None,
                    custom_message: None,
                    queue_key: None,
                    queue_visible: true,
                    policy: queue_policy_default(),
                    forced_batch: false,
                    images: Vec::new(),
                }),
                Value::Object(_) => serde_json::from_value(entry.clone()).ok(),
                _ => None,
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        (tmp, dir.join(name))
    }

    #[test]
    fn worker_journal_read_failures_are_not_empty_recovery_state() {
        let (_tmp, path) = temp_path("unreadable.jsonl");
        fs::create_dir_all(&path).unwrap();
        let sentinel = path.join("preserve");
        fs::write(&sentinel, b"original data").unwrap();

        assert!(tail_is_torn(&path).is_err());
        assert!(parse_worker_records(&path).is_err());
        assert!(WorkerRecoveryJournal::open(&path).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"original data");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn worker_journal_keeps_latest_per_session() {
        let (_tmp, path) = temp_path("worker.recovery.jsonl");
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal
            .record("s1", "sess1", Some("/a.jsonl"), true, "prompt")
            .unwrap();
        journal.record("s2", "sess2", None, false, "ready").unwrap();
        journal
            .record("s1", "sess1", Some("/a.jsonl"), false, "idle")
            .unwrap();
        let latest = WorkerRecoveryJournal::read_latest(&path).unwrap();
        assert_eq!(latest.len(), 2);
        let s1 = latest.iter().find(|r| r.active_session_id == "s1").unwrap();
        assert!(!s1.busy);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn worker_journal_interrupted_evidence_tracks_latest_busy() {
        let (_tmp, path) = temp_path("interrupted.recovery.jsonl");
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal
            .record("s1", "sess1", None, false, "shutdown")
            .unwrap();
        journal.record("s2", "sess2", None, false, "ready").unwrap();
        assert!(!WorkerRecoveryJournal::read_interrupted(&path));
        journal
            .record("s2", "sess2", Some("/b.jsonl"), true, "create")
            .unwrap();
        assert!(WorkerRecoveryJournal::read_interrupted(&path));
        journal
            .record("s2", "sess2", None, false, "shutdown")
            .unwrap();
        assert!(!WorkerRecoveryJournal::read_interrupted(&path));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn worker_journal_batched_checkpoint_matches_sequential_form() {
        let (_tmp, sequential_path) = temp_path("sequential.recovery.jsonl");
        let (_tmp, batched_path) = temp_path("batched.recovery.jsonl");
        let mut sequential = WorkerRecoveryJournal::open(&sequential_path).unwrap();
        let mut batched = WorkerRecoveryJournal::open(&batched_path).unwrap();
        let item = WorkerQueueItemRecord {
            message: "steer me".to_string(),
            priority: Some(crate::worker::QueuePriority::Human),
            preview: Some("preview".to_string()),
            custom_message: None,
            queue_key: None,
            queue_visible: true,
            policy: queue_policy_default(),
            forced_batch: false,
            images: Vec::new(),
        };
        sequential
            .record_queue_snapshot("s1", std::slice::from_ref(&item), &[], false)
            .unwrap();
        sequential
            .record("s1", "sess1", Some("/a.jsonl"), true, "prompt_accepted")
            .unwrap();
        sequential
            .record_queue_snapshot("s1", &[], &[], false)
            .unwrap();
        sequential
            .record("s1", "sess1", Some("/a.jsonl"), false, "turn_end")
            .unwrap();
        sequential
            .record_queue_snapshot("s1", &[], &[], false)
            .unwrap();
        sequential
            .record("s1", "sess1", Some("/a.jsonl"), false, "turn_end")
            .unwrap();
        batched
            .record_queue_checkpoint(
                "s1",
                "sess1",
                Some("/a.jsonl"),
                true,
                "prompt_accepted",
                std::slice::from_ref(&item),
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        batched
            .record_queue_checkpoint(
                "s1",
                "sess1",
                Some("/a.jsonl"),
                false,
                "turn_end",
                &[],
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        batched
            .record_queue_checkpoint(
                "s1",
                "sess1",
                Some("/a.jsonl"),
                false,
                "turn_end",
                &[],
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();

        let strip_stamps = |path: &std::path::Path| -> Vec<Value> {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    let mut value: Value = serde_json::from_str(line).unwrap();
                    if let Some(object) = value.as_object_mut() {
                        object.remove("recordedAt");
                        object.remove("recorded_at");
                    }
                    value
                })
                .collect()
        };
        assert_eq!(
            strip_stamps(&sequential_path),
            strip_stamps(&batched_path),
            "the batched checkpoint writes the same journal lines as the sequential form"
        );
        let latest_a = sequential.get_latest();
        let latest_b = batched.get_latest();
        assert_eq!(latest_a.len(), latest_b.len());
        assert_eq!(latest_a[0].busy, latest_b[0].busy);
        assert_eq!(latest_a[0].operation, latest_b[0].operation);
        let restored = WorkerRecoveryJournal::read_queue_snapshot(&batched_path, "s1").unwrap();
        assert_eq!(restored, Some((Vec::new(), Vec::new(), false)));
        let _ = fs::remove_dir_all(sequential_path.parent().unwrap());
        let _ = fs::remove_dir_all(batched_path.parent().unwrap());
    }

    #[test]
    fn worker_journal_batched_checkpoint_is_all_or_nothing() {
        let (_tmp, path) = temp_path("allornothing.recovery.jsonl");
        fs::write(&path, "").unwrap();
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal.record("s1", "sess1", None, false, "ready").unwrap();
        // Replace the journal with a directory: every append open now fails.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let result = journal.record_queue_checkpoint(
            "s1",
            "sess1",
            None,
            true,
            "prompt_accepted",
            &[],
            &[],
            false,
            PickedUpTurn::Ended,
        );
        assert!(result.is_err());
        assert!(journal.latest.get("s1").is_some_and(|record| !record.busy));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// TS parity oracle: TS compacts at every changed-idle record, and so
    /// does the port.
    #[test]
    fn worker_journal_settle_compacts_single_session() {
        let (_tmp, path) = temp_path("settle-compacts.recovery.jsonl");
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal
            .record("s1", "sess1", Some("/a.jsonl"), true, "prompt_accepted")
            .unwrap();
        journal
            .record("s1", "sess1", Some("/a.jsonl"), false, "turn_end")
            .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap().lines().count(),
            1,
            "the first settle compacted to the latest record"
        );
        journal
            .record("s1", "sess1", Some("/a.jsonl"), true, "prompt_accepted")
            .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap().lines().count(),
            2,
            "the second admission grows the compacted file"
        );
        journal
            .record("s1", "sess1", Some("/a.jsonl"), false, "turn_end")
            .unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "the settle compacts to the latest record");
        let record: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(record["busy"], false);
        assert_eq!(record["operation"], "turn_end");
        drop(journal);
        let reopened = WorkerRecoveryJournal::open(&path).unwrap();
        let latest = reopened.get_latest();
        assert_eq!(latest.len(), 1);
        assert!(!latest[0].busy);
        assert!(!WorkerRecoveryJournal::read_interrupted(&path));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn worker_journal_batched_settle_compacts_and_restores() {
        let (_tmp, path) = temp_path("batched-settle.recovery.jsonl");
        let item = WorkerQueueItemRecord {
            message: "steer me".to_string(),
            priority: Some(crate::worker::QueuePriority::Human),
            preview: Some("preview".to_string()),
            custom_message: None,
            queue_key: None,
            queue_visible: true,
            policy: queue_policy_default(),
            forced_batch: false,
            images: Vec::new(),
        };
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal
            .record_queue_checkpoint(
                "s1",
                "sess1",
                Some("/a.jsonl"),
                true,
                "prompt_accepted",
                std::slice::from_ref(&item),
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        journal
            .record_queue_checkpoint(
                "s1",
                "sess1",
                Some("/a.jsonl"),
                false,
                "turn_end",
                &[],
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        let after_first_settle = fs::read_to_string(&path).unwrap().lines().count();
        journal
            .record_queue_checkpoint(
                "s1",
                "sess1",
                Some("/a.jsonl"),
                true,
                "prompt_accepted",
                std::slice::from_ref(&item),
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        let after_second_admission = fs::read_to_string(&path).unwrap().lines().count();
        journal
            .record_queue_checkpoint(
                "s1",
                "sess1",
                Some("/a.jsonl"),
                false,
                "turn_end",
                &[],
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "the settle compacts to verdict + snapshot");
        assert_eq!(after_first_settle, 2, "the first settle compacted");
        assert_eq!(
            after_second_admission, 4,
            "the second admission grew the file"
        );
        let verdict: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(verdict["busy"], false);
        assert_eq!(verdict["operation"], "turn_end");
        let snapshot: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(snapshot["type"], "queue_snapshot");
        // the compact keeps the LATEST snapshot per session: the
        // settle's (empty) lanes, not the admission's parked row.
        assert_eq!(snapshot["steering"].as_array().map(Vec::len), Some(0));
        drop(journal);
        let reopened = WorkerRecoveryJournal::open(&path).unwrap();
        assert!(!WorkerRecoveryJournal::read_interrupted(&path));
        let restored = reopened.latest_queue_snapshot("s1").unwrap();
        assert_eq!(restored.0, Vec::new());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// An unchanged verdict appends the snapshot alone and never compacts
    /// (TS `record` early-returns before its compaction check).
    #[test]
    fn worker_journal_unchanged_verdict_does_not_compact() {
        let (_tmp, path) = temp_path("unchanged-nocompact.recovery.jsonl");
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal
            .record_queue_checkpoint(
                "s1",
                "sess1",
                None,
                true,
                "prompt_accepted",
                &[],
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        journal
            .record_queue_checkpoint(
                "s1",
                "sess1",
                None,
                false,
                "turn_end",
                &[],
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        let lines_after_settle = fs::read_to_string(&path).unwrap().lines().count();
        journal
            .record_queue_checkpoint(
                "s1",
                "sess1",
                None,
                false,
                "turn_end",
                &[],
                &[],
                false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        let lines_after_unchanged = fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(lines_after_unchanged, lines_after_settle + 1);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn worker_journal_missing_or_unreadable_file_is_not_interrupted() {
        let (_tmp, path) = temp_path("missing.recovery.jsonl");
        assert!(!WorkerRecoveryJournal::read_interrupted(&path));
        std::fs::write(&path, "not json").unwrap();
        assert!(!WorkerRecoveryJournal::read_interrupted(&path));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn worker_journal_open_skips_torn_utf8_tail() {
        let (_tmp, path) = temp_path("torn-utf8-tail.recovery.jsonl");
        let item = WorkerQueueItemRecord {
            message: "steer ünïcode".to_string(),
            priority: Some(crate::worker::QueuePriority::Human),
            preview: Some("preview".to_string()),
            custom_message: None,
            queue_key: None,
            queue_visible: true,
            policy: queue_policy_default(),
            forced_batch: false,
            images: Vec::new(),
        };
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal
            .record_queue_checkpoint(
                "s1",
                "sess1",
                Some("/a.jsonl"),
                true,
                "prompt_accepted",
                std::slice::from_ref(&item),
                &[],
                /*suspended*/ false,
                PickedUpTurn::Ended,
            )
            .unwrap();
        let whole = serde_json::to_string(&WorkerRecoveryRecord {
            active_session_id: "s2".to_string(),
            session_id: "sess2".to_string(),
            session_file: None,
            busy: false,
            operation: "shütdown".to_string(),
            recorded_at: "2026-10-07T00:00:00Z".to_string(),
        })
        .unwrap();
        let torn = &whole.as_bytes()[..=whole.find('ü').unwrap()];
        assert!(std::str::from_utf8(torn).is_err());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(torn).unwrap();
        drop(file);

        drop(journal);
        let mut healed = WorkerRecoveryJournal::open(&path).unwrap();
        // Reload after a fresh append so the assertions exercise the healed
        // file, rather than only the state loaded before the rewrite.
        healed
            .record("s1", "sess1", Some("/a.jsonl"), true, "resumed")
            .unwrap();
        drop(healed);
        let reopened = WorkerRecoveryJournal::open(&path).unwrap();
        assert!(WorkerRecoveryJournal::read_interrupted(&path));
        let latest = reopened.get_latest();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].active_session_id, "s1");
        assert!(latest[0].busy);
        let restored = reopened.latest_queue_snapshot("s1").unwrap();
        assert_eq!(restored.0, vec![item]);
        assert!(restored.1.is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn worker_journal_open_heals_torn_ascii_tail() {
        let (_tmp, path) = temp_path("torn-ascii-tail.recovery.jsonl");
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal
            .record("s1", "sess1", Some("/a.jsonl"), true, "prompt_accepted")
            .unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"activeSessionId":"s3","sessionId":"ses"#)
            .unwrap();
        drop(file);

        drop(journal);
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal.record("s2", "sess2", None, false, "ready").unwrap();
        drop(journal);
        let reopened = WorkerRecoveryJournal::open(&path).unwrap();
        assert!(WorkerRecoveryJournal::read_interrupted(&path));
        assert_eq!(
            reopened
                .get_latest()
                .iter()
                .filter(|record| record.active_session_id == "s2")
                .count(),
            1,
            "the record appended after the reopened journal must survive a reload"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_complete_queue_bytes_are_preserved_before_recovery_fails() {
        for newline_terminated in [true, false] {
            let (_tmp, path) = temp_path("corrupt-complete-queue.recovery.jsonl");
            let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
            let item = WorkerQueueItemRecord {
                message: "safeXinstruction".to_string(),
                priority: None,
                preview: None,
                custom_message: None,
                queue_key: None,
                queue_visible: true,
                policy: queue_policy_default(),
                forced_batch: false,
                images: Vec::new(),
            };
            journal
                .record_queue_snapshot("s1", &[item], &[], /*suspended*/ false)
                .unwrap();
            drop(journal);
            let mut bytes = fs::read(&path).unwrap();
            let marker = b"safeXinstruction";
            let offset = bytes
                .windows(marker.len())
                .position(|part| part == marker)
                .unwrap();
            bytes[offset + 4] = 0xff;
            if newline_terminated {
                bytes.extend_from_slice(br#"{"activeSessionId":"torn"#);
            } else {
                assert_eq!(bytes.pop(), Some(b'\n'));
            }
            fs::write(&path, &bytes).unwrap();
            let result = WorkerRecoveryJournal::open(&path);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert!(
                result.is_err(),
                "complete corrupt queued prompts must block recovery"
            );
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn corrupt_complete_worker_identifiers_are_preserved_before_recovery_fails() {
        for newline_terminated in [true, false] {
            let (_tmp, path) = temp_path("corrupt-complete-verdict.recovery.jsonl");
            let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
            journal
                .record("badXsession", "sess1", None, true, "prompt_accepted")
                .unwrap();
            drop(journal);
            let mut bytes = fs::read(&path).unwrap();
            let marker = b"badXsession";
            let offset = bytes
                .windows(marker.len())
                .position(|part| part == marker)
                .unwrap();
            bytes[offset + 3] = 0xff;
            if newline_terminated {
                bytes.extend_from_slice(br#"{"activeSessionId":"torn"#);
            } else {
                assert_eq!(bytes.pop(), Some(b'\n'));
            }
            fs::write(&path, &bytes).unwrap();
            assert!(WorkerRecoveryJournal::read_latest(&path).is_err());
            assert!(WorkerRecoveryJournal::open(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_complete_worker_record_without_newline_survives_healing() {
        let (_tmp, path) = temp_path("complete-eof.recovery.jsonl");
        let mut journal = WorkerRecoveryJournal::open(&path).unwrap();
        journal
            .record("s1", "sess1", None, true, "prompt_accepted")
            .unwrap();
        let expected = journal.get_latest();
        drop(journal);
        let mut bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(&path, bytes).unwrap();
        let reopened = WorkerRecoveryJournal::open(&path).unwrap();
        assert_eq!(reopened.get_latest(), expected);
        assert_eq!(fs::read(&path).unwrap().last(), Some(&b'\n'));
        drop(reopened);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn competing_worker_writers_cannot_replace_an_active_append() {
        for contender in ["open", "settle"] {
            let (_tmp, path) = temp_path("active-append.recovery.jsonl");
            let mut owner = WorkerRecoveryJournal::open(&path).unwrap();
            owner
                .record("s1", "sess1", None, true, "prompt_accepted")
                .unwrap();
            let first = owner.get_latest().pop().unwrap();
            let record = WorkerRecoveryRecord {
                active_session_id: "s2".to_string(),
                session_id: "sess2".to_string(),
                session_file: None,
                busy: true,
                operation: "prompt_accepted".to_string(),
                recorded_at: "2026-10-11T00:00:01Z".to_string(),
            };
            let mut bytes = serde_json::to_vec(&record).unwrap();
            bytes.push(b'\n');
            let split = bytes.len() / 2;
            let mut active_append = OpenOptions::new().append(true).open(&path).unwrap();
            active_append.write_all(&bytes[..split]).unwrap();
            let expected = fs::read(&path).unwrap();
            let result = match contender {
                "open" => WorkerRecoveryJournal::open(&path).map(drop),
                "settle" => WorkerRecoveryJournal::settle_busy_records(&path, "worker_gave_up"),
                _ => unreachable!("known contender"),
            };
            assert_eq!(fs::read(&path).unwrap(), expected);
            assert!(
                result.is_err(),
                "a rival writer must not acquire the active journal"
            );
            active_append.write_all(&bytes[split..]).unwrap();
            active_append.sync_all().unwrap();
            drop(active_append);
            drop(owner);
            let reopened = WorkerRecoveryJournal::open(&path).unwrap();
            let mut latest = reopened.get_latest();
            latest.sort_by(|left, right| left.active_session_id.cmp(&right.active_session_id));
            assert_eq!(latest, vec![first, record]);
            drop(reopened);
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_journal_releases_ownership_while_a_duplicate_handle_survives() {
        let (_tmp, path) = temp_path("duplicate-owner.recovery.jsonl");
        let mut owner = WorkerRecoveryJournal::open(&path).unwrap();
        owner
            .record("s1", "sess1", None, true, "prompt_accepted")
            .unwrap();
        let expected = owner.get_latest();
        let inherited = owner.write_owner.file.try_clone().unwrap();
        assert!(WorkerRecoveryJournal::open(&path).is_err());

        drop(owner);
        let successor = WorkerRecoveryJournal::open(&path)
            .expect("logical owner drop releases its lock even while a duplicate handle survives");
        assert_eq!(successor.get_latest(), expected);
        assert!(WorkerRecoveryJournal::open(&path).is_err());

        drop(inherited);
        assert!(
            WorkerRecoveryJournal::open(&path).is_err(),
            "closing the old handle must not release the successor's ownership",
        );
        drop(successor);
        let reopened = WorkerRecoveryJournal::open(&path).unwrap();
        assert_eq!(reopened.get_latest(), expected);
        drop(reopened);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
