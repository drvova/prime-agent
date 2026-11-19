//! Per-provider WebSocket session state: the session connection cache with idle expiry and a
//! maximum connection age, SSE-fallback pinning, debug stats, and continuation bookkeeping.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::connection::{spawn_connection_worker, AcquiredConnection, WireProfile, WorkerCommand};
use super::continuation::ContinuationState;
use super::errors::ResponsesStreamError;

/// Idle time after which a session's cached connection closes.
pub(crate) const SESSION_WEBSOCKET_CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Debug counters.
#[derive(Debug, Clone, Default)]
pub struct WebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: u64,
    pub last_delta_input_items: Option<u64>,
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    pub websocket_fallback_active: Option<bool>,
    pub last_websocket_error: Option<String>,
}

pub struct CachedConnection {
    /// Command channel to the connection's worker task.
    pub worker: mpsc::Sender<WorkerCommand>,
    pub busy: bool,
    pub continuation: Option<ContinuationState>,
    /// Generation counter for pending idle-expiry tasks.
    pub expiry_generation: u64,
    /// Unique id of this connection (monotonic).
    pub connection_id: u64,
    /// When the handshake completed (the server caps a connection's lifetime).
    pub opened_at: Instant,
}

#[derive(Default)]
pub struct SessionState {
    pub connections: HashMap<String, CachedConnection>,
    pub stats: HashMap<String, WebSocketDebugStats>,
    pub sse_fallback_sessions: HashSet<String>,
    /// Consecutive mid-stream transport failures per session (reset by a completed request).
    pub failure_streaks: HashMap<String, u32>,
}

/// One provider's WebSocket sessions. Each provider owns its own instance, so a session that
/// switches providers never reuses a socket, a continuation, or a fallback pin across endpoints.
pub(crate) struct WebSocketSessions {
    state: OnceLock<Mutex<SessionState>>,
    profile: WireProfile,
    /// Connections older than this are closed before reuse instead of serving a request the
    /// server would cut at its lifetime limit; `None` keeps them until idle expiry.
    max_connection_age: Option<Duration>,
}

impl WebSocketSessions {
    pub(crate) const fn new(profile: WireProfile, max_connection_age: Option<Duration>) -> Self {
        Self {
            state: OnceLock::new(),
            profile,
            max_connection_age,
        }
    }

    pub(crate) fn state(&self) -> &Mutex<SessionState> {
        self.state
            .get_or_init(|| Mutex::new(SessionState::default()))
    }

    pub(crate) fn is_sse_fallback_active(&self, session_id: Option<&str>) -> bool {
        match session_id {
            Some(session_id) => self
                .state()
                .lock()
                .is_ok_and(|state| state.sse_fallback_sessions.contains(session_id)),
            None => false,
        }
    }

    pub(crate) fn record_sse_fallback(&self, session_id: Option<&str>) {
        let Some(session_id) = session_id else {
            return;
        };
        super::note_counts(session_id, |counts| counts.sse_fallbacks += 1);
        let Ok(mut state) = self.state().lock() else {
            return;
        };
        let active = state.sse_fallback_sessions.contains(session_id);
        let session_stats = state.stats.entry(session_id.to_string()).or_default();
        session_stats.sse_fallbacks += 1;
        session_stats.websocket_fallback_active = Some(active);
    }

    /// Pins the session to SSE fallback and records the failure in the debug stats.
    pub(crate) fn record_failure(&self, session_id: Option<&str>, error: &ResponsesStreamError) {
        let Some(session_id) = session_id else {
            return;
        };
        let Ok(mut state) = self.state().lock() else {
            return;
        };
        state.sse_fallback_sessions.insert(session_id.to_string());
        let session_stats = state.stats.entry(session_id.to_string()).or_default();
        session_stats.websocket_failures += 1;
        session_stats.last_websocket_error = Some(error.to_string());
        session_stats.websocket_fallback_active = Some(true);
    }

    /// Count one mid-stream transport failure; the session is pinned to SSE once `limit`
    /// failures arrive in a row. Returns whether the session is now pinned.
    pub(crate) fn record_mid_stream_failure(
        &self,
        session_id: Option<&str>,
        error: &ResponsesStreamError,
        limit: u32,
    ) -> bool {
        let Some(session_id) = session_id else {
            return false;
        };
        let streak = {
            let Ok(mut state) = self.state().lock() else {
                return false;
            };
            let streak = state
                .failure_streaks
                .entry(session_id.to_string())
                .or_default();
            *streak += 1;
            let streak = *streak;
            let session_stats = state.stats.entry(session_id.to_string()).or_default();
            session_stats.websocket_failures += 1;
            session_stats.last_websocket_error = Some(error.to_string());
            streak
        };
        if streak < limit {
            return false;
        }
        self.record_failure(Some(session_id), error);
        true
    }

