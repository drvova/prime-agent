//! The `oauth` descriptor strategy's request side: a provider whose
//! models.json `auth` block declares `oauth` serves the access token of its
//! stored `oauth` credential (written by the browser or device-code sign-in
//! in [`super::descriptor_oauth_login`]) and refreshes it at the
//! descriptor's `tokenUrl` shortly before it expires.
//!
//! The refresh is single-flight per provider: an in-process async gate plus
//! the auth backend's cross-process refresh lock (the subscription OAuth
//! refresh's lock file for the same provider id). Under the lock the stored
//! credential is re-read, so a waiter reuses the token the winner just
//! wrote, and a rotated refresh token lands in the same guarded write as the
//! new access token. A refresh that fails is never retried silently: the
//! user is told to sign in again.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::descriptor::OAuthSpec;
use super::resolver::{
    AuthCounter, AuthHeaders, AuthResolveContext, AuthResolveError, AuthResolver,
};
use super::storage::{parse_storage_data, AuthStorageBackend};
use super::token_exchange::{
    commit_unchanged, exchange_flight, now_epoch_ms, parse_token_response, send_failure,
    ExchangedToken, REFRESH_MARGIN_MS,
};
use super::types::AuthCredential;

/// The longest OAuth `error` code a failure message carries.
const MAX_ERROR_CODE_LEN: usize = 64;

/// Why a token-endpoint request failed. Neither arm carries a token, a
/// request field, or the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TokenRequestError {
    /// The endpoint answered an RFC 6749 §5.2 error (`authorization_pending`,
    /// `slow_down`, `invalid_grant`, ...), sanitized to the code's charset.
    OAuth { status: u16, code: String },
    /// Any other failure, as a fixed sentence.
    Other(String),
}

impl std::fmt::Display for TokenRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenRequestError::OAuth { status, code } => {
                write!(f, "the token endpoint answered HTTP {status} ({code})")
            }
            TokenRequestError::Other(reason) => f.write_str(reason),
        }
    }
}

/// An RFC 6749 `error` code reduced to its allowed characters: a server
/// that echoes a request value into the field never gets it printed.
fn sanitized_error_code(body: &Value) -> Option<String> {
    let code = body.get("error")?.as_str()?;
    let valid = !code.is_empty()
        && code.len() <= MAX_ERROR_CODE_LEN
        && code
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character));
    Some(if valid {
        code.to_string()
    } else {
        "unrecognized error".to_string()
    })
}

/// The client authentication fields every token request carries: the
/// `client_id`, plus the `client_secret` (RFC 6749 `client_secret_post`) when
/// the descriptor names one, resolved only now.
pub(super) fn client_fields(spec: &OAuthSpec) -> Result<Vec<(&'static str, String)>, String> {
    let mut fields = vec![("client_id", spec.client_id.clone())];
    if let Some(config) = &spec.client_secret {
        let secret = super::resolve_config_value::resolve_config_value(config)
            .ok_or("the \"clientSecret\" config value resolved to nothing")?;
        fields.push(("client_secret", secret));
    }
    Ok(fields)
}

