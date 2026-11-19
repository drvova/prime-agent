//! The `oauth` descriptor strategy's sign-in: any models.json provider whose
//! `auth` block declares `oauth` signs in through the browser (authorization
//! code + PKCE S256, a loopback callback on 127.0.0.1 that checks `state`)
//! or the RFC 8628 device-code flow, and stores the answer as the provider's
//! `oauth` credential in auth.json. The sign-in runs in its own command or
//! task, never on a paint path; every wait in it is bounded.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::descriptor::{AuthStrategy, OAuthSpec, ProviderAuthDescriptor};
use super::descriptor_oauth::{
    client_fields, post_form, signed_in_credential, token_grant, TokenRequestError,
};
use super::manager::AuthStorage;
use crate::mcp::oauth_callback::{CallbackRoute, CallbackServer};

/// RFC 8628's device grant type.
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// The poll interval when the device authorization names none (RFC 8628 §3.2).
const DEFAULT_DEVICE_INTERVAL: u64 = 5;
/// How much a `slow_down` answer adds to the poll interval (RFC 8628 §3.5).
const SLOW_DOWN_STEP: u64 = 5;
/// The device code's lifetime when the answer names none.
const DEFAULT_DEVICE_EXPIRES_IN: u64 = 600;
/// How often a wait checks the UI's cancellation.
const CANCEL_CHECK: Duration = Duration::from_millis(100);

/// How the sign-in reached the authorization server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInMethod {
    /// Authorization code + PKCE through the browser and a loopback callback.
    Browser,
    /// The RFC 8628 device-code flow (a code typed on any device).
    DeviceCode,
}

/// Which sign-in method to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInFlow {
    /// The browser when the descriptor has an `authorizationUrl` and a
    /// browser can open here; the device code when the descriptor has only
    /// a `deviceAuthorizationUrl`, or has both and there is no display
    /// (SSH, a headless host).
    Auto,
    /// The browser flow (the descriptor must have an `authorizationUrl`).
    Browser,
    /// The device-code flow (the descriptor must have a
    /// `deviceAuthorizationUrl`).
    DeviceCode,
}

/// The waits one sign-in bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignInTiming {
    /// How long the browser flow waits for the loopback callback.
    pub callback_timeout: Duration,
    /// The length of one device-flow interval second (one second outside
    /// tests): the poll waits `interval` of these between polls.
    pub device_poll_unit: Duration,
}

impl Default for SignInTiming {
    fn default() -> Self {
        SignInTiming {
            callback_timeout: Duration::from_secs(300),
            device_poll_unit: Duration::from_secs(1),
        }
    }
}

/// The surface a sign-in talks to (the CLI's terminal, the TUI's auth
/// panel).
///
/// Implementations show what they are given and return at once: the
/// browser URL is shown even when the implementation also opens a browser
/// (a failed launch must never strand the user), and `is_cancelled` is a
/// cheap flag read the flow polls between waits and before the credential
/// write.
pub trait OAuthSignInUi: Send + Sync {
    /// The browser flow's authorization URL: show it, and open the browser.
    fn show_authorization_url(&self, url: &str);
    /// The device flow's verification URL and the code to enter there.
    fn show_device_code(&self, verification_url: &str, user_code: &str);
    /// One progress line.
    fn progress(&self, message: &str);
    /// Whether the user cancelled the sign-in.
    fn is_cancelled(&self) -> bool;
}

/// Why a sign-in did not complete. Never carries a token, a code, or a
/// secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInError {
    /// The user cancelled.
    Cancelled,
    /// The sign-in failed; the sentence says why.
    Failed(String),
}

impl std::fmt::Display for SignInError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignInError::Cancelled => f.write_str("the sign-in was cancelled"),
            SignInError::Failed(reason) => f.write_str(reason),
        }
    }
}

/// One sign-in's outcome: the method it ran (none when it never started)
/// and the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInAttempt {
    pub method: Option<SignInMethod>,
    pub result: Result<(), SignInError>,
}

/// The models.json providers (under `agent_dir`) whose `auth` block declares
/// the `oauth` strategy, sorted. A missing or invalid models.json lists
/// none (the model registry reports the load error).
#[must_use]
pub fn oauth_descriptor_providers(agent_dir: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(agent_dir.join("models.json")) else {
        return Vec::new();
    };
    let Ok(config) = crate::models::custom::parse_models_config(&content) else {
        return Vec::new();
    };
    let mut providers: Vec<String> = config
        .providers
        .iter()
        .filter(|(provider, entry)| {
            entry.auth.as_ref().is_some_and(|block| {
                matches!(
                    ProviderAuthDescriptor::parse(provider, block)
                        .map(|descriptor| descriptor.strategy),
                    Ok(AuthStrategy::Oauth(_))
                )
            })
        })
        .map(|(provider, _)| provider.clone())
        .collect();
    providers.sort();
    providers
}

