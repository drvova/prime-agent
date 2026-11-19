//! Native in-process file search for the kernel: `fs.grep`, `fs.glob` and
//! `fs.find` (the REPL's `rlm.fs` module), served by `fs.*` host requests.
//!
//! The model would otherwise shell out to `rg`/`grep`/`find` through `bash`,
//! which needs those tools installed (Windows has none of them) and returns
//! unstructured text. Here the ripgrep library crates run in the host
//! process (after oh-my-pi's native grep/glob/find, MIT): `ignore` walks
//! with `.gitignore`/`.ignore` semantics in parallel, `globset` matches
//! globs, `grep-searcher` + `grep-regex` search. Results are structured
//! (path, line, column, text, context), capped and clipped so they stay
//! token-efficient, and paths print with `/` on every platform.
//!
//! Paths resolve in the kernel: the client sends the absolute root and the
//! REPL's working directory, and results show paths relative to it.

mod grep;
mod host;
#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail};
use globset::GlobBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use ignore::{WalkBuilder, WalkState};
use serde::Serialize;
use serde_json::{json, Value};

pub(crate) use grep::grep;
pub(crate) use host::{FsSearchHost, SearchObserver};

/// Default and largest `max_results` for `fs.grep`.
const GREP_DEFAULT_RESULTS: usize = 100;
const GREP_MAX_RESULTS: usize = 5_000;
/// Default and largest `max_results` for `fs.glob`/`fs.find`.
const LIST_DEFAULT_RESULTS: usize = 500;
const LIST_MAX_RESULTS: usize = 20_000;
/// Default and largest characters shown of one line.
const DEFAULT_MAX_COLUMNS: usize = 300;
const MAX_MAX_COLUMNS: usize = 10_000;
/// Largest context window on either side of a match.
const MAX_CONTEXT: usize = 50;

/// Which kernel operation ran (the telemetry bucket).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsOperation {
    Grep,
    Glob,
    Find,
}

/// Case matching for `fs.grep` (ripgrep's `-s`/`-i`/`-S`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Case {
    Sensitive,
    Insensitive,
    /// Insensitive unless the pattern has an uppercase letter.
    Smart,
}

/// One `fs.grep` to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepRequest {
    pub(crate) pattern: String,
    /// A directory or one file (absolute, resolved by the kernel).
    pub(crate) root: PathBuf,
    /// The REPL's working directory: paths print relative to it.
    pub(crate) cwd: PathBuf,
    /// ripgrep `-g` globs (`!` excludes; no `/` matches at any depth).
    pub(crate) globs: Vec<String>,
    /// ripgrep `-t` file types (`rust`, `py`, ...).
    pub(crate) types: Vec<String>,
    pub(crate) case: Case,
    /// The pattern is a literal string, not a regex.
    pub(crate) literal: bool,
    /// Match whole words only.
    pub(crate) word: bool,
    /// The pattern may span lines (`.` matches newlines).
    pub(crate) multiline: bool,
    pub(crate) before: usize,
    pub(crate) after: usize,
    pub(crate) scope: Scope,
    pub(crate) max_results: usize,
    pub(crate) max_per_file: usize,
    pub(crate) max_columns: usize,
}

/// One match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GrepMatch {
    pub(crate) path: String,
    /// 1-based line.
    pub(crate) line: u64,
    /// 1-based character column of the match's start.
    pub(crate) column: usize,
    /// The (first) matched line, clipped around the match.
    pub(crate) text: String,
    /// Further lines a multiline match spans.
    pub(crate) extra_lines: usize,
    pub(crate) before: Vec<String>,
    pub(crate) after: Vec<String>,
}

/// A finished `fs.grep`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepReport {
    pub(crate) matches: Vec<GrepMatch>,
    pub(crate) files_searched: usize,
    pub(crate) files_matched: usize,
    /// `max_results` stopped the search: more matches exist.
    pub(crate) truncated: bool,
    /// Entries the walk or the searcher could not read.
    pub(crate) unreadable: usize,
}

