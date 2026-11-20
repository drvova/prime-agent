//! The `oauth` provider auth descriptor end to end: a models.json provider
//! whose `auth` block declares `oauth` signs in through the browser
//! (authorization code + PKCE S256 against a loopback mock authorization
//! server, the callback on 127.0.0.1) or the device-code flow, stores the
//! answer as its `oauth` credential, and refreshes it before expiry through
//! the public registry. Every request stays on loopback; every test owns a
//! temp agent dir (never the real `~/.prime`).

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use pa_core::auth::{
    sign_in_with_descriptor, AuthStorage, OAuthSignInUi, SignInAttempt, SignInFlow, SignInMethod,
    SignInTiming,
};
use pa_core::models::{ModelRegistry, ResolvedRequestAuth};
use pa_types::ai::Model;
use serde_json::{json, Value};
use sha2::Digest as _;

const PROVIDER: &str = "acme";

/// One request the mock server received.
#[derive(Debug, Clone)]
struct Received {
    path: String,
    query: BTreeMap<String, String>,
    form: BTreeMap<String, String>,
    at: Instant,
}

/// The shared request log.
type Log = Arc<Mutex<Vec<Received>>>;

type Handler = dyn Fn(&Received, &[Received]) -> Vec<u8> + Send + Sync;

/// The loopback mock authorization server: every request is logged, then
/// answered by the test's handler (which sees the log so far).
struct Server {
    port: u16,
    received: Log,
}

impl Server {
    fn start(handler: Arc<Handler>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let received: Log = Arc::default();
        let log = Arc::clone(&received);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let log = Arc::clone(&log);
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let Some(request) = read_request(&mut stream) else {
                        return;
                    };
                    let earlier = {
                        let mut log = log.lock().unwrap();
                        let earlier = log.clone();
                        log.push(request.clone());
                        earlier
                    };
                    let response = handler(&request, &earlier);
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
        requests_at(&self.received.lock().unwrap(), path)
    }
}

fn requests_at(log: &[Received], path: &str) -> Vec<Received> {
    log.iter()
        .filter(|request| request.path == path)
        .cloned()
        .collect()
}

/// A string map from literal pairs.
fn pairs(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

fn decode_pairs(raw: &str) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(raw.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
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
        let target = lines.next()?.split_whitespace().nth(1)?.to_string();
        let length: usize = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);
        while bytes.len() < end + 4 + length {
            let read = stream.read(&mut buffer).ok()?;
            if read == 0 {
                return None;
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        let body = String::from_utf8_lossy(&bytes[end + 4..end + 4 + length]).to_string();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        return Some(Received {
            path: path.to_string(),
            query: decode_pairs(query),
            form: decode_pairs(&body),
            at: Instant::now(),
        });
    }
}

fn json_response(status: u16, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} Status\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn redirect_response(location: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        .into_bytes()
}

/// One blocking browser-style GET: the status and the `Location` header.
fn browser_get(url: &str) -> (u16, Option<String>) {
    let url = url::Url::parse(url).unwrap();
    let mut stream =
        std::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).unwrap();
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    // One write: the callback server reads the request head in one read.
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        url.host_str().unwrap()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let location = response
        .lines()
        .find_map(|line| line.strip_prefix("location: "))
        .map(str::to_string);
    (status, location)
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A free loopback port for the callback (the descriptor pins it, so the
/// test never shares the default callback range with other logins).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A temp agent dir whose models.json declares the `oauth` provider.
fn agent_dir(server: &Server) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("models.json"),
        json!({ "providers": { PROVIDER: {
            "baseUrl": server.url("/v1"),
            "api": "openai-completions",
            "auth": {
                "strategy": "oauth",
                "authorizationUrl": server.url("/authorize"),
                "deviceAuthorizationUrl": server.url("/device"),
                "tokenUrl": server.url("/token"),
                "clientId": "cli",
                "clientSecret": "cs-literal-1",
                "scopes": ["openid", "offline_access"],
                "audience": "https://api.acme.test",
                "extraAuthParams": { "prompt": "consent" },
                "redirect": { "port": free_port(), "path": "/oauth/done" }
            },
            "models": [ { "id": "acme-chat" } ]
        } } })
        .to_string(),
    )
    .unwrap();
    dir
}