/// The provider's validated `oauth` descriptor from models.json.
fn oauth_spec(agent_dir: &Path, provider: &str) -> Result<std::sync::Arc<OAuthSpec>, String> {
    let models = agent_dir.join("models.json");
    let content = std::fs::read_to_string(&models)
        .map_err(|_| format!("{} could not be read", models.display()))?;
    let config = crate::models::custom::parse_models_config(&content)?;
    let block = config
        .providers
        .get(provider)
        .and_then(|entry| entry.auth.as_ref())
        .ok_or_else(|| format!("Provider \"{provider}\" has no \"auth\" block in models.json"))?;
    match ProviderAuthDescriptor::parse(provider, block)?.strategy {
        AuthStrategy::Oauth(spec) => Ok(spec),
        AuthStrategy::ApiKey | AuthStrategy::TokenExchange(_) => Err(format!(
            "Provider \"{provider}\" does not declare the \"oauth\" auth strategy in models.json"
        )),
    }
}

/// Wait `duration`, polling the UI's cancellation; `false` when cancelled.
async fn wait_unless_cancelled(ui: &dyn OAuthSignInUi, duration: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if ui.is_cancelled() {
            return false;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return true;
        }
        tokio::time::sleep((deadline - now).min(CANCEL_CHECK)).await;
    }
}

