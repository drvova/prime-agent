//! The transcript store: the one boundary every session-transcript writer and
//! reader goes through.
//!
//! A transcript is addressed by its locator, the `<sessions dir>/<id>.jsonl`
//! path the rest of the system already uses as a session's identity (the wire
//! `sessionFile`, the lease key, the ledger edges, the sidecar and artifact
//! paths derived from it). A store keeps a transcript's JSONL lines verbatim:
//! what [`TranscriptStore::read`] returns is byte for byte what was created
//! and appended, so a cursor into those bytes ([`TranscriptReader`] positions)
//! means the same thing to every implementation.
//!
//! [`store`] is the process-wide store. It holds every session's transcript,
//! so it is process-wide by design, like the telemetry origin: no session's
//! scope selects it (it is chosen from the process's own agent dir), and a
//! session reaches its own transcript only by its locator.
//!
//! Two implementations:
//!
//! - The SQLite store (`SqliteTranscriptStore`, the default): every
//!   transcript in `<agent dir>/agent.db` (`pa_log`), keyed by its canonical
//!   locator. A JSONL file the database does not hold yet is read from the
//!   file, imported whole on its first write, and then left in place,
//!   read-only, as the rollback; [`import_session_trees`] imports the agent
//!   dir's session trees in the background, and [`export_jsonl`] writes
//!   transcripts back out as JSONL files.
//! - The file store (`FileTranscriptStore`): each transcript in the JSONL
//!   file at its locator. The global setting `"transcriptStore": "file"`
//!   selects it, the escape hatch for one release.
//!
//! The window and session-info caches keep their sidecar files next to the
//! locator with either store, and certify them against the store's
//! [`TranscriptGeneration`]s, so a sidecar written under one store is never
//! taken for the other's bytes.

use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::Digest;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

mod export;
mod file;
mod repair;
mod sqlite;

pub use export::{export_jsonl, ExportReport, ExportSelection};

use file::FileTranscriptStore;
use sqlite::{OriginalImport, SqliteTranscriptStore};

/// The database file in the agent dir.
pub const DATABASE_FILE: &str = "agent.db";

/// The test-isolation seam: when set (to a directory), every agent dir's
/// transcript database is `<dir>/<sha256 of the agent dir>.db` instead of
/// `<agent dir>/agent.db`. The repository's `.cargo/config.toml` sets it for
/// every `cargo test` and `cargo run`, so a test run never creates or writes
/// the database of a real agent dir (the developer's own `~/.prime/agent`
/// included), while the processes of one test that share an agent dir (a
/// daemon and the `export-jsonl` that reads it back) still share its
/// database. Installed builds never set it.
pub const DATABASE_DIR_ENV: &str = "PRIME_AGENT_TRANSCRIPT_DB_DIR";

/// Where the transcript database of `agent_dir` is: `<agent dir>/agent.db`,
/// or under [`DATABASE_DIR_ENV`] when the process sets it.
#[must_use]
pub fn database_path(agent_dir: &Path) -> PathBuf {
    let isolated = {
        // The process's own setting, never a session's.
        let _process = pa_types::session_scope::enter(None);
        pa_types::session_scope::var_os(DATABASE_DIR_ENV).filter(|dir| !dir.is_empty())
    };
    let Some(dir) = isolated else {
        return agent_dir.join(DATABASE_FILE);
    };
    let digest = sha2::Sha256::digest(spelled(agent_dir).to_string_lossy().as_bytes());
    let name = digest.iter().fold(String::new(), |mut name, byte| {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
        name
    });
    PathBuf::from(dir).join(format!("{name}.db"))
}

/// One spelling of `path` for every process: the nearest existing ancestor
/// canonicalized (links resolved, Windows short names expanded) and the
/// missing tail rejoined, so a path named before it exists is spelled as it
/// is once created.
pub(crate) fn spelled(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    loop {
        if let Ok(canonical) = pa_types::platform::canonicalize(existing) {
            return missing
                .iter()
                .rev()
                .fold(canonical, |joined, part| joined.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return absolute.clone(),
        }
    }
}

/// The store the process uses.
enum Selected {
    /// The file store, and why the database could not be opened when that is
    /// why it is in use.
    Files(FileTranscriptStore, Option<String>),
    /// The store, the agent dir whose database it opened, and the database.
    Sqlite(SqliteTranscriptStore, PathBuf, PathBuf),
}

/// Which store [`store`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptStoreKind {
    /// Every transcript in `<agent dir>/agent.db`.
    Sqlite,
    /// One JSONL file per transcript (`"transcriptStore": "file"`, or the
    /// database could not be opened).
    File,
}

