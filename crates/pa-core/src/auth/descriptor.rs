//! Provider auth descriptors: the optional `auth` block of a models.json
//! provider entry, declaring how the provider authenticates. An absent
//! block keeps today's static-key resolution; the `token-exchange` and
//! `oauth` strategies are opt-in per provider.
//!
//! The block is validated at load: an unknown field, a malformed value, or
//! a template naming anything but a stored credential field is a load error
//! that names the provider. Templates (`{{refresh}}`, `{{accountId}}`,
//! `{{orgId}}`) render only into the outgoing token request.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The token-endpoint deadline when the descriptor sets none.
const DEFAULT_EXCHANGE_TIMEOUT_MS: u64 = 10_000;
/// The longest token-endpoint deadline a descriptor may configure: the
/// exchange runs on the inference path, so it stays bounded.
const MAX_EXCHANGE_TIMEOUT_MS: u64 = 60_000;

/// The strategy names on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StrategyName {
    ApiKey,
    TokenExchange,
    Oauth,
}

/// The token request's HTTP method (`POST` when absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum ExchangeMethod {
    Get,
    Post,
    Put,
}

/// How `requestBody` is encoded (`json` when absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BodyEncoding {
    Json,
    Form,
}

/// How the short-lived token is attached to inference requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TokenHeaderConfig {
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prefix: Option<String>,
}

/// The `oauth` strategy's loopback redirect: the port the callback listens
/// on (a free port in the default range when absent) and its path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RedirectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
}

/// The `auth` block exactly as written in models.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProviderAuthConfig {
    pub(crate) strategy: StrategyName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) method: Option<ExchangeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) request_headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) request_body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) request_encoding: Option<BodyEncoding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_in_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) header: Option<TokenHeaderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) authorization_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) device_authorization_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_id: Option<String>,
    /// A config value (an environment variable name, a `!command`, or a
    /// literal), resolved only when a token request is sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scopes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) redirect: Option<RedirectConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) audience: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) extra_auth_params: Option<BTreeMap<String, String>>,
}

/// A validated `auth` block.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderAuthDescriptor {
    pub(crate) strategy: AuthStrategy,
    /// The request header that carries the harness session id (any strategy).
    pub(crate) session_header: Option<String>,
}

/// How the provider's request credential resolves.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AuthStrategy {
    /// Today's static key (stored, environment, or configured `apiKey`).
    ApiKey,
    /// A short-lived token exchanged from the stored `token_exchange` credential.
    TokenExchange(Arc<TokenExchangeSpec>),
    /// An OAuth 2.0 access token from a browser (or device-code) sign-in,
    /// stored as an `oauth` credential and refreshed before it expires.
    Oauth(Arc<OAuthSpec>),
}

/// The validated `oauth` descriptor: where the sign-in and the refresh go.
#[derive(Clone, PartialEq)]
pub(crate) struct OAuthSpec {
    /// The authorization-code endpoint (the browser flow); absent for a
    /// device-code-only provider.
    pub(crate) authorization_url: Option<url::Url>,
    /// The RFC 8628 device authorization endpoint (the device-code flow).
    pub(crate) device_authorization_url: Option<url::Url>,
    pub(crate) token_url: url::Url,
    pub(crate) client_id: String,
    /// The client secret's config value (environment variable, `!command`,
    /// or literal), never the resolved secret.
    pub(crate) client_secret: Option<String>,
    pub(crate) scopes: Vec<String>,
    /// The loopback callback port; `None` binds a free port in the default
    /// callback range.
    pub(crate) redirect_port: Option<u16>,
    pub(crate) redirect_path: String,
    pub(crate) audience: Option<String>,
    /// Extra authorization (and device authorization) request parameters.
    pub(crate) extra_auth_params: BTreeMap<String, String>,
    pub(crate) token_path: String,
    pub(crate) refresh_path: String,
    pub(crate) expiry: ExpirySource,
    pub(crate) header_name: String,
    pub(crate) header_prefix: String,
    pub(crate) timeout: Duration,
}

