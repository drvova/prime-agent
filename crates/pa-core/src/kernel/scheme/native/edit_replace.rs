//! `(edit path old-str new-str)`: replace one unique occurrence of a string
//! in an existing file, the `edit` skill of the Python kernel.
//!
//! The file is read and written as UTF-8, byte for byte. An exact match is
//! tried first. Only when it finds nothing, and `old-str` is not text pasted
//! from `fs-read` with its `LINE:abc|` anchors still on, does a ladder of
//! normalizations try in turn, each on its own: trailing whitespace ignored
//! per line, then leading and trailing whitespace ignored per line (the
//! replacement re-indented by the difference), then Unicode punctuation
//! folded to ASCII, then the literal `\n`/`\t`/`\"`/`\\` escapes a model
//! wrote into a Scheme string by mistake read as what they stand for. The
//! first step that finds anything must find exactly one match, or the edit
//! fails as ambiguous; the reply's `matchedBy` names the step. Whatever step
//! matched, only the matched text is replaced (whole lines for the two
//! whitespace steps). A miss says where `old-str` stops matching and shows
//! the file's lines where it most likely meant ([`closest`]), so the next
//! call can fix it without another read; a miss whose `new-str` is in the
//! file once says the edit looks already applied. The file is replaced whole
//! (a temp file beside it renamed onto it), never rewritten in place.

mod closest;

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};

use crate::fs_edit::Anchor;
use crate::tools::path_utils::resolve_to_cwd;

/// Characters of the file and of `old-str` shown at the divergence point.
const SPLIT_CONTEXT: usize = 60;

/// The visible characters a `new-str` needs before finding it in the file
/// once reads as the edit having been applied (the bound `OpenCode` uses): a shorter
/// one turns up by chance.
const ALREADY_APPLIED_MIN_CHARS: usize = 20;

/// How `old-str` was found in the file: the ladder's steps, in the order they
/// are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Matched {
    Exact,
    TrailingWhitespace,
    Indentation,
    UnicodePunctuation,
    Unescaped,
}

const LADDER: [Matched; 5] = [
    Matched::Exact,
    Matched::TrailingWhitespace,
    Matched::Indentation,
    Matched::UnicodePunctuation,
    Matched::Unescaped,
];

impl Matched {
    /// The reply's `matchedBy` word and the phrase errors and the reply's
    /// text use for the step.
    fn words(self) -> (&'static str, &'static str) {
        match self {
            Matched::Exact => ("exact", "exactly"),
            Matched::TrailingWhitespace => (
                "trailing-whitespace",
                "ignoring trailing whitespace on each line",
            ),
            Matched::Indentation => (
                "indentation",
                "ignoring indentation (new-str re-indented to match)",
            ),
            Matched::UnicodePunctuation => (
                "unicode-punctuation",
                "with curly quotes, dashes and non-breaking spaces read as ASCII",
            ),
            Matched::Unescaped => (
                "unescaped",
                "with old-str's literal \\n, \\t, \\\" and \\\\ escapes unescaped (new-str too)",
            ),
        }
    }
}

/// One place `old-str` matched: the file's bytes it covers and what replaces
/// them.
struct Candidate {
    range: Range<usize>,
    new: String,
}

/// An applied edit: the edited file, the reply, and the text that was
/// actually replaced and put in its place (what the cell's card shows).
pub(super) struct Replaced {
    pub(super) file: PathBuf,
    pub(super) reply: Value,
    pub(super) old: String,
    pub(super) new: String,
}

/// Why the ladder found nothing to replace (what the session counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MissOutcome {
    /// `old-str` carries `fs-read` anchor prefixes.
    AnchorPrefix,
    /// The first step to find `old-str` found it more than once.
    Ambiguous,
    /// No step found it.
    NotFound,
    /// No step found it, and `new-str` is in the file once.
    AlreadyApplied,
}

