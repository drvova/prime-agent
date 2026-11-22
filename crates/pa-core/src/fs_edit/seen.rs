//! Diff-only re-reads: the version of each file the model has seen this session, from its
//! whole-file `fs.read` results and its applied `fs.edit`s. A whole-file re-read of a file
//! unchanged since it was seen shows one line (`unchanged since turn N`) with a handle to the
//! full view; a changed file shows a diff against the seen version with the same handle. The
//! reply's `lines` stay complete, so code that uses the view (`view.anchor(n)`, `view.lines`)
//! behaves exactly as before; only what the transcript shows (`str(view)`) shrinks.
//!
//! A seen version counts only while its content is certainly still in the model's context:
//! - no compaction ran since it was seen (a compaction summarizes the transcript, and the
//!   summary may have dropped the read), and
//! - it was seen fewer than `window_turns` assistant turns ago, inside the newest rounds
//!   that request-view thinning never cuts (`context_thinning`'s `keep_recent_rounds`).
//!
//! Otherwise the re-read shows the whole file and is seen anew. The full view goes to the
//! session's spill store (`ctx.page(handle)`), so a session without one reads whole files.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde_json::{json, Value};

use crate::kernel::spill::{SpillCopy, SpillStore, HANDLE_PREFIX};
use crate::tools::kernel_cell::diff_lines;

/// Unchanged lines shown around each change of a re-read diff.
const DIFF_CONTEXT: usize = 2;
/// How long a read waits for the context probe; past it the read is shown whole (and not
/// recorded), so a stuck probe never holds a cell.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Where the model's context stands now: its assistant turns so far, and when the newest
/// compaction ran (`None` when none has).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextMark {
    pub(crate) turn: u64,
    pub(crate) compacted_at_ms: Option<u64>,
}

/// Reads the session's [`ContextMark`] (the engine binds it once its agent exists).
pub(crate) type ContextProbe =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ContextMark> + Send>> + Send + Sync>;

/// How one whole-file re-read was shown, for telemetry (counts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RereadFact {
    /// Unchanged since seen: one line and a handle.
    Unchanged,
    /// Changed since seen: a diff and a handle.
    Diff,
    /// Seen before, but shown whole: the seen version left the context, or the diff was no
    /// smaller than the file.
    Full,
}

/// Hears each re-read of a file seen before (session telemetry).
pub(crate) type RereadObserver = Arc<dyn Fn(RereadFact) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenVersion {
    lines: Vec<String>,
    /// The assistant turn it was seen in.
    turn: u64,
    /// When it was seen (epoch ms), against the newest compaction.
    at_ms: u64,
}

/// One session's seen files.
pub(crate) struct SeenFiles {
    files: Mutex<HashMap<PathBuf, SeenVersion>>,
    store: Option<SpillStore>,
    probe: OnceLock<ContextProbe>,
    window_turns: u64,
    observer: Option<RereadObserver>,
}

