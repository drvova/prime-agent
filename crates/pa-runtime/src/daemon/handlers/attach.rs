//! The old daemon's attach, `get_state` and `get_messages` replies on the
//! runtime daemon, ported from its worker (`pa-daemon` `worker/connection.rs`
//! `handle_attach`, `worker/commands.rs` `handle_get_state` /
//! `handle_get_messages`, `worker/summary.rs`) so a client of the old wire
//! reads them unchanged (pa-tui's `wire2_compat` maps the commands here):
//!
//! - `session.snapshot {session, shape: "attach", clientId?, capabilities?,
//!   resumeCursor?}`: the attach result. `snapshot` holds the session's
//!   summary, its connection state, the messages window, `lastEventSequence`
//!   and `lastEventCursor` (the stream's generation and counter), and
//!   `children` (empty: the roster is its own feed); the top level repeats
//!   the cursor with `replay`, and, unless the client is `slim_attach`, the
//!   summary as `state` and the messages. A `windowed_snapshot` client gets
//!   the newest 512 display rows and, when older ones exist,
//!   `historyBeforeEntryId` (the window's first row) and `lastUserPromptMs`;
//!   any other client the context's messages. An `elide_snapshot_images`
//!   client gets image payloads elided. The transcript always comes in the
//!   reply: the chunked `session_snapshot_*` transfer is a client capability
//!   (`chunked_snapshot`) this reply does not offer, as the old worker's own
//!   reply did not.
//! - `session.snapshot {session, shape: "state"}`: the summary (`get_state`).
//! - `transcript.page {session, shape: "messages", page?, historyLeafId?,
//!   capabilities?}`: without `page`, the context's messages; with one (the
//!   old `TranscriptPage`, an end or a start page), the display rows it
//!   names with `firstEntryId`, `lastEntryId`, `more` and `historyLeafId`.
//!   An anchor or a `historyLeafId` off the active branch is the old
//!   refusal, "History branch changed".
//!
//! The rows come from the transcript as stored and the active branch from
//! the session's leaf, so a read never waits on a running turn. What the old
//! summary took from its worker process is the daemon's here: `workerPid` is
//! the daemon's pid (sessions run in it) and `workerState` is `ready`; a
//! worker instance id, a model fallback notice and a roster-delta counter
//! have no counterpart (`rosterDeltaSequence` is 0, the others are absent).

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::Context as _;
use pa_core::session::display_transcript::{
    compaction_summary_message, continues_run, custom_message_row, elide_image_payloads,
    prompt_timestamp_ms, settle_cut, DisplayRow,
};
use pa_core::session::store::store;
use pa_core::settings::QueueModeSetting;
use pa_types::daemon::{DaemonResumeCursor, TranscriptPage};
use serde_json::{json, Map, Value};

use super::{transcript_rows, RuntimeHandlers};
use crate::session::SessionBuilder;

/// The display rows a windowed attach carries (the old worker's
/// `ATTACH_TAIL_MESSAGE_BUDGET`).
const ATTACH_TAIL_MESSAGE_BUDGET: usize = 512;

/// The old refusal of a page whose anchor left the active branch.
const BRANCH_CHANGED: &str = "History branch changed";

/// One stored row of the active branch: its kind, id, time, and the rest of
/// its members.
struct BranchRow {
    kind: String,
    id: String,
    timestamp: String,
    fields: Value,
}

impl BranchRow {
    /// The row's wire message, when it carries one (a `custom_message` row
    /// as `role: "custom"`).
    fn message(&self) -> Option<Cow<'_, Value>> {
        match self.kind.as_str() {
            "message" => self.fields.get("message").map(Cow::Borrowed),
            "custom_message" => Some(Cow::Owned(custom_message_row(
                &self.fields,
                &self.timestamp,
            ))),
            _ => None,
        }
    }

    /// The compaction row's summary message.
    fn summary(&self, retained_count: Option<usize>) -> Value {
        compaction_summary_message(&self.fields, iso_ms(&self.timestamp), retained_count)
    }

    /// The row as a display-transcript row: a message-bearing row, or a
    /// compaction as its summary in place.
    fn display(&self) -> Option<DisplayRow<'_>> {
        let message = if self.kind == "compaction" {
            Cow::Owned(self.summary(None))
        } else {
            self.message()?
        };
        Some((self.id.as_str(), message))
    }
}

