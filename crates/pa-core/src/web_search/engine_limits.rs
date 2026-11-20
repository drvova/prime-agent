//! Per-engine limits for the keyless scraped engines (`DuckDuckGo`, Startpage, Ecosia,
//! Mojeek): each engine gets its own request rate and concurrency from the IO-free
//! controller (`pa_types::control`, hosted by pa-ai's permit pool), an engine that answers with
//! a block, a CAPTCHA wall, or a 429 sits out a cooldown that doubles while it keeps blocking,
//! and the aggregate's soft deadline follows each engine's observed latency.
//!
//! Nothing waits here: a search takes an engine only when it has a permit now, so an engine
//! over its budget or cooling down is skipped (the aggregate's other engines answer, the auto
//! chain moves on) instead of being hammered into a longer block.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pa_ai::admission::{AdmissionKey, AdmissionRequest, AdmissionSource, Grant, LocalAdmission};
use pa_types::control::{ControllerConfig, LimitReport, Observation, Outcome, Priority};

use super::catalog::ProviderId;
use super::http::{ProviderError, ProviderResult};
use super::SearchResponse;

/// An engine's tuning: a modest burst, pacing a shared IP stays welcome at.
const ENGINE_CONTROLLER_CONFIG: ControllerConfig = ControllerConfig {
    initial_concurrency: 2,
    min_concurrency: 1,
    max_concurrency: 4,
    initial_rate_per_min: 20,
    min_rate_per_min: 2,
    max_rate_per_min: 30,
    rate_step_per_min: 2,
    decrease_keep_pct: 50,
    latency_keep_pct: 75,
    latency_rise_pct: 300,
    latency_rise_samples: 3,
    headroom_pct: 0,
    decrease_spacing_ms: 2_000,
    ceiling_ttl_ms: 10 * 60_000,
};

/// The first block's cooldown; each further consecutive block doubles it.
const BLOCK_COOLDOWN_MS: u64 = 60_000;
/// The longest cooldown.
const MAX_BLOCK_COOLDOWN_MS: u64 = 15 * 60_000;
/// The soft deadline of an engine with no latency yet (the fixed deadline it replaces).
pub(crate) const COLD_SOFT_DEADLINE: Duration = Duration::from_secs(5);
/// The shortest soft deadline: a fast engine still gets a moment past its usual latency.
const MIN_SOFT_DEADLINE: Duration = Duration::from_secs(1);

/// What the engines taught this process so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Health {
    /// Recent successful latency (an EWMA).
    latency_ms: Option<u64>,
    /// Consecutive blocks.
    blocks: u32,
}

/// The cooldown after the `blocks`-th consecutive block.
fn cooldown_ms(blocks: u32) -> u64 {
    BLOCK_COOLDOWN_MS
        .saturating_mul(1_u64 << blocks.saturating_sub(1).min(8))
        .min(MAX_BLOCK_COOLDOWN_MS)
}

/// What one engine call tells its controller, and the engine's health after it: a block
/// (status 429: a wall, a CAPTCHA, or a real 429) throttles it with a cooldown; an answer is a
/// success with its latency; anything else says nothing about capacity.
fn observe(
    result: &ProviderResult<SearchResponse>,
    latency_ms: u64,
    health: Health,
) -> (Observation, Health) {
    let (outcome, health) = match result {
        Ok(_) => (
            Outcome::Success,
            Health {
                latency_ms: Some(
                    health
                        .latency_ms
                        .map_or(latency_ms, |ewma| (ewma * 3 + latency_ms) / 4),
                ),
                blocks: 0,
            },
        ),
        Err(ProviderError {
            status: Some(429), ..
        }) => (
            Outcome::Throttled,
            Health {
                blocks: health.blocks.saturating_add(1),
                ..health
            },
        ),
        Err(_) => (Outcome::Failed, health),
    };
    let retry_after_ms = match outcome {
        Outcome::Throttled => Some(cooldown_ms(health.blocks)),
        Outcome::Success | Outcome::Overloaded | Outcome::Failed => None,
    };
    (
        Observation {
            outcome,
            latency_ms: (outcome == Outcome::Success).then_some(latency_ms),
            limits: LimitReport {
                retry_after_ms,
                ..LimitReport::default()
            },
        },
        health,
    )
}

