//! The `token-exchange` resolver: trades a provider's stored long-lived
//! credentials (the auth.json `token_exchange` entry) for a short-lived
//! token at the endpoint its models.json `auth` descriptor declares, and
//! caches the token in that same entry until shortly before it expires.
//!
//! The refresh is single-flight per (provider, account): an in-process
//! async gate plus the auth backend's cross-process refresh lock (the
//! OAuth refresh's [`RefreshLock`](super::storage::RefreshLock) pattern,
//! keyed on the account too). Under the lock the stored credential is
//! re-read, so a waiter reuses the token the winner just wrote.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use serde_json::Value;

use super::descriptor::{
    render_json, render_template, ExchangeBody, ExchangeMethod, ExpirySource, TokenExchangeSpec,
};
use super::resolver::{
    AuthCounter, AuthHeaders, AuthResolveContext, AuthResolveError, AuthResolver,
};
use super::storage::{parse_storage_data, AuthStorageBackend};
use super::types::AuthCredential;

/// A cached token within this margin of its expiry is refreshed instead.
const REFRESH_MARGIN_MS: i64 = 60_000;

/// Epoch values at or above this are milliseconds, below it seconds.
const EPOCH_MS_THRESHOLD: f64 = 1e12;

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |d| d.as_millis() as i64)
}

/// The in-process half of the single flight: one async gate per
/// (provider, account), held across the re-check, the exchange, and the
/// write.
fn exchange_flight(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static FLIGHTS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut flights = FLIGHTS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(flights.entry(key.to_string()).or_default())
}

/// The fields of a stored `token_exchange` credential.
#[derive(Clone, PartialEq, Eq)]
struct StoredExchange {
    refresh: String,
    account_id: Option<String>,
    org_id: Option<String>,
    access: Option<String>,
    expires: Option<i64>,
}

impl StoredExchange {
    fn credential(&self) -> AuthCredential {
        AuthCredential::TokenExchange {
            refresh: self.refresh.clone(),
            account_id: self.account_id.clone(),
            org_id: self.org_id.clone(),
            access: self.access.clone(),
            expires: self.expires,
        }
    }
}

/// One exchange's answer. Holds secrets, so it has no `Debug`.
struct ExchangedToken {
    access: String,
    expires_at: Option<i64>,
    /// The rotated refresh credential, when the response carries one.
    refresh: Option<String>,
}

/// One provider's token-exchange resolver over the shared auth store.
pub(crate) struct TokenExchange {
    spec: Arc<TokenExchangeSpec>,
    storage: Arc<dyn AuthStorageBackend>,
}

impl TokenExchange {
    pub(crate) fn new(spec: Arc<TokenExchangeSpec>, storage: Arc<dyn AuthStorageBackend>) -> Self {
        TokenExchange { spec, storage }
    }

    /// The provider's stored `token_exchange` credential, read off the
    /// executor (a store read can wait on the document lock).
    async fn stored(&self, provider: &str) -> Result<StoredExchange, AuthResolveError> {
        let storage = Arc::clone(&self.storage);
        let failed = |reason: &str| AuthResolveError::Exchange {
            provider: provider.to_string(),
            reason: reason.to_string(),
        };
        let content = tokio::task::spawn_blocking(move || storage.read())
            .await
            .map_err(|_| failed("the auth store read did not finish"))?
            .map_err(|_| failed("auth.json could not be read"))?;
        let data = parse_storage_data(content.as_deref())
            .map_err(|_| failed("auth.json does not parse"))?;
        match data.credential(provider) {
            Some(AuthCredential::TokenExchange {
                refresh,
                account_id,
                org_id,
                access,
                expires,
            }) => Ok(StoredExchange {
                refresh,
                account_id,
                org_id,
                access,
                expires,
            }),
            Some(
                AuthCredential::ApiKey { .. }
                | AuthCredential::McpStaticToken { .. }
                | AuthCredential::Oauth { .. },
            )
            | None => Err(AuthResolveError::MissingCredential {
                provider: provider.to_string(),
            }),
        }
    }

