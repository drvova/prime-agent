//! The files a compaction reads again. The summary keeps a file's path, not
//! its content or its anchors, so the first `fs-edit` after a compaction used
//! to need another `fs-read`. After a compaction commits, the session appends
//! one hidden `compaction_recent_files` row with fresh `fs-read` views (the
//! anchors included) of the files its eval cells read or edited last before
//! the cut, read from disk at that moment, so a file changed since shows as
//! it is now (the idea of `ZCode`'s post-compaction file reminders,
//! reimplemented). The row comes after the compaction row, so it adds to the
//! post-compaction context and rewrites nothing already sent.
//!
//! Bounded below `ZCode`'s 5 files and 50,000 tokens: the post-compaction
//! context also holds the summary, the carried user messages (up to 20,000
//! tokens) and the kept tail (`keepRecentTokens`, 20,000 by default), and a
//! view the model does not need costs it on every later request until the
//! next compaction.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use pa_agent::agent::Agent;
use pa_agent::types::AgentMessage;
use pa_types::session::{CustomMessage, FileEntry};

use crate::session::manager::SessionManager;

#[cfg(test)]
#[path = "compaction_files_tests.rs"]
mod tests;

/// The row's custom type.
pub(crate) const RECENT_FILES_CUSTOM_TYPE: &str = "compaction_recent_files";

/// The most files read again.
const MAX_FILES: usize = 4;
/// The most tokens (chars/4) one file's view takes, so four take at most
/// 20,000. A larger file, or one past `fs-read`'s default 2,000 lines, gets
/// a note to read it again instead.
const MAX_FILE_TOKENS: u64 = 5_000;
/// A smaller window gives each file at most a fortieth of itself, a tenth
/// for all four: 3,200 tokens each in a 128k window, 800 in a 32k one.
const FILE_WINDOW_DIVISOR: u64 = 40;

/// The most tokens the re-read files take in all in a session whose model
/// has `context_window` tokens ([`MAX_FILES`] views of at most
/// [`MAX_FILE_TOKENS`] each); the compaction trigger's retention floor
/// counts them, as they ride every post-compaction context.
pub(crate) fn recent_files_budget_tokens(context_window: u64) -> u64 {
    MAX_FILE_TOKENS.min(context_window / FILE_WINDOW_DIVISOR) * MAX_FILES as u64
}

/// Read again the files the session's eval cells read or edited last before
/// the compaction's first kept entry, newest first (at most [`MAX_FILES`];
/// a file the kept tail reads is in context already and skipped), and append
/// their views as one hidden row: the durable row first, then the live loop
/// context (before a trailing error assistant turn, as the kernel-state
/// notice). An older row leaves the live context, as the built context keeps
/// only the newest. `None` when no file qualifies.
///
/// # Errors
///
/// The durable row cannot be appended, or the reading thread failed.
pub(crate) async fn sync_after_compaction(
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    agent: &Arc<Agent>,
    first_kept_entry_id: &str,
    context_window: u64,
) -> std::io::Result<Option<CustomMessage>> {
    let (paths, cwd) = {
        let session = session.lock().await;
        let entries = session.retained_entries();
        let first_kept = entries
            .iter()
            .position(|entry| entry.id() == Some(first_kept_entry_id))
            .unwrap_or(entries.len());
        // A kernel cell's details name what it edited and read.
        let cell_details = |entry: &FileEntry| {
            let FileEntry::Message {
                message: pa_types::session::AgentMessage::ToolResult(result),
                ..
            } = entry
            else {
                return None;
            };
            pa_types::ai::is_kernel_tool_name(&result.tool_name)
                .then(|| result.details.as_ref()?.as_object().cloned())
                .flatten()
        };
        let reads = |details: &serde_json::Map<String, serde_json::Value>| {
            details
                .get("fileReads")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        };
        let in_tail: std::collections::HashSet<PathBuf> = entries[first_kept..]
            .iter()
            .filter_map(cell_details)
            .flat_map(|details| reads(&details))
            .collect();
        let cwd = session.get_cwd().to_path_buf();
        let mut paths: Vec<PathBuf> = Vec::new();
        for details in entries[..first_kept].iter().rev().filter_map(cell_details) {
            let edited = details
                .get("diffs")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|diff| diff.get("path").and_then(serde_json::Value::as_str))
                .map(|path| cwd.join(path));
            // Within one cell the edits count as later than the reads.
            for path in edited.rev().chain(reads(&details).into_iter().rev()) {
                if paths.len() < MAX_FILES && !in_tail.contains(&path) && !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        (paths, cwd)
    };
    if paths.is_empty() {
        return Ok(None);
    }
    let max_chars = usize::try_from(
        (recent_files_budget_tokens(context_window) / MAX_FILES as u64).saturating_mul(4),
    )
    .unwrap_or(usize::MAX);
    let content = tokio::task::spawn_blocking(move || {
        let mut views = Vec::new();
        let mut notes = String::new();
        for path in paths {
            let shown = crate::fs_search::display_path(&path, &cwd);
            let request = crate::fs_edit::ReadRequest {
                target: crate::fs_edit::Target {
                    path,
                    shown: shown.clone(),
                },
                start: 1,
                end: None,
            };
            match crate::fs_edit::read(&request) {
                Ok((reply, _)) => {
                    let text = reply["text"].as_str().unwrap_or_default();
                    let chars = text.chars().count();
                    if reply["truncated"] == serde_json::Value::Bool(true) || chars > max_chars {
                        let _ = writeln!(
                            notes,
                            "- {shown}: {} lines, about {} tokens, too large to show here; `(fs-read \"{shown}\")` it again when you need it.",
                            reply["totalLines"],
                            chars / 4
                        );
                    } else {
                        views.push(text.to_string());
                    }
                }
                Err(error) => {
                    let _ = writeln!(notes, "- {shown}: not read again ({error:#}).");
                }
            }
        }
        let mut content = "[recent-files]\n\nThe files your eval cells read or edited last before the compaction, newest first, read again from disk after it. Their anchors are current: `fs-edit` takes them without another `fs-read` while a file stays as shown here.".to_string();
        for view in views {
            let _ = write!(content, "\n\n{view}");
        }
        if !notes.is_empty() {
            let _ = write!(content, "\n\n{}", notes.trim_end());
        }
        content
    })
    .await
    .map_err(std::io::Error::other)?;
    let row = CustomMessage {
        custom_type: RECENT_FILES_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(content),
        display: false,
        details: None,
        timestamp: pa_ai::admission::now_ms(),
        rest: serde_json::Map::default(),
    };
    session.lock().await.append_custom_message(
        &row.custom_type,
        row.content.clone(),
        row.display,
        row.details.clone(),
    )?;
    let Some(loop_message) =
        super::session_message_to_loop(&pa_types::session::AgentMessage::Custom(row.clone()))
    else {
        return Ok(Some(row));
    };
    let mut messages = agent.state().await.messages;
    messages.retain(|message| {
        !matches!(
            message,
            AgentMessage::Custom(custom)
                if custom.payload.get("customType").and_then(serde_json::Value::as_str)
                    == Some(RECENT_FILES_CUSTOM_TYPE)
        )
    });
    let insert_before_error = matches!(
        messages.last(),
        Some(AgentMessage::Standard(pa_agent::types::Message::Assistant(assistant)))
            if assistant.stop_reason == pa_agent::types::StopReason::Error
    );
    if insert_before_error {
        messages.insert(messages.len() - 1, loop_message);
    } else {
        messages.push(loop_message);
    }
    agent.set_messages(messages).await;
    Ok(Some(row))
}