/// What a listed entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// One `fs.glob`/`fs.find` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FsEntry {
    pub(crate) path: String,
    pub(crate) kind: EntryKind,
    /// Bytes, for files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) size: Option<u64>,
}

/// A finished `fs.glob`/`fs.find`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listing {
    pub(crate) entries: Vec<FsEntry>,
    /// `max_results` stopped the walk: more entries exist.
    pub(crate) truncated: bool,
}

/// One `fs.glob`: a glob over paths relative to `root` (`*` stays inside a
/// directory, `**` crosses them, as in `pathlib`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlobRequest {
    pub(crate) pattern: String,
    pub(crate) root: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) scope: Scope,
    pub(crate) max_results: usize,
}

/// One `fs.find`: entries whose file name matches `name` (a glob, as
/// `find -name`), of `kind`, at most `max_depth` below `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FindRequest {
    pub(crate) name: Option<String>,
    pub(crate) ignore_case: bool,
    pub(crate) kind: Option<EntryKind>,
    pub(crate) max_depth: Option<usize>,
    pub(crate) root: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) scope: Scope,
    pub(crate) max_results: usize,
}

/// Which files a walk sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scope {
    /// Hidden files and directories too (never `.git`).
    pub(crate) hidden: bool,
    /// Respect `.gitignore`, `.ignore`, and git excludes.
    pub(crate) gitignore: bool,
}

impl Scope {
    /// ripgrep's default: visible files that no ignore rule excludes.
    pub(crate) const PROJECT: Scope = Scope {
        hidden: false,
        gitignore: true,
    };
}

fn parse_scope(data: &Value) -> anyhow::Result<Scope> {
    Ok(Scope {
        hidden: bool_field(data, "hidden", Scope::PROJECT.hidden)?,
        gitignore: bool_field(data, "gitignore", Scope::PROJECT.gitignore)?,
    })
}

/// What a walk skips and keeps.
pub(crate) struct WalkFilter<'a> {
    pub(crate) scope: Scope,
    pub(crate) globs: &'a [String],
    pub(crate) types: &'a [String],
    pub(crate) max_depth: Option<usize>,
}

/// A glob as the platform's matcher takes it: on Windows a `\` is a path
/// separator the model may type, not an escape.
fn native_glob(pattern: &str) -> String {
    if cfg!(windows) {
        pattern.replace('\\', "/")
    } else {
        pattern.to_string()
    }
}

/// The parallel walk over `root` that `filter` describes; `.git` is never
/// entered.
pub(crate) fn walker(root: &Path, filter: &WalkFilter<'_>) -> anyhow::Result<WalkBuilder> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!filter.scope.hidden)
        .git_ignore(filter.scope.gitignore)
        .git_global(filter.scope.gitignore)
        .git_exclude(filter.scope.gitignore)
        .ignore(filter.scope.gitignore)
        .parents(filter.scope.gitignore)
        .max_depth(filter.max_depth)
        .filter_entry(|entry| entry.file_name() != ".git");
    if !filter.globs.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        for glob in filter.globs {
            overrides
                .add(&native_glob(glob))
                .map_err(|error| anyhow!("invalid glob {glob:?}: {error}"))?;
        }
        builder.overrides(
            overrides
                .build()
                .map_err(|error| anyhow!("invalid globs: {error}"))?,
        );
    }
    if !filter.types.is_empty() {
        let mut types = TypesBuilder::new();
        types.add_defaults();
        for name in filter.types {
            types.select(name);
        }
        builder.types(
            types
                .build()
                .map_err(|error| anyhow!("invalid file type: {error}"))?,
        );
    }
    Ok(builder)
}

