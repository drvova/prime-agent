//! The state a compaction carries as data instead of leaving it to the
//! summarizer's prose: the user's own constraint sentences (verbatim), the
//! active goal, the open todo items, and the last failure the summarized
//! span shows. It rides the compaction row's `details` (so the next
//! compaction merges it instead of re-deriving it from a summary) and is
//! rendered as harness blocks after the model-written summary.

use std::fmt::Write as _;

use pa_types::session::AgentMessage;
use pa_types::todo::{TodoItem, TodoList, TodoStatus};

#[cfg(test)]
#[path = "compaction_carry_tests.rs"]
mod tests;

/// The most constraint sentences carried; the oldest drop first.
const MAX_CONSTRAINTS: usize = 32;
/// The most characters all carried constraint sentences take together; the
/// oldest drop first.
const MAX_CONSTRAINT_TOTAL_CHARS: usize = 4_000;
/// The longest single constraint sentence kept whole; a longer one keeps its
/// head and ends in [`CLIPPED_MARK`].
const MAX_CONSTRAINT_CHARS: usize = 400;
const CLIPPED_MARK: &str = " […]";
/// The most open todo items carried (the list's own order).
const MAX_OPEN_TODOS: usize = 30;
/// The longest failing command and error text carried (the error keeps its tail).
const MAX_FAILURE_COMMAND_CHARS: usize = 300;
const MAX_FAILURE_ERROR_CHARS: usize = 600;
/// The most failure-naming lines a failure's error keeps (the newest).
const MAX_FAILURE_LINES: usize = 8;
/// Substrings that mark a line of a failed command's output as naming the
/// failure.
const FAILURE_LINE_MARKERS: [&str; 6] = [
    "panicked",
    "assertion",
    "error",
    "Error",
    "FAILED",
    "failed",
];

/// Words and phrases that make a user's sentence a constraint or an
/// instruction worth keeping verbatim (matched as whole lowercase words).
const CONSTRAINT_MARKERS: [&str; 24] = [
    "don't",
    "do not",
    "never",
    "always",
    "must",
    "mustn't",
    "only",
    "should",
    "shouldn't",
    "avoid",
    "prefer",
    "make sure",
    "ensure",
    "required",
    "require",
    "requires",
    "keep",
    "instead of",
    "rather than",
    "no longer",
    "stop",
    "without",
    "please",
    "not allowed",
];

/// The tags of the blocks [`CarriedState::render`] writes; the summary of a
/// previous compaction is stripped of them before it is updated.
pub(crate) const CARRIED_BLOCK_TAGS: [&str; 2] = ["user-constraints", "open-work"];

/// The last failure the summarized span shows, and not a later success of
/// the same command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarriedFailure {
    /// The `!` command, the background command, or the kernel cell's code.
    pub command: String,
    /// The error's tail: the cell's error value, or the command's output.
    pub error: String,
}

/// The deterministic state a compaction carries (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarriedState {
    /// The user's constraint and instruction sentences, verbatim, oldest
    /// first, deduplicated (a repeat moves to its newest position).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_constraints: Vec<String>,
    /// The active goal's objective.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// The todo items not yet done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_todos: Vec<TodoItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<CarriedFailure>,
}

/// The live session state a compaction reads at prepare time.
#[derive(Debug, Clone, Default)]
pub struct LiveWork {
    pub goal: Option<pa_types::goal::GoalState>,
    pub todos: Option<TodoList>,
}

