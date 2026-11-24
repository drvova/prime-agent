//! Where an `old-str` that matched nowhere most likely meant: the file's
//! lines there, numbered, and the first line pair that differs (the voting miss
//! hint of `OpenCode`, reimplemented).
//!
//! Each line of `old-str` with a letter or digit in it votes for the file
//! line `old-str` would start on: a file line equal to it once whitespace
//! runs are folded votes twice, and, for a line no file line equals, a file
//! line holding at least half of its words votes once. The start with the
//! most weight wins (the earliest on a tie) if it has at least half of what
//! the voting lines could give, so the region is found even when the first
//! line is the wrong one. A line or word on more than [`MAX_CANDIDATES`]
//! file lines says nothing about where `old-str` is and does not vote.
//!
//! The cost is one pass over the file (a key and the words of each line)
//! plus at most [`MAX_CANDIDATES`] votes per word of the first
//! [`MAX_OLD_LINES`] lines of `old-str`; a file of more than
//! [`MAX_FILE_LINES`] lines gets no hint. Measured on a 100,000-line file
//! (3.6 MB) with a near miss at its end: about 34 ms in a release build,
//! about 0.6 s in a debug one.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Lines of a file past which no closest lines are looked for.
const MAX_FILE_LINES: usize = 200_000;
/// Lines of `old-str` that vote (the region shown still spans all of it).
const MAX_OLD_LINES: usize = 200;
/// File lines a line or a word may be on and still vote.
const MAX_CANDIDATES: usize = 64;
/// Characters shown of one line.
const LINE_CHARS: usize = 200;
/// Characters of the shown region in all.
const REGION_CHARS: usize = 2_000;

/// `line` with its whitespace runs folded to one space and trimmed, into
/// `key` (reused across lines: the file pass allocates nothing per line).
fn fold(line: &str, key: &mut String) {
    key.clear();
    for word in line.split_whitespace() {
        if !key.is_empty() {
            key.push(' ');
        }
        key.push_str(word);
    }
}
/// A line's words: runs of letters, digits and `_`, two characters or more.
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .filter(|word| word.chars().nth(1).is_some())
}

/// The closest lines to `old` in `content`, or `None` when no region gets
/// enough votes: a header naming the region, the region's lines (one line
/// before and after `old`'s span) as `N: text`, and the first line of the
/// span that differs from `old`'s line with both sides quoted.
pub(super) fn closest_lines(content: &str, old: &str) -> Option<String> {
    let body = content.strip_suffix('\n').unwrap_or(content);
    let file: Vec<&str> = body
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if file.len() > MAX_FILE_LINES {
        return None;
    }
    let wanted: Vec<&str> = old
        .strip_suffix('\n')
        .unwrap_or(old)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    // (offset in old-str, folded line, its words) of each line that votes.
    let voters: Vec<(usize, String, BTreeSet<&str>)> = wanted
        .iter()
        .take(MAX_OLD_LINES)
        .enumerate()
        .map(|(at, line)| {
            let mut key = String::new();
            fold(line, &mut key);
            (at, key, words(line).collect())
        })
        .filter(|(_, key, _)| key.chars().any(char::is_alphanumeric))
        .collect();
    let wanted_keys: HashSet<&str> = voters.iter().map(|(_, key, _)| key.as_str()).collect();
    let wanted_words: HashSet<&str> = voters
        .iter()
        .flat_map(|(_, _, words)| words.iter().copied())
        .collect();
    // One pass: the file lines each wanted key and word is on, kept up to one
    // past the cap (which marks it as saying nothing).
    let mut key_lines: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut word_lines: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut key = String::new();
    for (at, line) in file.iter().enumerate() {
        fold(line, &mut key);
        if let Some(found) = wanted_keys.get(key.as_str()) {
            let lines = key_lines.entry(found).or_default();
            if lines.len() <= MAX_CANDIDATES {
                lines.push(at);
            }
        }
        for word in words(line) {
            if let Some(found) = wanted_words.get(word) {
                let lines = word_lines.entry(found).or_default();
                // A word twice on one line is on it once.
                if lines.len() <= MAX_CANDIDATES && lines.last() != Some(&at) {
                    lines.push(at);
                }
            }
        }
    }
    let telling = |lines: &&Vec<usize>| lines.len() <= MAX_CANDIDATES;
    let mut tally: BTreeMap<isize, usize> = BTreeMap::new();
    let mut possible = 0;
    for (offset, key, line_words) in &voters {
        let offset = offset.cast_signed();
        match key_lines.get(key.as_str()) {
            Some(lines) if !telling(&lines) => continue,
            Some(lines) => {
                for at in lines {
                    *tally.entry(at.cast_signed() - offset).or_default() += 2;
                }
            }
            None => {
                let rare: Vec<&Vec<usize>> = line_words
                    .iter()
                    .filter_map(|word| word_lines.get(word).filter(telling))
                    .collect();
                let mut shared: BTreeMap<usize, usize> = BTreeMap::new();
                for at in rare.iter().copied().flatten() {
                    *shared.entry(*at).or_default() += 1;
                }
                for (at, count) in shared {
                    if count * 2 >= rare.len() {
                        *tally.entry(at.cast_signed() - offset).or_default() += 1;
                    }
                }
            }
        }
        possible += 2;
    }
    let (start, weight) = tally.iter().fold(
        None,
        |best: Option<(isize, usize)>, (&start, &weight)| match best {
            Some((_, most)) if most >= weight => best,
            Some(_) | None => Some((start, weight)),
        },
    )?;
    if weight * 2 < possible {
        return None;
    }
    let cut = |line: &str| match line.char_indices().nth(LINE_CHARS) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    };
    let span = usize::try_from(start).unwrap_or(0);
    let first = span.saturating_sub(1);
    let end = (span + wanted.len() + 1).min(file.len());
    let width = end.to_string().len();
    let mut hint = format!(
        "closest lines in the file (lines {}-{end}; the numbers are not part of the file):",
        first + 1
    );
    let mut shown = 0;
    for (at, line) in file.iter().enumerate().take(end).skip(first) {
        if shown > REGION_CHARS {
            hint.push_str("\n  …");
            break;
        }
        let row = format!("\n  {:>width$}: {}", at + 1, cut(line));
        shown += row.chars().count();
        hint.push_str(&row);
    }
    let differs = wanted.iter().enumerate().find_map(|(offset, old_line)| {
        let at = usize::try_from(start + offset.cast_signed()).ok()?;
        let file_line = file.get(at)?;
        (file_line != old_line).then_some((at, *file_line, *old_line))
    });
    Some(match differs {
        Some((at, file_line, old_line)) => format!(
            "{hint}\nfirst difference, at line {}:\n  file    {:?}\n  old-str {:?}",
            at + 1,
            cut(file_line),
            cut(old_line)
        ),
        None => hint,
    })
}
