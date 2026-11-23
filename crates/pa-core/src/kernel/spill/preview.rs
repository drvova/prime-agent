//! Surprise-weighted previews: when an output is spilled or thinned to a
//! preview, the preview keeps the lines that carry information, not the lines
//! that happen to sit at its ends (compression is prediction: the surprising
//! parts are the information; here that idea selects lines, it encodes
//! nothing).
//!
//! Within the same character budget as the head/tail cut it replaces, a
//! preview always ends with the output's last lines as one unbroken block
//! (up to [`TAIL_BLOCK_LINES`] shown lines, in at most half the budget, a run
//! of one line repeated shown once with its `×N` count): what a command
//! printed last, a traceback or a compiler's final error, is read whole and
//! in order. Of the lines before that block it keeps, in priority order: the
//! head anchors (first lines), error/warning/failure lines, then the lines
//! whose tokens are rarest in this output (a token seen once, especially a
//! number, path, or identifier, is surprising; one on every line is not).
//! Each distinct line there appears once, its repeats collapsed into a `×N`
//! count; lines matching a learned noise pattern (`super::noise`) collapse
//! into one marker per run. The kept lines print in their original order,
//! with a `[… N lines …]` gap marker where lines were left out. The whole
//! output stays one `(ctx-page handle)` or `(ctx-grep handle pattern)` away:
//! the output's handle follows its preview.
//!
//! Selection is a pure function of the text and the noise snapshot (ties
//! break by line position), so a request view built from it stays a pure
//! function of history.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use fancy_regex::Regex;

use super::noise::{line_shape, NoiseSet};

/// Lines always kept from the start, when they fit.
const HEAD_LINES: usize = 2;
/// The most lines of the unbroken block the preview ends with: a Python
/// traceback a few frames deep, or a compiler error with its source lines
/// and notes, is 10 to 30 lines. The block takes at most half the budget, so
/// the other half still shows what came before it (an earlier first error).
const TAIL_BLOCK_LINES: usize = 30;
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