/// An ISO time's epoch milliseconds (0 when it is none).
fn iso_ms(iso: &str) -> u64 {
    pa_types::incident::timestamp_to_ms(iso)
        .and_then(|ms| u64::try_from(ms).ok())
        .unwrap_or(0)
}

/// What the attach reads of the session's transcript: its rows as stored
/// and the active branch through `leaf`.
pub(super) struct Transcript {
    rows: Vec<Value>,
    /// The active branch's rows, root first.
    branch: Vec<BranchRow>,
    leaf: Option<String>,
}

impl Transcript {
    /// Reads the transcript at `path`; the branch runs back from `leaf`
    /// (the session's own leaf; the last row's when it has none).
    pub(super) fn read(
        path: &std::path::Path,
        open: bool,
        leaf: Option<String>,
    ) -> anyhow::Result<Self> {
        let rows = transcript_rows(path, open)?;
        let leaf = leaf.or_else(|| {
            rows.last()
                .filter(|row| row["type"] != "session")
                .and_then(|row| row["id"].as_str())
                .map(str::to_string)
        });
        let by_id: std::collections::HashMap<&str, &Value> = rows
            .iter()
            .filter_map(|row| Some((row["id"].as_str()?, row)))
            .collect();
        let mut branch = Vec::new();
        let mut next = leaf.as_deref().and_then(|id| by_id.get(id).copied());
        while let Some(row) = next {
            if row["type"] == "session" {
                break;
            }
            let mut fields = row.clone();
            if let Some(object) = fields.as_object_mut() {
                for key in ["type", "id", "parentId", "timestamp"] {
                    object.remove(key);
                }
            }
            branch.push(BranchRow {
                kind: row["type"].as_str().unwrap_or_default().to_string(),
                id: row["id"].as_str().unwrap_or_default().to_string(),
                timestamp: row["timestamp"].as_str().unwrap_or_default().to_string(),
                fields,
            });
            // A cycle in a damaged transcript ends the walk.
            if branch.len() > rows.len() {
                break;
            }
            next = row["parentId"]
                .as_str()
                .and_then(|id| by_id.get(id).copied());
        }
        branch.reverse();
        Ok(Transcript { rows, branch, leaf })
    }

    /// The branch's conversation, compacted view first (the old store's
    /// `messages`): a `compactionSummary` message with the retained count,
    /// the retained messages from `firstKeptEntryId`, then everything
    /// appended after; without a compaction, the plain message list.
    pub(super) fn context_messages(&self) -> Vec<Value> {
        let Some(at) = self.branch.iter().rposition(|row| row.kind == "compaction") else {
            return self
                .branch
                .iter()
                .filter_map(|row| row.message().map(Cow::into_owned))
                .collect();
        };
        let compaction = &self.branch[at];
        let first_kept = compaction.fields["firstKeptEntryId"]
            .as_str()
            .unwrap_or_default();
        // The durable cut can name bookkeeping, not just a message row.
        let kept: Vec<Value> = self.branch[..at]
            .iter()
            .skip_while(|row| row.id != first_kept)
            .filter_map(|row| row.message().map(Cow::into_owned))
            .collect();
        let mut messages = vec![compaction.summary(Some(kept.len()))];
        messages.extend(kept);
        messages.extend(
            self.branch[at + 1..]
                .iter()
                .filter_map(|row| row.message().map(Cow::into_owned)),
        );
        messages
    }

    /// The newest user prompt's time on the branch (the attach's resume
    /// marker).
    fn last_user_prompt_ms(&self) -> Option<u64> {
        self.branch
            .iter()
            .rev()
            .filter(|row| row.kind == "message")
            .find_map(|row| row.fields.get("message").filter(|m| m["role"] == "user"))
            .and_then(prompt_timestamp_ms)
    }

