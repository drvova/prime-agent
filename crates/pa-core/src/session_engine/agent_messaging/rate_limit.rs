//! The per-sender agent-message rate bucket: every delivery a session's
//! `agent_message.send` attempts takes one token (a broadcast takes one per
//! recipient), the bucket bursts [`DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY`]
//! and then refills one token per [`DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    AGENT_MESSAGE_WAKE_REFUSED_STATUS, DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY,
    DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS,
};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// The clock the bucket reads: `Instant::now` in production, a manual clock in tests.
pub(crate) type AgentMessageClock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The deliveries one `agent_message.send` call had refused (counts only): the sender's rate
/// bucket throttled them, or an idle receiver's wake budget refused them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AgentMessageRefusals {
    pub(crate) throttled: u64,
    pub(crate) wake_refused: u64,
}

/// Hears the refusals of one `agent_message.send` call that had any.
pub(crate) type AgentMessageRefusalObserver = Arc<dyn Fn(AgentMessageRefusals) + Send + Sync>;

/// The broadcast receipt field naming a throttled recipient's retry hint.
pub(crate) const RETRY_AFTER_MS_FIELD: &str = "retryAfterMs";

/// A delivery refused because the sender's bucket is empty; `retry_after_ms`
/// is when the next token lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentMessageThrottled {
    pub(crate) retry_after_ms: u64,
}

impl std::fmt::Display for AgentMessageThrottled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Agent message rate limit exceeded ({DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY} burst, then 1 per {DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS} ms); retry after {} ms",
            self.retry_after_ms
        )
    }
}

impl std::error::Error for AgentMessageThrottled {}

struct Bucket {
    tokens: usize,
    /// When the refill window that earns the next token started.
    refilled_at: Instant,
}

/// One sending session's token bucket.
pub(crate) struct AgentMessageRateLimiter {
    clock: AgentMessageClock,
    bucket: Mutex<Bucket>,
}

impl AgentMessageRateLimiter {
    pub(crate) fn new(clock: AgentMessageClock) -> Self {
        let now = clock();
        AgentMessageRateLimiter {
            clock,
            bucket: Mutex::new(Bucket {
                tokens: DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY,
                refilled_at: now,
            }),
        }
    }

    /// Take one token for one delivery, or answer when the next one lands.
    pub(crate) fn acquire(&self) -> Result<(), AgentMessageThrottled> {
        let refill = Duration::from_millis(DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS);
        let now = (self.clock)();
        let mut bucket = self
            .bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let elapsed = now.saturating_duration_since(bucket.refilled_at);
        let earned =
            usize::try_from(elapsed.as_millis() / refill.as_millis()).unwrap_or(usize::MAX);
        let tokens = bucket.tokens.saturating_add(earned);
        if tokens >= DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY {
            // A full bucket earns nothing: the next refill window starts at the next spend.
            bucket.tokens = DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY;
            bucket.refilled_at = now;
        } else if earned > 0 {
            // Below capacity `earned` is a handful of windows; the partial window carries over.
            bucket.tokens = tokens;
            bucket.refilled_at += refill * u32::try_from(earned).unwrap_or(u32::MAX);
        }
        if bucket.tokens == 0 {
            let waited = now.saturating_duration_since(bucket.refilled_at);
            let retry_after = refill.saturating_sub(waited);
            return Err(AgentMessageThrottled {
                retry_after_ms: u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX),
            });
        }
        bucket.tokens -= 1;
        Ok(())
    }
}

/// Count the deliveries each `agent_message.send` call had refused into `observer`: wraps the
/// registered send handler (a no-op when none is registered). A throttled direct send is one
/// error; a broadcast counts its receipts that carry a retry hint and no delivery status. A
/// wake-refused receipt (direct, or a broadcast entry) carries the `wake_refused` status.
pub(crate) fn observe_agent_message_refusals(
    handlers: &mut HostRequestHandlers,
    observer: AgentMessageRefusalObserver,
) {
    let Some(send) = handlers.get("agent_message.send").cloned() else {
        return;
    };
    handlers.register(
        "agent_message.send",
        host_handler(move |payload| {
            let send = Arc::clone(&send);
            let observer = Arc::clone(&observer);
            async move {
                let result = send(payload).await;
                let wake_refused = |receipt: &Value| {
                    receipt.get("deliveryStatus").and_then(Value::as_str)
                        == Some(AGENT_MESSAGE_WAKE_REFUSED_STATUS)
                };
                let refusals = match &result {
                    Ok(value) => match value.get("receipts").and_then(Value::as_array) {
                        Some(receipts) => AgentMessageRefusals {
                            throttled: receipts
                                .iter()
                                .filter(|receipt| {
                                    receipt.get(RETRY_AFTER_MS_FIELD).is_some()
                                        && receipt.get("deliveryStatus").is_none()
                                })
                                .count() as u64,
                            wake_refused: receipts
                                .iter()
                                .filter(|receipt| wake_refused(receipt))
                                .count() as u64,
                        },
                        None => AgentMessageRefusals {
                            throttled: 0,
                            wake_refused: u64::from(wake_refused(value)),
                        },
                    },
                    Err(error) => AgentMessageRefusals {
                        throttled: u64::from(
                            error.downcast_ref::<AgentMessageThrottled>().is_some(),
                        ),
                        wake_refused: 0,
                    },
                };
                if refusals != AgentMessageRefusals::default() {
                    observer(refusals);
                }
                result
            }
        }),
    );
}
