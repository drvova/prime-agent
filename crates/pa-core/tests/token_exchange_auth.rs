//! Descriptor-driven provider auth end to end: a models.json provider whose
//! `auth` block declares `token-exchange` trades the auth.json
//! `token_exchange` credential for a short-lived token at a loopback token
//! endpoint before its inference calls, through the public registry and the
//! live-auth stream seam the daemon wires. Nothing leaves loopback; every
//! test owns a temp agent dir (never the real `~/.prime`).

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pa_core::auth::AuthStorage;
use pa_core::models::{ModelRegistry, ResolvedRequestAuth};
use pa_core::session_engine::auto_retry::{
    run_turn_with_auto_retry, AuthRecovery, AuthRecoveryCallback, AuthRecoveryFuture,
};
use pa_core::session_engine::provider_adapter::{
    switchable_stream_fn_with_live_auth, LiveAuthResolver, ProviderTarget,
};
use pa_core::session_engine::provider_retry::{
    provider_stream_failure_retry_after_ms, ProviderRetryPolicy,
};
use pa_types::ai::Model;
use serde_json::{json, Value};

const PROVIDER: &str = "acme";
const CHILD_ENV: &str = "PA_TOKEN_EXCHANGE_CHILD_DIR";

/// One request the loopback server received.
#[derive(Debug, Clone, PartialEq)]
struct Received {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

/// How the token endpoint answers.
#[derive(Clone, Copy)]
enum TokenMode {
    /// `{"access_token": "short-<n>", "expires_in": 3600}` at once.
    Immediate,
    /// The same, but the first request is held until a second token request
    /// arrives or the hold deadline passes: a serialized peer never sends
    /// one while the first holds the refresh lock, so one request is the
    /// correct outcome and a racing double exchange would record two.
    HoldFirst,
    /// Never answer: a token endpoint that hangs.
    Hang,
    /// Rotate refresh tokens: only the refresh issued last (initially
    /// `r-long`) is accepted, each exchange answers a new
    /// `"refresh_token": "r-<n>"`, and a spent one gets a 401.
    Rotating,
    /// [`TokenMode::Rotating`] with the first request held like
    /// [`TokenMode::HoldFirst`].
    RotatingHoldFirst,
}

/// The loopback token + inference server.
struct Server {
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Server {
    fn start(token_mode: TokenMode, inference: Vec<Vec<u8>>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let received: Arc<Mutex<Vec<Received>>> = Arc::default();
        let inference = Arc::new(Mutex::new(VecDeque::from(inference)));
        let log = Arc::clone(&received);
        let valid_refresh = Arc::new(Mutex::new("r-long".to_string()));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let log = Arc::clone(&log);
                let inference = Arc::clone(&inference);
                let valid_refresh = Arc::clone(&valid_refresh);
                std::thread::spawn(move || {
                    let Some(request) = read_request(&mut stream) else {
                        return;
                    };
                    let is_token = request.path == "/token";
                    let sent_refresh = request.body["refresh_token"].clone();
                    log.lock().unwrap().push(request);
                    let response = if is_token {
                        let count = || {
                            log.lock()
                                .unwrap()
                                .iter()
                                .filter(|request| request.path == "/token")
                                .count()
                        };
                        let issued = count();
                        if matches!(token_mode, TokenMode::Hang) {
                            // Hold the connection open past any bounded wait.
                            std::thread::sleep(Duration::from_secs(30));
                            return;
                        }
                        if matches!(
                            token_mode,
                            TokenMode::HoldFirst | TokenMode::RotatingHoldFirst
                        ) && issued == 1
                        {
                            let deadline = Instant::now() + Duration::from_millis(1500);
                            while count() < 2 && Instant::now() < deadline {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                        }
                        match token_mode {
                            TokenMode::Rotating | TokenMode::RotatingHoldFirst => {
                                let mut valid = valid_refresh.lock().unwrap();
                                if sent_refresh == json!(*valid) {
                                    *valid = format!("r-{issued}");
                                    json_response(
                                        200,
                                        &json!({ "access_token": format!("short-{issued}"),
                                            "expires_in": 3600, "refresh_token": *valid }),
                                    )
                                } else {
                                    json_response(401, &json!({ "error": "invalid_grant" }))
                                }
                            }
                            TokenMode::Immediate | TokenMode::HoldFirst | TokenMode::Hang => {
                                json_response(
                                    200,
                                    &json!({ "access_token": format!("short-{issued}"), "expires_in": 3600 }),
                                )
                            }
                        }
                    } else {
                        inference
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| json_response(500, &json!({})))
                    };
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                });
            }
        });
        Server { port, received }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn requests(&self, path: &str) -> Vec<Received> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path == path)
            .cloned()
            .collect()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Received> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&bytes[..end]).to_string();
        let mut lines = head.lines();
        let mut start = lines.next()?.split_whitespace();
        let method = start.next()?.to_string();
        let path = start.next()?.to_string();
        let headers: BTreeMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let length: usize = headers
            .get("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        while bytes.len() < end + 4 + length {
            let read = stream.read(&mut buffer).ok()?;
            if read == 0 {
                return None;
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        let raw = &bytes[end + 4..end + 4 + length];
        let body = serde_json::from_slice(raw)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(raw).to_string()));
        return Some(Received {
            method,
            path,
            headers,
            body,
        });
    }
}

