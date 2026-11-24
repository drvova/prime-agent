//! The `web.crawl` host request: the kernel's `(web-crawl url ...)` reaches
//! the crawler through it, as `web-read` reaches the reader.
//!
//! Payload: `{url, maxPages?, maxDepth?, include?, exclude?, maxChars?,
//! subdomains?, sitemap?, render?, timeoutSeconds?}`.
//! Reply: the [`super::Crawl::to_json`] shape.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::{
    capture, identity, parse_request, CaptureOutcome, CrawlOutcome, Identity, WebCrawlEngine,
};
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::kernel::spill::SpillStore;
use crate::settings::WebCrawlSettings;

/// Observes each finished crawl (adoption telemetry folds these into
/// session counters); never sees a URL or a page.
pub(crate) type CrawlObserver = Arc<dyn Fn(CrawlOutcome) + Send + Sync>;

/// Observes each finished `web.capture` (counts only, like crawls).
pub(crate) type CaptureObserver = Arc<dyn Fn(CaptureOutcome) + Send + Sync>;

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
    /// The user's settings; `None` crawls with the honest defaults.
    pub(crate) settings: Option<CrawlSettings>,
}

impl WebCrawlHost {
    /// Register the `web.crawl` and `web.capture` handlers.
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
        handlers.register(
            "web.capture",
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { host.capture(&payload.data).await }
            }),
        );
    }

    /// The user's identity settings, read now.
    fn settings(&self) -> WebCrawlSettings {
        self.settings
            .as_ref()
            .map(|read| read())
            .unwrap_or_default()
    }

    #[tracing::instrument(skip_all)]
    async fn capture(&self, data: &Value) -> anyhow::Result<Value> {
        let request = capture::parse_request(data)?;
        let identity = Identity::from_settings(&self.settings())?;
        let captured = capture::capture(&request, &identity).await;
        if let Some(observer) = &self.capture_observer {
            observer(CaptureOutcome {
                pdf: match request.kind {
                    capture::CaptureKind::Screenshot { .. } => false,
                    capture::CaptureKind::Pdf { .. } => true,
                },
                ok: captured.is_ok(),
            });
        }
        captured
    }

    #[tracing::instrument(skip_all)]
    pub(super) async fn handle(&self, data: &Value) -> anyhow::Result<Value> {
        let request = parse_request(data)?;
        let settings = self.settings();
        let identity = Identity::from_settings(&settings)?;
        let started = Instant::now();
        let crawled = self
            .engine
            .crawl(&request, &identity, self.store.as_ref())
            .await;
        if let Some(observer) = &self.observer {
            observer(CrawlOutcome {
                render: request.render,
                pages: crawled.as_ref().ok().map(|crawl| crawl.pages.len()),
                elapsed: started.elapsed(),
                identity: !identity::changes(&settings).is_empty(),
                archived: crawled.as_ref().is_ok_and(|crawl| crawl.warc.is_some()),
            });
        }
        crawled.map(|crawl| crawl.to_json())
    }
}
