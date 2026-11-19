//! `fs.edit`: parse a patch of anchored edits, check every anchor against
//! the file as it is now, and apply the patch atomically or reject all of it.
//!
//! Every edit addresses the file as last read: line numbers never shift
//! between the edits of one patch. Order of checks, each writing nothing on
//! failure: every anchor must match (else a structured rejection with the
//! stale anchors and the current lines around them), ranges must run forward,
//! and no two edits may touch the same lines or insert at the same place.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{anyhow, bail};
use serde_json::{json, Value};

use super::document::{self, Document, Eol, Line};
use super::{anchored_row, parse_target, write_row, Anchor, EditFact, Target};

/// Lines of current context shown around each stale anchor.
const STALE_CONTEXT: usize = 2;
/// Lines of context shown around each changed region (their anchors moved
/// with it: a hash covers its neighbours).
const HUNK_CONTEXT: usize = 1;

/// Where an insert goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gap {
    After(Anchor),
    Before(Anchor),
    /// After the last line (unanchored: also how an empty file is filled).
    End,
}

/// One anchored edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Edit {
    /// Replace the lines `start..=end` with `lines`; no lines deletes them.
    Replace {
        start: Anchor,
        end: Anchor,
        lines: Vec<String>,
    },
    /// Insert `lines` (at least one) at a gap.
    Insert { at: Gap, lines: Vec<String> },
}

impl Edit {
    fn anchors(&self) -> Vec<Anchor> {
        match self {
            Self::Replace { start, end, .. } if start == end => vec![*start],
            Self::Replace { start, end, .. } => vec![*start, *end],
            Self::Insert {
                at: Gap::After(anchor) | Gap::Before(anchor),
                ..
            } => vec![*anchor],
            Self::Insert { at: Gap::End, .. } => Vec::new(),
        }
    }
}

/// One `fs.edit` to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditRequest {
    pub(crate) target: Target,
    pub(crate) edits: Vec<Edit>,
}

/// A patch's `text`: a string (one trailing newline dropped, `""` is no
/// lines) or a list of lines. Line terminators are the file's, never the
/// text's.
fn text_lines(value: Option<&Value>, number: usize) -> anyhow::Result<Vec<String>> {
    let strip_cr = |line: &str| line.strip_suffix('\r').unwrap_or(line).to_string();
    match value {
        Some(Value::String(text)) if text.is_empty() => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(text
            .strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .map(strip_cr)
            .collect()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item.as_str() {
                Some(line) if !line.contains('\n') => Ok(strip_cr(line)),
                Some(_) => Err(anyhow!("edit {number}: a text list item holds a newline")),
                None => Err(anyhow!("edit {number}: text lists hold strings")),
            })
            .collect(),
        None | Some(Value::Null) => bail!("edit {number}: text is required"),
        Some(_) => bail!("edit {number}: text must be a string or a list of strings"),
    }
}

fn anchor_field(edit: &Value, key: &str, number: usize) -> anyhow::Result<Option<Anchor>> {
    match edit.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => Anchor::parse(raw)
            .map(Some)
            .map_err(|error| anyhow!("edit {number}: {error}")),
        Some(_) => bail!("edit {number}: {key} must be an anchor string"),
    }
}

