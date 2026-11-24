//! Interactive provider-failure auto-retry: the session-level retry loop
//! for user-driven turns ([`super::provider_retry`] is the one-shot helper).
//! This module owns only the retry decision and the event surface: the
//! caller drives the actual turn; events are delivered as data.

use std::future::Future;

use pa_agent::abort::AbortSignal;
use pa_agent::types::{AssistantMessage, StopReason};

use crate::models::QuotaFallback;

use super::provider_retry::{
    backoff_attempt, has_provider_stream_failure, is_agent_lifecycle_failure,
    is_context_overflow_failure, is_permanent_provider_failure_kind, is_unsupported_tool_failure,
    jittered_delay_ms, provider_retry_delay, provider_stream_failure_kind,
    provider_stream_failure_retry_after_ms, provider_stream_failure_status, retry_jitter_rand01,
    ProviderRetryDelay, ProviderRetryPolicy, OUTPUT_LIMIT_KIND,
};

/// The whole-episode retry ceiling (SANCTIONED DIVERGENCE, operator
/// ruling 2026-09-23): a turn gives up after 8 retries on its provider
/// whatever `retry.maxRetries` says - the same anchor as TS's own
/// provider-wait `maxParks: 8`.
pub const MAX_TOTAL_PROVIDER_RETRIES: u32 = 8;

/// Final replies cut off at the output limit in a row at which a turn gives
/// up: the ones before it are continued, so a turn sends at most two
/// continuations (the cap `OpenCode` uses). An identical retry tends to hit the same
/// limit again; a continuation keeps the cut reply and tells the model to
/// carry on in smaller pieces, and a model that is cut off a third time in
/// a row is not making progress that way either.
pub const MAX_CONSECUTIVE_OUTPUT_LIMIT_CUTS: u32 = 3;

/// What the retry driver asks of one attempt of the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnAttempt {
    /// The turn's first attempt.
    First,
    /// The previous attempt failed: its reply leaves the loop context and
    /// the run is re-issued.
    Retry,
    /// The previous reply was cut off at the output limit
    /// ([`OUTPUT_LIMIT_KIND`]): it stays in the loop context and the run
    /// continues behind the guiding row
    /// ([`super::AgentSession::continue_after_output_limit`]).
    ContinueAfterOutputLimit,
}

/// One retry-loop event, in the TS wire vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoRetryEvent {
    /// `auto_retry_start`: the loop parks the failed turn and waits.
    Start {
        /// 1-based retry number.
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    /// `auto_retry_end`: the loop settled. `final_error` is present exactly
    /// when `success` is false.
    End {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
}

/// The host's verdict on a credential rejection (kind `auth`) about to be retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRecovery {
    Retry,
    /// End the turn with this user-facing error.
    EndTurn(String),
}

pub type AuthRecoveryFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = AuthRecovery> + Send>>;

/// Consulted before a credential rejection's one retry.
pub type AuthRecoveryCallback<'a> = &'a mut dyn FnMut(&AssistantMessage) -> AuthRecoveryFuture;

pub type QuotaFallbackFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = QuotaFallback> + Send>>;

/// Consulted when a failure's reset lies beyond the retry cap (an exhausted
/// quota), with the reset in milliseconds: the host arms the provider's
/// declared quota fallback (`ModelRegistry::arm_quota_fallback`).
pub type QuotaFallbackCallback<'a> =
    &'a mut dyn FnMut(&AssistantMessage, u64) -> QuotaFallbackFuture;

/// The failed turn carrying the recovery's error, when the recovery ends it.
pub(super) async fn end_turn_on_auth_rejection(
    message: &AssistantMessage,
    auth_recovery: Option<&mut AuthRecoveryCallback<'_>>,
) -> Option<AssistantMessage> {
    if provider_stream_failure_kind(message).as_deref() != Some("auth") {
        return None;
    }
    match auth_recovery?(message).await {
        AuthRecovery::Retry => None,
        AuthRecovery::EndTurn(error) => {
            let mut message = message.clone();
            message.error_message = Some(error);
            Some(message)
        }
    }
}

