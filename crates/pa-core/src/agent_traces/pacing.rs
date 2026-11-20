//! The one trace-upload controller: every request to the traces endpoint — the continuous
//! delivery of live sessions, its catch-up sweep, and the `/traces` upload-all sweep — takes a
//! permit from one shared, background-class gate, so the paths never stack their limits on top
//! of each other (they used to: a 4-permit delivery semaphore beside a separate 4-way sweep and
//! its own 12 s request spacing).
//!
//! The gate hosts `pa_types::control`'s adaptive controller through pa-ai's permit pool on
//! tokio's clock: it starts at the platform's documented 5 requests a minute and two in flight,
//! widens to four while requests succeed, cuts on a 429 or 503 (honoring `Retry-After`), and
//! probes back up as the cut ages out.

use std::sync::OnceLock;

use pa_ai::admission::{AdmissionKey, AdmissionRequest, AdmissionSource, Grant, LocalAdmission};
use pa_types::control::{ControllerConfig, LimitReport, Observation, Outcome, Priority};

use super::{
    retry_after_delay, TraceHttpError, TraceHttpResponse, TraceUploadCancel, TraceUploadDelay,
    TraceUploadDelaySink, TRACE_UPLOAD_RATE_LIMIT_WINDOW_MS,
};

/// The platform's documented limit: 5 requests a minute.
const TRACE_RATE_PER_MIN: u32 = 5;

/// The trace endpoint's tuning (see the module docs).
const TRACE_CONTROLLER_CONFIG: ControllerConfig = ControllerConfig {
    initial_concurrency: 2,
    min_concurrency: 1,
    max_concurrency: 4,
    initial_rate_per_min: TRACE_RATE_PER_MIN,
    min_rate_per_min: 1,
    max_rate_per_min: TRACE_RATE_PER_MIN,
    rate_step_per_min: 1,
    decrease_keep_pct: 50,
    latency_keep_pct: 75,
    latency_rise_pct: 300,
    latency_rise_samples: 3,
    headroom_pct: 0,
    decrease_spacing_ms: 2_000,
    ceiling_ttl_ms: 15 * 60_000,
};

/// Milliseconds on tokio's clock (paused in tests), from the first read.
fn tokio_clock_ms() -> u64 {
    static BASE: OnceLock<tokio::time::Instant> = OnceLock::new();
    let base = *BASE.get_or_init(tokio::time::Instant::now);
    u64::try_from(
        tokio::time::Instant::now()
            .saturating_duration_since(base)
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn trace_key() -> AdmissionKey {
    AdmissionKey {
        provider: super::PRIME_AGENT_TRACES_PROVIDER_ID.to_string(),
        credential: String::new(),
        scope: String::new(),
    }
}

/// A handle on a trace-upload controller. Clones share it.
#[derive(Clone)]
pub struct TraceRequestGate {
    pool: LocalAdmission,
}

impl Default for TraceRequestGate {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceRequestGate {
    /// A controller of its own (tests, and a caller that wants an isolated budget).
    #[must_use]
    pub fn new() -> Self {
        Self {
            pool: LocalAdmission::with_clock(TRACE_CONTROLLER_CONFIG, tokio_clock_ms),
        }
    }

    /// The process-wide controller every trace upload shares.
    #[must_use]
    pub fn shared() -> Self {
        static SHARED: OnceLock<TraceRequestGate> = OnceLock::new();
        SHARED.get_or_init(Self::new).clone()
    }

    /// Wait for a request permit (background class); a cancel ends the wait. A wait reports
    /// the rate interval it is held to through `on_upload_delay`.
    pub(super) async fn before_request(
        &self,
        cancel: Option<&TraceUploadCancel>,
        on_upload_delay: Option<&TraceUploadDelaySink>,
    ) -> Result<TracePermit, TraceHttpError> {
        let request = AdmissionRequest {
            key: trace_key(),
            priority: Priority::Background,
            enqueued_at_ms: self.pool.now_ms(),
        };
        if let Some(grant) = self.pool.try_acquire(&request) {
            return Ok(self.permit(grant));
        }
        if let Some(sink) = on_upload_delay {
            let rate = u64::from(self.pool.target(&request.key).rate_per_min.max(1));
            sink(TraceUploadDelay::RateLimit(60_000_u64.div_ceil(rate)));
        }
        let acquire = self.pool.acquire_until(&request, None);
        let grant = match cancel {
            Some(cancel) => tokio::select! {
                grant = acquire => grant,
                () = cancel.wait() => None,
            },
            None => acquire.await,
        };
        match grant {
            Some(grant) if !cancel.is_some_and(TraceUploadCancel::is_cancelled) => {
                Ok(self.permit(grant))
            }
            Some(_) | None => Err(TraceHttpError::Cancelled),
        }
    }

    fn permit(&self, grant: Grant) -> TracePermit {
        TracePermit {
            grant,
            started_ms: self.pool.now_ms(),
            pool: self.pool.clone(),
        }
    }

    /// The target the controller applies now.
    #[cfg(test)]
    pub(super) fn target(&self) -> pa_types::control::Target {
        self.pool.target(&trace_key())
    }

    /// Requests in flight on this controller now.
    #[cfg(test)]
    pub(super) fn in_flight(&self) -> u32 {
        self.pool.in_flight(&trace_key())
    }
}

/// One admitted request: returned with what the endpoint answered.
pub(super) struct TracePermit {
    grant: Grant,
    started_ms: u64,
    pool: LocalAdmission,
}

impl TracePermit {
    /// Return the permit with the request's result.
    pub(super) fn finish(self, result: &Result<TraceHttpResponse, TraceHttpError>) {
        let latency_ms = self.pool.now_ms().saturating_sub(self.started_ms);
        let _cut = self.grant.finish(observe(result, latency_ms));
    }
}

/// What one trace request tells the controller: a 429 is a throttle and a 503 an overload
/// (both with the server's `Retry-After`), a 2xx a success with its latency; anything else
/// says nothing about capacity.
fn observe(result: &Result<TraceHttpResponse, TraceHttpError>, latency_ms: u64) -> Observation {
    let (outcome, retry_after) = match result {
        Ok(response) => match response.status {
            429 => (Outcome::Throttled, response.retry_after.as_deref()),
            503 => (Outcome::Overloaded, response.retry_after.as_deref()),
            200..=299 => (Outcome::Success, None),
            _ => (Outcome::Failed, None),
        },
        Err(
            TraceHttpError::TimedOut { .. }
            | TraceHttpError::Cancelled
            | TraceHttpError::Transport(_),
        ) => (Outcome::Failed, None),
    };
    Observation {
        outcome,
        latency_ms: (outcome == Outcome::Success).then_some(latency_ms),
        limits: LimitReport {
            retry_after_ms: retry_after_delay(retry_after, TRACE_UPLOAD_RATE_LIMIT_WINDOW_MS),
            ..LimitReport::default()
        },
    }
}

#[cfg(test)]
mod tests;