/// `path` as results show it: relative to `cwd` when under it, with `/`
/// separators on every platform.
pub(crate) fn display_path(path: &Path, cwd: &Path) -> String {
    let shown = path.strip_prefix(cwd).unwrap_or(path);
    let text = if shown.as_os_str().is_empty() {
        ".".into()
    } else {
        shown.to_string_lossy()
    };
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

fn entry_kind(file_type: Option<std::fs::FileType>) -> Option<EntryKind> {
    let file_type = file_type?;
    if file_type.is_symlink() {
        Some(EntryKind::Symlink)
    } else if file_type.is_dir() {
        Some(EntryKind::Dir)
    } else if file_type.is_file() {
        Some(EntryKind::File)
    } else {
        None
    }
}

/// Walk `walk` in parallel, keeping the entries `keep` accepts, at most
/// `max_results` of them, sorted by path.
fn list(
    walk: &WalkBuilder,
    root: &Path,
    cwd: &Path,
    max_results: usize,
    keep: &(dyn Fn(&ignore::DirEntry, EntryKind) -> bool + Sync),
) -> Listing {
    let found: Mutex<Vec<FsEntry>> = Mutex::new(Vec::new());
    let full = AtomicBool::new(false);
    walk.build_parallel().run(|| {
        let found = &found;
        let full = &full;
        Box::new(move |entry| {
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if entry.path() == root {
                return WalkState::Continue;
            }
            let Some(kind) = entry_kind(entry.file_type()) else {
                return WalkState::Continue;
            };
            if !keep(&entry, kind) {
                return WalkState::Continue;
            }
            let size = match kind {
                EntryKind::File => entry.metadata().ok().map(|metadata| metadata.len()),
                EntryKind::Dir | EntryKind::Symlink => None,
            };
            let mut entries = found
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if entries.len() >= max_results {
                full.store(true, Ordering::SeqCst);
                return WalkState::Quit;
            }
            entries.push(FsEntry {
                path: display_path(entry.path(), cwd),
                kind,
                size,
            });
            WalkState::Continue
        })
    });
    let mut entries = found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Listing {
        entries,
        truncated: full.into_inner(),
    }
}

/// Run an `fs.glob`. Blocking.
///
/// # Errors
///
/// An invalid glob or a missing root.
pub(crate) fn glob(request: &GlobRequest) -> anyhow::Result<Listing> {
    if !request.root.is_dir() {
        bail!(
            "fs.glob: {} is not a directory",
            display_path(&request.root, &request.cwd)
        );
    }
    let matcher = GlobBuilder::new(&native_glob(&request.pattern))
        .literal_separator(true)
        .build()
        .map_err(|error| anyhow!("fs.glob: invalid pattern: {error}"))?
        .compile_matcher();
    let walk = walker(
        &request.root,
        &WalkFilter {
            scope: request.scope,
            globs: &[],
            types: &[],
            max_depth: None,
        },
    )?;
    let root = request.root.as_path();
    Ok(list(
        &walk,
        root,
        &request.cwd,
        request.max_results,
        &|entry, _| {
            entry
                .path()
                .strip_prefix(root)
                .is_ok_and(|relative| matcher.is_match(relative))
        },
    ))
}

/// Run an `fs.find`. Blocking.
///
/// # Errors
///
/// An invalid name glob or a missing root.
pub(crate) fn find(request: &FindRequest) -> anyhow::Result<Listing> {
    if !request.root.is_dir() {
        bail!(
            "fs.find: {} is not a directory",
            display_path(&request.root, &request.cwd)
        );
    }
    let name = request
        .name
        .as_deref()
        .map(|name| {
            GlobBuilder::new(name)
                .case_insensitive(request.ignore_case)
                .literal_separator(true)
                .build()
                .map(|glob| glob.compile_matcher())
                .map_err(|error| anyhow!("fs.find: invalid name: {error}"))
        })
        .transpose()?;
    let walk = walker(
        &request.root,
        &WalkFilter {
            scope: request.scope,
            globs: &[],
            types: &[],
            max_depth: request.max_depth,
        },
    )?;
    Ok(list(
        &walk,
        &request.root,
        &request.cwd,
        request.max_results,
        &|entry, kind| {
            request.kind.is_none_or(|wanted| wanted == kind)
                && name
                    .as_ref()
                    .is_none_or(|name| name.is_match(entry.file_name()))
        },
    ))
}

fn count_of(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

impl GrepReport {
    /// The model-readable rendering: matches grouped under their file,
    /// `N:text` for a match and `N-text` for context (ripgrep's style),
    /// then a summary line.
    pub(crate) fn text(&self, max_results: usize) -> String {
        let mut out = String::new();
        let mut current: Option<&str> = None;
        let mut last_line = 0u64;
        for found in &self.matches {
            if current != Some(found.path.as_str()) {
                if current.is_some() {
                    out.push('\n');
                }
                out.push_str(&found.path);
                out.push('\n');
                current = Some(found.path.as_str());
                last_line = 0;
            }
            let first = found.line.saturating_sub(found.before.len() as u64);
            if last_line > 0
                && first > last_line + 1
                && !(found.before.is_empty() && found.after.is_empty())
            {
                out.push_str("--\n");
            }
            for (offset, line) in found.before.iter().enumerate() {
                let number = first + offset as u64;
                if number > last_line {
                    let _ = writeln!(out, "{number}-{line}");
                }
            }
            let _ = writeln!(out, "{}:{}", found.line, found.text);
            last_line = found.line + found.extra_lines as u64;
            for (offset, line) in found.after.iter().enumerate() {
                let number = found.line + 1 + offset as u64;
                let _ = writeln!(out, "{number}-{line}");
                last_line = number;
            }
        }
        let summary = if self.matches.is_empty() {
            format!(
                "No matches ({} searched).",
                count_of(self.files_searched, "file", "files")
            )
        } else if self.truncated {
            format!(
                "[{} in {} shown; stopped at max_results={max_results}: more exist, narrow the pattern, path, glob or type]",
                count_of(self.matches.len(), "match", "matches"),
                count_of(self.files_matched, "file", "files"),
            )
        } else {
            format!(
                "[{} in {} ({} searched)]",
                count_of(self.matches.len(), "match", "matches"),
                count_of(self.files_matched, "file", "files"),
                self.files_searched
            )
        };
        out.push_str(&summary);
        out
    }

    /// The host-request reply: the structured report plus its rendering.
    pub(crate) fn to_json(&self, max_results: usize) -> Value {
        json!({
            "matches": self.matches,
            "filesSearched": self.files_searched,
            "filesMatched": self.files_matched,
            "truncated": self.truncated,
            "unreadable": self.unreadable,
            "text": self.text(max_results),
        })
    }
}

impl Listing {
    /// One path per line (directories end in `/`), then a note when capped.
    pub(crate) fn text(&self, max_results: usize) -> String {
        let mut lines: Vec<String> = self
            .entries
            .iter()
            .map(|entry| match entry.kind {
                EntryKind::Dir => format!("{}/", entry.path),
                EntryKind::File | EntryKind::Symlink => entry.path.clone(),
            })
            .collect();
        if self.entries.is_empty() {
            lines.push("No matches.".to_string());
        } else if self.truncated {
            lines.push(format!(
                "[first {} shown; stopped at max_results={max_results}: more exist]",
                self.entries.len()
            ));
        }
        lines.join("\n")
    }

    pub(crate) fn to_json(&self, max_results: usize) -> Value {
        json!({
            "entries": self.entries,
            "truncated": self.truncated,
            "text": self.text(max_results),
        })
    }
}

/// What adoption telemetry learns of one call: counts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SearchFact {
    pub(crate) operation: FsOperation,
    /// Matches or entries returned; `None` when the call failed.
    pub(crate) results: Option<usize>,
    pub(crate) truncated: bool,
    pub(crate) elapsed: Duration,
}

fn path_field(data: &Value, key: &str) -> anyhow::Result<PathBuf> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow!("fs: {key} must be an absolute path"))
}