impl CarriedState {
    /// The state after a compaction of `span` (the summarized messages,
    /// tool results included), on top of the previous compaction's.
    #[must_use]
    pub fn carry(previous: Option<&CarriedState>, span: &[AgentMessage], live: LiveWork) -> Self {
        let mut constraints: Vec<String> = previous
            .map(|state| state.user_constraints.clone())
            .unwrap_or_default();
        for message in span {
            if let AgentMessage::User(user) = message {
                for sentence in constraint_sentences(&user.content.text()) {
                    let key = dedupe_key(&sentence);
                    constraints.retain(|kept| dedupe_key(kept) != key);
                    constraints.push(sentence);
                }
            }
        }
        // Newest kept: drop from the oldest end until both caps hold.
        while constraints.len() > MAX_CONSTRAINTS
            || constraints.iter().map(|c| c.chars().count()).sum::<usize>()
                > MAX_CONSTRAINT_TOTAL_CHARS
        {
            constraints.remove(0);
        }
        let goal = live
            .goal
            .filter(|goal| goal.active)
            .and_then(|goal| goal.objective)
            .filter(|objective| !objective.trim().is_empty());
        let open_todos = live
            .todos
            .map(|list| {
                list.items
                    .into_iter()
                    .filter(|item| item.status != TodoStatus::Done)
                    .take(MAX_OPEN_TODOS)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            user_constraints: constraints,
            goal,
            open_todos,
            last_failure: last_failure(previous.and_then(|state| state.last_failure.clone()), span),
        }
    }

    /// The harness blocks for the summary text (empty when nothing is
    /// carried): the constraints, then the open work.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.user_constraints.is_empty() {
            // Newest first and numbered: the harness cannot tell when a
            // later sentence overrides an earlier one on the same subject,
            // so it keeps both and says which is newer.
            out.push_str("\n\n<user-constraints>\nThe user's own words from the compacted conversation, quoted verbatim, newest first: [1] is the newest. Each still applies unless the user withdrew it later; where two disagree, the newer one (the lower number) wins.\n");
            for (rank, constraint) in self.user_constraints.iter().rev().enumerate() {
                let _ = writeln!(out, "[{}] \"{constraint}\"", rank + 1);
            }
            out.push_str("</user-constraints>");
        }
        if self.goal.is_none() && self.open_todos.is_empty() && self.last_failure.is_none() {
            return out;
        }
        out.push_str("\n\n<open-work>\nAs the harness recorded it at the compaction; the retained messages below are newer.\n");
        if let Some(goal) = &self.goal {
            let _ = writeln!(out, "goal: {goal}");
        }
        for TodoItem {
            id,
            text,
            status,
            note,
        } in &self.open_todos
        {
            let _ = write!(out, "todo {id} [{}]: {text}", status.slug());
            if let Some(note) = note {
                let _ = write!(out, " ({note})");
            }
            out.push('\n');
        }
        if let Some(CarriedFailure { command, error }) = &self.last_failure {
            let _ = writeln!(out, "last failure: {command}\n{error}");
        }
        out.push_str("</open-work>");
        out
    }
}

/// The constraint sentences of one user message, verbatim (a list marker
/// dropped), in order. A question is not a constraint, and an expanded
/// skill or attached file (`<skill …>…</skill>`, `<file …>…</file>`) is not
/// the user's words.
fn constraint_sentences(text: &str) -> Vec<String> {
    let mut own = text.to_string();
    for (open, close) in [("<skill ", "</skill>"), ("<file ", "</file>")] {
        while let Some(start) = own.find(open) {
            let Some(length) = own[start..].find(close) else {
                own.truncate(start);
                break;
            };
            own.replace_range(start..start + length + close.len(), "");
        }
    }
    let mut sentences = Vec::new();
    for line in own.lines() {
        let line = line.trim();
        let line = ["- ", "* ", "• "]
            .iter()
            .find_map(|marker| line.strip_prefix(marker))
            .unwrap_or(line);
        let mut start = 0;
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        for (position, (index, ch)) in chars.iter().enumerate() {
            let ends = matches!(ch, '.' | '!' | '?')
                && chars
                    .get(position + 1)
                    .is_none_or(|(_, next)| next.is_whitespace());
            if ends {
                let end = index + ch.len_utf8();
                sentences.push(line[start..end].trim().to_string());
                start = end;
            }
        }
        sentences.push(line[start..].trim().to_string());
    }
    sentences
        .into_iter()
        .filter(|sentence| !sentence.is_empty() && !sentence.ends_with('?'))
        .filter(|sentence| {
            // Whole lowercase words, so `only` never matches `commonly`.
            let lowered = sentence.to_lowercase().replace('’', "'");
            let words: Vec<&str> = lowered
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '\''))
                .filter(|word| !word.is_empty())
                .collect();
            let joined = format!(" {} ", words.join(" "));
            CONSTRAINT_MARKERS
                .iter()
                .any(|marker| joined.contains(&format!(" {marker} ")))
        })
        .map(|sentence| clip_head(&sentence, MAX_CONSTRAINT_CHARS))
        .collect()
}

