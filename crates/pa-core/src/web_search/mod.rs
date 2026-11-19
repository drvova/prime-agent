//! Native web search: one engine, shared by every kernel, session, and
//! subagent of the process, that the Python REPL reaches through the
//! `web.search` host request (`rlm.web.search(...)` in the kernel, and the
//! bundled websearch skill on top of it).
//!
//! The design: a provider catalog
//! ([`ProviderId`]) over keyed APIs and keyless engines, the `auto` chain that
//! falls through a provider that has no key, errors, or returns nothing, the
//! structured query parse that maps `site:`/date directives onto native
//! filters and post-filters results leniently, and one normalized result
//! shape ([`SearchReport`]) with a model-readable rendering.
//!
//! Nothing here runs at startup: the HTTP client is built on the first
//! search, and searches run only when the kernel asks.

mod ai_search;
mod catalog;
pub(crate) mod dates;
mod host;
pub(crate) mod http;
mod open_indexes;
mod query;
mod scrapers;
mod serp_apis;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

pub use catalog::{ProviderAccess, ProviderId, WebSearchProvider, KEYED_CREDENTIALS};
pub use host::{SearchObserver, WebSearchHost};

use http::{ProviderCall, ProviderResult};

/// Default per-request transport timeout (the websearch skill's historical
/// `PRIME_AGENT_WEBSEARCH_TIMEOUT` default).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);
/// Upper bound on a caller-supplied timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_mins(5);
/// A failure reason quoted in a fallback note is capped at this many chars.
const MAX_REASON_CHARS: usize = 240;

/// A pure time filter on results; providers without one ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    /// The window parsed from `day`/`week`/`month`/`year`.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "day" => Some(Recency::Day),
            "week" => Some(Recency::Week),
            "month" => Some(Recency::Month),
            "year" => Some(Recency::Year),
            _ => None,
        }
    }

    /// The window's name, as Tavily, Mojeek, Perplexity, and `SearXNG` take it.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        }
    }

    /// The window in days (month = 30, year = 365).
    pub(crate) fn days(self) -> i64 {
        match self {
            Recency::Day => 1,
            Recency::Week => 7,
            Recency::Month => 30,
            Recency::Year => 365,
        }
    }
}

/// One search result, normalized across providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSource {
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// ISO date or the provider's relative age (`2 days ago`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// A cited URL of an answer-synthesizing provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchCitation {
    pub url: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cited_text: Option<String>,
}

/// One provider's normalized answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResponse {
    /// A synthesized answer (Tavily, Exa, Perplexity, Kagi, Serper's
    /// knowledge graph, `SearXNG` answers).
    pub answer: Option<String>,
    pub sources: Vec<SearchSource>,
    pub citations: Vec<SearchCitation>,
    pub related_questions: Vec<String>,
}

impl SearchResponse {
    pub(crate) fn from_sources(sources: Vec<SearchSource>) -> Self {
        Self {
            sources,
            ..Self::default()
        }
    }
}

/// One search to run.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    /// `auto` walks the chain; an engine pins that engine alone.
    pub provider: WebSearchProvider,
    /// Results wanted (clamped per provider); `None` takes its default.
    pub num_results: Option<usize>,
    pub recency: Option<Recency>,
    /// Per-request transport timeout.
    pub timeout: Duration,
}

/// A provider that failed before another answered (or the search gave up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub provider: ProviderId,
    /// Flattened and capped, as the fallback note quotes it.
    pub reason: String,
    /// HTTP-style status (204: answered with nothing).
    pub status: Option<u16>,
}

/// A successful search: who answered, what failed first, and the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport {
    pub provider: ProviderId,
    pub response: SearchResponse,
    /// Providers that failed first, in chain order (the fallback depth).
    pub failures: Vec<ProviderFailure>,
    /// Leading notes: the fallback summary and relaxed query constraints.
    pub notes: Vec<String>,
    pub elapsed: Duration,
}

/// Every candidate failed (or the selected provider is unusable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchError {
    pub message: String,
    pub failures: Vec<ProviderFailure>,
    pub elapsed: Duration,
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SearchError {}

