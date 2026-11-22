//! The `ctx.*` host requests the kernel's `rlm.ctx` client reaches: read a
//! paged-out output by its handle (`ctx.page`, `ctx.tail`, `ctx.grep`,
//! `ctx.info`) instead of re-running the command that printed it.
//!
//! Every reply is bounded by [`SLICE_CHARS_CAP`], well under a cell's output
//! cap, so reading a slice never spills again. A handle without stored text
//! answers its state (`evicted`, `not_stored` with the reason, `pending`,
//! `unknown`, or `unavailable` for a session with no store) instead of text.

use std::fmt::Write as _;

use serde_json::{json, Value};

use super::{SpillLookup, SpillStore, HANDLE_PREFIX, ID_HEX_LEN};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// The most characters of output one `ctx.*` reply carries.
pub(crate) const SLICE_CHARS_CAP: usize = 16_384;
/// The most lines one `ctx.page`/`ctx.tail` asks for.
pub(crate) const PAGE_LINES_MAX: usize = 2_000;
/// The most matches one `ctx.grep` shows.
pub(crate) const GREP_MATCHES_MAX: usize = 500;
/// The most context lines around a `ctx.grep` match.
pub(crate) const GREP_CONTEXT_MAX: usize = 20;
/// A `ctx.grep` line longer than this shows its first this-many chars.
const GREP_LINE_CHARS_CAP: usize = 2_000;

/// One `ctx.*` read, reported to telemetry (counts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CtxRead {
    Page,
    Grep,
    Tail,
    Info,
}

/// Hears each answered `ctx.*` read and the id of the handle it read
/// (telemetry, and the neighbour prefetch's use tracking).
pub(crate) type CtxObserver = std::sync::Arc<dyn Fn(CtxRead, &str) + Send + Sync>;

/// The id inside a handle (`out:7f3a9c1e04`, or the bare id); `None` when it
/// is not a spill handle's shape.
pub(crate) fn parse_handle(handle: &str) -> Option<&str> {
    let handle = handle.trim();
    let id = handle.strip_prefix(HANDLE_PREFIX).unwrap_or(handle);
    (id.len() == ID_HEX_LEN
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
    .then_some(id)
}

/// Register the `ctx.*` handlers over the session's store (`None` for an
/// ephemeral session, whose handles answer `unavailable`).
pub(crate) fn register_ctx_handlers(
    handlers: &mut HostRequestHandlers,
    store: Option<&SpillStore>,
    observer: Option<&CtxObserver>,
) {
    for request_type in ["ctx.page", "ctx.grep", "ctx.tail", "ctx.info"] {
        let read = match request_type {
            "ctx.grep" => CtxRead::Grep,
            "ctx.tail" => CtxRead::Tail,
            "ctx.info" => CtxRead::Info,
            _ => CtxRead::Page,
        };
        let store = store.cloned();
        let observer = observer.cloned();
        handlers.register(
            request_type,
            host_handler(move |payload| {
                let store = store.clone();
                let observer = observer.clone();
                Box::pin(async move {
                    let reply = answer(store.as_ref(), read, &payload.data).await?;
                    if let Some(observer) = observer {
                        let handle = payload.data.get("handle").and_then(Value::as_str);
                        if let Some(id) = handle.and_then(parse_handle) {
                            observer(read, id);
                        }
                    }
                    Ok(reply)
                })
            }),
        );
    }
}

/// One read's reply.
async fn answer(store: Option<&SpillStore>, read: CtxRead, data: &Value) -> anyhow::Result<Value> {
    let raw = data
        .get("handle")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ctx: handle must be a string"))?;
    let Some(id) = parse_handle(raw) else {
        anyhow::bail!("ctx: {raw:?} is not an output handle (expected {HANDLE_PREFIX}<{ID_HEX_LEN} hex digits>)");
    };
    let handle = format!("{HANDLE_PREFIX}{id}");
    let Some(store) = store else {
        return Ok(json!({"handle": handle, "state": "unavailable"}));
    };
    let spill = match store.lookup(id).await {
        SpillLookup::Stored(spill) => spill,
        SpillLookup::Evicted => return Ok(json!({"handle": handle, "state": "evicted"})),
        SpillLookup::NotStored(reason) => {
            return Ok(json!({"handle": handle, "state": "not_stored", "reason": reason}));
        }
        SpillLookup::Pending => return Ok(json!({"handle": handle, "state": "pending"})),
        SpillLookup::Unknown => return Ok(json!({"handle": handle, "state": "unknown"})),
    };
    let lines: Vec<&str> = spill.text.lines().collect();
    let mut reply = json!({
        "handle": handle,
        "state": "stored",
        "totalChars": spill.total_chars,
        "lines": spill.lines,
        "storedLines": lines.len(),
    });
    let fields = match read {
        CtxRead::Info => return Ok(reply),
        CtxRead::Page => {
            let start = count_arg(data, "start", 0, usize::MAX, "ctx.page")?;
            let wanted = count_arg(data, "lines", 200, PAGE_LINES_MAX, "ctx.page")?;
            slice_forward(&lines, start, wanted)
        }
        CtxRead::Tail => {
            let wanted = count_arg(data, "n", 200, PAGE_LINES_MAX, "ctx.tail")?;
            slice_back(&lines, wanted)
        }
        CtxRead::Grep => grep(&lines, data)?,
    };
    if let (Some(reply), Value::Object(fields)) = (reply.as_object_mut(), fields) {
        reply.extend(fields);
    }
    Ok(reply)
}

/// A non-negative integer argument, `default` when absent, capped at `max`.
fn count_arg(
    data: &Value,
    name: &str,
    default: usize,
    max: usize,
    op: &str,
) -> anyhow::Result<usize> {
    match data.get(name) {
        None | Some(Value::Null) => Ok(default.min(max)),
        Some(value) => value
            .as_u64()
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX).min(max))
            .ok_or_else(|| anyhow::anyhow!("{op}: {name} must be a non-negative integer")),
    }
}

