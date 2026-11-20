//! Error enrichment: a failed cell, or a failed `bash()` command a cell
//! awaited, carries the source lines around each error location its output
//! names, so the model reads the failing code in the same result instead of
//! spending a round on `fs.read`.
//!
//! The registry in [`parsers`] finds the locations (rustc/cargo, Rust panics,
//! pytest, Python tracebacks and unittest, tsc, go, gcc/clang). Each is
//! resolved against the session's workspace (its working directory): only a
//! file inside it (and outside installed-package trees) gets its lines
//! attached, each location once, and the whole block stays under a fixed size.
//! A failed command's full output pages out to the session's spill store, so
//! the block names a handle to it instead of repeating it.
//!
//! The cell result never waits on the file system: the source reads run on a
//! blocking worker under a short budget, and a read that has not answered by
//! then leaves the locations without their lines.

mod parsers;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parsers::ParsedLocation;

use crate::kernel::shared::KernelCommandFailure;
use crate::kernel::spill::marker::group_digits;
use crate::kernel::spill::{SpillCopy, SpillStore, HANDLE_PREFIX};

/// Locations that get their source lines.
const MAX_LOCATIONS: usize = 6;
/// Parsed locations considered (resolved in order until [`MAX_LOCATIONS`] fit).
const MAX_CANDIDATES: usize = 24;
/// Lines shown on each side of an error line.
const CONTEXT_LINES: u32 = 3;
/// Characters of one source line shown.
const MAX_LINE_CHARS: usize = 200;
/// Characters of the whole block, past which further locations are counted, not shown.
const MAX_BLOCK_CHARS: usize = 6_000;
/// Characters of one text the parsers read (its head: compilers report the
/// first errors first).
const MAX_PARSE_CHARS: usize = 2 * 1024 * 1024;
/// Bytes of a source file read for its lines.
const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
/// Characters of a failed command shown in its handle line.
const MAX_COMMAND_CHARS: usize = 120;
/// How long the cell result waits for the source reads.
const READ_BUDGET: Duration = Duration::from_millis(250);
/// Path components whose files are never the project's own code.
const FOREIGN_DIRS: [&str; 3] = ["site-packages", "dist-packages", "node_modules"];

/// Reads the files an error location points into, for [`ErrorContext`].
///
/// Called only from a blocking worker thread under the enrichment's time
/// budget, never from an async task, so an implementation may block for as
/// long as its file system does: a slow or hung read costs only the snippet
/// (the cell result returns at the budget without it). `canonical` answers the
/// path with every symlink and `..` resolved (the containment check compares
/// canonical paths); `read` answers the file's UTF-8 text, refusing a file too
/// large to show a few lines of.
pub(crate) trait SourceReader: Send + Sync {
    fn canonical(&self, path: &Path) -> std::io::Result<PathBuf>;
    fn read(&self, path: &Path) -> std::io::Result<String>;
}

/// The real file system.
struct DiskReader;

impl SourceReader for DiskReader {
    fn canonical(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<String> {
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() > MAX_SOURCE_BYTES {
            return Err(std::io::Error::other("too large to show"));
        }
        std::io::read_to_string(file)
    }
}

/// What one enrichment did (adoption counts only: never a path or a line).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EnrichmentFact {
    /// Failed `bash()` commands the cell awaited.
    pub(crate) failed_commands: usize,
    /// Locations whose source lines were attached.
    pub(crate) locations: usize,
    /// Parsed locations left out: outside the workspace, unreadable, or past the cap.
    pub(crate) skipped: usize,
    /// The source reads did not answer within the budget.
    pub(crate) timed_out: bool,
}

/// Hears each enrichment of a failed cell or command.
pub(crate) type ErrorContextObserver = Arc<dyn Fn(EnrichmentFact) + Send + Sync>;

/// One session's error enrichment (see the module docs). The default is off,
/// as for a session without a workspace.
#[derive(Clone, Default)]
pub struct ErrorContext {
    inner: Option<Arc<Settings>>,
}

struct Settings {
    workspace: PathBuf,
    reader: Arc<dyn SourceReader>,
    budget: Duration,
    observer: Option<ErrorContextObserver>,
}

