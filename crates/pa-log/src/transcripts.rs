//! Transcripts addressed by locator, byte for byte: what the transcript store
//! (`pa_core::session::store`) keeps.
//!
//! A transcript is the exact bytes of a JSONL session file, stored as its
//! lines in order (each with its newline, the last one possibly without), so
//! the bytes come back unchanged whatever they hold: a torn last write, a
//! line that is not JSON, bytes that are not UTF-8. Each line records where
//! it starts in those bytes, so a read at any byte position goes straight to
//! the line holding it instead of reading the transcript from its start.
//!
//! A transcript is found by its locator (the path the rest of the system
//! names the session by), which the caller passes canonical. Its row id is
//! its identity: appends keep it, and every replace gives the transcript a
//! new one (ids are never reused), so the id and the byte length together
//! name the bytes exactly.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Deserialize;

use crate::{Log, Receipt};

/// A stored transcript, without its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRow {
    /// Its identity: kept by appends, new after every replace.
    pub id: i64,
    pub locator: String,
    /// The directory listing it ([`Log::transcripts_in`]).
    pub dir: String,
    /// The `id` its first line names, when that line is a session header.
    pub session: Option<String>,
    /// Its length in bytes.
    pub len: u64,
    /// How many lines it holds (an unterminated last line counts).
    pub lines: u64,
    /// Counts the changes to its bytes since it was stored.
    pub version: u64,
    /// Unix nanoseconds.
    pub created_ns: i64,
    /// Unix nanoseconds of the last change.
    pub modified_ns: i64,
    /// The file it was imported from, if it was.
    pub source: Option<String>,
}

/// Contiguous bytes of a transcript, from `start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptChunk {
    pub start: u64,
    pub bytes: Vec<u8>,
}

/// A transcript to store whole.
#[derive(Debug, Clone)]
pub struct NewTranscript {
    pub locator: String,
    pub dir: String,
    pub bytes: Vec<u8>,
    /// Unix nanoseconds.
    pub created_ns: i64,
    /// Unix nanoseconds.
    pub modified_ns: i64,
    /// The file it is imported from, if it is.
    pub source: Option<String>,
}

/// What [`Log::put_transcript`] does when the locator already holds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutMode {
    /// Replace it (the new transcript gets a new id).
    Replace,
    /// Keep it and store nothing (an import that another got to first).
    IfAbsent,
}

/// How [`Log::append_transcript`] treats a transcript whose last line is not
/// newline-terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendMode {
    /// End that line first, so the appended bytes start a line of their own
    /// (a torn write never merges with the next line).
    NewLine,
    /// Append as they are (the caller is finishing that line).
    AsIs,
}

/// What [`Log::move_transcript`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved(TranscriptRow),
    /// Nothing is stored at the source locator.
    Missing,
    /// The target locator already holds a transcript; nothing moved.
    Occupied,
}

/// The fields of a line a stored line indexes.
#[derive(Default, Deserialize)]
struct Indexed {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    #[serde(rename = "parentId")]
    parent_id: Option<String>,
}

impl Indexed {
    /// A terminated line's fields; nothing for an unterminated line or one
    /// that is not a JSON object with string fields.
    fn of(line: &[u8]) -> Self {
        match line.strip_suffix(b"\n") {
            Some(content) => serde_json::from_slice(content).unwrap_or_default(),
            None => Self::default(),
        }
    }
}

const ROW_COLUMNS: &str = "id, locator, dir, session, len, lines, version, created_ns, \
                           modified_ns, source";

fn row(row: &Row<'_>) -> rusqlite::Result<TranscriptRow> {
    let unsigned = |index: usize| -> rusqlite::Result<u64> {
        let value: i64 = row.get(index)?;
        u64::try_from(value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Integer,
                error.into(),
            )
        })
    };
    Ok(TranscriptRow {
        id: row.get(0)?,
        locator: row.get(1)?,
        dir: row.get(2)?,
        session: row.get(3)?,
        len: unsigned(4)?,
        lines: unsigned(5)?,
        version: unsigned(6)?,
        created_ns: row.get(7)?,
        modified_ns: row.get(8)?,
        source: row.get(9)?,
    })
}

fn by_locator(connection: &Connection, locator: &str) -> Result<Option<TranscriptRow>> {
    Ok(connection
        .prepare_cached(&format!(
            "SELECT {ROW_COLUMNS} FROM transcripts WHERE locator = ?1"
        ))?
        .query_row(params![locator], row)
        .optional()?)
}

fn by_id(connection: &Connection, id: i64) -> Result<Option<TranscriptRow>> {
    Ok(connection
        .prepare_cached(&format!(
            "SELECT {ROW_COLUMNS} FROM transcripts WHERE id = ?1"
        ))?
        .query_row(params![id], row)
        .optional()?)
}

/// Split `bytes` into lines, each with its newline (the last may have none).
fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|byte| *byte == b'\n')
}