/// `line` cut to at most `cap` characters, the cut marked with `…` (which
/// counts) and never inside a character or between a character and what
/// joins or marks it (a combining mark, a joiner, a variation selector, a
/// skin tone).
fn clip(line: &str, cap: usize) -> String {
    if line.chars().count() <= cap {
        return line.to_string();
    }
    let joins = |letter: char| {
        unicode_normalization::char::is_combining_mark(letter)
            || matches!(letter, '\u{200D}' | '\u{FE00}'..='\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}' | '\u{E0020}'..='\u{E007F}')
    };
    let letters: Vec<(usize, char)> = line.char_indices().collect();
    let mut end = cap.saturating_sub(1);
    // Back off while the first letter cut away belongs to the one before it,
    // or the last one kept is a joiner that expects one after it.
    while end > 0 && (joins(letters[end].1) || letters[end - 1].1 == '\u{200D}') {
        end -= 1;
    }
    format!("{}…", &line[..letters[end].0])
}

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
    let all: Vec<&str> = text.lines().collect();
    // The block the preview ends with: the last runs of lines that fit in
    // half the budget, each shown line with its newline.
    let half = budget / 2;
    let tail_cap = (MAX_LINE_CHARS + 1).min(half.saturating_sub(1));
    let (mut tail, mut tail_chars, mut tail_repeats) = (Vec::new(), 0, 0);
    let mut start = all.len();
    while start > 0 && tail.len() < TAIL_BLOCK_LINES {
        let line = all[start - 1];
        let run = all[..start]
            .iter()
            .rev()
            .take_while(|other| **other == line)
            .count();
        let mut shown = clip(line, tail_cap);
        if run > 1 {
            let _ = write!(shown, "  ×{run}");
        }
        let cost = shown.chars().count() + 1;
        if tail_chars + cost > half {
            break;
        }
        tail_chars += cost;
        tail_repeats += run - 1;
        tail.push(shown);
        start -= run;
    }
    tail.reverse();
    let lines = &all[..start];
    let budget = budget - tail_chars;
    // Distinct lines: each kept once, at its first position, with its count.
    let mut first_at: HashMap<&str, usize> = HashMap::new();
    let mut count = vec![0usize; lines.len()];
    for (at, line) in lines.iter().enumerate() {
        let first = *first_at.entry(line).or_insert(at);
        count[first] += 1;
    }
    let distinct: Vec<usize> = (0..lines.len()).filter(|at| count[*at] > 0).collect();
    let is_noise: Vec<bool> = lines
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
        if at < HEAD_LINES {
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
        .filter(|at| !is_noise[**at])
        .map(|at| (rank(*at), *at))
        .collect();
    candidates.sort();
    let shown = |at: usize| -> String {
        let mut out = clip(lines[at], MAX_LINE_CHARS + 1);
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
        let mut preview = render(lines, &count, &is_noise, &keep, &shown);
        if preview.text.chars().count() <= budget || chosen.is_empty() {
            preview.kept_lines += tail.len();
            preview.repeats_collapsed += tail_repeats;
            if !preview.text.is_empty() {
                preview.text.push('\n');
            }
            preview.text.push_str(&tail.join("\n"));
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
    is_noise: &[bool],
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
        let noise_line = is_noise[at];
        if noise_line {
            preview.noise_lines += line_count;
        } else {
            preview.dropped_lines += line_count;
        }
        run = match run {
            Some((kind, lines)) if kind == noise_line => Some((kind, lines + line_count)),
            other => {
                out.extend(other.map(marker));
                Some((noise_line, *line_count))
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
    use proptest::prelude::*;

    const NOISY_CARGO: &str = include_str!("fixtures/noisy_cargo.txt");
    const PYTHON_JOB_TRACEBACK: &str = include_str!("fixtures/python_job_traceback.txt");

    /// A long job that dies with a traceback no failure pattern names ends
    /// its preview with the whole traceback, in order, after the lines the
    /// selection kept from the progress before it.
    #[test]
    fn a_job_that_dies_with_a_traceback_ends_its_preview_with_all_of_it() {
        assert_eq!(
            build(PYTHON_JOB_TRACEBACK, 2000, &NoiseSet::default()),
            Some(Preview {
                text: "loading dataset from /data/shards (48 files)\nmodel: resnet50, params 25.6M, device cuda:0\nepoch 1 step   0/500 loss=1.4706 lr=0.0010 8 img/s\n[… 9 lines …]\nepoch 1 step 250/500 loss=1.0753 lr=0.0010 91 img/s\n[… 1 line …]\nepoch 1 step 300/500 loss=1.0204 lr=0.0010 108 img/s\n[… 3 lines …]\nepoch 1 step 400/500 loss=0.9259 lr=0.0010 141 img/s\n[… 1 line …]\nepoch 1 step 450/500 loss=0.8850 lr=0.0010 158 img/s\n[… 1 line …]\nepoch 1 done: val_acc=0.680 checkpoint saved to /work/ckpt/epoch1.pt\n[… 13 lines …]\nepoch 2 step 325/500 loss=0.7782 lr=0.0010 233 img/s\n[… 1 line …]\nepoch 2 step 375/500 loss=0.7491 lr=0.0010 266 img/s\nepoch 2 step 400/500 loss=0.7353 lr=0.0010 283 img/s\n[… 1 line …]\nepoch 2 step 450/500 loss=0.7092 lr=0.0010 316 img/s\nepoch 2 step 475/500 loss=0.6969 lr=0.0010 333 img/s\nepoch 2 done: val_acc=0.750 checkpoint saved to /work/ckpt/epoch2.pt\n[… 15 lines …]\nepoch 3 step 375/500 loss=0.6192 lr=0.0010 400 img/s\nepoch 3 step 400/500 loss=0.6098 lr=0.0010 425 img/s\nepoch 3 step 425/500 loss=0.6006 lr=0.0010 450 img/s\nepoch 3 step 450/500 loss=0.5917 lr=0.0010 475 img/s\nepoch 3 step 475/500 loss=0.5831 lr=0.0010 500 img/s\nepoch 3 done: val_acc=0.820 checkpoint saved to /work/ckpt/epoch3.pt\nepoch 4 step   0/500 loss=0.8812 lr=0.0010 0 img/s\nTraceback (most recent call last):\n  File \"/work/train.py\", line 142, in <module>\n    main()\n  File \"/work/train.py\", line 131, in main\n    loss = step(model, batch, optimizer)\n  File \"/work/train.py\", line 77, in step\n    logits = model(batch.images)\n  File \"/usr/lib/python3/site-packages/torch/nn/modules/module.py\", line 1532, in _call_impl\n    return forward_call(*args, **kwargs)\n  File \"/work/model.py\", line 58, in forward\n    x = self.layer4(x)\ntorch.OutOfMemoryError: CUDA out of memory. Tried to allocate 2.00 GiB (GPU 0; 15.78 GiB total capacity; 13.10 GiB already allocated)".to_string(),
                kept_lines: 33,
                dropped_lines: 45,
                repeats_collapsed: 0,
                noise_lines: 0,
            })
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 48,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x7a11_b10c),
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        /// Any output previews within its budget and ends with its own last
        /// line, never cut inside a character or a joined sequence.
        #[test]
        fn a_preview_keeps_its_budget_and_ends_with_the_last_line(
            lines in prop::collection::vec(
                prop_oneof![
                    "[a-z0-9 ./:_=-]{0,40}",
                    "(error|warning|Traceback|  File \"/w/a.py\", line [0-9]+)[ a-z]{0,30}",
                    "[\\PC]{0,260}",
                    "(e\u{301}|\u{1F469}\u{200D}\u{1F4BB}|\u{1F44D}\u{1F3FD}|x){80,140}",
                    Just("progress 50%".to_string()),
                ],
                1..60,
            ),
            budget in 64usize..3000,
            noisy in any::<bool>(),
        ) {
            let text = lines.join("\n");
            let noise = if noisy {
                NoiseSet::from_shapes(&["progress #%"])
            } else {
                NoiseSet::default()
            };
            let preview = build(&text, budget, &noise);
            prop_assert_eq!(preview.is_none(), text.chars().count() <= budget);
            if let Some(preview) = preview {
                prop_assert!(preview.text.chars().count() <= budget, "{} > {budget}", preview.text.chars().count());
                let last = text.lines().last().unwrap_or_default();
                let shown = preview.text.rsplit('\n').next().unwrap_or_default();
                // A repeated last line is shown once with its count.
                let shown = shown
                    .rsplit_once("  ×")
                    .filter(|(_, count)| count.parse::<usize>().is_ok())
                    .map_or(shown, |(line, _)| line);
                let kept = shown.strip_suffix('…').unwrap_or(shown);
                prop_assert!(last.starts_with(kept), "{shown:?} is not the end of {last:?}");
                let cut = last[kept.len()..].chars().next();
                prop_assert!(
                    kept.len() == last.len()
                        || !(kept.ends_with('\u{200D}')
                            || cut.is_some_and(|letter| unicode_normalization::char::is_combining_mark(letter)
                                || matches!(letter, '\u{200D}' | '\u{1F3FB}'..='\u{1F3FF}'))),
                    "{shown:?} cuts a joined sequence of {last:?}"
                );
            }
        }
    }

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
    /// folds the learned banners into one marker, ends with its last error
    /// whole, keeps under its budget, and is the same preview every time.
    #[test]
    fn a_noisy_build_previews_its_failures_and_first_occurrences() {
        let preview = build(NOISY_CARGO, 600, &cargo_noise());
        assert_eq!(
            preview,
            Some(Preview {
                text: "[… 11 noise lines …]\n   Compiling app v0.1.0 (/work/app)\nwarning: unused variable: `total`\n --> src/report.rs:14:9\nerror[E0308]: mismatched types\n --> src/lib.rs:6:22\n  |\n6 |     let count: u32 = \"three\";\n  |                ---   ^^^^^^^ expected `u32`, found `&str`\n  |\nFor more information about this error, try `rustc --explain E0308`.\nwarning: `app` (lib) generated 1 warning\nerror: could not compile `app` (lib) due to 1 previous error; 1 warning emitted".to_string(),
                kept_lines: 12,
                dropped_lines: 0,
                repeats_collapsed: 0,
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
            (Some("   Compiling proc-macro2 v1.0.86\n   Compiling unicode-ident v1.0.12\n[… 10 lines …]\nwarning: unused variable: `total`\n --> src/report.rs:14:9\nerror[E0308]: mismatched types\n --> src/lib.rs:6:22\n  |\n6 |     let count: u32 = \"three\";\n  |                ---   ^^^^^^^ expected `u32`, found `&str`\n  |\nFor more information about this error, try `rustc --explain E0308`.\nwarning: `app` (lib) generated 1 warning\nerror: could not compile `app` (lib) due to 1 previous error; 1 warning emitted".to_string()), None)
        );
    }
}
