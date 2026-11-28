//! The saved-session catalog both daemons list: one row per session file,
//! folded from its transcript and kept warm across reads (moved from
//! pa-daemon's session store and `session_scan`, unchanged, so the old
//! daemon's `list_saved_sessions` and the runtime daemon's
//! `sessions.list {shape: "saved"}` serve the same rows).
//!
//! - [`read_session_info`]: one file's [`SessionInfo`]. Unchanged files
//!   answer from a process-wide cache, grown files fold only their appended
//!   rows, rewritten files rescan; a lease holder's release persists the fold
//!   beside the file ([`write_info_sidecar`]), so the next process resumes it.
//! - [`list_sessions_with`]: a page of a store's files, newest first, with a
//!   per-row callback so a listing streams rows while it walks, and the
//!   cursor that resumes the walk ([`SessionPage`]).
//! - [`saved_session_row`]: a row's wire form, the old `session_list_item`'s
//!   `session` (`DaemonSavedSessionInfo`).

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::Path;

use pa_types::session::SessionHeader;
use serde_json::{json, Map, Value};

mod info;
mod info_sidecar;
#[cfg(test)]
mod info_tests;
mod scan;

pub use info::{
    find_most_recent_session_for_cwd, legacy_ledger_usage, read_session_info, SessionInfo,
};
pub use scan::{list_sessions, list_sessions_with, SessionPage};

/// The bounded first-line read cap for header-only judgments: a session
/// header line lands well inside 512 bytes.
pub const SESSION_LIST_HEADER_READ_MAX_BYTES: usize = 512;

/// Parse one session file line as the `session` header: a JSON object tagged
/// `session` that deserializes into the typed header.
#[must_use]
pub fn parse_session_header_line(line: &str) -> Option<SessionHeader> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// A message content field's text: the string itself, or its text blocks
/// joined by a space; anything else is empty.
#[must_use]
pub fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    block.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// A stored `session_state` status as the catalog reports it: the retired
/// `hidden` and `sleep` read as `archived`.
#[must_use]
pub fn normalize_state_status(status: &str) -> String {
    match status {
        "hidden" | "sleep" => "archived".to_string(),
        other => other.to_string(),
    }
}

/// What a session lease holder writes beside its window snapshot
/// (`SessionLease::with_info_sidecar`): the info scan state of the file at
/// `path`, folded current (rows appended since the last read, the close's own
/// final rows, fold here instead of at the next open) and persisted.
/// Best-effort: a failed write costs the next open its warm resume.
pub fn write_info_sidecar(path: &Path) {
    let _ = read_session_info(path);
    info_sidecar::persist_info_sidecar(path);
}

/// The catalog row of `info`, as the old daemon's `session_list_item` carries
/// it: `persistent_shells` holds the live persistent shells' activity rows by
/// session id, `factory_specs` the stored factory spec counts by session id
/// (a session with neither has no such key).
#[must_use]
pub fn saved_session_row<S: BuildHasher>(
    info: &SessionInfo,
    persistent_shells: &HashMap<String, Vec<Value>, S>,
    factory_specs: &HashMap<String, usize, S>,
) -> Value {
    let mut object = Map::new();
    for (key, value) in [
        ("path", json!(info.path.to_string_lossy())),
        ("id", json!(info.id)),
        ("cwd", json!(info.cwd)),
        ("rlmDepth", json!(info.rlm_depth)),
        ("created", json!(info.created)),
        ("modified", json!(info.modified)),
        ("messageCount", json!(info.message_count)),
        ("firstMessage", json!(info.first_message)),
        (
            "state",
            json!(info.state.as_ref().map(|state| json!({ "status": state }))),
        ),
    ] {
        object.insert(key.to_string(), value);
    }
    if let Some(name) = &info.name {
        object.insert("name".to_string(), json!(name));
    }
    if let Some(parent) = &info.parent_session_path {
        object.insert("parentSessionPath".to_string(), json!(parent));
    }
    if let Some((provider, model_id)) = &info.model {
        object.insert(
            "model".to_string(),
            json!({ "provider": provider, "modelId": model_id }),
        );
    }
    // TS `serializeSavedSessionInfo` publishes the scan's own-usage summary:
    // the spend columns and the archived-row keep-condition read
    // `saved.usage.cost`, so rollups never double count.
    if let Some(usage) = &info.usage {
        object.insert("usage".to_string(), json!(usage));
    }
    // TS #2506 `serializeSavedSessionInfo`'s optional `deletedDescendantUsage`: the
    // recursive spend of tombstoned descendants — the deleted child keeps no row anywhere,
    // its spend bills here exactly once.
    if let Some(deleted) = &info.deleted_descendant_usage {
        object.insert("deletedDescendantUsage".to_string(), json!(deleted));
    }
    // The persisted thinking level rides the catalog row too: the TUI merges it into
    // live summaries that lack one.
    if let Some(level) = &info.thinking_level {
        object.insert("thinkingLevel".to_string(), json!(level));
    }
    // Persistent `bash()` shells outlive the session's kernel: an inactive row still
    // shows the live ones (kernel activity row shape). Its stored factory specs are
    // durable harness entries, readable without a kernel too.
    if let Some(shells) = persistent_shells.get(&info.id) {
        object.insert("persistentShells".to_string(), json!(shells));
    }
    if let Some(specs) = factory_specs.get(&info.id) {
        object.insert("factorySpecs".to_string(), json!(specs));
    }
    Value::Object(object)
}
