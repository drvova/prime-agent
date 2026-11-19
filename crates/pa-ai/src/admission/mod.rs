//! Provider request admission: every model request waits for a permit
//! before it is sent, so parallel work (subagents, many sessions) stays
//! under each provider's real limits instead of sending into 429s.
//!
//! Limits are learned per [`AdmissionKey`] (provider, credential hash,
//! model) by the IO-free controller in [`pa_types::control`]: from the
//! provider's own rate-limit headers when it sends them ([`headers`]), and
//! by AIMD from outcomes (429 / overload / latency rise) when it does not.
//! Nothing is configured: the starting point is the conservative
//! [`provider_controller_config`].
//!
//! The permit source is pluggable ([`AdmissionSource`]). A process starts
//! with its own [`LocalAdmission`]; the daemon worker installs a source
//! that shares one budget across every worker through the supervisor and
//! falls back to the local pool when the supervisor does not answer in
//! time. Admission sits on the provider request path only (inside
//! [`crate::stream`] / [`crate::stream_simple`]); nothing on paint,
//! startup or first render waits on it.

mod gate;
mod headers;
mod pool;

#[cfg(test)]
mod tests;

pub use pa_types::control::Priority;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use pa_types::control::{ControllerConfig, Observation, Outcome};
use serde::{Deserialize, Serialize};

pub(crate) use gate::{admitted_stream, CallOptions};
pub use headers::parse_limit_report;
pub use pool::LocalAdmission;

/// The independently limited unit: one provider account's budget for one
/// model. Carries no secret: the credential is a short one-way hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionKey {
    pub provider: String,
    /// The first 12 hex digits of the credential's SHA-256 (empty input
    /// for a provider that resolves its own auth).
    pub credential: String,
    /// The model id (providers scope their limits per model).
    pub scope: String,
}

impl AdmissionKey {
    /// The key for a request to `model` with `api_key`.
    #[must_use]
    pub fn for_request(model: &crate::types::Model, api_key: Option<&str>) -> Self {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(api_key.unwrap_or_default().as_bytes());
        Self {
            provider: model.provider.clone(),
            credential: hex::encode(&digest[..6]),
            scope: model.id.clone(),
        }
    }
}

/// One permit request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionRequest {
    pub key: AdmissionKey,
    pub priority: Priority,
    /// When the caller started waiting (epoch ms): the aging clock, kept
    /// across re-polls so a waiter never loses its place.
    pub enqueued_at_ms: u64,
}

/// Returns a permit with what its request observed; `true` when the source's
/// controller cut capacity in response (a remote source answers later and
/// counts its own decreases).
pub type FinishFn = Box<dyn FnOnce(Observation) -> bool + Send>;

/// A held permit. [`Grant::finish`] returns it with the request's
/// observation; dropping it unfinished returns it as [`Outcome::Failed`]
/// (no capacity signal), so an aborted request never leaks a permit.
pub struct Grant {
    /// How long the request waited for the permit.
    pub waited_ms: u64,
    finish: Option<FinishFn>,
}

impl Grant {
    #[must_use]
    pub fn new(waited_ms: u64, finish: FinishFn) -> Self {
        Self {
            waited_ms,
            finish: Some(finish),
        }
    }

    /// Return the permit with what the request observed; `true` when the
    /// controller cut capacity in response.
    #[must_use]
    pub fn finish(mut self, observation: Observation) -> bool {
        self.finish.take().is_some_and(|finish| finish(observation))
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            let _cut = finish(Observation {
                outcome: Outcome::Failed,
                latency_ms: None,
                limits: pa_types::control::LimitReport::default(),
            });
        }
    }
}

/// The boxed future an [`AdmissionSource`] returns. Boxed (not RPITIT)
/// because the source is installed process-wide behind `dyn`.
pub type GrantFuture = Pin<Box<dyn Future<Output = Grant> + Send>>;

/// A source of provider request permits.
///
/// `acquire` resolves once the request may be sent: the source's
/// concurrency cap, rate bucket, and any server-requested wait for the key
/// all allow it, and no earlier-deadline waiter
/// ([`pa_types::control::service_deadline_ms`]) is queued ahead. It must
/// always resolve: a source that depends on another process bounds that
/// dependency and falls back to a local pool rather than hang a request.
/// The returned [`Grant`] carries the observation back to the same source.
pub trait AdmissionSource: Send + Sync {
    fn acquire(&self, request: AdmissionRequest) -> GrantFuture;

    /// A permit available right now without waiting, if any. A source that cannot answer
    /// synchronously (it asks another process) returns `None`; the caller then awaits
    /// [`AdmissionSource::acquire`].
    fn try_acquire(&self, _request: &AdmissionRequest) -> Option<Grant> {
        None
    }
}

