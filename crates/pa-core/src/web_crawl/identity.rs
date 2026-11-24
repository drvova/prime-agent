//! What a crawl presents to a site, from the user's `web.crawl` settings
//! (global scope only, read at each crawl, never from the model): whether
//! robots.txt is obeyed, the user agent (Prime Agent's own, a custom one, or
//! a different real browser's on each request), extra headers and cookies
//! for the crawled site's hosts, proxies (each request takes the next), and
//! a rendered crawl's stealth and fingerprint. Unset, nothing changes:
//! robots.txt is obeyed and the crawl says it is Prime Agent.

use anyhow::{anyhow, bail, Context};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use spider::website::Website;
use url::Url;

use super::{Render, USER_AGENT};
use crate::settings::WebCrawlSettings;

/// Proxy schemes a crawl accepts.
const PROXY_SCHEMES: [&str; 4] = ["http", "https", "socks5", "socks5h"];

/// The user agent a crawl sends.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UserAgent {
    /// Prime Agent's own.
    Honest,
    Custom(String),
    /// A different real browser's on each request.
    Rotating,
}

/// The identity one crawl runs with.
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    respect_robots: bool,
    user_agent: UserAgent,
    /// The extra headers, without the cookies.
    headers: HeaderMap,
    cookies: Option<String>,
    /// The extra headers with the cookies as a `Cookie` header: what a
    /// plain request to the site's hosts adds.
    pub(crate) site_headers: HeaderMap,
    proxies: Vec<String>,
    stealth: bool,
    fingerprint: bool,
}

impl Default for Identity {
    fn default() -> Self {
        Identity {
            respect_robots: true,
            user_agent: UserAgent::Honest,
            headers: HeaderMap::new(),
            cookies: None,
            site_headers: HeaderMap::new(),
            proxies: Vec::new(),
            stealth: false,
            fingerprint: false,
        }
    }
}