/// Resolved access for the keyed providers and the `SearXNG` instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    keys: BTreeMap<ProviderId, String>,
    searxng_endpoint: Option<String>,
}

impl Credentials {
    /// Resolve every provider's access the way the product stores it: a
    /// nonblank environment variable (`SERPER_API_KEY`, ..., `SEARXNG_ENDPOINT`)
    /// wins over the auth-store slot named by the provider id (an api-key
    /// credential, its value a literal, an env-var name, or a `!command`).
    /// Blocking: a `!command` key runs its command.
    #[must_use]
    pub fn resolve(auth: &crate::auth::AuthStorageData) -> Self {
        let mut credentials = Self::default();
        for provider in ProviderId::ALL {
            match provider.access() {
                ProviderAccess::ApiKey { env, .. } => {
                    let ambient = std::env::var(env)
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty());
                    let key = ambient.or_else(|| match auth.credential(provider.id()) {
                        Some(crate::auth::AuthCredential::ApiKey { key, .. }) => {
                            crate::auth::resolve_config_value::resolve_config_value(&key)
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty())
                        }
                        Some(_) | None => None,
                    });
                    if let Some(key) = key {
                        credentials.keys.insert(provider, key);
                    }
                }
                ProviderAccess::Endpoint { env } => {
                    credentials.searxng_endpoint = std::env::var(env)
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty());
                }
                ProviderAccess::Keyless => {}
            }
        }
        credentials
    }

    /// Add (or replace) a provider's API key.
    #[must_use]
    pub fn with_key(mut self, provider: ProviderId, key: impl Into<String>) -> Self {
        self.keys.insert(provider, key.into());
        self
    }

    /// The provider's access: its key, the `SearXNG` URL, or `None`
    /// (keyless providers need none; a missing one makes keyed ones unusable).
    fn access(&self, provider: ProviderId) -> Option<&str> {
        match provider.access() {
            ProviderAccess::ApiKey { .. } => self.keys.get(&provider).map(String::as_str),
            ProviderAccess::Endpoint { .. } => self.searxng_endpoint.as_deref(),
            ProviderAccess::Keyless => None,
        }
    }

    fn is_usable(&self, provider: ProviderId) -> bool {
        match provider.access() {
            ProviderAccess::ApiKey { .. } | ProviderAccess::Endpoint { .. } => {
                self.access(provider).is_some()
            }
            ProviderAccess::Keyless => true,
        }
    }
}

/// The search engine: a lazily built HTTP client plus the provider chain.
pub struct WebSearchEngine {
    client: OnceLock<reqwest::Client>,
    /// Tests route every provider to `{base}/{provider id}{production path}`.
    base_override: Option<String>,
}

