//! Session-boot admission: one adaptive gate for every "boot a session" — the worker spawn
//! behind an `rlm.spawn` / `rlm.create_session` child, a kernel's interpreter start, and the
//! supervisor's crash-recovery adoption.
//!
//! Every boot takes a permit from the session-boot budget
//! ([`pa_ai::admission::AdmissionKey::session_boot`]) of an admission source: inside a daemon
//! worker that is the supervisor's daemon-wide pool, so boots across every session share one
//! width. The width is the IO-free controller's (`pa_types::control`): it climbs while boots
//! stay fast, halves when a boot finishes under CPU or memory pressure ([`HostLoad`]), and backs
//! off when boot latency doubles. `PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS` pins it. Waiters
//! are served in priority order with aging (the foreground session's boots ahead of
//! subagents', recovery last).
//!
//! A permit covers the boot only, never what follows it: a parent waiting on its child, or a
//! child booting its own kernel, holds no slot, so nested boots cannot deadlock the budget. A
//! boot abandoned while it waits returns any permit granted after the fact.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pa_ai::admission::{AdmissionKey, AdmissionRequest, AdmissionSource, Grant, Priority};
use pa_types::control::{LimitReport, Observation, Outcome};

/// Memory available below this share of the total is pressure.
const MEMORY_PRESSURE_AVAILABLE_PCT: u32 = 10;
/// A one-minute load above this share of the cores is pressure.
const CPU_PRESSURE_LOAD_PCT: u32 = 200;

/// The host's load when a boot finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLoad {
    /// The one-minute load average per core, in percent.
    pub load_per_core_pct: u32,
    /// Available memory as a share of the total, in percent.
    pub memory_available_pct: u32,
}

impl HostLoad {
    /// The host's load now, where the platform exposes it without a blocking call (Linux's
    /// in-memory `/proc`); `None` elsewhere: boots then adapt from latency alone.
    #[must_use]
    pub fn sample() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            let loadavg = std::fs::read_to_string("/proc/loadavg").ok()?;
            let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
            let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
            Self::parse(&loadavg, &meminfo, cores)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Read `/proc/loadavg` and `/proc/meminfo` text for a host with `cores` cores.
    #[must_use]
    pub fn parse(loadavg: &str, meminfo: &str, cores: usize) -> Option<Self> {
        // The one-minute load in hundredths ("1.23" -> 123).
        let load = loadavg.split_whitespace().next()?;
        let (whole, fraction) = load.split_once('.').unwrap_or((load, ""));
        let fraction: String = fraction.chars().chain("00".chars()).take(2).collect();
        let load_hundredths = whole
            .parse::<u32>()
            .ok()?
            .saturating_mul(100)
            .saturating_add(fraction.parse::<u32>().ok()?);
        let field = |name: &str| -> Option<u64> {
            meminfo
                .lines()
                .find_map(|line| line.strip_prefix(name))?
                .trim_start_matches(':')
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        };
        let total = field("MemTotal").filter(|total| *total > 0)?;
        let available = field("MemAvailable")?;
        let cores = u32::try_from(cores.max(1)).unwrap_or(u32::MAX);
        Some(Self {
            load_per_core_pct: load_hundredths / cores,
            memory_available_pct: u32::try_from(available.saturating_mul(100) / total)
                .unwrap_or(100)
                .min(100),
        })
    }

    /// Whether the host is short on CPU or memory.
    #[must_use]
    pub const fn under_pressure(&self) -> bool {
        self.memory_available_pct < MEMORY_PRESSURE_AVAILABLE_PCT
            || self.load_per_core_pct > CPU_PRESSURE_LOAD_PCT
    }
}

/// How a boot ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootResult {
    Booted,
    Failed,
}

/// What a finished boot tells the controller: pressure on the host is an overload whatever
/// the boot's own result; otherwise a boot is a success (its latency feeds the latency
/// gradient) or a failure that says nothing about capacity.
#[must_use]
pub fn boot_outcome(result: BootResult, load: Option<HostLoad>) -> Outcome {
    if load.is_some_and(|load| load.under_pressure()) {
        return Outcome::Overloaded;
    }
    match result {
        BootResult::Booted => Outcome::Success,
        BootResult::Failed => Outcome::Failed,
    }
}