static SELECTED: OnceLock<Selected> = OnceLock::new();

fn selected() -> &'static Selected {
    SELECTED.get_or_init(|| {
        let _process = pa_types::session_scope::enter(None);
        match pa_types::platform::agent_dir() {
            Some(agent_dir) => select(agent_dir),
            None => Selected::Files(FileTranscriptStore, None),
        }
    })
}

/// Select the process's [`store`] for `agent_dir`: the agent dir the binary
/// resolved for everything else it keeps (the daemon's `--agent-dir`, the
/// CLI's configured dir), so its transcript database never follows a
/// different resolution than its sessions dir, settings and logs. A binary
/// calls it once at start, before any transcript is touched; without it the
/// store resolves the process's agent dir itself on first use. Returns
/// whether this call selected the store (false: it was in use already).
pub fn use_agent_dir(agent_dir: &Path) -> bool {
    let mut chose = false;
    SELECTED.get_or_init(|| {
        chose = true;
        let _process = pa_types::session_scope::enter(None);
        select(agent_dir.to_path_buf())
    });
    chose
}

/// Choose the process's store for `agent_dir` (never a session's): the
/// SQLite store unless the global settings say `"transcriptStore": "file"`,
/// and the file store when the database cannot be opened.
fn select(agent_dir: PathBuf) -> Selected {
    let settings = crate::settings::SettingsManager::create(&agent_dir, &agent_dir);
    match settings.global_settings().transcript_store.as_deref() {
        Some("file") => return Selected::Files(FileTranscriptStore, None),
        Some("sqlite") | None => {}
        Some(other) => eprintln!(
            "transcript store: unknown \"transcriptStore\": {other:?} (\"sqlite\" or \"file\"); \
             using sqlite"
        ),
    }
    let database = database_path(&agent_dir);
    match SqliteTranscriptStore::open(&database) {
        Ok(store) => Selected::Sqlite(store, agent_dir, database),
        Err(error) => {
            let reason = format!("{} ({error:#})", database.display());
            // The daemon log is its standard error (no tracing subscriber is
            // installed); the hello carries the reason to clients.
            eprintln!("transcript store: cannot open {reason}; sessions are kept as JSONL files");
            Selected::Files(FileTranscriptStore, Some(reason))
        }
    }
}

/// The process-wide transcript store.
#[must_use]
pub fn store() -> &'static dyn TranscriptStore {
    match selected() {
        Selected::Files(store, _) => store,
        Selected::Sqlite(store, ..) => store,
    }
}

/// A SQLite store over the database at `database` (another agent dir's
/// `agent.db`, to read or export what another process keeps there), apart
/// from the process's own [`store`]. Files it does not hold are read as they
/// are, as the process's store reads them.
///
/// # Errors
///
/// The database cannot be opened or migrated.
pub fn open_database(database: &Path) -> anyhow::Result<Box<dyn TranscriptStore>> {
    Ok(Box::new(SqliteTranscriptStore::open(database)?))
}

/// The database the process's [`store`] keeps transcripts in (none for the
/// file store).
#[must_use]
pub fn database() -> Option<&'static Path> {
    match selected() {
        Selected::Files(..) => None,
        Selected::Sqlite(_, _, database) => Some(database),
    }
}

/// The database the process's [`store`] keeps (none for the file store):
/// the agent's other state ([`crate::agent_state`]) is kept in it too.
pub(crate) fn database_log() -> Option<&'static pa_log::Log> {
    match selected() {
        Selected::Files(..) => None,
        Selected::Sqlite(store, ..) => Some(store.log()),
    }
}

/// [`database_log`] when the process opened the database of `agent_dir`.
pub(crate) fn database_log_of(agent_dir: &Path) -> Option<&'static pa_log::Log> {
    let Selected::Sqlite(store, opened, _) = selected() else {
        return None;
    };
    let resolved =
        |dir: &Path| pa_types::platform::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    (resolved(opened) == resolved(agent_dir)).then(|| store.log())
}

