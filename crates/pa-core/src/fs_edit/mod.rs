//! Hashline editing for the kernel: `fs.read` and `fs.edit` (the REPL's
//! `rlm.fs` module), served by `fs.read`/`fs.edit` host requests.
//!
//! `fs.read` tags every line with an anchor, `LINE:abc` (the 1-based line
//! number and three letters of a content hash), and renders `12:abc|text`.
//! `fs.edit` takes edits that point at anchors instead of retyping the old
//! text: replace a line or a range between two anchors, delete, insert
//! before/after an anchor, or append. Every anchor must still match the file
//! as it is now, or the whole patch is rejected with the stale anchors and
//! the current lines around them, and nothing is written. Adapted from
//! oh-my-pi's hashline edit mode (MIT), which tags lines `LINE:HASH|text`
//! and rejects edits whose hashes no longer match.
//!
//! The hash covers the line and both neighbours (SHA-256 of
//! `previous \n line \n next`, a sentinel past either end), reduced to three
//! letters `a`-`z` (26³ = 17,576 values, 14.1 bits). With the line number
//! in the anchor, an anchor survives only while its line sits at the same
//! number between the same neighbours; a stale anchor passes only on a hash
//! collision (1 in 17,576 per anchor). Lowercase letters cost fewer tokens
//! than hex for more bits.

mod document;
mod host;
mod patch;
mod seen;
#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use document::{Document, Encoding, LineEnds};
pub(crate) use host::{EditObserver, FsEditHost};
pub(crate) use seen::{ContextMark, RereadFact, RereadObserver, SeenFiles};

/// Letters in an anchor's hash.
const HASH_LETTERS: usize = 3;
/// Lines one `fs.read` returns by default, and at most.
const READ_DEFAULT_LINES: usize = 2_000;
const READ_MAX_LINES: usize = 20_000;

/// Which kernel operation ran (the telemetry bucket).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsEditOperation {
    Read,
    Edit,
}

/// What adoption telemetry learns of one call: counts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditFact {
    /// `fs.read` returned `lines` lines.
    Read { lines: usize },
    /// `fs.edit` applied a patch of `edits` edits.
    Applied { edits: usize },
    /// `fs.edit` rejected a patch: `stale` anchors no longer matched.
    Rejected { stale: usize },
    /// The call failed (bad input, binary file, I/O): nothing was written.
    Failed { operation: FsEditOperation },
}

/// A line anchor: `LINE:abc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Anchor {
    /// 1-based line number.
    pub(crate) line: usize,
    pub(crate) hash: [u8; HASH_LETTERS],
}

impl std::fmt::Display for Anchor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let letters: String = self.hash.iter().map(|byte| char::from(*byte)).collect();
        write!(formatter, "{}:{letters}", self.line)
    }
}

impl Anchor {
    /// The anchor of the line at 0-based `index`: the hash of the line and
    /// its neighbours.
    pub(crate) fn of(lines: &[document::Line], index: usize) -> Self {
        let mut hasher = Sha256::new();
        let neighbour = |hasher: &mut Sha256, line: Option<&document::Line>| match line {
            // 0xFF never occurs in UTF-8, so the file's edges hash apart
            // from any line's text.
            None => hasher.update([0xFF]),
            Some(line) => hasher.update(line.text.as_bytes()),
        };
        neighbour(
            &mut hasher,
            index.checked_sub(1).and_then(|at| lines.get(at)),
        );
        hasher.update(b"\n");
        hasher.update(lines[index].text.as_bytes());
        hasher.update(b"\n");
        neighbour(&mut hasher, lines.get(index + 1));
        let digest = hasher.finalize();
        let mut value = digest[..8]
            .iter()
            .fold(0_u64, |acc, byte| (acc << 8) | u64::from(*byte));
        let mut hash = [0_u8; HASH_LETTERS];
        for letter in &mut hash {
            *letter = b'a' + (value % 26) as u8;
            value /= 26;
        }
        Self {
            line: index + 1,
            hash,
        }
    }

    /// Parse an anchor as `fs.read` prints it. A pasted row prefix is
    /// accepted too (`> 12:abc|text` reads as `12:abc`).
    ///
    /// # Errors
    ///
    /// Not `LINE:abc` with a positive line and three letters.
    pub(crate) fn parse(raw: &str) -> anyhow::Result<Self> {
        let invalid =
            || anyhow!("{raw:?} is not an anchor (expected LINE:abc as fs.read prints it)");
        let head = raw.split('|').next().unwrap_or_default();
        let head = head.trim().trim_start_matches(['>', '*']).trim_start();
        let (line, letters) = head.split_once(':').ok_or_else(invalid)?;
        let line: usize = line.parse().map_err(|_| invalid())?;
        let letters = letters.trim().to_ascii_lowercase();
        let hash: [u8; HASH_LETTERS] = letters
            .as_bytes()
            .try_into()
            .ok()
            .filter(|hash: &[u8; HASH_LETTERS]| hash.iter().all(u8::is_ascii_lowercase))
            .ok_or_else(invalid)?;
        if line == 0 {
            return Err(invalid());
        }
        Ok(Self { line, hash })
    }
}

