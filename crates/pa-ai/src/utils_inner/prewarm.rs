//! Provider connection prewarm: open the shared HTTP/1.1 pool's connection
//! to a provider's API host before the first request needs it, so the first
//! turn skips the TCP (and TLS) handshake (omp's
//! `ai/src/providers/connection-prewarm.ts`).
//!
//! One `HEAD` request to the provider's base URL through the same pooled
//! client every provider streams through: the response status is irrelevant
//! (any answer proves the connection is up), and the keep-alive connection
//! stays in the pool for the first real request. At most one prewarm per
//! origin per process; never in offline mode.
//!
//! The pool drops a connection idle for 90 s, and a provider may close one
//! sooner, so a turn after a pause would pay the handshake again. Every
//! paced provider request records its origin's activity here; when a turn
//! starts, [`rewarm_idle_origins`] re-opens each origin that sat idle past
//! its learned threshold ([`IdleThreshold`]), in the background.

mod idle;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use pa_types::ai::KnownApi;

use idle::{ConnectionFound, IdleThreshold, RewarmDecision};

/// Whether the process may touch the network for a prewarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    Online,
    /// `PI_OFFLINE` (`--offline`): no prewarm connection is ever opened.
    Offline,
}

impl NetworkMode {
    /// The process's mode from `PI_OFFLINE` (`1`, `true` or `yes`, as the
    /// package manager reads it).
    #[must_use]
    pub fn from_env() -> Self {
        let offline = pa_types::session_scope::var("PI_OFFLINE").is_ok_and(|value| {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        });
        if offline {
            Self::Offline
        } else {
            Self::Online
        }
    }
}

/// What [`prewarm_connection`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrewarmOutcome {
    /// A connection to the origin is open in the shared pool.
    Warmed,
    /// Offline mode: nothing was opened.
    Offline,
    /// Not prewarmed: the API's transport is chosen per request (bedrock's
    /// HTTP/2 paths) or is not a pooled HTTP provider (faux, custom APIs), or
    /// the base URL is not an `http(s)` URL with a remote (non-loopback) host.
    Unsupported,
    /// This process already prewarmed the origin.
    AlreadyWarmed,
    /// The request failed (logged as a debug event; the first real request
    /// connects as it always did).
    Failed(String),
}

/// What this process knows about one provider origin.
#[derive(Debug, Clone)]
struct OriginState {
    /// A base URL on the origin, the target of the warm-up `HEAD`.
    base_url: String,
    /// When a request or warm-up last used the origin's connection.
    last_active: Instant,
    /// Provider requests in flight: an origin in use is never idle.
    in_flight: u32,
    /// A warm-up `HEAD` is in flight.
    warming: bool,
    /// Whether the first prewarm already ran (or a request already used
    /// the origin, which opened its connection).
    prewarmed: bool,
    threshold: IdleThreshold,
}

/// The origins this process prewarmed or sent provider requests to.
static ORIGINS: Mutex<Option<HashMap<String, OriginState>>> = Mutex::new(None);

/// The prewarm request's own bound: a slow host must not hold a pool slot.
const PREWARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn with_origins<T>(apply: impl FnOnce(&mut HashMap<String, OriginState>) -> T) -> T {
    apply(
        ORIGINS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(HashMap::new),
    )
}

/// The pooled origin a request over `api` to `base_url` connects to, when
/// warming it can save a handshake: only the APIs that stream through the
/// shared HTTP/1.1 pool, and only a remote host (a loopback server — a
/// local model, a test double — never sees a warm-up request).
fn pooled_origin(api: &str, base_url: &str) -> Option<String> {
    match serde_json::from_value::<KnownApi>(serde_json::Value::String(api.to_string())) {
        Ok(
            KnownApi::OpenaiCompletions
            | KnownApi::MistralConversations
            | KnownApi::OpenaiResponses
            | KnownApi::AzureOpenaiResponses
            | KnownApi::OpenaiCodexResponses
            | KnownApi::AnthropicMessages
            | KnownApi::GoogleGenerativeAi
            | KnownApi::GoogleVertex,
        ) => {}
        Ok(KnownApi::BedrockConverseStream) | Err(_) => return None,
    }
    reqwest::Url::parse(base_url)
        .ok()
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host().is_some_and(|host| match host {
                    url::Host::Domain(domain) => !domain.eq_ignore_ascii_case("localhost"),
                    url::Host::Ipv4(address) => !address.is_loopback(),
                    url::Host::Ipv6(address) => !address.is_loopback(),
                })
        })
        .map(|url| url.origin().ascii_serialization())
}

