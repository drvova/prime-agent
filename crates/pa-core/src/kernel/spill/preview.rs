//! Surprise-weighted previews: when an output is spilled or thinned to a
//! preview, the preview keeps the lines that carry information, not the lines
//! that happen to sit at its ends (compression is prediction: the surprising
//! parts are the information; here that idea selects lines, it encodes
//! nothing).
//!
//! Within the same character budget as the head/tail cut it replaces, a
//! preview keeps, in priority order: the head and tail anchors (first and
//! last lines), error/warning/failure lines, then the lines whose tokens are
//! rarest in this output (a token seen once, especially a number, path, or
//! identifier, is surprising; one on every line is not). Each distinct line
//! appears once, its repeats collapsed into a `×N` count; lines matching a
//! learned noise pattern (`super::noise`) collapse into one marker per run.
//! The kept lines print in their original order, with a `[… N lines …]` gap
//! marker where lines were left out. The whole output stays one `ctx.page`
//! away.
//!
//! Selection is a pure function of the text and the noise snapshot (ties
//! break by line position), so a request view built from it stays a pure
//! function of history.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use fancy_regex::Regex;

use super::noise::{line_shape, NoiseSet};

/// Lines always kept from each end, when they fit.
const HEAD_LINES: usize = 2;
const TAIL_LINES: usize = 2;
/// Characters of one kept line.
const MAX_LINE_CHARS: usize = 200;
/// Characters a gap marker is budgeted at.
const GAP_CHARS: usize = 16;

static FAILURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:error|errors|warning|fail|failed|failure|failures|panic|panicked|exception|traceback|assert\w*|fatal|denied|not found|undefined|cannot|unexpected)\b|^E\s{2,}|\S+\.\w+:\d+(?::\d+)?(?:[:\s]|$)",
    )
    .expect("the failure pattern compiles")
});
/// The failure lines that outrank the rest: errors, then error locations.
static SEVERE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:error|errors|fail|failed|failure|panic|panicked|exception|traceback|assert\w*|fatal)\b|^E\s{2,}")
        .expect("the severe pattern compiles")
});
static LOCATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\S+\.\w+:\d+(?::\d+)?(?:[:\s]|$)").expect("the location pattern compiles")
});
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z_][\w./:-]*|\d[\w.:-]*").expect("the token pattern compiles")
});

/// Whether `line` reads as an error, warning, or failure.
pub(crate) fn is_failure_line(line: &str) -> bool {
    FAILURE.is_match(line).unwrap_or(false)
}

/// One built preview and what it chose (the counts feed telemetry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Preview {
    pub(crate) text: String,
    pub(crate) kept_lines: usize,
    pub(crate) dropped_lines: usize,
    /// Repeated lines folded into a kept line's `×N`.
    pub(crate) repeats_collapsed: usize,
    /// Lines folded into noise markers.
    pub(crate) noise_lines: usize,
}

/// What one line is, for selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Anchor,
    /// Errors and the location right under each (0), then other locations
    /// (1), then warnings and the rest (2).
    Failure(u8),
    /// Rarer first: the negated, scaled surprise score.
    Surprise(i64),
}

