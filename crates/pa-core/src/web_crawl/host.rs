//! The `web.crawl` host request: the kernel's `(web-crawl url ...)` reaches
//! the crawler through it, as `web-read` reaches the reader.
//!
//! Payload: `{url, maxPages?, maxDepth?, include?, exclude?, maxChars?,
//! subdomains?, sitemap?, render?, timeoutSeconds?, automate?, ...}`.
//! Reply: the [`super::Crawl::to_json`] shape.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail};
use serde_json::{json, Value};

use reqwest::cookie::CookieStore as _;

use super::{
    automate, capture, extract, identity, parse_request, CaptureOutcome, CookieJar, CookieScope,
    CrawlOutcome, ExtractOutcome, Identity, Render, WebCrawlEngine,
};
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::kernel::spill::SpillStore;
use crate::settings::WebCrawlSettings;

/// Observes each finished crawl (adoption telemetry folds these into
/// session counters); never sees a URL or a page.
pub(crate) type CrawlObserver = Arc<dyn Fn(CrawlOutcome) + Send + Sync>;

/// Observes each finished `web.capture` (counts only, like crawls).
pub(crate) type CaptureObserver = Arc<dyn Fn(CaptureOutcome) + Send + Sync>;

/// Observes each `web.automate` (counts only: never a URL, a goal, or a key).
pub(crate) type AutomateObserver = Arc<dyn Fn(automate::AutomateOutcome) + Send + Sync>;

/// Observes each finished `web.extract`: whether it succeeded, nothing else.
pub(crate) type ExtractObserver = Arc<dyn Fn(ExtractOutcome) + Send + Sync>;

/// The largest HTML a `web.extract` call may hand over itself.
const MAX_EXTRACT_HTML: usize = 8 * 1024 * 1024;

/// Reads the user's `web.crawl` settings, fresh for each crawl (so a change
/// takes effect for the next one).
pub(crate) type CrawlSettings = Arc<dyn Fn() -> WebCrawlSettings + Send + Sync>;

/// One session's `web.crawl` handler wiring.
pub(crate) struct WebCrawlHost {
    pub(crate) engine: Arc<WebCrawlEngine>,
    /// Where pages past a crawl's output budget are paged out; `None` for a
    /// session that keeps no paged-out output.
    pub(crate) store: Option<SpillStore>,
    pub(crate) observer: Option<CrawlObserver>,
    pub(crate) capture_observer: Option<CaptureObserver>,
    pub(crate) extract_observer: Option<ExtractObserver>,
    /// The user's settings; `None` crawls with the honest defaults.
    pub(crate) settings: Option<CrawlSettings>,
    /// The session's cookie jar for its rendered crawls and captures.
    pub(crate) cookies: CookieJar,
    /// The session's provider credentials, which `web.automate` reads the
    /// key `web.crawl.ai.keyFrom` names from; `None` for a session without.
    pub(crate) credentials: Option<automate::Credentials>,
    pub(crate) automate_observer: Option<AutomateObserver>,
}

