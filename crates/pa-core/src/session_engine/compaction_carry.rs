//! The state a compaction carries as data instead of leaving it to the
//! summarizer's prose: the user's own messages (verbatim, newest first,
//! within a token budget), the active goal, the open todo items, and the
//! last failure the summarized span shows. It rides the compaction row's
//! `details` (so the next compaction merges it instead of re-deriving it
//! from a summary) and is rendered as harness blocks after the
//! model-written summary, in the row written once: nothing already sent is
//! rewritten.

use std::fmt::Write as _;

use pa_types::session::AgentMessage;
use pa_types::todo::{TodoItem, TodoList, TodoStatus};

#[cfg(test)]
#[path = "compaction_carry_tests.rs"]
mod tests;

/// The most tokens (chars/4) the carried user messages take, as Codex keeps
/// 20,000 tokens of them beside its summary; past it the oldest drop.
const MAX_CARRIED_USER_TOKENS: u64 = 20_000;
/// A smaller window carries at most a tenth of itself in user messages:
/// the post-compaction context also holds the summary and the kept tail
/// (`keepRecentTokens`, 20,000 by default) and must stay well under the
/// compaction trigger. A 200k window carries the full 20,000 tokens, a 128k
/// one 12,800, a 32k one 3,200.
const CARRIED_USER_WINDOW_DIVISOR: u64 = 10;
/// The most constraint sentences the excerpt of one message too large for
/// the budget keeps; the oldest drop first.
const MAX_CONSTRAINTS: usize = 32;
/// The most characters one excerpt's sentences take together; the oldest
/// drop first.
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
/// previous compaction is stripped of them before it is updated
/// (`user-constraints` is the block rows wrote before whole user messages
/// were carried).
pub(crate) const CARRIED_BLOCK_TAGS: [&str; 3] = ["user-messages", "user-constraints", "open-work"];

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

/// How much of a user message a carried one holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CarriedText {
    /// The whole message, verbatim (an expanded skill or attached file
    /// dropped).
    Whole,
    /// Only its instruction sentences, by marker word: the message alone
    /// was larger than the whole budget, or a row written before whole
    /// messages were carried kept only these.
    Excerpt,
}

/// One user message a compaction carries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarriedUserMessage {
    pub text: String,
    pub form: CarriedText,
}

/// The deterministic state a compaction carries (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarriedState {
    /// The user's own messages, newest first, deduplicated (a repeat keeps
    /// its newest place), within the window's budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_messages: Vec<CarriedUserMessage>,
    /// The constraint sentences, oldest first, of a row written before
    /// whole user messages were carried: read so the next compaction keeps
    /// them (as excerpts); a new state leaves it empty.
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

/// The most tokens the carried user messages take in a session whose model
/// has `context_window` tokens; the compaction trigger's retention floor
/// counts them, as they ride every post-compaction context.
pub(crate) fn carried_user_budget_tokens(context_window: u64) -> u64 {
    MAX_CARRIED_USER_TOKENS.min(context_window / CARRIED_USER_WINDOW_DIVISOR)
}