/// The preview of `text` within `budget` characters (`None` when the text
/// fits whole: nothing to choose).
pub(crate) fn build(text: &str, budget: usize, noise: &NoiseSet) -> Option<Preview> {
    if text.chars().count() <= budget {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    // Distinct lines: each kept once, at its first position, with its count.
    let mut first_at: HashMap<&str, usize> = HashMap::new();
    let mut count = vec![0usize; lines.len()];
    for (at, line) in lines.iter().enumerate() {
        let first = *first_at.entry(line).or_insert(at);
        count[first] += 1;
    }
    let distinct: Vec<usize> = (0..lines.len()).filter(|at| count[*at] > 0).collect();
    let noisy: Vec<bool> = lines
        .iter()
        .map(|line| !is_failure_line(line) && noise.contains(&line_shape(line)))
        .collect();
    // Token rarity over the whole output (each line counted once per token).
    let mut token_lines: HashMap<&str, usize> = HashMap::new();
    for &at in &distinct {
        let mut seen: Vec<&str> = Vec::new();
        for token in TOKEN.find_iter(lines[at]).flatten() {
            if !seen.contains(&token.as_str()) {
                seen.push(token.as_str());
                *token_lines.entry(token.as_str()).or_default() += 1;
            }
        }
    }
    let rank = |at: usize| -> Rank {
        if at < HEAD_LINES || at + TAIL_LINES >= lines.len() {
            return Rank::Anchor;
        }
        if is_failure_line(lines[at]) {
            let severe = |line: &str| SEVERE.is_match(line).unwrap_or(false);
            let line = lines[at];
            let located = LOCATION.is_match(line).unwrap_or(false);
            // An error's own location line (the one right under it) ranks with it.
            let severity = if severe(line) || (located && severe(lines[at - 1])) {
                0
            } else if located {
                1
            } else {
                2
            };
            return Rank::Failure(severity);
        }
        let tokens: Vec<&str> = TOKEN
            .find_iter(lines[at])
            .flatten()
            .map(|m| m.as_str())
            .collect();
        if tokens.is_empty() {
            return Rank::Surprise(0);
        }
        let score: f64 = tokens
            .iter()
            .map(|token| {
                let seen = token_lines.get(token).copied().unwrap_or(1) as f64;
                let shaped = token.contains(['/', '.', ':'])
                    || token.starts_with(|c: char| c.is_ascii_digit());
                (if shaped { 2.0 } else { 1.0 }) / seen
            })
            .sum::<f64>()
            / (tokens.len() as f64).sqrt();
        Rank::Surprise(-((score * 1000.0).round() as i64))
    };
    let mut candidates: Vec<(Rank, usize)> = distinct
        .iter()
        .filter(|at| !noisy[**at])
        .map(|at| (rank(*at), *at))
        .collect();
    candidates.sort();
    let shown = |at: usize| -> String {
        let line = lines[at];
        let mut out = match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((end, _)) => format!("{}…", &line[..end]),
            None => line.to_string(),
        };
        if count[at] > 1 {
            let _ = write!(out, "  ×{}", count[at]);
        }
        out
    };
    // Greedy by rank under an estimate (each kept line plus one marker),
    // then drop the lowest-ranked kept lines until the rendered text fits.
    let mut keep = vec![false; lines.len()];
    let mut chosen: Vec<usize> = Vec::new();
    let mut used = GAP_CHARS;
    for (_, at) in &candidates {
        let cost = shown(*at).chars().count() + 1 + GAP_CHARS;
        if used + cost > budget {
            continue;
        }
        used += cost;
        keep[*at] = true;
        chosen.push(*at);
    }
    loop {
        let preview = render(&lines, &count, &noisy, &keep, &shown);
        if preview.text.chars().count() <= budget || chosen.is_empty() {
            return Some(preview);
        }
        if let Some(lowest) = chosen.pop() {
            keep[lowest] = false;
        }
    }
}