/// Send the warm-up `HEAD` and time it. Draining the (empty) body hands the
/// connection back to the pool.
async fn warm(base_url: &str) -> Result<u64, reqwest::Error> {
    let started = Instant::now();
    let response = super::http::client()
        .head(base_url)
        .timeout(PREWARM_TIMEOUT)
        .send()
        .await?;
    response.bytes().await?;
    Ok(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
}

/// Open and keep alive a pooled connection to `base_url`'s origin, for a
/// model served over `api` (the model's wire API id).
pub async fn prewarm_connection(
    api: String,
    base_url: String,
    mode: NetworkMode,
) -> PrewarmOutcome {
    match mode {
        NetworkMode::Offline => return PrewarmOutcome::Offline,
        NetworkMode::Online => {}
    }
    let Some(origin) = pooled_origin(&api, &base_url) else {
        return PrewarmOutcome::Unsupported;
    };
    let first = with_origins(|origins| {
        let state = origins
            .entry(origin.clone())
            .or_insert_with(|| OriginState {
                base_url: base_url.clone(),
                last_active: Instant::now(),
                in_flight: 0,
                warming: false,
                prewarmed: false,
                threshold: IdleThreshold::new(),
            });
        let first = !state.prewarmed;
        state.prewarmed = true;
        state.warming |= first;
        first
    });
    if !first {
        return PrewarmOutcome::AlreadyWarmed;
    }
    let result = warm(&base_url).await;
    with_origins(|origins| {
        if let Some(state) = origins.get_mut(&origin) {
            state.warming = false;
            state.last_active = Instant::now();
            if let Ok(latency_ms) = result {
                // The first prewarm always opens a fresh connection: its time is the
                // reconnect baseline later warm-ups are judged against.
                state.threshold.note_reopened(latency_ms);
            }
        }
    });
    match result {
        Ok(_) => PrewarmOutcome::Warmed,
        Err(error) => {
            tracing::debug!(%origin, error = %error, "provider connection prewarm failed");
            PrewarmOutcome::Failed(error.to_string())
        }
    }
}

/// One paced provider request's use of its origin's pooled connection:
/// the origin is busy while the guard lives, and its idle clock restarts
/// when the guard drops.
#[derive(Debug)]
pub(crate) struct OriginActivity {
    origin: Option<String>,
}

impl OriginActivity {
    /// Mark a request over `api` to `base_url` as started.
    pub(crate) fn start(api: &str, base_url: &str) -> Self {
        let origin = pooled_origin(api, base_url);
        if let Some(origin) = &origin {
            with_origins(|origins| {
                let state = origins
                    .entry(origin.clone())
                    .or_insert_with(|| OriginState {
                        base_url: base_url.to_string(),
                        last_active: Instant::now(),
                        in_flight: 0,
                        warming: false,
                        // The request opens the origin's connection itself.
                        prewarmed: true,
                        threshold: IdleThreshold::new(),
                    });
                state.in_flight += 1;
            });
        }
        Self { origin }
    }
}

impl Drop for OriginActivity {
    fn drop(&mut self) {
        if let Some(origin) = &self.origin {
            with_origins(|origins| {
                if let Some(state) = origins.get_mut(origin) {
                    state.in_flight = state.in_flight.saturating_sub(1);
                    state.last_active = Instant::now();
                }
            });
        }
    }
}

/// Counts for the adoption telemetry: primitives only, never an origin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RewarmStats {
    /// Warm-ups sent because an origin sat idle at a turn start.
    pub rewarm_count: u64,
    /// Warm-ups that found the connection closed and reconnected: each is
    /// a handshake the turn's first request no longer pays.
    pub reopened_count: u64,
    /// The summed time of those reconnecting warm-ups (the handshake cost
    /// moved off the turn).
    pub reopened_ms: u64,
}

static STATS: Mutex<RewarmStats> = Mutex::new(RewarmStats {
    rewarm_count: 0,
    reopened_count: 0,
    reopened_ms: 0,
});