/// The first `chars` chars of `line`.
fn clip(line: &str, chars: usize) -> &str {
    line.char_indices()
        .nth(chars)
        .map_or(line, |(at, _)| &line[..at])
}

/// Lines `start..` (at most `wanted`) within the slice cap: whole lines, or
/// the first line's head when it alone is over the cap.
fn slice_forward(lines: &[&str], start: usize, wanted: usize) -> Value {
    let start = start.min(lines.len());
    let mut text = String::new();
    let mut used = 0usize;
    let mut end = start;
    let mut truncated = false;
    for line in lines.iter().skip(start).take(wanted) {
        let chars = line.chars().count() + 1;
        if used + chars > SLICE_CHARS_CAP {
            if end == start {
                text.push_str(clip(line, SLICE_CHARS_CAP));
                end += 1;
            }
            truncated = true;
            break;
        }
        text.push_str(line);
        text.push('\n');
        used += chars;
        end += 1;
    }
    json!({"start": start, "end": end, "text": text, "truncated": truncated})
}

/// The last `wanted` lines within the slice cap, newest kept first.
fn slice_back(lines: &[&str], wanted: usize) -> Value {
    let mut start = lines.len();
    let mut used = 0usize;
    let mut truncated = false;
    for line in lines.iter().rev().take(wanted) {
        let chars = line.chars().count() + 1;
        if used + chars > SLICE_CHARS_CAP {
            truncated = true;
            break;
        }
        used += chars;
        start -= 1;
    }
    if start == lines.len() && !lines.is_empty() && wanted > 0 {
        // The last line alone is over the cap: show its end.
        let last = lines[lines.len() - 1];
        let total = last.chars().count();
        let from = last
            .char_indices()
            .nth(total.saturating_sub(SLICE_CHARS_CAP))
            .map_or(0, |(at, _)| at);
        return json!({"start": lines.len() - 1, "end": lines.len(), "text": &last[from..], "truncated": true});
    }
    let mut text = String::with_capacity(used);
    for line in &lines[start..] {
        text.push_str(line);
        text.push('\n');
    }
    json!({"start": start, "end": lines.len(), "text": text, "truncated": truncated})
}

/// `grep -n`-style matches with context: `N:line` for a match, `N-line` for
/// context (`N` the 0-based line `ctx.page` starts at), `--` between groups.
fn grep(lines: &[&str], data: &Value) -> anyhow::Result<Value> {
    let pattern = data
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ctx.grep: pattern must be a string"))?;
    let regex = fancy_regex::Regex::new(pattern)
        .map_err(|error| anyhow::anyhow!("ctx.grep: invalid pattern: {error}"))?;
    let context = count_arg(data, "context", 2, GREP_CONTEXT_MAX, "ctx.grep")?;
    let max_matches = count_arg(data, "max_matches", 50, GREP_MATCHES_MAX, "ctx.grep")?;
    let mut matched = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if regex
            .is_match(line)
            .map_err(|error| anyhow::anyhow!("ctx.grep: the pattern failed: {error}"))?
        {
            matched.push(at);
        }
    }
    let shown_matches = &matched[..matched.len().min(max_matches)];
    let mut text = String::new();
    let mut used = 0usize;
    let mut shown = 0usize;
    let mut truncated = matched.len() > shown_matches.len();
    let mut next = 0usize;
    'matches: for (position, &at) in shown_matches.iter().enumerate() {
        let from = at.saturating_sub(context).max(next);
        if position > 0 && from > next {
            text.push_str("--\n");
        }
        let to = (at + context + 1).min(lines.len());
        for (line_at, line) in lines.iter().enumerate().take(to).skip(from) {
            let mark = if shown_matches.binary_search(&line_at).is_ok() {
                ':'
            } else {
                '-'
            };
            let mut row = String::new();
            let _ = write!(row, "{line_at}{mark}{}", clip(line, GREP_LINE_CHARS_CAP));
            let row_chars = row.chars().count() + 1;
            if used + row_chars > SLICE_CHARS_CAP {
                truncated = true;
                break 'matches;
            }
            used += row_chars;
            text.push_str(&row);
            text.push('\n');
        }
        next = to;
        shown = position + 1;
    }
    Ok(json!({
        "pattern": pattern,
        "matches": matched.len(),
        "shown": shown,
        "text": text,
        "truncated": truncated,
    }))
}

#[cfg(test)]
mod tests;
