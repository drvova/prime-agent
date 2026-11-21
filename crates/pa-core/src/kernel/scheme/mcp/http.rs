//! A streamable-HTTP MCP server: every message is one POST to the server's
//! endpoint, answered with a JSON body or with a server-sent event stream
//! that carries the reply.
//!
//! The configured headers and the bearer token ride on every request and
//! redirects are never followed, so a redirecting endpoint is not sent them.
//! The session id the server assigns at `initialize` is sent back on every
//! later request and released with a DELETE when the connection closes. The
//! standalone GET stream for server-initiated messages is not opened: this
//! client subscribes to nothing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Map, Value};

use super::{reply_outcome, server_request_answer, Failure, FailureKind};

/// How long a connection to the server may take to be made.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a courtesy message (a cancellation notice, the session's release)
/// may take.
const COURTESY_TIMEOUT: Duration = Duration::from_secs(2);

const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// An HTTP MCP endpoint and the session it assigned.
pub(super) struct HttpTransport {
    server: String,
    client: reqwest::Client,
    url: String,
    /// The configured headers and the bearer token.
    headers: HeaderMap,
    session: Mutex<Option<HeaderValue>>,
    /// The protocol revision the server settled on at `initialize`.
    protocol: Mutex<Option<HeaderValue>>,
    next_id: AtomicU64,
    /// Set once the server has forgotten the session.
    closed: AtomicBool,
}

/// A request in flight. Dropped before its exchange ended (its cell was
/// stopped, or it timed out), it tells the server the request was cancelled.
struct InFlight<'a> {
    transport: &'a HttpTransport,
    id: u64,
    /// Cleared when the exchange ended; the protocol also forbids
    /// cancelling `initialize`.
    cancellable: bool,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.cancellable {
            return;
        }
        // Outside a runtime (the process is ending) there is no one to tell.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let notice = self
                .transport
                .post(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": self.id, "reason": "the caller stopped waiting"},
                }))
                .timeout(COURTESY_TIMEOUT);
            runtime.spawn(async move {
                // A server that cannot be told carries on; nothing to report.
                let _ = notice.send().await;
            });
        }
    }
}

