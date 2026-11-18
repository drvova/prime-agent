//! Prompt-cache seam at the session's `StreamFn` boundary.
//!
//! Every real request passes through [`wrap_stream_fn`], which
//! - asks the provider for the session's cache retention (the
//!   `PI_CACHE_RETENTION` env override, else the `cacheRetention` setting);
//! - names the system prompt's cache-stable prefix (its static layers), so
//!   providers with explicit cache markers mark it as its own block and a new
//!   session or subagent reads those layers from the cache;
//! - folds the request's cache saving (reads against writes) into the
//!   session counters;
//! - arms one cache warmer for the session from the settled round (see
//!   [`policy`]): near the end of the cache entry's lifetime the warmer
//!   re-sends that round with one output token when the expected saving
//!   clears the threshold, and stops after a cap.
//!
//! The warmer runs on its own task and never sits on the request path: the
//! next real request cancels it synchronously before it is dispatched (a
//! refresh not yet sent is never sent, one on the wire is dropped), and never
//! awaits it.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use pa_agent::stream::{
    AssistantMessageEvent, LlmContext, ModelStream, OnPayloadHook, StreamFn, StreamRequestOptions,
};
use pa_agent::types::{AssistantMessage, Model, StopReason};
use tokio_util::sync::CancellationToken;

use super::telemetry::SessionCounters;
use crate::settings::{CacheRetentionSetting, CacheWarmingSetting};

pub(crate) mod policy;

#[cfg(test)]
mod tests;

use policy::{CachePrices, DueWarmer, WarmPhase, WarmShape, WarmVerdict};

/// Answers whether the session still has work in flight that will wake it:
/// a live background `bash()` job or a running subagent.
pub(crate) type PendingWorkProbe = Arc<dyn Fn() -> pa_agent::BoxFut<'static, bool> + Send + Sync>;

/// Where the session sits in the recursion tree: a subagent whose turn
/// settled waits on its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionRole {
    Root,
    Subagent,
}

/// The session's cache policy and the seams the warmer reaches.
pub(crate) struct CacheWarmingConfig {
    pub mode: CacheWarmingSetting,
    pub retention: CacheRetentionSetting,
    /// The system prompt's static layers; a request whose system prompt
    /// starts with them marks them as their own cache block.
    pub static_prefix: String,
    pub role: SessionRole,
    pub pending_work: PendingWorkProbe,
    pub counters: Arc<SessionCounters>,
    /// The raw provider stream a refresh goes out on: below the request
    /// timing and semantic-edge wrappers, so a refresh is neither a timed
    /// turn nor a ledger edge.
    pub warm_stream_fn: StreamFn,
}

struct Shared {
    config: CacheWarmingConfig,
    armed: Mutex<ArmedSlot>,
}

/// The session's one armed warmer: the generation of the latest real
/// request and the cancel handle of the warmer it armed, if any.
#[derive(Default)]
struct ArmedSlot {
    generation: u64,
    cancel: Option<CancellationToken>,
}

impl Shared {
    /// A real request starts: take down the armed warmer (unscheduling it,
    /// or dropping its refresh mid-flight) and open a new generation. Only
    /// a lock and a cancel: nothing here waits on the warmer.
    fn begin_real_request(&self) -> u64 {
        let mut slot = self.armed.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cancel) = slot.cancel.take() {
            cancel.cancel();
        }
        slot.generation += 1;
        slot.generation
    }
}

/// Cancels the armed warmer when the session's stream fn drops.
struct DisarmOnDrop(Arc<Shared>);

impl Drop for DisarmOnDrop {
    fn drop(&mut self) {
        self.0.begin_real_request();
    }
}

/// The request a refresh repeats, captured before it went out.
struct WarmReplay {
    model: Model,
    context: LlmContext,
    options: StreamRequestOptions,
}

/// A real round that settled: what its warmer repeats and prices.
struct SettledRound {
    replay: WarmReplay,
    shape: WarmShape,
    turn: TurnState,
    prompt_tokens: u64,
    prices: CachePrices,
    started: Instant,
}

/// Whether the round left its turn open (tool calls to run) or settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    Open,
    Settled,
}

