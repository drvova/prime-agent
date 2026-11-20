//! Paged-out cell output: the session's content-addressed spill store.
//!
//! A cell stream (stdout, stderr) or result over its transcript cap is not
//! destroyed: the whole text is written under `<session artifact dir>/spill/`
//! and the transcript carries a compact handle
//! (`[out:<id> · N chars · L lines · M chars not shown]`) where the
//! truncation marker used to be. The handle names the text by its content
//! hash, so the same output always gets the same handle.
//!
//! The host owns the store, not the kernel runtime: the host already receives
//! every byte of a cell's streams and result (the cut happens host-side, in
//! the kernel manager), it owns the session directory and its layout, and the
//! store must outlive any one kernel process (a crashed or restarted kernel
//! would lose a write the kernel had queued). Readers reach it through host
//! requests, so the kernel never learns the on-disk layout and the dependency
//! direction stays host → kernel.
//!
//! The write stays off the cell-result path: [`SpillStore::spill`] hashes the
//! text (CPU only), hands it to the store's writer thread over a bounded queue
//! without blocking, and returns the handle at once. The writer retries a
//! failed write a bounded number of times. A write that still fails, or a
//! spill the full queue cannot take, is recorded as not stored, so the handle
//! answers honestly instead of pointing at nothing.
//!
//! Each spill is written to a temp file and renamed into place; the
//! append-only `index.jsonl` beside the files records each handle's state
//! (stored, failed, evicted). A fresh process (resume, daemon restart) folds
//! the index, so handles outlive the process that wrote them. Stored bytes
//! stay under the store's cap ([`SPILL_STORE_CAP_BYTES`]) by evicting the
//! oldest stored spills first; an evicted handle answers "evicted".

mod index;
mod marker;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use sha2::{Digest, Sha256};

use index::{Entry, Index, IndexRecord, StoredMeta};

/// The prefix every spill handle carries (`out:7f3a9c1e04`).
pub(crate) const HANDLE_PREFIX: &str = "out:";
/// Stored bytes one session's store keeps before evicting its oldest spills.
pub const SPILL_STORE_CAP_BYTES: u64 = 64 * 1024 * 1024;
/// The most text one stream retains for its spill. Past it the stream stops
/// copying and the handle says how much of the output was stored.
pub(crate) const SPILL_COPY_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Hex digits of the content hash a handle carries.
const ID_HEX_LEN: usize = 10;
/// Spills the writer thread may have queued before a new one is refused.
const WRITE_QUEUE_DEPTH: usize = 16;
/// Waits before each retry of a failed write (so `len() + 1` attempts).
const WRITE_RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(25), Duration::from_millis(100)];
/// How long a read waits for this process's in-flight write of its handle.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the ctx.* host requests of the next change read the store"
    )
)]
const PENDING_READ_WAIT: Duration = Duration::from_secs(5);
/// The directory under a session's artifact dir that holds the store.
const SPILL_DIR: &str = "spill";
const INDEX_FILE: &str = "index.jsonl";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One session's spill store. Cheap to clone; every clone (and every
/// [`SpillStore::for_session_dir`] call for the same directory in this
/// process) shares one writer, so the index has a single appender.
#[derive(Clone)]
pub struct SpillStore {
    inner: Arc<StoreInner>,
}

impl std::fmt::Debug for SpillStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpillStore")
            .field("dir", &self.inner.state.dir)
            .finish_non_exhaustive()
    }
}

struct StoreInner {
    /// The writer thread's queue, started by the first spill.
    sender: OnceLock<SyncSender<Job>>,
    state: Arc<StoreState>,
}

/// State the writer thread shares with spills and reads. It never holds the
/// sender, so dropping the last [`SpillStore`] ends the writer.
struct StoreState {
    dir: PathBuf,
    cap_bytes: u64,
    /// Spills queued in this process and not yet resolved, by id (a count:
    /// the same content can be queued twice). The cell-result path takes only
    /// this lock, and never across disk work.
    pending: Mutex<HashMap<String, usize>>,
    /// Spills this process could not queue, with the reason, so their handle
    /// answers "not stored" instead of "unknown".
    refused: Mutex<HashMap<String, String>>,
    /// The folded index, loaded on first use by the writer or a read.
    index: Mutex<Option<Index>>,
    /// Woken whenever a queued spill resolves.
    settled: tokio::sync::Notify,
}