/// Why the process's [`store`] is the file store although the settings asked
/// for the SQLite one: the database that could not be opened, and the error.
/// `None` when the database is in use or the settings chose the file store.
/// The daemon's hello carries it to clients, since nothing else of the
/// fallback is visible to the user.
#[must_use]
pub fn fallback_reason() -> Option<&'static str> {
    match selected() {
        Selected::Files(_, reason) => reason.as_deref(),
        Selected::Sqlite(..) => None,
    }
}

/// Which store [`store`] is.
#[must_use]
pub fn store_kind() -> TranscriptStoreKind {
    match selected() {
        Selected::Files(..) => TranscriptStoreKind::File,
        Selected::Sqlite(..) => TranscriptStoreKind::Sqlite,
    }
}

/// What [`import_session_trees`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TranscriptImportReport {
    /// Session files imported by this pass.
    pub imported: u64,
    /// Their bytes.
    pub bytes: u64,
    /// Session files the database already held.
    pub already_stored: u64,
    /// Files that could not be imported, and why; nothing of them is stored.
    pub failed: Vec<(PathBuf, String)>,
}

/// Import every JSONL session file under the session trees of `agent_dir`
/// ([`session_trees`], recursively) that the SQLite store does not hold yet;
/// `None` when nothing ran (the file store is in use, `agent_dir` is not the
/// one whose database the process opened, or every tree was imported
/// completely before). Idempotent: a file
/// already imported is left alone, so a pass that stopped part way resumes.
/// Each file is imported whole or not at all, then made read-only and never
/// written again. Slow on a large history: run it in the background, never
/// on a path a client waits on (until then, the files are read as they are).
#[must_use]
pub fn import_session_trees(agent_dir: &Path) -> Option<TranscriptImportReport> {
    let Selected::Sqlite(store, ..) = selected() else {
        return None;
    };
    database_log_of(agent_dir)?;
    import_trees(store, &session_trees(agent_dir))
}

/// [`import_session_trees`] into `store`.
fn import_trees(
    store: &SqliteTranscriptStore,
    roots: &[PathBuf],
) -> Option<TranscriptImportReport> {
    let pending: Vec<(&PathBuf, String)> = roots
        .iter()
        .map(|root| (root, format!("imported:{}", root.display())))
        .filter(|(_, marker)| store.log().meta(marker).ok().flatten().is_none())
        .collect();
    if pending.is_empty() {
        return None;
    }
    let mut report = TranscriptImportReport::default();
    for (root, marker) in pending {
        let failed_before = report.failed.len();
        let mut files = Vec::new();
        session_files(root, &mut files, &mut report.failed);
        for file in files {
            match store.import_original(&file) {
                Ok(OriginalImport::Imported(bytes)) => {
                    report.imported += 1;
                    report.bytes += bytes;
                }
                Ok(OriginalImport::AlreadyStored) => report.already_stored += 1,
                Ok(OriginalImport::NotATranscript) => {}
                Err(error) => report.failed.push((file, error.to_string())),
            }
        }
        if report.failed.len() == failed_before {
            // A root with failures keeps no marker: the next pass retries them.
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis());
            if let Err(error) = store
                .log()
                .set_meta(marker, stamp.to_string())
                .wait_blocking()
            {
                report
                    .failed
                    .push((root.clone(), format!("record the import: {error:#}")));
            }
        }
    }
    Some(report)
}

/// The `.jsonl` files under `dir`, recursively (symbolic links are not
/// followed); a directory that cannot be listed is a failure, except a root
/// that does not exist.
fn session_files(dir: &Path, files: &mut Vec<PathBuf>, failed: &mut Vec<(PathBuf, String)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            failed.push((dir.to_path_buf(), error.to_string()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failed.push((dir.to_path_buf(), error.to_string()));
                continue;
            }
        };
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => session_files(&path, files, failed),
            Ok(kind) if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") => {
                files.push(path);
            }
            Ok(_) => {}
            Err(error) => failed.push((path, error.to_string())),
        }
    }
}