fn json_response(status: u16, body: &Value) -> Vec<u8> {
    json_response_with(status, &[], body)
}

fn json_response_with(status: u16, headers: &[(&str, &str)], body: &Value) -> Vec<u8> {
    let body = body.to_string();
    let mut extra = String::new();
    for (name, value) in headers {
        extra.push_str(name);
        extra.push_str(": ");
        extra.push_str(value);
        extra.push_str("\r\n");
    }
    format!(
        "HTTP/1.1 {status} Status\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A complete openai-completions SSE reply.
fn sse_reply(text: &str) -> Vec<u8> {
    let delta = json!({"id":"r","object":"chat.completion.chunk","created":0,"model":"acme-chat",
        "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    let done = json!({"id":"r","object":"chat.completion.chunk","created":0,"model":"acme-chat",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {delta}\n\ndata: {done}\n\ndata: [DONE]\n\n"
    )
    .into_bytes()
}

fn unauthorized() -> Vec<u8> {
    json_response(
        401,
        &json!({"error": {"type": "authentication_error", "message": "token expired"}}),
    )
}

/// A temp agent dir with the token-exchange provider and its credential
/// (an expired cached token, so the first call exchanges).
fn agent_dir(server: &Server) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("models.json"),
        json!({ "providers": { PROVIDER: {
            "baseUrl": server.url("/v1"),
            "api": "openai-completions",
            "auth": {
                "strategy": "token-exchange",
                "tokenUrl": server.url("/token"),
                "requestHeaders": { "x-account": "{{accountId}}" },
                "requestBody": { "refresh_token": "{{refresh}}", "account": "{{accountId}}", "org": "{{orgId}}" },
                "tokenPath": "/access_token",
                "expiresInPath": "/expires_in",
                "sessionHeader": "x-acme-session"
            },
            "models": [ { "id": "acme-chat" } ]
        } } })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("auth.json"),
        json!({ PROVIDER: {
            "type": "token_exchange", "refresh": "r-long", "accountId": "acc_1",
            "orgId": "wrk_1", "access": "expired", "expires": 1
        } })
        .to_string(),
    )
    .unwrap();
    dir
}

fn registry(dir: &std::path::Path) -> ModelRegistry {
    ModelRegistry::create(AuthStorage::create(dir), dir.join("models.json"))
}

fn acme_model(registry: &ModelRegistry) -> Model {
    registry
        .get_all()
        .iter()
        .find(|model| model.provider == PROVIDER)
        .cloned()
        .expect("the models.json provider loads")
}

fn resolve(dir: &std::path::Path) -> ResolvedRequestAuth {
    let mut registry = registry(dir);
    let model = acme_model(&registry);
    registry.get_api_key_and_headers(&model, model.headers.as_ref())
}

/// Set one field of the provider's models.json `auth` block.
fn set_auth_field(dir: &std::path::Path, field: &str, value: Value) {
    let models_path = dir.join("models.json");
    let mut models: Value =
        serde_json::from_str(&std::fs::read_to_string(&models_path).unwrap()).unwrap();
    models["providers"][PROVIDER]["auth"][field] = value;
    std::fs::write(&models_path, models.to_string()).unwrap();
}

/// Force the next resolution to exchange: the provider rejected the token
/// this store served (the 401 recovery path).
fn reject_served_token(dir: &std::path::Path) {
    let mut auth = AuthStorage::create(dir);
    auth.refresh_rejected_oauth(PROVIDER).unwrap();
}

fn stored(dir: &std::path::Path) -> Value {
    let auth: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    auth[PROVIDER].clone()
}

/// The resolution an exchanged `short-<n>` token produces.
fn served(token: &str) -> ResolvedRequestAuth {
    ResolvedRequestAuth {
        ok: true,
        api_key: Some(token.to_string()),
        headers: Some(BTreeMap::from([
            ("Authorization".to_string(), format!("Bearer {token}")),
            ("x-acme-session".to_string(), "{{sessionId}}".to_string()),
        ])),
        error: None,
        request_model: None,
    }
}

/// A provider with no `auth` block keeps its static-key resolution exactly.
#[test]
fn a_provider_without_an_auth_block_keeps_its_static_key() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("models.json"),
        json!({ "providers": { "plain": {
            "baseUrl": "http://127.0.0.1:9/v1",
            "api": "openai-completions",
            "apiKey": "sk-static",
            "models": [ { "id": "plain-chat" } ]
        } } })
        .to_string(),
    )
    .unwrap();
    let mut registry = registry(dir.path());
    let model = registry
        .get_all()
        .iter()
        .find(|model| model.provider == "plain")
        .cloned()
        .unwrap();
    assert_eq!(
        registry.get_api_key_and_headers(&model, model.headers.as_ref()),
        ResolvedRequestAuth {
            ok: true,
            api_key: Some("sk-static".to_string()),
            headers: None,
            error: None,
            request_model: None,
        }
    );
}