/// The client secret's config value can be a literal secret, so `Debug`
/// shows only whether one is set.
impl std::fmt::Debug for OAuthSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSpec")
            .field("authorization_url", &self.authorization_url)
            .field("device_authorization_url", &self.device_authorization_url)
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("scopes", &self.scopes)
            .field("redirect_port", &self.redirect_port)
            .field("redirect_path", &self.redirect_path)
            .field("audience", &self.audience)
            .field("extra_auth_params", &self.extra_auth_params)
            .field("token_path", &self.token_path)
            .field("refresh_path", &self.refresh_path)
            .field("expiry", &self.expiry)
            .field("header_name", &self.header_name)
            .field("header_prefix", &self.header_prefix)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// The validated exchange half of a `token-exchange` descriptor.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TokenExchangeSpec {
    pub(crate) token_url: url::Url,
    pub(crate) method: ExchangeMethod,
    /// Header name -> value template.
    pub(crate) request_headers: BTreeMap<String, String>,
    pub(crate) request_body: Option<ExchangeBody>,
    /// JSON pointer to the token in the response.
    pub(crate) token_path: String,
    /// JSON pointer to a rotated refresh credential in the response; a
    /// non-empty string there replaces the stored `refresh`.
    pub(crate) refresh_path: Option<String>,
    pub(crate) expiry: ExpirySource,
    pub(crate) header_name: String,
    pub(crate) header_prefix: String,
    pub(crate) timeout: Duration,
}

/// The token request body template.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ExchangeBody {
    /// A JSON template: every string value renders.
    Json(Value),
    /// Form fields: field -> value template.
    Form(BTreeMap<String, String>),
}

/// Where the token's expiry comes from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ExpirySource {
    /// An absolute epoch timestamp at this JSON pointer. Values at or above
    /// 10^12 are epoch milliseconds, smaller values epoch seconds.
    AtPath(String),
    /// A lifetime in seconds at this JSON pointer, counted from the response.
    InPath(String),
    /// [`ExpirySource::InPath`] when the response carries a number there,
    /// else no expiry (the `oauth` default: `/expires_in` is optional in
    /// RFC 6749 token responses).
    OptionalInPath(String),
    /// No expiry: the token serves until the provider rejects it.
    Unknown,
}

/// Render `template`'s `{{name}}` placeholders through `lookup`.
///
/// # Errors
///
/// Returns why a placeholder cannot render: an unterminated `{{`, or the
/// `lookup` refusal (an unknown name or a missing stored field).
pub(crate) fn render_template(
    template: &str,
    lookup: &dyn Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        rendered.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err("a template has an unterminated \"{{\"".to_string());
        };
        rendered.push_str(&lookup(after[..end].trim())?);
        rest = &after[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

/// Render every string inside a JSON body template.
///
/// # Errors
///
/// Returns the first string's [`render_template`] refusal.
pub(crate) fn render_json(
    template: &Value,
    lookup: &dyn Fn(&str) -> Result<String, String>,
) -> Result<Value, String> {
    Ok(match template {
        Value::String(text) => Value::String(render_template(text, lookup)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render_json(item, lookup))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| Ok((key.clone(), render_json(value, lookup)?)))
                .collect::<Result<_, String>>()?,
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => template.clone(),
    })
}

/// The load-time placeholder check: only stored credential fields render.
fn known_placeholder(name: &str) -> Result<String, String> {
    match name {
        "refresh" | "accountId" | "orgId" => Ok(String::new()),
        other => Err(format!(
            "unknown template placeholder \"{{{{{other}}}}}\" (use {{{{refresh}}}}, {{{{accountId}}}}, or {{{{orgId}}}})"
        )),
    }
}

fn valid_header_name(field: &str, name: &str) -> Result<String, String> {
    reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map(|_| name.to_string())
        .map_err(|_| format!("\"{field}\" is not a valid header name"))
}

fn valid_pointer(field: &str, pointer: &str) -> Result<String, String> {
    if pointer.starts_with('/') {
        Ok(pointer.to_string())
    } else {
        Err(format!(
            "\"{field}\" must be a JSON pointer starting with \"/\""
        ))
    }
}

