//! The read-only command cache: an idempotent read-only command (`git
//! status`/`log`/`diff`, `ls`, `rg`/`grep`, `cat`) whose inputs have not
//! changed answers from its last result at once, marked `cached`, instead of
//! running again.
//!
//! Only a strict allowlist is cached: one simple command (no pipe, redirect,
//! chain, substitution, expansion, or glob) from the programs above, with no
//! option that writes or runs another program. A result is kept under the
//! exact command (the session's working directory never changes) and is
//! valid while the files it read are unchanged: each entry holds a
//! fingerprint (size, modification time, inode) of what the command depends
//! on (the named files for `cat`; the listed directories for `ls`; the
//! searched tree for `rg`/`grep`; the work tree with `.git/HEAD`, `index`,
//! and `packed-refs` for `git`), taken before the command ran and again when
//! it ended. An `fs-edit` or `edit` forgets every entry, and any other write
//! misses through the fingerprint. A tree too large to fingerprint cheaply is
//! never cached.
//!
//! Only commands are cached. `fs-read` is answered by the session's own
//! `fs.read` handler and never from here.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::job::Finished;

/// The `git` subcommands that only read.
const GIT_READS: [&str; 3] = ["status", "log", "diff"];
/// The programs whose listed uses only read.
const PROGRAMS: [&str; 5] = ["git", "ls", "cat", "rg", "grep"];
/// Options that write a file or run another program.
const WRITING_OPTIONS: [&str; 4] = ["--output", "--ext-diff", "--pre", "--exec"];
/// Shell syntax that makes a command more than one read.
const SHELL_SYNTAX: &str = "|;&<>$`()*?[]{}~\\\n!";
/// Files a fingerprint may stat before the command counts as uncacheable.
const MAX_FINGERPRINT_ENTRIES: usize = 5_000;
/// Entries kept, and the longest output kept per entry.
const MAX_ENTRIES: usize = 64;
const MAX_OUTPUT_CHARS: usize = 256 * 1024;
/// Directories a tree fingerprint never walks into: writes there never
/// change these reads.
const SKIPPED_DIRS: [&str; 5] = [".git", "node_modules", "target", "__pycache__", ".venv"];

/// What a cacheable command depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Plan {
    /// Files it names.
    files: Vec<String>,
    /// Directories it lists.
    dirs: Vec<String>,
    /// Trees it walks.
    trees: Vec<String>,
    /// It reads the repository's state.
    git: bool,
}

/// One path's state: its size, modification time, and inode, or `None` when
/// it cannot be read.
type Stamp = (PathBuf, Option<(u64, SystemTime, u64)>);

/// The state of everything a command read.
pub(super) type Fingerprint = Vec<Stamp>;

/// The dependencies of an allowlisted read-only `command`, or `None` when it
/// is not cacheable.
pub(super) fn plan(command: &str) -> Option<Plan> {
    if command.chars().any(|letter| SHELL_SYNTAX.contains(letter)) {
        return None;
    }
    // Shell words: with the syntax above excluded, only quotes remain.
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = None;
    for letter in command.chars() {
        if quote == Some(letter) {
            quote = None;
        } else if quote.is_none() && (letter == '\'' || letter == '"') {
            quote = Some(letter);
            word.get_or_insert_with(String::new);
        } else if quote.is_none() && letter.is_whitespace() {
            words.extend(word.take());
        } else {
            word.get_or_insert_with(String::new).push(letter);
        }
    }
    if quote.is_some() {
        return None;
    }
    words.extend(word);
    let (program, arguments) = words.split_first()?;
    if !PROGRAMS.contains(&program.as_str())
        || arguments.iter().any(|argument| {
            WRITING_OPTIONS
                .iter()
                .any(|option| argument.starts_with(option))
        })
    {
        return None;
    }
    let operands: Vec<String> = arguments
        .iter()
        .filter(|argument| !argument.starts_with('-'))
        .cloned()
        .collect();
    let here = || vec![".".to_string()];
    let none = Vec::new;
    match program.as_str() {
        "git" => arguments
            .first()
            .is_some_and(|subcommand| GIT_READS.contains(&subcommand.as_str()))
            .then(|| Plan {
                files: none(),
                dirs: none(),
                trees: here(),
                git: true,
            }),
        "cat" => (!operands.is_empty()).then(|| Plan {
            files: operands,
            dirs: none(),
            trees: none(),
            git: false,
        }),
        "ls" => {
            let recursive = arguments.iter().any(|argument| {
                argument.starts_with('-') && !argument.starts_with("--") && argument.contains('R')
            });
            let targets = if operands.is_empty() {
                here()
            } else {
                operands
            };
            Some(if recursive {
                Plan {
                    files: none(),
                    dirs: none(),
                    trees: targets,
                    git: false,
                }
            } else {
                Plan {
                    files: none(),
                    dirs: targets,
                    trees: none(),
                    git: false,
                }
            })
        }
        // rg/grep: the first operand is the pattern (unless -e/-f gave it);
        // the rest are paths.
        "rg" | "grep" => {
            let pattern_given = arguments.iter().any(|argument| {
                argument == "-f"
                    || ["-e", "--regexp", "--file"]
                        .iter()
                        .any(|option| argument.starts_with(option))
            });
            let paths: Vec<String> = operands
                .into_iter()
                .skip(usize::from(!pattern_given))
                .collect();
            // grep without a path reads stdin.
            (program != "grep" || !paths.is_empty()).then(|| Plan {
                files: none(),
                dirs: none(),
                trees: if paths.is_empty() { here() } else { paths },
                git: false,
            })
        }
        _unlisted => None,
    }
}

