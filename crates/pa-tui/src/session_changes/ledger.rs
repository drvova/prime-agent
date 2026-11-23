//! The changes ledger: what the loaded transcript says about the files the agent changed and
//! the checks that ran after, kept beside the view's entries and folded incrementally.
//!
//! Each entry is read once into its [`Facts`] (an edit cell's diffs, a check's outcome, a user
//! prompt) and the read is kept until the view marks the entry stale. The fold of the settled
//! prefix of the transcript is kept too: an append extends it by the new entries only, and an
//! entry still running (a cell without its final result, a streaming reply) is folded fresh on
//! top of it each time the summary is asked for. A change inside the folded prefix (a page that
//! loaded above, an evicted chunk, an old card settled late) folds again from the kept reads.
//!
//! What counts as a check, conservatively (documented in the crate README):
//! - a kernel cell whose stdout carries the kernel's test summary line (`[tests: cargo · 3
//!   passed · 0 failed]`): failed when it counts a failure;
//! - a kernel cell that ran exactly one `bash` command matching [`check_kind`] and whose value
//!   is that command's result: passed on exit code 0, failed on another code or a timeout;
//! - a `[bash-done]` row of a command matching [`check_kind`], by its exit code;
//! - the user's own `!` command matching [`check_kind`], once it ended with an exit code.
//!
//! A file's evidence is the latest test run in a later entry than the file's last edit, else the
//! latest build there: a cell that edits and tests in one go does not say which came first, and
//! a build after a failed test does not hide the failure.

use std::collections::HashMap;

use serde_json::Value;

use crate::chat::ChatEntry;
use crate::custom_message::BackgroundOutcome;
use crate::tool_card::scheme_value::PrintedHash;

/// What a check ran: a test suite, or only a build or a lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckKind {
    Test,
    Build,
}

/// One check's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Check {
    pub(crate) kind: CheckKind,
    pub(crate) passed: bool,
}

/// What a file row says about the checks after its last edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// The latest check after the last edit, and how it went.
    After(Check),
    /// No check ran after the last edit.
    None,
}

/// One file of a scope's summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeRow {
    pub(crate) path: String,
    pub(crate) added: usize,
    pub(crate) removed: usize,
    pub(crate) edits: usize,
    pub(crate) evidence: Evidence,
    /// The entries whose cells edited the file, oldest first.
    pub(crate) entries: Vec<usize>,
}

/// One scope's files, the most recently changed first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChangeSummary {
    pub(crate) rows: Vec<ChangeRow>,
    /// The scope's start is among the loaded entries: a turn whose prompt is loaded. The whole
    /// session's start is the transcript window's to say.
    pub(crate) start_loaded: bool,
}

/// The two scopes the panel toggles between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Turn,
    Session,
}

/// One edit a cell's card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Edit {
    path: String,
    added: usize,
    removed: usize,
}

/// What one entry says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Facts {
    Nothing,
    /// A user prompt: the start of a turn.
    Prompt,
    /// A settled kernel cell: the edits its card shows and the check it ran.
    Cell {
        edits: Vec<Edit>,
        check: Option<Check>,
    },
    /// A finished command outside a cell (a `[bash-done]` row, the user's `!` command).
    Check(Check),
}

