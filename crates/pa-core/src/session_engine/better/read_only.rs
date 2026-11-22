//! The helper's read-only tools: the parent's tool definitions, byte for byte (so the helper's
//! requests share the session's cached prompt prefix), with host-side executors that can only
//! read.
//!
//! Why the host executes them instead of the session's kernel and shell: neither can be
//! restricted from the inside. A cell can call `fs-write`, `fs-edit`, or `bash`, and a shell
//! line can redirect, pipe into anything, or reach a program through a reading command's own
//! flags (`rg --pre`, `find -exec`, `git grep -O`, a repository-configured fsmonitor hook or
//! diff driver). So the helper never reaches either:
//!
//! - `eval` accepts only calls of `fs-read`/`fs-grep`/`fs-glob`/`fs-find`/`fs-symbol` with
//!   literal arguments (`(displayln (hash-ref (fs-read "src/lib.rs" (hash 'start 1 'end 80))
//!   'text))`, one per form). The host reads the cell as data (nothing evaluates it) and runs
//!   each call through the readers the kernel's own functions use ([`scheme_reads`]). Anything
//!   else is refused with what is accepted.
//! - `bash` accepts only one read-only `git` command (`log`, `show`, `diff`, `status`,
//!   `ls-files`, `grep`, `rev-parse`), parsed without a shell: no pipes, redirects,
//!   substitutions, or variables, and no flag that opens a pager, writes a file, or starts a
//!   program. It runs with the autonomous gates' hardening overrides (no fsmonitor hook, no
//!   index write, no lazy fetch, no text conversion or external diff). The one residual those
//!   overrides document stays: a `.gitattributes` clean filter can run on a modified tracked
//!   file for `diff`/`status`.
//! - Every other tool (`edit` and the internal helpers) is refused: this helper never writes.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use serde_json::Value;

mod scheme_reads;

/// Progress the helper reports while it reads (a short label per read).
pub type ReadProgress = Arc<dyn Fn(&str) + Send + Sync>;

/// The most text one tool result returns; the rest is cut with a note. Reads are for grounding a
/// rewrite, not for copying files, so a few screens per call is plenty.
const RESULT_MAX_CHARS: usize = 12_000;

/// How long one read-only git command may run.
const GIT_TIMEOUT_MS: u64 = 15_000;

/// The git subcommands the helper may run: each only reads the repository.
const GIT_READ_SUBCOMMANDS: [&str; 7] = [
    "log",
    "show",
    "diff",
    "status",
    "ls-files",
    "grep",
    "rev-parse",
];

/// Flag prefixes refused on any git subcommand: each writes a file (`--output`), opens a pager
/// program (`-O`, `--open-files-in-pager`), or re-enables a program the overrides turn off.
const GIT_REFUSED_FLAGS: [&str; 6] = [
    "--output",
    "-O",
    "--open-files-in-pager",
    "--ext-diff",
    "--textconv",
    "--exec",
];

pub(crate) const EDIT_REFUSED: &str = "Refused: the /better helper is read-only. It cannot edit, \
     write, or delete files, or evaluate code. Read what you need with eval's file readers, \
     then reply with your <rewrite> block.";

const BASH_ACCEPTS: &str = "bash in this read-only helper runs only one read-only git command \
     (`git log`, `show`, `diff`, `status`, `ls-files`, `grep`, `rev-parse`) without pipes, \
     redirects, or substitutions. Read a file with `git show HEAD:path` and search with `git grep`.";

/// The helper's read allowance, shared by its tools: each pass grants more on top.
#[derive(Debug, Default)]
pub(crate) struct ReadBudget {
    used: AtomicU32,
    limit: AtomicU32,
}

impl ReadBudget {
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            used: AtomicU32::new(0),
            limit: AtomicU32::new(limit),
        }
    }

    /// Take one read; `false` once the pass's allowance is spent.
    fn take(&self) -> bool {
        let limit = self.limit.load(Ordering::SeqCst);
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < limit).then_some(used + 1)
            })
            .is_ok()
    }

    /// Grant `reads` more for the next pass.
    pub(crate) fn grow(&self, reads: u32) {
        self.limit.fetch_add(reads, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    pub(crate) fn limit(&self) -> u32 {
        self.limit.load(Ordering::SeqCst)
    }
}

/// What every read-only executor shares: where reads resolve, the budget, and the progress tap.
#[derive(Clone)]
pub(crate) struct ReadContext {
    pub(crate) cwd: PathBuf,
    pub(crate) budget: Arc<ReadBudget>,
    pub(crate) progress: ReadProgress,
}

impl ReadContext {
    fn budget_spent(&self) -> String {
        format!(
            "The read budget for this pass is used up ({} reads). Reply now with your <depth>, \
             <unresolved>, and <rewrite> blocks; list under <unresolved> what you could not \
             locate yet.",
            self.budget.limit()
        )
    }
}

/// Which executor a wrapped tool runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Executor {
    FsCalls,
    Git,
    Refused,
}

/// One parent tool's definition with a read-only executor in place of its own.
struct ReadOnlyTool {
    definition: Arc<dyn AgentTool>,
    executor: Executor,
    context: ReadContext,
}

/// The parent's tools, each wrapped with its read-only executor (see the module docs).
pub(crate) fn read_only_tools(
    parent: &[Arc<dyn AgentTool>],
    context: &ReadContext,
) -> Vec<Arc<dyn AgentTool>> {
    parent
        .iter()
        .map(|tool| {
            let executor = match tool.name() {
                pa_types::ai::KERNEL_TOOL_NAME => Executor::FsCalls,
                "bash" => Executor::Git,
                _ => Executor::Refused,
            };
            Arc::new(ReadOnlyTool {
                definition: Arc::clone(tool),
                executor,
                context: context.clone(),
            }) as Arc<dyn AgentTool>
        })
        .collect()
}