/// Store a signed-in credential bound to the server's token endpoint.
fn store_sign_in(
    dir: &std::path::Path,
    server: &Server,
    access: &str,
    refresh: &str,
    expires: i64,
) {
    std::fs::write(
        dir.join("auth.json"),
        json!({ PROVIDER: {
            "type": "oauth", "access": access, "refresh": refresh, "expires": expires,
            "tokenEndpoint": server.url("/token"), "clientId": "cli"
        } })
        .to_string(),
    )
    .unwrap();
}

fn stored(dir: &std::path::Path) -> Value {
    let auth: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    auth[PROVIDER].clone()
}

/// The stored credential with its clock-dependent expiry replaced by
/// whether it lands `lifetime_ms` after `before` (within a minute).
fn stored_with_expiry_checked(dir: &std::path::Path, before: i64, lifetime_ms: i64) -> Value {
    let mut credential = stored(dir);
    let expires = credential["expires"].as_i64().unwrap();
    credential["expires"] =
        json!((before + lifetime_ms..=before + lifetime_ms + 60_000).contains(&expires));
    credential
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
    let mut registry = ModelRegistry::create(AuthStorage::create(dir), dir.join("models.json"));
    let model = acme_model(&registry);
    registry.get_api_key_and_headers(&model, model.headers.as_ref())
}

/// The resolution a stored access token produces.
fn served(token: &str) -> ResolvedRequestAuth {
    ResolvedRequestAuth {
        ok: true,
        api_key: Some(token.to_string()),
        headers: Some(pairs(&[("Authorization", &format!("Bearer {token}"))])),
        error: None,
        request_model: None,
        admission_credential: None,
    }
}

/// What a test sign-in surface saw.
#[derive(Default)]
struct Shown {
    authorization_urls: Vec<String>,
    device_codes: Vec<(String, String)>,
    browser_statuses: Vec<u16>,
}

/// The sign-in surface the tests drive: the browser flow's "browser" is a
/// thread that first sends a forged callback, then follows the real
/// authorization redirect to the loopback callback.
#[derive(Default)]
struct TestUi {
    shown: Arc<Mutex<Shown>>,
}

impl OAuthSignInUi for TestUi {
    fn show_authorization_url(&self, url: &str) {
        self.shown
            .lock()
            .unwrap()
            .authorization_urls
            .push(url.to_string());
        let shown = Arc::clone(&self.shown);
        let url = url.to_string();
        std::thread::spawn(move || {
            let redirect_uri = url::Url::parse(&url)
                .unwrap()
                .query_pairs()
                .find(|(name, _)| name == "redirect_uri")
                .map(|(_, value)| value.into_owned())
                .unwrap();
            // A stray local request with the wrong state must not end the login.
            let (forged, _) = browser_get(&format!("{redirect_uri}?code=forged&state=wrong"));
            let (authorize, location) = browser_get(&url);
            let (callback, _) = browser_get(&location.unwrap());
            shown.lock().unwrap().browser_statuses = vec![forged, authorize, callback];
        });
    }

    fn show_device_code(&self, verification_url: &str, user_code: &str) {
        self.shown
            .lock()
            .unwrap()
            .device_codes
            .push((verification_url.to_string(), user_code.to_string()));
    }

    fn progress(&self, _message: &str) {}

    fn is_cancelled(&self) -> bool {
        false
    }
}

fn sign_in(
    dir: &std::path::Path,
    flow: SignInFlow,
    timing: &SignInTiming,
    ui: &TestUi,
) -> SignInAttempt {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(sign_in_with_descriptor(dir, PROVIDER, flow, timing, ui))
}

fn tokens(n: usize) -> Vec<u8> {
    json_response(
        200,
        &json!({ "access_token": format!("at-{n}"), "refresh_token": format!("rt-{n}"),
            "expires_in": 3600, "token_type": "Bearer" }),
    )
}