/// An edit the ladder refused: the error a cell sees, and why.
#[derive(Debug)]
pub(super) struct Miss {
    pub(super) outcome: MissOutcome,
    message: String,
}

impl std::fmt::Display for Miss {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Miss {}

/// Replace the one occurrence of `old-str` in the file at `path` (relative to
/// `cwd`, absolute, or `~`-prefixed). The reply carries `path`, `startLine`
/// (the 1-based line the match starts on), `matchedBy`, and `text`.
///
/// # Errors
///
/// Bad options, a file that is missing or not UTF-8, an `old-str` carrying
/// `fs-read` anchors, or an `old-str` that no step of the ladder finds or
/// that the first step to find it finds more than once; nothing is written
/// then.
pub(super) fn replace(cwd: &Path, options: &Value) -> anyhow::Result<Replaced> {
    let text = |name: &str| {
        options
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{name} must be a string"))
    };
    let (path, old, new) = (text("path")?, text("old-str")?, text("new-str")?);
    if path.is_empty() {
        bail!("path must be a non-empty string");
    }
    let file = PathBuf::from(resolve_to_cwd(path, &cwd.to_string_lossy()));
    if !file.exists() {
        bail!("{path} not found");
    }
    let content = std::fs::read_to_string(&file)
        .with_context(|| format!("{path} is not readable as UTF-8"))?;
    let mut found = None;
    for step in LADDER {
        let candidates = match step {
            Matched::Exact => substring_matches(&content, old, new),
            Matched::TrailingWhitespace => line_matches(&content, old, new, str::trim_end),
            Matched::Indentation => line_matches(&content, old, new, str::trim),
            Matched::UnicodePunctuation => {
                let fold = |ch: char| match ch {
                    '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
                    '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
                    '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
                    '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => {
                        ' '
                    }
                    other => other,
                };
                // The folded file, and for each of its bytes the file's byte
                // it came from (one past the end too).
                let mut folded = String::with_capacity(content.len());
                let mut origin = Vec::with_capacity(content.len() + 1);
                for (at, ch) in content.char_indices() {
                    let ch = fold(ch);
                    origin.extend(std::iter::repeat_n(at, ch.len_utf8()));
                    folded.push(ch);
                }
                origin.push(content.len());
                let needle: String = old.chars().map(fold).collect();
                folded
                    .match_indices(needle.as_str())
                    .map(|(at, _)| Candidate {
                        range: origin[at]..origin[at + needle.len()],
                        new: new.to_string(),
                    })
                    .collect()
            }
            Matched::Unescaped => {
                let escapes = ["\\n", "\\t", "\\\"", "\\\\"];
                let unescape = |text: &str| {
                    let mut out = String::with_capacity(text.len());
                    let mut chars = text.chars().peekable();
                    while let Some(ch) = chars.next() {
                        let follower = if ch == '\\' {
                            chars.next_if(|next| matches!(next, 'n' | 't' | '"' | '\\'))
                        } else {
                            None
                        };
                        out.push(match follower {
                            Some('n') => '\n',
                            Some('t') => '\t',
                            Some(other) => other,
                            None => ch,
                        });
                    }
                    out
                };
                // Only when old-str spells an escape the file never does:
                // a file that has `\n` in it may be what old-str means.
                let mistaken = escapes.iter().any(|escape| old.contains(escape))
                    && !escapes
                        .iter()
                        .any(|escape| old.contains(escape) && content.contains(escape));
                if mistaken {
                    substring_matches(&content, &unescape(old), &unescape(new))
                } else {
                    Vec::new()
                }
            }
        };
        match candidates.len() {
            0 => {}
            1 => {
                found = candidates.into_iter().next().map(|found| (step, found));
                break;
            }
            count => {
                let (_, how) = step.words();
                let how = match step {
                    Matched::Exact => String::new(),
                    Matched::TrailingWhitespace
                    | Matched::Indentation
                    | Matched::UnicodePunctuation
                    | Matched::Unescaped => format!(" {how}"),
                };
                return Err(Miss {
                    outcome: MissOutcome::Ambiguous,
                    message: format!(
                        "found {count} occurrences in {path}{how}, need exactly 1 — widen the snippet to make it unique"
                    ),
                }
                .into());
            }
        }
        if step == Matched::Exact {
            let mut lines = old.strip_suffix('\n').unwrap_or(old).split('\n');
            if lines.all(|line| line.contains('|') && Anchor::parse(line).is_ok()) {
                return Err(Miss {
                    outcome: MissOutcome::AnchorPrefix,
                    message: format!(
                        "old-str carries fs-read anchor prefixes (`LINE:abc|`), which are not in {path}: \
                         drop them and pass the file's text alone, or edit those lines with fs-edit and their anchors"
                    ),
                }
                .into());
            }
        }
    }
    let Some((matched, Candidate { range, new })) = found else {
        let applied_at = (new.trim().chars().count() >= ALREADY_APPLIED_MIN_CHARS)
            .then(|| {
                let mut found = content.match_indices(new);
                match (found.next(), found.next()) {
                    (Some((at, _)), None) => Some(at),
                    (Some(_), Some(_)) | (None, _) => None,
                }
            })
            .flatten();
        return Err(match applied_at {
            Some(at) => Miss {
                outcome: MissOutcome::AlreadyApplied,
                message: format!(
                    "string not found in {path}, but new-str is, once, at line {}: this edit looks already applied; \
                     read the file before editing it again",
                    content[..at].matches('\n').count() + 1
                ),
            },
            None => Miss {
                outcome: MissOutcome::NotFound,
                message: miss_diagnostic(path, &content, old),
            },
        }
        .into());
    };
    let start_line = content[..range.start].matches('\n').count() + 1;
    let replaced = content[range.clone()].to_string();
    let mut edited = content.clone();
    edited.replace_range(range, &new);
    // Replaced whole, as `fs-write` and `fs-edit` replace theirs: a reader,
    // or a daemon that dies mid-write, sees the old text or the new.
    crate::fs_edit::replace_atomically(&file, path, content.as_bytes(), edited.as_bytes())?;
    let file = file.canonicalize().unwrap_or(file);
    let (matched_by, how) = matched.words();
    let text = match matched {
        Matched::Exact => format!("Edited {}", file.display()),
        Matched::TrailingWhitespace
        | Matched::Indentation
        | Matched::UnicodePunctuation
        | Matched::Unescaped => format!(
            "Edited {} (old-str matched {how}; check the result)",
            file.display()
        ),
    };
    let reply = json!({
        "path": file.to_string_lossy(),
        "startLine": start_line,
        "matchedBy": matched_by,
        "text": text,
    });
    Ok(Replaced {
        file,
        reply,
        old: replaced,
        new,
    })
}