/// What one cell left that can name error locations.
pub(crate) struct CellFailure<'a> {
    /// The cell's exception text (`None` when the cell ended with a value).
    pub(crate) traceback: Option<&'a str>,
    /// The cell's stdout and stderr, read only when the cell raised.
    pub(crate) streams: [&'a str; 2],
    /// The failed `bash()` commands the cell awaited.
    pub(crate) commands: &'a [KernelCommandFailure],
}

/// One location resolved inside the workspace, with its lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    /// The path relative to the workspace, with `/`.
    display: String,
    location: ParsedLocation,
    /// `(line number, text)` around the error line.
    lines: Vec<(u32, String)>,
}

/// What the blocking worker found.
struct Reads {
    resolved: Vec<Resolved>,
    skipped: usize,
}

impl ErrorContext {
    /// Enrichment for a session working in `workspace`.
    pub(crate) fn for_workspace(
        workspace: PathBuf,
        observer: Option<ErrorContextObserver>,
    ) -> Self {
        Self::with_reader(workspace, Arc::new(DiskReader), READ_BUDGET, observer)
    }

    fn with_reader(
        workspace: PathBuf,
        reader: Arc<dyn SourceReader>,
        budget: Duration,
        observer: Option<ErrorContextObserver>,
    ) -> Self {
        Self {
            inner: Some(Arc::new(Settings {
                workspace,
                reader,
                budget,
                observer,
            })),
        }
    }

    /// The `[error context]` block for one cell, or `None` when it names no
    /// location inside the workspace. A failed command's output pages out to
    /// `spill` (when the session has a store) so the block can name it.
    /// Waits at most the read budget.
    pub(crate) async fn enrich(
        &self,
        failure: CellFailure<'_>,
        spill: Option<&SpillStore>,
    ) -> Option<String> {
        let settings = self.inner.as_ref()?;
        if failure.traceback.is_none() && failure.commands.is_empty() {
            return None;
        }
        let candidates = candidates(&failure);
        let mut fact = EnrichmentFact {
            failed_commands: failure.commands.len(),
            ..EnrichmentFact::default()
        };
        let block = if candidates.is_empty() {
            None
        } else {
            let worker = {
                let settings = Arc::clone(settings);
                let candidates = candidates.clone();
                tokio::task::spawn_blocking(move || {
                    read_locations(&settings.workspace, settings.reader.as_ref(), &candidates)
                })
            };
            // A worker that panicked counts like one that ran out of time.
            if let Ok(Ok(reads)) = tokio::time::timeout(settings.budget, worker).await {
                fact.locations = reads.resolved.len();
                fact.skipped = candidates.len() - reads.resolved.len();
                (!reads.resolved.is_empty())
                    .then(|| render(&reads.resolved, reads.skipped, failure.commands, spill))
            } else {
                fact.timed_out = true;
                fact.skipped = candidates.len();
                None
            }
        };
        if let Some(observer) = &settings.observer {
            observer(fact);
        }
        block
    }
}

/// The distinct locations the cell's texts name, the failed commands' first
/// (their output is the failure), in order, capped.
fn candidates(failure: &CellFailure<'_>) -> Vec<ParsedLocation> {
    let mut texts: Vec<&str> = failure
        .commands
        .iter()
        .map(|command| command.output.as_str())
        .collect();
    if let Some(traceback) = failure.traceback {
        texts.push(traceback);
        texts.extend(failure.streams);
    }
    let mut seen = HashSet::new();
    texts
        .into_iter()
        .flat_map(|text| parsers::parse(head(text, MAX_PARSE_CHARS)))
        .filter(|location| !location.path.starts_with('<'))
        .filter(|location| seen.insert((location.path.clone(), location.line)))
        .take(MAX_CANDIDATES)
        .collect()
}

fn head(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(end, _)| &text[..end])
}