/// The templates render into the token request only, the token and its
/// expiry land in the credential's cache, and a cache hit sends nothing.
#[test]
fn an_exchange_renders_its_templates_and_a_cache_hit_sends_nothing() {
    let server = Server::start(TokenMode::Immediate, Vec::new());
    let dir = agent_dir(&server);
    let before = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    assert_eq!(resolve(dir.path()), served("short-1"));
    assert_eq!(resolve(dir.path()), served("short-1"), "the cache serves");
    let requests = server.requests("/token");
    assert_eq!(requests.len(), 1, "a cache hit makes no second request");
    let request = &requests[0];
    assert_eq!(
        (
            request.method.as_str(),
            request.headers.get("x-account").map(String::as_str),
            &request.body
        ),
        (
            "POST",
            Some("acc_1"),
            &json!({ "refresh_token": "r-long", "account": "acc_1", "org": "wrk_1" })
        )
    );
    let mut cached = stored(dir.path());
    let expires = cached["expires"].as_i64().unwrap();
    assert!(
        (before + 3_600_000..=before + 3_660_000).contains(&expires),
        "expires_in counts from the response: {expires}"
    );
    cached["expires"] = json!(0);
    assert_eq!(
        cached,
        json!({ "type": "token_exchange", "refresh": "r-long", "accountId": "acc_1",
            "orgId": "wrk_1", "access": "short-1", "expires": 0 })
    );
}

/// Loading the registry and answering availability never exchange: the
/// exchange runs only on the inference path, so a hanging token endpoint
/// cannot delay startup or first paint.
#[test]
fn loading_the_registry_never_exchanges() {
    let server = Server::start(TokenMode::Hang, Vec::new());
    let dir = agent_dir(&server);
    let registry = registry(dir.path());
    let model = acme_model(&registry);
    assert!(registry.has_configured_auth(&model));
    assert!(registry
        .get_available()
        .iter()
        .any(|available| available.provider == PROVIDER));
    assert_eq!(server.requests("/token").len(), 0);
}

/// A hanging token endpoint fails the call within the descriptor's bounded
/// deadline, with no credential in the error.
#[test]
fn a_hanging_token_endpoint_fails_within_its_deadline() {
    let server = Server::start(TokenMode::Hang, Vec::new());
    let dir = agent_dir(&server);
    set_auth_field(dir.path(), "timeoutMs", json!(300));
    let started = Instant::now();
    let resolved = resolve(dir.path());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait is bounded"
    );
    assert_eq!(
        resolved,
        ResolvedRequestAuth {
            ok: false,
            error: Some(
                "Authentication failed for \"acme\": the token exchange failed (the token endpoint did not answer within 300ms).\n\nUpdate the provider's token_exchange credential in auth.json."
                    .to_string()
            ),
            ..Default::default()
        }
    );
}