    /// The request credential for a token.
    fn headers_for(&self, access: &str, expires_at: Option<i64>) -> AuthHeaders {
        AuthHeaders {
            api_key: Some(access.to_string()),
            headers: BTreeMap::from([(
                self.spec.header_name.clone(),
                format!("{}{access}", self.spec.header_prefix),
            )]),
            expires_at,
        }
    }

    /// The cached token when it is still fresh.
    fn cached(&self, stored: &StoredExchange) -> Option<AuthHeaders> {
        let access = stored
            .access
            .as_deref()
            .filter(|access| !access.is_empty())?;
        stored
            .expires
            .is_none_or(|expires| now_epoch_ms() + REFRESH_MARGIN_MS < expires)
            .then(|| self.headers_for(access, stored.expires))
    }

    /// Run the declared exchange for `stored`: the token, its expiry, and a
    /// rotated refresh credential when the descriptor declares one.
    /// Error texts carry no URL, body, header, or credential: only fixed
    /// sentences, field names, and the status code.
    async fn exchange(&self, stored: &StoredExchange) -> Result<ExchangedToken, String> {
        let StoredExchange {
            refresh,
            account_id,
            org_id,
            ..
        } = stored;
        let lookup = |name: &str| -> Result<String, String> {
            let missing = |field: &str| format!("the stored credential has no {field}");
            match name {
                "refresh" => Ok(refresh.clone()),
                "accountId" => account_id.clone().ok_or_else(|| missing("accountId")),
                "orgId" => org_id.clone().ok_or_else(|| missing("orgId")),
                other => Err(format!("unknown template placeholder {other}")),
            }
        };
        let spec = &self.spec;
        let client = pa_ai::utils::tls::http_client_builder()
            .timeout(spec.timeout)
            .build()
            .map_err(|_| "the HTTP client could not be built".to_string())?;
        let method = match spec.method {
            ExchangeMethod::Get => reqwest::Method::GET,
            ExchangeMethod::Post => reqwest::Method::POST,
            ExchangeMethod::Put => reqwest::Method::PUT,
        };
        let mut request = client
            .request(method, spec.token_url.clone())
            .header(reqwest::header::ACCEPT, "application/json");
        for (name, template) in &spec.request_headers {
            let mut value =
                reqwest::header::HeaderValue::from_str(&render_template(template, &lookup)?)
                    .map_err(|_| format!("the rendered {name} header is not a valid value"))?;
            value.set_sensitive(true);
            request = request.header(name.as_str(), value);
        }
        request = match &spec.request_body {
            Some(ExchangeBody::Json(template)) => request.json(&render_json(template, &lookup)?),
            Some(ExchangeBody::Form(fields)) => request.form(
                &fields
                    .iter()
                    .map(|(field, template)| {
                        Ok((field.clone(), render_template(template, &lookup)?))
                    })
                    .collect::<Result<BTreeMap<String, String>, String>>()?,
            ),
            None => request,
        };
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                format!(
                    "the token endpoint did not answer within {}ms",
                    spec.timeout.as_millis()
                )
            } else {
                "the token endpoint could not be reached".to_string()
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "the token endpoint answered HTTP {}",
                status.as_u16()
            ));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| "the token endpoint answered without a JSON body".to_string())?;
        let access = body
            .pointer(&spec.token_path)
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| format!("the token response has no token at {}", spec.token_path))?;
        let number_at = |path: &str| {
            let value = body.pointer(path);
            value
                .and_then(Value::as_f64)
                .or_else(|| {
                    value
                        .and_then(Value::as_str)
                        .and_then(|text| text.parse().ok())
                })
                .filter(|number: &f64| number.is_finite() && *number >= 0.0)
                .ok_or_else(|| format!("the token response has no number at {path}"))
        };
        let expires_at = match &spec.expiry {
            ExpirySource::AtPath(path) => {
                let at = number_at(path)?;
                Some(if at >= EPOCH_MS_THRESHOLD {
                    at as i64
                } else {
                    (at * 1000.0) as i64
                })
            }
            ExpirySource::InPath(path) => Some(now_epoch_ms() + (number_at(path)? * 1000.0) as i64),
            ExpirySource::Unknown => None,
        };
        // A rotated refresh credential replaces the stored one; an absent or
        // empty value keeps it.
        let refresh = spec.refresh_path.as_deref().and_then(|path| {
            body.pointer(path)
                .and_then(Value::as_str)
                .filter(|refresh| !refresh.is_empty())
                .map(str::to_string)
        });
        Ok(ExchangedToken {
            access: access.to_string(),
            expires_at,
            refresh,
        })
    }

    /// Persist the fresh token into the credential it was exchanged from,
    /// only while the stored entry is still that credential (a replaced or
    /// removed credential stays authoritative).
    async fn commit(&self, provider: &str, original: &StoredExchange, fresh: &StoredExchange) {
        let storage = Arc::clone(&self.storage);
        let provider = provider.to_string();
        let original = original.credential();
        let fresh = fresh.credential();
        let written = tokio::task::spawn_blocking(move || {
            storage.with_lock(&mut |current| {
                let mut data = parse_storage_data(current.as_deref())?;
                if data.credential(&provider).as_ref() != Some(&original) {
                    return Ok(((), None));
                }
                data.insert(&provider, &fresh);
                Ok(((), Some(serde_json::to_string_pretty(&data.0)?)))
            })
        })
        .await;
        if !matches!(written, Ok(Ok(()))) {
            // The token still serves this call; the next call re-exchanges.
            tracing::debug!("the exchanged token could not be cached in auth.json");
        }
    }
}

