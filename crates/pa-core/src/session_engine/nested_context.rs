//! Nested project context. The system prompt carries the context files of the session's
//! working directory and its parents (`resources::load_project_context_files`); a
//! subdirectory's own AGENTS.md (or CLAUDE.md, by the same file-name rules) is never in it.
//! When an `eval` cell reads or edits a file below the working directory, every directory
//! between the working directory and that file that holds such a file not already in the
//! conversation has it appended once, as a row after the cell's result: steered onto the
//! session's agent, so the loop adds it to the next request and the transcript, and nothing
//! already sent changes.
//!
//! The directories come from what the cell's result records: `fileReads` (`fs-read`) and the
//! `diffs` of `edit`, `fs-edit` and `fs-write`. A `bash` command always starts in the session's
//! working directory, and a search records no path, so neither names a directory.
//!
//! "Already in the conversation" is read from the live context: a file whose row a compaction
//! summarized away is appended again the next time a cell works under it, and one the kept
//! tail still holds is not.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use pa_agent::agent::Agent;
use pa_agent::agent_loop::AfterToolCallFn;
use pa_agent::types::{AfterToolCallContext, AgentMessage};
use pa_types::ai::UserContent;
use pa_types::session::CustomMessage;
use serde_json::{json, Value};

use super::messages::NESTED_CONTEXT_CUSTOM_TYPE;
use super::telemetry::SessionCounters;
use super::{session_message_to_loop, SessionAgentMessage};

/// The most bytes of one nested context file a row carries; a longer file is cut at the last
/// whole line within it and the row says how to read the rest. The prompt's own context files
/// are not capped, but a nested file arrives unasked, possibly many times in a session.
const MAX_FILE_BYTES: usize = 32 * 1024;

/// One session's nested context files.
pub(crate) struct NestedContext {
    /// The session's working directory, canonical.
    root: PathBuf,
    /// The canonical paths of the context files the system prompt already carries.
    in_prompt: HashSet<PathBuf>,
    /// The agent the rows steer onto; bound once it exists, weak so the agent's own hook does
    /// not keep it alive.
    agent: OnceLock<Weak<Agent>>,
    counters: Arc<SessionCounters>,
    /// The rows steered during one tool batch (keyed by the batch's context length) that its
    /// context cannot show yet: parallel cells of one batch see the same context.
    steered: Mutex<(usize, HashSet<String>)>,
}

