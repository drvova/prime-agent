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
//! [`store`] is the process-wide store. It holds every session's transcript
//! (one directory tree today, one database later), so it is process-wide by
//! design, like the telemetry origin: no session's scope selects it, and a
//! session reaches its own transcript only by its locator.
//!
//! The file store (`FileTranscriptStore`, the only one today) keeps each
//! transcript in its own file. The window and session-info caches certify
//! their sidecars against that store's [`TranscriptGeneration`]s; they are its
//! internals and keep their formats.

use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

mod file;

use file::FileTranscriptStore;

/// The process-wide transcript store.
#[must_use]
pub fn store() -> &'static dyn TranscriptStore {
    static FILES: FileTranscriptStore = FileTranscriptStore;
    &FILES
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
/// the transcript. Its fields are the file store's (length and the file's
/// device, inode and change times); outside this module it is opaque.
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
/// child-usage recovery) make up for.
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
pub fn read_to_string(locator: &Path) -> io::Result<String> {
    String::from_utf8(store().read(locator)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