/// An endpoint URL must be https; plain http is allowed only for loopback
/// hosts (local test servers).
fn valid_endpoint_url(field: &str, raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw).map_err(|_| format!("\"{field}\" is not a valid URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err(format!(
            "\"{field}\" must use https (plain http only for loopback hosts)"
        )),
    }
}

/// The strategies a field belongs to, for the "applies only to" error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldOwner {
    TokenExchange,
    Oauth,
    /// Both token-endpoint strategies.
    TokenEndpoint,
}

impl FieldOwner {
    fn accepts(self, strategy: StrategyName) -> bool {
        match (self, strategy) {
            (FieldOwner::TokenExchange, StrategyName::TokenExchange)
            | (FieldOwner::Oauth, StrategyName::Oauth)
            | (FieldOwner::TokenEndpoint, StrategyName::TokenExchange | StrategyName::Oauth) => {
                true
            }
            (
                FieldOwner::TokenExchange | FieldOwner::Oauth | FieldOwner::TokenEndpoint,
                StrategyName::ApiKey | StrategyName::TokenExchange | StrategyName::Oauth,
            ) => false,
        }
    }

    fn label(self) -> &'static str {
        match self {
            FieldOwner::TokenExchange => "the \"token-exchange\" strategy",
            FieldOwner::Oauth => "the \"oauth\" strategy",
            FieldOwner::TokenEndpoint => "the \"token-exchange\" and \"oauth\" strategies",
        }
    }
}

/// The first field set in `config` that `strategy` does not own.
fn reject_foreign_fields(config: &ProviderAuthConfig) -> Result<(), String> {
    let fields = [
        (
            "tokenUrl",
            FieldOwner::TokenEndpoint,
            config.token_url.is_some(),
        ),
        ("method", FieldOwner::TokenExchange, config.method.is_some()),
        (
            "requestHeaders",
            FieldOwner::TokenExchange,
            config.request_headers.is_some(),
        ),
        (
            "requestBody",
            FieldOwner::TokenExchange,
            config.request_body.is_some(),
        ),
        (
            "requestEncoding",
            FieldOwner::TokenExchange,
            config.request_encoding.is_some(),
        ),
        (
            "tokenPath",
            FieldOwner::TokenEndpoint,
            config.token_path.is_some(),
        ),
        (
            "refreshPath",
            FieldOwner::TokenEndpoint,
            config.refresh_path.is_some(),
        ),
        (
            "expiresAtPath",
            FieldOwner::TokenEndpoint,
            config.expires_at_path.is_some(),
        ),
        (
            "expiresInPath",
            FieldOwner::TokenEndpoint,
            config.expires_in_path.is_some(),
        ),
        ("header", FieldOwner::TokenEndpoint, config.header.is_some()),
        (
            "timeoutMs",
            FieldOwner::TokenEndpoint,
            config.timeout_ms.is_some(),
        ),
        (
            "authorizationUrl",
            FieldOwner::Oauth,
            config.authorization_url.is_some(),
        ),
        (
            "deviceAuthorizationUrl",
            FieldOwner::Oauth,
            config.device_authorization_url.is_some(),
        ),
        ("clientId", FieldOwner::Oauth, config.client_id.is_some()),
        (
            "clientSecret",
            FieldOwner::Oauth,
            config.client_secret.is_some(),
        ),
        ("scopes", FieldOwner::Oauth, config.scopes.is_some()),
        ("redirect", FieldOwner::Oauth, config.redirect.is_some()),
        ("audience", FieldOwner::Oauth, config.audience.is_some()),
        (
            "extraAuthParams",
            FieldOwner::Oauth,
            config.extra_auth_params.is_some(),
        ),
    ];
    match fields
        .iter()
        .find(|(_, owner, set)| *set && !owner.accepts(config.strategy))
    {
        Some((field, owner, _)) => Err(format!("\"{field}\" applies only to {}", owner.label())),
        None => Ok(()),
    }
}

/// The token-endpoint fields both `token-exchange` and `oauth` declare.
struct TokenEndpointFields {
    token_url: url::Url,
    token_path: String,
    refresh_path: Option<String>,
    expiry: ExpirySource,
    header_name: String,
    header_prefix: String,
    timeout: Duration,
}