impl WebCrawlHost {
    /// Register the `web.crawl`, `web.extract` and `web.capture` handlers.
    pub(crate) fn register(self, handlers: &mut HostRequestHandlers) {
        let host = Arc::new(self);
        let crawler = Arc::clone(&host);
        handlers.register(
            "web.crawl",
            host_handler(move |payload| {
                let host = Arc::clone(&crawler);
                async move { host.handle(&payload.data).await }
            }),
        );
        let automator = Arc::clone(&host);
        handlers.register(
            "web.automate",
            host_handler(move |payload| {
                let host = Arc::clone(&automator);
                async move { host.automate(&payload.data).await }
            }),
        );
        let extractor = Arc::clone(&host);
        handlers.register(
            "web.extract",
            host_handler(move |payload| {
                let host = Arc::clone(&extractor);
                async move {
                    let extracted = host.extract(&payload.data).await;
                    if let Some(observer) = &host.extract_observer {
                        observer(ExtractOutcome {
                            ok: extracted.is_ok(),
                        });
                    }
                    extracted
                }
            }),
        );
        handlers.register(
            "web.capture",
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { host.capture(&payload.data).await }
            }),
        );
    }

    /// The jar a load with `scope` uses: the session's, or a fresh one.
    fn jar(&self, scope: CookieScope) -> CookieJar {
        match scope {
            CookieScope::Session => Arc::clone(&self.cookies),
            CookieScope::None => CookieJar::default(),
        }
    }

    /// The user's identity settings, read now.
    fn settings(&self) -> WebCrawlSettings {
        self.settings
            .as_ref()
            .map(|read| read())
            .unwrap_or_default()
    }

    /// `web.extract`: the values `selectors` find in the given `html`, or in
    /// the page at `url` read as a one-page crawl (robots.txt and the user's
    /// identity apply; `render` and `mode` load it in the browser).
    #[tracing::instrument(skip_all)]
    async fn extract(&self, data: &Value) -> anyhow::Result<Value> {
        let selectors = extract::Selectors::parse(data, "selectors", "web.extract")?
            .ok_or_else(|| anyhow!("web.extract requires selectors"))?;
        if let Some(html) = data.get("html").and_then(Value::as_str) {
            if html.len() > MAX_EXTRACT_HTML {
                bail!("web.extract takes at most {MAX_EXTRACT_HTML} bytes of HTML");
            }
            let html = html.to_string();
            return crate::web_read::on_own_thread("web-extract", move || selectors.extract(&html))
                .await;
        }
        let request = parse_request(&json!({
            "url": data.get("url"),
            "render": data.get("render"),
            "mode": data.get("mode"),
            "timeoutSeconds": data.get("timeoutSeconds"),
            "maxPages": 1,
            "extract": data.get("selectors"),
            "cookies": data.get("cookies"),
        }))?;
        let identity = Identity::from_settings(&self.settings())?;
        let crawl = self
            .engine
            .crawl(
                &request,
                &identity,
                None,
                &self.jar(request.cookies),
                /*automation*/ None,
            )
            .await?;
        match (crawl.pages.first(), crawl.failed.first()) {
            (Some(page), _) => page.get("extract").cloned().ok_or_else(|| {
                anyhow!(
                    "nothing was extracted from {} ({}): {}",
                    page["finalUrl"].as_str().unwrap_or_default(),
                    page["contentType"].as_str().unwrap_or_default(),
                    page["notes"]
                )
            }),
            (None, Some(failed)) => bail!(
                "web.extract: {} answered HTTP {}",
                failed["url"].as_str().unwrap_or_default(),
                failed["status"]
            ),
            (None, None) => bail!("web.extract read nothing: {}", crawl.notes.join("; ")),
        }
    }

    /// `web.automate`: spider's model-driven automation of one page, only
    /// with the user's `web.crawl.ai` setting.
    #[tracing::instrument(skip_all)]
    pub(super) async fn automate(&self, data: &Value) -> anyhow::Result<Value> {
        let request = automate::parse_request(data)?;
        let settings = self.settings();
        let observe = |outcome| {
            if let Some(observer) = &self.automate_observer {
                observer(outcome);
            }
        };
        let model =
            match automate::Model::resolve(settings.ai.as_ref(), self.credentials.as_ref()).await {
                Ok(model) => model,
                Err(error) => {
                    observe(automate::AutomateOutcome::Refused);
                    return Err(error);
                }
            };
        let identity = Identity::from_settings(&settings)?;
        let (reply, outcome) =
            automate::automate(&request, &identity, &self.jar(request.cookies), &model).await;
        observe(outcome);
        reply
    }

    #[tracing::instrument(skip_all)]
    pub(super) async fn capture(&self, data: &Value) -> anyhow::Result<Value> {
        let request = capture::parse_request(data)?;
        let identity = Identity::from_settings(&self.settings())?;
        let jar = self.jar(request.cookies);
        let session_cookies = jar.cookies(&request.url).is_some();
        let captured = capture::capture(&request, &identity, &jar).await;
        if let Some(observer) = &self.capture_observer {
            observer(CaptureOutcome {
                pdf: match request.kind {
                    capture::CaptureKind::Screenshot { .. } => false,
                    capture::CaptureKind::Pdf { .. } => true,
                },
                ok: captured.is_ok(),
                session_cookies,
            });
        }
        captured
    }

    /// `web.crawl`; with `automate`, only with the user's `web.crawl.ai`
    /// setting, its key scrubbed from the whole reply.
    #[tracing::instrument(skip_all)]
    pub(super) async fn handle(&self, data: &Value) -> anyhow::Result<Value> {
        let request = parse_request(data)?;
        let settings = self.settings();
        let model = match request.automate {
            Some(_) => Some(
                automate::Model::resolve(settings.ai.as_ref(), self.credentials.as_ref()).await?,
            ),
            None => None,
        };
        let identity = Identity::from_settings(&settings)?;
        let started = Instant::now();
        let jar = self.jar(request.cookies);
        let session_cookies = match request.render {
            Render::Http => false,
            Render::Browser | Render::Smart => jar.cookies(&request.url).is_some(),
        };
        let crawled = self
            .engine
            .crawl(
                &request,
                &identity,
                self.store.as_ref(),
                &jar,
                model.as_ref(),
            )
            .await;
        if let Some(observer) = &self.observer {
            observer(CrawlOutcome {
                render: request.render,
                pages: crawled.as_ref().ok().map(|crawl| crawl.pages.len()),
                elapsed: started.elapsed(),
                identity: !identity::changes(&settings).is_empty(),
                archived: crawled.as_ref().is_ok_and(|crawl| crawl.warc.is_some()),
                per_page: super::PageWork {
                    shaped: !request.shaping.is_default(),
                    extracted: request.extract.is_some(),
                    session_cookies,
                    automation_calls: crawled
                        .as_ref()
                        .ok()
                        .and_then(|crawl| crawl.automation)
                        .map(|(calls, _)| calls)
                        .or(model.as_ref().map(|_| 0)),
                },
            });
        }
        match (crawled, &model) {
            (Ok(crawl), None) => Ok(crawl.to_json()),
            (Err(error), None) => Err(error),
            // The key never leaves in a reply or an error, whatever echoed it.
            (Ok(crawl), Some(model)) => {
                serde_json::from_str(&model.scrub(&crawl.to_json().to_string()))
                    .map_err(|error| anyhow!("scrubbing the crawl: {error}"))
            }
            (Err(error), Some(model)) => Err(anyhow!(model.scrub(&format!("{error:#}")))),
        }
    }
}