/// One line as replies carry it.
fn anchored_row(lines: &[document::Line], index: usize) -> Value {
    json!({
        "line": index + 1,
        "anchor": Anchor::of(lines, index).to_string(),
        "text": lines[index].text,
    })
}

/// Render the line at `index` as `LINE:abc|text`.
fn write_row(out: &mut String, lines: &[document::Line], index: usize) {
    let _ = write!(out, "{}|{}", Anchor::of(lines, index), lines[index].text);
}

/// The `(…)` notes a header carries when a file is not plain LF UTF-8 with
/// a final newline.
fn file_notes(document: &Document) -> String {
    let mut notes = String::new();
    match document.line_ends() {
        LineEnds::CrLf => notes.push_str(", crlf"),
        LineEnds::Mixed => notes.push_str(", mixed line ends"),
        LineEnds::Lf | LineEnds::Unterminated => {}
    }
    if !document.final_newline() {
        notes.push_str(", no final newline");
    }
    match document.encoding {
        Encoding::Utf8 => {}
        other => {
            let _ = write!(notes, ", {}", other.label());
        }
    }
    notes
}

/// The path and working directory every `fs.read`/`fs.edit` payload carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// Absolute (the kernel resolves it against the REPL's cwd).
    pub(crate) path: PathBuf,
    /// As replies show it: relative to the REPL's cwd, `/`-separated.
    pub(crate) shown: String,
}

fn parse_target(data: &Value) -> anyhow::Result<Target> {
    let field = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| anyhow!("{key} must be an absolute path"))
    };
    let path = field("path")?;
    let cwd = field("cwd")?;
    let shown = crate::fs_search::display_path(&path, &cwd);
    Ok(Target { path, shown })
}

fn line_field(data: &Value, key: &str) -> anyhow::Result<Option<usize>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|line| usize::try_from(line).ok())
            .filter(|line| *line >= 1)
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be a line number >= 1")),
    }
}

/// One `fs.read` to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadRequest {
    pub(crate) target: Target,
    /// First line shown (1-based).
    pub(crate) start: usize,
    /// Last line shown (inclusive); `None` reads [`READ_DEFAULT_LINES`].
    pub(crate) end: Option<usize>,
}

/// Parse the `fs.read` payload.
///
/// # Errors
///
/// A missing or relative path, or a bad line range.
pub(crate) fn parse_read(data: &Value) -> anyhow::Result<ReadRequest> {
    let target = parse_target(data)?;
    let start = line_field(data, "start")?.unwrap_or(1);
    let end = line_field(data, "end")?;
    if let Some(end) = end {
        if end < start {
            bail!("end {end} is before start {start}");
        }
    }
    Ok(ReadRequest { target, start, end })
}

/// Run one `fs.read`: the reply and the number of lines it shows.
///
/// # Errors
///
/// The file cannot be loaded as text, or `start` is past its end.
pub(crate) fn read(request: &ReadRequest) -> anyhow::Result<(Value, usize)> {
    let shown = &request.target.shown;
    let document = document::load(&request.target.path, shown)?.document;
    let total = document.lines.len();
    let notes = file_notes(&document);
    if total == 0 {
        return Ok((
            json!({
                "path": shown,
                "start": 0,
                "end": 0,
                "totalLines": 0,
                "lines": [],
                "truncated": false,
                "lineEnds": document.line_ends().label(),
                "encoding": document.encoding.label(),
                "finalNewline": document.final_newline(),
                "text": format!("{shown} (empty{notes})"),
            }),
            0,
        ));
    }
    if request.start > total {
        bail!(
            "start {} is past the end of {shown} ({total} lines)",
            request.start
        );
    }
    let wanted = request
        .end
        .unwrap_or(request.start + READ_DEFAULT_LINES - 1)
        .min(request.start + READ_MAX_LINES - 1);
    let end = wanted.min(total);
    let mut text = format!("{shown} (lines {}-{end} of {total}{notes})", request.start);
    for index in request.start - 1..end {
        text.push('\n');
        write_row(&mut text, &document.lines, index);
    }
    // The default or the cap stopped short of the file's end (an explicit
    // `end` inside the file is not a truncation).
    let truncated = end < total && request.end.is_none_or(|asked| asked > end);
    if truncated {
        let _ = write!(
            text,
            "\n[{} more lines; continue with start={}]",
            total - end,
            end + 1
        );
    }
    let rows: Vec<Value> = (request.start - 1..end)
        .map(|index| anchored_row(&document.lines, index))
        .collect();
    Ok((
        json!({
            "path": shown,
            "start": request.start,
            "end": end,
            "totalLines": total,
            "lines": rows,
            "truncated": truncated,
            "lineEnds": document.line_ends().label(),
            "encoding": document.encoding.label(),
            "finalNewline": document.final_newline(),
            "text": text,
        }),
        end + 1 - request.start,
    ))
}

pub(crate) use patch::{edit, parse_edit, Rewrite};
