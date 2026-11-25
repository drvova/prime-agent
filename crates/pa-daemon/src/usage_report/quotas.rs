//! The `/usage` page's provider quota cards: the stored subscription credentials (read through
//! the auth store, never modified here beyond its own expiry refresh), each provider's usage
//! endpoint fetched concurrently under a bounded timeout, and a per-provider cache so reopening
//! the page within the TTL costs no request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pa_ai::oauth::{ProviderHttp, ReqwestProviderHttp};
use pa_ai::quota::{
    fetch_anthropic_quota, fetch_github_copilot_quota, fetch_openai_codex_quota,
    fetch_synthetic_quota, github_api_base_url, FetchedQuota, ANTHROPIC_USAGE_BASE_URL,
    OPENAI_CODEX_USAGE_BASE_URL, QUOTA_REQUEST_TIMEOUT_MS, SYNTHETIC_QUOTA_BASE_URL,
};
use pa_core::auth::{AuthCredential, AuthStorage};
use pa_types::provider_usage::{ProviderQuota, QuotaReport, QuotaSource, QuotaState};

/// A fresh card serves from the cache this long (the providers' own usage pages poll on the
/// same order; a shorter TTL only invites their 429s).
const QUOTA_CACHE_TTL_MS: i64 = 5 * 60_000;
/// A failed card retries sooner than a good one.
const FAILED_QUOTA_CACHE_TTL_MS: i64 = 30_000;

/// Points every provider's usage origin at one host (a loopback fixture server for end-to-end
/// runs); unset in production.
pub(crate) const QUOTA_BASE_URL_ENV: &str = "PRIME_AGENT_QUOTA_BASE_URL";

/// Whether a request may serve a cached card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CachePolicy {
    UseFresh,
    Bypass,
}

/// The plan providers whose own usage endpoint the page reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum QuotaProvider {
    Anthropic,
    OpenAiCodex,
    GithubCopilot,
    Synthetic,
}

impl QuotaProvider {
    const ALL: [QuotaProvider; 4] = [
        QuotaProvider::Anthropic,
        QuotaProvider::OpenAiCodex,
        QuotaProvider::GithubCopilot,
        QuotaProvider::Synthetic,
    ];

    fn id(self) -> &'static str {
        match self {
            QuotaProvider::Anthropic => "anthropic",
            QuotaProvider::OpenAiCodex => "openai-codex",
            QuotaProvider::GithubCopilot => "github-copilot",
            QuotaProvider::Synthetic => "synthetic",
        }
    }
}

/// Which of a provider's credentials a card reads: set only when the provider holds several
/// (the default slot and named credentials from `auth-accounts.json`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QuotaAccount {
    /// The named credential's id, or `default` (the cache key).
    id: String,
    /// The card title's label: the credential's label, or `default`.
    label: String,
}

/// One credential's usable token, or why the stored sign-in cannot open its usage endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QuotaCredential {
    Token {
        provider: QuotaProvider,
        account: Option<QuotaAccount>,
        token: String,
        enterprise_domain: Option<String>,
    },
    Unusable {
        provider: QuotaProvider,
        account: Option<QuotaAccount>,
        reason: String,
    },
}

/// Where each provider's usage endpoint lives and how long one fetch may take.
#[derive(Clone)]
pub(crate) struct QuotaFetchConfig {
    /// One origin for every provider (the [`QUOTA_BASE_URL_ENV`] override), else production.
    pub origin_override: Option<String>,
    pub timeout: Duration,
    pub http: Arc<dyn ProviderHttp>,
}

impl QuotaFetchConfig {
    /// The production configuration (plus the environment's origin override).
    pub(crate) fn from_env() -> Self {
        Self {
            origin_override: pa_types::session_scope::var(QUOTA_BASE_URL_ENV)
                .ok()
                .filter(|origin| !origin.trim().is_empty()),
            // The transport's own bound plus slack: the outer timeout only fires on a transport
            // that ignores its bound.
            timeout: Duration::from_millis(QUOTA_REQUEST_TIMEOUT_MS + 2_000),
            http: Arc::new(ReqwestProviderHttp::new()),
        }
    }
}

type QuotaCache = Mutex<HashMap<(PathBuf, QuotaProvider, Option<QuotaAccount>), ProviderQuota>>;

fn quota_cache() -> &'static QuotaCache {
    static CACHE: OnceLock<QuotaCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The stored plan credentials, each with a usable token: the default slot, then every named
