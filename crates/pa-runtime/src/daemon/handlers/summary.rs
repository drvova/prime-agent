//! `sessions.list`: the saved and open sessions, each row in the old
//! daemon's session-summary shape, so the CLI's `list` and `sessions`
//! render the runtime daemon's rows as they render the old daemon's.
//!
//! A row keeps the runtime daemon's own keys (`session`, `path`, `cwd`,
//! `open`) and carries the summary's:
//!
//! - **Identity.** `id`, `activeSessionId` and `sessionId` are all the
//!   session's id: the runtime daemon addresses a session by its durable id,
//!   where the old daemon had a worker id too. `sessionFile`, `sessionName`
//!   (its last rename, also as `name`), `firstMessage` (its first user
//!   message's text), `created` (its header's time), `modified` (its
//!   transcript's), `model` (its last model change, `{provider, id}`) and
//!   `thinkingLevel` (its last thinking-level change).
//! - **Activity.** For an open session, from its lane: `activity` is
//!   `working` while a task runs, `isStreaming` the same, `isCompacting`
//!   while the running task is a `/compact`, `isSessionActive` while one
//!   runs or waits, and `sessionActions` holds the waiting tasks' prompts by
//!   lane (`steering`: the steers, `followUps`: the rest) and their count.
//!   `attachedClients` counts the connections following the session. A
//!   saved session is idle, with none of these.
//! - **Lifecycle.** `live` once the session has a message (or, open, a
//!   running task), `draft` before.
//!
//! `{"open": true}` lists the open sessions only.
//!
//! `session.rename` answers the renamed session's row, as the old daemon's
//! rename answers its summary.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Context as _;
use pa_core::session::store::store;
use pa_types::event_log::Priority;
use serde_json::{json, Value};

use super::{transcript_rows, RuntimeHandlers};
use crate::session::SessionBuilder;

/// What an open session's row says beyond its transcript.
struct Activity {
    running: bool,
    compacting: bool,
    steering: Vec<String>,
    follow_ups: Vec<String>,
    attached: usize,
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) async fn list(&self, args: &Value) -> anyhow::Result<Value> {
        let open_only = match args.get("open") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(open)) => *open,
            Some(_) => anyhow::bail!("the `open` argument must be a boolean"),
        };
        let open: Vec<(String, PathBuf, Activity)> = self
            .open
            .paths()
            .into_iter()
            .map(|(id, path)| {
                let activity = self.activity(&id);
                (id, path, activity)
            })
            .collect();
        let session_dir = self.session_dir.clone();
        tokio::task::spawn_blocking(move || {
            let mut sessions = Vec::new();
            if !open_only {
                for header in pa_core::session::discovery::scan_session_headers(&session_dir) {
                    if open.iter().any(|(id, _, _)| *id == header.id) {
                        continue;
                    }
                    // A transcript removed since the scan is not listed.
                    if let Ok(rows) = transcript_rows(&header.path, false) {
                        sessions.push(row(&header.id, &header.path, &rows, None));
                    }
                }
            }
            for (id, path, activity) in &open {
                let rows = transcript_rows(path, true)?;
                sessions.push(row(id, path, &rows, Some(activity)));
            }
            // `spareKernels`, as on the old daemon's `list`: how many built engines wait for
            // the next sessions.
            anyhow::Ok(json!({
                "sessions": sessions,
                "spareKernels": pa_core::kernel::spare_scheme_kernels(),
            }))
        })
        .await
        .context("the session list's thread panicked")?
    }
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// What open session `id`'s row says beyond its transcript, from its
    /// lane and its followers.
    fn activity(&self, id: &str) -> Activity {
        let lane = self.runtime.lane(id);
        let (steering, follow_ups) = lane
            .queued
            .iter()
            .partition::<Vec<_>, _>(|task| task.priority == Priority::Steer);
        Activity {
            running: lane.running.is_some(),
            compacting: lane
                .running
                .as_ref()
                .is_some_and(|task| task.prompt.trim_start().starts_with("/compact")),
            steering: steering
                .into_iter()
                .map(|task| task.prompt.clone())
                .collect(),
            follow_ups: follow_ups
                .into_iter()
                .map(|task| task.prompt.clone())
                .collect(),
            attached: self.attached.clients_of(id),
        }
    }

    /// Session `id`'s row, as `sessions.list` lists it: the old daemon's
    /// session summary, which its rename answers.
    pub(super) async fn summary(
        &self,
        id: &str,
        path: PathBuf,
        open: bool,
    ) -> anyhow::Result<Value> {
        let activity = open.then(|| self.activity(id));
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let rows = transcript_rows(&path, open)?;
            anyhow::Ok(row(&id, &path, &rows, activity.as_ref()))
        })
        .await
        .context("the session summary's thread panicked")?
    }
}

/// The text of the session's first user message (the old summary's
/// `firstMessage`); empty before one.
fn first_message(rows: &[Value]) -> String {
    rows.iter()
        .filter(|row| row["type"] == "message" && row["message"]["role"] == "user")
        .find_map(|row| match &row["message"]["content"] {
            Value::String(text) => Some(text.clone()),
            Value::Array(blocks) => blocks
                .iter()
                .find(|block| block["type"] == "text")
                .and_then(|block| block["text"].as_str())
                .map(str::to_string),
            _ => None,
        })
        .unwrap_or_default()
}

/// The row of session `id` (transcript `path`, its `rows` as stored), open
/// when it has an `activity`.
fn row(id: &str, path: &Path, rows: &[Value], activity: Option<&Activity>) -> Value {
    let header = rows.first().filter(|row| row["type"] == "session");
    let last = |kind: &str| rows.iter().rev().find(|row| row["type"] == kind);
    let messages = rows.iter().filter(|row| row["type"] == "message").count();
    let modified = store()
        .stat(path)
        .ok()
        .and_then(|stat| stat.modified)
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .map(pa_core::session::manager::format_iso);
    let running = activity.is_some_and(|activity| activity.running);
    let queued = activity.map_or(0, |activity| {
        activity.steering.len() + activity.follow_ups.len()
    });
    json!({
        "session": id,
        "path": path,
        "cwd": header.and_then(|header| header.get("cwd")).cloned().unwrap_or(json!("")),
        "open": activity.is_some(),
        "id": id,
        "activeSessionId": id,
        "sessionId": id,
        "sessionFile": path,
        "sessionName": last("session_info").and_then(|row| row.get("name")).cloned(),
        "name": last("session_info").and_then(|row| row.get("name")).cloned(),
        "firstMessage": first_message(rows),
        "created": header.and_then(|header| header.get("timestamp")).cloned(),
        "modified": modified,
        "model": last("model_change").map(|row| json!({
            "provider": row.get("provider"),
            "id": row.get("modelId"),
        })),
        "thinkingLevel": last("thinking_level_change")
            .and_then(|row| row.get("thinkingLevel"))
            .cloned(),
        "lifecycle": if messages > 0 || running { "live" } else { "draft" },
        "activity": if running { "working" } else { "idle" },
        "isSessionActive": running || queued > 0,
        "isStreaming": running,
        "isCompacting": activity.is_some_and(|activity| activity.compacting),
        "attachedClients": activity.map_or(0, |activity| activity.attached),
        "messageCount": messages,
        "sessionActions": {
            "queuedCount": queued,
            "steering": activity.map_or_else(Vec::new, |activity| activity.steering.clone()),
            "followUps": activity.map_or_else(Vec::new, |activity| activity.follow_ups.clone()),
        },
    })
}
