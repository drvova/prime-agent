//! `web.automate`: spider's model-driven browser automation (`spider_agent`'s
//! `RemoteMultimodalEngine`). The kernel's `(web-automate url goal ...)`
//! reaches it. Each round the engine sends the page (its URL, title, cleaned
//! HTML, and a screenshot for a vision model) and the goal to an
//! OpenAI-compatible chat-completions endpoint, runs the steps the model
//! answers with (click, fill, type, scroll, evaluate, ...) on the page, and
//! stops when the model says it is done or after `max_steps` rounds.
//!
//! Off unless the user sets `web.crawl.ai` (global settings only): it spends
//! model tokens on the user's account. The key is never a setting: it is the
//! credential the user already configured for the provider `keyFrom` names,
//! read from the session's auth storage at each call, held only in the
//! engine for that call, sent only to the configured endpoint (HTTPS, or
//! plain HTTP to this machine), and scrubbed from everything the call
//! returns, so neither the transcript nor telemetry ever sees it.
//!
//! The engine's HTTP client is pa-core's (`pa_ai::utils::tls`: the session's
//! proxies and CA bundle), not spider's own. The page is pa-core's browser
//! ([`super::capture::with_page`]): contained, killed when the call ends. A
//! stopped cell drops the request's future, which drops the engine's
//! in-flight request and the browser; nothing it starts outlives the call.

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use spider_agent::{RemoteMultimodalConfig, RemoteMultimodalEngine};
use url::Url;

use super::capture::{with_page, PageSetup};
use super::identity::Identity;
use super::{CookieJar, CookieScope};
use crate::settings::WebCrawlAiSettings;
use crate::web_read::{Fetched, ReadRequest};

/// Rounds unless asked otherwise.
const DEFAULT_STEPS: usize = 6;
/// The most rounds one call may run.
const MAX_STEPS: usize = 30;
/// The whole call's default deadline.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(2);
/// The longest deadline a call may ask for.
const MAX_TIMEOUT: Duration = Duration::from_mins(10);
/// One model request's deadline.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest goal or extraction prompt.
pub(super) const MAX_PROMPT_CHARS: usize = 8_000;
/// Characters of the final page's markdown returned unless asked otherwise.
const DEFAULT_MAX_CHARS: usize = 8_000;
/// What the key reads as wherever it would have appeared.
const REDACTED: &str = "[redacted key]";

/// One automation to run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AutomateRequest {
    pub(crate) url: Url,
    pub(crate) goal: String,
    pub(crate) max_steps: usize,
    /// What the model returns as `extracted` once done.
    pub(crate) extract: Option<String>,
    pub(crate) max_chars: usize,
    pub(crate) timeout: Duration,
    pub(crate) cookies: CookieScope,
}

/// Parse a `web.automate` payload.
///
/// # Errors
///
/// A missing or non-http(s) URL, a missing goal, or an option out of range.
pub(crate) fn parse_request(data: &Value) -> anyhow::Result<AutomateRequest> {
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let raw = text("url").ok_or_else(|| anyhow!("web.automate requires a url"))?;
    let url =
        Url::parse(raw).map_err(|error| anyhow!("web.automate: invalid url {raw:?}: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("web.automate drives http(s) pages; got {}", url.scheme());
    }
    let prompt = |key: &str| -> anyhow::Result<Option<String>> {
        match text(key) {
            Some(prompt) if prompt.chars().count() > MAX_PROMPT_CHARS => {
                bail!("web.automate {key} is longer than {MAX_PROMPT_CHARS} chars")
            }
            Some(prompt) => Ok(Some(prompt.to_string())),
            None => Ok(None),
        }
    };
    let count = |key: &str, default: usize, max: usize| -> anyhow::Result<usize> {
        match data.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(value) => value
                .as_u64()
                .and_then(|count| usize::try_from(count).ok())
                .filter(|count| (1..=max).contains(count))
                .ok_or_else(|| anyhow!("web.automate {key} must be 1 to {max}")),
        }
    };
    let timeout = match data.get("timeoutSeconds").and_then(Value::as_f64) {
        None => DEFAULT_TIMEOUT,
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            Duration::from_secs_f64(seconds).min(MAX_TIMEOUT)
        }
        Some(_) => bail!("web.automate timeoutSeconds must be positive"),
    };
    Ok(AutomateRequest {
        url,
        goal: prompt("goal")?.ok_or_else(|| anyhow!("web.automate requires a goal"))?,
        max_steps: count("maxSteps", DEFAULT_STEPS, MAX_STEPS)?,
        extract: prompt("extract")?,
        max_chars: count(
            "maxChars",
            DEFAULT_MAX_CHARS,
            crate::web_read::MAX_MAX_CHARS,
        )?,
        timeout,
        cookies: CookieScope::parse(data, "web.automate")?,
    })
}

