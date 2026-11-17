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

use std::collections::HashSet;
use std::sync::Mutex;

use pa_types::ai::KnownApi;

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
        let offline = std::env::var("PI_OFFLINE").is_ok_and(|value| {
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

/// The origins this process already prewarmed (or is prewarming).
static PREWARMED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// The prewarm request's own bound: a slow host must not hold a pool slot.
const PREWARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
    // Only the APIs that stream through the shared HTTP/1.1 pool share its
    // connection with the prewarm.
    match serde_json::from_value::<KnownApi>(serde_json::Value::String(api)) {
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
        Ok(KnownApi::BedrockConverseStream) | Err(_) => return PrewarmOutcome::Unsupported,
    }
    // Only a remote host has a handshake worth saving: a loopback server (a
    // local model, a test double) never sees the warm-up request.
    let Some(origin) = reqwest::Url::parse(&base_url)
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
    else {
        return PrewarmOutcome::Unsupported;
    };
    let first = PREWARMED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(HashSet::new)
        .insert(origin.clone());
    if !first {
        return PrewarmOutcome::AlreadyWarmed;
    }
    let sent = super::http::client()
        .head(&base_url)
        .timeout(PREWARM_TIMEOUT)
        .send()
        .await;
    let result = match sent {
        // Draining the (empty) body hands the connection back to the pool.
        Ok(response) => response.bytes().await.map(|_| ()),
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => PrewarmOutcome::Warmed,
        Err(error) => {
            tracing::debug!(%origin, error = %error, "provider connection prewarm failed");
            PrewarmOutcome::Failed(error.to_string())
        }
    }
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

    /// A keep-alive HTTP/1.1 host counting the connections it accepted and
    /// the requests it answered.
    async fn counting_host() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind((interface_address(), 0))
            .await
            .unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let (accepted, answered) = (Arc::clone(&connections), Arc::clone(&requests));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                let answered = Arc::clone(&answered);
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
                            answered.fetch_add(1, Ordering::SeqCst);
                            let reply: &[u8] = if head {
                                b"HTTP/1.1 404 Not Found\r\ncontent-length: 2\r\n\r\n"
                            } else {
                                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
                            };
                            if socket.write_all(reply).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (base_url, connections, requests)
    }

    #[tokio::test]
    async fn the_first_request_reuses_the_prewarmed_connection() {
        let (base_url, connections, requests) = counting_host().await;
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
        let (base_url, connections, _) = counting_host().await;
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
}