/// Process-wide (or, in tests, per-engine-instance) limits of the scraped engines.
pub(crate) struct EngineLimits {
    pool: LocalAdmission,
    health: Mutex<HashMap<ProviderId, Health>>,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts for the adoption telemetry: primitives only, never a query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineLimitStats {
    /// Engine calls skipped because the engine was over its budget or cooling down.
    pub skipped_count: u64,
    /// Engine answers that were blocks.
    pub blocked_count: u64,
}

static STATS: Mutex<EngineLimitStats> = Mutex::new(EngineLimitStats {
    skipped_count: 0,
    blocked_count: 0,
});

/// Take (and reset) the counts since the last take.
#[must_use]
pub fn take_stats() -> EngineLimitStats {
    std::mem::take(
        &mut *STATS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

fn engine_key(engine: ProviderId) -> AdmissionKey {
    AdmissionKey {
        provider: engine.id().to_string(),
        credential: String::new(),
        scope: String::new(),
    }
}

impl EngineLimits {
    pub(crate) fn new() -> Self {
        Self {
            pool: LocalAdmission::new(ENGINE_CONTROLLER_CONFIG),
            health: Mutex::new(HashMap::new()),
        }
    }

    fn health(&self, engine: ProviderId) -> Health {
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&engine)
            .copied()
            .unwrap_or_default()
    }

    /// The aggregate's soft deadline for `engine`: twice its recent latency, 1..=5 s (5 s until
    /// it answered once).
    pub(crate) fn soft_deadline(&self, engine: ProviderId) -> Duration {
        self.health(engine)
            .latency_ms
            .map_or(COLD_SOFT_DEADLINE, |latency_ms| {
                Duration::from_millis(latency_ms.saturating_mul(2))
                    .clamp(MIN_SOFT_DEADLINE, COLD_SOFT_DEADLINE)
            })
    }

    /// Run `call` on `engine` when it has a permit now; otherwise skip it with a 429-style
    /// failure naming why (nothing waits, and a skip is not a new block).
    pub(crate) async fn run(
        &self,
        engine: ProviderId,
        call: impl std::future::Future<Output = ProviderResult<SearchResponse>>,
    ) -> ProviderResult<SearchResponse> {
        let request = AdmissionRequest {
            key: engine_key(engine),
            priority: Priority::Foreground,
            enqueued_at_ms: pa_ai::admission::now_ms(),
        };
        let Some(grant) = self.pool.try_acquire(&request) else {
            STATS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .skipped_count += 1;
            return Err(ProviderError::new(
                format!(
                    "{} was skipped: it recently blocked automated searches or is at its request budget; it is retried automatically",
                    engine.label()
                ),
                Some(429),
            ));
        };
        let started = Instant::now();
        let result = call.await;
        self.finish(engine, grant, &result, started.elapsed());
        result
    }

    fn finish(
        &self,
        engine: ProviderId,
        grant: Grant,
        result: &ProviderResult<SearchResponse>,
        elapsed: Duration,
    ) {
        let latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let (observation, health) = observe(result, latency_ms, self.health(engine));
        if observation.outcome == Outcome::Throttled {
            STATS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .blocked_count += 1;
        }
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(engine, health);
        let _cut = grant.finish(observation);
    }

    /// Record a successful answer of `engine` in `latency_ms` (tests seed a learned latency).
    #[cfg(test)]
    pub(crate) fn note_answer(&self, engine: ProviderId, latency_ms: u64) {
        let request = AdmissionRequest {
            key: engine_key(engine),
            priority: Priority::Foreground,
            enqueued_at_ms: pa_ai::admission::now_ms(),
        };
        let grant = self.pool.try_acquire(&request).expect("a free permit");
        self.finish(
            engine,
            grant,
            &Ok(SearchResponse::from_sources(Vec::new())),
            Duration::from_millis(latency_ms),
        );
    }
}

#[cfg(test)]
mod tests;