impl HttpTransport {
    /// The transport for the endpoint `config` describes; nothing is sent yet.
    ///
    /// # Errors
    ///
    /// The configuration has no URL or malformed headers, or the HTTP client
    /// could not be built.
    pub(super) fn start(
        server: &str,
        config: &Map<String, Value>,
        bearer: Option<&str>,
    ) -> Result<Self, Failure> {
        let invalid = |reason: String| Failure::new(FailureKind::Invalid, reason);
        let url = config
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| invalid(format!("MCP server '{server}' requires a URL")))?;
        let mut headers = HeaderMap::new();
        let configured = match config.get("headers") {
            None | Some(Value::Null) => None,
            Some(configured) => Some(configured.as_object().ok_or_else(|| {
                invalid(format!("MCP server '{server}' headers must be an object"))
            })?),
        };
        // The bearer goes in last: a configured Authorization never wins.
        let bearer = bearer.map(|token| ("authorization".to_string(), format!("Bearer {token}")));
        for (name, value) in configured
            .into_iter()
            .flatten()
            .map(|(name, value)| (name.clone(), value.as_str().map(str::to_string)))
            .chain(bearer.map(|(name, value)| (name, Some(value))))
        {
            let parsed = value.and_then(|value| {
                Some((
                    HeaderName::from_bytes(name.as_bytes()).ok()?,
                    HeaderValue::from_str(&value).ok()?,
                ))
            });
            let Some((name, mut value)) = parsed else {
                return Err(invalid(format!(
                    "MCP server '{server}' has a malformed HTTP header '{name}'"
                )));
            };
            value.set_sensitive(name == AUTHORIZATION);
            headers.insert(name, value);
        }
        let client = pa_ai::utils::tls::http_client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| {
                Failure::new(
                    FailureKind::Startup,
                    format!(
                        "the HTTP client for MCP server '{server}' could not be built: {error}"
                    ),
                )
            })?;
        Ok(HttpTransport {
            server: server.to_string(),
            client,
            url: url.to_string(),
            headers,
            session: Mutex::new(None),
            protocol: Mutex::new(None),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        })
    }

    /// Whether the server has forgotten this connection's session.
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// The server settled on protocol revision `version`: later requests
    /// name it.
    pub(super) fn negotiated(&self, version: &str) {
        *self.protocol.lock().unwrap_or_else(PoisonError::into_inner) =
            HeaderValue::from_str(version).ok();
    }

    /// One POST of `message` with this connection's headers.
    fn post(&self, message: &Value) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .json(message);
        for (name, held) in [
            (SESSION_HEADER, &self.session),
            (PROTOCOL_HEADER, &self.protocol),
        ] {
            if let Some(value) = held.lock().unwrap_or_else(PoisonError::into_inner).clone() {
                request = request.header(name, value);
            }
        }
        request
    }

    /// A failure to reach the server, without the URL it was reached at (a
    /// URL can carry a credential).
    fn unreachable(&self, error: reqwest::Error) -> Failure {
        Failure::new(
            FailureKind::Protocol,
            format!(
                "MCP server '{}' could not be reached: {}",
                self.server,
                error.without_url()
            ),
        )
    }

    /// Send one request and wait up to `limit` for its reply.
    ///
    /// # Errors
    ///
    /// The server refused the credentials, answered with an error or an
    /// unusable response, did not answer in time, or could not be reached.
    pub(super) async fn request(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, Failure> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut in_flight = InFlight {
            transport: self,
            id,
            cancellable: method != "initialize",
        };
        let server = &self.server;
        let exchange = async {
            let mut response = self
                .post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .send()
                .await
                .map_err(|error| self.unreachable(error))?;
            let status = response.status();
            if matches!(status.as_u16(), 401 | 403) {
                return Err(Failure::new(
                    FailureKind::CredentialsUnavailable,
                    format!(
                        "MCP server '{server}' refused the credentials (HTTP {}). Ask the user \
                         to connect it (/plugins or /mcp login {server}).",
                        status.as_u16()
                    ),
                ));
            }
            let in_session = self
                .session
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some();
            if status.as_u16() == 404 && in_session {
                self.closed.store(true, Ordering::SeqCst);
                return Err(Failure::new(
                    FailureKind::Protocol,
                    format!("MCP server '{server}' ended the session; call again to reconnect"),
                ));
            }
            if !status.is_success() {
                return Err(Failure::new(
                    FailureKind::Protocol,
                    format!("MCP server '{server}' answered HTTP {}", status.as_u16()),
                ));
            }
            if let Some(session) = response.headers().get(SESSION_HEADER) {
                *self.session.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(session.clone());
            }
            // The reply to this request among the messages of a response; a
            // request the server makes on the way is answered.
            let settle = |messages: Vec<Value>| async move {
                let mut reply = None;
                for message in messages {
                    if let Some(answer) = server_request_answer(&message) {
                        // A server that cannot take the answer times its own
                        // request out; this call is not affected.
                        let _ = self.post(&answer).timeout(COURTESY_TIMEOUT).send().await;
                    } else if message.get("id").and_then(Value::as_u64) == Some(id) {
                        reply = Some(reply_outcome(message));
                    }
                }
                reply
            };
            let streamed = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|kind| kind.to_str().ok())
                .is_some_and(|kind| kind.contains("text/event-stream"));
            if !streamed {
                let body = response
                    .bytes()
                    .await
                    .map_err(|error| self.unreachable(error))?;
                let messages = match serde_json::from_slice::<Value>(&body) {
                    Ok(Value::Array(batch)) => batch,
                    Ok(single) => vec![single],
                    Err(_) => Vec::new(),
                };
                return settle(messages).await.unwrap_or_else(|| {
                    Err(Failure::new(
                        FailureKind::Protocol,
                        format!("MCP server '{server}' sent no reply to {method}"),
                    ))
                });
            }
            // Server-sent events: `data:` lines up to a blank line are one
            // message.
            let (mut unread, mut data) = (Vec::<u8>::new(), String::new());
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| self.unreachable(error))?
            {
                unread.extend_from_slice(&chunk);
                while let Some(end) = unread.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = unread.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line);
                    let line = line.trim_end_matches(['\r', '\n']);
                    if let Some(rest) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                    } else if line.is_empty() {
                        // An event that is not JSON (a keep-alive) is skipped.
                        let event = serde_json::from_str::<Value>(&std::mem::take(&mut data));
                        if let Some(reply) = settle(event.into_iter().collect()).await {
                            return reply;
                        }
                    }
                }
            }
            Err(Failure::new(
                FailureKind::Protocol,
                format!("MCP server '{server}' ended its stream before replying to {method}"),
            ))
        };
        let Ok(outcome) = tokio::time::timeout(limit, exchange).await else {
            return Err(Failure::new(
                FailureKind::TimedOut,
                format!(
                    "MCP server '{server}' did not answer {method} within {} ms",
                    limit.as_millis()
                ),
            ));
        };
        in_flight.cancellable = false;
        outcome
    }

    /// Send one notification.
    ///
    /// # Errors
    ///
    /// The server could not be reached.
    pub(super) async fn notify(&self, method: &str, limit: Duration) -> Result<(), Failure> {
        self.post(&json!({"jsonrpc": "2.0", "method": method}))
            .timeout(limit)
            .send()
            .await
            .map(drop)
            .map_err(|error| self.unreachable(error))
    }

    /// Release the session the server assigned, if it assigned one.
    pub(super) async fn shutdown(self) {
        let session = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(session) = session {
            // A server that keeps the session expires it on its own.
            let _ = self
                .client
                .delete(&self.url)
                .headers(self.headers.clone())
                .header(SESSION_HEADER, session)
                .timeout(COURTESY_TIMEOUT)
                .send()
                .await;
        }
    }
}