impl WebSearchEngine {
    /// The process-wide engine every session's `web.search` handler shares.
    #[must_use]
    pub fn shared() -> Arc<WebSearchEngine> {
        static SHARED: OnceLock<Arc<WebSearchEngine>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| {
            Arc::new(WebSearchEngine {
                client: OnceLock::new(),
                base_override: None,
            })
        }))
    }

    /// An engine whose providers all answer from `base` (a loopback test
    /// server), bypassing any proxy.
    #[cfg(test)]
    pub(crate) fn for_test_server(base: &str) -> Self {
        let engine = WebSearchEngine {
            client: OnceLock::new(),
            base_override: Some(base.to_string()),
        };
        let _ = engine.client.set(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("test client"),
        );
        engine
    }

    fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| {
            pa_ai::utils::tls::http_client_builder()
                .connect_timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default()
        })
    }

    /// Run one search: the selected engine, or the `auto` chain (configured
    /// keyed providers, a configured `SearXNG`, then the keyless engines;
    /// Hacker News first for queries scoped to it). A provider without its
    /// key is skipped; one that errors or answers nothing is recorded and the
    /// chain moves on.
    ///
    /// # Errors
    ///
    /// [`SearchError`] when the selected provider is unusable or every
    /// candidate failed; its `failures` list them in chain order.
    #[tracing::instrument(skip_all, fields(provider = request.provider.id()))]
    pub async fn search(
        &self,
        request: &SearchRequest,
        credentials: &Credentials,
    ) -> Result<SearchReport, SearchError> {
        let started = Instant::now();
        let parsed = query::parse_search_query(&request.query);
        let candidates = match request.provider {
            WebSearchProvider::Engine(provider) => {
                if !credentials.is_usable(provider) {
                    // What to tell the user when the selected provider cannot run.
                    let message = match provider.access() {
                        ProviderAccess::ApiKey {
                            credential_name,
                            signup_url,
                            ..
                        } => format!(
                            "Web search is not set up yet: no {label} API key is configured.\n\
                             Tell the user how to enable it:\n  \
                             1. Get an API key at {signup_url} (sign up, copy the key).\n  \
                             2. In Prime Agent, run /mcp, choose \"{credential_name}\", and paste the key.\n\
                             Or choose the keyless `auto` provider in /settings. Do not ask the user to set \
                             environment variables.",
                            label = provider.label()
                        ),
                        ProviderAccess::Endpoint { env } => format!(
                            "{} needs its instance URL in {env}; choose `auto` in /settings for keyless search.",
                            provider.label()
                        ),
                        ProviderAccess::Keyless => format!("{} is unavailable.", provider.label()),
                    };
                    return Err(SearchError {
                        message,
                        failures: Vec::new(),
                        elapsed: started.elapsed(),
                    });
                }
                vec![provider]
            }
            WebSearchProvider::Auto => {
                let hn_only = !parsed.sites.is_empty()
                    && parsed
                        .sites
                        .iter()
                        .all(|site| site.split('/').next() == Some("news.ycombinator.com"));
                hn_only
                    .then_some(ProviderId::Hackernews)
                    .into_iter()
                    .chain(ProviderId::AUTO_ORDER)
                    .filter(|provider| credentials.is_usable(*provider))
                    .collect()
            }
        };
        let mut failures = Vec::new();
        for provider in candidates {
            let call = ProviderCall {
                provider,
                client: self.client(),
                base_override: self.base_override.as_deref(),
                query: &parsed,
                num_results: request.num_results,
                recency: request.recency,
                timeout: request.timeout,
                access: credentials.access(provider),
            };
            match dispatch(&call).await {
                Ok(mut response) => {
                    // The lenient constraint pass over whatever the provider
                    // returned; relaxed dimensions become notes.
                    let mut constraint_notes = Vec::new();
                    if parsed.has_constraints() && !response.sources.is_empty() {
                        let (kept, dropped) = query::apply_query_constraints(
                            std::mem::take(&mut response.sources),
                            &parsed,
                        );
                        response.sources = kept;
                        constraint_notes.extend(dropped.into_iter().map(|label| {
                            format!("no results matched `{label}`; the constraint was relaxed")
                        }));
                    }
                    let renderable = response.answer.is_some()
                        || !response.sources.is_empty()
                        || !response.citations.is_empty()
                        || !response.related_questions.is_empty();
                    if !renderable {
                        failures.push(ProviderFailure {
                            provider,
                            reason: format!("{} returned no results", provider.label()),
                            status: Some(204),
                        });
                        continue;
                    }
                    let mut notes = Vec::new();
                    if !failures.is_empty() {
                        let failed: Vec<String> = failures
                            .iter()
                            .map(|failure: &ProviderFailure| {
                                format!("{} failed ({})", failure.provider.label(), failure.reason)
                            })
                            .collect();
                        notes.push(format!(
                            "answered by {} after {}",
                            provider.label(),
                            failed.join(" and ")
                        ));
                    }
                    notes.append(&mut constraint_notes);
                    return Ok(SearchReport {
                        provider,
                        response,
                        failures,
                        notes,
                        elapsed: started.elapsed(),
                    });
                }
                Err(error) => {
                    let flattened = http::normalize_text(&error.message).unwrap_or_default();
                    failures.push(ProviderFailure {
                        provider,
                        reason: http::truncate_chars(
                            flattened.trim_end_matches('.'),
                            MAX_REASON_CHARS,
                        ),
                        status: error.status,
                    });
                }
            }
        }
        let message = match failures.as_slice() {
            [] => "No web search provider is available.".to_string(),
            [only] => format!("{} failed: {}", only.provider.label(), only.reason),
            many => format!(
                "All web search providers failed: {}",
                many.iter()
                    .map(|failure| format!("{}: {}", failure.provider.id(), failure.reason))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        };
        Err(SearchError {
            message,
            failures,
            elapsed: started.elapsed(),
        })
    }
}

