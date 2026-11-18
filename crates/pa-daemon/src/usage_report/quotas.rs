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
    github_api_base_url, FetchedQuota, ANTHROPIC_USAGE_BASE_URL, OPENAI_CODEX_USAGE_BASE_URL,
    QUOTA_REQUEST_TIMEOUT_MS,
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

/// The subscription providers whose own usage endpoint the page reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum QuotaProvider {
    Anthropic,
    OpenAiCodex,
    GithubCopilot,
}

impl QuotaProvider {
    const ALL: [QuotaProvider; 3] = [
        QuotaProvider::Anthropic,
        QuotaProvider::OpenAiCodex,
        QuotaProvider::GithubCopilot,
    ];

    fn id(self) -> &'static str {
        match self {
            QuotaProvider::Anthropic => "anthropic",
            QuotaProvider::OpenAiCodex => "openai-codex",
            QuotaProvider::GithubCopilot => "github-copilot",
        }
    }
}

/// One provider's usable token, or why the stored sign-in cannot open its usage endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QuotaCredential {
    Token {
        provider: QuotaProvider,
        token: String,
        enterprise_domain: Option<String>,
    },
    Unusable {
        provider: QuotaProvider,
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
            origin_override: std::env::var(QUOTA_BASE_URL_ENV)
                .ok()
                .filter(|origin| !origin.trim().is_empty()),
            // The transport's own bound plus slack: the outer timeout only fires on a transport
            // that ignores its bound.
            timeout: Duration::from_millis(QUOTA_REQUEST_TIMEOUT_MS + 2_000),
            http: Arc::new(ReqwestProviderHttp::new()),
        }
    }
}

type QuotaCache = Mutex<HashMap<(PathBuf, QuotaProvider), ProviderQuota>>;

fn quota_cache() -> &'static QuotaCache {
    static CACHE: OnceLock<QuotaCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The stored subscription credentials, each with a usable token. Blocking: the auth store reads
/// its file and refreshes an expired OAuth token synchronously.
fn quota_credentials(agent_dir: &Path) -> Vec<QuotaCredential> {
    let mut auth = AuthStorage::create(agent_dir);
    let stored = auth.get_all();
    let mut credentials = Vec::new();
    for provider in QuotaProvider::ALL {
        let Some(credential) = stored.credential(provider.id()) else {
            continue;
        };
        let oauth = matches!(credential, AuthCredential::Oauth { .. });
        let credential = match provider {
            QuotaProvider::Anthropic | QuotaProvider::OpenAiCodex => {
                // A Claude OAuth token pasted into the API-key slot opens the usage route too; a
                // plain API key has no subscription quota to show.
                let api_key_is_oauth = match &credential {
                    AuthCredential::ApiKey { key, .. } => key.starts_with("sk-ant-oat"),
                    AuthCredential::McpStaticToken { .. } | AuthCredential::Oauth { .. } => false,
                };
                let usable = oauth || (provider == QuotaProvider::Anthropic && api_key_is_oauth);
                if !usable {
                    continue;
                }
                match auth.get_api_key(provider.id()) {
                    Some(token) => QuotaCredential::Token {
                        provider,
                        token,
                        enterprise_domain: None,
                    },
                    None => QuotaCredential::Unusable {
                        provider,
                        reason: "the stored sign-in has expired; run /login".to_string(),
                    },
                }
            }
            QuotaProvider::GithubCopilot => match credential {
                // The GitHub OAuth token is the refresh token: the Copilot API token in
                // `access` does not open the GitHub REST route.
                AuthCredential::Oauth {
                    refresh: Some(token),
                    enterprise_url,
                    ..
                } => QuotaCredential::Token {
                    provider,
                    token,
                    enterprise_domain: enterprise_url,
                },
                AuthCredential::Oauth { refresh: None, .. }
                | AuthCredential::ApiKey { .. }
                | AuthCredential::McpStaticToken { .. } => continue,
            },
        };
        credentials.push(credential);
    }
    credentials
}

/// Fetch one provider's card under the configured bound.
async fn fetch_card(
    config: &QuotaFetchConfig,
    provider: QuotaProvider,
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
    let credentials = tokio::task::spawn_blocking(move || quota_credentials(&credential_dir))
        .await
        .unwrap_or_default();
    let fetches = credentials.into_iter().map(|credential| {
        let agent_dir = agent_dir.clone();
        async move {
            let (provider, token, enterprise_domain) = match credential {
                QuotaCredential::Token {
                    provider,
                    token,
                    enterprise_domain,
                } => (provider, token, enterprise_domain),
                QuotaCredential::Unusable { provider, reason } => {
                    return ProviderQuota {
                        provider: provider.id().to_string(),
                        plan: None,
                        source: QuotaSource::Provider,
                        fetched_at_ms: now_ms,
                        state: QuotaState::Failed { error: reason },
                    };
                }
            };
            let key = (agent_dir, provider);
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
    }
}
