//! Session transcripts, kept line for line.
//!
//! A session's JSONL file is stored as its lines, each with the `type`, `id`
//! and `parentId` it names (none for a line that is not a JSON object, such as
//! a torn last write, which is kept as it is so nothing is lost). The first
//! line is the session header (`"type": "session"`). Exporting writes the
//! lines back in order, with the file's final newline, so an imported file
//! comes back as the same bytes.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Log, Receipt};

/// A stored session, as a listing shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub cwd: Option<String>,
    /// The header's `timestamp`.
    pub created: Option<String>,
    /// Unix milliseconds of the last change (an import's file time).
    pub updated: u64,
    /// The JSONL file it was imported from.
    pub imported_from: Option<String>,
    pub entries: u64,
}

/// One stored line of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRow {
    /// The line's place in the session: the header is 0.
    pub seq: u64,
    pub entry_id: Option<String>,
    pub parent_id: Option<String>,
    /// The line's `type`; `None` when the line is not a JSON object.
    pub kind: Option<String>,
    pub line: String,
}

/// What importing one session file did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported {
        session: String,
        entries: u64,
    },
    /// The file is unchanged since it was imported.
    Unchanged {
        session: String,
    },
    /// The file changed since it was imported; its lines replaced the old ones.
    Replaced {
        session: String,
        entries: u64,
    },
    /// Not imported, and why; the file is left as it is.
    Skipped {
        reason: String,
    },
}

/// The fields of `line` a stored entry indexes: its `type`, `id` and `parentId`.
fn indexed(line: &str) -> (Option<String>, Option<String>, Option<String>) {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) else {
        return (None, None, None);
    };
    let field = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
    (field("type"), field("id"), field("parentId"))
}

fn insert_lines(connection: &Connection, session: &str, lines: &[String]) -> Result<u64> {
    let mut statement = connection.prepare_cached(
        "INSERT INTO entries (session, seq, entry_id, parent_id, kind, line) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (seq, line) in lines.iter().enumerate() {
        let (kind, entry_id, parent_id) = indexed(line);
        statement.execute(params![
            session,
            i64::try_from(seq)?,
            entry_id,
            parent_id,
            kind,
            line
        ])?;
    }
    Ok(u64::try_from(lines.len())?)
}

/// A session file read and checked, ready to store.
struct SessionFile {
    source: String,
    sha256: String,
    session: String,
    cwd: Option<String>,
    created: Option<String>,
    /// The file's modification time, Unix milliseconds.
    updated: i64,
    trailing_newline: bool,
    lines: Vec<String>,
}

impl SessionFile {
    /// The file at `path` with its lines, or why it is not a session file.
    fn read(path: &Path) -> Result<std::result::Result<Self, String>> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let modified = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .with_context(|| format!("read {}", path.display()))?;
        let source = path.to_string_lossy().into_owned();
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(Err(format!("{source} is not UTF-8")));
        };
        let trailing_newline = text.ends_with('\n');
        let lines: Vec<String> = text
            .strip_suffix('\n')
            .unwrap_or(&text)
            .split('\n')
            .map(str::to_string)
            .collect();
        let (Some(kind), Some(session), _) = indexed(&lines[0]) else {
            return Ok(Err(format!(
                "{source} does not start with a session header"
            )));
        };
        if kind != "session" {
            return Ok(Err(format!(
                "{source} does not start with a session header"
            )));
        }
        let header: Value = serde_json::from_str(&lines[0])?;
        let field = |key: &str| header.get(key).and_then(Value::as_str).map(str::to_string);
        let updated = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
            });
        Ok(Ok(SessionFile {
            cwd: field("cwd"),
            created: field("timestamp"),
            source,
            sha256,
            session,
            updated,
            trailing_newline,
            lines,
        }))
    }

    /// Store the file as a new session, or in place of its own earlier import.
    fn store(self, connection: &Connection) -> Result<ImportOutcome> {
        let SessionFile {
            source,
            sha256,
            session,
            cwd,
            created,
            updated,
            trailing_newline,
            lines,
        } = self;
        let by_source: Option<(String, String)> = connection
            .query_row(
                "SELECT id, source_sha256 FROM sessions WHERE imported_from = ?1",
                params![source],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match by_source {
            Some((stored, stored_sha)) if stored == session && stored_sha == sha256 => {
                return Ok(ImportOutcome::Unchanged { session });
            }
            Some((stored, _)) if stored == session => {
                connection.execute("DELETE FROM entries WHERE session = ?1", params![session])?;
                let entries = insert_lines(connection, &session, &lines)?;
                connection.execute(
                    "UPDATE sessions SET cwd = ?2, created = ?3, updated = ?4, \
                     trailing_newline = ?5, source_sha256 = ?6 WHERE id = ?1",
                    params![session, cwd, created, updated, trailing_newline, sha256],
                )?;
                return Ok(ImportOutcome::Replaced { session, entries });
            }
            Some((stored, _)) => {
                return Ok(ImportOutcome::Skipped {
                    reason: format!(
                        "{source} now holds session {session}, not the {stored} it was imported \
                         with"
                    ),
                });
            }
            None => {}
        }
        let elsewhere: Option<Option<String>> = connection
            .query_row(
                "SELECT imported_from FROM sessions WHERE id = ?1",
                params![session],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(elsewhere) = elsewhere {
            return Ok(ImportOutcome::Skipped {
                reason: format!(
                    "session {session} of {source} is already stored{}",
                    elsewhere.map_or_default(|other| format!(", from {other}"))
                ),
            });
        }
        connection.execute(
            "INSERT INTO sessions (id, cwd, created, updated, trailing_newline, imported_from, \
             source_sha256) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session,
                cwd,
                created,
                updated,
                trailing_newline,
                source,
                sha256
            ],
        )?;
        let entries = insert_lines(connection, &session, &lines)?;
        Ok(ImportOutcome::Imported { session, entries })
    }
}