fn installed() -> &'static RwLock<Option<Arc<dyn AdmissionSource>>> {
    static SOURCE: OnceLock<RwLock<Option<Arc<dyn AdmissionSource>>>> = OnceLock::new();
    SOURCE.get_or_init(|| RwLock::new(None))
}

/// This process's own pool: the default source and every remote source's
/// fallback.
#[must_use]
pub fn local() -> &'static Arc<LocalAdmission> {
    static LOCAL: OnceLock<Arc<LocalAdmission>> = OnceLock::new();
    LOCAL.get_or_init(|| Arc::new(LocalAdmission::new(provider_controller_config())))
}

/// Route this process's provider requests through `source` (the daemon
/// worker's supervisor-shared budget).
pub fn install(source: Arc<dyn AdmissionSource>) {
    *installed()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(source);
}

/// The source requests acquire from now.
#[must_use]
pub fn current_source() -> Arc<dyn AdmissionSource> {
    installed()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| Arc::clone(local()) as Arc<dyn AdmissionSource>)
}

static PROCESS_PRIORITY: AtomicU8 = AtomicU8::new(0);

/// The class of this process's requests when a request names none: a
/// daemon worker hosting a subagent session sets [`Priority::Subagent`].
pub fn set_process_priority(priority: Priority) {
    PROCESS_PRIORITY.store(priority_code(priority), Ordering::Relaxed);
}

/// The class a request without its own runs at.
#[must_use]
pub fn process_priority() -> Priority {
    match PROCESS_PRIORITY.load(Ordering::Relaxed) {
        1 => Priority::Subagent,
        2 => Priority::Helper,
        3 => Priority::Background,
        _ => Priority::Foreground,
    }
}

const fn priority_code(priority: Priority) -> u8 {
    match priority {
        Priority::Foreground => 0,
        Priority::Subagent => 1,
        Priority::Helper => 2,
        Priority::Background => 3,
    }
}

/// The starting point for a provider nobody has measured yet: conservative
/// concurrency, a rate that a reported limit replaces at the first
/// response, AIMD halving, and a 15-minute ceiling memory.
#[must_use]
pub const fn provider_controller_config() -> ControllerConfig {
    ControllerConfig {
        initial_concurrency: 4,
        min_concurrency: 1,
        max_concurrency: 64,
        initial_rate_per_min: 600,
        min_rate_per_min: 1,
        max_rate_per_min: 100_000,
        rate_step_per_min: 60,
        decrease_keep_pct: 50,
        latency_keep_pct: 75,
        latency_rise_pct: 300,
        latency_rise_samples: 3,
        headroom_pct: 20,
        decrease_spacing_ms: 2_000,
        ceiling_ttl_ms: 15 * 60_000,
    }
}

/// Counts for the adoption telemetry: primitives only, never a key or URL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdmissionStats {
    /// Requests that waited for a permit.
    pub waited_count: u64,
    /// Total time those requests waited.
    pub wait_ms: u64,
    /// Responses that came back throttled or overloaded.
    pub throttled_count: u64,
    /// Multiplicative decreases the controller made.
    pub decrease_count: u64,
}

static WAITED: AtomicU64 = AtomicU64::new(0);
static WAIT_MS: AtomicU64 = AtomicU64::new(0);
static THROTTLED: AtomicU64 = AtomicU64::new(0);
static DECREASES: AtomicU64 = AtomicU64::new(0);

/// Take (and reset) the counts since the last take.
#[must_use]
pub fn take_stats() -> AdmissionStats {
    AdmissionStats {
        waited_count: WAITED.swap(0, Ordering::Relaxed),
        wait_ms: WAIT_MS.swap(0, Ordering::Relaxed),
        throttled_count: THROTTLED.swap(0, Ordering::Relaxed),
        decrease_count: DECREASES.swap(0, Ordering::Relaxed),
    }
}

/// Count a controller decrease (the pool that owns the controller calls
/// this; a remote source calls it when its supervisor reports one).
pub fn note_decrease() {
    DECREASES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn note_wait(waited_ms: u64) {
    if waited_ms > 0 {
        WAITED.fetch_add(1, Ordering::Relaxed);
        WAIT_MS.fetch_add(waited_ms, Ordering::Relaxed);
    }
}

pub(crate) fn note_outcome(outcome: Outcome) {
    if matches!(outcome, Outcome::Throttled | Outcome::Overloaded) {
        THROTTLED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Epoch milliseconds (the controller's clock).
#[must_use]
pub fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}