/// The authorization-request parameters both flows share: scope, audience,
/// and the descriptor's extras.
fn authorization_extras(spec: &OAuthSpec) -> Vec<(String, String)> {
    let mut params = Vec::new();
    if !spec.scopes.is_empty() {
        params.push(("scope".to_string(), spec.scopes.join(" ")));
    }
    if let Some(audience) = &spec.audience {
        params.push(("audience".to_string(), audience.clone()));
    }
    params.extend(
        spec.extra_auth_params
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    params
}

/// Authorization code + PKCE through the browser: the token answer.
async fn browser_sign_in(
    spec: &OAuthSpec,
    provider: &str,
    timing: &SignInTiming,
    ui: &dyn OAuthSignInUi,
) -> Result<super::token_exchange::ExchangedToken, SignInError> {
    let failed = SignInError::Failed;
    let authorization_url = spec.authorization_url.as_ref().ok_or_else(|| {
        failed(format!(
            "Provider \"{provider}\" has no \"authorizationUrl\" for a browser sign-in"
        ))
    })?;
    let (verifier, challenge) = crate::mcp::oauth_discovery::generate_pkce();
    let state = crate::mcp::oauth_discovery::random_state();
    // Loopback only: the callback never listens beyond 127.0.0.1, whatever
    // the MCP callback host override says.
    let server = CallbackServer::start_route(
        provider,
        spec.redirect_port,
        CallbackRoute {
            host: "127.0.0.1",
            path: &spec.redirect_path,
            expected_state: Some(&state),
        },
    )
    .await
    .map_err(|error| failed(error.to_string()))?;
    let redirect_uri = format!("http://127.0.0.1:{}{}", server.port(), spec.redirect_path);
    let mut url = authorization_url.clone();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &spec.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .extend_pairs(authorization_extras(spec));
    ui.show_authorization_url(url.as_str());
    ui.progress("Waiting for the browser sign-in...");
    let code = tokio::select! {
        code = server.wait_for_code() => code,
        finished = wait_unless_cancelled(ui, timing.callback_timeout) => {
            server.cancel().await;
            return Err(if finished {
                failed(format!(
                    "the browser sign-in did not complete within {}s",
                    timing.callback_timeout.as_secs()
                ))
            } else {
                SignInError::Cancelled
            });
        }
    };
    let code = match code {
        Some(callback) if callback.state == state => callback.code,
        Some(_) => return Err(failed("the sign-in callback's state did not match".to_string())),
        None => {
            return Err(failed(
                "the authorization server returned no authorization code (the sign-in was denied or failed)"
                    .to_string(),
            ))
        }
    };
    ui.progress("Exchanging the authorization code...");
    token_grant(
        spec,
        &[
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ],
    )
    .await
    .map_err(|error| failed(format!("the code exchange failed ({error})")))
}

/// The RFC 8628 device-code flow: the token answer.
async fn device_sign_in(
    spec: &OAuthSpec,
    provider: &str,
    timing: &SignInTiming,
    ui: &dyn OAuthSignInUi,
) -> Result<super::token_exchange::ExchangedToken, SignInError> {
    let failed = SignInError::Failed;
    let device_url = spec.device_authorization_url.as_ref().ok_or_else(|| {
        failed(format!(
            "Provider \"{provider}\" has no \"deviceAuthorizationUrl\" for a device-code sign-in"
        ))
    })?;
    let mut form: Vec<(String, String)> = client_fields(spec)
        .map_err(failed)?
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    form.extend(authorization_extras(spec));
    let form: Vec<(&str, String)> = form
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect();
    let answer = post_form(spec, device_url, &form)
        .await
        .map_err(|error| failed(format!("the device authorization failed ({error})")))?;
    let text = |field: &str| {
        answer
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let missing = |field: &str| failed(format!("the device authorization answer has no {field}"));
    let device_code = text("device_code").ok_or_else(|| missing("device_code"))?;
    let user_code = text("user_code").ok_or_else(|| missing("user_code"))?;
    // `verification_url` is the pre-RFC spelling some servers still send.
    let verification = text("verification_uri")
        .or_else(|| text("verification_url"))
        .ok_or_else(|| missing("verification_uri"))?;
    let seconds =
        |field: &str, default: u64| answer.get(field).and_then(Value::as_u64).unwrap_or(default);
    let mut interval = seconds("interval", DEFAULT_DEVICE_INTERVAL);
    let expires_in = seconds("expires_in", DEFAULT_DEVICE_EXPIRES_IN);
    let deadline = tokio::time::Instant::now()
        + timing
            .device_poll_unit
            .saturating_mul(u32::try_from(expires_in).unwrap_or(u32::MAX));
    ui.show_device_code(&verification, &user_code);
    ui.progress("Waiting for the code to be entered...");
    loop {
        let wait = timing
            .device_poll_unit
            .saturating_mul(u32::try_from(interval).unwrap_or(u32::MAX));
        if !wait_unless_cancelled(ui, wait).await {
            return Err(SignInError::Cancelled);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(failed(
                "the device code expired before the sign-in completed".to_string(),
            ));
        }
        let grant = [
            ("grant_type", DEVICE_CODE_GRANT.to_string()),
            ("device_code", device_code.clone()),
        ];
        match token_grant(spec, &grant).await {
            Ok(token) => return Ok(token),
            Err(TokenRequestError::OAuth { code, .. }) if code == "authorization_pending" => {}
            Err(TokenRequestError::OAuth { code, .. }) if code == "slow_down" => {
                interval += SLOW_DOWN_STEP;
            }
            Err(TokenRequestError::OAuth { code, .. }) if code == "access_denied" => {
                return Err(failed("the sign-in was denied".to_string()));
            }
            Err(TokenRequestError::OAuth { code, .. }) if code == "expired_token" => {
                return Err(failed(
                    "the device code expired before the sign-in completed".to_string(),
                ));
            }
            Err(error) => return Err(failed(format!("the device sign-in failed ({error})"))),
        }
    }
}

/// Sign `provider` in through its models.json `oauth` descriptor (under
/// `agent_dir`) and store the answer in `agent_dir`'s auth.json.
pub async fn sign_in_with_descriptor(
    agent_dir: &Path,
    provider: &str,
    flow: SignInFlow,
    timing: &SignInTiming,
    ui: &dyn OAuthSignInUi,
) -> SignInAttempt {
    let spec = match oauth_spec(agent_dir, provider) {
        Ok(spec) => spec,
        Err(reason) => {
            return SignInAttempt {
                method: None,
                result: Err(SignInError::Failed(reason)),
            }
        }
    };
    let method = match (
        flow,
        spec.authorization_url.is_some(),
        spec.device_authorization_url.is_some(),
    ) {
        (SignInFlow::Browser, _, _) | (SignInFlow::Auto, true, false) => SignInMethod::Browser,
        (SignInFlow::DeviceCode, _, _) | (SignInFlow::Auto, false, _) => SignInMethod::DeviceCode,
        (SignInFlow::Auto, true, true) => {
            if crate::platform::browser::can_open_browser() {
                SignInMethod::Browser
            } else {
                SignInMethod::DeviceCode
            }
        }
    };
    let token = match method {
        SignInMethod::Browser => browser_sign_in(&spec, provider, timing, ui).await,
        SignInMethod::DeviceCode => device_sign_in(&spec, provider, timing, ui).await,
    };
    let result = token.and_then(|token| {
        // The surface closed while the sign-in ran: no credential lands.
        if ui.is_cancelled() {
            return Err(SignInError::Cancelled);
        }
        let mut auth = AuthStorage::create(agent_dir);
        if let Some(error) = auth.load_error() {
            return Err(SignInError::Failed(format!(
                "auth.json could not be read ({error})"
            )));
        }
        auth.set(provider, signed_in_credential(&spec, token, None));
        match auth.drain_errors().pop() {
            Some(error) => Err(SignInError::Failed(format!(
                "the sign-in could not be saved ({error})"
            ))),
            None => Ok(()),
        }
    });
    SignInAttempt {
        method: Some(method),
        result,
    }
}
