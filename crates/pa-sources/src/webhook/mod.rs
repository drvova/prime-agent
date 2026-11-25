//! The webhook source: an HTTP listener that publishes each authenticated
//! `POST` to its path on `source/<name>/received`, always with Untrusted
//! trust, since its body is whatever the sender wrote.
//!
//! A JSON body is published as `{ "body": <json>, "headers": { ... } }`, the
//! headers being only the safe, descriptive ones in [`REPORTED_HEADERS`]; any
//! other body as `{ "body": <text> }` (invalid UTF-8 replaced).
//!
//! **Listening.** On `127.0.0.1` (port 0, any free one, by default). A
//! non-loopback `bind` is refused unless `allow_remote` is set. At most
//! [`MAX_CONNECTIONS`] connections are served at once; a client gets
//! [`HEADER_TIMEOUT`] to send its headers and [`BODY_TIMEOUT`] for its body.
//!
//! **Auth.** A webhook needs a `token`, an `hmac_secret`, or both (a local
//! listener without one would take events from any web page the user's
//! browser opens); every one configured must pass:
//! - `token`: `Authorization: Bearer <token>`, compared in constant time;
//! - `hmac_secret`: `X-Signature-256: sha256=<hex>`, an HMAC-SHA256 of the
//!   body under the secret.
//!
//! **Answers.** 202 published; 401 bad or missing auth; 404 another path;
//! 405 another method; 408 a body too slow; 413 a body over
//! `max_body_bytes`; 400 a body that failed; 503 the runtime takes no more
//! events.

use std::convert::Infallible;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, CONTENT_LENGTH};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use pa_types::event_log::{Topic, Trust};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::task::JoinSet;

use crate::{
    source_event, source_topic, spawn, Publisher, PublisherClosed, Source, SourceHandle,
    StartContext,
};

/// The request headers a JSON body's event carries, when present: what
/// kind of delivery it is, never credentials or signatures.
pub const REPORTED_HEADERS: &[&str] = &[
    "content-type",
    "user-agent",
    "x-github-event",
    "x-github-delivery",
    "x-gitlab-event",
    "x-request-id",
];
/// Most connections a webhook serves at once; more wait to be accepted.
pub const MAX_CONNECTIONS: usize = 64;
/// How long a client has to send a request's headers.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a client has to send a request's body.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// A token or signing key: it deserializes from a string and never prints.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(..)")
    }
}

/// A webhook's options.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookOptions {
    /// The address to listen on, an IP and a port (0 for any free one).
    #[serde(default = "loopback_any_port")]
    pub bind: SocketAddr,
    /// The request path events are posted to.
    #[serde(default = "root_path")]
    pub path: String,
    #[serde(default)]
    pub token: Option<Secret>,
    #[serde(default)]
    pub hmac_secret: Option<Secret>,
    #[serde(default = "one_mebibyte")]
    pub max_body_bytes: usize,
    /// Whether `bind` may be other than a loopback address.
    #[serde(default)]
    pub allow_remote: bool,
}

fn loopback_any_port() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn root_path() -> String {
    "/".to_string()
}

fn one_mebibyte() -> usize {
    1024 * 1024
}

/// Starts webhook listeners.
#[derive(Debug, Clone, Copy, Default)]
pub struct WebhookSource;

/// What every connection of one webhook answers with.
struct Hook {
    name: String,
    topic: Topic,
    path: String,
    token: Option<Secret>,
    hmac_secret: Option<Secret>,
    max_body_bytes: usize,
    publisher: Arc<dyn Publisher>,
}

impl Source for WebhookSource {
    type Options = WebhookOptions;