/// Resolve each candidate inside the workspace and read its lines, until
/// [`MAX_LOCATIONS`] are found. Runs on a blocking worker.
fn read_locations(
    workspace: &Path,
    reader: &dyn SourceReader,
    candidates: &[ParsedLocation],
) -> Reads {
    let mut resolved = Vec::new();
    let mut skipped = 0;
    let Ok(root) = reader.canonical(workspace) else {
        return Reads {
            resolved,
            skipped: candidates.len(),
        };
    };
    let mut seen = HashSet::new();
    for location in candidates {
        if resolved.len() == MAX_LOCATIONS {
            skipped += 1;
            continue;
        }
        let path = Path::new(&location.path);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        };
        let Some(file) = reader.canonical(&joined).ok().filter(|file| {
            file.strip_prefix(&root).is_ok_and(|inner| {
                !inner.components().any(|part| match part {
                    Component::Normal(name) => FOREIGN_DIRS.iter().any(|dir| name == *dir),
                    Component::Prefix(_)
                    | Component::RootDir
                    | Component::CurDir
                    | Component::ParentDir => false,
                })
            })
        }) else {
            skipped += 1;
            continue;
        };
        // `./a.go` and `a.go` name one file.
        if !seen.insert((file.clone(), location.line)) {
            continue;
        }
        let Ok(text) = reader.read(&file) else {
            skipped += 1;
            continue;
        };
        let lines = lines_around(&text, location.line);
        if lines.is_empty() {
            skipped += 1;
            continue;
        }
        let display = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        resolved.push(Resolved {
            display,
            location: location.clone(),
            lines,
        });
    }
    Reads { resolved, skipped }
}

/// The lines within [`CONTEXT_LINES`] of 1-based `line` (empty past the end).
fn lines_around(text: &str, line: u32) -> Vec<(u32, String)> {
    let first = line.saturating_sub(CONTEXT_LINES).max(1);
    let last = line.saturating_add(CONTEXT_LINES);
    let lines: Vec<(u32, String)> = text
        .lines()
        .zip(1u32..)
        .skip_while(|(_, number)| *number < first)
        .take_while(|(_, number)| *number <= last)
        .map(
            |(text, number)| match text.char_indices().nth(MAX_LINE_CHARS) {
                Some((end, _)) => (number, format!("{}…", &text[..end])),
                None => (number, text.to_string()),
            },
        )
        .collect();
    if lines.iter().any(|(number, _)| *number == line) {
        lines
    } else {
        Vec::new()
    }
}

/// The block: each location with its lines, then one line per failed
/// command naming its full output's spill handle (when the session has a store).
fn render(
    resolved: &[Resolved],
    skipped: usize,
    commands: &[KernelCommandFailure],
    spill: Option<&SpillStore>,
) -> String {
    let mut block = format!(
        "[error context: {} location{} · source ±{CONTEXT_LINES} lines]",
        resolved.len(),
        if resolved.len() == 1 { "" } else { "s" },
    );
    let mut not_shown = 0;
    for entry in resolved {
        let mut section = String::new();
        let column = entry
            .location
            .column
            .map(|column| format!(":{column}"))
            .unwrap_or_default();
        let _ = write!(
            section,
            "\n{}:{}{column} ({})",
            entry.display,
            entry.location.line,
            entry.location.tool.label()
        );
        if !entry.location.message.is_empty() {
            let _ = write!(section, " {}", entry.location.message);
        }
        let width = entry
            .lines
            .last()
            .map_or(1, |(number, _)| number.to_string().len());
        for (number, text) in &entry.lines {
            let mark = if *number == entry.location.line {
                '>'
            } else {
                ' '
            };
            let _ = write!(section, "\n{mark} {number:>width$} | {text}");
        }
        if block.len() + section.len() > MAX_BLOCK_CHARS {
            not_shown += 1;
            continue;
        }
        block.push_str(&section);
    }
    let more = not_shown + skipped;
    if more > 0 {
        let _ = write!(
            block,
            "\n[{more} more location{} not shown]",
            if more == 1 { "" } else { "s" }
        );
    }
    let Some(store) = spill else {
        return block;
    };
    // A command awaited twice reports its failure twice: one line each.
    let mut listed = HashSet::new();
    for command in commands
        .iter()
        .filter(|command| !command.output.is_empty())
        .filter(|command| listed.insert((&command.command, &command.output)))
    {
        let handle = store.spill(SpillCopy::of_text(&command.output));
        let first_line = command.command.lines().next().unwrap_or_default();
        let shown = head(first_line, MAX_COMMAND_CHARS);
        let ellipsis = if shown.len() < command.command.len() {
            "…"
        } else {
            ""
        };
        let _ = write!(
            block,
            "\n[failed command (exit {}): {shown}{ellipsis} · full output {HANDLE_PREFIX}{} · {} chars · {} lines]",
            command.exit_code,
            handle.id,
            group_digits(handle.total_chars),
            group_digits(handle.lines),
        );
    }
    block
}
