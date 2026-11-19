//! The learned idle threshold of one provider origin: how long its pooled
//! connection survives without traffic, so a turn re-opens it only when the
//! provider (or the pool) has likely closed it.
//!
//! Pure: every call takes the measured idle gap and warm-up time. A warm-up
//! `HEAD` over a live keep-alive connection costs one round trip; one that
//! has to reconnect also pays the TCP (and TLS) handshake, so it takes well
//! over twice as long as a reused one. Measured against the origin's
//! reconnect time (its first prewarm always opens a fresh connection), each
//! warm-up says whether the connection was still open after that gap:
//!
//! - open after a gap at or past the threshold: the provider keeps
//!   connections longer, so the threshold rises (never past the pool's own
//!   limit);
//! - closed after a gap below the threshold (a probe): the provider closes
//!   sooner, so the threshold drops just under that gap.

/// The highest threshold: the shared pool drops a connection idle for
/// 90 s, so a turn after 80 s re-opens it in time.
const IDLE_THRESHOLD_CEILING_MS: u64 = 80_000;
/// The lowest threshold: a connection closed sooner than this is re-opened
/// at every turn anyway.
const IDLE_THRESHOLD_FLOOR_MS: u64 = 10_000;
/// Gaps from this share of the threshold up to it are probed: a warm-up is
/// sent so a provider that closes sooner is noticed.
const PROBE_BAND_PCT: u64 = 75;
/// How far an open connection past the threshold raises it.
const RAISE_STEP_MS: u64 = 10_000;
/// A warm-up under this share of the reconnect time reused the connection.
const REUSED_PCT: u64 = 50;

/// What a turn start should do about an origin idle for some time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewarmDecision {
    /// Idle too briefly for the connection to have closed.
    Fresh,
    /// Idle just under the threshold: warm up to learn whether the provider
    /// closes connections sooner.
    Probe,
    /// Idle past the threshold: the connection is likely closed.
    Rewarm,
}

/// What one warm-up found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionFound {
    /// The pooled connection was still open.
    Open,
    /// The warm-up had to reconnect.
    Reopened,
    /// No reconnect time to compare with yet.
    Unknown,
}

/// The learned idle threshold of one origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleThreshold {
    threshold_ms: u64,
    /// The time of a warm-up that opened a fresh connection (an EWMA).
    reopen_ms: Option<u64>,
}

impl Default for IdleThreshold {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleThreshold {
    /// The starting point: re-open after the pool's own limit.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            threshold_ms: IDLE_THRESHOLD_CEILING_MS,
            reopen_ms: None,
        }
    }

    /// The current threshold.
    #[cfg(test)]
    #[must_use]
    pub const fn threshold_ms(&self) -> u64 {
        self.threshold_ms
    }

    /// What to do for an origin idle `idle_ms`.
    #[must_use]
    pub const fn decide(&self, idle_ms: u64) -> RewarmDecision {
        if idle_ms >= self.threshold_ms {
            RewarmDecision::Rewarm
        } else if idle_ms.saturating_mul(100) >= self.threshold_ms.saturating_mul(PROBE_BAND_PCT) {
            RewarmDecision::Probe
        } else {
            RewarmDecision::Fresh
        }
    }

    /// Record a warm-up known to have opened a fresh connection.
    pub fn note_reopened(&mut self, latency_ms: u64) {
        self.reopen_ms = Some(
            self.reopen_ms
                .map_or(latency_ms, |baseline| (baseline * 3 + latency_ms) / 4),
        );
    }

    /// Fold in a warm-up sent after `idle_ms` that took `latency_ms`, and
    /// say what it found.
    pub fn learn(&mut self, idle_ms: u64, latency_ms: u64) -> ConnectionFound {
        let Some(reopen_ms) = self.reopen_ms else {
            return ConnectionFound::Unknown;
        };
        if latency_ms.saturating_mul(100) < reopen_ms.saturating_mul(REUSED_PCT) {
            if idle_ms >= self.threshold_ms {
                self.threshold_ms = idle_ms
                    .saturating_add(RAISE_STEP_MS)
                    .min(IDLE_THRESHOLD_CEILING_MS);
            }
            return ConnectionFound::Open;
        }
        self.note_reopened(latency_ms);
        if idle_ms < self.threshold_ms {
            self.threshold_ms = (idle_ms / 10 * 9).max(IDLE_THRESHOLD_FLOOR_MS);
        }
        ConnectionFound::Reopened
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One turn start after `idle_ms`, warmed in `latency_ms` when the
    /// decision sends a warm-up.
    fn turn(
        threshold: &mut IdleThreshold,
        idle_ms: u64,
        latency_ms: u64,
    ) -> (RewarmDecision, Option<ConnectionFound>, u64) {
        let decision = threshold.decide(idle_ms);
        let found = match decision {
            RewarmDecision::Fresh => None,
            RewarmDecision::Probe | RewarmDecision::Rewarm => {
                Some(threshold.learn(idle_ms, latency_ms))
            }
        };
        (decision, found, threshold.threshold_ms())
    }

    #[test]
    fn a_provider_that_closes_early_lowers_the_threshold_and_one_that_keeps_raises_it() {
        let mut threshold = IdleThreshold::new();
        // The first prewarm opened a fresh connection in 300 ms.
        threshold.note_reopened(300);
        // A provider that closes idle connections after ~45 s, then one
        // that keeps them for the pool's whole life.
        let sequence = [
            (30_000, 100),
            (62_000, 300),
            (50_000, 310),
            (40_000, 90),
            (30_000, 80),
            (85_000, 300),
            (45_000, 100),
            (70_000, 100),
        ]
        .map(|(idle_ms, latency_ms)| turn(&mut threshold, idle_ms, latency_ms));
        assert_eq!(
            sequence,
            [
                (RewarmDecision::Fresh, None, 80_000),
                // Closed at 62 s, under the threshold: just under the gap.
                (
                    RewarmDecision::Probe,
                    Some(ConnectionFound::Reopened),
                    55_800
                ),
                (
                    RewarmDecision::Probe,
                    Some(ConnectionFound::Reopened),
                    45_000
                ),
                // Open at 40 s: a probe that found the connection alive
                // keeps the threshold.
                (RewarmDecision::Probe, Some(ConnectionFound::Open), 45_000),
                (RewarmDecision::Fresh, None, 45_000),
                (
                    RewarmDecision::Rewarm,
                    Some(ConnectionFound::Reopened),
                    45_000
                ),
                // Open past the threshold: the provider keeps connections
                // longer; the threshold climbs, capped at the pool limit.
                (RewarmDecision::Rewarm, Some(ConnectionFound::Open), 55_000),
                (RewarmDecision::Rewarm, Some(ConnectionFound::Open), 80_000),
            ]
        );
    }

    #[test]
    fn without_a_reconnect_time_nothing_is_learned() {
        let mut threshold = IdleThreshold::new();
        assert_eq!(
            turn(&mut threshold, 85_000, 40),
            (
                RewarmDecision::Rewarm,
                Some(ConnectionFound::Unknown),
                80_000
            )
        );
    }
}