impl SeenFiles {
    pub(crate) fn new(
        store: Option<SpillStore>,
        window_turns: u64,
        observer: Option<RereadObserver>,
    ) -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            store,
            probe: OnceLock::new(),
            window_turns,
            observer,
        }
    }

    /// Bind the session's context probe. A second bind is ignored.
    pub(crate) fn bind(&self, probe: ContextProbe) {
        let _ = self.probe.set(probe);
    }

    fn valid(&self, seen: &SeenVersion, mark: ContextMark) -> bool {
        mark.compacted_at_ms.is_none_or(|at| at < seen.at_ms)
            && mark.turn >= seen.turn
            && mark.turn - seen.turn < self.window_turns
    }

    async fn mark(&self) -> Option<ContextMark> {
        let probe = self.probe.get()?;
        tokio::time::timeout(PROBE_TIMEOUT, probe()).await.ok()
    }

    /// Shape one `fs.read` reply of `path` against the version seen before, and record what it
    /// shows as seen. Only a whole-file read with a spill store is shaped.
    pub(crate) async fn shape_read(&self, path: &Path, reply: &mut Value) {
        let whole = reply["start"].as_u64().is_some_and(|start| start <= 1)
            && reply["truncated"] == false
            && reply["end"] == reply["totalLines"];
        let Some(store) = self.store.as_ref().filter(|_| whole) else {
            return;
        };
        let Some(mark) = self.mark().await else {
            return;
        };
        let Some(rows) = reply["lines"].as_array() else {
            return;
        };
        let lines: Vec<String> = rows
            .iter()
            .map(|row| row["text"].as_str().unwrap_or_default().to_string())
            .collect();
        let now = pa_ai::admission::now_ms();
        let prior = lock(&self.files).get(path).cloned();
        let seen_now = SeenVersion {
            lines: lines.clone(),
            turn: mark.turn,
            at_ms: now,
        };
        let Some(prior) = prior else {
            lock(&self.files).insert(path.to_path_buf(), seen_now);
            return;
        };
        let full_text = reply["text"].as_str().unwrap_or_default().to_string();
        let shown = reply["path"].as_str().unwrap_or_default().to_string();
        let total = lines.len();
        let fact = if !self.valid(&prior, mark) {
            lock(&self.files).insert(path.to_path_buf(), seen_now);
            RereadFact::Full
        } else if prior.lines == lines {
            let handle = handle_for(store, &full_text);
            reply["text"] = Value::from(format!(
                "{shown} (unchanged since turn {}, {total} lines; whole view: {handle})",
                prior.turn
            ));
            reply["seen"] = json!({"state": "unchanged", "turn": prior.turn, "handle": handle});
            RereadFact::Unchanged
        } else {
            let (body, removed, added) = render_diff(&prior.lines, &lines, rows);
            lock(&self.files).insert(path.to_path_buf(), seen_now);
            if body.len() >= full_text.len() {
                RereadFact::Full
            } else {
                let handle = handle_for(store, &full_text);
                reply["text"] = Value::from(format!(
                    "{shown} (changed since turn {}: -{removed} +{added} lines, {total} total; whole view: {handle}){body}",
                    prior.turn
                ));
                reply["seen"] = json!({"state": "changed", "turn": prior.turn, "handle": handle});
                RereadFact::Diff
            }
        };
        if let Some(observer) = &self.observer {
            observer(fact);
        }
    }

    /// An applied `fs.edit` turned `before` into `after`: a file seen as `before` (and still in
    /// context) is now seen as `after`, since the model wrote the change itself.
    pub(crate) async fn note_edit(&self, path: &Path, before: &[String], after: Vec<String>) {
        let Some(mark) = self.mark().await else {
            return;
        };
        let mut files = lock(&self.files);
        let Some(seen) = files.get(path) else {
            return;
        };
        if self.valid(seen, mark) && seen.lines == before {
            files.insert(
                path.to_path_buf(),
                SeenVersion {
                    lines: after,
                    turn: mark.turn,
                    at_ms: pa_ai::admission::now_ms(),
                },
            );
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Page the full view out to the spill store: its handle.
fn handle_for(store: &SpillStore, full_text: &str) -> String {
    let handle = store.spill(SpillCopy::of_text(full_text));
    format!("{HANDLE_PREFIX}{}", handle.id)
}

/// One line of a re-read diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row<'a> {
    /// Unchanged, at this 0-based index of the current file.
    Keep(usize),
    /// Only in the current file, at this index.
    Add(usize),
    /// Only in the seen version.
    Remove(&'a str),
}

/// The diff of `seen` to `current` as rows (` LINE:abc|text`, `+LINE:abc|text`, `-text`) with
/// [`DIFF_CONTEXT`] unchanged lines around each change, `...` between hunks; and the removed
/// and added line counts. `rows` are the current file's anchored rows.
fn render_diff(seen: &[String], current: &[String], rows: &[Value]) -> (String, usize, usize) {
    let joined = |lines: &[String]| {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    };
    let mut diff: Vec<Row<'_>> = Vec::new();
    let (mut old_at, mut new_at) = (0, 0);
    for part in diff_lines(&joined(seen), &joined(current)) {
        let count = part.value.matches('\n').count();
        for _ in 0..count {
            if part.added {
                diff.push(Row::Add(new_at));
                new_at += 1;
            } else if part.removed {
                diff.push(Row::Remove(seen[old_at].as_str()));
                old_at += 1;
            } else {
                diff.push(Row::Keep(new_at));
                old_at += 1;
                new_at += 1;
            }
        }
    }
    let changed: Vec<usize> = (0..diff.len())
        .filter(|at| !matches!(diff[*at], Row::Keep(_)))
        .collect();
    let shown = |at: usize| {
        changed
            .iter()
            .any(|change| change.abs_diff(at) <= DIFF_CONTEXT)
    };
    let row_text = |index: usize| {
        let row = &rows[index];
        format!(
            "{}|{}",
            row["anchor"].as_str().unwrap_or_default(),
            row["text"].as_str().unwrap_or_default()
        )
    };
    let mut body = String::new();
    let mut last_shown: Option<usize> = None;
    for (at, row) in diff.iter().enumerate() {
        if !shown(at) {
            continue;
        }
        if last_shown.is_some_and(|last| last + 1 < at) {
            body.push_str("\n...");
        }
        last_shown = Some(at);
        let _ = match row {
            Row::Keep(index) => write!(body, "\n {}", row_text(*index)),
            Row::Add(index) => write!(body, "\n+{}", row_text(*index)),
            Row::Remove(text) => write!(body, "\n-{text}"),
        };
    }
    let removed = diff
        .iter()
        .filter(|row| matches!(row, Row::Remove(_)))
        .count();
    (body, removed, changed.len() - removed)
}

#[cfg(test)]
mod tests;