impl AgentTool for ReadOnlyTool {
    fn name(&self) -> &str {
        self.definition.name()
    }

    fn description(&self) -> &str {
        self.definition.description()
    }

    fn parameters(&self) -> &Value {
        self.definition.parameters()
    }

    fn label(&self) -> &str {
        self.definition.label()
    }

    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let text = match self.executor {
                Executor::Refused => EDIT_REFUSED.to_string(),
                Executor::FsCalls => {
                    let code = params.get("code").and_then(Value::as_str).unwrap_or("");
                    scheme_reads::run_eval_cell(code, &self.context).await
                }
                Executor::Git => {
                    let command = params.get("command").and_then(Value::as_str).unwrap_or("");
                    run_git_command(command, &self.context).await
                }
            };
            Ok(AgentToolResult::text(text))
        })
    }
}

/// Cut `text` to [`RESULT_MAX_CHARS`] with a note.
fn capped(text: String) -> String {
    match text.char_indices().nth(RESULT_MAX_CHARS) {
        Some((end, _)) => format!(
            "{}\n[output cut at {RESULT_MAX_CHARS} characters; read a narrower range]",
            &text[..end]
        ),
        None => text,
    }
}

// ---------------------------------------------------------------------------------------------
// bash: one read-only git command

/// Split a command line into words without a shell: single and double quotes group, a
/// backslash escapes the next character, and every shell operator is refused.
fn shell_words(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(inner) => word.push(inner),
                        None => return Err("a quote is not closed".to_string()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('$' | '`') => {
                            return Err("substitutions are not accepted".to_string());
                        }
                        Some('\\') => match chars.next() {
                            Some(escaped) => word.push(escaped),
                            None => return Err("a quote is not closed".to_string()),
                        },
                        Some(inner) => word.push(inner),
                        None => return Err("a quote is not closed".to_string()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(escaped) => word.push(escaped),
                    None => return Err("a trailing backslash is not accepted".to_string()),
                }
            }
            '|' | '&' | ';' | '<' | '>' | '$' | '`' | '(' | ')' | '{' | '}' | '\n' | '*' | '?'
            | '~' | '!' => {
                return Err(format!("`{c}` is a shell operator and is not accepted"));
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// The full argument list for one accepted read-only git command, or why it is refused.
pub(crate) fn git_arguments(command: &str) -> Result<Vec<String>, String> {
    let words = shell_words(command)?;
    let Some((program, rest)) = words.split_first() else {
        return Err("the command is empty".to_string());
    };
    if program != "git" {
        return Err(format!("`{program}` is not read-only git"));
    }
    let Some((subcommand, arguments)) = rest.split_first() else {
        return Err("name a git subcommand".to_string());
    };
    if !GIT_READ_SUBCOMMANDS.contains(&subcommand.as_str()) {
        return Err(format!(
            "`git {subcommand}` is not one of the read-only subcommands"
        ));
    }
    if let Some(flag) = arguments.iter().find(|argument| {
        GIT_REFUSED_FLAGS
            .iter()
            .any(|refused| argument.starts_with(refused))
    }) {
        return Err(format!("`{flag}` can write a file or start a program"));
    }
    let mut full: Vec<String> = crate::autonomous::SNAPSHOT_GIT_OVERRIDES
        .iter()
        .map(ToString::to_string)
        .collect();
    full.extend(
        ["--no-pager", "-c", "core.pager=cat"]
            .iter()
            .map(ToString::to_string),
    );
    full.push(subcommand.clone());
    let hardening: &[&str] = match subcommand.as_str() {
        "diff" | "show" | "log" => &["--no-ext-diff", "--no-textconv"],
        "grep" => &["--no-textconv"],
        "status" => &["--ignore-submodules=all"],
        _ => &[],
    };
    full.extend(hardening.iter().map(ToString::to_string));
    full.extend(arguments.iter().cloned());
    Ok(full)
}

/// Run one `bash` call: an accepted git command, against the budget.
async fn run_git_command(command: &str, context: &ReadContext) -> String {
    let arguments = match git_arguments(command) {
        Ok(arguments) => arguments,
        Err(reason) => return format!("Refused: {reason}. {BASH_ACCEPTS}"),
    };
    if !context.budget.take() {
        return context.budget_spent();
    }
    (context.progress)(command.trim());
    // The same transport lock-down as the gates' snapshot probes: no lazy fetch can start a
    // repository-chosen transport.
    let result = crate::autonomous::run_child_process(
        "git",
        &arguments,
        &context.cwd,
        GIT_TIMEOUT_MS,
        &[
            ("GIT_ALLOW_PROTOCOL", ""),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_PAGER", "cat"),
            ("GIT_OPTIONAL_LOCKS", "0"),
        ],
    )
    .await;
    let text = match result {
        Ok(result) if result.timed_out => {
            format!("git timed out after {GIT_TIMEOUT_MS} ms")
        }
        Ok(result) => {
            let mut text = result.stdout;
            if !result.stderr.is_empty() {
                text.push_str(&result.stderr);
            }
            if result.status != Some(0) {
                let _ = write!(text, "\n[exit {}]", result.status.unwrap_or(-1));
            }
            text
        }
        Err(error) => format!("git did not run: {error:#}"),
    };
    capped(text)
}

#[cfg(test)]
pub(super) mod tests;