/// Validate the shared token-endpoint fields. `oauth` defaults the RFC 6749
/// response fields (`/access_token` and an optional `/expires_in`; its
/// caller defaults `/refresh_token`); `token-exchange` requires `tokenPath`.
fn token_endpoint_fields(config: &ProviderAuthConfig) -> Result<TokenEndpointFields, String> {
    let strategy = match config.strategy {
        StrategyName::TokenExchange => "token-exchange",
        StrategyName::Oauth => "oauth",
        StrategyName::ApiKey => return Err("\"api-key\" has no token endpoint".to_string()),
    };
    let oauth = config.strategy == StrategyName::Oauth;
    let token_url = valid_endpoint_url(
        "tokenUrl",
        config
            .token_url
            .as_deref()
            .ok_or_else(|| format!("\"tokenUrl\" is required for \"{strategy}\""))?,
    )?;
    let token_path = match config.token_path.as_deref() {
        Some(path) => valid_pointer("tokenPath", path)?,
        None if oauth => "/access_token".to_string(),
        None => return Err(format!("\"tokenPath\" is required for \"{strategy}\"")),
    };
    let refresh_path = config
        .refresh_path
        .as_deref()
        .map(|path| valid_pointer("refreshPath", path))
        .transpose()?;
    let expiry = match (&config.expires_at_path, &config.expires_in_path) {
        (Some(_), Some(_)) => {
            return Err("set one of \"expiresAtPath\" and \"expiresInPath\", not both".to_string())
        }
        (Some(path), None) => ExpirySource::AtPath(valid_pointer("expiresAtPath", path)?),
        (None, Some(path)) => ExpirySource::InPath(valid_pointer("expiresInPath", path)?),
        (None, None) if oauth => ExpirySource::OptionalInPath("/expires_in".to_string()),
        (None, None) => ExpirySource::Unknown,
    };
    let (header_name, header_prefix) = match &config.header {
        Some(header) => (
            valid_header_name("header.name", &header.name)?,
            header.prefix.clone().unwrap_or_default(),
        ),
        None => ("Authorization".to_string(), "Bearer ".to_string()),
    };
    let timeout_ms = config.timeout_ms.unwrap_or(DEFAULT_EXCHANGE_TIMEOUT_MS);
    if !(1..=MAX_EXCHANGE_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!(
            "\"timeoutMs\" must be between 1 and {MAX_EXCHANGE_TIMEOUT_MS}"
        ));
    }
    Ok(TokenEndpointFields {
        token_url,
        token_path,
        refresh_path,
        expiry,
        header_name,
        header_prefix,
        timeout: Duration::from_millis(timeout_ms),
    })
}

/// The authorization-request parameters the sign-in sets itself: an
/// `extraAuthParams` entry may not override them.
const RESERVED_AUTH_PARAMS: [&str; 9] = [
    "response_type",
    "client_id",
    "client_secret",
    "redirect_uri",
    "state",
    "code_challenge",
    "code_challenge_method",
    "scope",
    "audience",
];