fn bool_field(data: &Value, key: &str, default: bool) -> anyhow::Result<bool> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(anyhow!("fs: {key} must be a boolean")),
    }
}

fn count_field(data: &Value, key: &str, default: usize, max: usize) -> anyhow::Result<usize> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .map(|count| count.min(max))
            .ok_or_else(|| anyhow!("fs: {key} must be a non-negative integer")),
    }
}

fn strings_field(data: &Value, key: &str) -> anyhow::Result<Vec<String>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(one)) => Ok(vec![one.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("fs: {key} must hold strings"))
            })
            .collect(),
        Some(_) => Err(anyhow!("fs: {key} must be a string or a list of strings")),
    }
}

/// Parse the `fs.grep` payload.
///
/// # Errors
///
/// A missing pattern or root, or an out-of-range option.
pub(crate) fn parse_grep(data: &Value) -> anyhow::Result<GrepRequest> {
    let pattern = data
        .get("pattern")
        .and_then(Value::as_str)
        .filter(|pattern| !pattern.is_empty())
        .ok_or_else(|| anyhow!("fs.grep requires a non-empty pattern"))?
        .to_string();
    let case = match data.get("case").and_then(Value::as_str) {
        None | Some("sensitive") => Case::Sensitive,
        Some("insensitive") => Case::Insensitive,
        Some("smart") => Case::Smart,
        Some(other) => bail!("fs.grep case must be sensitive, insensitive or smart; got {other}"),
    };
    let max_results = count_field(data, "maxResults", GREP_DEFAULT_RESULTS, GREP_MAX_RESULTS)?;
    Ok(GrepRequest {
        pattern,
        root: path_field(data, "root")?,
        cwd: path_field(data, "cwd")?,
        globs: strings_field(data, "globs")?,
        types: strings_field(data, "types")?,
        case,
        literal: bool_field(data, "literal", false)?,
        word: bool_field(data, "word", false)?,
        multiline: bool_field(data, "multiline", false)?,
        before: count_field(data, "before", 0, MAX_CONTEXT)?,
        after: count_field(data, "after", 0, MAX_CONTEXT)?,
        scope: parse_scope(data)?,
        max_results: max_results.max(1),
        max_per_file: count_field(data, "maxPerFile", usize::MAX, usize::MAX)?.max(1),
        max_columns: count_field(data, "maxColumns", DEFAULT_MAX_COLUMNS, MAX_MAX_COLUMNS)?.max(20),
    })
}

