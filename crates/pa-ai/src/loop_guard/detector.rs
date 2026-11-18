//! One stream of text's repetition detector: an exact-cycle check over the stream's tail and,
//! at the semantic level, near-duplicate and stalled paragraph checks.
//!
//! Adapted from Nodecode's stream loop guard (`src/engine/loop-guard.lisp`, MIT), itself a port
//! of oh-my-pi's thinking-loop guard (`packages/ai/src/utils/thinking-loop.ts`, MIT). The
//! thresholds are theirs, calibrated on real model output. This port measures in UTF-8 bytes
//! rather than characters, and is more conservative on structured output: hex-literal cycles
//! (`0x00, 0x00, ...`) count as data like digits, and the paragraph checks also drop list items,
//! table rows and data lines (hex dumps, number columns), which repeat one skeleton by design.

use std::collections::{HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::LazyLock;

/// A set of precomputed 64-bit hashes (words, word trigrams): the keys are already hashes, so
/// the set does not hash them again.
type HashedSet = HashSet<u64, BuildHasherDefault<PrehashedKey>>;

/// The [`Hasher`] of a [`HashedSet`]: it passes the precomputed `u64` key through.
#[derive(Default)]
struct PrehashedKey(u64);

impl Hasher for PrehashedKey {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0 = fnv1a(self.0, bytes);
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

/// FNV-1a over `bytes`, continuing from `seed` (0 starts a fresh hash).
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = if seed == 0 {
        0xcbf2_9ce4_8422_2325
    } else {
        seed
    };
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Bytes of the stream's tail the exact-cycle check scans.
const TAIL_WINDOW: usize = 4096;
/// The longest exact cycle considered, in bytes.
const MAX_UNIT: usize = 1024;
/// New bytes between exact scans; one delta this long is scanned at once. The scan is linear in
/// the window, so the stride keeps the check O(n) amortized over the stream.
const CHECK_STRIDE: usize = 128;
/// A cycle this short or shorter needs four repeats; a longer one, three.
const SHORT_UNIT: usize = 60;
/// What a short one-line cycle must cover to count as a loop.
const SHORT_REPEATED: usize = 180;
/// What a long cycle, a cycle spanning a line break, or a cycle without letters must cover: a
/// status line restated a few times is ordinary output.
const LONG_REPEATED: usize = 1024;
/// A paragraph with no blank line past this many bytes is cut here, so a wall of text still
/// segments.
const SEGMENT_CAP: usize = 700;
/// Normalized bytes below which a paragraph is too short to judge.
const SEGMENT_FLOOR: usize = 60;
/// Recent paragraphs a new one is compared against.
const SEGMENT_WINDOW: usize = 16;
/// Paragraphs judged before either paragraph check may fire.
const SEGMENT_WARMUP: usize = 8;
/// Near-duplicates (the new paragraph among them) that make a loop.
const SEGMENT_CLUSTER: usize = 4;
/// Recent paragraphs whose pooled words are the novelty baseline.
const NOVELTY_WINDOW: usize = 8;
/// Recycling paragraphs in a row, none naming anything new, that make a loop.
const STALL_RUN: usize = 8;

/// A concrete reference the model reasons about: a code span, a file name or dotted member, a path
/// of three parts or more, a snake, camel or Pascal identifier. A paragraph naming a new one is
/// progress, which spares per-file work from the stall check.
static ANCHOR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"`[^`]+`|\b\w{2,}\.[a-zA-Z]\w{0,4}\b|[\w-]+(?:/[\w-]+){2,}|\b\w+_\w+\b|\b[a-z]+[A-Z]\w*\b|\b[A-Z][a-z]+[A-Z]\w*\b",
    )
    .expect("static regex")
});

