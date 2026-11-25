//! The SQLite store: every transcript in one database (`pa_log`), byte for
//! byte, addressed by its locator.
//!
//! - **Locators.** A transcript is keyed by its canonical locator: the
//!   canonical form of the directory it names (the directory is real: create
//!   makes it, as the per-session artifact trees beside the transcript need
//!   it) joined with its file name. Any spelling of the same path reaches the
//!   same transcript, in the default sessions dir or a custom one.
//! - **Reads are byte streams.** A reader serves the transcript's exact
//!   bytes. Every stored line records where it starts, so a seek goes
//!   straight to the line that holds the position and a read fetches only
//!   the lines it covers: the windowed reverse reads of a long session read
//!   its tail, never the whole transcript.
//! - **Generations** are minted from the store: the database's identity, the
//!   transcript's row id (kept by appends, new after every replace, never
//!   reused), its byte length and its change count. Bytes only ever grow
//!   under one row id, so a generation names its bytes exactly.
//! - **Originals.** A JSONL file at a locator the database does not hold yet
//!   (a session from before the move, or one another build wrote) is read
//!   from the file as it is, so reading never waits on an import. The first
//!   write to it imports the file whole first. An imported file is left in
//!   place, made read-only and never written again: it is the rollback. The
//!   background import ([`SqliteTranscriptStore::import_original`]) brings
//!   the rest in ahead of time.
//! - **Durability.** A write returns once its transaction commits. The
//!   database runs `synchronous=NORMAL` in WAL mode: a commit survives a
//!   crash of the process, and a power loss can drop the last commits but
//!   never leaves the database inconsistent.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pa_log::{
    AppendMode, Log, MoveOutcome, NewTranscript, PutMode, TranscriptChunk, TranscriptRow,
};

use super::file::FileTranscriptStore;
use super::{
    repair, ListedTranscript, TranscriptCompletion, TranscriptGeneration, TranscriptIdentity,
    TranscriptListing, TranscriptReader, TranscriptRemoval, TranscriptStat, TranscriptStore,
};

/// How many symbolic links a locator is followed through.
const MAX_LINKS: usize = 40;

/// How much a reader fetches past the position it reads, in whole lines.
const READ_AHEAD_BYTES: usize = 64 * 1024;

/// The [`TranscriptStore`] over the SQLite database.
pub(crate) struct SqliteTranscriptStore {
    log: Arc<Log>,
    /// The database's identity, part of every generation it mints.
    store_id: u64,
    /// Reads the originals the database does not hold yet.
    files: FileTranscriptStore,
    /// One import at a time in this process, so two first writes to the
    /// same original import it once (another process is kept out by the
    /// import's own only-if-absent write).
    imports: Mutex<()>,
}

/// What [`SqliteTranscriptStore::import_original`] did with one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OriginalImport {
    /// Imported, this many bytes.
    Imported(u64),
    /// The database already holds the transcript at that locator.
    AlreadyStored,
    /// The file does not start with a session header: not a transcript.
    NotATranscript,
}

/// A locator as the database keys it.
struct Key {
    locator: String,
    dir: String,
}

fn io_error(error: &anyhow::Error) -> io::Error {
    io::Error::other(format!("{error:#}"))
}

fn ns(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_nanos()).unwrap_or(i64::MAX)
    })
}

fn time(ns: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns.unsigned_abs())
}

impl TranscriptGeneration {
    fn of_row(store_id: u64, row: &TranscriptRow) -> Self {
        Self {
            len: row.len,
            dev: store_id,
            ino: row.id.unsigned_abs(),
            mtime: i64::try_from(row.version).unwrap_or(i64::MAX),
            mtime_ns: 0,
            ctime: 0,
            ctime_ns: 0,
        }
    }
}

impl TranscriptStat {
    fn of_row(store_id: u64, row: &TranscriptRow) -> Self {
        Self {
            generation: TranscriptGeneration::of_row(store_id, row),
            identity: TranscriptIdentity(store_id, row.id.unsigned_abs()),
            modified: Some(time(row.modified_ns)),
            created: Some(time(row.created_ns)),
            is_transcript: true,
        }
    }
}