fn dedupe_key(sentence: &str) -> String {
    sentence
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '!'])
        .to_lowercase()
}

fn clip_head(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}{CLIPPED_MARK}")
}

/// The newest failure in `span` that no later success of the same command
/// cleared, else the previous one unless `span` cleared it.
fn last_failure(previous: Option<CarriedFailure>, span: &[AgentMessage]) -> Option<CarriedFailure> {
    let mut current = previous;
    let mut cell_code = std::collections::HashMap::new();
    for message in span {
        // (command, Some(error) for a failure, None for a success)
        let outcome: Option<(String, Option<String>)> = match message {
            AgentMessage::Assistant(assistant) => {
                for block in &assistant.content {
                    if let pa_types::ai::AssistantContentBlock::ToolCall(call) = block {
                        if let Some(code) =
                            call.arguments.get("code").and_then(|code| code.as_str())
                        {
                            cell_code.insert(call.id.clone(), code.trim().to_string());
                        }
                    }
                }
                None
            }
            AgentMessage::ToolResult(result) => {
                if pa_types::ai::is_kernel_tool_name(&result.tool_name) {
                    cell_code.get(&result.tool_call_id).map(|code| {
                        let error = result.is_error.then(|| {
                            result
                                .details
                                .as_ref()
                                .and_then(|details| details.pointer("/error/evalue"))
                                .and_then(|value| value.as_str())
                                .map_or_else(
                                    || {
                                        result
                                            .content
                                            .iter()
                                            .filter_map(pa_types::ai::UserContentBlock::text)
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    },
                                    str::to_string,
                                )
                        });
                        (code.clone(), error)
                    })
                } else {
                    None
                }
            }
            AgentMessage::BashExecution(bash) => {
                match (bash.exit_code, bash.exclude_from_context) {
                    (Some(code), None | Some(false)) => Some((
                        bash.command.clone(),
                        (code != 0).then(|| bash.output.clone()),
                    )),
                    (None, _) | (Some(_), Some(true)) => None,
                }
            }
            AgentMessage::Custom(custom)
                if custom.custom_type == super::messages::ASYNC_BASH_COMPLETION_CUSTOM_TYPE =>
            {
                let details = custom.details.as_ref();
                let command = details
                    .and_then(|details| details.get("command"))
                    .and_then(|command| command.as_str());
                let exit = details
                    .and_then(|details| details.get("exitCode"))
                    .and_then(serde_json::Value::as_i64);
                match (command, exit) {
                    (Some(command), Some(exit)) => Some((
                        command.to_string(),
                        (exit != 0).then(|| custom.content.text()),
                    )),
                    (None, _) | (_, None) => None,
                }
            }
            AgentMessage::User(_)
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary(_)
            | AgentMessage::CompactionSummary(_) => None,
        };
        match outcome {
            Some((command, Some(error))) => {
                // The lines that name the failure (a test log holds its
                // panic in the middle), else the whole text; either keeps
                // its tail within the cap.
                let named: Vec<&str> = error
                    .lines()
                    .filter(|line| {
                        FAILURE_LINE_MARKERS
                            .iter()
                            .any(|marker| line.contains(marker))
                    })
                    .collect();
                let error = if named.is_empty() {
                    error.trim().to_string()
                } else {
                    named[named.len().saturating_sub(MAX_FAILURE_LINES)..].join("\n")
                };
                let count = error.chars().count();
                let error = if count <= MAX_FAILURE_ERROR_CHARS {
                    error
                } else {
                    let tail: String = error
                        .chars()
                        .skip(count - MAX_FAILURE_ERROR_CHARS)
                        .collect();
                    format!("{CLIPPED_MARK}{tail}")
                };
                current = Some(CarriedFailure {
                    command: clip_head(&command, MAX_FAILURE_COMMAND_CHARS),
                    error,
                });
            }
            // A success of the failed command clears it.
            Some((command, None))
                if current.as_ref().is_some_and(|failure| {
                    failure.command == clip_head(&command, MAX_FAILURE_COMMAND_CHARS)
                }) =>
            {
                current = None;
            }
            Some((_, None)) | None => {}
        }
    }
    current
}