/// What the user's `web.crawl` settings change from the honest defaults, in
/// words (the environment section's line); empty when nothing is changed.
pub(crate) fn changes(settings: &WebCrawlSettings) -> Vec<&'static str> {
    let set = |value: &Option<bool>| *value == Some(true);
    [
        (
            settings.respect_robots_txt == Some(false),
            "robots.txt ignored",
        ),
        (settings.user_agent.is_some(), "a custom user agent"),
        (
            settings.user_agent.is_none() && set(&settings.rotate_user_agent),
            "a rotating browser user agent",
        ),
        (
            settings
                .headers
                .as_ref()
                .is_some_and(|headers| !headers.is_empty()),
            "extra headers",
        ),
        (
            settings
                .cookies
                .as_ref()
                .is_some_and(|cookies| !cookies.trim().is_empty()),
            "cookies",
        ),
        (
            settings
                .proxies
                .as_ref()
                .is_some_and(|proxies| !proxies.is_empty()),
            "proxies",
        ),
        (set(&settings.stealth), "stealth"),
        (set(&settings.fingerprint), "a browser fingerprint"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect()
}

impl Identity {
    /// The identity the user's settings ask for.
    ///
    /// # Errors
    ///
    /// A setting that is not a valid header, cookie or proxy; the error names
    /// it, so the user can fix it.
    pub(crate) fn from_settings(settings: &WebCrawlSettings) -> anyhow::Result<Identity> {
        let header_value = |key: &str, value: &str| {
            HeaderValue::from_str(value)
                .map_err(|_| anyhow!("the web.crawl.{key} setting is not a valid header value"))
        };
        let user_agent = match (&settings.user_agent, settings.rotate_user_agent) {
            (Some(custom), _) => {
                header_value("userAgent", custom)?;
                UserAgent::Custom(custom.clone())
            }
            (None, Some(true)) => UserAgent::Rotating,
            (None, Some(false) | None) => UserAgent::Honest,
        };
        let mut headers = HeaderMap::new();
        for (name, value) in settings.headers.iter().flatten() {
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("the web.crawl.headers setting names {name:?}"))?;
            if name == reqwest::header::HOST || name == reqwest::header::COOKIE {
                bail!("the web.crawl.headers setting may not set {name}; cookies go in web.crawl.cookies");
            }
            headers.insert(name, header_value("headers", value)?);
        }
        let cookies = settings
            .cookies
            .as_deref()
            .map(str::trim)
            .filter(|cookies| !cookies.is_empty())
            .map(str::to_string);
        let mut site_headers = headers.clone();
        if let Some(cookies) = &cookies {
            site_headers.insert(reqwest::header::COOKIE, header_value("cookies", cookies)?);
        }
        let proxies = settings
            .proxies
            .iter()
            .flatten()
            .map(|proxy| {
                let parsed = Url::parse(proxy)
                    .map_err(|error| anyhow!("the web.crawl.proxies entry {proxy:?}: {error}"))?;
                if !PROXY_SCHEMES.contains(&parsed.scheme()) {
                    bail!(
                        "the web.crawl.proxies entry {proxy:?} must be one of {}",
                        PROXY_SCHEMES
                            .map(|scheme| format!("{scheme}://"))
                            .join(", ")
                    );
                }
                Ok(proxy.clone())
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Identity {
            respect_robots: settings.respect_robots_txt.unwrap_or(true),
            user_agent,
            headers,
            cookies,
            site_headers,
            proxies,
            stealth: settings.stealth.unwrap_or(false),
            fingerprint: settings.fingerprint.unwrap_or(false),
        })
    }

    /// Set what spider itself presents on `website`: robots.txt, the user
    /// agent it matches robots.txt rules with, and for a rendered crawl the
    /// browser's headers, cookies, proxy, stealth and fingerprint.
    pub(crate) fn apply(&self, website: &mut Website, render: Render) {
        website
            .with_respect_robots_txt(self.respect_robots)
            .with_user_agent(Some(match &self.user_agent {
                UserAgent::Rotating => spider::configuration::get_ua(render == Render::Browser),
                UserAgent::Honest | UserAgent::Custom(_) => self.user_agent(),
            }))
            .with_stealth(self.stealth)
            .with_fingerprint(self.fingerprint);
        match render {
            // A plain crawl's requests are pa-core's own: `site_headers` and
            // `clients` carry these.
            Render::Http => {}
            Render::Browser => {
                if !self.headers.is_empty() {
                    website.with_headers(Some(self.headers.clone()));
                }
                if let Some(cookies) = &self.cookies {
                    website.with_cookies(cookies);
                }
                if !self.proxies.is_empty() {
                    website.with_proxies(Some(self.proxies.clone()));
                }
            }
        }
    }

    /// The clients a plain crawl's requests go out on: one per proxy, taken
    /// in turn, else `direct`.
    ///
    /// # Errors
    ///
    /// A proxy reqwest cannot use, or a client that would not build.
    pub(crate) fn clients(&self, direct: &reqwest::Client) -> anyhow::Result<Vec<reqwest::Client>> {
        if self.proxies.is_empty() {
            return Ok(vec![direct.clone()]);
        }
        self.proxies
            .iter()
            .map(|proxy| {
                let proxy = reqwest::Proxy::all(proxy)
                    .with_context(|| format!("the web.crawl.proxies entry {proxy:?}"))?;
                pa_ai::utils::tls::http_client_builder()
                    .no_proxy()
                    .proxy(proxy)
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(std::time::Duration::from_secs(15))
                    .build()
                    .context("building the crawl's proxied client")
            })
            .collect()
    }

    /// The `User-Agent` of one plain request.
    pub(crate) fn user_agent(&self) -> &str {
        match &self.user_agent {
            UserAgent::Honest => USER_AGENT,
            UserAgent::Custom(custom) => custom,
            UserAgent::Rotating => spider::configuration::get_ua(false),
        }
    }
}