/// What one stream of text has said so far, as the loop checks need it.
pub(crate) struct LoopDetector {
    semantic: bool,
    /// The stream's tail (at most twice the window; trimmed back to the window).
    tail: Vec<u8>,
    unscanned: usize,
    /// The Z-array scratch the exact scan reuses.
    z: Vec<usize>,
    /// Text not yet a whole paragraph, and where the next break search starts.
    pending: String,
    scan_from: usize,
    trigrams: VecDeque<HashedSet>,
    words: VecDeque<HashedSet>,
    anchors: VecDeque<HashSet<String>>,
    judged: usize,
    stall: usize,
}

impl LoopDetector {
    pub(crate) fn exact() -> Self {
        Self::with_paragraph_checks(false)
    }

    pub(crate) fn semantic() -> Self {
        Self::with_paragraph_checks(true)
    }

    fn with_paragraph_checks(semantic: bool) -> Self {
        Self {
            semantic,
            tail: Vec::new(),
            unscanned: 0,
            z: Vec::new(),
            pending: String::new(),
            scan_from: 0,
            trigrams: VecDeque::new(),
            words: VecDeque::new(),
            anchors: VecDeque::new(),
            judged: 0,
            stall: 0,
        }
    }

    /// Feed `delta`, the next streamed text: the loop it completes, in words, or `None`.
    pub(crate) fn push(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        self.tail.extend_from_slice(delta.as_bytes());
        if self.tail.len() > 2 * TAIL_WINDOW {
            self.tail.drain(..self.tail.len() - TAIL_WINDOW);
        }
        // At a bounded cadence: a scan per token-sized delta would be quadratic.
        self.unscanned += delta.len();
        if self.unscanned >= CHECK_STRIDE {
            self.unscanned = 0;
            if let Some(hit) = self.exact_cycle_detail() {
                return Some(hit);
            }
        }
        if !self.semantic {
            return None;
        }
        self.pending.push_str(delta);
        while let Some(paragraph) = self.take_paragraph() {
            if let Some(hit) = self.judge_paragraph(&paragraph) {
                return Some(hit);
            }
        }
        None
    }