    fn options_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "webhook",
            "description": "An HTTP listener publishing each authenticated POST on source/<name>/received, always Untrusted. Needs token, hmac_secret, or both.",
            "type": "object",
            "properties": {
                "bind": {
                    "type": "string",
                    "default": "127.0.0.1:0",
                    "description": "IP:port to listen on; port 0 is any free port. Non-loopback needs allow_remote."
                },
                "path": {
                    "type": "string",
                    "pattern": "^/",
                    "default": "/"
                },
                "token": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Required as Authorization: Bearer <token>."
                },
                "hmac_secret": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Required as X-Signature-256: sha256=<hex HMAC-SHA256 of the body>."
                },
                "max_body_bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 1_048_576
                },
                "allow_remote": {
                    "type": "boolean",
                    "default": false,
                    "description": "Allow a non-loopback bind."
                }
            },
            "anyOf": [
                { "required": ["token"] },
                { "required": ["hmac_secret"] }
            ],
            "additionalProperties": false
        })
    }

    fn start(
        &self,
        name: &str,
        options: WebhookOptions,
        context: StartContext,
    ) -> anyhow::Result<SourceHandle> {
        let topic = source_topic(name, format!("source/{name}/received"))?;
        if !options.bind.ip().is_loopback() && !options.allow_remote {
            anyhow::bail!(
                "webhook {name:?}: {} is not a loopback address; listening on it needs `allow_remote: true`",
                options.bind
            );
        }
        let token = options.token.filter(|token| !token.0.is_empty());
        let hmac_secret = options.hmac_secret.filter(|secret| !secret.0.is_empty());
        if token.is_none() && hmac_secret.is_none() {
            anyhow::bail!("webhook {name:?}: needs a `token`, an `hmac_secret`, or both");
        }
        if !options.path.starts_with('/') {
            anyhow::bail!(
                "webhook {name:?}: `path` starts with `/`: {:?}",
                options.path
            );
        }
        let listener = std::net::TcpListener::bind(options.bind)
            .map_err(|error| anyhow::anyhow!("webhook {name:?}: bind {}: {error}", options.bind))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let hook = Arc::new(Hook {
            name: name.to_string(),
            topic,
            path: options.path,
            token,
            hmac_secret,
            max_body_bytes: options.max_body_bytes,
            publisher: context.publisher,
        });
        spawn(Some(local_addr), move |cancel| async move {
            let listener = tokio::net::TcpListener::from_std(listener)?;
            // Dropped with the accept loop, which aborts every connection.
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    Some(_) = connections.join_next() => {}
                    accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                        let (stream, _) = accepted
                            .map_err(|error| anyhow::anyhow!("webhook {:?}: accept: {error}", hook.name))?;
                        let hook = Arc::clone(&hook);
                        let service = service_fn(move |request| answer(Arc::clone(&hook), request));
                        connections.spawn(async move {
                            // A connection that fails (a client that hung up,
                            // a malformed or slow request) is that client's:
                            // hyper has answered it as far as it could.
                            let _ = http1::Builder::new()
                                .timer(TokioTimer::new())
                                .header_read_timeout(HEADER_TIMEOUT)
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                        });
                    }
                }
            }
        })
    }
}

/// Answers one request: checks it, then publishes its body.
async fn answer(
    hook: Arc<Hook>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let status = if request.uri().path() != hook.path {
        StatusCode::NOT_FOUND
    } else if request.method() != Method::POST {
        StatusCode::METHOD_NOT_ALLOWED
    } else {
        let headers = request.headers();
        let bearer = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "));
        let declared_length = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
        let signature = headers
            .get("x-signature-256")
            .and_then(|value| hex::decode(value.as_bytes().strip_prefix(b"sha256=")?).ok());
        let reported: Map<String, Value> = REPORTED_HEADERS
            .iter()
            .filter_map(|header| {
                let value = headers.get(*header)?.to_str().ok()?;
                Some(((*header).to_string(), Value::from(value)))
            })
            .collect();
        let limit = u64::try_from(hook.max_body_bytes).unwrap_or(u64::MAX);
        if hook.token.as_ref().is_some_and(|token| {
            !bearer.is_some_and(|bearer| bool::from(bearer.ct_eq(token.0.as_bytes())))
        }) {
            StatusCode::UNAUTHORIZED
        } else if declared_length.is_some_and(|length| length > limit) {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            let body = Limited::new(request.into_body(), hook.max_body_bytes).collect();
            match tokio::time::timeout(BODY_TIMEOUT, body).await {
                Err(_) => StatusCode::REQUEST_TIMEOUT,
                Ok(Err(error)) if error.downcast_ref::<LengthLimitError>().is_some() => {
                    StatusCode::PAYLOAD_TOO_LARGE
                }
                Ok(Err(_)) => StatusCode::BAD_REQUEST,
                Ok(Ok(collected)) => {
                    let body = collected.to_bytes();
                    let signed = hook.hmac_secret.as_ref().is_none_or(|secret| {
                        signature.as_ref().is_some_and(|signature| {
                            Hmac::<Sha256>::new_from_slice(secret.0.as_bytes()).is_ok_and(
                                |mut mac| {
                                    mac.update(&body);
                                    mac.verify_slice(signature).is_ok()
                                },
                            )
                        })
                    });
                    if signed {
                        let payload = match serde_json::from_slice::<Value>(&body) {
                            Ok(json) => json!({ "body": json, "headers": reported }),
                            Err(_) => json!({ "body": String::from_utf8_lossy(&body) }),
                        };
                        let event =
                            source_event(hook.topic.clone(), &hook.name, payload, Trust::Untrusted);
                        match hook.publisher.publish(event) {
                            Ok(()) => StatusCode::ACCEPTED,
                            Err(PublisherClosed) => StatusCode::SERVICE_UNAVAILABLE,
                        }
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }
            }
        }
    };
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = status;
    Ok(response)
}

#[cfg(test)]
mod tests;