    /// A completed request ends the session's mid-stream failure streak.
    pub(crate) fn record_success(&self, session_id: Option<&str>) {
        if let (Some(session_id), Ok(mut state)) = (session_id, self.state().lock()) {
            state.failure_streaks.remove(session_id);
        }
    }

    pub(crate) fn debug_stats(&self, session_id: &str) -> Option<WebSocketDebugStats> {
        self.state().lock().ok()?.stats.get(session_id).cloned()
    }

    pub(crate) fn reset_debug_stats(&self, session_id: Option<&str>) {
        let Ok(mut state) = self.state().lock() else {
            return;
        };
        if let Some(session_id) = session_id {
            state.stats.remove(session_id);
            state.sse_fallback_sessions.remove(session_id);
            state.failure_streaks.remove(session_id);
        } else {
            state.stats.clear();
            state.sse_fallback_sessions.clear();
            state.failure_streaks.clear();
        }
    }

    /// Close the cached connection for one session or all of them.
    pub(crate) fn close(&self, session_id: Option<&str>) {
        let Ok(mut state) = self.state().lock() else {
            return;
        };
        let entries: Vec<mpsc::Sender<WorkerCommand>> = match session_id {
            Some(session_id) => state
                .connections
                .remove(session_id)
                .map(|entry| vec![entry.worker])
                .unwrap_or_default(),
            None => state
                .connections
                .drain()
                .map(|(_, entry)| entry.worker)
                .collect(),
        };
        for worker in entries {
            let _ = worker.try_send(WorkerCommand::Close);
        }
    }

    /// Record debug stats for a request (mirrors the stats block in `processWebSocketStream`).
    pub(crate) fn record_request_stats(
        &self,
        session_id: &str,
        reused: bool,
        use_cached_context: bool,
        request_body: &Value,
    ) {
        let delta = request_body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_some();
        super::note_counts(session_id, |counts| {
            counts.websocket_requests += 1;
            if delta {
                counts.delta_requests += 1;
            } else {
                counts.full_context_requests += 1;
            }
        });
        let Ok(mut state) = self.state().lock() else {
            return;
        };
        let session_stats = state.stats.entry(session_id.to_string()).or_default();
        session_stats.requests += 1;
        if reused {
            session_stats.connections_reused += 1;
        } else {
            session_stats.connections_created += 1;
        }
        if use_cached_context {
            session_stats.cached_context_requests += 1;
        }
        if request_body.get("store").and_then(Value::as_bool) == Some(true) {
            session_stats.store_true_requests += 1;
        }
        let input_items = request_body
            .get("input")
            .and_then(Value::as_array)
            .map_or(0, |items| items.len() as u64);
        session_stats.last_input_items = input_items;
        if let Some(previous_response_id) = request_body
            .get("previous_response_id")
            .and_then(Value::as_str)
        {
            session_stats.delta_requests += 1;
            session_stats.last_delta_input_items = Some(input_items);
            session_stats.last_previous_response_id = Some(previous_response_id.to_string());
        } else {
            session_stats.full_context_requests += 1;
            session_stats.last_delta_input_items = None;
            session_stats.last_previous_response_id = None;
        }
    }