/// The session trees of `agent_dir` that [`import_session_trees`] and
/// `export-jsonl --all` walk: its `sessions/` and `sessions-archive/`, and
/// the sessions dir `PRIME_AGENT_SESSION_DIR` or the global `sessionDir`
/// setting names. Transcripts elsewhere are imported on their first write.
#[must_use]
pub fn session_trees(agent_dir: &Path) -> Vec<PathBuf> {
    let mut trees = vec![
        agent_dir.join("sessions"),
        agent_dir.join("sessions-archive"),
    ];
    let overridden = pa_types::session_scope::var_os("PRIME_AGENT_SESSION_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute());
    let configured =
        crate::settings::SettingsManager::create(agent_dir, agent_dir).get_session_dir();
    for tree in overridden.into_iter().chain(configured) {
        if !trees.contains(&tree) {
            trees.push(tree);
        }
    }
    trees
}

/// Every locator the SQLite store holds (none for the file store).
///
/// # Errors
///
/// The database could not be read.
pub fn stored_locators() -> io::Result<Vec<PathBuf>> {
    match selected() {
        Selected::Files(..) => Ok(Vec::new()),
        Selected::Sqlite(store, ..) => store.locators(),
    }
}

/// Where session transcripts are kept, and every operation on them.
///
/// Implementations are shared by all sessions of the process, so every method
/// takes `&self` and must be safe to call from any thread at once. All methods
/// are synchronous and may block on storage: callers on an async runtime move
/// them to a blocking thread exactly as they would a file read. A method that
/// reports success has done all of its work; none of them defers a write past
/// its return.
///
/// "Durably" is as durable as the store keeps anything: the file store
/// fsyncs each write; the SQLite store commits it under `synchronous=NORMAL`
/// in WAL mode, which survives a crash of the process (a power loss can drop
/// the last commits, never corrupt the database).
///
/// Behaviour every implementation keeps:
///
/// - The bytes of a transcript are exactly what [`create`](Self::create) wrote
///   followed by every [`append`](Self::append), in order, except where
///   [`repair_tail`](Self::repair_tail) dropped or re-terminated a damaged
///   line. Nothing reorders, re-serializes or normalizes a line.
/// - A transcript's first line is its `session` header; the store does not
///   check this (its readers do).
/// - The [`TranscriptGeneration`] of a transcript changes whenever its bytes
///   change. Bytes once appended keep their positions until the transcript
///   is replaced ([`create`](Self::create), [`repair_tail`](Self::repair_tail),
///   [`restore`](Self::restore), [`import`](Self::import)), which also changes
///   its generation, so a cursor is good exactly while a generation check
///   passes.
/// - A read can race an append and end in a torn last line; readers skip or
///   hold back an unterminated last line, as they do today.
pub trait TranscriptStore: Send + Sync {
    /// Open the transcript at `locator` for reading, positioned at its start.
    ///
    /// # Errors
    ///
    /// `NotFound` when no transcript is there; any other storage error.
    fn open(&self, locator: &Path) -> io::Result<Box<dyn TranscriptReader>>;

    /// The transcript's exact bytes (the export: the same bytes a JSONL file
    /// of the session holds).
    ///
    /// # Errors
    ///
    /// `NotFound` when no transcript is there; any other storage error.
    fn read(&self, locator: &Path) -> io::Result<Vec<u8>>;

    /// What the store knows about the transcript without reading it.
    ///
    /// # Errors
    ///
    /// `NotFound` when nothing is there; any other storage error.
    fn stat(&self, locator: &Path) -> io::Result<TranscriptStat>;

    /// Whether anything is at `locator` ([`stat`](Self::stat) succeeds).
    fn exists(&self, locator: &Path) -> bool {
        self.stat(locator).is_ok()
    }

    /// The transcripts and the subdirectories directly inside `dir`. A
    /// transcript is a `.jsonl` locator; the listing is in no particular
    /// order.
    ///
    /// # Errors
    ///
    /// `dir` cannot be listed (including when it does not exist).
    fn list(&self, dir: &Path) -> io::Result<TranscriptListing>;

    /// Create or replace the transcript at `locator` with what `write` writes,
    /// atomically and durably: until this returns `Ok`, a reader sees the
    /// previous transcript (or none), and after, the new one, which a crash
    /// cannot lose. The containing directory is created when missing. A
    /// failed `write` leaves the previous transcript untouched. A new
    /// transcript is private to the user.
    ///
    /// # Errors
    ///
    /// The error `write` returned, or the storage error that stopped the
    /// replace.
    fn create(
        &self,
        locator: &Path,
        write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()>;

    /// Append `bytes` (whole JSONL lines) to the existing transcript at
    /// `locator`, durably: when this returns `Ok`, a crash cannot lose them.
    /// When the transcript's last line is not newline-terminated (a torn
    /// write), one newline goes first, so the appended lines never merge with
    /// it. Appending no bytes still makes everything before it durable.
    ///
    /// Returns the transcript's generation right after the append.
    ///
    /// # Errors
    ///
    /// `NotFound` when there is no transcript to append to; any storage error.
    fn append(&self, locator: &Path, bytes: &[u8]) -> io::Result<TranscriptGeneration>;

    /// Open the transcript at `locator` to finish a row whose append was cut
    /// short: the handle reads like [`open`](Self::open)'s, and what it
    /// writes goes to the end as-is (no newline guard: the caller is
    /// completing a line it proved is its own).
    ///
    /// # Errors
    ///
    /// `NotFound` when no transcript is there; any other storage error.
    fn open_for_completion(&self, locator: &Path) -> io::Result<Box<dyn TranscriptCompletion>>;

    /// Make everything appended to the transcript so far durable.
    ///
    /// # Errors
    ///
    /// `NotFound` when no transcript is there; any other storage error.
    fn sync(&self, locator: &Path) -> io::Result<()>;

    /// Repair crash damage at the transcript's end once, before it is
    /// resumed: a zero-filled or unterminated tail and an unparseable last
    /// line. A transcript without a `session` header, or one that is not
    /// damaged, is left as it is. Best effort: a repair that cannot complete
    /// leaves the transcript as it was.
    fn repair_tail(&self, locator: &Path);

    /// Bring a transcript in from outside the store: `source` is a JSONL
    /// session file, copied to `locator` as it is (replacing what was there).
    ///
    /// # Errors
    ///
    /// `source` cannot be read or the transcript cannot be written.
    fn import(&self, source: &Path, locator: &Path) -> io::Result<()>;

    /// Move the archived transcript at `archived` to `locator`, keeping its
    /// bytes and modification time. Never replaces anything already at
    /// `locator` (`AlreadyExists`); the archived copy is removed only once
    /// the transcript is durably at `locator`.
    ///
    /// # Errors
    ///
    /// The step that failed, with its storage error.
    fn restore(&self, archived: &Path, locator: &Path) -> io::Result<()>;

    /// Delete the transcript at `locator` (to the user's trash when the
    /// platform offers one), reporting how.
    ///
    /// # Errors
    ///
    /// The transcript could not be removed.
    fn remove(&self, locator: &Path) -> io::Result<TranscriptRemoval>;
}

/// An open transcript, read through its bytes. Positions ([`Seek`]) are byte
/// offsets into the bytes [`TranscriptStore::read`] returns; the window and
/// session-info caches keep them as their resume cursors, and only a reader of
/// the same generation may use one.
pub trait TranscriptReader: Read + Seek + Send {
    /// The transcript as it is now, seen through this handle (its generation
    /// may have moved on since the open; compare it with the store's
    /// [`TranscriptStore::stat`] to prove nothing changed in between).
    ///
    /// # Errors
    ///
    /// The storage error that stopped the query.
    fn stat(&self) -> io::Result<TranscriptStat>;
}

/// A transcript opened by [`TranscriptStore::open_for_completion`]: it reads
/// like a [`TranscriptReader`], and its writes go to the end as-is.
pub trait TranscriptCompletion: TranscriptReader + Write {
    /// Make what this handle wrote durable.
    ///
    /// # Errors
    ///
    /// The storage error that stopped the sync.
    fn sync(&self) -> io::Result<()>;
}

/// The version of a transcript's bytes: equal generations mean equal bytes.
/// The caches store it in their sidecars to prove a snapshot still describes
/// the transcript. For the file store its fields are the file's length,
/// device, inode and change times; the SQLite store mints them from the
/// database's identity, the transcript's row id, length and change count.
/// Outside this module it is opaque.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptGeneration {
    len: u64,
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}

impl TranscriptGeneration {
    /// The transcript's length in bytes at this generation.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the transcript is empty at this generation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `other` is a later generation of the same transcript, so bytes
    /// up to this generation's length may still be what they were (a
    /// replaced transcript is never the same). Without a stable identity
    /// (non-unix file stores) this is always false.
    #[must_use]
    pub fn same_transcript(&self, other: &Self) -> bool {
        // No dev/ino from std on other platforms: an mtime identity would
        // certify an in-place rewrite as a resume.
        cfg!(unix) && self.dev == other.dev && self.ino == other.ino
    }
}

/// A transcript's identity apart from its bytes: equal while the same
/// transcript is at its locator, different once it was replaced. On unix the
/// file's device and inode; on Windows its creation time, a limited anchor
/// (not a unique file id), which the owned-row checks of its one user (the
/// child-usage recovery) make up for. The SQLite store's is the database's
/// identity and the transcript's row id, on every platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptIdentity(u64, u64);

/// What [`TranscriptStore::stat`] and [`TranscriptReader::stat`] report.
#[derive(Clone, Copy, Debug)]
pub struct TranscriptStat {
    /// The version of the bytes.
    pub generation: TranscriptGeneration,
    /// The identity the bytes are kept under.
    pub identity: TranscriptIdentity,
    /// When the bytes last changed, when the store knows.
    pub modified: Option<SystemTime>,
    /// When the transcript was created, when the store knows.
    pub created: Option<SystemTime>,
    /// Whether a transcript is there (for the file store: a regular file,
    /// not a directory, FIFO or device, which a read could block on).
    pub is_transcript: bool,
}

impl TranscriptStat {
    /// The transcript's length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.generation.len
    }

    /// Whether the transcript is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.generation.len == 0
    }
}