/// The canonical form of `dir`: resolved when it exists, else made absolute.
fn canonical_dir(dir: &Path) -> io::Result<PathBuf> {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    match pa_types::platform::canonicalize(dir) {
        Ok(dir) => Ok(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::path::absolute(dir),
        Err(error) => Err(error),
    }
}

fn key(locator: &Path) -> io::Result<Key> {
    // A symbolic link at the locator names the transcript at its target, as
    // it does for a file (the target need not exist as a file: the database
    // may hold it).
    let mut locator = std::borrow::Cow::Borrowed(locator);
    for _ in 0..MAX_LINKS {
        match fs::read_link(&locator) {
            Ok(target) => {
                let target = match locator.parent() {
                    Some(parent) if target.is_relative() => parent.join(target),
                    _ => target,
                };
                locator = std::borrow::Cow::Owned(target);
            }
            Err(_) => break,
        }
    }
    let name = locator.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} names no transcript", locator.display()),
        )
    })?;
    let dir = canonical_dir(locator.parent().unwrap_or(Path::new("")))?;
    Ok(Key {
        locator: dir.join(name).to_string_lossy().into_owned(),
        dir: dir.to_string_lossy().into_owned(),
    })
}

/// Make an imported original read-only: it is the rollback, never written
/// again. A failure leaves the import standing and is logged.
fn freeze_original(path: &Path) {
    let frozen = fs::metadata(path).and_then(|metadata| {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            return Ok(());
        }
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)
    });
    if let Err(error) = frozen {
        eprintln!(
            "transcript store: imported {} but could not make it read-only: {error}",
            path.display()
        );
    }
}

/// Whether `bytes` start with a `session` header line.
fn starts_with_header(bytes: &[u8]) -> bool {
    let first = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    std::str::from_utf8(first).is_ok_and(|line| {
        crate::session::parse_session_entries(line)
            .first()
            .is_some_and(|entry| matches!(entry, pa_types::session::FileEntry::Header { .. }))
    })
}

impl SqliteTranscriptStore {
    /// Open the store on the database at `path` (created when missing).
    pub(crate) fn open(path: &Path) -> anyhow::Result<Self> {
        let log = Log::open(path, pa_log::DEFAULT_EVENT_RETENTION)?;
        Ok(Self {
            store_id: log.store_id()?,
            log: Arc::new(log),
            files: FileTranscriptStore,
            imports: Mutex::new(()),
        })
    }

    /// The database.
    pub(crate) fn log(&self) -> &Log {
        &self.log
    }

    fn row(&self, key: &Key) -> io::Result<Option<TranscriptRow>> {
        self.log
            .transcript(&key.locator)
            .map_err(|error| io_error(&error))
    }

    fn reader(&self, row: TranscriptRow) -> DbReader {
        DbReader {
            log: Arc::clone(&self.log),
            store_id: self.store_id,
            position: 0,
            chunk: TranscriptChunk {
                start: 0,
                bytes: Vec::new(),
            },
            known: row,
        }
    }