/// One queued write.
struct Job {
    id: String,
    text: String,
    meta: StoredMeta,
}

/// The text one cell stream retains for its spill, with the counts of the
/// whole stream (which outlive the copy's own cap).
#[derive(Debug, Default)]
pub(crate) struct SpillCopy {
    text: String,
    total_chars: usize,
    newlines: usize,
    ends_with_newline: bool,
    /// The stream outgrew [`SPILL_COPY_MAX_BYTES`]; `text` is its prefix.
    partial: bool,
}

impl SpillCopy {
    /// A copy of one whole text (a cell result).
    pub(crate) fn of_text(text: &str) -> Self {
        let mut copy = Self::default();
        copy.push(text, text.chars().count());
        copy
    }

    /// Append one stream frame of `chars` Unicode scalars.
    pub(crate) fn push(&mut self, text: &str, chars: usize) {
        if text.is_empty() {
            return;
        }
        self.total_chars += chars;
        self.newlines += text.bytes().filter(|byte| *byte == b'\n').count();
        self.ends_with_newline = text.ends_with('\n');
        if self.partial {
            return;
        }
        let room = SPILL_COPY_MAX_BYTES - self.text.len();
        if text.len() <= room {
            self.text.push_str(text);
            return;
        }
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        self.partial = true;
    }

    /// Chars of the whole stream.
    pub(crate) fn total_chars(&self) -> usize {
        self.total_chars
    }

    /// Lines of the whole stream (a final unterminated line counts).
    fn lines(&self) -> usize {
        self.newlines + usize::from(self.total_chars > 0 && !self.ends_with_newline)
    }
}

/// What a spill left in the transcript: its id, the whole output's counts,
/// and whether the store took it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpillHandle {
    pub(crate) id: String,
    pub(crate) total_chars: usize,
    pub(crate) lines: usize,
    /// Chars of the output the store holds (less than `total_chars` when the
    /// stream outgrew its copy).
    pub(crate) stored_chars: usize,
    pub(crate) outcome: SpillOutcome,
}

/// Whether the store took a spill when it was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpillOutcome {
    /// Handed to the writer; its write runs in the background.
    Queued,
    /// The writer could not take it; the reason is the handle's answer.
    NotStored(String),
}

/// What a handle answers when read.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the ctx.* host requests of the next change read the store"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpillLookup {
    Stored(StoredSpill),
    /// The store's size cap evicted it.
    Evicted,
    /// It was never stored; the reason says why.
    NotStored(String),
    /// This process is still writing it.
    Pending,
    /// No spill of this session has this id.
    Unknown,
}

/// One stored spill's text and the counts of the output it came from.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the ctx.* host requests of the next change read the store"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredSpill {
    pub(crate) text: String,
    pub(crate) total_chars: usize,
    pub(crate) lines: usize,
}