/// The browser sign-in end to end: the authorization request carries the
/// client, the S256 challenge, the state, and the descriptor's scope,
/// audience, and extras; a forged callback with the wrong state gets the
/// failure page without ending the login; the code exchange proves the
/// PKCE verifier; the credential lands bound to the token endpoint, and the
/// registry serves it.
#[test]
fn the_browser_sign_in_exchanges_a_pkce_code_through_the_loopback_callback() {
    let challenge: Arc<Mutex<Option<String>>> = Arc::default();
    let challenge_seen = Arc::clone(&challenge);
    let server = Server::start(Arc::new(
        move |request: &Received, _: &[Received]| match request.path.as_str() {
            "/authorize" => {
                *challenge_seen.lock().unwrap() = request.query.get("code_challenge").cloned();
                let location = format!(
                    "{}?code=code-1&state={}",
                    request.query["redirect_uri"], request.query["state"]
                );
                redirect_response(&location)
            }
            "/token" => {
                let verifier = request
                    .form
                    .get("code_verifier")
                    .cloned()
                    .unwrap_or_default();
                let proven = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(sha2::Sha256::digest(verifier.as_bytes()));
                if Some(proven) == *challenge_seen.lock().unwrap() {
                    tokens(1)
                } else {
                    json_response(400, &json!({ "error": "invalid_grant" }))
                }
            }
            _ => json_response(404, &json!({})),
        },
    ));
    let dir = agent_dir(&server);
    let ui = TestUi::default();
    let before = now_ms();
    let timing = SignInTiming {
        callback_timeout: Duration::from_secs(20),
        device_poll_unit: Duration::from_secs(1),
    };
    let attempt = sign_in(dir.path(), SignInFlow::Browser, &timing, &ui);
    assert_eq!(
        attempt,
        SignInAttempt {
            method: Some(SignInMethod::Browser),
            result: Ok(())
        }
    );
    let authorize = &server.requests("/authorize")[0];
    let redirect_uri = authorize.query["redirect_uri"].clone();
    assert!(
        redirect_uri.starts_with("http://127.0.0.1:") && redirect_uri.ends_with("/oauth/done"),
        "{redirect_uri}"
    );
    let state = authorize.query["state"].clone();
    let challenge = challenge.lock().unwrap().clone().unwrap();
    assert_eq!(
        authorize.query,
        pairs(&[
            ("response_type", "code"),
            ("client_id", "cli"),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("scope", "openid offline_access"),
            ("audience", "https://api.acme.test"),
            ("prompt", "consent"),
        ])
    );
    let mut token_form = server.requests("/token")[0].form.clone();
    token_form.remove("code_verifier");
    assert_eq!(
        token_form,
        pairs(&[
            ("grant_type", "authorization_code"),
            ("code", "code-1"),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", "cli"),
            ("client_secret", "cs-literal-1"),
        ])
    );
    assert_eq!(
        ui.shown.lock().unwrap().browser_statuses,
        vec![400, 302, 200],
        "the forged callback is refused, the real redirect completes"
    );
    assert_eq!(
        stored_with_expiry_checked(dir.path(), before, 3_600_000),
        json!({ "type": "oauth", "access": "at-1", "refresh": "rt-1", "expires": true,
            "tokenEndpoint": server.url("/token"), "clientId": "cli" })
    );
    assert_eq!(resolve(dir.path()), served("at-1"));
    // The stored credential never prints its tokens.
    let debug = format!(
        "{:?}",
        AuthStorage::create(dir.path())
            .get_all()
            .credential(PROVIDER)
    );
    assert!(
        !debug.contains("at-1") && !debug.contains("rt-1"),
        "{debug}"
    );
}

