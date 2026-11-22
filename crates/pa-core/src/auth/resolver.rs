//! The request-auth resolver seam: the model registry's per-call lookup
//! resolves every inference call's credential through one [`AuthResolver`]
//! — [`StaticKey`] (today's key),
//! [`TokenExchange`](super::token_exchange::TokenExchange) (a short-lived
//! token exchanged from stored long-lived credentials), or
//! [`DescriptorOAuth`](super::descriptor_oauth::DescriptorOAuth) (a browser
//! sign-in's access token, refreshed before it expires), as a provider's
//! models.json `auth` descriptor declares.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

use super::descriptor_oauth::DescriptorOAuth;
use super::token_exchange::TokenExchange;

/// What a resolution is for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AuthResolveContext<'a> {
    /// The provider id the credential is stored and locked under.
    pub(crate) provider: &'a str,
    /// The provider's credential slot the resolution reads: the default (`auth.json`) or the
    /// session's pinned named credential. A resolver never serves another slot than this one.
    pub(crate) credential: CredentialRef<'a>,
}

/// One of a provider's credential slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialRef<'a> {
    /// `auth.json[provider]` through today's ladder.
    Default,
    /// The named credential with this id in `auth-accounts.json`.
    Named(&'a str),
}

impl CredentialRef<'_> {
    /// What keys a refresh single-flight per (provider, credential) after the provider's own key:
    /// nothing for the default slot (today's key), the id for a named credential.
    pub(crate) fn flight_suffix(self) -> String {
        match self {
            CredentialRef::Default => String::new(),
            CredentialRef::Named(id) => format!("\u{1f}{id}"),
        }
    }
}

/// The request credential one resolution answers.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AuthHeaders {
    /// The value for the provider's own key slot: each provider attaches it
    /// in its native wire form (Authorization Bearer, `x-api-key`, ...), so a
    /// static key's requests stay byte-identical to the pre-seam ones.
    pub(crate) api_key: Option<String>,
    /// Headers the request carries on top of the provider's own.
    pub(crate) headers: BTreeMap<String, String>,
    /// Epoch milliseconds the credential expires at, when known.
    pub(crate) expires_at: Option<i64>,
}

/// Header values carry the credential, so `Debug` prints names only.
impl std::fmt::Debug for AuthHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthHeaders")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Why a resolution cannot serve; `Display` is the user-facing sentence and
/// never carries a credential value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthResolveError {
    /// The descriptor wants a token exchange, but auth.json holds no
    /// `token_exchange` credential for the provider.
    MissingCredential { provider: String },
    /// The exchange (or the store around it) failed.
    Exchange { provider: String, reason: String },
    /// The descriptor wants an OAuth sign-in, but auth.json holds no
    /// `oauth` credential for the provider.
    SignInRequired { provider: String },
    /// The stored OAuth sign-in cannot serve (its refresh failed, it has no
    /// refresh token, or it belongs to another endpoint): only a new
    /// sign-in recovers.
    SignInAgain { provider: String, reason: String },
}

/// The command that signs a provider in (the build's own binary name).
fn login_command(provider: &str) -> String {
    format!(
        "{} login {provider}",
        crate::update::installer::BUILD_DISTRIBUTION.command()
    )
}

impl std::fmt::Display for AuthResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthResolveError::MissingCredential { provider } => write!(
                f,
                "Provider \"{provider}\" authenticates by token exchange, but auth.json holds no token_exchange credential for it."
            ),
            AuthResolveError::Exchange { provider, reason } => write!(
                f,
                "Authentication failed for \"{provider}\": the token exchange failed ({reason}).\n\nUpdate the provider's token_exchange credential in auth.json."
            ),
            AuthResolveError::SignInRequired { provider } => write!(
                f,
                "Provider \"{provider}\" signs in with OAuth, but no sign-in is stored for it.\n\nSign in: {}",
                login_command(provider)
            ),
            AuthResolveError::SignInAgain { provider, reason } => write!(
                f,
                "Authentication failed for \"{provider}\": {reason}.\n\nSign in again: {}",
                login_command(provider)
            ),
        }
    }
}

