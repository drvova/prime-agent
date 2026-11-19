//! Bounded HTTP for `web.read`: redirects followed by hand (every hop is
//! scheme-checked, counted, and the last one becomes the page's final URL),
//! a body cap per kind of fetch, and the loopback rewrite the tests use.

use std::time::Duration;

use anyhow::{anyhow, bail};
use url::Url;

/// Hops followed before a read gives up.
const MAX_REDIRECTS: usize = 8;
/// The user agent every read sends (GitHub's API refuses requests without one).
const USER_AGENT: &str =
    "Mozilla/5.0 (compatible; prime-agent web.read; +https://github.com/PrimeIntellect-ai/prime-agent)";

/// What a fetch asks for: the `Accept` header it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Accept {
    /// A page or document of any kind (HTML preferred).
    Document,
    /// A JSON API.
    Json,
    /// A PDF.
    Pdf,
}

impl Accept {
    fn header(self) -> &'static str {
        match self {
            Accept::Document => {
                "text/html,application/xhtml+xml,application/pdf;q=0.9,text/plain;q=0.8,*/*;q=0.5"
            }
            Accept::Json => "application/json",
            Accept::Pdf => "application/pdf,*/*;q=0.5",
        }
    }

    /// Largest body read: PDFs get room for a long paper, everything else less.
    fn body_cap(self) -> usize {
        match self {
            Accept::Document | Accept::Json => 8 * 1024 * 1024,
            Accept::Pdf => 32 * 1024 * 1024,
        }
    }
}

/// One finished fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fetched {
    /// The URL after redirects (the citable one).
    pub(crate) final_url: Url,
    /// The `Content-Type` header's media type, lowercased, without parameters.
    pub(crate) media_type: Option<String>,
    pub(crate) body: Vec<u8>,
}

/// A fetch that got a non-2xx answer: its status lets a site route fall
/// through (an arXiv paper without an HTML rendering answers 404).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpStatusError {
    pub(crate) status: u16,
    pub(crate) url: String,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {} from {}", self.status, self.url)
    }
}

impl std::error::Error for HttpStatusError {}

/// The shared client plus the read's deadline and the test rewrite.
pub(crate) struct Fetcher<'a> {
    pub(crate) client: &'a reqwest::Client,
    /// Tests route `https://host/path?q` to `{base}/host/path?q`.
    pub(crate) base_override: Option<&'a str>,
    pub(crate) timeout: Duration,
}

impl Fetcher<'_> {
    /// The URL the request really goes to.
    fn wire_url(&self, url: &Url) -> String {
        match self.base_override {
            Some(base) => {
                let mut wire = format!(
                    "{}/{}{}",
                    base.trim_end_matches('/'),
                    url.host_str().unwrap_or_default(),
                    url.path()
                );
                if let Some(query) = url.query() {
                    wire.push('?');
                    wire.push_str(query);
                }
                wire
            }
            None => url.as_str().to_string(),
        }
    }

    /// GET `url`, following redirects, and read the body up to the kind's cap.
    ///
    /// # Errors
    ///
    /// A transport failure, a redirect loop or a non-http(s) hop, a body over
    /// the cap, or a non-2xx answer ([`HttpStatusError`] in the chain).
    #[tracing::instrument(skip_all, fields(host = url.host_str().unwrap_or_default()))]
    pub(crate) async fn get(&self, url: &Url, accept: Accept) -> anyhow::Result<Fetched> {
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            let response = self
                .client
                .get(self.wire_url(&current))
                .header(reqwest::header::ACCEPT, accept.header())
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
                .timeout(self.timeout)
                .send()
                .await
                .map_err(|error| transport_error(&current, &error))?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        anyhow!("HTTP {} from {current} without a Location", status.as_u16())
                    })?;
                let next = current
                    .join(location)
                    .map_err(|_| anyhow!("{current} redirected to an invalid URL: {location}"))?;
                if !matches!(next.scheme(), "http" | "https") {
                    bail!("{current} redirected to a non-http URL: {next}");
                }
                current = next;
                continue;
            }
            if !status.is_success() {
                return Err(HttpStatusError {
                    status: status.as_u16(),
                    url: current.to_string(),
                }
                .into());
            }
            let media_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty());
            let cap = accept.body_cap();
            if response
                .content_length()
                .is_some_and(|length| length > cap as u64)
            {
                bail!(
                    "{current} is larger than the {} MiB read cap",
                    cap / (1024 * 1024)
                );
            }
            let mut response = response;
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| transport_error(&current, &error))?
            {
                if body.len() + chunk.len() > cap {
                    bail!(
                        "{current} is larger than the {} MiB read cap",
                        cap / (1024 * 1024)
                    );
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(Fetched {
                final_url: current,
                media_type,
                body,
            });
        }
        bail!("{url} redirected more than {MAX_REDIRECTS} times")
    }
}

fn transport_error(url: &Url, error: &reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow!("{url} timed out")
    } else {
        anyhow!("{url} request failed: {error}")
    }
}

/// The status of a fetch failure, when the server answered at all.
pub(crate) fn failed_status(error: &anyhow::Error) -> Option<u16> {
    error
        .downcast_ref::<HttpStatusError>()
        .map(|error| error.status)
}