    /// The transcript at `locator` in the database, importing the original
    /// file there first when the database does not hold it yet.
    fn adopt(&self, locator: &Path, key: &Key) -> io::Result<TranscriptRow> {
        if let Some(row) = self.row(key)? {
            return Ok(row);
        }
        let _one_import = self.imports.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(row) = self.row(key)? {
            return Ok(row);
        }
        let metadata = fs::metadata(locator)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a transcript file", locator.display()),
            ));
        }
        let bytes = fs::read(locator)?;
        self.put_original(locator, key, &metadata, bytes)?;
        freeze_original(locator);
        self.row(key)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} was removed while it was imported", locator.display()),
            )
        })
    }

    /// Store an original's bytes at its locator unless the database holds
    /// one there already; whether this stored it.
    fn put_original(
        &self,
        locator: &Path,
        key: &Key,
        metadata: &fs::Metadata,
        bytes: Vec<u8>,
    ) -> io::Result<bool> {
        let modified = metadata.modified().map_or(0, ns);
        let stored = self
            .log
            .put_transcript(
                NewTranscript {
                    locator: key.locator.clone(),
                    dir: key.dir.clone(),
                    bytes,
                    created_ns: metadata.created().map_or(modified, ns),
                    modified_ns: modified,
                    source: Some(locator.to_string_lossy().into_owned()),
                },
                PutMode::IfAbsent,
            )
            .wait_blocking()
            .map_err(|error| io_error(&error))?;
        Ok(stored.is_some())
    }

    /// Import the JSONL session file at `path` (its locator), whole, unless
    /// the database already holds that locator or the file is not a
    /// transcript. An imported file is made read-only and never written
    /// again.
    ///
    /// # Errors
    ///
    /// The file could not be read or the database not written; nothing of
    /// the file is stored then.
    pub(crate) fn import_original(&self, path: &Path) -> io::Result<OriginalImport> {
        let key = key(path)?;
        if self.row(&key)?.is_some() {
            return Ok(OriginalImport::AlreadyStored);
        }
        let _one_import = self.imports.lock().unwrap_or_else(PoisonError::into_inner);
        let metadata = fs::metadata(path)?;
        let bytes = fs::read(path)?;
        if !starts_with_header(&bytes) {
            return Ok(OriginalImport::NotATranscript);
        }
        let len = bytes.len() as u64;
        if !self.put_original(path, &key, &metadata, bytes)? {
            return Ok(OriginalImport::AlreadyStored);
        }
        freeze_original(path);
        Ok(OriginalImport::Imported(len))
    }

    /// Every locator the database holds.
    pub(crate) fn locators(&self) -> io::Result<Vec<PathBuf>> {
        Ok(self
            .log
            .all_transcripts()
            .map_err(|error| io_error(&error))?
            .into_iter()
            .map(|row| PathBuf::from(row.locator))
            .collect())
    }

    fn put(&self, key: &Key, bytes: Vec<u8>, created_ns: i64) -> io::Result<TranscriptRow> {
        let now = ns(SystemTime::now());
        self.log
            .put_transcript(
                NewTranscript {
                    locator: key.locator.clone(),
                    dir: key.dir.clone(),
                    bytes,
                    created_ns,
                    modified_ns: now,
                    source: None,
                },
                PutMode::Replace,
            )
            .wait_blocking()
            .map_err(|error| io_error(&error))?
            .ok_or_else(|| io::Error::other("the replaced transcript was not stored"))
    }

    fn repair_row(&self, key: &Key, row: TranscriptRow) -> io::Result<()> {
        let created = row.created_ns;
        let mut reader = self.reader(row);
        if !repair::tail_looks_damaged(&mut reader)
            || !repair::first_line_is_session_header(&mut reader)
        {
            return Ok(());
        }
        reader.seek(SeekFrom::Start(0))?;
        let mut repaired = Vec::new();
        if repair::repair_lines(&mut reader, &mut repaired)? {
            self.put(key, repaired, created)?;
        }
        Ok(())
    }
}

impl TranscriptStore for SqliteTranscriptStore {
    fn open(&self, locator: &Path) -> io::Result<Box<dyn TranscriptReader>> {
        match self.row(&key(locator)?)? {
            Some(row) => Ok(Box::new(self.reader(row))),
            None => self.files.open(locator),
        }
    }