/// The kept lines in order, each run of noise or left-out lines as one marker.
fn render(
    lines: &[&str],
    count: &[usize],
    noisy: &[bool],
    keep: &[bool],
    shown: &dyn Fn(usize) -> String,
) -> Preview {
    let mut out: Vec<String> = Vec::new();
    let mut run: Option<(bool, usize)> = None;
    let mut preview = Preview {
        text: String::new(),
        kept_lines: 0,
        dropped_lines: 0,
        repeats_collapsed: 0,
        noise_lines: 0,
    };
    let marker = |(noise, lines): (bool, usize)| {
        let plural = if lines == 1 { "" } else { "s" };
        if noise {
            format!("[… {lines} noise line{plural} …]")
        } else {
            format!("[… {lines} line{plural} …]")
        }
    };
    for (at, line_count) in count.iter().enumerate().take(lines.len()) {
        if *line_count == 0 {
            continue; // a repeat, folded into its first occurrence
        }
        if keep[at] {
            out.extend(run.take().map(marker));
            out.push(shown(at));
            preview.kept_lines += 1;
            preview.repeats_collapsed += line_count - 1;
            continue;
        }
        let noise = noisy[at];
        if noise {
            preview.noise_lines += line_count;
        } else {
            preview.dropped_lines += line_count;
        }
        run = match run {
            Some((kind, lines)) if kind == noise => Some((kind, lines + line_count)),
            other => {
                out.extend(other.map(marker));
                Some((noise, *line_count))
            }
        };
    }
    out.extend(run.map(marker));
    preview.text = out.join("\n");
    preview
}

#[cfg(test)]
mod tests {
    use super::{build, Preview};
    use crate::kernel::spill::noise::NoiseSet;

    const NOISY_CARGO: &str = include_str!("fixtures/noisy_cargo.txt");

    /// The shapes a project learned as noise for its `cargo` runs: the dependency
    /// banners and the progress bar.
    fn cargo_noise() -> NoiseSet {
        NoiseSet::from_shapes(&[
            "Building [=======> ] #/#: serde_derive",
            "Compiling libc v#",
            "Compiling memchr v#",
            "Compiling proc-macro# v#",
            "Compiling quote v#",
            "Compiling serde v#",
            "Compiling serde_derive v#",
            "Compiling syn v#",
            "Compiling unicode-ident v#",
        ])
    }

    /// A noisy failed build previews its error, warning, and location lines,
    /// folds a repeated line into `×N` and the learned banners into one
    /// marker, keeps under its budget, and is the same preview every time.
    #[test]
    fn a_noisy_build_previews_its_failures_and_first_occurrences() {
        let preview = build(NOISY_CARGO, 600, &cargo_noise());
        assert_eq!(
            preview,
            Some(Preview {
                text: "[… 11 noise lines …]\n   Compiling app v0.1.0 (/work/app)\nwarning: unused variable: `total`\n --> src/report.rs:14:9\nerror[E0308]: mismatched types\n --> src/lib.rs:6:22\n  |  ×2\n6 |     let count: u32 = \"three\";\n[… 1 line …]\nFor more information about this error, try `rustc --explain E0308`.\nwarning: `app` (lib) generated 1 warning\nerror: could not compile `app` (lib) due to 1 previous error; 1 warning emitted".to_string(),
                kept_lines: 10,
                dropped_lines: 1,
                repeats_collapsed: 1,
                noise_lines: 11,
            })
        );
        assert!(preview
            .as_ref()
            .is_some_and(|p| p.text.chars().count() <= 600));
        assert_eq!(build(NOISY_CARGO, 600, &cargo_noise()), preview);
    }

    /// Without learned noise the same output still keeps its failure lines
    /// over the banners that fill a head/tail cut; text that fits whole is no
    /// preview at all.
    #[test]
    fn without_noise_the_failures_still_win_and_short_text_stays_whole() {
        assert_eq!(
            (
                build(NOISY_CARGO, 600, &NoiseSet::default()).map(|preview| preview.text),
                build("ok\n", 600, &NoiseSet::default()),
            ),
            (Some("   Compiling proc-macro2 v1.0.86\n   Compiling unicode-ident v1.0.12\n[… 10 lines …]\nwarning: unused variable: `total`\n --> src/report.rs:14:9\nerror[E0308]: mismatched types\n --> src/lib.rs:6:22\n[… 2 lines …]\n6 |     let count: u32 = \"three\";\n[… 1 line …]\nFor more information about this error, try `rustc --explain E0308`.\nwarning: `app` (lib) generated 1 warning\nerror: could not compile `app` (lib) due to 1 previous error; 1 warning emitted".to_string()), None)
        );
    }
}