impl AuthResolver for TokenExchange {
    #[tracing::instrument(name = "token_exchange_resolve", skip_all, fields(provider = %ctx.provider))]
    async fn resolve(&self, ctx: &AuthResolveContext<'_>) -> Result<AuthHeaders, AuthResolveError> {
        AuthCounter::TokenExchangeResolve.note();
        let stored = self.stored(ctx.provider).await?;
        if let Some(headers) = self.cached(&stored) {
            return Ok(headers);
        }
        // The flight key: provider and account (a unit separator keeps the
        // pair unambiguous); the cross-process lock file hashes it.
        let flight_key = format!(
            "{}\u{1f}{}",
            ctx.provider,
            stored.account_id.as_deref().unwrap_or_default()
        );
        let _flight = exchange_flight(&flight_key).lock_owned().await;
        let storage = Arc::clone(&self.storage);
        let lock_key = flight_key.clone();
        let lock_failed = |reason: &str| AuthResolveError::Exchange {
            provider: ctx.provider.to_string(),
            reason: reason.to_string(),
        };
        let _cross_process = tokio::task::spawn_blocking(move || storage.refresh_lock(&lock_key))
            .await
            .map_err(|_| lock_failed("the refresh lock wait did not finish"))?
            .map_err(|_| lock_failed("another process holds the refresh lock"))?;
        // A waiter re-reads: the winner's fresh token serves it.
        let stored = self.stored(ctx.provider).await?;
        if let Some(headers) = self.cached(&stored) {
            return Ok(headers);
        }
        let ExchangedToken {
            access,
            expires_at,
            refresh,
        } = match self.exchange(&stored).await {
            Ok(token) => {
                AuthCounter::Exchange.note();
                token
            }
            Err(reason) => {
                AuthCounter::ExchangeFailure.note();
                tracing::warn!(reason = %reason, "token exchange failed");
                return Err(AuthResolveError::Exchange {
                    provider: ctx.provider.to_string(),
                    reason,
                });
            }
        };
        // The rotated refresh lands in the same guarded write as the token,
        // still under both locks: a waiter re-reads it, never the spent one.
        let fresh = StoredExchange {
            refresh: refresh.unwrap_or_else(|| stored.refresh.clone()),
            access: Some(access.clone()),
            expires: expires_at,
            ..stored.clone()
        };
        self.commit(ctx.provider, &stored, &fresh).await;
        Ok(self.headers_for(&access, expires_at))
    }
}
