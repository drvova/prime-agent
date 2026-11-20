//! The per-provider call context and the transport helpers every provider
//! family shares: bounded body reads, quota/auth error classification,
//! browser-profiled HTML fetches, and the text normalizers.

use std::time::Duration;

use serde_json::Value;

use super::catalog::ProviderId;
use super::query::StructuredQuery;
use super::Recency;

/// Largest provider body read; bigger answers are an error, not a parse.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Error bodies quoted in a failure reason are capped at this.
const MAX_ERROR_BYTES: usize = 8 * 1024;

/// A provider's failure: the reason a fallback note quotes and the HTTP-style
/// status it carried (204 marks an empty answer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderError {
    pub(crate) message: String,
    pub(crate) status: Option<u16>,
}

impl ProviderError {
    pub(crate) fn new(message: impl Into<String>, status: Option<u16>) -> Self {
        Self {
            message: message.into(),
            status,
        }
    }
}

pub(crate) type ProviderResult<T> = Result<T, ProviderError>;

/// Everything one provider call reads: the shared client, the endpoint
/// override, the parsed query, and the resolved access.
pub(crate) struct ProviderCall<'a> {
    pub(crate) provider: ProviderId,
    pub(crate) client: &'a reqwest::Client,
    /// Tests route every provider to `{base}/{id}{production path}`.
    pub(crate) base_override: Option<&'a str>,
    pub(crate) query: &'a StructuredQuery,
    pub(crate) num_results: Option<usize>,
    pub(crate) recency: Option<Recency>,
    pub(crate) timeout: Duration,
    /// The API key (keyed providers) or the instance URL (`SearXNG`).
    pub(crate) access: Option<&'a str>,
    /// The scraped engines' limits (the engine instance's own).
    pub(crate) limits: &'a super::engine_limits::EngineLimits,
}

impl ProviderCall<'_> {
    /// `production`, or its path under the test server.
    pub(crate) fn endpoint(&self, production: &str) -> String {
        match self.base_override {
            Some(base) => {
                let path = url::Url::parse(production).map_or_else(
                    |_| String::new(),
                    |url| {
                        let mut path = url.path().to_string();
                        if let Some(query) = url.query() {
                            path.push('?');
                            path.push_str(query);
                        }
                        path
                    },
                );
                format!(
                    "{}/{}{path}",
                    base.trim_end_matches('/'),
                    self.provider.id()
                )
            }
            None => production.to_string(),
        }
    }

    /// The requested result count clamped to `[1, max]`, else `default`.
    pub(crate) fn count(&self, default: usize, max: usize) -> usize {
        self.num_results
            .map_or(default, |count| count.clamp(1, max))
    }

    pub(crate) fn key(&self) -> ProviderResult<&str> {
        self.access.ok_or_else(|| {
            ProviderError::new(
                format!("{} has no API key configured", self.provider.label()),
                Some(401),
            )
        })
    }

    pub(crate) fn label(&self) -> &'static str {
        self.provider.label()
    }

    /// Send `request` with the call's timeout and return (status, body),
    /// classifying non-2xx answers into the provider's error.
    pub(crate) async fn send(&self, request: reqwest::RequestBuilder) -> ProviderResult<String> {
        let (status, body) = self.send_raw(request).await?;
        if !(200..300).contains(&status) {
            return Err(self.http_error(status, &body));
        }
        Ok(body)
    }

    /// Send and parse a JSON body.
    pub(crate) async fn send_json(
        &self,
        request: reqwest::RequestBuilder,
    ) -> ProviderResult<Value> {
        let body = self.send(request).await?;
        serde_json::from_str(&body).map_err(|_| {
            ProviderError::new(
                format!("{} API returned invalid JSON", self.label()),
                Some(502),
            )
        })
    }

    /// Send without status classification: (status, body).
    pub(crate) async fn send_raw(
        &self,
        request: reqwest::RequestBuilder,
    ) -> ProviderResult<(u16, String)> {
        let response = request
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|error| self.transport_error(&error))?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| self.transport_error(&error))?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(ProviderError::new(
                format!("{} response exceeded 2 MiB", self.label()),
                Some(500),
            ));
        }
        Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn transport_error(&self, error: &reqwest::Error) -> ProviderError {
        if error.is_timeout() {
            return ProviderError::new(format!("{} search timed out", self.label()), Some(504));
        }
        ProviderError::new(
            format!("{} request failed: {error}", self.label()),
            Some(503),
        )
    }

    /// Quota/auth signals into compact provider-tagged messages; anything
    /// else quotes the (capped) body.
    pub(crate) fn http_error(&self, status: u16, body: &str) -> ProviderError {
        let id = self.provider.id();
        let lowered = body.to_lowercase();
        let credit = lowered.contains("quota")
            || lowered.contains("insufficient")
            || [
                "credit exhausted",
                "credits exhausted",
                "credit exceeded",
                "credits exceeded",
            ]
            .iter()
            .any(|marker| lowered.contains(marker));
        let message = if credit {
            format!("{id}: credits exhausted")
        } else {
            match status {
                402 => format!("{id}: 402 credits exhausted"),
                401 | 403 => format!(
                    "{} authorization failed ({status}). Check the API key.",
                    self.label()
                ),
                _ => format!(
                    "{} API error ({status}): {}",
                    self.label(),
                    truncate_chars(body.trim(), MAX_ERROR_BYTES)
                ),
            }
        };
        ProviderError::new(message, Some(status))
    }

    /// A browser-profiled HTML fetch (the keyless scrapers): a stable desktop
    /// Chrome navigation fingerprint, plus a same-origin referer when given.
    pub(crate) fn browser_request(
        &self,
        method: reqwest::Method,
        url: &str,
        referer: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut request = self.client.request(method, url);
        for (name, value) in BROWSER_HEADERS {
            request = request.header(*name, *value);
        }
        match referer {
            Some(referer) => request
                .header("Referer", referer)
                .header("Sec-Fetch-Site", "same-origin"),
            None => request.header("Sec-Fetch-Site", "none"),
        }
    }
}