/// Drive `attempt` under the shared retry policy until it settles: a turn
/// with stop reason `Error` is classified against the policy; `emit` observes
/// retry events, `wait` sleeps one delay (returning `false` aborts).
///
/// A reply cut off at the output limit ([`OUTPUT_LIMIT_KIND`]) is no retry:
/// the next attempt is asked to continue it ([`TurnAttempt`]) at once, with
/// no retry event, whether or not retries are enabled, until
/// [`MAX_CONSECUTIVE_OUTPUT_LIMIT_CUTS`] attempts in a row end cut off;
/// that one fails the turn like a permanent failure. Any other outcome ends
/// the run of cuts.
///
/// A server-requested wait above the policy cap (a quota reset) fails the
/// turn at once with the provider's raw error: nothing waits it out. When
/// `quota_fallback` arms the provider's declared fallback, the turn instead
/// retries once, at once, on the fallback credential; a further quota
/// failure, or a fallback that cannot serve, fails the turn with the raw
/// error plus a note naming the fallback.
/// `auth_recovery` is consulted before a credential rejection's retry.
///
/// # Errors
///
/// Returns the `attempt` future's error when a turn attempt fails, or the
/// `emit` callback's error while observing retry events.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn_with_auto_retry<A, AF, E, EF, W, WF>(
    policy: &ProviderRetryPolicy,
    context_window: u64,
    signal: Option<&AbortSignal>,
    mut attempt: A,
    mut emit: E,
    mut wait: W,
    mut auth_recovery: Option<AuthRecoveryCallback<'_>>,
    mut quota_fallback: Option<QuotaFallbackCallback<'_>>,
) -> anyhow::Result<AssistantMessage>
where
    A: FnMut(TurnAttempt) -> AF,
    AF: Future<Output = anyhow::Result<AssistantMessage>>,
    E: FnMut(AutoRetryEvent) -> EF,
    EF: Future<Output = anyhow::Result<()>>,
    W: FnMut(std::time::Duration) -> WF,
    WF: Future<Output = bool>,
{
    let max_retries = policy.max_retries.min(MAX_TOTAL_PROVIDER_RETRIES);
    let mut retries_performed = 0u32;
    let mut fallback_retried = false;
    let mut next = TurnAttempt::First;
    let mut output_limit_cuts = 0u32;
    loop {
        let message = attempt(next).await?;
        next = TurnAttempt::Retry;
        if message.stop_reason != StopReason::Error {
            if retries_performed > 0 {
                emit(AutoRetryEvent::End {
                    success: true,
                    attempt: retries_performed,
                    final_error: None,
                })
                .await?;
            }
            return Ok(message);
        }
        if signal.is_some_and(AbortSignal::is_aborted) {
            return Ok(with_stop_reason_aborted(message));
        }
        if provider_stream_failure_kind(&message).as_deref() == Some(OUTPUT_LIMIT_KIND) {
            output_limit_cuts += 1;
            if output_limit_cuts < MAX_CONSECUTIVE_OUTPUT_LIMIT_CUTS {
                next = TurnAttempt::ContinueAfterOutputLimit;
                continue;
            }
            let mut message = message;
            message.error_message = Some(format!(
                "The model's reply was cut off at its output limit {MAX_CONSECUTIVE_OUTPUT_LIMIT_CUTS} times in a row"
            ));
            emit(AutoRetryEvent::End {
                success: false,
                attempt: retries_performed,
                final_error: Some(final_error_of(&message)),
            })
            .await?;
            return Ok(message);
        }
        output_limit_cuts = 0;
        // Non-retryable failures never enter the retry bookkeeping: a
        // permanent failure only closes the active retry.
        let non_retryable = is_agent_lifecycle_failure(&message)
            // A context overflow can never succeed unchanged: the
            // compact-and-retry recovery owns it.
            || is_context_overflow_failure(&message, context_window)
            || is_unsupported_tool_failure(&message)
            || is_permanent_provider_failure_kind(
                provider_stream_failure_kind(&message).as_deref(),
                retries_performed,
                provider_stream_failure_status(&message),
            );
        if !policy.enabled || non_retryable {
            // SANCTIONED DIVERGENCE (the 402 diagnosis, operator ruling): the
            // outcome row is FAILURE-scoped, not episode-scoped. TS emits retry
            // events only once a retry was attempted; here a provider failure
            // discloses at attempt 0, the self-managed arms stay silent.
            if retries_performed > 0
                || (has_provider_stream_failure(&message)
                    && !is_context_overflow_failure(&message, context_window))
            {
                emit(AutoRetryEvent::End {
                    success: false,
                    attempt: retries_performed,
                    final_error: Some(final_error_of(&message)),
                })
                .await?;
            }
            return Ok(message);
        }
        if let Some(message) = end_turn_on_auth_rejection(&message, auth_recovery.as_mut()).await {
            emit(AutoRetryEvent::End {
                success: false,
                attempt: retries_performed,
                final_error: Some(final_error_of(&message)),
            })
            .await?;
            return Ok(message);
        }
        // The attempt counter bumps before deciding, so the exhaustion
        // check compares past the (ceiling-capped) `max_retries`.
        retries_performed += 1;
        let delay = provider_retry_delay(
            backoff_attempt(&message, retries_performed),
            provider_stream_failure_retry_after_ms(&message),
            policy,
        );
        let delay_ms = match delay {
            // Jittered (SANCTIONED DIVERGENCE, operator ruling 2026-09-23):
            // the jittered value is both waited and reported, so the countdown
            // stays honest while retried sessions spread off the ladder ticks.
            ProviderRetryDelay::Wait { delay_ms } => {
                if retries_performed > max_retries {
                    emit(AutoRetryEvent::End {
                        success: false,
                        attempt: retries_performed - 1,
                        final_error: Some(final_error_of(&message)),
                    })
                    .await?;
                    return Ok(message);
                }
                jittered_delay_ms(delay_ms, retry_jitter_rand01())
            }
            ProviderRetryDelay::ExceedsCap { retry_after_ms } => {
                // A reset beyond the cap is an exhausted quota: the turn
                // fails now with the provider's own error text, unless the
                // provider's declared fallback serves one immediate retry.
                let decision = match quota_fallback.as_deref_mut() {
                    Some(arm) => arm(&message, retry_after_ms).await,
                    None => QuotaFallback::NotDeclared,
                };
                let mut message = message;
                match decision {
                    QuotaFallback::Armed { notice, .. } if !fallback_retried => {
                        fallback_retried = true;
                        emit(AutoRetryEvent::Start {
                            attempt: retries_performed,
                            max_attempts: max_retries,
                            delay_ms: 0,
                            error_message: notice,
                        })
                        .await?;
                        continue;
                    }
                    QuotaFallback::Unavailable { reason } => {
                        message.error_message = Some(format!(
                            "{}\n\nQuota fallback: {reason}.",
                            final_error_of(&message)
                        ));
                    }
                    // One fallback retry per turn: a second arming fails raw.
                    QuotaFallback::Armed { .. } | QuotaFallback::NotDeclared => {}
                }
                emit(AutoRetryEvent::End {
                    success: false,
                    attempt: retries_performed - 1,
                    final_error: Some(final_error_of(&message)),
                })
                .await?;
                return Ok(message);
            }
        };
        emit(AutoRetryEvent::Start {
            attempt: retries_performed,
            max_attempts: max_retries,
            delay_ms,
            error_message: final_error_of(&message),
        })
        .await?;
        if !wait(std::time::Duration::from_millis(delay_ms)).await {
            emit(AutoRetryEvent::End {
                success: false,
                attempt: retries_performed,
                final_error: Some("Retry cancelled".to_string()),
            })
            .await?;
            return Ok(with_stop_reason_aborted(message));
        }
    }
}

/// The user-visible error text of a failed turn.
fn final_error_of(message: &AssistantMessage) -> String {
    message
        .error_message
        .as_deref()
        .filter(|error| !error.is_empty())
        .unwrap_or("Unknown error")
        .to_string()
}

fn with_stop_reason_aborted(mut message: AssistantMessage) -> AssistantMessage {
    message.stop_reason = StopReason::Aborted;
    message
}

// The inline unit battery moved to the child module at the same tree
// position (session_engine::auto_retry::tests); its use-super glob keeps
// resolving through this facade's bindings (the request-timing precedent,
// #3084).
#[cfg(test)]
mod tests;
