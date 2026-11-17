//! The provider catalog: every engine the native search can query, how each
//! one authenticates, the `auto` chain's order, and the settings selection.
//!
//! Ids are wire identifiers: the `webSearchProvider` settings value, the
//! auth-store slot of a keyed provider (auth.json's `serper`, `eco`, ...),
//! the `web.search(provider=...)` argument, and the kernel's
//! `PRIME_AGENT_WEBSEARCH_PROVIDER` value. Never rename one.

use serde::{Deserialize, Serialize};

/// One search engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProviderId {
    /// Serper (Google results).
    Serper,
    /// You.com Eco Search.
    Eco,
    Brave,
    Tavily,
    Exa,
    Kagi,
    /// Perplexity's Sonar API (an answer plus its sources).
    Perplexity,
    Parallel,
    /// Jina Reader search (`s.jina.ai`).
    Jina,
    Firecrawl,
    /// A self-hosted `SearXNG` instance (`SEARXNG_ENDPOINT`).
    Searxng,
    Duckduckgo,
    Startpage,
    Mojeek,
    Ecosia,
    /// The Hacker News Search API (Algolia), keyless.
    Hackernews,
    /// Every keyless HTML engine at once, merged by cross-engine consensus.
    Public,
}

/// How a provider gets the access it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAccess {
    /// An API key: the nonblank `env` variable wins over the auth-store slot
    /// named by the provider id; `credential_name` labels the `/mcp` row.
    ApiKey {
        env: &'static str,
        credential_name: &'static str,
        signup_url: &'static str,
    },
    /// A configured instance URL (`SearXNG`: `SEARXNG_ENDPOINT`).
    Endpoint { env: &'static str },
    /// Public: no key, no configuration.
    Keyless,
}

impl ProviderId {
    /// Every provider, in catalog order.
    pub const ALL: [ProviderId; 17] = [
        ProviderId::Serper,
        ProviderId::Eco,
        ProviderId::Brave,
        ProviderId::Tavily,
        ProviderId::Exa,
        ProviderId::Kagi,
        ProviderId::Perplexity,
        ProviderId::Parallel,
        ProviderId::Jina,
        ProviderId::Firecrawl,
        ProviderId::Searxng,
        ProviderId::Duckduckgo,
        ProviderId::Startpage,
        ProviderId::Mojeek,
        ProviderId::Ecosia,
        ProviderId::Hackernews,
        ProviderId::Public,
    ];

    /// The `auto` chain: configured keyed providers first (in this order),
    /// then a configured `SearXNG`, then the keyless engines. Hacker News
    /// leads only for queries scoped to it; the Public aggregate is
    /// explicit-only (fanning out to every scraper is a deliberate choice).
    pub(crate) const AUTO_ORDER: [ProviderId; 15] = [
        ProviderId::Serper,
        ProviderId::Eco,
        ProviderId::Brave,
        ProviderId::Tavily,
        ProviderId::Exa,
        ProviderId::Kagi,
        ProviderId::Perplexity,
        ProviderId::Parallel,
        ProviderId::Jina,
        ProviderId::Firecrawl,
        ProviderId::Searxng,
        ProviderId::Duckduckgo,
        ProviderId::Startpage,
        ProviderId::Mojeek,
        ProviderId::Ecosia,
    ];

    /// The wire id (settings value, auth slot, `provider=` argument).
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            ProviderId::Serper => crate::auth::SERPER_CREDENTIAL_ID,
            ProviderId::Eco => crate::auth::ECO_CREDENTIAL_ID,
            ProviderId::Brave => "brave",
            ProviderId::Tavily => "tavily",
            ProviderId::Exa => "exa",
            ProviderId::Kagi => "kagi",
            ProviderId::Perplexity => "perplexity",
            ProviderId::Parallel => "parallel",
            ProviderId::Jina => "jina",
            ProviderId::Firecrawl => "firecrawl",
            ProviderId::Searxng => "searxng",
            ProviderId::Duckduckgo => "duckduckgo",
            ProviderId::Startpage => "startpage",
            ProviderId::Mojeek => "mojeek",
            ProviderId::Ecosia => "ecosia",
            ProviderId::Hackernews => "hackernews",
            ProviderId::Public => "public",
        }
    }

    /// The provider parsed from its wire id.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|provider| provider.id() == id)
    }

    /// The display label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ProviderId::Serper => "Serper",
            ProviderId::Eco => "You.com Eco",
            ProviderId::Brave => "Brave",
            ProviderId::Tavily => "Tavily",
            ProviderId::Exa => "Exa",
            ProviderId::Kagi => "Kagi",
            ProviderId::Perplexity => "Perplexity",
            ProviderId::Parallel => "Parallel",
            ProviderId::Jina => "Jina",
            ProviderId::Firecrawl => "Firecrawl",
            ProviderId::Searxng => "SearXNG",
            ProviderId::Duckduckgo => "DuckDuckGo",
            ProviderId::Startpage => "Startpage",
            ProviderId::Mojeek => "Mojeek",
            ProviderId::Ecosia => "Ecosia",
            ProviderId::Hackernews => "Hacker News",
            ProviderId::Public => "Public Web",
        }
    }

    /// How the provider authenticates.
    #[must_use]
    pub fn access(self) -> ProviderAccess {
        let key = |env, credential_name, signup_url| ProviderAccess::ApiKey {
            env,
            credential_name,
            signup_url,
        };
        match self {
            ProviderId::Serper => key(
                "SERPER_API_KEY",
                crate::auth::SERPER_CREDENTIAL_NAME,
                "https://serper.dev",
            ),
            ProviderId::Eco => key(
                "YOU_API_KEY",
                crate::auth::ECO_CREDENTIAL_NAME,
                "https://you.com/platform",
            ),
            ProviderId::Brave => key(
                "BRAVE_API_KEY",
                "Brave Search (web search)",
                "https://brave.com/search/api/",
            ),
            ProviderId::Tavily => key(
                "TAVILY_API_KEY",
                "Tavily (web search)",
                "https://tavily.com",
            ),
            ProviderId::Exa => key("EXA_API_KEY", "Exa (web search)", "https://exa.ai"),
            ProviderId::Kagi => key(
                "KAGI_API_KEY",
                "Kagi (web search)",
                "https://kagi.com/settings/api",
            ),
            ProviderId::Perplexity => key(
                "PERPLEXITY_API_KEY",
                "Perplexity (web search)",
                "https://www.perplexity.ai/settings/api",
            ),
            ProviderId::Parallel => key(
                "PARALLEL_API_KEY",
                "Parallel (web search)",
                "https://platform.parallel.ai",
            ),
            ProviderId::Jina => key("JINA_API_KEY", "Jina (web search)", "https://jina.ai"),
            ProviderId::Firecrawl => key(
                "FIRECRAWL_API_KEY",
                "Firecrawl (web search)",
                "https://firecrawl.dev",
            ),
            ProviderId::Searxng => ProviderAccess::Endpoint {
                env: "SEARXNG_ENDPOINT",
            },
            ProviderId::Duckduckgo
            | ProviderId::Startpage
            | ProviderId::Mojeek
            | ProviderId::Ecosia
            | ProviderId::Hackernews
            | ProviderId::Public => ProviderAccess::Keyless,
        }
    }
}