/// Set the request's cache hints the caller left unset: the retention (the
/// env override, else the setting) and the static-prefix boundary when the
/// system prompt starts with `static_prefix`.
fn apply_cache_hints(
    retention: CacheRetentionSetting,
    static_prefix: &str,
    context: &LlmContext,
    options: &mut StreamRequestOptions,
) {
    if options.cache_retention.is_none() {
        options.cache_retention = policy::request_cache_retention(
            std::env::var(policy::CACHE_RETENTION_ENV).ok().as_deref(),
            retention,
        );
    }
    if options.cached_system_prefix_len.is_none() && !static_prefix.is_empty() {
        options.cached_system_prefix_len = context
            .system_prompt
            .as_deref()
            .filter(|prompt| prompt.starts_with(static_prefix))
            .map(|_| static_prefix.len());
    }
}

/// Give a forked request (one that reads the session's cache but is not a
/// turn of its own, like the goal judge) the session's cache hints, so the
/// provider renders the same system blocks and markers as the main loop and
/// the fork reads its cache entry. It neither arms nor cancels the warmer.
pub(crate) fn with_cache_hints(
    retention: CacheRetentionSetting,
    static_prefix: String,
    inner: StreamFn,
) -> StreamFn {
    Arc::new(move |model, context, mut options| {
        apply_cache_hints(retention, &static_prefix, &context, &mut options);
        inner(model, context, options)
    })
}

/// Wrap the session's agent stream fn with the cache seam. `inner` serves
/// real requests; refreshes go out on `config.warm_stream_fn`.
pub(crate) fn wrap_stream_fn(config: CacheWarmingConfig, inner: StreamFn) -> StreamFn {
    let shared = Arc::new(Shared {
        config,
        armed: Mutex::new(ArmedSlot::default()),
    });
    let guard = Arc::new(DisarmOnDrop(Arc::clone(&shared)));
    Arc::new(
        move |model: Model, context: LlmContext, mut options: StreamRequestOptions| {
            let _keep_alive = &guard;
            // First, before anything else: no refresh goes out once a real
            // request has started.
            let generation = shared.begin_real_request();
            let config = &shared.config;
            apply_cache_hints(
                config.retention,
                &config.static_prefix,
                &context,
                &mut options,
            );
            let replay = match config.mode {
                CacheWarmingSetting::Off => None,
                CacheWarmingSetting::Active | CacheWarmingSetting::Idle => Some(WarmReplay {
                    model: model.clone(),
                    context: context.clone(),
                    options: StreamRequestOptions {
                        max_tokens: Some(1),
                        signal: pa_agent::abort::AbortSignal::never(),
                        on_payload: None,
                        on_response: None,
                        ..options.clone()
                    },
                }),
            };
            // The provider shows the payload it sends: its cache markers
            // say whether the round can be repeated, and for how long.
            let shape: Arc<Mutex<Option<WarmShape>>> = Arc::default();
            let observed = Arc::clone(&shape);
            let inner_payload: Option<OnPayloadHook> = options.on_payload.take();
            options.on_payload = Some(Arc::new(move |payload, model| {
                let next = inner_payload
                    .as_ref()
                    .and_then(|hook| hook(payload.clone(), model));
                let sent = next.as_ref().unwrap_or(&payload);
                *observed.lock().unwrap_or_else(PoisonError::into_inner) = policy::warm_shape(sent);
                next
            }));
            let pending = PendingRound {
                shared: Arc::clone(&shared),
                generation,
                replay,
                shape,
                cost: model.cost.clone(),
                started: Instant::now(),
            };
            let request = inner(model, context, options);
            Box::pin(async move {
                let stream = request.await?;
                Ok(Box::new(SettlingStream {
                    inner: stream,
                    pending: Some(pending),
                }) as Box<dyn ModelStream>)
            })
        },
    )
}

/// A real round in flight: settles into the counters and the warmer once
/// its final message arrives.
struct PendingRound {
    shared: Arc<Shared>,
    generation: u64,
    replay: Option<WarmReplay>,
    shape: Arc<Mutex<Option<WarmShape>>>,
    cost: pa_agent::types::UsageCost,
    started: Instant,
}

