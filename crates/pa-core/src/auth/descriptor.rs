//! Provider auth descriptors: the optional `auth` block of a models.json
//! provider entry, declaring how the provider authenticates. An absent
//! block keeps today's static-key resolution; the `token-exchange`
//! strategy is opt-in per provider.
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
    pub(crate) expires_at_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_in_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) header: Option<TokenHeaderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeout_ms: Option<u64>,
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

/// The token URL must be https; plain http is allowed only for loopback
/// hosts (local test servers).
fn valid_token_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw).map_err(|_| "\"tokenUrl\" is not a valid URL".to_string())?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err("\"tokenUrl\" must use https (plain http only for loopback hosts)".to_string()),
    }
}

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
                let strategy = match config.strategy {
                    StrategyName::ApiKey => {
                        let exchange_only = [
                            ("tokenUrl", config.token_url.is_some()),
                            ("method", config.method.is_some()),
                            ("requestHeaders", config.request_headers.is_some()),
                            ("requestBody", config.request_body.is_some()),
                            ("requestEncoding", config.request_encoding.is_some()),
                            ("tokenPath", config.token_path.is_some()),
                            ("expiresAtPath", config.expires_at_path.is_some()),
                            ("expiresInPath", config.expires_in_path.is_some()),
                            ("header", config.header.is_some()),
                            ("timeoutMs", config.timeout_ms.is_some()),
                        ];
                        if let Some((field, _)) = exchange_only.iter().find(|(_, set)| *set) {
                            return Err(format!(
                                "\"{field}\" applies only to the \"token-exchange\" strategy"
                            ));
                        }
                        AuthStrategy::ApiKey
                    }
                    StrategyName::TokenExchange => {
                        let token_url = valid_token_url(
                            config
                                .token_url
                                .as_deref()
                                .ok_or("\"tokenUrl\" is required for \"token-exchange\"")?,
                        )?;
                        let token_path = valid_pointer(
                            "tokenPath",
                            config
                                .token_path
                                .as_deref()
                                .ok_or("\"tokenPath\" is required for \"token-exchange\"")?,
                        )?;
                        let expiry =
                            match (&config.expires_at_path, &config.expires_in_path) {
                                (Some(_), Some(_)) => return Err(
                                    "set one of \"expiresAtPath\" and \"expiresInPath\", not both"
                                        .to_string(),
                                ),
                                (Some(path), None) => {
                                    ExpirySource::AtPath(valid_pointer("expiresAtPath", path)?)
                                }
                                (None, Some(path)) => {
                                    ExpirySource::InPath(valid_pointer("expiresInPath", path)?)
                                }
                                (None, None) => ExpirySource::Unknown,
                            };
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
                        AuthStrategy::TokenExchange(Arc::new(TokenExchangeSpec {
                            token_url,
                            method,
                            request_headers,
                            request_body,
                            token_path,
                            expiry,
                            header_name,
                            header_prefix,
                            timeout: Duration::from_millis(timeout_ms),
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
                "Provider acme: invalid \"auth\": unknown field `colour`, expected one of `strategy`, `tokenUrl`, `method`, `requestHeaders`, `requestBody`, `requestEncoding`, `tokenPath`, `expiresAtPath`, `expiresInPath`, `header`, `sessionHeader`, `timeoutMs`".to_string(),
                "Provider acme: invalid \"auth\": \"tokenUrl\" must use https (plain http only for loopback hosts)".to_string(),
                "Provider acme: invalid \"auth\": unknown template placeholder \"{{HOME}}\" (use {{refresh}}, {{accountId}}, or {{orgId}})".to_string(),
            ]
        );
    }
}
