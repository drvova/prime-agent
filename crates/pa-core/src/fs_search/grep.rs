//! `fs.grep`: ripgrep's searcher and regex matcher over an `ignore` walk,
//! in process — the same crates `rg` is built from, so the semantics match
//! (regex syntax, `.gitignore`/`.ignore`, `-g` globs, `-t` types, binary
//! files skipped, CRLF line ends) on every platform, with no `rg` installed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::anyhow;
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkState;

use super::{display_path, walker, Case, GrepMatch, GrepReport, GrepRequest, WalkFilter};

/// Build the matcher `request` describes.
fn matcher(request: &GrepRequest) -> anyhow::Result<RegexMatcher> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .case_insensitive(request.case == Case::Insensitive)
        .case_smart(request.case == Case::Smart)
        .fixed_strings(request.literal)
        .word(request.word)
        .crlf(true);
    if request.multiline {
        builder.multi_line(true).dot_matches_new_line(true);
    } else {
        builder.line_terminator(Some(b'\n'));
    }
    builder
        .build(&request.pattern)
        .map_err(|error| anyhow!("fs.grep: invalid pattern: {error}"))
}

/// One line of output text: lossy UTF-8 without its line terminator.
fn line_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.trim_end_matches(['\n', '\r']).to_string()
}

/// At most `max` chars of `text`, a window around char `focus` when cut.
pub(crate) fn clip(text: &str, focus: usize, max: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= max {
        return (text.to_string(), false);
    }
    let start = focus.saturating_sub(max / 3).min(count - max);
    let mut clipped: String = text.chars().skip(start).take(max).collect();
    if start > 0 {
        clipped.insert(0, '\u{2026}');
    }
    if start + max < count {
        clipped.push('\u{2026}');
    }
    (clipped, true)
}

/// Collects one file's matches with their context.
struct FileSink<'m> {
    matcher: &'m RegexMatcher,
    path: String,
    max_columns: usize,
    max_per_file: usize,
    before: Vec<String>,
    matches: Vec<GrepMatch>,
    /// The search-wide match budget, shared by every file.
    budget: &'m AtomicUsize,
    exhausted: &'m AtomicBool,
}

impl Sink for FileSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        // Claim one slot of the shared budget; none left stops every file.
        if self
            .budget
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_err()
        {
            self.exhausted.store(true, Ordering::SeqCst);
            return Ok(false);
        }
        let bytes = mat.bytes();
        let first_line = bytes.split(|byte| *byte == b'\n').next().unwrap_or(bytes);
        let column_byte = self
            .matcher
            .find(bytes)
            .ok()
            .flatten()
            .map_or(0, |found| found.start().min(first_line.len()));
        let text = line_text(first_line);
        let column = String::from_utf8_lossy(&first_line[..column_byte])
            .chars()
            .count();
        let (text, _) = clip(&text, column, self.max_columns);
        let extra_lines = mat.lines().count().saturating_sub(1);
        self.matches.push(GrepMatch {
            path: self.path.clone(),
            line: mat.line_number().unwrap_or_default(),
            column: column + 1,
            text,
            extra_lines,
            before: std::mem::take(&mut self.before),
            after: Vec::new(),
        });
        Ok(self.matches.len() < self.max_per_file)
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> Result<bool, Self::Error> {
        let (text, _) = clip(&line_text(context.bytes()), 0, self.max_columns);
        match context.kind() {
            SinkContextKind::Before => self.before.push(text),
            SinkContextKind::After => {
                if let Some(last) = self.matches.last_mut() {
                    last.after.push(text);
                }
            }
            SinkContextKind::Other => {}
        }
        Ok(true)
    }
}

/// Run `request`: walk in parallel, search each file, and sort the matches
/// by path and line. Blocking (run it off the async runtime).
///
/// # Errors
///
/// An invalid pattern, glob, or type, or a missing root.
pub(crate) fn grep(request: &GrepRequest) -> anyhow::Result<GrepReport> {
    let matcher = matcher(request)?;
    if !request.root.exists() {
        return Err(anyhow!(
            "fs.grep: {} does not exist",
            display_path(&request.root, &request.cwd)
        ));
    }
    let walk = walker(
        &request.root,
        &WalkFilter {
            scope: request.scope,
            globs: &request.globs,
            types: &request.types,
            max_depth: None,
        },
    )?;
    let budget = AtomicUsize::new(request.max_results);
    let exhausted = AtomicBool::new(false);
    let files_seen = AtomicUsize::new(0);
    let found: Mutex<Vec<GrepMatch>> = Mutex::new(Vec::new());
    let errors = AtomicUsize::new(0);
    walk.build_parallel().run(|| {
        let matcher = &matcher;
        let budget = &budget;
        let exhausted = &exhausted;
        let files_seen = &files_seen;
        let found = &found;
        let errors = &errors;
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .multi_line(request.multiline)
            .before_context(request.before)
            .after_context(request.after)
            .binary_detection(BinaryDetection::quit(0))
            .build();
        Box::new(move |entry| {
            if exhausted.load(Ordering::SeqCst) {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                errors.fetch_add(1, Ordering::Relaxed);
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return WalkState::Continue;
            }
            files_seen.fetch_add(1, Ordering::Relaxed);
            let mut sink = FileSink {
                matcher,
                path: display_path(entry.path(), &request.cwd),
                max_columns: request.max_columns,
                max_per_file: request.max_per_file,
                before: Vec::new(),
                matches: Vec::new(),
                budget,
                exhausted,
            };
            if searcher
                .search_path(matcher, entry.path(), &mut sink)
                .is_err()
            {
                errors.fetch_add(1, Ordering::Relaxed);
            }
            if !sink.matches.is_empty() {
                found
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .append(&mut sink.matches);
            }
            if exhausted.load(Ordering::SeqCst) {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });
    let mut collected = found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    collected.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    let mut files: Vec<&str> = collected.iter().map(|found| found.path.as_str()).collect();
    files.dedup();
    let files_matched = files.len();
    Ok(GrepReport {
        files_searched: files_seen.into_inner(),
        files_matched,
        truncated: exhausted.into_inner(),
        unreadable: errors.into_inner(),
        matches: collected,
    })
}