    /// Judge what the detector still holds at the end of its stream: the exact check once more,
    /// whatever its cadence, and the last paragraph, which may complete a cluster.
    pub(crate) fn flush(&mut self) -> Option<String> {
        if let Some(hit) = self.exact_cycle_detail() {
            return Some(hit);
        }
        if self.semantic && !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.scan_from = 0;
            return self.judge_paragraph(&rest);
        }
        None
    }

    /// The exact cycle the stream's tail ends in, in words. A unit holding a letter counts at
    /// the short or long floor; one holding no letter only past [`LONG_REPEATED`]; a data unit
    /// (digits, whitespace, hex-literal letters and separators alone) never: padding and
    /// zero-filled dumps such as `0x00, 0x00, ...` are ordinary output.
    ///
    /// A Z-array over the tail reversed (indexed from the end, never copied) holds every suffix
    /// period in one linear pass: `z[i]` is how far the end agrees with itself shifted by `i`, so
    /// a unit of length `i` repeats `1 + z[i] / i` times.
    fn exact_cycle_detail(&mut self) -> Option<String> {
        let text = &self.tail[self.tail.len().saturating_sub(TAIL_WINDOW)..];
        let n = text.len();
        if n < SHORT_REPEATED {
            return None;
        }
        let last = n - 1;
        let max_unit = MAX_UNIT.min(n / 3);
        let z = &mut self.z;
        z.clear();
        z.resize(max_unit + 1, 0);
        let (mut left, mut right) = (0usize, 0usize);
        for i in 1..=max_unit {
            let mut zi = if i <= right {
                (right + 1 - i).min(z[i - left])
            } else {
                0
            };
            while i + zi < n && text[last - zi] == text[last - i - zi] {
                zi += 1;
            }
            z[i] = zi;
            if i + zi > right + 1 {
                left = i;
                right = i + zi - 1;
            }
        }
        for len in 2..=max_unit {
            let repeats = 1 + z[len] / len;
            let needed = if len <= SHORT_UNIT { 4 } else { 3 };
            if repeats < needed {
                continue;
            }
            let unit = &text[n - len..];
            let data = unit.iter().all(|byte| {
                byte.is_ascii_hexdigit()
                    || byte.is_ascii_whitespace()
                    || matches!(byte, b'x' | b'X' | b',' | b';' | b':' | b'.')
            });
            if data {
                continue;
            }
            let worded = unit
                .iter()
                .any(|byte| !byte.is_ascii() || byte.is_ascii_alphabetic());
            let floor = if worded && len <= SHORT_UNIT && !unit.contains(&b'\n') {
                SHORT_REPEATED
            } else {
                LONG_REPEATED
            };
            if len * repeats >= floor {
                return Some(format!(
                    "repeated a {len}-byte passage {repeats} times in a row"
                ));
            }
        }
        None
    }

    /// The next whole paragraph out of `pending`: the text before a blank line, or the first
    /// [`SEGMENT_CAP`] bytes of a wall of text. The break search resumes where it left off, so
    /// the pending text is scanned once per byte.
    fn take_paragraph(&mut self) -> Option<String> {
        let bytes = self.pending.as_bytes();
        let mut index = self.scan_from;
        let mut resume = bytes.len();
        while index < bytes.len() {
            if bytes[index] != b'\n' {
                index += 1;
                continue;
            }
            let mut cursor = index + 1;
            let mut last_newline = None;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                if bytes[cursor] == b'\n' {
                    last_newline = Some(cursor);
                }
                cursor += 1;
            }
            if let Some(end) = last_newline {
                let paragraph = self.pending[..index].to_string();
                self.pending.drain(..=end);
                self.scan_from = 0;
                return Some(paragraph);
            }
            if cursor == bytes.len() {
                // The whitespace run reaches the end: the next delta may complete the break.
                resume = index;
                break;
            }
            index = cursor;
        }
        self.scan_from = resume;
        if self.pending.len() > SEGMENT_CAP {
            let mut cut = SEGMENT_CAP;
            while !self.pending.is_char_boundary(cut) {
                cut -= 1;
            }
            let paragraph = self.pending[..cut].to_string();
            self.pending.drain(..cut);
            self.scan_from = 0;
            return Some(paragraph);
        }
        None
    }

    /// Judge one paragraph in pieces of at most [`SEGMENT_CAP`] bytes, so each stays comparable.
    fn judge_paragraph(&mut self, text: &str) -> Option<String> {
        let mut rest = text;
        while !rest.is_empty() {
            let mut cut = rest.len().min(SEGMENT_CAP);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            let (piece, tail) = rest.split_at(cut);
            if let Some(hit) = self.judge_segment(piece) {
                return Some(hit);
            }
            rest = tail;
        }
        None
    }

    fn judge_segment(&mut self, raw: &str) -> Option<String> {
        let segment = raw
            .lines()
            .filter(|line| !is_structured_line(line))
            .collect::<Vec<_>>()
            .join("\n");
        // Lowercase words of ASCII letters and digits, one space apart, the digit-only words
        // dropped.
        let mapped: String = segment
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() {
                    char::from(byte.to_ascii_lowercase())
                } else {
                    ' '
                }
            })
            .collect();
        let normalized = mapped
            .split_whitespace()
            .filter(|word| word.bytes().any(|byte| byte.is_ascii_lowercase()))
            .collect::<Vec<_>>()
            .join(" ");
        if normalized.len() < SEGMENT_FLOOR {
            return None;
        }
        let words: Vec<u64> = normalized
            .split(' ')
            .map(|word| fnv1a(0, word.as_bytes()))
            .collect();
        let trigrams: HashedSet = if words.len() < 3 {
            std::iter::once(fnv1a(0, normalized.as_bytes())).collect()
        } else {
            words
                .windows(3)
                .map(|trigram| {
                    trigram
                        .iter()
                        .fold(0, |hash, word| fnv1a(hash, &word.to_le_bytes()))
                })
                .collect()
        };
        // Near-duplicates: the same paragraph with cosmetic drift, at a word-trigram Jaccard of
        // 4/5 or more.
        let cluster = 1 + self
            .trigrams
            .iter()
            .filter(|earlier| {
                let shared = trigrams.iter().filter(|key| earlier.contains(*key)).count();
                let union = trigrams.len() + earlier.len() - shared;
                !earlier.is_empty() && shared * 5 >= union * 4
            })
            .count();
        let word_set: HashedSet = words.iter().copied().collect();
        // A stall: recent wording reshuffled, nothing new named.
        let unseen = word_set
            .iter()
            .filter(|word| !self.words.iter().any(|earlier| earlier.contains(*word)))
            .count();
        let recycles = !self.words.is_empty() && unseen * 5 <= word_set.len();
        let anchors: HashSet<String> = ANCHOR
            .find_iter(&segment)
            .map(|anchor| anchor.as_str().replace('`', "").to_lowercase())
            .collect();
        let new_anchor = anchors
            .iter()
            .any(|anchor| !self.anchors.iter().any(|earlier| earlier.contains(anchor)));
        self.stall = if recycles && !new_anchor {
            self.stall + 1
        } else {
            0
        };
        remember(&mut self.trigrams, trigrams, SEGMENT_WINDOW);
        remember(&mut self.words, word_set, NOVELTY_WINDOW);
        remember(&mut self.anchors, anchors, NOVELTY_WINDOW);
        self.judged += 1;
        if self.judged < SEGMENT_WARMUP {
            return None;
        }
        if cluster >= SEGMENT_CLUSTER {
            return Some(format!(
                "wrote {cluster} near-identical paragraphs within its last {SEGMENT_WINDOW}"
            ));
        }
        if self.stall >= STALL_RUN {
            return Some(format!(
                "wrote {} paragraphs in a row that only reshuffle its recent wording",
                self.stall
            ));
        }
        None
    }
}