impl NestedContext {
    /// The nested context of a session in `cwd` whose prompt carries `prompt_files`.
    pub(crate) fn new(
        cwd: &Path,
        prompt_files: &[crate::resources::ContextFile],
        counters: Arc<SessionCounters>,
    ) -> Self {
        Self {
            root: cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()),
            in_prompt: prompt_files
                .iter()
                .map(|file| {
                    file.path
                        .canonicalize()
                        .unwrap_or_else(|_| file.path.clone())
                })
                .collect(),
            agent: OnceLock::new(),
            counters,
            steered: Mutex::default(),
        }
    }

    /// Bind the agent the rows steer onto.
    pub(crate) fn bind(&self, agent: &Arc<Agent>) {
        let _ = self.agent.set(Arc::downgrade(agent));
    }

    /// The session's tool hook: `next` (the loop guard, when on) runs after this one has
    /// steered the rows a finished `eval` cell calls for.
    pub(crate) fn after_tool_call(
        self: &Arc<Self>,
        next: Option<AfterToolCallFn>,
    ) -> AfterToolCallFn {
        let nested = Arc::clone(self);
        Arc::new(move |call, signal| {
            nested.observe(&call);
            match &next {
                Some(next) => next(call, signal),
                None => Box::pin(async { Ok(None) }),
            }
        })
    }

    /// Steer a row for each context file under the directories `call` worked in that the
    /// conversation does not hold yet, root-most first, as one batch.
    fn observe(&self, call: &AfterToolCallContext) {
        if !pa_types::ai::is_kernel_tool_name(&call.tool_call.name) {
            return;
        }
        let details = &call.result.details;
        let edited = details
            .get("diffs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|diff| diff.get("path"));
        let touched: Vec<PathBuf> = details
            .get("fileReads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(edited)
            .filter_map(Value::as_str)
            .filter(|path| !path.is_empty())
            .map(|path| {
                let path = Path::new(path);
                path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
            })
            .collect();
        // Every directory strictly between the root and a touched file; sorted, a parent
        // comes before its children.
        let dirs: BTreeSet<&Path> = touched
            .iter()
            .flat_map(|file| file.ancestors().skip(1))
            .filter(|dir| dir.starts_with(&self.root) && *dir != self.root)
            .collect();
        if dirs.is_empty() {
            return;
        }
        let shown: HashSet<&str> = call
            .context
            .messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Custom(custom)
                    if custom.payload.get("customType").and_then(Value::as_str)
                        == Some(NESTED_CONTEXT_CUSTOM_TYPE) =>
                {
                    custom
                        .payload
                        .pointer("/details/path")
                        .and_then(Value::as_str)
                }
                AgentMessage::Custom(_) | AgentMessage::Standard(_) => None,
            })
            .collect();
        let mut steered = self.steered.lock().unwrap_or_else(PoisonError::into_inner);
        let batch = call.context.messages.len();
        if steered.0 != batch {
            *steered = (batch, HashSet::new());
        }
        let mut rows = Vec::new();
        for dir in dirs {
            let Some(file) = crate::resources::load_context_file_from_dir(dir) else {
                continue;
            };
            // A file that links out of the project, or to one the prompt carries, is not new.
            let target = file
                .path
                .canonicalize()
                .unwrap_or_else(|_| file.path.clone());
            if !target.starts_with(&self.root) || self.in_prompt.contains(&target) {
                continue;
            }
            let Ok(relative) = file.path.strip_prefix(&self.root) else {
                continue;
            };
            let label = relative.to_string_lossy().replace('\\', "/");
            if shown.contains(label.as_str()) || steered.1.contains(&label) || self.gitignored(dir)
            {
                continue;
            }
            let row = nested_context_row(&label, &file.content, crate::autonomous::now_millis());
            if let Some(row) = session_message_to_loop(&SessionAgentMessage::Custom(row)) {
                steered.1.insert(label);
                self.counters.note_nested_context_file();
                rows.push(row);
            }
        }
        drop(steered);
        if rows.is_empty() {
            return;
        }
        if let Some(agent) = self.agent.get().and_then(Weak::upgrade) {
            agent.steer(rows);
        }
    }

    /// Whether a `.gitignore` between the root and `dir` ignores `dir`: a dependency or build
    /// tree (`node_modules`, `vendor`, `target`) is not the project's to instruct.
    fn gitignored(&self, dir: &Path) -> bool {
        dir.ancestors()
            .skip(1)
            .take_while(|parent| parent.starts_with(&self.root))
            .any(|parent| {
                let mut builder = ignore::gitignore::GitignoreBuilder::new(parent);
                builder.add(parent.join(".gitignore"));
                builder.build().is_ok_and(|matcher| {
                    matcher
                        .matched_path_or_any_parents(dir, /*is_dir*/ true)
                        .is_ignore()
                })
            })
    }
}

/// The row for the context file at `label` (relative to the working directory) holding
/// `content`, cut to [`MAX_FILE_BYTES`].
fn nested_context_row(label: &str, content: &str, timestamp: u64) -> CustomMessage {
    let directory = label
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    let mut text = format!(
        "[nested-context {label}]\n\nThe project instructions for `{directory}/`, loaded because a cell worked there. They apply to the files under that directory, beside the project context in the system prompt.\n\n"
    );
    let truncated = content.len() > MAX_FILE_BYTES;
    if truncated {
        let mut end = MAX_FILE_BYTES;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        let kept = content[..end]
            .rfind('\n')
            .map_or(&content[..end], |newline| &content[..=newline]);
        let shown_lines = kept.lines().count();
        text.push_str(kept);
        // Writing to a string cannot fail.
        let _ = write!(
            text,
            "\n[truncated: lines 1-{shown_lines} of {}; read the rest with `(fs-read \"{label}\" (hash 'start {}))`]",
            content.lines().count(),
            shown_lines + 1
        );
    } else {
        text.push_str(content);
    }
    CustomMessage {
        custom_type: NESTED_CONTEXT_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(text),
        display: true,
        details: Some(json!({ "path": label, "truncated": truncated })),
        timestamp,
        rest: serde_json::Map::default(),
    }
}

#[cfg(test)]
mod tests;