fn parse_one(edit: &Value, number: usize) -> anyhow::Result<Edit> {
    let op = edit
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("edit {number}: op is required"))?;
    let anchor = anchor_field(edit, "anchor", number)?;
    let end = anchor_field(edit, "end", number)?;
    let required = || anchor.ok_or_else(|| anyhow!("edit {number}: {op} needs an anchor"));
    let no_end = || match end {
        Some(_) => Err(anyhow!("edit {number}: {op} takes one anchor, not a range")),
        None => Ok(()),
    };
    let inserted = || -> anyhow::Result<Vec<String>> {
        let lines = text_lines(edit.get("text"), number)?;
        if lines.is_empty() {
            bail!("edit {number}: {op} needs text to insert");
        }
        Ok(lines)
    };
    match op {
        "replace" | "delete" => {
            let start = required()?;
            let lines = if op == "delete" {
                if edit.get("text").is_some_and(|text| !text.is_null()) {
                    bail!("edit {number}: delete takes no text");
                }
                Vec::new()
            } else {
                text_lines(edit.get("text"), number)?
            };
            Ok(Edit::Replace {
                start,
                end: end.unwrap_or(start),
                lines,
            })
        }
        "insert_after" | "insert_before" => {
            let anchor = required()?;
            no_end()?;
            let at = if op == "insert_after" {
                Gap::After(anchor)
            } else {
                Gap::Before(anchor)
            };
            Ok(Edit::Insert {
                at,
                lines: inserted()?,
            })
        }
        "append" => {
            if anchor.is_some() || end.is_some() {
                bail!("edit {number}: append takes no anchor");
            }
            Ok(Edit::Insert {
                at: Gap::End,
                lines: inserted()?,
            })
        }
        other => bail!(
            "edit {number}: unknown op {other:?} (replace, delete, insert_after, insert_before, append)"
        ),
    }
}

/// Parse the `fs.edit` payload.
///
/// # Errors
///
/// A missing or relative path, no edits, or a malformed edit.
pub(crate) fn parse_edit(data: &Value) -> anyhow::Result<EditRequest> {
    let target = parse_target(data)?;
    let edits = data
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("edits must be a list"))?;
    if edits.is_empty() {
        bail!("edits is empty");
    }
    let edits = edits
        .iter()
        .enumerate()
        .map(|(index, edit)| parse_one(edit, index + 1))
        .collect::<anyhow::Result<_>>()?;
    Ok(EditRequest { target, edits })
}

/// An anchor that no longer matches.
struct Stale {
    /// 1-based edit number.
    edit: usize,
    anchor: Anchor,
    /// The anchor its line has now; `None` when the line is past the end.
    current: Option<Anchor>,
}

/// The structured rejection: every stale anchor and the current lines
/// around each, with fresh anchors to retry from.
fn rejection(shown: &str, lines: &[Line], stale: &[Stale], anchors: usize) -> Value {
    let total = lines.len();
    let mut text = format!(
        "fs.edit rejected, nothing was written: {} of {anchors} anchors in {shown} no longer match the file.",
        stale.len()
    );
    let mut shown_rows = BTreeSet::new();
    let mut marked = BTreeSet::new();
    for entry in stale {
        match entry.current {
            Some(current) => {
                let _ = write!(
                    text,
                    "\n- edit {}: {} is now {current} (the line or a neighbour changed)",
                    entry.edit, entry.anchor
                );
                marked.insert(current.line - 1);
            }
            None => {
                let _ = write!(
                    text,
                    "\n- edit {}: {} is past the end ({total} lines)",
                    entry.edit, entry.anchor
                );
            }
        }
        let center = entry.anchor.line.min(total).saturating_sub(1);
        if total > 0 {
            shown_rows.extend(
                center.saturating_sub(STALE_CONTEXT)..(center + STALE_CONTEXT + 1).min(total),
            );
        }
    }
    if shown_rows.is_empty() {
        text.push_str("\nThe file is now empty.");
    } else {
        text.push_str("\nCurrent lines (> marks a stale anchor's line):");
        let mut previous: Option<usize> = None;
        for &index in &shown_rows {
            if previous.is_some_and(|previous| index > previous + 1) {
                text.push_str("\n  ...");
            }
            previous = Some(index);
            text.push_str(if marked.contains(&index) {
                "\n> "
            } else {
                "\n  "
            });
            write_row(&mut text, lines, index);
        }
    }
    json!({
        "applied": false,
        "path": shown,
        "stale": stale
            .iter()
            .map(|entry| json!({
                "edit": entry.edit,
                "anchor": entry.anchor.to_string(),
                "current": entry.current.map(|current| current.to_string()),
            }))
            .collect::<Vec<_>>(),
        "lines": shown_rows
            .iter()
            .map(|&index| anchored_row(lines, index))
            .collect::<Vec<_>>(),
        "text": text,
    })
}