/// A provider credential, never printed.
pub(super) struct ApiKey(pub(super) String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// The model the user's `web.crawl.ai` names, with its credential.
#[derive(Debug)]
pub(crate) struct Model {
    pub(super) endpoint: Url,
    pub(super) name: String,
    pub(super) key: Option<ApiKey>,
}

/// Where a provider's credential is read from: the session's auth storage.
pub(crate) type Credentials = std::sync::Arc<tokio::sync::Mutex<crate::auth::AuthStorage>>;

impl Model {
    /// The model `settings` names, its key read from `credentials` for the
    /// provider `keyFrom` names.
    ///
    /// # Errors
    ///
    /// The setting is off or incomplete, its endpoint is not HTTPS (or HTTP
    /// to this machine), or the provider it names has no credential.
    pub(crate) async fn resolve(
        settings: Option<&WebCrawlAiSettings>,
        credentials: Option<&Credentials>,
    ) -> anyhow::Result<Model> {
        const OFF: &str = "web-automate and web-crawl #:automate are off: they spend model tokens on the user's account, so only the user can turn them on, with the global web.crawl.ai setting ({\"endpoint\": an OpenAI-compatible chat-completions URL, \"model\": ..., \"keyFrom\": a configured provider id}); do the steps yourself with web-crawl #:actions instead";
        let Some(settings) = settings else {
            bail!(OFF);
        };
        let (Some(endpoint), Some(name)) = (
            settings
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            settings
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ) else {
            bail!("{OFF} (web.crawl.ai needs both endpoint and model)");
        };
        let endpoint = Url::parse(endpoint)
            .map_err(|error| anyhow!("the web.crawl.ai endpoint {endpoint:?}: {error}"))?;
        let local = match endpoint.host() {
            Some(url::Host::Domain(host)) => host == "localhost",
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        };
        match (endpoint.scheme(), local) {
            ("https", _) | ("http", true) => {}
            (scheme, _) => bail!(
                "the web.crawl.ai endpoint must be https (or http to this machine); got {scheme}://"
            ),
        }
        let key = match settings
            .key_from
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            None => None,
            Some(provider) => {
                let credentials = credentials.ok_or_else(|| {
                    anyhow!("web.crawl.ai keyFrom {provider:?}: this session has no provider credentials")
                })?;
                let key = credentials.lock().await.get_api_key(provider).ok_or_else(|| {
                    anyhow!("web.crawl.ai keyFrom {provider:?}: no credential is configured for that provider (/login or its API key setting)")
                })?;
                Some(ApiKey(key))
            }
        };
        Ok(Model {
            endpoint,
            name: name.to_string(),
            key,
        })
    }

    /// `text` with the key, wherever it appears, replaced.
    pub(super) fn scrub(&self, text: &str) -> String {
        match &self.key {
            Some(ApiKey(key)) if !key.is_empty() => text.replace(key.as_str(), REDACTED),
            Some(_) | None => text.to_string(),
        }
    }
}

/// The client the model requests go out on: pa-core's (`pa_ai::utils::tls`:
/// the session's proxies and CA bundle), never `spider_agent`'s static one.
///
/// # Errors
///
/// The client could not be built.
pub(super) fn client() -> anyhow::Result<reqwest::Client> {
    pa_ai::utils::tls::http_client_builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("building the automation's client")
}

/// What adoption telemetry learns of one `web-automate`: counts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutomateOutcome {
    /// Refused before anything ran: the setting is off or incomplete.
    Refused,
    Ran {
        ok: bool,
        /// Steps the engine ran on the page.
        steps: usize,
        /// Model requests it made.
        calls: u32,
    },
}