fn stamp(path: PathBuf) -> Stamp {
    let state = std::fs::metadata(&path).ok().map(|metadata| {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
        #[cfg(not(unix))]
        let inode = 0;
        (
            metadata.len(),
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            inode,
        )
    });
    (path, state)
}

/// The stamps of everything `plan` reads under `cwd`, or `None` when there
/// are too many to take cheaply.
pub(super) fn fingerprint(cwd: &Path, plan: &Plan) -> Option<Fingerprint> {
    let mut stamps: Fingerprint = plan
        .files
        .iter()
        .map(|name| stamp(cwd.join(name)))
        .collect();
    let mut budget = MAX_FINGERPRINT_ENTRIES;
    // A directory's entries by name, files apart from directories; one that
    // cannot be read has none.
    let listing = |directory: &Path| {
        let mut files = Vec::new();
        let mut subdirs = Vec::new();
        for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                subdirs.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
        files.sort();
        subdirs.sort();
        (files, subdirs)
    };
    for name in &plan.dirs {
        let directory = cwd.join(name);
        let (files, subdirs) = listing(&directory);
        budget = budget.checked_sub(files.len() + subdirs.len())?;
        stamps.push(stamp(directory));
        stamps.extend(files.into_iter().chain(subdirs).map(stamp));
    }
    if plan.git {
        stamps.extend(
            ["HEAD", "index", "packed-refs"]
                .into_iter()
                .map(|name| stamp(cwd.join(".git").join(name))),
        );
    }
    for name in &plan.trees {
        let root = cwd.join(name);
        stamps.push(stamp(root.clone()));
        let mut pending = vec![root];
        while let Some(directory) = pending.pop() {
            if !directory.is_dir() {
                continue;
            }
            let (files, subdirs) = listing(&directory);
            budget = budget.checked_sub(files.len() + 1)?;
            stamps.push(stamp(directory));
            stamps.extend(files.into_iter().map(stamp));
            // A linked directory is stamped by its parent's listing, not walked.
            pending.extend(subdirs.into_iter().rev().filter(|subdir| {
                !subdir.is_symlink()
                    && subdir.file_name().is_none_or(|name| {
                        !SKIPPED_DIRS
                            .iter()
                            .any(|skipped| name.to_str() == Some(skipped))
                    })
            }));
        }
    }
    Some(stamps)
}

/// The session's cached results, least recently used first.
#[derive(Default)]
pub(super) struct CommandCache {
    /// Raised by every write the kernel makes itself: a result from before it
    /// is never kept.
    pub(super) epoch: u64,
    entries: VecDeque<(String, Fingerprint, Finished)>,
}

impl CommandCache {
    /// Forget every entry: the kernel wrote a file.
    pub(super) fn invalidate(&mut self) {
        self.epoch += 1;
        self.entries.clear();
    }

    /// The result kept for `command` while `stamps` still hold.
    pub(super) fn lookup(&mut self, command: &str, stamps: &Fingerprint) -> Option<Finished> {
        let at = self
            .entries
            .iter()
            .position(|(kept, taken, _)| kept == command && taken == stamps)?;
        let entry = self.entries.remove(at)?;
        let result = entry.2.clone();
        self.entries.push_back(entry);
        Some(result)
    }

    /// Keep `result` for `command` under the stamps taken before it ran. A
    /// write since then (a newer epoch) or an output too long to keep leaves
    /// the cache as it was.
    pub(super) fn remember(
        &mut self,
        command: &str,
        stamps: Fingerprint,
        epoch: u64,
        result: Finished,
    ) {
        if epoch != self.epoch || result.output.chars().count() > MAX_OUTPUT_CHARS {
            return;
        }
        self.entries.retain(|(kept, _, _)| kept != command);
        self.entries
            .push_back((command.to_string(), stamps, result));
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }
}