/// One edit placed on the file as read.
#[derive(Debug, Clone, Copy)]
enum Placed<'a> {
    /// Replace original lines `first..=last` (1-based).
    Lines {
        first: usize,
        last: usize,
        lines: &'a [String],
    },
    /// Insert into the gap after original line `gap` (0 is the file's head).
    Gap { gap: usize, lines: &'a [String] },
}

fn describe_lines(first: usize, last: usize) -> String {
    if first == last {
        format!("line {first}")
    } else {
        format!("lines {first}-{last}")
    }
}

/// Place every edit, rejecting backward ranges and edits that touch the
/// same lines or insert at the same place.
fn place(edits: &[Edit], total: usize) -> anyhow::Result<Vec<Placed<'_>>> {
    let mut placed = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        placed.push(match edit {
            Edit::Replace { start, end, lines } => {
                if end.line < start.line {
                    bail!(
                        "edit {}: the range ends ({end}) before it starts ({start})",
                        index + 1
                    );
                }
                Placed::Lines {
                    first: start.line,
                    last: end.line,
                    lines,
                }
            }
            Edit::Insert { at, lines } => Placed::Gap {
                gap: match at {
                    Gap::After(anchor) => anchor.line,
                    Gap::Before(anchor) => anchor.line - 1,
                    Gap::End => total,
                },
                lines,
            },
        });
    }
    for (left_index, left) in placed.iter().enumerate() {
        for (right_index, right) in placed.iter().enumerate().skip(left_index + 1) {
            let (left_number, right_number) = (left_index + 1, right_index + 1);
            match (*left, *right) {
                (
                    Placed::Lines {
                        first: a_first,
                        last: a_last,
                        ..
                    },
                    Placed::Lines {
                        first: b_first,
                        last: b_last,
                        ..
                    },
                ) => {
                    if a_first <= b_last && b_first <= a_last {
                        bail!(
                            "edits {left_number} and {right_number} overlap ({} and {}); merge them into one edit",
                            describe_lines(a_first, a_last),
                            describe_lines(b_first, b_last)
                        );
                    }
                }
                (Placed::Lines { first, last, .. }, Placed::Gap { gap, .. }) => {
                    if first <= gap && gap < last {
                        bail!(
                            "edit {right_number} inserts inside the {} edit {left_number} replaces; merge them into one edit",
                            describe_lines(first, last)
                        );
                    }
                }
                (Placed::Gap { gap, .. }, Placed::Lines { first, last, .. }) => {
                    if first <= gap && gap < last {
                        bail!(
                            "edit {left_number} inserts inside the {} edit {right_number} replaces; merge them into one edit",
                            describe_lines(first, last)
                        );
                    }
                }
                (Placed::Gap { gap: a_gap, .. }, Placed::Gap { gap: b_gap, .. }) => {
                    if a_gap == b_gap {
                        bail!(
                            "edits {left_number} and {right_number} insert at the same place; merge them into one edit"
                        );
                    }
                }
            }
        }
    }
    Ok(placed)
}