/// credential of the provider (one card each). Blocking: the auth store reads its file and
/// refreshes an expired default-slot OAuth token synchronously; a named sign-in serves its stored
/// access token while it is fresh (its refresh belongs to the session that pins it).
fn quota_credentials(agent_dir: &Path, now_ms: i64) -> Vec<QuotaCredential> {
    let mut auth = AuthStorage::create(agent_dir);
    let stored = auth.get_all();
    let named_store = pa_core::auth::AuthAccounts::create(agent_dir);
    let mut credentials = Vec::new();
    for provider in QuotaProvider::ALL {
        // An unreadable `auth-accounts.json` shows the default slot alone (`/logout` reports it).
        let named = named_store.list(provider.id()).unwrap_or_default();
        let default_account = (!named.is_empty()).then(|| QuotaAccount {
            id: "default".to_string(),
            label: "default".to_string(),
        });
        let default = 'slot: {
            Some(match provider {
                QuotaProvider::Anthropic | QuotaProvider::OpenAiCodex => {
                    let Some(credential) = stored.credential(provider.id()) else {
                        break 'slot None;
                    };
                    let usable = match &credential {
                        AuthCredential::Oauth { .. } => true,
                        // A Claude OAuth token pasted into the API-key slot opens the usage route
                        // too; a plain API key has no subscription quota to show.
                        AuthCredential::ApiKey { key, .. } => {
                            provider == QuotaProvider::Anthropic && key.starts_with("sk-ant-oat")
                        }
                        // A token-exchange sign-in (a models.json `auth` descriptor) has no
                        // subscription usage endpoint: its card comes from observed rate limits and
                        // local spend.
                        AuthCredential::McpStaticToken { .. }
                        | AuthCredential::TokenExchange { .. } => false,
                    };
                    if !usable {
                        break 'slot None;
                    }
                    match auth.get_api_key(provider.id()) {
                        Some(token) => QuotaCredential::Token {
                            provider,
                            account: default_account,
                            token,
                            enterprise_domain: None,
                        },
                        None => QuotaCredential::Unusable {
                            provider,
                            account: default_account,
                            reason: "the stored sign-in has expired; run /login".to_string(),
                        },
                    }
                }
                QuotaProvider::GithubCopilot => match stored.credential(provider.id()) {
                    // The GitHub OAuth token is the refresh token: the Copilot API token in
                    // `access` does not open the GitHub REST route.
                    Some(AuthCredential::Oauth {
                        refresh: Some(token),
                        enterprise_url,
                        ..
                    }) => QuotaCredential::Token {
                        provider,
                        account: default_account,
                        token,
                        enterprise_domain: enterprise_url,
                    },
                    // No GitHub OAuth token, so no route to read; a token-exchange sign-in has no
                    // usage endpoint and falls through to observed rate limits and local spend.
                    None
                    | Some(
                        AuthCredential::Oauth { refresh: None, .. }
                        | AuthCredential::ApiKey { .. }
                        | AuthCredential::McpStaticToken { .. }
                        | AuthCredential::TokenExchange { .. },
                    ) => break 'slot None,
                },
                // The plan is sold on an API key, stored or from the environment.
                QuotaProvider::Synthetic => match auth.get_api_key(provider.id()) {
                    Some(token) => QuotaCredential::Token {
                        provider,
                        account: default_account,
                        token,
                        enterprise_domain: None,
                    },
                    None => break 'slot None,
                },
            })
        };
        credentials.extend(default);
        for named in named {
            let account = Some(QuotaAccount {
                id: named.id,
                label: named.label,
            });
            let token = match (provider, named.credential) {
                (
                    QuotaProvider::Anthropic | QuotaProvider::OpenAiCodex,
                    AuthCredential::Oauth {
                        access, expires, ..
                    },
                ) => Some((now_ms < expires).then_some((access, None))),
                (QuotaProvider::Anthropic, AuthCredential::ApiKey { key, .. })
                    if key.starts_with("sk-ant-oat") =>
                {
                    Some(Some((key, None)))
                }
                (
                    QuotaProvider::GithubCopilot,
                    AuthCredential::Oauth {
                        refresh: Some(token),
                        enterprise_url,
                        ..
                    },
                ) => Some(Some((token, enterprise_url))),
                (QuotaProvider::Synthetic, AuthCredential::ApiKey { key, .. }) => {
                    Some(Some((key, None)))
                }
                // No subscription usage endpoint for this credential's kind.
                (
                    QuotaProvider::Anthropic
                    | QuotaProvider::OpenAiCodex
                    | QuotaProvider::GithubCopilot
                    | QuotaProvider::Synthetic,
                    AuthCredential::Oauth { .. }
                    | AuthCredential::ApiKey { .. }
                    | AuthCredential::McpStaticToken { .. }
                    | AuthCredential::TokenExchange { .. },
                ) => None,
            };
            match token {
                Some(Some((token, enterprise_domain))) => {
                    credentials.push(QuotaCredential::Token {
                        provider,
                        account,
                        token,
                        enterprise_domain,
                    });
                }
                Some(None) => credentials.push(QuotaCredential::Unusable {
                    provider,
                    account,
                    reason: "the stored sign-in has expired; a session pinned to it refreshes it"
                        .to_string(),
                }),
                None => {}
            }
        }
    }
    credentials
}