/// POST one form to `url` and read the JSON answer. A 2xx JSON body is the
/// answer; an error body's `error` code is [`TokenRequestError::OAuth`].
pub(super) async fn post_form(
    spec: &OAuthSpec,
    url: &url::Url,
    form: &[(&str, String)],
) -> Result<Value, TokenRequestError> {
    let client = pa_ai::utils::tls::http_client_builder()
        .timeout(spec.timeout)
        .build()
        .map_err(|_| TokenRequestError::Other("the HTTP client could not be built".to_string()))?;
    let response = client
        .post(url.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()
        .await
        .map_err(|error| TokenRequestError::Other(send_failure(&error, spec.timeout)))?;
    let status = response.status();
    let body: Option<Value> = response.json().await.ok();
    if status.is_success() {
        return body.ok_or_else(|| {
            TokenRequestError::Other("the token endpoint answered without a JSON body".to_string())
        });
    }
    Err(match body.as_ref().and_then(sanitized_error_code) {
        Some(code) => TokenRequestError::OAuth {
            status: status.as_u16(),
            code,
        },
        None => TokenRequestError::Other(format!(
            "the token endpoint answered HTTP {}",
            status.as_u16()
        )),
    })
}

/// POST one grant to the token endpoint and read the token through the
/// descriptor's pointers.
pub(super) async fn token_grant(
    spec: &OAuthSpec,
    grant: &[(&str, String)],
) -> Result<ExchangedToken, TokenRequestError> {
    let mut form: Vec<(&str, String)> = grant.to_vec();
    form.extend(client_fields(spec).map_err(TokenRequestError::Other)?);
    let body = post_form(spec, &spec.token_url, &form).await?;
    parse_token_response(
        &body,
        &spec.token_path,
        Some(&spec.refresh_path),
        &spec.expiry,
    )
    .map_err(TokenRequestError::Other)
}

/// The stored credential a token answer becomes: bound to the descriptor's
/// token endpoint and client (a refresh never sends the refresh token
/// anywhere else). A response without a rotated refresh token keeps
/// `previous_refresh`; one without an expiry serves until a rejection.
pub(super) fn signed_in_credential(
    spec: &OAuthSpec,
    token: ExchangedToken,
    previous_refresh: Option<String>,
) -> AuthCredential {
    AuthCredential::Oauth {
        access: token.access,
        refresh: token.refresh.or(previous_refresh),
        expires: token.expires_at.unwrap_or(i64::MAX),
        account_id: None,
        enterprise_url: None,
        endpoint: None,
        token_endpoint: Some(spec.token_url.to_string()),
        client_id: Some(spec.client_id.clone()),
        resource: None,
        issuer: None,
        audience_mode: None,
    }
}

/// The request credential for one access token.
fn token_headers(spec: &OAuthSpec, access: &str, expires_at: i64) -> AuthHeaders {
    AuthHeaders {
        api_key: Some(access.to_string()),
        headers: BTreeMap::from([(
            spec.header_name.clone(),
            format!("{}{access}", spec.header_prefix),
        )]),
        expires_at: (expires_at != i64::MAX).then_some(expires_at),
    }
}

/// One provider's `oauth` resolver over the shared auth store.
pub(crate) struct DescriptorOAuth {
    spec: Arc<OAuthSpec>,
    storage: Arc<dyn AuthStorageBackend>,
}

impl DescriptorOAuth {
    pub(crate) fn new(spec: Arc<OAuthSpec>, storage: Arc<dyn AuthStorageBackend>) -> Self {
        DescriptorOAuth { spec, storage }
    }

    /// The provider's stored sign-in, read off the executor (a store read
    /// can wait on the document lock). A credential bound to another token
    /// endpoint or client is refused: its refresh token belongs elsewhere.
    async fn stored(&self, provider: &str) -> Result<AuthCredential, AuthResolveError> {
        let storage = Arc::clone(&self.storage);
        let sign_in_again = |reason: &str| AuthResolveError::SignInAgain {
            provider: provider.to_string(),
            reason: reason.to_string(),
        };
        let content = tokio::task::spawn_blocking(move || storage.read())
            .await
            .map_err(|_| sign_in_again("the auth store read did not finish"))?
            .map_err(|_| sign_in_again("auth.json could not be read"))?;
        let data = parse_storage_data(content.as_deref())
            .map_err(|_| sign_in_again("auth.json does not parse"))?;
        match data.credential(provider) {
            Some(credential @ AuthCredential::Oauth { .. }) => {
                let bound = matches!(
                    &credential,
                    AuthCredential::Oauth { token_endpoint, client_id, .. }
                        if token_endpoint.as_deref() == Some(self.spec.token_url.as_str())
                            && client_id.as_deref() == Some(self.spec.client_id.as_str())
                );
                if bound {
                    Ok(credential)
                } else {
                    Err(sign_in_again(
                        "the stored sign-in was issued for a different token endpoint or client",
                    ))
                }
            }
            Some(
                AuthCredential::ApiKey { .. }
                | AuthCredential::McpStaticToken { .. }
                | AuthCredential::TokenExchange { .. },
            )
            | None => Err(AuthResolveError::SignInRequired {
                provider: provider.to_string(),
            }),
        }
    }

    /// The stored access token when it is still fresh.
    fn fresh(&self, credential: &AuthCredential) -> Option<AuthHeaders> {
        match credential {
            AuthCredential::Oauth {
                access, expires, ..
            } if !access.is_empty()
                && now_epoch_ms().saturating_add(REFRESH_MARGIN_MS) < *expires =>
            {
                Some(token_headers(&self.spec, access, *expires))
            }
            AuthCredential::Oauth { .. }
            | AuthCredential::ApiKey { .. }
            | AuthCredential::McpStaticToken { .. }
            | AuthCredential::TokenExchange { .. } => None,
        }
    }
}

impl AuthResolver for DescriptorOAuth {
    #[tracing::instrument(name = "descriptor_oauth_resolve", skip_all, fields(provider = %ctx.provider))]
    async fn resolve(&self, ctx: &AuthResolveContext<'_>) -> Result<AuthHeaders, AuthResolveError> {
        AuthCounter::OAuthResolve.note();
        let provider = ctx.provider;
        let sign_in_again = |reason: String| AuthResolveError::SignInAgain {
            provider: provider.to_string(),
            reason,
        };
        let stored = self.stored(provider).await?;
        if let Some(headers) = self.fresh(&stored) {
            return Ok(headers);
        }
        let _flight = exchange_flight(&format!(
            "oauth\u{1f}{provider}{}",
            ctx.credential.flight_suffix()
        ))
        .lock_owned()
        .await;
        let storage = Arc::clone(&self.storage);
        let lock_key = provider.to_string();
        let _cross_process = tokio::task::spawn_blocking(move || storage.refresh_lock(&lock_key))
            .await
            .map_err(|_| sign_in_again("the refresh lock wait did not finish".to_string()))?
            .map_err(|_| sign_in_again("another process holds the refresh lock".to_string()))?;
        // A waiter re-reads: the winner's fresh token serves it.
        let stored = self.stored(provider).await?;
        if let Some(headers) = self.fresh(&stored) {
            return Ok(headers);
        }
        let refresh = match &stored {
            AuthCredential::Oauth {
                refresh: Some(refresh),
                ..
            } if !refresh.is_empty() => refresh.clone(),
            AuthCredential::Oauth { .. }
            | AuthCredential::ApiKey { .. }
            | AuthCredential::McpStaticToken { .. }
            | AuthCredential::TokenExchange { .. } => {
                AuthCounter::OAuthRefreshFailure.note();
                return Err(sign_in_again(
                    "the access token expired and the sign-in holds no refresh token".to_string(),
                ));
            }
        };
        let grant = [
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh.clone()),
        ];
        let token = match token_grant(&self.spec, &grant).await {
            Ok(token) => {
                AuthCounter::OAuthRefresh.note();
                token
            }
            Err(error) => {
                AuthCounter::OAuthRefreshFailure.note();
                tracing::warn!(reason = %error, "oauth descriptor refresh failed");
                return Err(sign_in_again(format!("the token refresh failed ({error})")));
            }
        };
        // The rotated refresh token lands in the same guarded write as the
        // access token, still under both locks: a waiter re-reads it, never
        // the spent one.
        let headers = token_headers(
            &self.spec,
            &token.access,
            token.expires_at.unwrap_or(i64::MAX),
        );
        let fresh = signed_in_credential(&self.spec, token, Some(refresh));
        commit_unchanged(Arc::clone(&self.storage), provider, stored, fresh).await;
        Ok(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that echoes request values into `error` never gets them
    /// printed: only a code-shaped value survives.
    #[test]
    fn an_error_code_keeps_only_the_rfc_charset() {
        let codes: Vec<Option<String>> = [
            serde_json::json!({ "error": "invalid_grant" }),
            serde_json::json!({ "error": "refresh r-secret-token rejected" }),
            serde_json::json!({ "message": "no code" }),
        ]
        .iter()
        .map(sanitized_error_code)
        .collect();
        assert_eq!(
            codes,
            vec![
                Some("invalid_grant".to_string()),
                Some("unrecognized error".to_string()),
                None
            ]
        );
    }
}