/// The one request-auth seam: resolve the credential an inference call
/// sends.
///
/// Implementations return the credential from their cache or refresh it,
/// never block the caller's executor (blocking I/O runs off it), bound any
/// network wait, and never log, trace, or put into an error a credential
/// value. A resolver may be called concurrently: one that refreshes must
/// single-flight the refresh per (provider, account) across threads and
/// processes, and a caller that waited must reuse the winner's result.
pub(crate) trait AuthResolver {
    /// Resolve the request credential for `ctx`.
    fn resolve(
        &self,
        ctx: &AuthResolveContext<'_>,
    ) -> impl Future<Output = Result<AuthHeaders, AuthResolveError>> + Send;
}

/// Today's behaviour: the resolved static key (stored, environment, or the
/// configured `apiKey`) rides the provider's own key slot, with no extra
/// header and no expiry.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StaticKey {
    api_key: Option<String>,
}

impl StaticKey {
    pub(crate) fn new(api_key: Option<String>) -> Self {
        StaticKey { api_key }
    }

    fn headers(&self) -> AuthHeaders {
        AuthHeaders {
            api_key: self.api_key.clone(),
            headers: BTreeMap::new(),
            expires_at: None,
        }
    }
}

impl AuthResolver for StaticKey {
    fn resolve(
        &self,
        _ctx: &AuthResolveContext<'_>,
    ) -> impl Future<Output = Result<AuthHeaders, AuthResolveError>> + Send {
        std::future::ready(Ok(self.headers()))
    }
}

/// The resolver the registry picked for one call.
pub(crate) enum ProviderAuthResolver {
    Static(StaticKey),
    TokenExchange(TokenExchange),
    Oauth(DescriptorOAuth),
}

impl ProviderAuthResolver {
    /// Resolve from synchronous code: the registry's per-call lookup is
    /// synchronous by contract (the live stream runs it on a blocking thread
    /// raced with the turn's abort). A static key never awaits, so it answers
    /// inline; a token exchange or an OAuth refresh runs on a dedicated
    /// current-thread runtime (the subscription OAuth refresh's pattern),
    /// which works whether or not the caller is inside a runtime.
    ///
    /// # Errors
    ///
    /// Returns the resolver's refusal, or an exchange error when the
    /// resolution thread cannot run.
    pub(crate) fn resolve_blocking(
        &self,
        ctx: &AuthResolveContext<'_>,
    ) -> Result<AuthHeaders, AuthResolveError> {
        if let ProviderAuthResolver::Static(key) = self {
            return Ok(key.headers());
        }
        let failed = |reason: &str| AuthResolveError::Exchange {
            provider: ctx.provider.to_string(),
            reason: reason.to_string(),
        };
        let session_scope = pa_types::session_scope::current();
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("request-auth".to_string())
                .spawn_scoped(scope, || {
                    let _session_scope = pa_types::session_scope::enter(session_scope.clone());
                    let runtime = || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|_| failed("the auth runtime could not start"))
                    };
                    match self {
                        ProviderAuthResolver::Static(key) => Ok(key.headers()),
                        ProviderAuthResolver::TokenExchange(exchange) => {
                            runtime()?.block_on(exchange.resolve(ctx))
                        }
                        ProviderAuthResolver::Oauth(oauth) => {
                            runtime()?.block_on(oauth.resolve(ctx))
                        }
                    }
                })
                .map_err(|_| failed("the auth thread could not start"))?
                .join()
                .map_err(|_| failed("the auth thread panicked"))?
        })
    }
}

/// One resolver fact the adoption telemetry counts.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AuthCounter {
    /// A resolution for a provider whose descriptor declares `api-key`.
    StaticDescriptorResolve,
    /// A resolution for a provider whose descriptor declares `token-exchange`.
    TokenExchangeResolve,
    /// A completed exchange at the token endpoint.
    Exchange,
    /// A failed exchange.
    ExchangeFailure,
    /// A provider rejection (401) that invalidated the cached token, forcing
    /// one refresh before the retry.
    RejectedRetry,
    /// A resolution for a provider whose descriptor declares `oauth`.
    OAuthResolve,
    /// A completed OAuth refresh at the token endpoint.
    OAuthRefresh,
    /// A failed OAuth refresh (the user must sign in again).
    OAuthRefreshFailure,
    /// A quota-exhausted provider's declared fallback credential was chosen
    /// for the retry.
    QuotaFallbackChosen,
    /// A quota-exhausted provider declares a fallback, but none could serve
    /// (none present or usable, or the fallback was exhausted too).
    QuotaFallbackExhausted,
}