/// The id a handle names `text` by: the leading hex digits of its SHA-256.
pub(crate) fn content_id(text: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
    digest[..ID_HEX_LEN].to_string()
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<StoreInner>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<StoreInner>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

impl SpillStore {
    /// The store of the session whose artifact directory is `session_dir`
    /// (spills live in its `spill/` child). No disk work: the directory is
    /// created by the first write. Every call for one directory in this
    /// process shares one store.
    #[must_use]
    pub fn for_session_dir(session_dir: &Path) -> Self {
        let dir = session_dir.join(SPILL_DIR);
        let mut stores = lock(registry());
        if let Some(inner) = stores.get(&dir).and_then(Weak::upgrade) {
            return Self { inner };
        }
        stores.retain(|_, store| store.strong_count() > 0);
        let store = Self::unregistered(dir.clone(), SPILL_STORE_CAP_BYTES);
        stores.insert(dir, Arc::downgrade(&store.inner));
        store
    }

    /// A store over `dir` that no other instance shares (tests open a second
    /// one over the same directory to stand in for a restarted process).
    fn unregistered(dir: PathBuf, cap_bytes: u64) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                sender: OnceLock::new(),
                state: Arc::new(StoreState {
                    dir,
                    cap_bytes,
                    pending: Mutex::new(HashMap::new()),
                    refused: Mutex::new(HashMap::new()),
                    index: Mutex::new(None),
                    settled: tokio::sync::Notify::new(),
                }),
            }),
        }
    }

    /// Page `copy` out: queue its write and return its handle at once. Never
    /// touches disk and never blocks on the writer.
    pub(crate) fn spill(&self, copy: SpillCopy) -> SpillHandle {
        let id = content_id(&copy.text);
        let meta = StoredMeta {
            bytes: copy.text.len() as u64,
            chars: copy.text.chars().count(),
            total_chars: copy.total_chars,
            lines: copy.lines(),
        };
        let mut handle = SpillHandle {
            id: id.clone(),
            total_chars: meta.total_chars,
            lines: meta.lines,
            stored_chars: meta.chars,
            outcome: SpillOutcome::Queued,
        };
        let state = &self.inner.state;
        *lock(&state.pending).entry(id.clone()).or_insert(0) += 1;
        let job = Job {
            id: id.clone(),
            text: copy.text,
            meta,
        };
        let refusal = match self.sender().try_send(job) {
            Ok(()) => {
                lock(&state.refused).remove(&id);
                return handle;
            }
            Err(TrySendError::Full(_)) => "the spill writer is busy",
            Err(TrySendError::Disconnected(_)) => "the spill writer is not running",
        };
        state.resolve_pending(&id);
        lock(&state.refused).insert(id, refusal.to_string());
        handle.outcome = SpillOutcome::NotStored(refusal.to_string());
        handle
    }

    /// The file a stored spill's text lives in (the user can open it while no reader exists).
    pub(crate) fn stored_path(&self, id: &str) -> PathBuf {
        self.inner.state.spill_path(id)
    }

    /// The writer thread's queue, starting the thread on first use. A thread
    /// that cannot start leaves a disconnected queue: every spill then
    /// answers "not stored".
    fn sender(&self) -> &SyncSender<Job> {
        self.inner.sender.get_or_init(|| {
            let (sender, receiver) = sync_channel(WRITE_QUEUE_DEPTH);
            let state = Arc::clone(&self.inner.state);
            let spawned = std::thread::Builder::new()
                .name("spill-writer".to_string())
                .spawn(move || run_writer(&state, &receiver));
            if let Err(error) = spawned {
                tracing::warn!(%error, "spill writer thread did not start");
            }
            sender
        })
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the ctx.* host requests of the next change read the store"
        )
    )]
    /// Answer one handle: its stored text, or why there is none. Waits (up
    /// to [`PENDING_READ_WAIT`]) for this process's in-flight write of it.
    pub(crate) async fn lookup(&self, id: &str) -> SpillLookup {
        let state = Arc::clone(&self.inner.state);
        let settled = state.wait_settled(id).await;
        if !settled {
            return SpillLookup::Pending;
        }
        if let Some(reason) = lock(&state.refused).get(id) {
            return SpillLookup::NotStored(reason.clone());
        }
        let id = id.to_string();
        tokio::task::spawn_blocking(move || state.lookup_on_disk(&id))
            .await
            .unwrap_or_else(|error| SpillLookup::NotStored(format!("the read failed: {error}")))
    }
}