/// Take (and reset) the counts since the last take.
#[must_use]
pub fn take_rewarm_stats() -> RewarmStats {
    std::mem::take(
        &mut *STATS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// A turn is starting: re-open, in the background, the pooled connection of
/// every origin this process used that sat idle past its learned threshold
/// (or inside the probe band below it, where the threshold is learned).
/// Returns at once with the number of warm-ups started; it never waits on
/// the network, so the turn start never does either. Needs a tokio runtime
/// (none: nothing is started).
#[must_use]
pub fn rewarm_idle_origins(mode: NetworkMode) -> usize {
    match mode {
        NetworkMode::Offline => return 0,
        NetworkMode::Online => {}
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return 0;
    };
    let now = Instant::now();
    let due: Vec<(String, String, u64)> = with_origins(|origins| {
        origins
            .iter_mut()
            .filter(|(_, state)| state.in_flight == 0 && !state.warming)
            .filter_map(|(origin, state)| {
                let idle_ms =
                    u64::try_from(now.saturating_duration_since(state.last_active).as_millis())
                        .unwrap_or(u64::MAX);
                match state.threshold.decide(idle_ms) {
                    RewarmDecision::Fresh => None,
                    RewarmDecision::Probe | RewarmDecision::Rewarm => {
                        state.warming = true;
                        Some((origin.clone(), state.base_url.clone(), idle_ms))
                    }
                }
            })
            .collect()
    });
    let started = due.len();
    for (origin, base_url, idle_ms) in due {
        runtime.spawn(async move {
            let result = warm(&base_url).await;
            let found = with_origins(|origins| {
                let state = origins.get_mut(&origin)?;
                state.warming = false;
                state.last_active = Instant::now();
                let latency_ms = result.as_ref().ok().copied()?;
                Some((state.threshold.learn(idle_ms, latency_ms), latency_ms))
            });
            let mut stats = STATS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            stats.rewarm_count += 1;
            match found {
                Some((ConnectionFound::Reopened, latency_ms)) => {
                    stats.reopened_count += 1;
                    stats.reopened_ms = stats.reopened_ms.saturating_add(latency_ms);
                }
                Some((ConnectionFound::Open | ConnectionFound::Unknown, _)) | None => {}
            }
            drop(stats);
            if let Err(error) = result {
                tracing::debug!(%origin, error = %error, "provider connection re-warm failed");
            }
        });
    }
    started
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// This machine's outbound interface address (a remote host to the
    /// prewarm: loopback is never warmed). A UDP `connect` only picks the
    /// route; nothing is sent.
    fn interface_address() -> std::net::IpAddr {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        probe.connect("198.51.100.1:9").unwrap();
        let address = probe.local_addr().unwrap().ip();
        assert!(
            !address.is_loopback(),
            "a non-loopback interface: {address}"
        );
        address
    }

    /// What the test host does with a connection after answering.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum HostKeepAlive {
        /// Keep it open for the next request.
        Keep,
        /// Close it, as a provider closing idle connections does.
        CloseAfterReply,
        /// Never answer at all (a hanging provider).
        Hang,
    }

    /// An HTTP/1.1 host counting the connections it accepted and the
    /// requests it answered; `answered` wakes on every answer.
    async fn counting_host(
        keep_alive: HostKeepAlive,
    ) -> (
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<tokio::sync::Notify>,
    ) {
        let listener = tokio::net::TcpListener::bind((interface_address(), 0))
            .await
            .unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(tokio::sync::Notify::new());
        let (accepted, answered, woken) = (
            Arc::clone(&connections),
            Arc::clone(&requests),
            Arc::clone(&notify),
        );
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                let (answered, woken) = (Arc::clone(&answered), Arc::clone(&woken));
                if keep_alive == HostKeepAlive::Hang {
                    woken.notify_one();
                    tokio::spawn(async move {
                        let mut sink = Vec::new();
                        let _ = socket.read_to_end(&mut sink).await;
                    });
                    continue;
                }
                tokio::spawn(async move {
                    let mut pending = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while let Ok(read) = socket.read(&mut chunk).await {
                        if read == 0 {
                            return;
                        }
                        pending.extend_from_slice(&chunk[..read]);
                        while let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = pending.starts_with(b"HEAD ");
                            pending.drain(..end + 4);
                            let reply: &[u8] = if head {
                                b"HTTP/1.1 404 Not Found\r\ncontent-length: 2\r\n\r\n"
                            } else {
                                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
                            };
                            if socket.write_all(reply).await.is_err() {
                                return;
                            }
                            answered.fetch_add(1, Ordering::SeqCst);
                            woken.notify_one();
                            if keep_alive == HostKeepAlive::CloseAfterReply {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (base_url, connections, requests, notify)
    }

    /// Wait until `counter` reaches `count`, woken by the host (bounded, so
    /// a warm-up that never happens fails the test instead of hanging it).
    async fn reach(counter: &AtomicUsize, notify: &tokio::sync::Notify, count: usize) {
        let reached = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let woken = notify.notified();
                if counter.load(Ordering::SeqCst) >= count {
                    return;
                }
                woken.await;
            }
        })
        .await;
        assert_eq!(
            (reached.is_ok(), counter.load(Ordering::SeqCst)),
            (true, count),
            "the host never reached {count}"
        );
    }

    /// Age `base_url`'s origin as if its last use was `idle` ago.
    fn age_origin(base_url: &str, idle: std::time::Duration) {
        let origin = pooled_origin("openai-completions", base_url).unwrap();
        with_origins(|origins| {
            let state = origins.get_mut(&origin).unwrap();
            state.last_active = Instant::now().checked_sub(idle).unwrap();
        });
    }

    /// The tests that age origins and re-warm every idle one run one at a
    /// time, so one never re-warms another's origin.
    static REWARM_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn the_first_request_reuses_the_prewarmed_connection() {
        let (base_url, connections, requests, _) = counting_host(HostKeepAlive::Keep).await;
        assert_eq!(
            prewarm_connection(
                "openai-completions".to_string(),
                base_url.clone(),
                NetworkMode::Online
            )
            .await,
            PrewarmOutcome::Warmed
        );
        // The connection exists before the first prompt.
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        // A second prewarm of the same origin is rate-limited away.
        assert_eq!(
            prewarm_connection(
                "openai-completions".to_string(),
                format!("{base_url}/chat"),
                NetworkMode::Online
            )
            .await,
            PrewarmOutcome::AlreadyWarmed
        );
        // The first real request rides the pooled connection.
        let response = super::super::http::send(super::super::http::RequestOptions::new(
            reqwest::Method::POST,
            format!("{base_url}/chat/completions"),
        ))
        .await
        .unwrap();
        assert_eq!(response.status, 200);
        drop(response);
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_loopback_provider_is_never_warmed() {
        assert_eq!(
            prewarm_connection(
                "openai-completions".to_string(),
                "http://127.0.0.1:9/v1".to_string(),
                NetworkMode::Online
            )
            .await,
            PrewarmOutcome::Unsupported
        );
    }

    #[tokio::test]
    async fn offline_mode_opens_no_connection() {
        let (base_url, connections, _, _) = counting_host(HostKeepAlive::Keep).await;
        assert_eq!(
            prewarm_connection(
                "openai-completions".to_string(),
                base_url.clone(),
                NetworkMode::Offline
            )
            .await,
            PrewarmOutcome::Offline
        );
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_turn_after_an_idle_gap_reconnects_before_the_first_request() {
        let _serial = REWARM_SERIAL.lock().await;
        let (base_url, connections, requests, notify) =
            counting_host(HostKeepAlive::CloseAfterReply).await;
        assert_eq!(
            prewarm_connection(
                "openai-completions".to_string(),
                base_url.clone(),
                NetworkMode::Online
            )
            .await,
            PrewarmOutcome::Warmed
        );
        // The provider closed the warm connection; the turn starts 85 s on.
        age_origin(&base_url, std::time::Duration::from_secs(85));
        let started = rewarm_idle_origins(NetworkMode::Online);
        reach(&requests, &notify, 2).await;
        // The reconnect happened at the turn start, ahead of the request.
        assert_eq!((started, connections.load(Ordering::SeqCst)), (1, 2));
    }

    #[tokio::test]
    async fn a_hanging_provider_never_holds_the_turn_start() {
        let _serial = REWARM_SERIAL.lock().await;
        let (base_url, connections, _, notify) = counting_host(HostKeepAlive::Hang).await;
        // A request used the origin (no prewarm), then it sat idle.
        drop(OriginActivity::start("openai-completions", &base_url));
        age_origin(&base_url, std::time::Duration::from_secs(85));
        let clock = Instant::now();
        let first = rewarm_idle_origins(NetworkMode::Online);
        let returned_in = clock.elapsed();
        // The warm-up is in flight against a host that never answers...
        reach(&connections, &notify, 1).await;
        // ...and a second turn start neither waits for it nor doubles it.
        let second = rewarm_idle_origins(NetworkMode::Online);
        assert_eq!((first, second), (1, 0));
        assert!(
            returned_in < std::time::Duration::from_millis(50),
            "the turn start returned in {returned_in:?}"
        );
    }

    #[tokio::test]
    async fn an_origin_in_use_or_offline_is_never_rewarmed() {
        let _serial = REWARM_SERIAL.lock().await;
        let (base_url, connections, _, _) = counting_host(HostKeepAlive::Keep).await;
        let in_flight = OriginActivity::start("openai-completions", &base_url);
        age_origin(&base_url, std::time::Duration::from_secs(85));
        let busy = rewarm_idle_origins(NetworkMode::Online);
        drop(in_flight);
        age_origin(&base_url, std::time::Duration::from_secs(85));
        let offline = rewarm_idle_origins(NetworkMode::Offline);
        assert_eq!(
            (busy, offline, connections.load(Ordering::SeqCst)),
            (0, 0, 0)
        );
    }
}