fn remember<T>(window: &mut VecDeque<T>, item: T, limit: usize) {
    window.push_back(item);
    if window.len() > limit {
        window.pop_front();
    }
}

/// A line the paragraph checks drop, because it repeats one skeleton with different content by
/// design, so only prose is judged:
/// - code: a fence, an indented line that is not a list item, or a line ending in a block,
///   statement or markup delimiter;
/// - a markdown heading or a whole-line bold title (`**Maintaining Momentum**`), a summarizer's
///   ever-changing per-thought title that would mask a loop by inflating novelty;
/// - a list item or a table row;
/// - a data line (a hex dump row, a column of figures): fewer than half of its tokens are
///   digit-free words.
fn is_structured_line(line: &str) -> bool {
    let body = line.trim_start_matches([' ', '\t']);
    let indented = line.starts_with("  ") || line.starts_with('\t');
    let code = body.starts_with("```")
        || (indented && !is_list_item(body))
        || line
            .trim_end_matches([' ', '\t'])
            .ends_with(['{', '}', '[', ']', '(', ';', ',', '>']);
    let hashes = body.bytes().take_while(|byte| *byte == b'#').count();
    let heading = (1..=6).contains(&hashes) && body[hashes..].starts_with([' ', '\t']);
    let trimmed = body.trim_end_matches([' ', '\t']);
    let title = trimmed.len() >= 5 && trimmed.starts_with("**") && trimmed.ends_with("**");
    let (tokens, words) =
        body.split_whitespace()
            .fold((0usize, 0usize), |(tokens, words), token| {
                let word = !token.bytes().any(|byte| byte.is_ascii_digit());
                (tokens + 1, words + usize::from(word))
            });
    let data = tokens >= 4 && words * 2 < tokens;
    code || heading || title || is_list_item(body) || body.starts_with('|') || data
}

/// A list item: a `-`, `*` or `+` bullet, or a `1.` / `1)` number, then a space or tab.
fn is_list_item(body: &str) -> bool {
    let marker_end = if body.starts_with(['-', '*', '+']) {
        1
    } else {
        let digits = body.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !body[digits..].starts_with(['.', ')']) {
            return false;
        }
        digits + 1
    };
    body[marker_end..].starts_with([' ', '\t'])
}