impl CarriedState {
    /// The state after a compaction of `span` (the summarized messages,
    /// tool results included), on top of the previous compaction's, for a
    /// session whose model has `context_window` tokens. The user's messages
    /// are the `user` rows: the harness's own notices are custom rows, never
    /// user ones.
    #[must_use]
    pub fn carry(
        previous: Option<&CarriedState>,
        span: &[AgentMessage],
        live: LiveWork,
        context_window: u64,
    ) -> Self {
        let budget = usize::try_from(carried_user_budget_tokens(context_window).saturating_mul(4))
            .unwrap_or(usize::MAX);
        // Newest first: the span's messages, then the previous row's, then
        // the sentences an older row kept.
        let span_messages = span.iter().rev().filter_map(|message| {
            let AgentMessage::User(user) = message else {
                return None;
            };
            // An expanded skill or attached file is not the user's words.
            let mut own = user.content.text();
            for (open, close) in [("<skill ", "</skill>"), ("<file ", "</file>")] {
                while let Some(start) = own.find(open) {
                    let Some(length) = own[start..].find(close) else {
                        own.truncate(start);
                        break;
                    };
                    own.replace_range(start..start + length + close.len(), "");
                }
            }
            let own = own.trim();
            (!own.is_empty()).then(|| CarriedUserMessage {
                text: own.to_string(),
                form: CarriedText::Whole,
            })
        });
        let previous_messages = previous.into_iter().flat_map(|state| {
            state
                .user_messages
                .iter()
                .cloned()
                .chain(
                    state
                        .user_constraints
                        .iter()
                        .rev()
                        .map(|sentence| CarriedUserMessage {
                            text: sentence.clone(),
                            form: CarriedText::Excerpt,
                        }),
                )
        });
        let mut user_messages = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut remaining = budget;
        for candidate in span_messages.chain(previous_messages) {
            if !seen.insert(dedupe_key(&candidate.text)) {
                continue;
            }
            // Only a message larger than the whole budget falls back to its
            // instruction sentences; one that does not fit what is left ends
            // the walk, so the newest are kept.
            let carried = match candidate.form {
                CarriedText::Whole if candidate.text.chars().count() > budget => {
                    let sentences = constraint_sentences(&candidate.text);
                    if sentences.is_empty() {
                        continue;
                    }
                    CarriedUserMessage {
                        text: sentences.join("\n"),
                        form: CarriedText::Excerpt,
                    }
                }
                CarriedText::Whole | CarriedText::Excerpt => candidate,
            };
            let size = carried.text.chars().count();
            if size > remaining {
                break;
            }
            remaining -= size;
            user_messages.push(carried);
        }
        let goal = live
            .goal
            .filter(|goal| goal.active)
            .and_then(|goal| goal.objective)
            .filter(|objective| !objective.trim().is_empty());
        let open_todos = live.todos.map_or_default(|list| {
            list.items
                .into_iter()
                .filter(|item| item.status != TodoStatus::Done)
                .take(MAX_OPEN_TODOS)
                .collect()
        });
        Self {
            user_messages,
            user_constraints: Vec::new(),
            goal,
            open_todos,
            last_failure: last_failure(previous.and_then(|state| state.last_failure.clone()), span),
        }
    }

    /// The harness blocks for the summary text (empty when nothing is
    /// carried): the user's messages, then the open work.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.user_messages.is_empty() {
            // Newest first and numbered: the harness cannot tell when a
            // later message overrides an earlier one on the same subject,
            // so it keeps both and says which is newer.
            out.push_str("\n\n<user-messages>\nThe user's own messages from the compacted conversation, verbatim, newest first: [1] is the newest. Each still applies unless the user withdrew it later; where two disagree, the newer one (the lower number) wins.\n");
            for (rank, CarriedUserMessage { text, form }) in self.user_messages.iter().enumerate() {
                let rank = rank + 1;
                let _ = match form {
                    CarriedText::Whole => writeln!(out, "\n[{rank}]\n{text}"),
                    CarriedText::Excerpt => writeln!(
                        out,
                        "\n[{rank}] (its instruction sentences only: the message was too long to carry whole)\n{text}"
                    ),
                };
            }
            out.push_str("</user-messages>");
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
/// dropped), in order, deduplicated, the newest kept within
/// [`MAX_CONSTRAINTS`] and [`MAX_CONSTRAINT_TOTAL_CHARS`]. A question is
/// not a constraint.
fn constraint_sentences(own: &str) -> Vec<String> {
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
    let mut kept: Vec<String> = Vec::new();
    for sentence in sentences
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
    {
        let key = dedupe_key(&sentence);
        kept.retain(|earlier| dedupe_key(earlier) != key);
        kept.push(sentence);
    }
    // Newest kept: drop from the oldest end until both caps hold.
    while kept.len() > MAX_CONSTRAINTS
        || kept.iter().map(|c| c.chars().count()).sum::<usize>() > MAX_CONSTRAINT_TOTAL_CHARS
    {
        kept.remove(0);
    }
    kept
}

/// Whitespace-normalized, case-folded, without a final full stop: the key
/// two copies of the same words share.
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