/// N concurrent resolutions of one expired token make exactly one exchange,
/// and every waiter serves the winner's token.
#[test]
fn concurrent_resolutions_single_flight_one_exchange() {
    let server = Server::start(TokenMode::HoldFirst, Vec::new());
    let dir = agent_dir(&server);
    let path = dir.path().to_path_buf();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let resolutions: Vec<ResolvedRequestAuth> = (0..8)
        .map(|_| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut registry = registry(&path);
                let model = acme_model(&registry);
                barrier.wait();
                registry.get_api_key_and_headers(&model, model.headers.as_ref())
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(server.requests("/token").len(), 1);
    assert_eq!(resolutions, vec![served("short-1"); 8]);
}

/// Child role only: the parent test re-executes this binary.
#[test]
fn child_resolver() {
    let Ok(dir) = std::env::var(CHILD_ENV) else {
        return;
    };
    println!(
        "CHILD_KEY={}",
        resolve(std::path::Path::new(&dir))
            .api_key
            .unwrap_or_default()
    );
}

/// Two processes sharing an auth dir resolve one expired token with one
/// exchange: the child exchanges under the cross-process refresh lock while
/// the parent waits on it, then the parent re-reads and reuses the token.
#[test]
fn two_processes_share_one_exchange() {
    let server = Server::start(TokenMode::HoldFirst, Vec::new());
    let dir = agent_dir(&server);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_resolver", "--nocapture"])
        .env(CHILD_ENV, dir.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The child-first schedule: its exchange is in flight (holding the
    // lock) before this process resolves.
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.requests("/token").is_empty() {
        assert!(Instant::now() < deadline, "the child never exchanged");
        std::thread::sleep(Duration::from_millis(10));
    }
    let parent = resolve(dir.path());
    let output = child.wait_with_output().unwrap();
    let child_key = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("CHILD_KEY=").map(str::to_string));
    assert_eq!(server.requests("/token").len(), 1);
    assert_eq!(parent, served("short-1"));
    assert_eq!(child_key.as_deref(), Some("short-1"));
}

/// The refresh credential every token request sent, in order.
fn sent_refreshes(server: &Server) -> Vec<String> {
    server
        .requests("/token")
        .iter()
        .map(|request| {
            request.body["refresh_token"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// A rotating provider accepts only the refresh it issued last: two forced
/// exchanges in a row both succeed because each stores the rotated refresh
/// in the same entry as the token.
#[test]
fn a_rotated_refresh_is_stored_and_the_next_exchange_sends_it() {
    let server = Server::start(TokenMode::Rotating, Vec::new());
    let dir = agent_dir(&server);
    set_auth_field(dir.path(), "refreshPath", json!("/refresh_token"));
    assert_eq!(resolve(dir.path()), served("short-1"));
    reject_served_token(dir.path());
    assert_eq!(resolve(dir.path()), served("short-2"));
    assert_eq!(sent_refreshes(&server), vec!["r-long", "r-1"]);
    assert_eq!(stored(dir.path())["refresh"], json!("r-2"));
}

/// Two processes racing one rotation never send a spent refresh: the child
/// exchanges (and rotates) under the cross-process lock while the parent
/// waits, the parent reuses the child's token, and the parent's next forced
/// exchange sends the refresh the child stored.
#[test]
fn two_processes_racing_a_rotation_never_send_a_spent_refresh() {
    let server = Server::start(TokenMode::RotatingHoldFirst, Vec::new());
    let dir = agent_dir(&server);
    set_auth_field(dir.path(), "refreshPath", json!("/refresh_token"));
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_resolver", "--nocapture"])
        .env(CHILD_ENV, dir.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.requests("/token").is_empty() {
        assert!(Instant::now() < deadline, "the child never exchanged");
        std::thread::sleep(Duration::from_millis(10));
    }
    let parent = resolve(dir.path());
    let output = child.wait_with_output().unwrap();
    let child_key = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("CHILD_KEY=").map(str::to_string));
    reject_served_token(dir.path());
    let after_rotation = resolve(dir.path());
    assert_eq!(
        (parent, child_key, after_rotation),
        (
            served("short-1"),
            Some("short-1".to_string()),
            served("short-2")
        )
    );
    assert_eq!(sent_refreshes(&server), vec!["r-long", "r-1"]);
}

/// The daemon's live-auth stream over one shared registry, and the auth
/// recovery it hands the retry driver.
struct Session {
    stream_fn: pa_agent::stream::StreamFn,
    registry: Arc<Mutex<ModelRegistry>>,
}

impl Session {
    fn new(dir: &std::path::Path) -> Self {
        let registry = registry(dir);
        let model = acme_model(&registry);
        let registry = Arc::new(Mutex::new(registry));
        let live_registry = Arc::clone(&registry);
        let live_auth: LiveAuthResolver = Arc::new(move |model: &Model| {
            live_registry
                .lock()
                .unwrap()
                .get_api_key_and_headers(model, model.headers.as_ref())
        });
        let target = Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
            api_key: None,
            model,
            service_tier: None,
            headers: None,
        })));
        Session {
            stream_fn: switchable_stream_fn_with_live_auth(target, live_auth),
            registry,
        }
    }

    /// One turn under the retry driver (no waits), with the daemon's auth
    /// recovery: refresh the rejected credential, then retry.
    async fn turn(&self) -> pa_agent::types::AssistantMessage {
        let policy = ProviderRetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 1,
            max_retry_delay_ms: 60_000,
            max_delay_ms: 1,
        };
        let registry = Arc::clone(&self.registry);
        let mut recover = move |_: &pa_agent::types::AssistantMessage| -> AuthRecoveryFuture {
            let registry = Arc::clone(&registry);
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    registry
                        .lock()
                        .unwrap()
                        .auth
                        .refresh_rejected_oauth(PROVIDER)
                })
                .await
                .unwrap()
                .unwrap();
                AuthRecovery::Retry
            })
        };
        let auth_recovery: AuthRecoveryCallback<'_> = &mut recover;
        run_turn_with_auto_retry(
            &policy,
            128_000,
            None,
            || async {
                let options = pa_agent::stream::StreamRequestOptions {
                    session_id: Some("conversation-1".to_string()),
                    ..Default::default()
                };
                let mut stream = (self.stream_fn)(
                    pa_agent::types::Model::unknown(),
                    pa_agent::stream::LlmContext::default(),
                    options,
                )
                .await?;
                stream.result().await
            },
            |_| async { Ok(()) },
            |_| async { true },
            None,
            Some(auth_recovery),
        )
        .await
        .unwrap()
    }
}