    fn read(&self, locator: &Path) -> io::Result<Vec<u8>> {
        let key = key(locator)?;
        let Some(row) = self.row(&key)? else {
            return self.files.read(locator);
        };
        let mut reader = self.reader(row);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn stat(&self, locator: &Path) -> io::Result<TranscriptStat> {
        match self.row(&key(locator)?)? {
            Some(row) => Ok(TranscriptStat::of_row(self.store_id, &row)),
            None => self.files.stat(locator),
        }
    }

    fn list(&self, dir: &Path) -> io::Result<TranscriptListing> {
        let mut listing = self.files.list(dir)?;
        let stored = self
            .log
            .transcripts_in(&canonical_dir(dir)?.to_string_lossy())
            .map_err(|error| io_error(&error))?;
        let mut names: std::collections::HashSet<std::ffi::OsString> =
            std::collections::HashSet::new();
        for row in &stored {
            if let Some(name) = Path::new(&row.locator).file_name() {
                names.insert(name.to_os_string());
            }
        }
        for listed in &mut listing.transcripts {
            if listed
                .locator
                .file_name()
                .is_some_and(|name| names.remove(name))
            {
                listed.listed_as_transcript = true;
            }
        }
        listing
            .transcripts
            .extend(names.into_iter().map(|name| ListedTranscript {
                locator: dir.join(name),
                listed_as_transcript: true,
            }));
        Ok(listing)
    }

    fn create(
        &self,
        locator: &Path,
        write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        if let Some(parent) = locator
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        refuse_directory(locator)?;
        let key = key(locator)?;
        let mut bytes = Vec::new();
        write(&mut bytes)?;
        self.put(&key, bytes, ns(SystemTime::now())).map(drop)
    }

    fn append(&self, locator: &Path, bytes: &[u8]) -> io::Result<TranscriptGeneration> {
        let key = key(locator)?;
        self.adopt(locator, &key)?;
        let row = self
            .log
            .append_transcript(
                key.locator,
                bytes.to_vec(),
                AppendMode::NewLine,
                ns(SystemTime::now()),
            )
            .wait_blocking()
            .map_err(|error| io_error(&error))?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} was removed", locator.display()),
                )
            })?;
        Ok(TranscriptGeneration::of_row(self.store_id, &row))
    }

    fn open_for_completion(&self, locator: &Path) -> io::Result<Box<dyn TranscriptCompletion>> {
        let key = key(locator)?;
        let row = self.adopt(locator, &key)?;
        Ok(Box::new(DbCompletion {
            reader: self.reader(row),
            locator: key.locator,
        }))
    }

    fn sync(&self, locator: &Path) -> io::Result<()> {
        // Every write returned after its commit: nothing is pending.
        match self.row(&key(locator)?)? {
            Some(_) => Ok(()),
            None => self.files.sync(locator),
        }
    }

    fn repair_tail(&self, locator: &Path) {
        // Best effort by contract: a repair that cannot complete leaves the
        // transcript as it was.
        let _ = (|| -> io::Result<()> {
            let key = key(locator)?;
            let row = if let Some(row) = self.row(&key)? {
                row
            } else {
                // An original is imported only when it needs the repair.
                let mut original = self.files.open(locator)?;
                if !repair::tail_looks_damaged(&mut *original)
                    || !repair::first_line_is_session_header(&mut *original)
                {
                    return Ok(());
                }
                drop(original);
                self.adopt(locator, &key)?
            };
            self.repair_row(&key, row)
        })();
    }

    fn import(&self, source: &Path, locator: &Path) -> io::Result<()> {
        let bytes = self.read(source)?;
        if let Some(parent) = locator
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        refuse_directory(locator)?;
        self.put(&key(locator)?, bytes, ns(SystemTime::now()))
            .map(drop)
    }

    fn restore(&self, archived: &Path, locator: &Path) -> io::Result<()> {
        if let Some(parent) = locator
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let to = key(locator)?;
        let occupied = || {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("publish restored session: {} exists", locator.display()),
            )
        };
        if self.row(&to)?.is_some() || fs::symlink_metadata(locator).is_ok() {
            return Err(occupied());
        }
        let from = key(archived)?;
        if self.row(&from)?.is_some() {
            match self
                .log
                .move_transcript(from.locator, to.locator, to.dir)
                .wait_blocking()
                .map_err(|error| io_error(&error))?
            {
                MoveOutcome::Moved(_) => {}
                MoveOutcome::Occupied => return Err(occupied()),
                MoveOutcome::Missing => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{} was removed", archived.display()),
                    ))
                }
            }
            // The archived original it was imported from goes too, or the
            // archive would list it again.
            if fs::symlink_metadata(archived).is_err() {
                return Ok(());
            }
        } else {
            let metadata = fs::metadata(archived)?;
            let bytes = fs::read(archived)?;
            let modified = metadata.modified().map_or(0, ns);
            let stored = self
                .log
                .put_transcript(
                    NewTranscript {
                        locator: to.locator,
                        dir: to.dir,
                        bytes,
                        created_ns: ns(SystemTime::now()),
                        modified_ns: modified,
                        source: None,
                    },
                    PutMode::IfAbsent,
                )
                .wait_blocking()
                .map_err(|error| io_error(&error))?;
            if stored.is_none() {
                return Err(occupied());
            }
        }
        writable(archived);
        fs::remove_file(archived).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("unlink restored archive {}: {error}", archived.display()),
            )
        })
    }

    fn remove(&self, locator: &Path) -> io::Result<TranscriptRemoval> {
        // A link is removed alone, as a file store removes it: the
        // transcript it names stays.
        if fs::symlink_metadata(locator).is_ok_and(|metadata| metadata.is_symlink()) {
            return self.files.remove(locator);
        }
        let key = key(locator)?;
        let stored = self
            .log
            .remove_transcript(key.locator)
            .wait_blocking()
            .map_err(|error| io_error(&error))?;
        // The original goes too: left behind, it would be read again.
        if fs::symlink_metadata(locator).is_ok() {
            writable(locator);
            return self.files.remove(locator);
        }
        if stored {
            Ok(TranscriptRemoval::Unlink)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no transcript at {}", locator.display()),
            ))
        }
    }
}