    /// The transcript's `message` rows (the old store's `message_count`).
    fn message_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row["type"] == "message")
            .count()
    }
}

/// One page of the display transcript (the old store's `transcript_page`):
/// its messages, first and last row ids and whether rows lie past its far
/// side; `None` when an anchor is not a display row on the branch.
fn page(
    display: &[DisplayRow<'_>],
    page: &TranscriptPage,
) -> Option<(Vec<Value>, Value, Value, bool)> {
    let anchor = |id: &Option<String>| -> Option<Option<usize>> {
        match id {
            None => Some(None),
            Some(id) => display.iter().position(|(row, _)| row == id).map(Some),
        }
    };
    let (range, more) = match page {
        TranscriptPage::End { before, limit } => end_page(display, anchor(before)?, *limit),
        TranscriptPage::Start {
            after,
            until,
            limit,
        } => start_page(display, anchor(after)?, anchor(until)?, *limit),
    };
    let rows = &display[range];
    Some((
        rows.iter()
            .map(|(_, message)| message.clone().into_owned())
            .collect(),
        rows.first().map_or(Value::Null, |(id, _)| json!(id)),
        rows.last().map_or(Value::Null, |(id, _)| json!(id)),
        more,
    ))
}

/// The newest `limit` rows before the `before` row (the tail without it),
/// cut where no tool call is split from its result.
fn end_page(
    display: &[DisplayRow<'_>],
    before: Option<usize>,
    limit: usize,
) -> (std::ops::Range<usize>, bool) {
    let end = before.unwrap_or(display.len());
    // The page, then the run that moves its cut back, then the rows the
    // cut's tool calls may pair with.
    let mut low = end.saturating_sub(limit);
    while low > 0 && low < end && continues_run(&display[low].1) {
        low -= 1;
    }
    let cut_row = low;
    let low = low.saturating_sub(limit);
    let slice = &display[low..before.map_or(end, |before| before + 1)];
    let start = settle_cut(slice, cut_row - low);
    ((low + start).min(end)..end, low + start > 0)
}

/// The oldest `limit` rows after the `after` row (the head without it),
/// stopping before the `until` row, cut where no tool call is split from
/// its result.
fn start_page(
    display: &[DisplayRow<'_>],
    after: Option<usize>,
    until: Option<usize>,
    limit: usize,
) -> (std::ops::Range<usize>, bool) {
    let first = after.map_or(0, |after| after + 1);
    let Some(last) = until.or_else(|| display.len().checked_sub(1)) else {
        return (0..0, false);
    };
    let through = (last + 1).max(first);
    let taken = (through - first).min(limit.saturating_mul(2).saturating_add(1));
    let reached_until = taken > 0 && until == Some(first + taken - 1);
    let available = taken - usize::from(reached_until);
    if available <= limit {
        return (first..first + available, false);
    }
    if limit == 0 {
        return (first..first, true);
    }
    // The `limit` rows before the page, for calls whose results the cut
    // would split.
    let low = after.map_or(first, |after| (after + 1).saturating_sub(limit));
    let slice = &display[low..first + taken];
    let cut = settle_cut(slice, first - low + limit);
    if cut > first - low {
        return (first..low + cut, true);
    }
    // The cut fell back to the page start: the page runs to the `until` row.
    (first..through - usize::from(until.is_some()), false)
}

/// A client's capability list from `args[key]` (strings only).
fn string_list(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

/// The wire name of a queue mode setting.
fn queue_mode(mode: QueueModeSetting) -> &'static str {
    match mode {
        QueueModeSetting::All => "all",
        QueueModeSetting::OneAtATime => "one-at-a-time",
    }
}

/// A message's text: its text blocks joined with spaces (the old summary's
/// `firstMessage`).
fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// What the attach reads from an open session's engine, on its runtime.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent readings of one engine, taken together under its lock"
)]
struct EngineState {
    model: pa_agent::types::Model,
    thinking_level: Value,
    streaming: bool,
    running_tools: bool,
    leaf: Option<String>,
    session_file: Option<PathBuf>,
    session_name: Option<String>,
    session_dir: PathBuf,
    cwd: PathBuf,
    header: Option<pa_types::session::SessionHeader>,
    service_tier: Option<pa_types::ai::ServiceTier>,
    todos: pa_types::todo::TodoList,
    compactions: usize,
    warning_shown: bool,
    tool_calls: u64,
    auto_compaction: bool,
    goal: Value,
    context_usage: Option<pa_types::usage::ContextUsage>,
}

/// The old summary and connection state of one open session.
pub(super) struct Composed {
    pub(super) summary: Value,
    state: Value,
    transcript: Transcript,
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `session.snapshot` with a `shape` (see the module documentation).
    pub(super) async fn shaped_snapshot(&self, args: &Value, shape: &str) -> anyhow::Result<Value> {
        match shape {
            "attach" => self.attach_snapshot(args).await,
            "state" => Ok(self.compose(args).await?.summary),
            other => anyhow::bail!("session.snapshot has no shape {other:?} (attach, state)"),
        }
    }

    /// Reads open session `args.session`'s engine and transcript into the
    /// old summary and connection state.
    async fn compose(&self, args: &Value) -> anyhow::Result<Composed> {
        let id = self.open_session(args)?;
        self.compose_session(&id).await
    }

    /// [`Self::compose`] of session `id`: an open session, or a subagent
    /// below one, whose summary also names its parent and its handle
    /// (`runtimeKind: "subagent"`, the old child worker's).
    #[expect(
        clippy::too_many_lines,
        reason = "the summary and the state are composed key by key in the old wire's order"
    )]
    pub(super) async fn compose_session(&self, id: &str) -> anyhow::Result<Composed> {
        let id = id.to_string();
        let lineage = self.runtime.subagents().lineage(&id);
        let path = match (self.open.path(&id), &lineage) {
            (Some(path), _) => path,
            (None, Some(lineage)) => lineage
                .session_file
                .as_ref()
                .map(PathBuf::from)
                .with_context(|| format!("subagent {id} has no transcript"))?,
            (None, None) => anyhow::bail!("session {id} is not open in the runtime daemon"),
        };
        let live = self.runtime.built_engine(&id).await?;
        let engine = std::sync::Arc::clone(&live.engine);
        let read = live
            .runtime
            .spawn(async move {
                let agent = engine.session.agent();
                let agent_state = agent.state().await;
                let persistence = engine.session.shared_persistence();
                let manager = persistence.lock().await;
                let context = manager.active_context();
                let warning_shown = manager.anthropic_warning_shown();
                let context_usage = pa_core::session_engine::wire_events::settled_context_usage(
                    &agent_state.messages,
                    agent_state.model.context_window,
                );
                let state = EngineState {
                    thinking_level: serde_json::to_value(agent_state.thinking_level)
                        .unwrap_or(json!("off")),
                    streaming: agent_state.is_streaming,
                    running_tools: agent_state.is_streaming
                        && !agent_state.pending_tool_calls.is_empty(),
                    model: agent_state.model,
                    leaf: manager.get_leaf_id().map(str::to_string),
                    session_file: manager.get_session_file().map(std::path::Path::to_path_buf),
                    session_name: manager.get_session_name(),
                    session_dir: manager.get_session_dir().to_path_buf(),
                    cwd: manager.get_cwd().to_path_buf(),
                    header: manager.get_header().cloned(),
                    service_tier: context.service_tier,
                    todos: manager.active_todo_list().unwrap_or_default(),
                    compactions: manager
                        .get_all_entries()
                        .iter()
                        .filter(|entry| {
                            matches!(entry, pa_types::session::FileEntry::Compaction { .. })
                        })
                        .count(),
                    warning_shown,
                    tool_calls: crate::subagents::branch_tool_calls(&manager),
                    auto_compaction: engine.session.auto_compaction_enabled(),
                    goal: Value::Null,
                    context_usage,
                };
                drop(manager);
                // The goal driver after the persistence guard: goal changes
                // take the driver first.
                let goal = serde_json::to_value(
                    engine
                        .goal_driver
                        .lock()
                        .await
                        .state_with_creation_elapsed(),
                )
                .unwrap_or(Value::Null);
                EngineState { goal, ..state }
            })
            .await
            .context("the session's runtime ended during the read")?;
        let model = self.full_model(&read.model).await?;
        // The modes the session's next turn takes from the settings, as a
        // runtime daemon's session does at each turn's start.
        let (steering_mode, follow_up_mode) = {
            let (cwd, agent_dir) = (read.cwd.clone(), self.agent_dir.clone());
            tokio::task::spawn_blocking(move || {
                let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
                (settings.get_steering_mode(), settings.get_follow_up_mode())
            })
            .await
            .context("the settings' thread panicked")?
        };
        let levels: Vec<&str> = pa_types::ai::get_supported_thinking_levels(&model)
            .into_iter()
            .map(pa_types::ai::ModelThinkingLevel::wire_name)
            .collect();
        let tier = pa_types::ai::clamp_service_tier(Some(&model), read.service_tier)
            .unwrap_or(pa_types::ai::ServiceTier::Auto);
        let model_metadata = json!({
            "id": model.id,
            "name": model.name,
            "api": model.api,
            "provider": model.provider,
            "reasoning": model.reasoning,
        });
        let ((_, _), actions) = self
            .runtime
            .stream_view(&id)
            .context("the session closed")?;
        let lane = self.runtime.lane(&id);
        let compacting = lane
            .running
            .as_ref()
            .is_some_and(|task| task.prompt.trim_start().starts_with("/compact"));
        let bash_running = self.open.bashes.running(&id);
        let subagents_running = self
            .runtime
            .subagents()
            .roster(&id)
            .await
            .iter()
            .any(|child| child["status"] == "running");
        let queued = actions["queuedCount"].as_u64().unwrap_or_default();
        let attached = self.attached.clients_of(&id);
        // The usage is the catalog's own-usage fold of the transcript (the
        // old worker's summary read the same fold), with its billing split.
        let (transcript, usage) = {
            let (path, leaf) = (path.clone(), read.leaf.clone());
            tokio::task::spawn_blocking(move || {
                let usage =
                    pa_core::session::catalog::read_session_info(&path).and_then(|info| info.usage);
                Transcript::read(&path, true, leaf).map(|transcript| (transcript, usage))
            })
            .await
            .context("the transcript's thread panicked")??
        };
        let messages = transcript.context_messages();
        let created = read.header.as_ref().map(|header| header.timestamp.clone());
        // The transcript's time; before its first row is stored, the
        // session's creation (the old worker wrote its file's header at
        // create, so its `modified` was that time until the first turn).
        let modified = store()
            .stat(&path)
            .ok()
            .and_then(|stat| stat.modified)
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_millis()).ok())
            .map(pa_core::session::manager::format_iso)
            .or_else(|| created.clone());
        let last_activity_at = messages
            .iter()
            .filter_map(|message| message["timestamp"].as_u64())
            .next_back()
            .and_then(|ms| i64::try_from(ms).ok())
            .map(pa_core::session::manager::format_iso)
            .or_else(|| modified.clone())
            .or_else(|| created.clone());
        let first_message = transcript
            .rows
            .iter()
            .filter(|row| row["type"] == "message" && row["message"]["role"] == "user")
            .map(|row| message_text(&row["message"]))
            .find(|text| !text.is_empty());
        let message_count = transcript.message_count();
        let session_file = read.session_file.as_ref().map_or_else(
            || path.display().to_string(),
            |file| file.display().to_string(),
        );
        let cwd = read.cwd.display().to_string();
        let mut summary = Map::new();
        let mut put = |key: &str, value: Value| {
            summary.insert(key.to_string(), value);
        };
        put("id", json!(id));
        put(
            "lifecycle",
            json!(if message_count > 0 || read.streaming {
                "live"
            } else {
                "draft"
            }),
        );
        put(
            "activity",
            json!(if read.streaming || compacting || subagents_running {
                "working"
            } else {
                "idle"
            }),
        );
        put(
            "isSessionActive",
            json!(read.streaming || compacting || queued > 0),
        );
        put("hasRegisteredCronJob", json!(false));
        if let Some(at) = &last_activity_at {
            put("lastActivityAt", json!(at));
        }
        put(
            "rlmDepth",
            json!(lineage.as_ref().map_or_else(
                || read
                    .header
                    .as_ref()
                    .and_then(|header| header.rlm_depth)
                    .unwrap_or(0),
                |lineage| u64::from(lineage.depth)
            )),
        );
        put("activeSessionId", json!(id));
        put("sessionId", json!(id));
        put("sessionFile", json!(session_file));
        if let Some(name) = &read.session_name {
            put("sessionName", json!(name));
        }
        put("cwd", json!(cwd));
        put("thinkingLevel", read.thinking_level.clone());
        put("isStreaming", json!(read.streaming));
        put("isCompacting", json!(compacting));
        put("isBashRunning", json!(bash_running));
        put("isRunningTools", json!(read.running_tools));
        put("hasRunningSubagents", json!(subagents_running));
        put("attachedClients", json!(attached));
        put("messageCount", json!(message_count));
        put("sessionActions", actions.clone());
        if let Some(created) = &created {
            put("created", json!(created));
        }
        if let Some(modified) = &modified {
            put("modified", json!(modified));
        }
        if let Some(first) = &first_message {
            put("firstMessage", json!(first));
        }
        if read.tool_calls > 0 {
            put("toolUseCount", json!(read.tool_calls));
        }
        if let Some(lineage) = &lineage {
            put("parentSessionPath", json!(lineage.parent_file));
            put("parentActiveSessionId", json!(lineage.parent_id));
            put("parentSessionId", json!(lineage.parent_id));
            put("rlmChildId", json!(lineage.child_id));
        }
        if let Some(usage) = &usage {
            put("usage", json!(usage));
        }
        put("workerState", json!("ready"));
        put("workerPid", json!(std::process::id()));
        put("rosterDeltaSequence", json!(0));
        put("model", model_metadata.clone());
        put(
            "runtimeKind",
            json!(if lineage.is_some() {
                "subagent"
            } else {
                "top-level"
            }),
        );
        put("unfinishedActionCount", json!(0));
        put("anthropicWarningShown", json!(read.warning_shown));
        let summary = Value::Object(summary);
        let mut state = json!({
            "activeSessionId": id,
            "cwd": cwd,
            "model": model_metadata,
            "thinkingLevel": read.thinking_level,
            "serviceTier": tier,
            "availableThinkingLevels": levels,
            "isStreaming": read.streaming,
            "isCompacting": compacting,
            "isBashRunning": bash_running,
            "retryAttempt": 0,
            "steeringMode": queue_mode(steering_mode),
            "followUpMode": queue_mode(follow_up_mode),
            "sessionFile": session_file,
            "sessionId": id,
        });
        if let Some(name) = &read.session_name {
            state["sessionName"] = json!(name);
        }
        state["sessionDir"] = json!(read.session_dir.display().to_string());
        state["leafId"] = json!(transcript.leaf);
        state["autoCompactionEnabled"] = json!(read.auto_compaction);
        state["messageCount"] = json!(message_count);
        state["sessionActions"] = actions;
        state["compactionCount"] = json!(read.compactions);
        state["goal"] = read.goal;
        state["todos"] = serde_json::to_value(&read.todos)?;
        state["scopedModels"] = json!(self.open.scoped_models(&id));
        state["activeToolNames"] = json!([]);
        if let Some(usage) = read.context_usage {
            state["contextUsage"] = serde_json::to_value(usage)?;
        }
        Ok(Composed {
            summary,
            state,
            transcript,
        })
    }

    /// `session.snapshot {shape: "attach"}`: the old attach result.
    #[expect(
        clippy::too_many_lines,
        reason = "the attach result is built key by key in the old wire's order"
    )]
    async fn attach_snapshot(&self, args: &Value) -> anyhow::Result<Value> {
        let client_id = args
            .get("clientId")
            .and_then(Value::as_str)
            .unwrap_or("anonymous")
            .to_string();
        // The old wire's capability set: the supported ones, or its default
        // pair when the client names none.
        let supported = [
            "attach_snapshot",
            "event_sequence",
            "slim_attach",
            "chunked_snapshot",
            "client_owned_sessions",
            "elide_snapshot_images",
            "windowed_snapshot",
            pa_types::daemon::message_delta::MESSAGE_UPDATE_DELTA_CAPABILITY,
            pa_types::question::QUESTIONS_CAPABILITY,
        ];
        let capabilities: Vec<String> = string_list(args, "capabilities").map_or_else(
            || vec!["attach_snapshot".to_string(), "event_sequence".to_string()],
            |named| {
                named
                    .into_iter()
                    .filter(|capability| supported.contains(&capability.as_str()))
                    .collect()
            },
        );
        // A routed attach carries the client's own set as
        // `clientCapabilities`: the window and the elision follow it.
        let client_capabilities =
            string_list(args, "clientCapabilities").unwrap_or_else(|| capabilities.clone());
        let has = |name: &str| {
            client_capabilities
                .iter()
                .any(|capability| capability == name)
        };
        let resume: Option<DaemonResumeCursor> = args
            .get("resumeCursor")
            .filter(|cursor| !cursor.is_null())
            .and_then(|cursor| serde_json::from_value(cursor.clone()).ok());
        let id = self.open_session(args)?;
        let composed = self.compose(args).await?;
        let ((generation, sequence), _) = self
            .runtime
            .stream_view(&id)
            .context("the session closed")?;
        let display: Vec<DisplayRow<'_>> = composed
            .transcript
            .branch
            .iter()
            .filter_map(BranchRow::display)
            .collect();
        let (mut messages, history_before, last_user_prompt_ms) = if has("windowed_snapshot") {
            let (messages, first, _, more) = page(
                &display,
                &TranscriptPage::End {
                    before: None,
                    limit: ATTACH_TAIL_MESSAGE_BUDGET,
                },
            )
            .unwrap_or_default();
            // The resume marker only rides a snapshot with older history.
            (
                messages,
                first.as_str().filter(|_| more).map(str::to_string),
                more.then(|| composed.transcript.last_user_prompt_ms())
                    .flatten(),
            )
        } else {
            (composed.transcript.context_messages(), None, None)
        };
        if has("elide_snapshot_images") {
            elide_image_payloads(&mut messages);
        }
        let cursor = json!({ "generation": generation, "sequence": sequence });
        let replay = pa_types::daemon::replay_info(resume.as_ref(), sequence, &generation);
        let mut snapshot = json!({
            "activeSessionId": id,
            "summary": composed.summary,
            "state": composed.state,
            "messages": messages,
            "lastEventSequence": sequence,
            "lastEventCursor": cursor,
            "children": [],
        });
        if let Some(entry) = history_before {
            snapshot["historyBeforeEntryId"] = json!(entry);
            if let Some(ms) = last_user_prompt_ms {
                snapshot["lastUserPromptMs"] = json!(ms);
            }
        }
        let mut result = json!({
            "protocol": {
                "name": pa_types::daemon::DAEMON_PROTOCOL_NAME,
                "version": pa_types::daemon::DAEMON_PROTOCOL_VERSION,
            },
            "activeSessionId": id,
        });
        if !capabilities
            .iter()
            .any(|capability| capability == "slim_attach")
        {
            result["state"] = snapshot["summary"].clone();
            result["messages"] = snapshot["messages"].clone();
        }
        result["snapshot"] = snapshot;
        result["replay"] = serde_json::to_value(replay)?;
        result["lastEventSequence"] = json!(sequence);
        result["lastEventCursor"] = cursor;
        result["client"] = json!({
            "id": client_id,
            "capabilities": string_list(args, "clientCapabilities").unwrap_or(capabilities),
        });
        Ok(result)
    }

    /// `transcript.page {shape: "messages"}`: the old `get_messages` reply.
    pub(super) async fn messages_page(&self, args: &Value) -> anyhow::Result<Value> {
        let (id, path, open) = self.session(args)?;
        let page_args = match args.get("page") {
            None | Some(Value::Null) => None,
            Some(page) => Some(
                serde_json::from_value::<TranscriptPage>(page.clone())
                    .context("`page` is not a transcript page")?,
            ),
        };
        let leaf = if open {
            let live = self.runtime.built_engine(&id).await?;
            let engine = std::sync::Arc::clone(&live.engine);
            live.runtime
                .spawn(async move {
                    let persistence = engine.session.shared_persistence();
                    let manager = persistence.lock().await;
                    manager.get_leaf_id().map(str::to_string)
                })
                .await
                .context("the session's runtime ended during the read")?
        } else {
            None
        };
        let transcript = tokio::task::spawn_blocking(move || Transcript::read(&path, open, leaf))
            .await
            .context("the transcript's thread panicked")??;
        let elide = string_list(args, "capabilities")
            .is_some_and(|named| named.iter().any(|name| name == "elide_snapshot_images"));
        let Some(request) = page_args else {
            let mut messages = transcript.context_messages();
            if elide {
                elide_image_payloads(&mut messages);
            }
            return Ok(json!({ "messages": messages }));
        };
        // The page is read against the attached branch: appends keep its
        // leaf on the branch, navigation to another branch does not.
        let history_leaf = match args.get("historyLeafId").map(Value::as_str) {
            Some(Some(leaf)) if transcript.branch.iter().any(|row| row.id == leaf) => {
                leaf.to_string()
            }
            Some(_) => anyhow::bail!(BRANCH_CHANGED),
            None => transcript.leaf.clone().unwrap_or_default(),
        };
        let display: Vec<DisplayRow<'_>> = transcript
            .branch
            .iter()
            .filter_map(BranchRow::display)
            .collect();
        let (mut messages, first, last, more) = page(&display, &request).context(BRANCH_CHANGED)?;
        if elide {
            elide_image_payloads(&mut messages);
        }
        Ok(json!({
            "messages": messages,
            "firstEntryId": first,
            "lastEntryId": last,
            "more": more,
            "historyLeafId": history_leaf,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use pa_types::daemon::TranscriptPage;
    use serde_json::json;

    use super::page;

    /// A page never splits a tool call from its result: an end page whose cut
    /// falls on a result starts at its call, and a start page that would end
    /// on a call ends before it; a call outside the rows around the cut does
    /// not move it, and an anchor off the branch is no page.
    #[test]
    fn pages_cut_where_no_call_is_split_from_its_result() {
        let messages = [
            ("u1", json!({ "role": "user", "content": "q1" })),
            (
                "a1",
                json!({ "role": "assistant", "content": [
                { "type": "toolCall", "id": "c1", "name": "eval", "arguments": {} }] }),
            ),
            (
                "t1",
                json!({ "role": "toolResult", "toolCallId": "c1", "content": [] }),
            ),
            (
                "a2",
                json!({ "role": "assistant", "content": [{ "type": "text", "text": "done" }] }),
            ),
            ("u2", json!({ "role": "user", "content": "q2" })),
        ];
        let display: Vec<_> = messages
            .iter()
            .map(|(id, message)| (*id, Cow::Borrowed(message)))
            .collect();
        let ids = |request: &TranscriptPage| {
            let (messages, first, last, more) = page(&display, request).expect("a page");
            (messages.len(), first, last, more)
        };
        assert_eq!(
            [
                ids(&TranscriptPage::End {
                    before: None,
                    limit: 3
                }),
                ids(&TranscriptPage::End {
                    before: Some("u2".to_string()),
                    limit: 1
                }),
                ids(&TranscriptPage::Start {
                    after: None,
                    until: Some("a2".to_string()),
                    limit: 2,
                }),
                ids(&TranscriptPage::Start {
                    after: Some("t1".to_string()),
                    until: None,
                    limit: 5
                }),
            ],
            [
                (4, json!("a1"), json!("u2"), true),
                (1, json!("a2"), json!("a2"), true),
                (1, json!("u1"), json!("u1"), true),
                (2, json!("a2"), json!("u2"), false),
            ]
        );
        assert!(page(
            &display,
            &TranscriptPage::End {
                before: Some("gone".to_string()),
                limit: 1
            }
        )
        .is_none());
    }
}
