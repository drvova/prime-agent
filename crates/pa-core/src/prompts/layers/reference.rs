//! The kernel API reference ([`API_DOCS`]) cut into passages: the entries `(api-help name)`
//! prints, and the units `(api-search query)` ranks.
//!
//! A passage is one top-level bullet with the bullets nested under it, or one paragraph, tagged
//! with its topic and the section heading above it. A bullet that opens with a call form is that
//! function's entry. Search ranks passages with Okapi BM25 over their words: a word is a run of
//! letters, digits and hyphens, lowercased, with a plural `s` dropped; a hyphenated name also
//! counts as each of its parts (`fs-read` is `fs-read`, `fs` and `read`). A function's name and
//! the `##` heading above a passage count as its words, the name [`NAME_WEIGHT`] times. A query
//! word that is a function's name puts that function's entry ahead of every scored passage.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use super::{documented_calls, API_DOCS};

/// BM25's term-frequency saturation.
const K1: f64 = 1.2;
/// BM25's length normalization.
const B: f64 = 0.75;
/// How many times a function's name counts among its entry's words.
const NAME_WEIGHT: usize = 3;

/// One passage of the reference.
#[derive(Debug)]
pub(super) struct Passage {
    pub(super) topic: &'static str,
    /// The `##` heading the passage sits under; `None` above the first one.
    pub(super) section: Option<&'static str>,
    /// The function a bullet documents, when it opens with a call form.
    pub(super) function: Option<&'static str>,
    /// The passage's lines, joined with newlines, without a trailing one.
    pub(super) text: String,
}

/// The passages with their BM25 statistics, built at the first lookup.
struct Index {
    passages: Vec<Passage>,
    /// Each passage's word counts, in passage order.
    counts: Vec<HashMap<String, usize>>,
    /// Each passage's length in words.
    lengths: Vec<usize>,
    average_length: f64,
    /// How many passages hold each word.
    passages_with: HashMap<String, usize>,
}

static INDEX: LazyLock<Index> = LazyLock::new(|| {
    let mut passages = Vec::new();
    for (topic, doc) in API_DOCS {
        let mut section = None;
        let mut lines = doc.lines().skip(1).peekable();
        while let Some(line) = lines.next() {
            if let Some(heading) = line.strip_prefix("## ") {
                section = Some(heading.trim());
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            let mut text = line.to_string();
            let function = if line.starts_with("- ") {
                while let Some(nested) = lines.next_if(|next| next.starts_with("  ")) {
                    text.push('\n');
                    text.push_str(nested);
                }
                documented_calls(line)
                    .next()
                    .map(|call| call.split(' ').next().unwrap_or(call))
            } else {
                while let Some(more) = lines.next_if(|next| {
                    !next.trim().is_empty() && !next.starts_with("- ") && !next.starts_with('#')
                }) {
                    text.push('\n');
                    text.push_str(more);
                }
                None
            };
            passages.push(Passage {
                topic,
                section,
                function,
                text,
            });
        }
    }
    let mut counts = Vec::with_capacity(passages.len());
    let mut lengths = Vec::with_capacity(passages.len());
    let mut passages_with: HashMap<String, usize> = HashMap::new();
    for passage in &passages {
        let mut passage_words = words(&passage.text);
        passage_words.extend(passage.section.map_or_default(words));
        if let Some(name) = passage.function {
            let name_words = words(name);
            for _ in 1..NAME_WEIGHT {
                passage_words.extend(name_words.iter().cloned());
            }
        }
        lengths.push(passage_words.len());
        let mut count: HashMap<String, usize> = HashMap::new();
        for word in passage_words {
            *count.entry(word).or_default() += 1;
        }
        for word in count.keys() {
            *passages_with.entry(word.clone()).or_default() += 1;
        }
        counts.push(count);
    }
    let total: usize = lengths.iter().sum();
    let average_length = total as f64 / lengths.len().max(1) as f64;
    Index {
        passages,
        counts,
        lengths,
        average_length,
        passages_with,
    }
});

/// The entry of the function named `name`: its bullet and the bullets nested under it.
pub(super) fn function_entry(name: &str) -> Option<&'static Passage> {
    INDEX
        .passages
        .iter()
        .find(|passage| passage.function == Some(name))
}

/// The words of `text`, for the index and for a query alike.
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    for word in text
        .split(|letter: char| !(letter.is_alphanumeric() || letter == '-'))
        .map(|word| word.trim_matches('-'))
        .filter(|word| !word.is_empty())
    {
        let word = word.to_lowercase();
        if word.contains('-') {
            words.extend(
                word.split('-')
                    .filter(|part| !part.is_empty())
                    .map(singular),
            );
        }
        words.push(singular(&word));
    }
    words
}

/// `word` without a plural ending, so `files` finds `file` and `entries` finds `entry`.
fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies").filter(|stem| stem.len() > 1) {
        return format!("{stem}y");
    }
    match word.strip_suffix('s') {
        Some(stem) if stem.len() > 2 && !stem.ends_with('s') => stem.to_string(),
        Some(_) | None => word.to_string(),
    }
}

/// What `(api-search query #:limit)` prints: at most `limit` passages that share a word with
/// `query`, best first, each under its topic and section; `None` when no passage does.
pub(crate) fn api_search(query: &str, limit: usize) -> Option<String> {
    let index = &*INDEX;
    let mut terms = words(query);
    terms.sort_unstable();
    terms.dedup();
    let passage_count = index.passages.len() as f64;
    let weights: Vec<(&str, f64)> = terms
        .iter()
        .filter_map(|term| {
            let holding = *index.passages_with.get(term)? as f64;
            let idf = (1.0 + (passage_count - holding + 0.5) / (holding + 0.5)).ln();
            Some((term.as_str(), idf))
        })
        .collect();
    let mut ranked: Vec<(&Passage, f64)> = index
        .passages
        .iter()
        .zip(&index.counts)
        .zip(&index.lengths)
        .filter_map(|((passage, counts), length)| {
            let norm = K1 * (1.0 - B + B * *length as f64 / index.average_length);
            let score: f64 = weights
                .iter()
                .filter_map(|(term, idf)| {
                    let frequency = *counts.get(*term)? as f64;
                    Some(idf * frequency * (K1 + 1.0) / (frequency + norm))
                })
                .sum();
            (score > 0.0).then_some((passage, score))
        })
        .collect();
    // Entries of functions the query names come first, then the rest by score. The sort is
    // stable, so equal passages keep reference order.
    let named = |passage: &Passage| {
        passage.function.is_some_and(|name| {
            query
                .split(|letter: char| letter.is_whitespace() || "\"'`(),;".contains(letter))
                .any(|word| word == name)
        })
    };
    ranked.sort_by(|left, right| {
        named(right.0)
            .cmp(&named(left.0))
            .then(right.1.total_cmp(&left.1))
    });
    ranked.truncate(limit);
    if ranked.is_empty() {
        return None;
    }
    let mut printed = format!(
        "api-search: {} passage{} of the reference for {query:?}, best first. \
         (api-help \"<function>\") prints an entry, (api-help \"<topic>\") a topic.\n",
        ranked.len(),
        if ranked.len() == 1 { "" } else { "s" },
    );
    for (passage, _) in ranked {
        let _ = match passage.section {
            Some(section) => write!(printed, "\n[{} > {section}]\n", passage.topic),
            None => write!(printed, "\n[{}]\n", passage.topic),
        };
        let _ = writeln!(printed, "{}", passage.text);
    }
    Some(printed)
}
