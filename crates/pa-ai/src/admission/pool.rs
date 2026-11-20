//! The in-process permit pool: one adaptive controller, rate bucket, and
//! deadline-ordered waiter set per [`AdmissionKey`].

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use pa_types::control::{
    service_deadline_ms, AdjustReason, Controller, ControllerConfig, LearnedState, Observation,
    RateBucket,
};
use pa_types::provider_usage::ProviderPacing;
use tokio::sync::Notify;

use super::{now_ms, AdmissionKey, AdmissionRequest, AdmissionSource, Grant, GrantFuture};

/// The longest a waiter sleeps before re-checking its slot, so a wakeup
/// that raced the sleep costs at most this much.
const RECHECK_MS: u64 = 250;

struct Slot {
    controller: Controller,
    bucket: RateBucket,
    in_flight: u32,
    /// `(deadline, ticket)`: the first entry is served next.
    waiters: BTreeSet<(u64, u64)>,
    next_ticket: u64,
    notify: Arc<Notify>,
}

impl Slot {
    fn new(controller: Controller, now_ms: u64) -> Self {
        let bucket = RateBucket::new(controller.target().rate_per_min, now_ms);
        Self {
            controller,
            bucket,
            in_flight: 0,
            waiters: BTreeSet::new(),
            next_ticket: 0,
            notify: Arc::new(Notify::new()),
        }
    }
}

struct Inner {
    config: ControllerConfig,
    slots: Mutex<HashMap<AdmissionKey, Slot>>,
}

impl Inner {
    /// The tuning of `key`'s slot: the session-boot budget runs its own.
    fn config_for(&self, key: &AdmissionKey) -> ControllerConfig {
        if key.is_session_boot() {
            super::session_boot_controller_config()
        } else {
            self.config
        }
    }

    fn with_slot<T>(&self, key: &AdmissionKey, apply: impl FnOnce(&mut Slot, u64) -> T) -> T {
        let now = now_ms();
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = slots
            .entry(key.clone())
            .or_insert_with(|| Slot::new(Controller::new(self.config_for(key), now), now));
        apply(slot, now)
    }

    /// Return a permit and fold its observation in. `true` when the
    /// controller cut capacity.
    fn finish(&self, key: &AdmissionKey, observation: &Observation) -> bool {
        self.with_slot(key, |slot, now| {
            slot.in_flight = slot.in_flight.saturating_sub(1);
            let before = slot.controller.target();
            let after = slot.controller.observe(observation, now);
            if after.rate_per_min != before.rate_per_min {
                slot.bucket.set_rate(after.rate_per_min, now);
            }
            let requests = observation.limits.requests;
            if let Some(remaining) = requests.remaining {
                slot.bucket.sync(
                    remaining.saturating_sub(u64::from(slot.in_flight)),
                    requests.reset_ms,
                    now,
                );
            }
            let tokens = observation.limits.tokens;
            if tokens.remaining == Some(0) {
                slot.bucket.sync(0, tokens.reset_ms, now);
            }
            slot.notify.notify_waiters();
            matches!(
                after.reason,
                AdjustReason::Throttled | AdjustReason::Overloaded | AdjustReason::LatencyRise
            ) && (after.concurrency < before.concurrency
                || after.rate_per_min < before.rate_per_min)
        })
    }
}

/// A process-local permit pool. Cheap to clone: clones share one pool.
#[derive(Clone)]
pub struct LocalAdmission {
    inner: Arc<Inner>,
}

/// Removes a waiter that gave up (or whose request was dropped) and lets
/// the next one in.
struct Ticket<'a> {
    inner: &'a Inner,
    key: &'a AdmissionKey,
    entry: (u64, u64),
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        self.inner.with_slot(self.key, |slot, _| {
            if slot.waiters.remove(&self.entry) {
                slot.notify.notify_waiters();
            }
        });
    }
}

impl LocalAdmission {
    #[must_use]
    pub fn new(config: ControllerConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                slots: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Seed keys from persisted learned state; keys already live keep
    /// what they learned in this process.
    pub fn seed(&self, learned: impl IntoIterator<Item = (AdmissionKey, LearnedState)>) {
        let now = now_ms();
        let mut slots = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, state) in learned {
            let config = self.inner.config_for(&key);
            slots
                .entry(key)
                .or_insert_with(|| Slot::new(Controller::restore(config, state, now), now));
        }
    }