/// The device-code flow shows the code and verification URL, keeps polling
/// through `authorization_pending`, widens its interval on `slow_down`, and
/// stores the token it is finally issued.
#[test]
fn the_device_sign_in_polls_through_pending_and_slow_down() {
    let server = Server::start(Arc::new(
        |request: &Received, earlier: &[Received]| match request.path.as_str() {
            "/device" => json_response(
                200,
                &json!({ "device_code": "dc-1", "user_code": "ABCD-EFGH",
                    "verification_uri": "https://acme.test/activate",
                    "interval": 1, "expires_in": 600 }),
            ),
            "/token" => match requests_at(earlier, "/token").len() {
                0 => json_response(400, &json!({ "error": "authorization_pending" })),
                1 => json_response(400, &json!({ "error": "slow_down" })),
                _ => tokens(1),
            },
            _ => json_response(404, &json!({})),
        },
    ));
    let dir = agent_dir(&server);
    let ui = TestUi::default();
    let unit = Duration::from_millis(10);
    let timing = SignInTiming {
        callback_timeout: Duration::from_secs(5),
        device_poll_unit: unit,
    };
    let before = now_ms();
    assert_eq!(
        sign_in(dir.path(), SignInFlow::DeviceCode, &timing, &ui),
        SignInAttempt {
            method: Some(SignInMethod::DeviceCode),
            result: Ok(())
        }
    );
    assert_eq!(
        ui.shown.lock().unwrap().device_codes,
        vec![(
            "https://acme.test/activate".to_string(),
            "ABCD-EFGH".to_string()
        )]
    );
    assert_eq!(
        server.requests("/device")[0].form,
        pairs(&[
            ("client_id", "cli"),
            ("client_secret", "cs-literal-1"),
            ("scope", "openid offline_access"),
            ("audience", "https://api.acme.test"),
            ("prompt", "consent"),
        ])
    );
    let polls = server.requests("/token");
    let device_grant = pairs(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("device_code", "dc-1"),
        ("client_id", "cli"),
        ("client_secret", "cs-literal-1"),
    ]);
    assert_eq!(
        polls
            .iter()
            .map(|poll| poll.form.clone())
            .collect::<Vec<_>>(),
        vec![device_grant.clone(), device_grant.clone(), device_grant]
    );
    // `slow_down` adds five seconds (here: units) to the one-unit interval.
    let after_slow_down = polls[2].at - polls[1].at;
    assert!(after_slow_down >= unit * 6, "{after_slow_down:?}");
    assert_eq!(
        stored_with_expiry_checked(dir.path(), before, 3_600_000),
        json!({ "type": "oauth", "access": "at-1", "refresh": "rt-1", "expires": true,
            "tokenEndpoint": server.url("/token"), "clientId": "cli" })
    );
}

/// A token server that rotates refresh tokens: only the refresh token it
/// issued last (initially `rt-1`) is accepted, each refresh answers
/// `at-<n+1>`/`rt-<n+1>`, and a spent one gets `invalid_grant`. With
/// `hold_first`, the first refresh is held until a second refresh arrives
/// or the hold deadline passes (a serialized peer never sends one).
fn rotating_server(hold_first: bool) -> Server {
    let valid = Arc::new(Mutex::new(1usize));
    // Token requests that reached the handler, and their arrival signal.
    let arrivals: Arc<(Mutex<usize>, Condvar)> = Arc::default();
    Server::start(Arc::new(move |request: &Received, _: &[Received]| {
        if request.path != "/token" {
            return json_response(404, &json!({}));
        }
        let (count, arrived) = &*arrivals;
        let mut seen = count.lock().unwrap();
        *seen += 1;
        arrived.notify_all();
        if hold_first && *seen == 1 {
            // Hold until a second refresh arrives or the deadline passes:
            // an observable arrival, never a fixed sleep.
            let _ = arrived
                .wait_timeout_while(seen, Duration::from_millis(1500), |seen| *seen < 2)
                .unwrap();
        } else {
            drop(seen);
        }
        let mut valid = valid.lock().unwrap();
        if request.form.get("refresh_token") == Some(&format!("rt-{valid}")) {
            *valid += 1;
            tokens(*valid)
        } else {
            json_response(400, &json!({ "error": "invalid_grant" }))
        }
    }))
}

fn sent_refreshes(server: &Server) -> Vec<String> {
    server
        .requests("/token")
        .iter()
        .map(|request| request.form["refresh_token"].clone())
        .collect()
}