impl Facts {
    /// What `entry` says: a settled kernel cell's edits (its `details.diffs`, the rows its card
    /// shows) and its check, a finished command's check, or a prompt.
    fn of(entry: &ChatEntry) -> Facts {
        let command_check = |command: &str, code: i64| {
            check_kind(command).map_or(Facts::Nothing, |kind| {
                Facts::Check(Check {
                    kind,
                    passed: code == 0,
                })
            })
        };
        match entry {
            ChatEntry::User { .. } => Facts::Prompt,
            ChatEntry::Tool(card) => {
                let Some(result) = card.result.as_ref().filter(|_| !card.result_partial) else {
                    return Facts::Nothing;
                };
                if !pa_types::ai::is_kernel_tool_name(&card.name) {
                    return Facts::Nothing;
                }
                let details = &result.details;
                let edits: Vec<Edit> = details
                    .get("diffs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        let path = entry.get("path").and_then(Value::as_str)?;
                        let diff = entry.get("diff").and_then(Value::as_str)?;
                        let (added, removed) =
                            crate::tool_card::ipython::diffs::count_changes(diff);
                        Some(Edit {
                            path: path.to_string(),
                            added,
                            removed,
                        })
                    })
                    .collect();
                // The kernel's own test summary lines, one per test run the cell waited for.
                let mut announced = details
                    .get("stdout")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| {
                        let counts = line.strip_prefix("[tests: ")?.strip_suffix(']')?;
                        counts.split(" \u{b7} ").find_map(|part| {
                            part.strip_suffix(" failed")
                                .and_then(|failed| failed.parse::<u64>().ok())
                        })
                    })
                    .peekable();
                let tests = announced.peek().is_some().then(|| Check {
                    kind: CheckKind::Test,
                    passed: announced.all(|failed| failed == 0),
                });
                // The one command the cell ran, when the cell's value is that command's result.
                let returned = details
                    .get("bashCommands")
                    .filter(|commands| commands.get("count").and_then(Value::as_u64) == Some(1))
                    .and_then(|commands| commands.get("first")?.as_str())
                    .and_then(check_kind)
                    .and_then(|kind| {
                        let printed = details.get("result")?.as_str()?;
                        let outcome = PrintedHash::parse(printed)?.bash_outcome()?;
                        match (outcome.running, outcome.timed_out, outcome.exit_code) {
                            (true, _, _) | (false, false, None) => None,
                            (false, true, _) => Some(Check {
                                kind,
                                passed: false,
                            }),
                            (false, false, Some(code)) => Some(Check {
                                kind,
                                passed: code == 0,
                            }),
                        }
                    });
                // Both seen: the summary says it was a test run, and a failure in either is the
                // cell's.
                let check = match (tests, returned) {
                    (Some(tests), Some(returned)) => Some(Check {
                        kind: CheckKind::Test,
                        passed: tests.passed && returned.passed,
                    }),
                    (Some(one), None) | (None, Some(one)) => Some(one),
                    (None, None) => None,
                };
                if edits.is_empty() && check.is_none() {
                    Facts::Nothing
                } else {
                    Facts::Cell { edits, check }
                }
            }
            ChatEntry::BashExecution(card) => match card.exit_code {
                Some(code) if !card.running && !card.cancelled => {
                    command_check(&card.command, code)
                }
                Some(_) | None => Facts::Nothing,
            },
            ChatEntry::BackgroundCompletion(row) => match row.outcome {
                // `[bash-done pid:N exit:M]`, then `Command: "<the command, JSON-encoded>"`.
                BackgroundOutcome::Shell {
                    exit_code: Some(code),
                    ..
                } => row
                    .content
                    .lines()
                    .find_map(|line| line.strip_prefix("Command: "))
                    .and_then(|command| serde_json::from_str::<String>(command).ok())
                    .map_or(Facts::Nothing, |command| command_check(&command, code)),
                BackgroundOutcome::Shell {
                    exit_code: None, ..
                }
                | BackgroundOutcome::Cell { .. } => Facts::Nothing,
            },
            ChatEntry::Status { .. }
            | ChatEntry::RunSummary(_)
            | ChatEntry::SlashCommand { .. }
            | ChatEntry::CompactionSummary { .. }
            | ChatEntry::Assistant(_)
            | ChatEntry::AgentMessage(_)
            | ChatEntry::SkillInvocation(_)
            | ChatEntry::InjectedPrompt(_)
            | ChatEntry::RefinementOutcome(_)
            | ChatEntry::CustomPanel(_) => Facts::Nothing,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct FileFold {
    path: String,
    added: usize,
    removed: usize,
    edits: usize,
    last_edit: usize,
    entries: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
struct ScopeFold {
    files: Vec<FileFold>,
    by_path: HashMap<String, usize>,
    /// The latest test run and the latest build, each with the entry it ran in.
    last_test: Option<(usize, Check)>,
    last_build: Option<(usize, Check)>,
    started: bool,
}

impl ScopeFold {
    fn checked(&mut self, index: usize, check: Check) {
        match check.kind {
            CheckKind::Test => self.last_test = Some((index, check)),
            CheckKind::Build => self.last_build = Some((index, check)),
        }
    }

    fn summary(&self) -> ChangeSummary {
        let mut files: Vec<&FileFold> = self.files.iter().collect();
        files.sort_by_key(|file| std::cmp::Reverse(file.last_edit));
        ChangeSummary {
            rows: files
                .into_iter()
                .map(|file| ChangeRow {
                    path: file.path.clone(),
                    added: file.added,
                    removed: file.removed,
                    edits: file.edits,
                    // A test run after the edit says more than a build after it.
                    evidence: [self.last_test, self.last_build]
                        .into_iter()
                        .flatten()
                        .find(|(at, _)| *at > file.last_edit)
                        .map_or(Evidence::None, |(_, check)| Evidence::After(check)),
                    entries: file.entries.clone(),
                })
                .collect(),
            start_loaded: self.started,
        }
    }
}

/// Both scopes folded together: a prompt starts the turn's over.
#[derive(Debug, Clone, Default)]
struct Fold {
    session: ScopeFold,
    turn: ScopeFold,
}

impl Fold {
    fn apply(&mut self, index: usize, facts: &Facts) {
        match facts {
            Facts::Nothing => {}
            Facts::Prompt => {
                self.turn = ScopeFold {
                    started: true,
                    ..ScopeFold::default()
                };
            }
            Facts::Cell { edits, check } => {
                for scope in [&mut self.session, &mut self.turn] {
                    for edit in edits {
                        let at = if let Some(at) = scope.by_path.get(&edit.path) {
                            *at
                        } else {
                            scope.files.push(FileFold {
                                path: edit.path.clone(),
                                ..FileFold::default()
                            });
                            scope
                                .by_path
                                .insert(edit.path.clone(), scope.files.len() - 1);
                            scope.files.len() - 1
                        };
                        let file = &mut scope.files[at];
                        file.added += edit.added;
                        file.removed += edit.removed;
                        file.edits += 1;
                        file.last_edit = index;
                        if file.entries.last() != Some(&index) {
                            file.entries.push(index);
                        }
                    }
                    if let Some(check) = check {
                        scope.checked(index, *check);
                    }
                }
            }
            Facts::Check(check) => {
                self.session.checked(index, *check);
                self.turn.checked(index, *check);
            }
        }
    }
}

/// The ledger the view keeps in step with its entries (see the module docs).
#[derive(Debug, Default)]
pub(crate) struct ChangeLedger {
    /// One read per entry; `None` until read, and again once the entry changed.
    facts: Vec<Option<Facts>>,
    /// The fold of the settled entries `[0, folded)`.
    settled: Fold,
    folded: usize,
    /// Bumped on every change to the entries.
    revision: u64,
    /// The turn's and the session's summaries, and the revision they were folded at.
    cached: Option<(u64, [ChangeSummary; 2])>,
    /// How many entries were read: the incremental-cost test counts them.
    #[cfg(test)]
    pub(crate) reads: usize,
}

impl ChangeLedger {
    /// An entry was appended.
    pub(crate) fn pushed(&mut self) {
        self.facts.push(None);
        self.revision += 1;
    }

    /// The last entry was removed.
    pub(crate) fn popped(&mut self) {
        self.facts.pop();
        self.changed_at(self.facts.len());
    }

    /// `range` was replaced by `inserted` entries.
    pub(crate) fn spliced(&mut self, range: std::ops::Range<usize>, inserted: usize) {
        let start = range.start;
        self.facts
            .splice(range, std::iter::repeat_n(None, inserted));
        self.changed_at(start);
    }

    /// Every entry was dropped.
    pub(crate) fn cleared(&mut self) {
        self.facts.clear();
        self.changed_at(0);
    }

    /// The entry at `index` changed in place.
    pub(crate) fn stale(&mut self, index: usize) {
        if let Some(slot) = self.facts.get_mut(index) {
            *slot = None;
        }
        self.changed_at(index);
    }

    /// A change at `index`: a kept fold that covers it folds again.
    fn changed_at(&mut self, index: usize) {
        if index < self.folded {
            self.settled = Fold::default();
            self.folded = 0;
        }
        self.revision += 1;
    }

    /// The revision the entries are at: the panel keys its own caches on it.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// `scope`'s summary of `chat`, the entries this ledger is kept in step with. Folded once
    /// per revision; asking again at the same revision reads nothing.
    pub(crate) fn summary(&mut self, chat: &[ChatEntry], scope: Scope) -> &ChangeSummary {
        if self
            .cached
            .as_ref()
            .is_none_or(|(revision, _)| *revision != self.revision)
        {
            // Entries the ledger never heard of cannot arrive through the view's mutators; a
            // count that disagrees reads every entry again rather than misattribute one.
            if self.facts.len() != chat.len() {
                self.facts = vec![None; chat.len()];
                self.settled = Fold::default();
                self.folded = 0;
            }
            // An entry that no longer changes on its own: the kept fold may cover it.
            let settled = |entry: &ChatEntry| match entry {
                ChatEntry::Tool(card) => {
                    card.aborted || (card.result.is_some() && !card.result_partial)
                }
                ChatEntry::Assistant(message) => !message.streaming,
                ChatEntry::BashExecution(card) => !card.running,
                ChatEntry::Status { .. }
                | ChatEntry::RunSummary(_)
                | ChatEntry::User { .. }
                | ChatEntry::SlashCommand { .. }
                | ChatEntry::CompactionSummary { .. }
                | ChatEntry::AgentMessage(_)
                | ChatEntry::SkillInvocation(_)
                | ChatEntry::InjectedPrompt(_)
                | ChatEntry::BackgroundCompletion(_)
                | ChatEntry::RefinementOutcome(_)
                | ChatEntry::CustomPanel(_) => true,
            };
            // Everything the kept fold does not cover is folded below: read what is not kept.
            for (slot, entry) in self.facts.iter_mut().zip(chat).skip(self.folded) {
                if slot.is_none() {
                    #[cfg(test)]
                    {
                        self.reads += 1;
                    }
                    *slot = Some(Facts::of(entry));
                }
            }
            let facts = |index: usize| self.facts[index].as_ref().unwrap_or(&Facts::Nothing);
            while self.folded < chat.len() && settled(&chat[self.folded]) {
                self.settled.apply(self.folded, facts(self.folded));
                self.folded += 1;
            }
            let summaries = if self.folded == chat.len() {
                [self.settled.turn.summary(), self.settled.session.summary()]
            } else {
                let mut fold = self.settled.clone();
                for index in self.folded..chat.len() {
                    fold.apply(index, facts(index));
                }
                [fold.turn.summary(), fold.session.summary()]
            };
            self.cached = Some((self.revision, summaries));
        }
        let Some((_, [turn, session])) = self.cached.as_ref() else {
            unreachable!("the summaries were folded above");
        };
        match scope {
            Scope::Turn => turn,
            Scope::Session => session,
        }
    }
}

/// Whether `command` runs a test suite or a build, by the conservative rule the README states:
/// one of its simple commands (split at `&&`, `||`, `;`, `|`, `&`, parentheses and line breaks),
/// after leading `NAME=value` assignments and the `time`, `env`, `command`, `exec`, `nice`,
/// `npx`, `bunx` and `timeout [options] DURATION` wrappers, is
/// - a test run: `cargo test`, `cargo nextest`, `make test`/`tests`/`check`,
///   `npm`/`pnpm`/`yarn`/`bun` `[run] test`, `pytest`, `py.test`, `python… -m pytest`/`unittest`,
///   `go test`, `jest`, `vitest`;
/// - a build: `cargo build`/`check`/`clippy`, `npm`/`pnpm`/`yarn`/`bun` `[run] build`, `go
///   build`/`vet`.
///
/// A program is matched by its file name (`/usr/bin/cargo`, `./node_modules/.bin/jest`); a
/// `cargo +toolchain` and options before a subcommand are skipped. A test run anywhere in the
/// command makes it a test run.
pub(crate) fn check_kind(command: &str) -> Option<CheckKind> {
    let mut found = None;
    for segment in command.split(['\n', ';', '|', '&', '(', ')']) {
        let mut words = segment.split_whitespace().peekable();
        loop {
            match words.peek().copied() {
                Some("time" | "env" | "command" | "exec" | "nice" | "npx" | "bunx") => {
                    words.next();
                }
                Some("timeout") => {
                    words.next();
                    while let Some(option) = words.next_if(|word| word.starts_with('-')) {
                        // The options that take their value as the next word.
                        if matches!(option, "-s" | "-k" | "--signal" | "--kill-after") {
                            words.next();
                        }
                    }
                    // The duration.
                    words.next();
                }
                Some(word)
                    if word.find('=').is_some_and(|at| {
                        at > 0
                            && word[..at]
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    }) =>
                {
                    words.next();
                }
                Some(_) | None => break,
            }
        }
        let Some(program) = words.next() else {
            continue;
        };
        let program = program.rsplit(['/', '\\']).next().unwrap_or(program);
        let program = program.strip_suffix(".exe").unwrap_or(program);
        let rest: Vec<&str> = words.collect();
        let mut subcommands = rest
            .iter()
            .copied()
            .filter(|word| !word.starts_with('+') && !word.starts_with('-'));
        let kind = match program {
            "cargo" => match subcommands.next() {
                Some("test" | "nextest") => Some(CheckKind::Test),
                Some("build" | "check" | "clippy") => Some(CheckKind::Build),
                Some(_) | None => None,
            },
            "make" | "gmake" => rest
                .iter()
                .any(|word| matches!(*word, "test" | "tests" | "check"))
                .then_some(CheckKind::Test),
            "npm" | "pnpm" | "yarn" | "bun" => {
                let script = match subcommands.next() {
                    Some("run") => subcommands.next(),
                    other => other,
                };
                match script {
                    Some("test") => Some(CheckKind::Test),
                    Some("build") => Some(CheckKind::Build),
                    Some(_) | None => None,
                }
            }
            "pytest" | "py.test" | "jest" | "vitest" => Some(CheckKind::Test),
            "go" => match subcommands.next() {
                Some("test") => Some(CheckKind::Test),
                Some("build" | "vet") => Some(CheckKind::Build),
                Some(_) | None => None,
            },
            python if python.starts_with("python") => {
                matches!(rest.as_slice(), ["-m", "pytest" | "unittest", ..])
                    .then_some(CheckKind::Test)
            }
            _ => None,
        };
        match kind {
            Some(CheckKind::Test) => return Some(CheckKind::Test),
            Some(CheckKind::Build) => found = Some(CheckKind::Build),
            None => {}
        }
    }
    found
}

#[cfg(test)]
mod tests;