/// The provider's own search.
#[tracing::instrument(skip_all, fields(provider = call.provider.id()))]
pub(crate) async fn dispatch(call: &ProviderCall<'_>) -> ProviderResult<SearchResponse> {
    match call.provider {
        ProviderId::Serper => serp_apis::serper(call).await,
        ProviderId::Eco => serp_apis::eco(call).await,
        ProviderId::Brave => serp_apis::brave(call).await,
        ProviderId::Kagi => serp_apis::kagi(call).await,
        ProviderId::Tavily => ai_search::tavily(call).await,
        ProviderId::Exa => ai_search::exa(call).await,
        ProviderId::Perplexity => ai_search::perplexity(call).await,
        ProviderId::Parallel => ai_search::parallel(call).await,
        ProviderId::Jina => ai_search::jina(call).await,
        ProviderId::Firecrawl => ai_search::firecrawl(call).await,
        ProviderId::Searxng => open_indexes::searxng(call).await,
        ProviderId::Hackernews => open_indexes::hackernews(call).await,
        ProviderId::Duckduckgo => scrapers::duckduckgo(call).await,
        ProviderId::Startpage => scrapers::startpage(call).await,
        ProviderId::Mojeek => scrapers::mojeek(call).await,
        ProviderId::Ecosia => scrapers::ecosia(call).await,
        ProviderId::Public => Box::pin(scrapers::public(call)).await,
    }
}

/// `n noun(s)`.
fn count_of(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

impl SearchReport {
    /// The model-readable rendering: notes, the
    /// answer, numbered sources, citations, and related questions.
    #[must_use]
    pub fn text(&self) -> String {
        let response = &self.response;
        let mut parts: Vec<String> = self
            .notes
            .iter()
            .map(|note| format!("Note: {note}"))
            .collect();
        if let Some(answer) = &response.answer {
            parts.push(answer.clone());
            if !response.sources.is_empty() {
                parts.push("\n## Sources".to_string());
                parts.push(count_of(response.sources.len(), "source"));
            }
        }
        for (i, source) in response.sources.iter().enumerate() {
            let age = source
                .published_date
                .as_ref()
                .map(|date| format!(" ({date})"))
                .unwrap_or_default();
            parts.push(format!(
                "[{}] {}{age}\n    {}",
                i + 1,
                source.title,
                source.url
            ));
            if let Some(snippet) = &source.snippet {
                parts.push(format!("    {}", http::truncate_chars(snippet, 240)));
            }
        }
        if !response.citations.is_empty() {
            parts.push("\n## Citations".to_string());
            parts.push(count_of(response.citations.len(), "citation"));
            for (i, citation) in response.citations.iter().enumerate() {
                parts.push(format!(
                    "[{}] {}\n    {}",
                    i + 1,
                    citation.title,
                    citation.url
                ));
                if let Some(text) = &citation.cited_text {
                    parts.push(format!("    {}", http::truncate_chars(text, 240)));
                }
            }
        }
        if !response.related_questions.is_empty() {
            parts.push("\n## Related".to_string());
            parts.push(count_of(response.related_questions.len(), "question"));
            parts.extend(response.related_questions.iter().map(|q| format!("- {q}")));
        }
        parts.join("\n")
    }

    /// The host-request reply: the structured result plus its rendering.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "provider": self.provider.id(),
            "label": self.provider.label(),
            "answer": self.response.answer,
            "sources": self.response.sources,
            "citations": self.response.citations,
            "relatedQuestions": self.response.related_questions,
            "failures": self.failures.iter().map(|failure| json!({
                "provider": failure.provider.id(),
                "label": failure.provider.label(),
                "reason": failure.reason,
                "status": failure.status,
            })).collect::<Vec<_>>(),
            "notes": self.notes,
            "text": self.text(),
        })
    }
}