/// What [`TranscriptStore::list`] found in a directory.
#[derive(Debug, Default)]
pub struct TranscriptListing {
    /// The transcripts.
    pub transcripts: Vec<ListedTranscript>,
    /// The subdirectories (child sessions and other per-session trees).
    pub directories: Vec<PathBuf>,
}

/// One transcript in a [`TranscriptListing`].
#[derive(Debug)]
pub struct ListedTranscript {
    /// Its locator.
    pub locator: PathBuf,
    /// The listing itself saw a transcript (for the file store: the
    /// directory entry is a regular file, not a link or another node).
    pub listed_as_transcript: bool,
}

/// How [`TranscriptStore::remove`] removed a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptRemoval {
    /// Moved to the user's trash.
    Trash,
    /// Deleted outright.
    Unlink,
}

/// The transcript's bytes as text.
///
/// # Errors
///
/// The store's read error, or `InvalidData` when the bytes are not UTF-8.
pub fn read_to_string(locator: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(store().read(locator.as_ref())?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Create or replace the transcript at `locator` with `bytes`
/// ([`TranscriptStore::create`]).
///
/// # Errors
///
/// The store's create error.
pub fn write(locator: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    store().create(locator.as_ref(), &mut |out| out.write_all(bytes.as_ref()))
}

/// Read the first line from `reader` when it ends within `max_bytes` bytes.
/// `None` when no line ends within the bound: the caller judges with a full
/// read, never on truncated bytes. A final line without a trailing newline
/// still counts; one `\r\n` line return is dropped, like `str::lines`.
pub fn read_first_line_bounded(reader: &mut dyn Read, max_bytes: usize) -> Option<Vec<u8>> {
    // One byte over the cap separates a line that fits (judgeable) from an
    // over-long one: a newline at index `max_bytes` still bounds a full line.
    let mut buf = vec![0u8; max_bytes + 1];
    let mut filled = 0;
    while filled < buf.len() {
        let read = reader.read(&mut buf[filled..]).ok()?;
        if read == 0 {
            return (filled > 0).then(|| strip_line_return(&buf[..filled]));
        }
        if let Some(at) = buf[filled..filled + read]
            .iter()
            .position(|&byte| byte == b'\n')
        {
            if filled + at > max_bytes {
                return None;
            }
            return Some(strip_line_return(&buf[..filled + at]));
        }
        filled += read;
    }
    None
}

/// Drop one `\r\n` line return off the line's own bytes, like `str::lines`.
fn strip_line_return(line: &[u8]) -> Vec<u8> {
    let mut line = line.to_vec();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    line
}

#[cfg(test)]
mod tests;