/// A directory at a locator is no transcript, and nothing replaces it (as a
/// file could not be renamed over it).
fn refuse_directory(locator: &Path) -> io::Result<()> {
    if fs::symlink_metadata(locator).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            format!("{} is a directory", locator.display()),
        ));
    }
    Ok(())
}

/// Let a read-only original be deleted where the platform refuses to delete
/// a read-only file (Windows); elsewhere deleting needs no write permission.
fn writable(path: &Path) {
    #[cfg(windows)]
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        // Windows: clears the read-only attribute only
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
    #[cfg(not(windows))]
    let _ = path;
}

/// A transcript in the database, read through its bytes.
struct DbReader {
    log: Arc<Log>,
    store_id: u64,
    position: u64,
    /// The bytes fetched last.
    chunk: TranscriptChunk,
    /// The transcript as this reader last saw it.
    known: TranscriptRow,
}

impl DbReader {
    fn refresh(&mut self) -> io::Result<&TranscriptRow> {
        if let Some(row) = self
            .log
            .transcript_by_id(self.known.id)
            .map_err(|error| io_error(&error))?
        {
            self.known = row;
        }
        Ok(&self.known)
    }
}

impl Read for DbReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let chunk_end = self.chunk.start + self.chunk.bytes.len() as u64;
        if self.position < self.chunk.start || self.position >= chunk_end {
            match self
                .log
                .transcript_bytes(
                    self.known.id,
                    self.position,
                    READ_AHEAD_BYTES.max(buf.len()),
                )
                .map_err(|error| io_error(&error))?
            {
                Some(chunk) => self.chunk = chunk,
                // Replaced or removed since the open: nothing more to read.
                None => return Ok(0),
            }
        }
        let offset = usize::try_from(self.position - self.chunk.start)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let available = self.chunk.bytes.get(offset..).unwrap_or_default();
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for DbReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.refresh()?.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        let target = target.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )
        })?;
        self.position = target;
        Ok(target)
    }
}

impl TranscriptReader for DbReader {
    fn stat(&self) -> io::Result<TranscriptStat> {
        let row = self
            .log
            .transcript_by_id(self.known.id)
            .map_err(|error| io_error(&error))?;
        // Gone (replaced or removed): what this handle last saw, which no
        // later generation equals.
        Ok(TranscriptStat::of_row(
            self.store_id,
            row.as_ref().unwrap_or(&self.known),
        ))
    }
}

/// A transcript opened to finish its last line: reads like [`DbReader`],
/// and writes go to the end as they are.
struct DbCompletion {
    reader: DbReader,
    locator: String,
}

impl Read for DbCompletion {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Seek for DbCompletion {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.reader.seek(position)
    }
}

impl Write for DbCompletion {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let row = self
            .reader
            .log
            .append_transcript(
                self.locator.clone(),
                buf.to_vec(),
                AppendMode::AsIs,
                ns(SystemTime::now()),
            )
            .wait_blocking()
            .map_err(|error| io_error(&error))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the transcript was removed"))?;
        self.reader.known = row;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TranscriptReader for DbCompletion {
    fn stat(&self) -> io::Result<TranscriptStat> {
        self.reader.stat()
    }
}

impl TranscriptCompletion for DbCompletion {
    fn sync(&self) -> io::Result<()> {
        // Each write returned after its commit.
        Ok(())
    }
}
