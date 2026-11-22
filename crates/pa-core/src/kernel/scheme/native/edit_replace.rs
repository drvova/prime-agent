//! `(edit path old-str new-str)`: replace one exact, unique occurrence of a
//! string in an existing file, the `edit` skill of the Python kernel.
//!
//! The file is read and written as UTF-8, byte for byte: its line endings
//! stay its own, so `old-str` must spell them as the file does. A miss says
//! where `old-str` stops matching, so the next call can fix it without
//! another read.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};

use crate::tools::path_utils::resolve_to_cwd;

/// Characters of the file and of `old-str` shown at the divergence point.
const SPLIT_CONTEXT: usize = 60;

/// Replace the one occurrence of `old-str` in the file at `path` (relative to
/// `cwd`, absolute, or `~`-prefixed): the edited file and the reply (`path`,
/// `startLine`, the 1-based line the match starts on, and `text`).
///
/// # Errors
///
/// Bad options, a file that is missing or not UTF-8, or an `old-str` that is
/// absent or matches more than once; nothing is written then.
pub(super) fn replace(cwd: &Path, options: &Value) -> anyhow::Result<(PathBuf, Value)> {
    let text = |name: &str| {
        options
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{name} must be a string"))
    };
    let (path, old, new) = (text("path")?, text("old-str")?, text("new-str")?);
    if path.is_empty() {
        bail!("path must be a non-empty string");
    }
    let file = PathBuf::from(resolve_to_cwd(path, &cwd.to_string_lossy()));
    if !file.exists() {
        bail!("{path} not found");
    }
    let content = std::fs::read_to_string(&file)
        .with_context(|| format!("{path} is not readable as UTF-8"))?;
    let Some(at) = content.find(old) else {
        bail!("{}", miss_diagnostic(path, &content, old));
    };
    let count = content.matches(old).count();
    if count > 1 {
        bail!(
            "found {count} occurrences in {path}, need exactly 1 — widen the snippet to make it unique"
        );
    }
    let start_line = content[..at].matches('\n').count() + 1;
    std::fs::write(&file, content.replacen(old, new, 1))
        .with_context(|| format!("{path} could not be written"))?;
    let file = file.canonicalize().unwrap_or(file);
    let reply = json!({
        "path": file.to_string_lossy(),
        "startLine": start_line,
        "text": format!("Edited {}", file.display()),
    });
    Ok((file, reply))
}

/// Say where `old` stops matching: the longest leading piece of it the file
/// contains (at its first occurrence), a bounded view of both sides at the
/// split, and a hint when the split is a line-ending or whitespace difference.
fn miss_diagnostic(path: &str, content: &str, old: &str) -> String {
    // Containment is monotonic in the prefix length, so binary-search the
    // longest prefix the file contains (the whole of `old` is known absent).
    let starts: Vec<usize> = old.char_indices().map(|(start, _)| start).collect();
    let (mut low, mut high) = (0, starts.len().saturating_sub(1));
    while low < high {
        let middle = (low + high).div_ceil(2);
        if content.contains(&old[..starts[middle]]) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low == 0 {
        return format!(
            "string not found in {path}: the file does not contain even the first character of old-str"
        );
    }
    let matched = &old[..starts[low]];
    let point = content.find(matched).unwrap_or_default() + matched.len();
    let before = &content[..point];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count()
        + 1;
    let file_next: String = content[point..].chars().take(SPLIT_CONTEXT).collect();
    let old_next: String = old[starts[low]..].chars().take(SPLIT_CONTEXT).collect();
    let mut message = format!(
        "string not found in {path}: old-str matches its first {low} of {} characters and diverges at line {line}, column {column}:\n  file continues    {file_next:?}\n  old-str continues {old_next:?}",
        starts.len()
    );
    if file_next.starts_with("\r\n") && old_next.starts_with('\n') {
        message.push_str(
            "\nhint: the file has CRLF (\\r\\n) line endings; write them as \\r\\n in old-str",
        );
    } else if old_next.starts_with("\r\n") && file_next.starts_with('\n') {
        message.push_str("\nhint: old-str has CRLF (\\r\\n) line endings; the file has \\n");
    } else if [&file_next, &old_next]
        .iter()
        .any(|next| next.chars().next().is_some_and(char::is_whitespace))
    {
        message.push_str(
            "\nhint: whitespace differs at the split (indentation, tabs vs spaces, or trailing spaces)",
        );
    }
    message
}
