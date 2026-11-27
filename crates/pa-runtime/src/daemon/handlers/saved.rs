//! `sessions.list {shape: "saved"}`: the old daemon's `list_saved_sessions`
//! on the runtime daemon, ported from its supervisor
//! (`pa-daemon` `supervisor/sessions.rs` `handle_saved_session_list`) over the
//! catalog both daemons share (`pa_core::session::catalog`), so the agents
//! view reads the runtime daemon's catalog as it reads the old one's
//! (pa-tui's `wire2_compat` maps the command here).
//!
//! Arguments, as the old command's: `activeSessionId` (the session-addressed
//! form: an open session's cwd and sessions dir), else `cwd` (required) and
//! `sessionDir` (tilde expanded; the daemon's sessions dir without one);
//! `scope: "current"` keeps the rows of that cwd only; `limit` folds at most
//! that many files; `cursor` (a previous reply's `nextCursor`) resumes the
//! walk. `stream` names the request on its live-only topic
//! `runtime/session_list/<stream>`, which carries, while the store is walked:
//!
//! - one `{id, command, session[, activeSessionId]}` per row as its file
//!   folds (the old `session_list_item` without its `type`), each followed
//!   by `{id, command, loaded, total[, activeSessionId]}` (the old
//!   `session_list_progress`), `loaded` counting the files walked;
//! - once walked, a last `session_list_progress` with `loaded == total`.
//!
//! The walk never waits for a slow reader: a row the topic cannot take at
//! once loses its progress first, then itself, as the old daemon's did. The
//! reply is the old response's `data`, the authoritative catalog: `sessions`
//! (each row with its deleted descendants' spend, `deletedDescendantUsage`),
//! `remaining` (the files after the walk) and `nextCursor` while any remain.
//! Events and the reply travel separately, so a client takes the reply as
//! the catalog and can drop a row that reaches it after.
//!
//! Each row carries its session's live persistent shells (`persistentShells`)
//! and, while the factory lane is on, its stored factory specs
//! (`factorySpecs`), as the old rows do (8d-4).
//!
//! Not served yet: subagents kept only in the spawn ledger (the old daemon's
//! passive walk).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context as _;
use pa_core::session::catalog::{list_sessions_with, saved_session_row, SessionPage};
use pa_core::session_engine::kernel_activity::{count_factory_specs, factory_lane_enabled};
use pa_types::event_log::{NewEvent, Topic, Trust};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{optional_string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;

/// The old command the rows stand for.
const COMMAND: &str = "list_saved_sessions";

/// How many rows wait for the topic before the walk skips some.
const STREAM_CAPACITY: usize = 256;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    #[expect(
        clippy::too_many_lines,
        reason = "the old handler's walk, merge and reply, kept in its order"
    )]
    pub(super) async fn saved_list(&self, args: &Value) -> anyhow::Result<Value> {
        let active_session_id = optional_string_arg(args, "activeSessionId")?;
        let stream = optional_string_arg(args, "stream")?
            .map(|stream| {
                Topic::new(format!("runtime/session_list/{stream}"))
                    .with_context(|| format!("{stream:?} cannot name a session list stream"))
            })
            .transpose()?;
        let limit = match args.get("limit") {
            None | Some(Value::Null) => None,
            Some(limit) => Some(
                limit
                    .as_u64()
                    .and_then(|limit| usize::try_from(limit).ok())
                    .context("the `limit` argument must be a count")?,
            ),
        };
        let cursor = optional_string_arg(args, "cursor")?;
        let scope_current = args.get("scope").and_then(Value::as_str) == Some("current");
        // Session-addressed form: the open session's cwd and sessions dir.
        let (cwd, dir) = if let Some(active) = &active_session_id {
            let path = self
                .open
                .path(active)
                .with_context(|| format!("Unknown active session: {active}"))?;
            let header_path = path.clone();
            let header = tokio::task::spawn_blocking(move || {
                pa_core::session::manager::read_session_header(&header_path)
            })
            .await
            .context("the header's thread panicked")?;
            let cwd = header.map_or_else(|| self.cwd.display().to_string(), |header| header.cwd);
            let dir = path
                .parent()
                .map_or_else(|| self.session_dir.clone(), PathBuf::from);
            (cwd, dir)
        } else {
            // The TS supervisor runs Node's path.resolve on the missing cwd;
            // the old daemon reproduced the observable error string.
            let cwd = optional_string_arg(args, "cwd")?
                .context("The \"paths[0]\" property must be of type string, got undefined")?;
            let dir = match optional_string_arg(args, "sessionDir")? {
                Some(dir) => expand_tilde(&dir)?,
                None => self.session_dir.clone(),
            };
            (cwd, dir)
        };
        let request = Request {
            id: args.get("stream").cloned().unwrap_or(Value::Null),
            active_session_id: active_session_id.clone(),
        };
        let (rows, mut queued) = mpsc::channel::<Vec<Value>>(STREAM_CAPACITY);
        let streaming = stream.is_some();
        let (scan_cwd, scan_dir) = (cwd.clone(), dir.clone());
        let scan_request = request.clone();
        // The fold runs on the blocking pool, so a grown store never holds
        // up a runtime worker; rows reach the topic while it walks.
        let agent_dir = self.agent_dir.clone();
        let scan = tokio::task::spawn_blocking(move || {
            // Each row lists its session's live persistent shells.
            let persistent_shells =
                pa_core::kernel::persistent_shells::live_shell_rows_by_session(&agent_dir);
            // The factory is opt-in: while it stays off, no row counts stored specs.
            let factory_lane = factory_lane_enabled(&agent_dir);
            let mut factory_specs = HashMap::new();
            let mut file_total = 0usize;
            let page =
                list_sessions_with(&scan_dir, cursor.as_deref(), limit, |index, total, info| {
                    file_total = total;
                    if scope_current && info.cwd != scan_cwd {
                        // The row is out of scope, but the scan itself goes on.
                        return true;
                    }
                    if factory_lane {
                        count_factory_specs(info, &mut factory_specs);
                    }
                    if !streaming {
                        return true;
                    }
                    let row = saved_session_row(info, &persistent_shells, &factory_specs);
                    // A closed topic is the request's end: the scan stops. A
                    // FULL one skips the PROGRESS frame first, then the row.
                    let mut bundle = vec![
                        scan_request.frame(json!({ "session": row })),
                        scan_request.progress(index + 1, total),
                    ];
                    loop {
                        match rows.try_send(bundle) {
                            Ok(()) => break true,
                            Err(mpsc::error::TrySendError::Full(mut unsent)) => {
                                if unsent.len() > 1 {
                                    unsent.pop();
                                    bundle = unsent;
                                    continue;
                                }
                                break true;
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break false,
                        }
                    }
                });
            (page, file_total, persistent_shells, factory_specs)
        });
        let publish = async {
            while let Some(frames) = queued.recv().await {
                if let Some(topic) = &stream {
                    for payload in frames {
                        self.publish_list_frame(topic, payload).await;
                    }
                }
            }
        };
        let (scanned, ()) = tokio::join!(scan, publish);
        let (page, file_total, persistent_shells, factory_specs) =
            scanned.context("the saved-session scan failed")?;
        // The scan's parse trees folded and freed inside the blocking task;
        // return their arena high-water to the OS at the phase boundary.
        pa_types::memory_release::trim_freed_heap();
        let SessionPage {
            mut infos,
            remaining,
            next_cursor,
        } = page;
        // The current-cwd scope keeps only the session's own rows in the
        // reply: the reply is the authoritative catalog.
        if scope_current {
            infos.retain(|info| info.cwd == cwd);
        }
        // Every row carries its tombstoned descendants' spend, attached by
        // canonical parent path so the rollup bills deleted subagents to the
        // parent that spent them.
        if let Some(family) = self.runtime.subagents().daemon() {
            match family.deleted_descendant_usage().await {
                Ok(bucket) => {
                    for info in &mut infos {
                        let path = pa_core::session::lease::canonical_session_path(&info.path)
                            .to_string_lossy()
                            .to_string();
                        info.deleted_descendant_usage = bucket.get(&path).cloned();
                    }
                }
                Err(error) => {
                    tracing::warn!("could not attach deleted-descendant usage: {error:#}");
                }
            }
        }
        // The scan's completion marker: the progress counts the files walked
        // while rows stream only for valid ones, so the last progress can
        // land short of the total; a reader waiting for `loaded == total`
        // sees completion on this frame.
        if let Some(topic) = &stream {
            if file_total > 0 {
                self.publish_list_frame(topic, request.progress(file_total, file_total))
                    .await;
            }
        }
        let sessions: Vec<Value> = infos
            .iter()
            .map(|info| saved_session_row(info, &persistent_shells, &factory_specs))
            .collect();
        let mut data = json!({ "sessions": sessions, "remaining": remaining });
        if let Some(next_cursor) = next_cursor {
            data["nextCursor"] = json!(next_cursor);
        }
        Ok(data)
    }

    /// Publishes one of a saved-session list's frames on its `topic`.
    async fn publish_list_frame(&self, topic: &Topic, payload: Value) {
        let published = self
            .runtime
            .publish(NewEvent {
                topic: topic.clone(),
                source: "runtime".to_string(),
                session: None,
                payload,
                trust: Trust::Owner,
                cause: None,
                reply: None,
            })
            .await;
        if let Err(error) = published {
            tracing::warn!("a saved-session list frame was not published: {error:#}");
        }
    }
}

/// What every frame of one list request repeats: its `id` (the stream's
/// name) and, session-addressed, its `activeSessionId`.
#[derive(Clone)]
struct Request {
    id: Value,
    active_session_id: Option<String>,
}

impl Request {
    /// `body` as one of the request's frames.
    fn frame(&self, mut body: Value) -> Value {
        body["id"] = self.id.clone();
        body["command"] = json!(COMMAND);
        if let Some(active) = &self.active_session_id {
            body["activeSessionId"] = json!(active);
        }
        body
    }

    /// The old `session_list_progress` (without its `type`).
    fn progress(&self, loaded: usize, total: usize) -> Value {
        self.frame(json!({ "loaded": loaded, "total": total }))
    }
}

/// `path` with a leading `~` or `~/` read as the home directory, as the old
/// daemon read a `sessionDir`.
fn expand_tilde(path: &str) -> anyhow::Result<PathBuf> {
    let home = || pa_types::platform::dirs::home_dir().context("no home directory to expand ~ in");
    if let Some(rest) = path.strip_prefix("~/") {
        Ok(home()?.join(rest))
    } else if path == "~" {
        home()
    } else {
        Ok(PathBuf::from(path))
    }
}