/// The patched lines, and each changed region as a half-open range of new
/// line indices (empty for a deletion).
fn apply(document: &Document, placed: &[Placed<'_>]) -> (Vec<Line>, Vec<(usize, usize)>) {
    let mut inserts: BTreeMap<usize, &[String]> = BTreeMap::new();
    let mut replaces: BTreeMap<usize, (usize, &[String])> = BTreeMap::new();
    for edit in placed {
        match *edit {
            Placed::Lines { first, last, lines } => {
                replaces.insert(first, (last, lines));
            }
            Placed::Gap { gap, lines } => {
                inserts.insert(gap, lines);
            }
        }
    }
    let fresh = |text: &String| Line {
        text: text.clone(),
        eol: Eol::None,
    };
    let original = &document.lines;
    let mut out: Vec<Line> = Vec::with_capacity(original.len());
    let mut changed = Vec::new();
    let mut line = 1;
    loop {
        if let Some(lines) = inserts.get(&(line - 1)) {
            let at = out.len();
            out.extend(lines.iter().map(fresh));
            changed.push((at, out.len()));
        }
        if line > original.len() {
            break;
        }
        if let Some((last, lines)) = replaces.get(&line) {
            let at = out.len();
            out.extend(lines.iter().map(fresh));
            changed.push((at, out.len()));
            line = last + 1;
        } else {
            out.push(original[line - 1].clone());
            line += 1;
        }
    }
    // New lines take the file's majority terminator; the last line keeps
    // the file's own ending (with or without a final newline).
    let majority = document.majority_eol();
    let last_eol = original.last().map_or(majority, |line| line.eol);
    let count = out.len();
    for (index, line) in out.iter_mut().enumerate() {
        if index + 1 == count {
            line.eol = last_eol;
        } else if line.eol == Eol::None {
            line.eol = majority;
        }
    }
    (out, changed)
}

/// Run one `fs.edit`: the reply and its telemetry fact.
///
/// # Errors
///
/// The file cannot be loaded as text, the patch's layout is invalid, it
/// changes nothing, or the write fails. A stale anchor is not an error: it
/// is the structured rejection reply.
pub(crate) fn edit(request: &EditRequest) -> anyhow::Result<(Value, EditFact)> {
    let shown = &request.target.shown;
    let loaded = document::load(&request.target.path, shown)?;
    let lines = &loaded.document.lines;
    let mut stale = Vec::new();
    let mut anchors = 0;
    for (index, edit) in request.edits.iter().enumerate() {
        for anchor in edit.anchors() {
            anchors += 1;
            let current = (anchor.line <= lines.len()).then(|| Anchor::of(lines, anchor.line - 1));
            if current != Some(anchor) {
                stale.push(Stale {
                    edit: index + 1,
                    anchor,
                    current,
                });
            }
        }
    }
    if !stale.is_empty() {
        return Ok((
            rejection(shown, lines, &stale, anchors),
            EditFact::Rejected { stale: stale.len() },
        ));
    }
    let placed = place(&request.edits, lines.len())?;
    let (patched, mut changed) = apply(&loaded.document, &placed);
    let patched = Document {
        encoding: loaded.document.encoding,
        lines: patched,
    };
    let bytes = patched.encode();
    if bytes == loaded.bytes {
        bail!("the patch changes nothing in {shown}; nothing was written");
    }
    document::replace_atomically(&request.target.path, shown, &loaded.bytes, &bytes)?;

    let (mut removed, mut added) = (0, 0);
    for edit in &placed {
        match *edit {
            Placed::Lines { first, last, lines } => {
                removed += last + 1 - first;
                added += lines.len();
            }
            Placed::Gap { lines, .. } => added += lines.len(),
        }
    }
    let total = patched.lines.len();
    changed.sort_unstable();
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for (first, end) in changed {
        let low = first.saturating_sub(HUNK_CONTEXT);
        let high = (end + HUNK_CONTEXT).min(total);
        match hunks.last_mut() {
            Some(last) if low <= last.1 => last.1 = last.1.max(high),
            _ => hunks.push((low, high)),
        }
    }
    hunks.retain(|(low, high)| low < high);
    let edits = request.edits.len();
    let mut text = format!(
        "{shown}: {edits} edit{} applied, -{removed} +{added} lines, {total} total",
        if edits == 1 { "" } else { "s" }
    );
    if total == 0 {
        text.push_str("; the file is now empty");
    }
    for (number, (low, high)) in hunks.iter().enumerate() {
        if number > 0 {
            text.push_str("\n...");
        }
        for index in *low..*high {
            text.push('\n');
            write_row(&mut text, &patched.lines, index);
        }
    }
    Ok((
        json!({
            "applied": true,
            "path": shown,
            "edits": edits,
            "removed": removed,
            "added": added,
            "totalLines": total,
            "hunks": hunks
                .iter()
                .map(|(low, high)| json!({
                    "start": low + 1,
                    "end": high,
                    "lines": (*low..*high).map(|index| anchored_row(&patched.lines, index)).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
            "text": text,
        }),
        EditFact::Applied { edits },
    ))
}