impl Log {
    /// Import the JSONL session file at `path`: a new session, nothing when
    /// the file is unchanged since its import, or the file's new lines in
    /// place of the old. A file that is not UTF-8, has no session header, or
    /// whose session id another file already brought is skipped, whole.
    ///
    /// The receipt carries an error when the file cannot be read or the
    /// database write failed.
    pub fn import_session_file(&self, path: &Path) -> Receipt<ImportOutcome> {
        match SessionFile::read(path) {
            Ok(Ok(file)) => self.writer.submit(move |connection| file.store(connection)),
            Ok(Err(reason)) => Receipt::ready(Ok(ImportOutcome::Skipped { reason })),
            Err(error) => Receipt::ready(Err(error)),
        }
    }

    /// Append one line to a stored session; its sequence number.
    pub fn append_entry(
        &self,
        session: impl Into<String>,
        line: impl Into<String>,
    ) -> Receipt<u64> {
        let (session, line) = (session.into(), line.into());
        self.writer.submit(move |connection| {
            let next: i64 = connection
                .query_row(
                    "SELECT COALESCE(MAX(seq) + 1, 0) FROM entries WHERE session = ?1",
                    params![session],
                    |row| row.get(0),
                )
                .with_context(|| format!("find the end of session {session}"))?;
            let (kind, entry_id, parent_id) = indexed(&line);
            connection
                .execute(
                    "INSERT INTO entries (session, seq, entry_id, parent_id, kind, line) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![session, next, entry_id, parent_id, kind, line],
                )
                .with_context(|| format!("append to session {session}"))?;
            // An appended line ends with a newline, as a JSONL append does.
            connection.execute(
                "UPDATE sessions SET updated = ?2, trailing_newline = 1 WHERE id = ?1",
                params![session, i64::try_from(crate::now_ms())?],
            )?;
            Ok(u64::try_from(next)?)
        })
    }

    /// Write session `session` as its JSONL file: its lines in order, and its
    /// final newline.
    ///
    /// # Errors
    ///
    /// No session has that id, the database could not be read, or `out`
    /// failed.
    pub fn export_session(&self, session: &str, out: &mut dyn Write) -> Result<()> {
        self.read(|connection| {
            let trailing_newline: bool = connection
                .query_row(
                    "SELECT trailing_newline FROM sessions WHERE id = ?1",
                    params![session],
                    |row| row.get(0),
                )
                .optional()?
                .with_context(|| format!("no stored session is {session}"))?;
            let mut statement = connection
                .prepare_cached("SELECT line FROM entries WHERE session = ?1 ORDER BY seq")?;
            let mut rows = statement.query(params![session])?;
            let mut first = true;
            while let Some(row) = rows.next()? {
                if !first {
                    out.write_all(b"\n")?;
                }
                first = false;
                out.write_all(row.get_ref(0)?.as_str()?.as_bytes())?;
            }
            if trailing_newline {
                out.write_all(b"\n")?;
            }
            Ok(())
        })
    }

    /// Every stored session, most recently changed first.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn sessions(&self) -> Result<Vec<SessionRow>> {
        self.read(|connection| {
            let mut statement = connection.prepare_cached(
                "SELECT s.id, s.cwd, s.created, s.updated, s.imported_from, \
                 (SELECT COUNT(*) FROM entries e WHERE e.session = s.id) \
                 FROM sessions s ORDER BY s.updated DESC, s.id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?;
            rows.map(|row| {
                let (id, cwd, created, updated, imported_from, entries) = row?;
                Ok(SessionRow {
                    id,
                    cwd,
                    created,
                    updated: u64::try_from(updated)?,
                    imported_from,
                    entries: u64::try_from(entries)?,
                })
            })
            .collect()
        })
    }

    /// A session's lines after `after` (from the start when `None`), at most `limit`.
    ///
    /// # Errors
    ///
    /// The database could not be read.
    pub fn entries(
        &self,
        session: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EntryRow>> {
        self.read(|connection| {
            let mut statement = connection.prepare_cached(
                "SELECT seq, entry_id, parent_id, kind, line FROM entries \
                 WHERE session = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
            )?;
            let after = after.map_or(Ok(-1), i64::try_from)?;
            let rows = statement.query_map(
                params![session, after, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
            rows.map(|row| {
                let (seq, entry_id, parent_id, kind, line) = row?;
                Ok(EntryRow {
                    seq: u64::try_from(seq)?,
                    entry_id,
                    parent_id,
                    kind,
                    line,
                })
            })
            .collect()
        })
    }
}