/// Store `bytes` as the lines of transcript `id` from line `seq` and byte
/// `start` on; the number of lines stored.
fn insert_lines(
    connection: &Connection,
    id: i64,
    mut seq: i64,
    mut start: i64,
    bytes: &[u8],
) -> Result<i64> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO transcript_lines (transcript, seq, start, kind, entry_id, parent_id, bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut count = 0;
    for line in lines(bytes) {
        let indexed = Indexed::of(line);
        statement.execute(params![
            id,
            seq,
            start,
            indexed.kind,
            indexed.id,
            indexed.parent_id,
            line
        ])?;
        seq += 1;
        start += i64::try_from(line.len())?;
        count += 1;
    }
    Ok(count)
}

/// The session id a transcript's first line names, when it is a header.
fn header_session(bytes: &[u8]) -> Option<String> {
    let first = lines(bytes).next()?;
    let indexed = Indexed::of(first);
    (indexed.kind.as_deref() == Some("session"))
        .then_some(indexed.id)
        .flatten()
}

impl Log {
    /// The database's own identity: a random number minted when it was
    /// created, so ids from two databases never compare equal.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn store_id(&self) -> Result<u64> {
        self.read(|connection| {
            let id: i64 = connection.query_row(
                "SELECT value FROM store_meta WHERE key = 'store_id'",
                [],
                |row| row.get(0),
            )?;
            Ok(id.unsigned_abs())
        })
    }

    /// A value the store keeps about itself (such as an import marker).
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT CAST(value AS TEXT) FROM store_meta WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    /// Set a value [`Log::meta`] reads.
    pub fn set_meta(&self, key: impl Into<String>, value: impl Into<String>) -> Receipt<()> {
        let (key, value) = (key.into(), value.into());
        self.writer.submit(move |connection| {
            connection.execute(
                "INSERT INTO store_meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    /// The transcript at `locator`.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn transcript(&self, locator: &str) -> Result<Option<TranscriptRow>> {
        self.read(|connection| by_locator(connection, locator))
    }

    /// The transcript with identity `id`, while it has it.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn transcript_by_id(&self, id: i64) -> Result<Option<TranscriptRow>> {
        self.read(|connection| by_id(connection, id))
    }

    /// The transcripts listed in `dir`.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn transcripts_in(&self, dir: &str) -> Result<Vec<TranscriptRow>> {
        self.read(|connection| {
            let mut statement = connection.prepare_cached(&format!(
                "SELECT {ROW_COLUMNS} FROM transcripts WHERE dir = ?1 ORDER BY locator"
            ))?;
            let rows = statement.query_map(params![dir], row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Every stored transcript, by locator.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn all_transcripts(&self) -> Result<Vec<TranscriptRow>> {
        self.read(|connection| {
            let mut statement = connection.prepare_cached(&format!(
                "SELECT {ROW_COLUMNS} FROM transcripts ORDER BY locator"
            ))?;
            let rows = statement.query_map([], row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Transcript `id`'s bytes from byte `from` on: whole lines from the one
    /// holding `from` (trimmed to start at `from`), until at least
    /// `at_least` bytes or the end. Empty at or past the end; `None` once
    /// nothing has identity `id` (it was replaced or removed).
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn transcript_bytes(
        &self,
        id: i64,
        from: u64,
        at_least: usize,
    ) -> Result<Option<TranscriptChunk>> {
        self.read(|connection| {
            if by_id(connection, id)?.is_none() {
                return Ok(None);
            }
            let from_signed = i64::try_from(from)?;
            let mut statement = connection.prepare_cached(
                "SELECT start, bytes FROM transcript_lines WHERE transcript = ?1 AND start >= \
                 COALESCE((SELECT MAX(start) FROM transcript_lines \
                           WHERE transcript = ?1 AND start <= ?2), 0) \
                 ORDER BY start",
            )?;
            let mut rows = statement.query(params![id, from_signed])?;
            let mut bytes = Vec::new();
            while let Some(line) = rows.next()? {
                let start = u64::try_from(line.get::<_, i64>(0)?)?;
                let content = line.get_ref(1)?.as_blob()?;
                let end = start + content.len() as u64;
                if end <= from {
                    continue;
                }
                let skip = usize::try_from(from.saturating_sub(start))?;
                bytes.extend_from_slice(&content[skip..]);
                if bytes.len() >= at_least {
                    break;
                }
            }
            Ok(Some(TranscriptChunk { start: from, bytes }))
        })
    }

    /// Store a whole transcript at its locator; the stored row, or `None`
    /// when [`PutMode::IfAbsent`] found one there already.
    pub fn put_transcript(
        &self,
        transcript: NewTranscript,
        mode: PutMode,
    ) -> Receipt<Option<TranscriptRow>> {
        self.writer.submit(move |connection| {
            let NewTranscript {
                locator,
                dir,
                bytes,
                created_ns,
                modified_ns,
                source,
            } = transcript;
            match mode {
                PutMode::Replace => {
                    connection
                        .prepare_cached("DELETE FROM transcripts WHERE locator = ?1")?
                        .execute(params![locator])?;
                }
                PutMode::IfAbsent => {
                    if by_locator(connection, &locator)?.is_some() {
                        return Ok(None);
                    }
                }
            }
            connection
                .prepare_cached(
                    "INSERT INTO transcripts (locator, dir, session, len, lines, version, \
                     created_ns, modified_ns, source) VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, ?6, ?7)",
                )?
                .execute(params![
                    locator,
                    dir,
                    header_session(&bytes),
                    i64::try_from(bytes.len())?,
                    created_ns,
                    modified_ns,
                    source
                ])
                .with_context(|| format!("store transcript {locator}"))?;
            let id = connection.last_insert_rowid();
            let count = insert_lines(connection, id, 0, 0, &bytes)?;
            connection
                .prepare_cached("UPDATE transcripts SET lines = ?2 WHERE id = ?1")?
                .execute(params![id, count])?;
            by_id(connection, id)
        })
    }

    /// Append `bytes` to the transcript at `locator`; its row after, or
    /// `None` when nothing is stored there. Appending nothing changes
    /// nothing.
    pub fn append_transcript(
        &self,
        locator: impl Into<String>,
        bytes: Vec<u8>,
        mode: AppendMode,
        modified_ns: i64,
    ) -> Receipt<Option<TranscriptRow>> {
        let locator = locator.into();
        self.writer.submit(move |connection| {
            let Some(transcript) = by_locator(connection, &locator)? else {
                return Ok(None);
            };
            if bytes.is_empty() {
                return Ok(Some(transcript));
            }
            let last: Option<(i64, Vec<u8>)> = connection
                .prepare_cached(
                    "SELECT seq, bytes FROM transcript_lines WHERE transcript = ?1 \
                     ORDER BY seq DESC LIMIT 1",
                )?
                .query_row(params![transcript.id], |line| {
                    Ok((line.get(0)?, line.get(1)?))
                })
                .optional()?;
            let mut rest: &[u8] = &bytes;
            let mut len = i64::try_from(transcript.len)?;
            let mut next_seq = 0;
            if let Some((seq, mut tail)) = last {
                next_seq = seq + 1;
                if tail.last() != Some(&b'\n') {
                    let added = match mode {
                        AppendMode::NewLine => b"\n".as_slice(),
                        AppendMode::AsIs => {
                            let end = rest
                                .iter()
                                .position(|byte| *byte == b'\n')
                                .map_or(rest.len(), |at| at + 1);
                            let (head, remainder) = rest.split_at(end);
                            rest = remainder;
                            head
                        }
                    };
                    tail.extend_from_slice(added);
                    len += i64::try_from(added.len())?;
                    let indexed = Indexed::of(&tail);
                    connection
                        .prepare_cached(
                            "UPDATE transcript_lines SET bytes = ?3, kind = ?4, entry_id = ?5, \
                             parent_id = ?6 WHERE transcript = ?1 AND seq = ?2",
                        )?
                        .execute(params![
                            transcript.id,
                            seq,
                            tail,
                            indexed.kind,
                            indexed.id,
                            indexed.parent_id
                        ])?;
                    if seq == 0 {
                        connection
                            .prepare_cached("UPDATE transcripts SET session = ?2 WHERE id = ?1")?
                            .execute(params![transcript.id, header_session(&tail)])?;
                    }
                }
            }
            let added_lines = insert_lines(connection, transcript.id, next_seq, len, rest)?;
            if next_seq == 0 {
                connection
                    .prepare_cached("UPDATE transcripts SET session = ?2 WHERE id = ?1")?
                    .execute(params![transcript.id, header_session(rest)])?;
            }
            len += i64::try_from(rest.len())?;
            connection
                .prepare_cached(
                    "UPDATE transcripts SET len = ?2, lines = lines + ?3, version = version + 1, \
                     modified_ns = ?4 WHERE id = ?1",
                )?
                .execute(params![transcript.id, len, added_lines, modified_ns])?;
            by_id(connection, transcript.id)
        })
    }

    /// Move the transcript at `from` to `to` (listed in `to_dir`), keeping
    /// its bytes and times.
    pub fn move_transcript(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        to_dir: impl Into<String>,
    ) -> Receipt<MoveOutcome> {
        let (from, to, to_dir) = (from.into(), to.into(), to_dir.into());
        self.writer.submit(move |connection| {
            if by_locator(connection, &to)?.is_some() {
                return Ok(MoveOutcome::Occupied);
            }
            let Some(transcript) = by_locator(connection, &from)? else {
                return Ok(MoveOutcome::Missing);
            };
            connection
                .prepare_cached(
                    "UPDATE transcripts SET locator = ?2, dir = ?3, version = version + 1 \
                     WHERE id = ?1",
                )?
                .execute(params![transcript.id, to, to_dir])?;
            Ok(by_id(connection, transcript.id)?.map_or(MoveOutcome::Missing, MoveOutcome::Moved))
        })
    }

    /// Delete the transcript at `locator`; whether one was there.
    pub fn remove_transcript(&self, locator: impl Into<String>) -> Receipt<bool> {
        let locator = locator.into();
        self.writer.submit(move |connection| {
            Ok(connection
                .prepare_cached("DELETE FROM transcripts WHERE locator = ?1")?
                .execute(params![locator])?
                > 0)
        })
    }
}