/// A finished admitted boot.
#[derive(Debug)]
pub struct AdmittedBoot<T> {
    pub result: anyhow::Result<T>,
    /// How long the boot waited for its permit (0: one was free).
    pub queued_ms: u64,
}

/// Counts for the adoption telemetry: primitives only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootAdmissionStats {
    /// Boots that waited for a permit.
    pub waited_count: u64,
    /// Their summed wait.
    pub wait_ms: u64,
    /// Boots that finished under CPU or memory pressure.
    pub pressure_count: u64,
}

static WAITED: AtomicU64 = AtomicU64::new(0);
static WAIT_MS: AtomicU64 = AtomicU64::new(0);
static PRESSURE: AtomicU64 = AtomicU64::new(0);

/// Take (and reset) this process's counts since the last take.
#[must_use]
pub fn take_stats() -> BootAdmissionStats {
    BootAdmissionStats {
        waited_count: WAITED.swap(0, Ordering::Relaxed),
        wait_ms: WAIT_MS.swap(0, Ordering::Relaxed),
        pressure_count: PRESSURE.swap(0, Ordering::Relaxed),
    }
}

/// An acquire running on its own task: awaiting it yields the permit (`None`: the acquire
/// task failed); dropping it before then returns a permit that arrives after its waiter gave
/// up.
struct PendingGrant(Option<tokio::task::JoinHandle<Grant>>);

impl Future for PendingGrant {
    type Output = Option<Grant>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let Some(handle) = self.0.as_mut() else {
            return std::task::Poll::Ready(None);
        };
        match std::pin::Pin::new(handle).poll(cx) {
            std::task::Poll::Ready(joined) => {
                self.0 = None;
                std::task::Poll::Ready(joined.ok())
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl Drop for PendingGrant {
    fn drop(&mut self) {
        if let Some(pending) = self.0.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                // Dropping the late grant returns it as a capacity-neutral failure.
                runtime.spawn(async move { drop(pending.await) });
            }
        }
    }
}

/// Run `boot` once it holds a session-boot permit of `priority` from `source`, and return the
/// permit with the boot's latency and the host's load. The wait counts the time queued only.
#[tracing::instrument(level = "debug", skip_all, fields(priority = ?priority))]
pub async fn admitted_boot<T, Boot>(
    source: Arc<dyn AdmissionSource>,
    priority: Priority,
    boot: Boot,
) -> AdmittedBoot<T>
where
    Boot: Future<Output = anyhow::Result<T>>,
{
    let request = AdmissionRequest {
        key: AdmissionKey::session_boot(),
        priority,
        enqueued_at_ms: pa_ai::admission::now_ms(),
    };
    let queued = std::time::Instant::now();
    let grant = if let Some(grant) = source.try_acquire(&request) {
        grant
    } else {
        // The acquire runs on its own task: a boot dropped while it waits must not leak a
        // permit a remote source grants after the drop.
        let Some(grant) = PendingGrant(Some(tokio::spawn(source.acquire(request)))).await else {
            return AdmittedBoot {
                result: Err(anyhow::anyhow!("session boot admission failed")),
                queued_ms: elapsed_ms(queued),
            };
        };
        grant
    };
    let queued_ms = elapsed_ms(queued);
    if queued_ms > 0 {
        WAITED.fetch_add(1, Ordering::Relaxed);
        WAIT_MS.fetch_add(queued_ms, Ordering::Relaxed);
    }
    let started = std::time::Instant::now();
    let result = boot.await;
    let boot_ms = elapsed_ms(started);
    let outcome = boot_outcome(
        match &result {
            Ok(_) => BootResult::Booted,
            Err(_) => BootResult::Failed,
        },
        HostLoad::sample(),
    );
    if outcome == Outcome::Overloaded {
        PRESSURE.fetch_add(1, Ordering::Relaxed);
    }
    let _cut = grant.finish(Observation {
        outcome,
        latency_ms: (outcome == Outcome::Success).then_some(boot_ms),
        limits: LimitReport::default(),
    });
    AdmittedBoot { result, queued_ms }
}

fn elapsed_ms(since: std::time::Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