    /// Close the cached connection after the TTL if it stayed idle.
    pub(crate) fn schedule_expiry(&'static self, session_id: &str) {
        let Ok(mut state) = self.state().lock() else {
            return;
        };
        let generation = match state.connections.get_mut(session_id) {
            Some(entry) => {
                entry.expiry_generation += 1;
                entry.expiry_generation
            }
            None => return,
        };
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS)).await;
            let Ok(mut state) = self.state().lock() else {
                return;
            };
            let Some(entry) = state.connections.get_mut(&session_id) else {
                return;
            };
            if entry.expiry_generation != generation || entry.busy {
                return;
            }
            if let Some(entry) = state.connections.remove(&session_id) {
                let _ = entry.worker.try_send(WorkerCommand::Close);
            }
        });
    }

    /// Take the continuation for `connection_id`, if one is anchored to it (part of
    /// `buildCachedWebSocketRequestBody` in the TS).
    pub(crate) fn take_continuation_for(
        &self,
        session_id: &str,
        connection_id: u64,
    ) -> Option<ContinuationState> {
        self.state()
            .lock()
            .ok()?
            .connections
            .get_mut(session_id)
            .filter(|entry| entry.connection_id == connection_id)
            .and_then(|entry| entry.continuation.take())
    }

    /// Restore (or clear) the continuation after a failed continuation attempt.
    pub(crate) fn clear_continuation(&self, session_id: &str, connection_id: u64) {
        let Ok(mut state) = self.state().lock() else {
            return;
        };
        if let Some(entry) = state.connections.get_mut(session_id) {
            if entry.connection_id == connection_id {
                entry.continuation = None;
            }
        }
    }

    /// Reuse the session's idle connection when possible, otherwise open a fresh connection (cached
    /// per session). A cached connection past the maximum age is closed and replaced.
    pub(crate) async fn acquire(
        &self,
        url: &str,
        headers: &[(String, String)],
        session_id: Option<&str>,
        signal: Option<CancellationToken>,
        connect_timeout: Option<Duration>,
    ) -> Result<AcquiredConnection, ResponsesStreamError> {
        let Some(session_id) = session_id else {
            // No session: uncached one-shot connection, closed after the request.
            let (worker, connection_id) =
                spawn_connection_worker(url, headers, signal, self.profile, connect_timeout)
                    .await?;
            return Ok(AcquiredConnection {
                worker,
                session_id: None,
                reused: false,
                cached: false,
                connection_id,
            });
        };

        let cached_worker = {
            let mut state = self.state().lock().ok();
            if let (Some(state), Some(max_age)) = (state.as_mut(), self.max_connection_age) {
                let aged = state
                    .connections
                    .get(session_id)
                    .is_some_and(|entry| !entry.busy && entry.opened_at.elapsed() >= max_age);
                if aged {
                    if let Some(entry) = state.connections.remove(session_id) {
                        let _ = entry.worker.try_send(WorkerCommand::Close);
                    }
                }
            }
            match state
                .as_mut()
                .and_then(|state| state.connections.get_mut(session_id))
            {
                Some(entry) if !entry.busy => {
                    entry.busy = true;
                    entry.expiry_generation += 1;
                    Some((entry.worker.clone(), entry.connection_id))
                }
                _ => None,
            }
        };
        if let Some((worker, connection_id)) = cached_worker {
            return Ok(AcquiredConnection {
                worker,
                session_id: Some(session_id.to_string()),
                reused: true,
                cached: true,
                connection_id,
            });
        }

        // Fresh connection: cache it for the session unless an entry is busy.
        let entry_busy = self.state().lock().is_ok_and(|state| {
            state
                .connections
                .get(session_id)
                .is_some_and(|entry| entry.busy)
        });
        let (worker, connection_id) =
            spawn_connection_worker(url, headers, signal, self.profile, connect_timeout).await?;

        let cached = if entry_busy {
            false
        } else {
            let Ok(mut state) = self.state().lock() else {
                return Ok(AcquiredConnection {
                    worker,
                    session_id: Some(session_id.to_string()),
                    reused: false,
                    cached: false,
                    connection_id,
                });
            };
            state.connections.insert(
                session_id.to_string(),
                CachedConnection {
                    worker: worker.clone(),
                    busy: true,
                    continuation: None,
                    expiry_generation: 0,
                    connection_id,
                    opened_at: Instant::now(),
                },
            );
            true
        };

        Ok(AcquiredConnection {
            worker,
            session_id: Some(session_id.to_string()),
            reused: false,
            cached,
            connection_id,
        })
    }

    pub(crate) async fn release(
        &'static self,
        connection: AcquiredConnection,
        keep: bool,
        continuation: Option<ContinuationState>,
    ) {
        let session_id = connection.session_id.clone();
        let Some(session_id) = session_id else {
            // Uncached connections always close after the request.
            connection.close().await;
            return;
        };
        if !keep {
            self.close(Some(&session_id));
            return;
        }
        let matched = match self.state().lock() {
            Ok(mut state) => match state.connections.get_mut(&session_id) {
                Some(entry) if entry.connection_id == connection.connection_id => {
                    entry.busy = false;
                    entry.continuation = continuation;
                    true
                }
                _ => false,
            },
            Err(_) => false,
        };
        if matched {
            self.schedule_expiry(&session_id);
        } else {
            connection.close().await;
        }
    }
}