impl ProviderAuthDescriptor {
    /// Parse and validate one provider's `auth` block.
    ///
    /// # Errors
    ///
    /// Returns the load error, naming the provider, for an unknown field, a
    /// malformed value, a missing required field, or an invalid template.
    pub(crate) fn parse(provider: &str, value: &Value) -> Result<Self, String> {
        let validated = serde_json::from_value::<ProviderAuthConfig>(value.clone())
            .map_err(|error| error.to_string())
            .and_then(|config| {
                let session_header = config
                    .session_header
                    .as_deref()
                    .map(|name| valid_header_name("sessionHeader", name))
                    .transpose()?;
                reject_foreign_fields(&config)?;
                let strategy = match config.strategy {
                    StrategyName::ApiKey => AuthStrategy::ApiKey,
                    StrategyName::TokenExchange => {
                        let TokenEndpointFields {
                            token_url,
                            token_path,
                            refresh_path,
                            expiry,
                            header_name,
                            header_prefix,
                            timeout,
                        } = token_endpoint_fields(&config)?;
                        let method = config.method.unwrap_or(ExchangeMethod::Post);
                        let mut request_headers = BTreeMap::new();
                        for (name, template) in config.request_headers.iter().flatten() {
                            render_template(template, &known_placeholder)?;
                            request_headers
                                .insert(valid_header_name(name, name)?, template.clone());
                        }
                        let request_body = match (&config.request_body, config.request_encoding) {
                            (None, None) => None,
                            (None, Some(_)) => {
                                return Err(
                                    "\"requestEncoding\" needs a \"requestBody\"".to_string()
                                )
                            }
                            (Some(_), _) if method == ExchangeMethod::Get => {
                                return Err(
                                    "a GET token request carries no \"requestBody\"".to_string()
                                )
                            }
                            (Some(body), None | Some(BodyEncoding::Json)) => {
                                render_json(body, &known_placeholder)?;
                                Some(ExchangeBody::Json(body.clone()))
                            }
                            (Some(body), Some(BodyEncoding::Form)) => {
                                let fields = body
                                    .as_object()
                                    .ok_or("a form \"requestBody\" must be an object")?;
                                let mut form = BTreeMap::new();
                                for (field, template) in fields {
                                    let template = template
                                        .as_str()
                                        .ok_or("a form \"requestBody\" holds only string values")?;
                                    render_template(template, &known_placeholder)?;
                                    form.insert(field.clone(), template.to_string());
                                }
                                Some(ExchangeBody::Form(form))
                            }
                        };
                        AuthStrategy::TokenExchange(Arc::new(TokenExchangeSpec {
                            token_url,
                            method,
                            request_headers,
                            request_body,
                            token_path,
                            refresh_path,
                            expiry,
                            header_name,
                            header_prefix,
                            timeout,
                        }))
                    }
                    StrategyName::Oauth => {
                        let TokenEndpointFields {
                            token_url,
                            token_path,
                            refresh_path,
                            expiry,
                            header_name,
                            header_prefix,
                            timeout,
                        } = token_endpoint_fields(&config)?;
                        let authorization_url = config
                            .authorization_url
                            .as_deref()
                            .map(|raw| valid_endpoint_url("authorizationUrl", raw))
                            .transpose()?;
                        let device_authorization_url = config
                            .device_authorization_url
                            .as_deref()
                            .map(|raw| valid_endpoint_url("deviceAuthorizationUrl", raw))
                            .transpose()?;
                        if authorization_url.is_none() && device_authorization_url.is_none() {
                            return Err("\"oauth\" needs \"authorizationUrl\" (browser sign-in), \"deviceAuthorizationUrl\" (device code), or both".to_string());
                        }
                        let client_id = config
                            .client_id
                            .clone()
                            .filter(|id| !id.trim().is_empty())
                            .ok_or("\"clientId\" is required for \"oauth\"")?;
                        if config.client_secret.as_deref().is_some_and(str::is_empty) {
                            return Err("\"clientSecret\" must not be empty".to_string());
                        }
                        let scopes = config.scopes.clone().unwrap_or_default();
                        if scopes
                            .iter()
                            .any(|scope| scope.is_empty() || scope.contains(char::is_whitespace))
                        {
                            return Err(
                                "every \"scopes\" entry is one non-empty scope without spaces"
                                    .to_string(),
                            );
                        }
                        if config.redirect.is_some() && authorization_url.is_none() {
                            return Err(
                                "\"redirect\" needs an \"authorizationUrl\" (the browser sign-in)"
                                    .to_string(),
                            );
                        }
                        let redirect = config.redirect.clone().unwrap_or(RedirectConfig {
                            port: None,
                            path: None,
                        });
                        if redirect.port == Some(0) {
                            return Err(
                                "\"redirect.port\" must be between 1 and 65535".to_string()
                            );
                        }
                        let redirect_path =
                            redirect.path.unwrap_or_else(|| "/callback".to_string());
                        if !redirect_path.starts_with('/')
                            || redirect_path.contains(['?', '#', ' '])
                        {
                            return Err("\"redirect.path\" must be a URL path starting with \"/\" (no query or fragment)".to_string());
                        }
                        let extra_auth_params = config.extra_auth_params.clone().unwrap_or_default();
                        if let Some(reserved) = extra_auth_params
                            .keys()
                            .find(|key| RESERVED_AUTH_PARAMS.contains(&key.as_str()))
                        {
                            return Err(format!(
                                "\"extraAuthParams\" may not set \"{reserved}\" (the sign-in sets it)"
                            ));
                        }
                        AuthStrategy::Oauth(Arc::new(OAuthSpec {
                            authorization_url,
                            device_authorization_url,
                            token_url,
                            client_id,
                            client_secret: config.client_secret.clone(),
                            scopes,
                            redirect_port: redirect.port,
                            redirect_path,
                            audience: config.audience.filter(|audience| !audience.is_empty()),
                            extra_auth_params,
                            token_path,
                            refresh_path: refresh_path
                                .unwrap_or_else(|| "/refresh_token".to_string()),
                            expiry,
                            header_name,
                            header_prefix,
                            timeout,
                        }))
                    }
                };
                Ok(ProviderAuthDescriptor {
                    strategy,
                    session_header,
                })
            });
        validated.map_err(|error| format!("Provider {provider}: invalid \"auth\": {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full token-exchange block survives parse -> serialize unchanged,
    /// and validates into the spec it declares.
    #[test]
    fn a_full_token_exchange_descriptor_round_trips() {
        let block = serde_json::json!({
            "strategy": "token-exchange",
            "tokenUrl": "https://auth.example.test/v1/token",
            "method": "POST",
            "requestHeaders": { "x-refresh": "{{refresh}}" },
            "requestBody": { "refresh_token": "{{refresh}}", "account": "{{accountId}}", "org": "{{orgId}}" },
            "requestEncoding": "json",
            "tokenPath": "/access_token",
            "refreshPath": "/refresh_token",
            "expiresInPath": "/expires_in",
            "header": { "name": "Authorization", "prefix": "Bearer " },
            "sessionHeader": "x-example-session",
            "timeoutMs": 5000
        });
        let config: ProviderAuthConfig = serde_json::from_value(block.clone()).unwrap();
        assert_eq!(serde_json::to_value(&config).unwrap(), block);
        assert_eq!(
            ProviderAuthDescriptor::parse("example", &block).unwrap(),
            ProviderAuthDescriptor {
                strategy: AuthStrategy::TokenExchange(Arc::new(TokenExchangeSpec {
                    token_url: url::Url::parse("https://auth.example.test/v1/token").unwrap(),
                    method: ExchangeMethod::Post,
                    request_headers: BTreeMap::from([(
                        "x-refresh".to_string(),
                        "{{refresh}}".to_string()
                    )]),
                    request_body: Some(ExchangeBody::Json(serde_json::json!({
                        "refresh_token": "{{refresh}}", "account": "{{accountId}}", "org": "{{orgId}}"
                    }))),
                    token_path: "/access_token".to_string(),
                    refresh_path: Some("/refresh_token".to_string()),
                    expiry: ExpirySource::InPath("/expires_in".to_string()),
                    header_name: "Authorization".to_string(),
                    header_prefix: "Bearer ".to_string(),
                    timeout: Duration::from_secs(5),
                })),
                session_header: Some("x-example-session".to_string()),
            }
        );
    }

    /// A malformed block is a load error naming the provider, never a panic.
    #[test]
    fn a_malformed_descriptor_is_a_provider_named_load_error() {
        let errors: Vec<String> = [
            serde_json::json!({ "strategy": "token-exchange", "tokenUrl": "https://a.test", "tokenPath": "/t", "colour": "red" }),
            serde_json::json!({ "strategy": "token-exchange", "tokenUrl": "http://auth.example.test", "tokenPath": "/t" }),
            serde_json::json!({ "strategy": "token-exchange", "tokenUrl": "https://a.test", "tokenPath": "/t", "requestBody": { "k": "{{HOME}}" } }),
        ]
        .iter()
        .map(|block| ProviderAuthDescriptor::parse("acme", block).unwrap_err())
        .collect();
        assert_eq!(
            errors,
            vec![
                "Provider acme: invalid \"auth\": unknown field `colour`, expected one of `strategy`, `tokenUrl`, `method`, `requestHeaders`, `requestBody`, `requestEncoding`, `tokenPath`, `refreshPath`, `expiresAtPath`, `expiresInPath`, `header`, `sessionHeader`, `timeoutMs`, `authorizationUrl`, `deviceAuthorizationUrl`, `clientId`, `clientSecret`, `scopes`, `redirect`, `audience`, `extraAuthParams`".to_string(),
                "Provider acme: invalid \"auth\": \"tokenUrl\" must use https (plain http only for loopback hosts)".to_string(),
                "Provider acme: invalid \"auth\": unknown template placeholder \"{{HOME}}\" (use {{refresh}}, {{accountId}}, or {{orgId}})".to_string(),
            ]
        );
    }

    /// A full `oauth` block validates into the spec it declares.
    #[test]
    fn a_full_oauth_descriptor_validates_into_its_spec() {
        let block = serde_json::json!({
            "strategy": "oauth",
            "authorizationUrl": "https://auth.example.test/authorize",
            "deviceAuthorizationUrl": "https://auth.example.test/device",
            "tokenUrl": "https://auth.example.test/token",
            "clientId": "prime-agent-cli",
            "clientSecret": "EXAMPLE_CLIENT_SECRET",
            "scopes": ["openid", "offline_access"],
            "redirect": { "port": 8976, "path": "/oauth/done" },
            "audience": "https://api.example.test",
            "extraAuthParams": { "prompt": "consent" },
            "tokenPath": "/data/access",
            "refreshPath": "/data/refresh",
            "expiresAtPath": "/data/expires_at",
            "header": { "name": "x-api-token" },
            "timeoutMs": 5000
        });
        let config: ProviderAuthConfig = serde_json::from_value(block.clone()).unwrap();
        assert_eq!(serde_json::to_value(&config).unwrap(), block);
        assert_eq!(
            ProviderAuthDescriptor::parse("example", &block).unwrap(),
            ProviderAuthDescriptor {
                strategy: AuthStrategy::Oauth(Arc::new(OAuthSpec {
                    authorization_url: Some(
                        url::Url::parse("https://auth.example.test/authorize").unwrap()
                    ),
                    device_authorization_url: Some(
                        url::Url::parse("https://auth.example.test/device").unwrap()
                    ),
                    token_url: url::Url::parse("https://auth.example.test/token").unwrap(),
                    client_id: "prime-agent-cli".to_string(),
                    client_secret: Some("EXAMPLE_CLIENT_SECRET".to_string()),
                    scopes: vec!["openid".to_string(), "offline_access".to_string()],
                    redirect_port: Some(8976),
                    redirect_path: "/oauth/done".to_string(),
                    audience: Some("https://api.example.test".to_string()),
                    extra_auth_params: BTreeMap::from([(
                        "prompt".to_string(),
                        "consent".to_string()
                    )]),
                    token_path: "/data/access".to_string(),
                    refresh_path: "/data/refresh".to_string(),
                    expiry: ExpirySource::AtPath("/data/expires_at".to_string()),
                    header_name: "x-api-token".to_string(),
                    header_prefix: String::new(),
                    timeout: Duration::from_secs(5),
                })),
                session_header: None,
            }
        );
    }

    /// A minimal `oauth` block takes the RFC 6749 response defaults, the
    /// `/callback` path on a free port, and the bearer header.
    #[test]
    fn a_minimal_oauth_descriptor_takes_the_rfc_defaults() {
        let block = serde_json::json!({
            "strategy": "oauth",
            "authorizationUrl": "https://auth.example.test/authorize",
            "tokenUrl": "https://auth.example.test/token",
            "clientId": "cli"
        });
        assert_eq!(
            ProviderAuthDescriptor::parse("example", &block).unwrap(),
            ProviderAuthDescriptor {
                strategy: AuthStrategy::Oauth(Arc::new(OAuthSpec {
                    authorization_url: Some(
                        url::Url::parse("https://auth.example.test/authorize").unwrap()
                    ),
                    device_authorization_url: None,
                    token_url: url::Url::parse("https://auth.example.test/token").unwrap(),
                    client_id: "cli".to_string(),
                    client_secret: None,
                    scopes: Vec::new(),
                    redirect_port: None,
                    redirect_path: "/callback".to_string(),
                    audience: None,
                    extra_auth_params: BTreeMap::new(),
                    token_path: "/access_token".to_string(),
                    refresh_path: "/refresh_token".to_string(),
                    expiry: ExpirySource::OptionalInPath("/expires_in".to_string()),
                    header_name: "Authorization".to_string(),
                    header_prefix: "Bearer ".to_string(),
                    timeout: Duration::from_secs(10),
                })),
                session_header: None,
            }
        );
    }

    /// Each malformed `oauth` block is a provider-named load error, and the
    /// strategies refuse each other's fields.
    #[test]
    fn a_malformed_oauth_descriptor_is_a_provider_named_load_error() {
        let base = serde_json::json!({
            "strategy": "oauth",
            "authorizationUrl": "https://a.test/authorize",
            "tokenUrl": "https://a.test/token",
            "clientId": "cli"
        });
        let with = |field: &str, value: Value| {
            let mut block = base.clone();
            block[field] = value;
            block
        };
        let without = |field: &str| {
            let mut block = base.clone();
            block.as_object_mut().unwrap().remove(field);
            block
        };
        let errors: Vec<String> = [
            without("clientId"),
            without("authorizationUrl"),
            without("tokenUrl"),
            with("authorizationUrl", serde_json::json!("http://a.test/authorize")),
            with("extraAuthParams", serde_json::json!({ "state": "fixed" })),
            with("scopes", serde_json::json!(["read write"])),
            with("redirect", serde_json::json!({ "port": 0 })),
            with("redirect", serde_json::json!({ "path": "callback" })),
            with("method", serde_json::json!("POST")),
            serde_json::json!({ "strategy": "token-exchange", "tokenUrl": "https://a.test", "tokenPath": "/t", "clientId": "cli" }),
            serde_json::json!({ "strategy": "api-key", "tokenUrl": "https://a.test" }),
            serde_json::json!({ "strategy": "oauth", "deviceAuthorizationUrl": "https://a.test/device", "tokenUrl": "https://a.test/token", "clientId": "cli", "redirect": { "port": 8976 } }),
        ]
        .iter()
        .map(|block| ProviderAuthDescriptor::parse("acme", block).unwrap_err())
        .collect();
        let prefix = "Provider acme: invalid \"auth\": ";
        assert_eq!(
            errors,
            [
                "\"clientId\" is required for \"oauth\"",
                "\"oauth\" needs \"authorizationUrl\" (browser sign-in), \"deviceAuthorizationUrl\" (device code), or both",
                "\"tokenUrl\" is required for \"oauth\"",
                "\"authorizationUrl\" must use https (plain http only for loopback hosts)",
                "\"extraAuthParams\" may not set \"state\" (the sign-in sets it)",
                "every \"scopes\" entry is one non-empty scope without spaces",
                "\"redirect.port\" must be between 1 and 65535",
                "\"redirect.path\" must be a URL path starting with \"/\" (no query or fragment)",
                "\"method\" applies only to the \"token-exchange\" strategy",
                "\"clientId\" applies only to the \"oauth\" strategy",
                "\"tokenUrl\" applies only to the \"token-exchange\" and \"oauth\" strategies",
                "\"redirect\" needs an \"authorizationUrl\" (the browser sign-in)",
            ]
            .map(|error| format!("{prefix}{error}"))
        );
    }

    /// The client secret's config value can be a literal secret: `Debug`
    /// never prints it.
    #[test]
    fn the_oauth_spec_debug_redacts_the_client_secret() {
        let block = serde_json::json!({
            "strategy": "oauth",
            "authorizationUrl": "https://a.test/authorize",
            "tokenUrl": "https://a.test/token",
            "clientId": "cli",
            "clientSecret": "cs-literal-secret-123"
        });
        let descriptor = ProviderAuthDescriptor::parse("acme", &block).unwrap();
        let debug = format!("{descriptor:?}");
        assert!(
            !debug.contains("cs-literal-secret-123") && debug.contains("<redacted>"),
            "{debug}"
        );
    }
}