/// The `/mcp` view's api-key rows for the keyed providers: (auth slot, label).
pub const KEYED_CREDENTIALS: &[(&str, &str)] = &[
    ("serper", crate::auth::SERPER_CREDENTIAL_NAME),
    ("eco", crate::auth::ECO_CREDENTIAL_NAME),
    ("brave", "Brave Search (web search)"),
    ("tavily", "Tavily (web search)"),
    ("exa", "Exa (web search)"),
    ("kagi", "Kagi (web search)"),
    ("perplexity", "Perplexity (web search)"),
    ("parallel", "Parallel (web search)"),
    ("jina", "Jina (web search)"),
    ("firecrawl", "Firecrawl (web search)"),
];

/// `webSearchProvider`: the provider `web.search()` and the bundled
/// websearch skill use when the caller names none. `auto` (the default)
/// walks the fallback chain; an engine id pins that engine alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum WebSearchProvider {
    #[default]
    Auto,
    Engine(ProviderId),
}

impl WebSearchProvider {
    /// Every selectable value, `auto` first (the settings picker's order).
    #[must_use]
    pub fn all() -> Vec<WebSearchProvider> {
        std::iter::once(WebSearchProvider::Auto)
            .chain(ProviderId::ALL.into_iter().map(WebSearchProvider::Engine))
            .collect()
    }

    /// The settings value (`auto` or an engine id).
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            WebSearchProvider::Auto => "auto",
            WebSearchProvider::Engine(provider) => provider.id(),
        }
    }

    /// The selection parsed from its settings value.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "auto" => Some(WebSearchProvider::Auto),
            other => ProviderId::from_id(other).map(WebSearchProvider::Engine),
        }
    }
}

impl From<WebSearchProvider> for String {
    fn from(provider: WebSearchProvider) -> Self {
        provider.id().to_string()
    }
}

impl TryFrom<String> for WebSearchProvider {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        WebSearchProvider::from_id(&value)
            .ok_or_else(|| format!("unknown web search provider \"{value}\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_credential_rows_match_the_catalog() {
        let keyed: Vec<(&str, &str)> = ProviderId::ALL
            .into_iter()
            .filter_map(|provider| match provider.access() {
                ProviderAccess::ApiKey {
                    credential_name, ..
                } => Some((provider.id(), credential_name)),
                ProviderAccess::Endpoint { .. } | ProviderAccess::Keyless => None,
            })
            .collect();
        assert_eq!(keyed, KEYED_CREDENTIALS);
    }

    #[test]
    fn settings_values_round_trip_and_legacy_ids_parse() {
        let parsed: Vec<WebSearchProvider> =
            serde_json::from_value(serde_json::json!(["serper", "eco", "auto", "duckduckgo"]))
                .expect("parse");
        assert_eq!(
            parsed,
            vec![
                WebSearchProvider::Engine(ProviderId::Serper),
                WebSearchProvider::Engine(ProviderId::Eco),
                WebSearchProvider::Auto,
                WebSearchProvider::Engine(ProviderId::Duckduckgo),
            ]
        );
        assert_eq!(
            serde_json::to_value(WebSearchProvider::Engine(ProviderId::Eco)).expect("serialize"),
            serde_json::json!("eco")
        );
        assert!(serde_json::from_value::<WebSearchProvider>(serde_json::json!("bing")).is_err());
    }
}