    /// Every key's learned state, for persistence.
    #[must_use]
    pub fn learned(&self) -> Vec<(AdmissionKey, LearnedState)> {
        let slots = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut learned: Vec<_> = slots
            .iter()
            .map(|(key, slot)| (key.clone(), slot.controller.learned()))
            .collect();
        learned.sort_by(|(left, _), (right, _)| left.cmp(right));
        learned
    }

    /// The read-only controller view per provider key (the `/usage` page's pacing rows; the
    /// session-boot budget is not a provider and has no row).
    #[must_use]
    pub fn pacing(&self) -> Vec<ProviderPacing> {
        let slots = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: Vec<ProviderPacing> = slots
            .iter()
            .filter(|(key, _)| !key.is_session_boot())
            .map(|(key, slot)| {
                let learned = slot.controller.learned();
                ProviderPacing {
                    provider: key.provider.clone(),
                    model: key.scope.clone(),
                    concurrency: learned.concurrency,
                    rate_per_min: learned.rate_per_min,
                    concurrency_ceiling: learned.concurrency_ceiling,
                    rate_ceiling: learned.rate_ceiling,
                    in_flight: slot.in_flight,
                    reason: learned.reason,
                }
            })
            .collect();
        rows.sort_by(|left, right| {
            (&left.provider, &left.model).cmp(&(&right.provider, &right.model))
        });
        rows
    }

    /// Wait for a permit until `give_up` (forever when `None`). `None`
    /// only when the deadline passed first.
    pub async fn acquire_until(
        &self,
        request: &AdmissionRequest,
        give_up: Option<tokio::time::Instant>,
    ) -> Option<Grant> {
        let started = now_ms();
        let deadline = service_deadline_ms(request.priority, request.enqueued_at_ms);
        let entry = self.inner.with_slot(&request.key, |slot, _| {
            let entry = (deadline, slot.next_ticket);
            slot.next_ticket += 1;
            slot.waiters.insert(entry);
            entry
        });
        let ticket = Ticket {
            inner: &self.inner,
            key: &request.key,
            entry,
        };
        loop {
            let (notify, wait_ms) = self.inner.with_slot(&request.key, |slot, now| {
                let target = slot.controller.target();
                let wait_ms =
                    if let Some(until) = target.blocked_until_ms.filter(|until| *until > now) {
                        until - now
                    } else if slot.waiters.first() != Some(&entry)
                        || slot.in_flight >= target.concurrency
                    {
                        RECHECK_MS
                    } else {
                        match slot.bucket.try_take(now) {
                            Ok(()) => {
                                slot.waiters.remove(&entry);
                                slot.in_flight += 1;
                                0
                            }
                            Err(wait_ms) => wait_ms,
                        }
                    };
                (Arc::clone(&slot.notify), wait_ms)
            });
            if wait_ms == 0 {
                std::mem::forget(ticket);
                let inner = Arc::clone(&self.inner);
                let key = request.key.clone();
                return Some(Grant::new(
                    now_ms().saturating_sub(started),
                    Box::new(move |observation| inner.finish(&key, &observation)),
                ));
            }
            let sleep =
                tokio::time::sleep(std::time::Duration::from_millis(wait_ms.min(RECHECK_MS)));
            match give_up {
                Some(give_up) => {
                    tokio::select! {
                        () = notify.notified() => {}
                        () = sleep => {}
                        () = tokio::time::sleep_until(give_up) => return None,
                    }
                }
                None => {
                    tokio::select! {
                        () = notify.notified() => {}
                        () = sleep => {}
                    }
                }
            }
        }
    }
}

impl AdmissionSource for LocalAdmission {
    /// Granted only when nobody is queued, no server-requested wait holds the key, the
    /// concurrency cap has room, and the bucket has a token.
    fn try_acquire(&self, request: &AdmissionRequest) -> Option<Grant> {
        let granted = self.inner.with_slot(&request.key, |slot, now| {
            let target = slot.controller.target();
            let open = slot.waiters.is_empty()
                && target.blocked_until_ms.is_none_or(|until| until <= now)
                && slot.in_flight < target.concurrency
                && slot.bucket.try_take(now).is_ok();
            if open {
                slot.in_flight += 1;
            }
            open
        });
        granted.then(|| {
            let inner = Arc::clone(&self.inner);
            let key = request.key.clone();
            Grant::new(
                0,
                Box::new(move |observation| inner.finish(&key, &observation)),
            )
        })
    }

    fn acquire(&self, request: AdmissionRequest) -> GrantFuture {
        let pool = self.clone();
        Box::pin(async move {
            pool.acquire_until(&request, None)
                .await
                .expect("an acquire without a deadline always grants")
        })
    }
}