/// Parse the `fs.glob` payload.
///
/// # Errors
///
/// A missing pattern or root.
pub(crate) fn parse_glob(data: &Value) -> anyhow::Result<GlobRequest> {
    Ok(GlobRequest {
        pattern: data
            .get("pattern")
            .and_then(Value::as_str)
            .filter(|pattern| !pattern.is_empty())
            .ok_or_else(|| anyhow!("fs.glob requires a non-empty pattern"))?
            .to_string(),
        root: path_field(data, "root")?,
        cwd: path_field(data, "cwd")?,
        scope: parse_scope(data)?,
        max_results: count_field(data, "maxResults", LIST_DEFAULT_RESULTS, LIST_MAX_RESULTS)?
            .max(1),
    })
}

/// Parse the `fs.find` payload.
///
/// # Errors
///
/// A missing root or an unknown kind.
pub(crate) fn parse_find(data: &Value) -> anyhow::Result<FindRequest> {
    let kind = match data.get("kind").and_then(Value::as_str) {
        None => None,
        Some("file") => Some(EntryKind::File),
        Some("dir") => Some(EntryKind::Dir),
        Some("symlink") => Some(EntryKind::Symlink),
        Some(other) => bail!("fs.find kind must be file, dir or symlink; got {other}"),
    };
    Ok(FindRequest {
        name: data
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        ignore_case: bool_field(data, "ignoreCase", false)?,
        kind,
        max_depth: match data.get("maxDepth") {
            None | Some(Value::Null) => None,
            Some(_) => Some(count_field(data, "maxDepth", 0, usize::MAX)?),
        },
        root: path_field(data, "root")?,
        cwd: path_field(data, "cwd")?,
        scope: parse_scope(data)?,
        max_results: count_field(data, "maxResults", LIST_DEFAULT_RESULTS, LIST_MAX_RESULTS)?
            .max(1),
    })
}