fn inference_auth(requests: &[Received]) -> Vec<(Option<&str>, Option<&str>)> {
    requests
        .iter()
        .map(|request| {
            (
                request.headers.get("authorization").map(String::as_str),
                request.headers.get("x-acme-session").map(String::as_str),
            )
        })
        .collect()
}

/// A 401 forces exactly one refresh and one retry, which succeeds; the next
/// turn of the conversation reuses the refreshed token and carries the same
/// session header.
#[tokio::test]
async fn a_rejected_token_refreshes_once_and_the_retry_succeeds() {
    let server = Server::start(
        TokenMode::Immediate,
        vec![unauthorized(), sse_reply("one"), sse_reply("two")],
    );
    let dir = agent_dir(&server);
    let session = Session::new(dir.path());
    let first = session.turn().await;
    let second = session.turn().await;
    assert_eq!(
        (first.stop_reason, second.stop_reason),
        (
            pa_agent::types::StopReason::Stop,
            pa_agent::types::StopReason::Stop
        )
    );
    assert_eq!(server.requests("/token").len(), 2);
    assert_eq!(
        inference_auth(&server.requests("/v1/chat/completions")),
        vec![
            (Some("Bearer short-1"), Some("conversation-1")),
            (Some("Bearer short-2"), Some("conversation-1")),
            (Some("Bearer short-2"), Some("conversation-1")),
        ]
    );
}

/// A second 401 surfaces the error: one refresh, one retry, no loop.
#[tokio::test]
async fn a_second_rejection_surfaces_without_a_refresh_loop() {
    let server = Server::start(
        TokenMode::Immediate,
        vec![unauthorized(), unauthorized(), sse_reply("never")],
    );
    let dir = agent_dir(&server);
    let message = Session::new(dir.path()).turn().await;
    assert_eq!(message.stop_reason, pa_agent::types::StopReason::Error);
    assert_eq!(server.requests("/token").len(), 2);
    assert_eq!(server.requests("/v1/chat/completions").len(), 2);
}

/// A 429 surfaces the parsed reset (`x-ratelimit-reset-requests`) on the
/// structured failure, and a reset beyond the policy's wait cap gives up at
/// once instead of spinning.
#[tokio::test]
async fn a_rate_limit_surfaces_its_reset_without_retrying() {
    let server = Server::start(
        TokenMode::Immediate,
        vec![
            json_response_with(
                429,
                &[("x-ratelimit-reset-requests", "2m0s")],
                &json!({"error": {"type": "rate_limit_exceeded", "message": "slow down"}}),
            ),
            sse_reply("never"),
        ],
    );
    let dir = agent_dir(&server);
    let message = Session::new(dir.path()).turn().await;
    assert_eq!(
        (
            message.stop_reason,
            provider_stream_failure_retry_after_ms(&message)
        ),
        (pa_agent::types::StopReason::Error, Some(120_000))
    );
    assert_eq!(server.requests("/v1/chat/completions").len(), 1);
}