/// Drive `request.url` toward `request.goal` with `model`, in a fresh
/// browser with `identity` and `jar`'s cookies. Replies `{url, finalUrl,
/// title, label, success, steps, error, extracted, usage, markdown,
/// truncated, text}`, the key scrubbed from all of it, and the outcome for
/// telemetry.
///
/// # Errors
///
/// No browser, a page that would not load, an engine failure (its message
/// scrubbed), or the deadline.
#[tracing::instrument(skip_all, fields(host = request.url.host_str().unwrap_or_default()))]
pub(crate) async fn automate(
    request: &AutomateRequest,
    identity: &Identity,
    jar: &CookieJar,
    model: &Model,
) -> (anyhow::Result<Value>, AutomateOutcome) {
    let (mut steps, mut calls) = (0, 0);
    let reply = async {
        let client = client()?;
        let config = RemoteMultimodalConfig {
            max_rounds: request.max_steps,
            // No closing screenshot in the result: it would be a base64 image.
            screenshot: false,
            extra_ai_data: request.extract.is_some(),
            extraction_prompt: request.extract.clone(),
            ..RemoteMultimodalConfig::default()
        };
        let mut engine =
            RemoteMultimodalEngine::new(model.endpoint.as_str(), model.name.as_str(), None)
                .with_api_key(model.key.as_ref().map(|ApiKey(key)| key.as_str()))
                .with_config(config);
        engine
            .with_client(Some(client))
            .with_user_message_extra(Some(&format!("The goal: {}", request.goal)));
        let setup = PageSetup {
            url: &request.url,
            identity,
            jar,
            viewport: None,
            init_script: None,
            timeout: request.timeout,
            doing: "automating",
        };
        let (result, title, final_url, html) = with_page(setup, async |page| {
            let result = engine
                .run(page, request.url.as_str())
                .await
                .map_err(|error| anyhow!("the automation failed: {error}"))?;
            let title = page.get_title().await.ok().flatten();
            let final_url = page.url().await.ok().flatten();
            let html = page.content().await.context("reading the page")?;
            anyhow::Ok((result, title, final_url, html))
        })
        .await?;
        steps = result.steps_executed;
        calls = result.usage.llm_calls;
        let final_url = final_url
            .and_then(|url| Url::parse(&url).ok())
            .unwrap_or_else(|| request.url.clone());
        let read = ReadRequest {
            url: request.url.clone(),
            max_chars: request.max_chars,
            section: None,
            timeout: request.timeout,
            shaping: crate::web_read::Shaping::default(),
        };
        let document = crate::web_read::render(
            Fetched {
                final_url: final_url.clone(),
                media_type: Some("text/html".to_string()),
                body: html.into_bytes(),
            },
            &read.shaping,
        )
        .await?;
        let page = crate::web_read::finish(&read, document, Duration::ZERO)?;
        let status = match (&result.error, result.success) {
            (Some(error), _) => format!("did not finish: {error}"),
            (None, true) => "done".to_string(),
            (None, false) => "stopped before the goal was reached".to_string(),
        };
        let text = format!(
            "Automated {} in {} step{} over {} model call{} ({status}; {} tokens): {}\n\n{}",
            final_url,
            result.steps_executed,
            if result.steps_executed == 1 { "" } else { "s" },
            result.usage.llm_calls,
            if result.usage.llm_calls == 1 { "" } else { "s" },
            result.usage.total_tokens,
            result.label,
            page.text()
        );
        anyhow::Ok(json!({
            "url": request.url.as_str(),
            "finalUrl": final_url.as_str(),
            "title": title,
            "label": result.label,
            "success": result.success,
            "steps": result.steps_executed,
            "error": result.error,
            "extracted": result.extracted,
            "usage": {
                "llmCalls": result.usage.llm_calls,
                "promptTokens": result.usage.prompt_tokens,
                "completionTokens": result.usage.completion_tokens,
                "totalTokens": result.usage.total_tokens,
            },
            "markdown": page.markdown,
            "truncated": page.truncated,
            "text": text,
        }))
    }
    .await;
    // The key never leaves in a reply or an error, whatever echoed it.
    let reply = match reply {
        Ok(reply) => {
            serde_json::from_str(&model.scrub(&reply.to_string())).context("scrubbing the reply")
        }
        Err(error) => Err(anyhow!(model.scrub(&format!("{error:#}")))),
    };
    let outcome = AutomateOutcome::Ran {
        ok: reply.is_ok(),
        steps,
        calls,
    };
    (reply, outcome)
}

#[cfg(test)]
mod tests;