impl StoreState {
    fn resolve_pending(&self, id: &str) {
        let mut pending = lock(&self.pending);
        if let Some(count) = pending.get_mut(id) {
            *count -= 1;
            if *count == 0 {
                pending.remove(id);
            }
        }
        drop(pending);
        self.settled.notify_waiters();
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the ctx.* host requests of the next change read the store"
        )
    )]
    /// Whether `id` has no write in flight here, waiting a bounded time for
    /// one to resolve.
    async fn wait_settled(&self, id: &str) -> bool {
        let deadline = tokio::time::Instant::now() + PENDING_READ_WAIT;
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !lock(&self.pending).contains_key(id) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return !lock(&self.pending).contains_key(id);
            }
        }
    }

    /// Run `f` over the folded index, loading it from disk on first use.
    fn with_index<T>(&self, f: impl FnOnce(&mut Index) -> T) -> T {
        let mut guard = lock(&self.index);
        let index = guard.get_or_insert_with(|| Index::load(&self.dir.join(INDEX_FILE)));
        f(index)
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the ctx.* host requests of the next change read the store"
        )
    )]
    fn lookup_on_disk(&self, id: &str) -> SpillLookup {
        let entry = self.with_index(|index| index.entries.get(id).cloned());
        let path = self.spill_path(id);
        match entry {
            Some(Entry::Stored(meta)) => match std::fs::read_to_string(&path) {
                Ok(text) => SpillLookup::Stored(StoredSpill {
                    text,
                    total_chars: meta.total_chars,
                    lines: meta.lines,
                }),
                Err(error) => {
                    SpillLookup::NotStored(format!("the stored file is unreadable: {error}"))
                }
            },
            Some(Entry::Evicted) => SpillLookup::Evicted,
            Some(Entry::Failed(reason)) => SpillLookup::NotStored(reason),
            // A write whose index row did not land still left its file.
            None => match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let copy = SpillCopy::of_text(&text);
                    SpillLookup::Stored(StoredSpill {
                        total_chars: copy.total_chars,
                        lines: copy.lines(),
                        text,
                    })
                }
                Err(_) => SpillLookup::Unknown,
            },
        }
    }

    fn spill_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.txt"))
    }

    /// Append one record to the index file and fold it into the index.
    fn record(&self, index: &mut Index, record: IndexRecord) {
        if let Err(error) = self.append_record(&record) {
            tracing::warn!(%error, "spill index append failed");
        }
        index.apply(record);
    }

    fn append_record(&self, record: &IndexRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(INDEX_FILE))?
            .write_all(line.as_bytes())
    }

    /// Write one spill: temp file, then rename into place. A spill whose
    /// file already exists (the same content) is already stored.
    fn write_spill(&self, id: &str, text: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.spill_path(id);
        if path.exists() {
            return Ok(());
        }
        let temp = self.dir.join(format!("{id}.tmp"));
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        crate::platform::rename_onto(&temp, &path)
    }

    /// Store one job (with bounded retries), then evict past the cap.
    fn store(&self, job: &Job) {
        let stored =
            self.with_index(|index| matches!(index.entries.get(&job.id), Some(Entry::Stored(_))));
        if stored && self.spill_path(&job.id).exists() {
            return;
        }
        // The write runs without the index lock: a slow disk delays only
        // this job, never a read of another handle.
        let mut attempt = self.write_spill(&job.id, &job.text);
        for backoff in WRITE_RETRY_BACKOFF {
            if attempt.is_ok() {
                break;
            }
            std::thread::sleep(backoff);
            attempt = self.write_spill(&job.id, &job.text);
        }
        let mut guard = lock(&self.index);
        let index = guard.get_or_insert_with(|| Index::load(&self.dir.join(INDEX_FILE)));
        if let Err(error) = attempt {
            self.record(
                index,
                IndexRecord::Failed {
                    id: job.id.clone(),
                    error: format!("the write failed: {error}"),
                },
            );
            return;
        }
        lock(&self.refused).remove(&job.id);
        self.record(
            index,
            IndexRecord::Stored {
                id: job.id.clone(),
                meta: job.meta.clone(),
            },
        );
        while index.stored_bytes > self.cap_bytes {
            let Some(oldest) = index.oldest_stored_except(&job.id) else {
                break;
            };
            if let Err(error) = std::fs::remove_file(self.spill_path(&oldest)) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, "spill eviction could not remove a file");
                }
            }
            self.record(index, IndexRecord::Evicted { id: oldest });
        }
    }
}

/// The writer thread: store each queued job, resolving it for waiting reads.
/// Ends when the last [`SpillStore`] (the queue's sender) drops.
fn run_writer(state: &StoreState, receiver: &Receiver<Job>) {
    while let Ok(job) = receiver.recv() {
        state.store(&job);
        state.resolve_pending(&job.id);
    }
}