/// The process-wide counters: one daemon worker process serves one session,
/// so the session telemetry folds the per-session delta.
static COUNTERS: [AtomicU64; 10] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

impl AuthCounter {
    fn slot(self) -> &'static AtomicU64 {
        let index = match self {
            AuthCounter::StaticDescriptorResolve => 0,
            AuthCounter::TokenExchangeResolve => 1,
            AuthCounter::Exchange => 2,
            AuthCounter::ExchangeFailure => 3,
            AuthCounter::RejectedRetry => 4,
            AuthCounter::OAuthResolve => 5,
            AuthCounter::OAuthRefresh => 6,
            AuthCounter::OAuthRefreshFailure => 7,
            AuthCounter::QuotaFallbackChosen => 8,
            AuthCounter::QuotaFallbackExhausted => 9,
        };
        &COUNTERS[index]
    }

    /// Count one occurrence (fire-and-forget: a relaxed add).
    pub(crate) fn note(self) {
        self.slot().fetch_add(1, Ordering::Relaxed);
    }
}

/// A snapshot of the resolver counters (primitives only: no provider,
/// account, URL, or credential ever rides them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AuthMetrics {
    pub(crate) static_descriptor_resolves: u64,
    pub(crate) token_exchange_resolves: u64,
    pub(crate) exchanges: u64,
    pub(crate) exchange_failures: u64,
    pub(crate) rejected_retries: u64,
    pub(crate) oauth_resolves: u64,
    pub(crate) oauth_refreshes: u64,
    pub(crate) oauth_refresh_failures: u64,
    pub(crate) quota_fallbacks_chosen: u64,
    pub(crate) quota_fallbacks_exhausted: u64,
}

impl AuthMetrics {
    /// The counters now.
    pub(crate) fn current() -> Self {
        let read = |counter: AuthCounter| counter.slot().load(Ordering::Relaxed);
        AuthMetrics {
            static_descriptor_resolves: read(AuthCounter::StaticDescriptorResolve),
            token_exchange_resolves: read(AuthCounter::TokenExchangeResolve),
            exchanges: read(AuthCounter::Exchange),
            exchange_failures: read(AuthCounter::ExchangeFailure),
            rejected_retries: read(AuthCounter::RejectedRetry),
            oauth_resolves: read(AuthCounter::OAuthResolve),
            oauth_refreshes: read(AuthCounter::OAuthRefresh),
            oauth_refresh_failures: read(AuthCounter::OAuthRefreshFailure),
            quota_fallbacks_chosen: read(AuthCounter::QuotaFallbackChosen),
            quota_fallbacks_exhausted: read(AuthCounter::QuotaFallbackExhausted),
        }
    }

    /// What was counted since `baseline`.
    #[must_use]
    pub(crate) fn since(self, baseline: AuthMetrics) -> Self {
        AuthMetrics {
            static_descriptor_resolves: self
                .static_descriptor_resolves
                .saturating_sub(baseline.static_descriptor_resolves),
            token_exchange_resolves: self
                .token_exchange_resolves
                .saturating_sub(baseline.token_exchange_resolves),
            exchanges: self.exchanges.saturating_sub(baseline.exchanges),
            exchange_failures: self
                .exchange_failures
                .saturating_sub(baseline.exchange_failures),
            rejected_retries: self
                .rejected_retries
                .saturating_sub(baseline.rejected_retries),
            oauth_resolves: self.oauth_resolves.saturating_sub(baseline.oauth_resolves),
            oauth_refreshes: self
                .oauth_refreshes
                .saturating_sub(baseline.oauth_refreshes),
            oauth_refresh_failures: self
                .oauth_refresh_failures
                .saturating_sub(baseline.oauth_refresh_failures),
            quota_fallbacks_chosen: self
                .quota_fallbacks_chosen
                .saturating_sub(baseline.quota_fallbacks_chosen),
            quota_fallbacks_exhausted: self
                .quota_fallbacks_exhausted
                .saturating_sub(baseline.quota_fallbacks_exhausted),
        }
    }
}