/// Every occurrence of `needle` in `content`, each replaced by `new`.
fn substring_matches(content: &str, needle: &str, new: &str) -> Vec<Candidate> {
    content
        .match_indices(needle)
        .map(|(at, _)| Candidate {
            range: at..at + needle.len(),
            new: new.to_string(),
        })
        .collect()
}

/// Every run of whole lines of `content` whose lines equal `old`'s lines
/// once each side is put through `normalize`. The matched block is those
/// lines (and the last one's line ending when `old` ends in one); `new`
/// replaces it, re-indented by the difference between the first non-blank
/// line's indentation in the file and in `old`, and with CRLF line endings
/// when the block has them and `new` has none. An `old` with no visible
/// text matches nothing.
fn line_matches(
    content: &str,
    old: &str,
    new: &str,
    normalize: fn(&str) -> &str,
) -> Vec<Candidate> {
    let (body, ends_line) = old
        .strip_suffix('\n')
        .map_or((old, false), |body| (body, true));
    let wanted: Vec<&str> = body.split('\n').collect();
    let Some(anchor) = wanted.iter().position(|line| !normalize(line).is_empty()) else {
        return Vec::new();
    };
    // Each line's start and the end of its text (before its `\n`, if any).
    let mut lines = Vec::new();
    let mut start = 0;
    for piece in content.split('\n') {
        lines.push(start..start + piece.len());
        start += piece.len() + 1;
    }
    let indentation = |line: &str| line.len() - line.trim_start().len();
    let mut candidates = Vec::new();
    for window in lines.windows(wanted.len()) {
        let equal = window
            .iter()
            .zip(&wanted)
            .all(|(line, old_line)| normalize(&content[line.clone()]) == normalize(old_line));
        if !equal {
            continue;
        }
        let last = &content[window[wanted.len() - 1].clone()];
        let end = window[wanted.len() - 1].end;
        let end = if ends_line {
            (end + 1).min(content.len())
        } else {
            end - usize::from(last.ends_with('\r'))
        };
        let file_line = &content[window[anchor].clone()];
        let file_indent = &file_line[..indentation(file_line)];
        let old_indent = &wanted[anchor][..indentation(wanted[anchor])];
        let mut replacement = if file_indent == old_indent {
            new.to_string()
        } else {
            new.split('\n')
                .map(|line| match line.strip_prefix(old_indent) {
                    Some(rest) if !line.trim().is_empty() => format!("{file_indent}{rest}"),
                    Some(_) | None => line.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        if content[window[0].clone()].ends_with('\r') && !replacement.contains('\r') {
            replacement = replacement.replace('\n', "\r\n");
        }
        candidates.push(Candidate {
            range: window[0].start..end,
            new: replacement,
        });
    }
    candidates
}

/// Say where `old` stops matching: the longest leading piece of it the file
/// contains (at its first occurrence), a bounded view of both sides at the
/// split, and a hint when the split is a line-ending or whitespace difference;
/// then the closest lines ([`closest::closest_lines`]) when a region is found.
fn miss_diagnostic(path: &str, content: &str, old: &str) -> String {
    // Containment is monotonic in the prefix length, so binary-search the
    // longest prefix the file contains (the whole of `old` is known absent).
    let starts: Vec<usize> = old.char_indices().map(|(start, _)| start).collect();
    let (mut low, mut high) = (0, starts.len().saturating_sub(1));
    while low < high {
        let middle = (low + high).div_ceil(2);
        if content.contains(&old[..starts[middle]]) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let mut message = 'split: {
        if low == 0 {
            break 'split format!(
            "string not found in {path}: the file does not contain even the first character of old-str"
        );
        }
        let matched = &old[..starts[low]];
        let point = content.find(matched).unwrap_or_default() + matched.len();
        let before = &content[..point];
        let line = before.matches('\n').count() + 1;
        let column = before
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count()
            + 1;
        let file_next: String = content[point..].chars().take(SPLIT_CONTEXT).collect();
        let old_next: String = old[starts[low]..].chars().take(SPLIT_CONTEXT).collect();
        let mut message = format!(
        "string not found in {path}: old-str matches its first {low} of {} characters and diverges at line {line}, column {column}:\n  file continues    {file_next:?}\n  old-str continues {old_next:?}",
        starts.len()
    );
        if file_next.starts_with("\r\n") && old_next.starts_with('\n') {
            message.push_str(
                "\nhint: the file has CRLF (\\r\\n) line endings; write them as \\r\\n in old-str",
            );
        } else if old_next.starts_with("\r\n") && file_next.starts_with('\n') {
            message.push_str("\nhint: old-str has CRLF (\\r\\n) line endings; the file has \\n");
        } else if [&file_next, &old_next]
            .iter()
            .any(|next| next.chars().next().is_some_and(char::is_whitespace))
        {
            message.push_str(
            "\nhint: whitespace differs at the split (indentation, tabs vs spaces, or trailing spaces)",
        );
        }
        message
    };
    if let Some(hint) = closest::closest_lines(content, old) {
        message.push('\n');
        message.push_str(&hint);
    }
    message
}
