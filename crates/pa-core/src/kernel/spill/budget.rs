//! The per-cell output cap, scaled to the context the session has left:
//! generous early, tighter late. A cut cell output is paged out whole (see
//! the module root), so a tighter cap loses nothing; it only keeps one cell
//! from filling what is left of the window.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::kernel::shared::DEFAULT_MAX_OUTPUT_CHARS;

/// The smallest per-stream cap, however full the context.
pub(crate) const MIN_OUTPUT_CHARS: usize = 8_192;
/// One cell's streams may take at most this share (1/N) of the context left.
const REMAINING_SHARE_DIVISOR: u64 = 8;
/// The compaction estimator's chars-per-token heuristic.
const CHARS_PER_TOKEN: u64 = 4;

/// The cap for a context of `window_tokens` with `used_tokens` in use: an
/// eighth of the remaining tokens in characters (4 per token), clamped to
/// [`MIN_OUTPUT_CHARS`]..=[`DEFAULT_MAX_OUTPUT_CHARS`]. An unknown window
/// (zero) keeps the default cap.
#[must_use]
pub(crate) fn output_cap_chars(window_tokens: u64, used_tokens: u64) -> usize {
    if window_tokens == 0 {
        return DEFAULT_MAX_OUTPUT_CHARS;
    }
    let remaining = window_tokens.saturating_sub(used_tokens);
    let chars = remaining.saturating_mul(CHARS_PER_TOKEN) / REMAINING_SHARE_DIVISOR;
    usize::try_from(chars)
        .unwrap_or(usize::MAX)
        .clamp(MIN_OUTPUT_CHARS, DEFAULT_MAX_OUTPUT_CHARS)
}

/// The session's latest context fill, published by the request path and
/// read by the cell executor; both are in-memory atomics, so neither path
/// waits on the other. Never observed, it keeps the default cap.
#[derive(Debug, Clone, Default)]
pub struct OutputBudget {
    fill: Arc<Fill>,
}

#[derive(Debug, Default)]
struct Fill {
    window_tokens: AtomicU64,
    used_tokens: AtomicU64,
}

impl OutputBudget {
    /// Record the fill of the request about to go out.
    pub(crate) fn observe(&self, used_tokens: u64, window_tokens: u64) {
        self.fill.used_tokens.store(used_tokens, Ordering::Relaxed);
        self.fill
            .window_tokens
            .store(window_tokens, Ordering::Relaxed);
    }

    /// The per-stream cap the next cell runs with.
    pub(crate) fn max_output_chars(&self) -> usize {
        output_cap_chars(
            self.fill.window_tokens.load(Ordering::Relaxed),
            self.fill.used_tokens.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{output_cap_chars, OutputBudget, MIN_OUTPUT_CHARS};
    use crate::kernel::shared::DEFAULT_MAX_OUTPUT_CHARS;

    #[test]
    fn the_cap_is_generous_early_and_tightens_as_the_context_fills() {
        assert_eq!(
            [
                OutputBudget::default().max_output_chars(),
                output_cap_chars(200_000, 0),
                output_cap_chars(200_000, 100_000),
                output_cap_chars(200_000, 180_000),
                output_cap_chars(200_000, 199_000),
                output_cap_chars(200_000, 400_000),
            ],
            [
                DEFAULT_MAX_OUTPUT_CHARS,
                DEFAULT_MAX_OUTPUT_CHARS,
                50_000,
                10_000,
                MIN_OUTPUT_CHARS,
                MIN_OUTPUT_CHARS,
            ]
        );
    }
}