/// Fetch one provider's card under the configured bound.
async fn fetch_card(
    config: &QuotaFetchConfig,
    provider: QuotaProvider,
    account: Option<&QuotaAccount>,
    token: &str,
    enterprise_domain: Option<&str>,
    now_ms: i64,
) -> ProviderQuota {
    let http = config.http.as_ref();
    let origin = config.origin_override.as_deref();
    let fetch = async {
        match provider {
            QuotaProvider::Anthropic => {
                fetch_anthropic_quota(http, origin.unwrap_or(ANTHROPIC_USAGE_BASE_URL), token).await
            }
            QuotaProvider::OpenAiCodex => {
                fetch_openai_codex_quota(
                    http,
                    origin.unwrap_or(OPENAI_CODEX_USAGE_BASE_URL),
                    token,
                    now_ms,
                )
                .await
            }
            QuotaProvider::GithubCopilot => {
                let api =
                    origin.map_or_else(|| github_api_base_url(enterprise_domain), str::to_string);
                fetch_github_copilot_quota(http, &api, token).await
            }
            QuotaProvider::Synthetic => {
                fetch_synthetic_quota(http, origin.unwrap_or(SYNTHETIC_QUOTA_BASE_URL), token).await
            }
        }
    };
    let (plan, state) = match tokio::time::timeout(config.timeout, fetch).await {
        Ok(Ok(FetchedQuota { plan, windows })) => (plan, QuotaState::Windows { windows }),
        Ok(Err(error)) => (None, QuotaState::Failed { error }),
        Err(_) => (
            None,
            QuotaState::Failed {
                error: "the usage endpoint did not answer in time".to_string(),
            },
        ),
    };
    ProviderQuota {
        provider: provider.id().to_string(),
        plan,
        account: account.map(|account| account.label.clone()),
        source: QuotaSource::Provider,
        fetched_at_ms: now_ms,
        state,
    }
}

/// Every subscription provider's card: cached within the TTL unless `policy` bypasses it, the
/// rest fetched concurrently. A failure stays on its own card.
pub(crate) async fn quota_report(
    agent_dir: PathBuf,
    config: &QuotaFetchConfig,
    policy: CachePolicy,
    now_ms: i64,
) -> QuotaReport {
    let credential_dir = agent_dir.clone();
    let credentials =
        tokio::task::spawn_blocking(move || quota_credentials(&credential_dir, now_ms))
            .await
            .unwrap_or_default();
    let fetches = credentials.into_iter().map(|credential| {
        let agent_dir = agent_dir.clone();
        async move {
            let (provider, account, token, enterprise_domain) = match credential {
                QuotaCredential::Token {
                    provider,
                    account,
                    token,
                    enterprise_domain,
                } => (provider, account, token, enterprise_domain),
                QuotaCredential::Unusable {
                    provider,
                    account,
                    reason,
                } => {
                    return ProviderQuota {
                        provider: provider.id().to_string(),
                        plan: None,
                        account: account.map(|account| account.label),
                        source: QuotaSource::Provider,
                        fetched_at_ms: now_ms,
                        state: QuotaState::Failed { error: reason },
                    };
                }
            };
            let key = (agent_dir, provider, account.clone());
            let cached = quota_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .cloned();
            if let Some(card) = cached.filter(|card| {
                let ttl = match card.state {
                    QuotaState::Failed { .. } => FAILED_QUOTA_CACHE_TTL_MS,
                    QuotaState::Windows { .. } | QuotaState::RateLimited { .. } => {
                        QUOTA_CACHE_TTL_MS
                    }
                };
                policy == CachePolicy::UseFresh && now_ms - card.fetched_at_ms < ttl
            }) {
                return card;
            }
            let card = fetch_card(
                config,
                provider,
                account.as_ref(),
                &token,
                enterprise_domain.as_deref(),
                now_ms,
            )
            .await;
            quota_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, card.clone());
            card
        }
    });
    QuotaReport {
        providers: futures::future::join_all(fetches).await,
        pacing: Vec::new(),
    }
}