/// A stable desktop Mac Chrome navigation fingerprint, minus
/// `Accept-Encoding` (the client decodes identity bodies only).
const BROWSER_HEADERS: &[(&str, &str)] = &[
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
    ),
    ("Accept-Language", "en-US,en;q=0.9"),
    ("Cache-Control", "max-age=0"),
    (
        "Sec-Ch-Ua",
        "\"Google Chrome\";v=\"149\", \"Chromium\";v=\"149\", \";Not A Brand\";v=\"99\"",
    ),
    ("Sec-Ch-Ua-Mobile", "?0"),
    ("Sec-Ch-Ua-Platform", "\"macOS\""),
    ("Sec-Fetch-Dest", "document"),
    ("Sec-Fetch-Mode", "navigate"),
    ("Sec-Fetch-User", "?1"),
    ("Upgrade-Insecure-Requests", "1"),
    (
        "User-Agent",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
    ),
];

/// At most `max` chars, with an ellipsis when cut.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

/// Collapse whitespace runs; `None` for blank.
pub(crate) fn normalize_text(value: &str) -> Option<String> {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// A JSON string field, whitespace-normalized; `None` when absent or blank.
pub(crate) fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(normalize_text)
}

/// An array field's elements (empty when absent or not an array).
pub(crate) fn array_field<'v>(value: &'v Value, key: &str) -> &'v [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Keep only absolute http(s) URLs.
pub(crate) fn http_url(value: &str) -> Option<String> {
    let url = url::Url::parse(value.trim()).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// Strip tags, decode the entities result pages emit, normalize whitespace.
pub(crate) fn html_text(fragment: &str) -> String {
    let mut stripped = String::with_capacity(fragment.len());
    let mut in_tag = false;
    for ch in fragment.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                stripped.push(' ');
            }
            _ if !in_tag => stripped.push(ch),
            _ => {}
        }
    }
    normalize_text(&decode_entities(&stripped)).unwrap_or_default()
}

/// Decode numeric and the common named HTML entities.
pub(crate) fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        // Byte search: a char boundary need not fall 12 bytes in.
        let Some(end) = tail.as_bytes()[..tail.len().min(12)]
            .iter()
            .position(|byte| *byte == b';')
        else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity.to_ascii_lowercase().as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "ndash" => Some('\u{2013}'),
            "mdash" => Some('\u{2014}'),
            "hellip" => Some('\u{2026}'),
            "lsquo" => Some('\u{2018}'),
            "rsquo" => Some('\u{2019}'),
            "ldquo" => Some('\u{201c}'),
            "rdquo" => Some('\u{201d}'),
            "laquo" => Some('\u{ab}'),
            "raquo" => Some('\u{bb}'),
            "middot" => Some('\u{b7}'),
            "bull" => Some('\u{2022}'),
            "copy" => Some('\u{a9}'),
            "reg" => Some('\u{ae}'),
            "trade" => Some('\u{2122}'),
            "times" => Some('\u{d7}'),
            "deg" => Some('\u{b0}'),
            "sect" => Some('\u{a7}'),
            "para" => Some('\u{b6}'),
            "euro" => Some('\u{20ac}'),
            "larr" => Some('\u{2190}'),
            "rarr" => Some('\u{2192}'),
            "le" => Some('\u{2264}'),
            "ge" => Some('\u{2265}'),
            "minus" => Some('\u{2212}'),
            numeric => numeric.strip_prefix('#').and_then(|code| {
                let value = match code.strip_prefix('x') {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => code.parse().ok(),
                };
                value.and_then(char::from_u32)
            }),
        };
        if let Some(ch) = decoded {
            out.push(ch);
            rest = &tail[end + 1..];
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}
