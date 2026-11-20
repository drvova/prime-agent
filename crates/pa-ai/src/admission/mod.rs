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

/// Key hashes that belong to one stable credential identity (a pinned named credential's
/// `sha256(provider:id)` prefix): an OAuth refresh rotates the token, never the admission key.
static CREDENTIAL_ALIASES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::OnceLock::new();

/// The most aliases one process keeps: one per refresh of the session's pinned credential.
const MAX_CREDENTIAL_ALIASES: usize = 256;

fn key_hash(api_key: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(api_key.as_bytes());
    hex::encode(&digest[..6])
}

/// The stable admission identity of a named credential: the first 12 hex digits of
/// `sha256(provider:id)`. It never changes when an OAuth refresh rotates the token, and it carries
/// no secret.
#[must_use]
pub fn credential_identity(provider: &str, id: &str) -> String {
    key_hash(&format!("{provider}:{id}"))
}

/// Admit requests made with `api_key` under `identity` instead of the key's own hash. The
/// identity carries no secret; the key itself is never stored, only its short hash.
pub fn alias_credential(api_key: &str, identity: &str) {
    let mut aliases = CREDENTIAL_ALIASES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if aliases.len() >= MAX_CREDENTIAL_ALIASES {
        aliases.clear();
    }
    aliases.insert(key_hash(api_key), identity.to_string());
}

impl AdmissionKey {
    /// The key for a request to `model` with `api_key`: the key's short hash, or the stable
    /// identity [`alias_credential`] registered for it.
    #[must_use]
    pub fn for_request(model: &crate::types::Model, api_key: Option<&str>) -> Self {
        let hashed = key_hash(api_key.unwrap_or_default());
        let credential = CREDENTIAL_ALIASES
            .get()
            .and_then(|aliases| {
                aliases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&hashed)
                    .cloned()
            })
            .unwrap_or(hashed);
        Self {
            provider: model.provider.clone(),
            credential,
            scope: model.id.clone(),
        }
    }
}

/// The provider field of the daemon-wide session-boot budget's key: not a provider, the one
/// shared budget for booting sessions (worker spawns for `rlm.spawn` / `rlm.create_session`
/// children, kernel boots, crash-recovery adoption). Its slot runs
/// [`session_boot_controller_config`] instead of the pool's provider config.
pub const SESSION_BOOT: &str = "session-boot";

impl AdmissionKey {
    /// The session-boot budget's key.
    #[must_use]
    pub fn session_boot() -> Self {
        Self {
            provider: SESSION_BOOT.to_string(),
            credential: String::new(),
            scope: String::new(),
        }
    }

    /// Whether this is the session-boot budget, not a provider key.
    #[must_use]
    pub fn is_session_boot(&self) -> bool {
        self.provider == SESSION_BOOT && self.credential.is_empty() && self.scope.is_empty()
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

/// The boot concurrency `PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS` pins (1..=64; a larger value
/// caps at 64), or `None` when it is unset, empty, zero, or not a number.
#[must_use]
pub fn pinned_boot_concurrency(raw: Option<&str>) -> Option<u32> {
    let raw = raw.filter(|raw| !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit()))?;
    let parsed: u64 = raw.parse().unwrap_or(u64::MAX);
    (parsed >= 1).then(|| u32::try_from(parsed.min(64)).unwrap_or(64))
}

/// The session-boot budget's tuning: boots start at the old kernel-boot width (twice the
/// cores, 4..=16: boots are IO-bound), climb by one per window of fast boots, halve when a boot
/// finishes under CPU or memory pressure, and back off when boot latency doubles. Rate pacing
/// stays out of the way (boots are bounded by concurrency only). The
/// `PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS` override pins the width: a hard limit the
/// controller never moves.
#[must_use]
pub fn session_boot_controller_config() -> ControllerConfig {
    let pinned = pinned_boot_concurrency(
        std::env::var("PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS")
            .ok()
            .as_deref(),
    );
    let cores = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(64);
    let start = u32::try_from((cores * 2).clamp(4, 16)).unwrap_or(4);
    ControllerConfig {
        initial_concurrency: pinned.unwrap_or(start),
        min_concurrency: pinned.unwrap_or(1),
        max_concurrency: pinned.unwrap_or(64),
        initial_rate_per_min: 100_000,
        min_rate_per_min: 100_000,
        max_rate_per_min: 100_000,
        rate_step_per_min: 0,
        decrease_keep_pct: 50,
        latency_keep_pct: 75,
        latency_rise_pct: 200,
        latency_rise_samples: 2,
        headroom_pct: 0,
        decrease_spacing_ms: 2_000,
        ceiling_ttl_ms: 5 * 60_000,
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