/// A token inside the refresh margin is refreshed before it expires, the
/// rotated refresh token is persisted and sent next, a cache hit sends
/// nothing, and a provider rejection expires the served token so the next
/// resolution refreshes it.
#[test]
fn a_near_expiry_token_refreshes_and_the_rotated_refresh_token_is_sent_next() {
    let server = rotating_server(false);
    let dir = agent_dir(&server);
    store_sign_in(dir.path(), &server, "at-1", "rt-1", now_ms() + 30_000);
    let before = now_ms();
    assert_eq!(resolve(dir.path()), served("at-2"));
    assert_eq!(
        resolve(dir.path()),
        served("at-2"),
        "the fresh token serves"
    );
    assert_eq!(
        stored_with_expiry_checked(dir.path(), before, 3_600_000),
        json!({ "type": "oauth", "access": "at-2", "refresh": "rt-2", "expires": true,
            "tokenEndpoint": server.url("/token"), "clientId": "cli" })
    );
    // The 401 recovery: the provider rejected the token this store served.
    let mut registry = ModelRegistry::create(
        AuthStorage::create(dir.path()),
        dir.path().join("models.json"),
    );
    registry.auth.refresh_rejected_oauth(PROVIDER).unwrap();
    assert_eq!(resolve(dir.path()), served("at-3"));
    assert_eq!(sent_refreshes(&server), vec!["rt-1", "rt-2"]);
}

/// Concurrent resolutions of an expired token share one refresh: the first
/// is held until a racing second refresh would arrive, and with rotation a
/// second refresh would spend `rt-1` twice and fail.
#[test]
fn concurrent_resolutions_single_flight_one_refresh() {
    let server = rotating_server(true);
    let dir = agent_dir(&server);
    store_sign_in(dir.path(), &server, "at-1", "rt-1", 1);
    let resolved: Vec<ResolvedRequestAuth> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| resolve(dir.path())))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert_eq!(resolved, vec![served("at-2"); 4]);
    assert_eq!(sent_refreshes(&server), vec!["rt-1"]);
}

/// A refresh the token endpoint refuses surfaces the sign-in-again error
/// (never a silent fallback), names no token even when the server echoes
/// one, and leaves the stored credential for the next sign-in to replace.
#[test]
fn a_failed_refresh_surfaces_the_sign_in_again_error() {
    let server = Server::start(Arc::new(|_: &Received, _: &[Received]| {
        json_response(
            400,
            &json!({ "error": "invalid_grant", "error_description": "refresh token rt-secret-1 revoked" }),
        )
    }));
    let dir = agent_dir(&server);
    store_sign_in(dir.path(), &server, "at-secret-1", "rt-secret-1", 1);
    let command = pa_core::update::installer::BUILD_DISTRIBUTION.command();
    assert_eq!(
        resolve(dir.path()),
        ResolvedRequestAuth {
            ok: false,
            error: Some(format!(
                "Authentication failed for \"acme\": the token refresh failed (the token endpoint answered HTTP 400 (invalid_grant)).\n\nSign in again: {command} login acme"
            )),
            ..Default::default()
        }
    );
    assert_eq!(
        stored(dir.path()),
        json!({ "type": "oauth", "access": "at-secret-1", "refresh": "rt-secret-1", "expires": 1,
            "tokenEndpoint": server.url("/token"), "clientId": "cli" })
    );
}

/// Loading the registry and answering availability never refresh: the
/// refresh runs only on the inference path, so a token endpoint that never
/// answers cannot delay startup or first paint.
#[test]
fn loading_the_registry_never_refreshes() {
    let server = Server::start(Arc::new(|_: &Received, _: &[Received]| {
        std::thread::sleep(Duration::from_secs(30));
        Vec::new()
    }));
    let dir = agent_dir(&server);
    store_sign_in(dir.path(), &server, "at-1", "rt-1", 1);
    let started = Instant::now();
    let registry = ModelRegistry::create(
        AuthStorage::create(dir.path()),
        dir.path().join("models.json"),
    );
    let model = acme_model(&registry);
    assert!(registry.has_configured_auth(&model));
    let _ = registry.get_available();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(server.requests("/token").is_empty());
}