impl PendingRound {
    fn settle(self, message: &AssistantMessage) {
        let turn = match message.stop_reason {
            StopReason::ToolUse => TurnState::Open,
            StopReason::Stop | StopReason::Length => TurnState::Settled,
            StopReason::Error | StopReason::Aborted => return,
        };
        let shape = *self.shape.lock().unwrap_or_else(PoisonError::into_inner);
        let ttl = shape.map_or(Duration::ZERO, |shape| shape.ttl);
        let prices = CachePrices::for_ttl(&self.cost, ttl);
        let usage = &message.usage;
        if usage.cache_read + usage.cache_write > 0 {
            self.shared
                .config
                .counters
                .note_cache_savings(prices.request_savings(usage));
        }
        let (Some(replay), Some(shape)) = (self.replay, shape) else {
            return;
        };
        let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
        if prompt_tokens == 0 {
            return;
        }
        // Arm the warmer for this round, unless a later real request
        // already started.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let cancel = CancellationToken::new();
        {
            let mut slot = self
                .shared
                .armed
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if slot.generation != self.generation {
                return;
            }
            if let Some(previous) = slot.cancel.replace(cancel.clone()) {
                previous.cancel();
            }
        }
        let round = SettledRound {
            replay,
            shape,
            turn,
            prompt_tokens,
            prices,
            started: self.started,
        };
        runtime.spawn(run_warmer(Arc::clone(&self.shared), round, cancel));
    }
}

/// The real round's stream, settling its round on the final message.
struct SettlingStream {
    inner: Box<dyn ModelStream>,
    pending: Option<PendingRound>,
}

impl ModelStream for SettlingStream {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        Box::pin(async move {
            let event = self.inner.next_event().await?;
            if let AssistantMessageEvent::Done { message, .. } = &event {
                if let Some(pending) = self.pending.take() {
                    pending.settle(message);
                }
            }
            Some(event)
        })
    }

    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        Box::pin(async move {
            let result = self.inner.result().await;
            if let (Ok(message), Some(pending)) = (&result, self.pending.take()) {
                pending.settle(message);
            }
            result
        })
    }

    fn close(&mut self) {
        self.pending = None;
        self.inner.close();
    }
}

/// One armed warmer: wait until the refresh is due, judge it, send it, and
/// repeat until the verdict stops it or a real request cancels it.
#[tracing::instrument(level = "debug", name = "cache_warmer", skip_all)]
async fn run_warmer(shared: Arc<Shared>, round: SettledRound, cancel: CancellationToken) {
    let config = &shared.config;
    let mut last_read_start = round.started;
    let mut refreshes = 0u32;
    loop {
        let delay = policy::refresh_delay(round.shape.ttl, last_read_start.elapsed());
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        let phase = match round.turn {
            TurnState::Open => WarmPhase::ToolRunning,
            TurnState::Settled => {
                let pending_work = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    pending = (config.pending_work)() => pending,
                };
                match (pending_work, config.role) {
                    (true, _) => WarmPhase::AwaitingWork,
                    (false, SessionRole::Subagent) => WarmPhase::AwaitingParent,
                    (false, SessionRole::Root) => WarmPhase::Idle,
                }
            }
        };
        let verdict = policy::verdict(&DueWarmer {
            mode: config.mode,
            phase,
            age: round.started.elapsed(),
            refreshes,
            prompt_tokens: round.prompt_tokens,
            prices: round.prices,
        });
        match verdict {
            WarmVerdict::Stop(reason) => {
                tracing::debug!(?reason, ?phase, refreshes, "cache warmer stopped");
                config.counters.note_cache_warm_skipped();
                return;
            }
            WarmVerdict::Refresh {
                expected_saving_usd,
            } => tracing::debug!(?phase, expected_saving_usd, "cache warmer refreshing"),
        }
        let refresh_start = Instant::now();
        let request = (config.warm_stream_fn)(
            round.replay.model.clone(),
            round.replay.context.clone(),
            round.replay.options.clone(),
        );
        // Biased toward the cancel: a refresh a real request overtook before
        // its first poll never reaches the provider.
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            outcome = async {
                let mut stream = request.await?;
                while stream.next_event().await.is_some() {}
                stream.result().await
            } => outcome,
        };
        match outcome {
            Ok(message)
                if !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) =>
            {
                config
                    .counters
                    .note_cache_warm_sent(message.usage.cost.total);
                last_read_start = refresh_start;
                refreshes += 1;
            }
            // A refresh that fails is not retried: the next real round arms again.
            Ok(_) | Err(_) => {
                config.counters.note_cache_warm_skipped();
                return;
            }
        }
    }
}
