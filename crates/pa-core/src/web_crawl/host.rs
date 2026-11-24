//! The `web.crawl` host request: the kernel's `(web-crawl url ...)` reaches
//! the crawler through it, as `web-read` reaches the reader.
//!
//! Payload: `{url, maxPages?, maxDepth?, include?, exclude?, maxChars?,
//! subdomains?, sitemap?, render?, timeoutSeconds?}`.
//! Reply: the [`super::Crawl::to_json`] shape.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::{identity, parse_request, CrawlOutcome, Identity, WebCrawlEngine};
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::kernel::spill::SpillStore;
use crate::settings::WebCrawlSettings;

/// Observes each finished crawl (adoption telemetry folds these into
/// session counters); never sees a URL or a page.
pub(crate) type CrawlObserver = Arc<dyn Fn(CrawlOutcome) + Send + Sync>;

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
    /// The user's settings; `None` crawls with the honest defaults.
    pub(crate) settings: Option<CrawlSettings>,
}

impl WebCrawlHost {
    /// Register the `web.crawl` handler.
    pub(crate) fn register(self, handlers: &mut HostRequestHandlers) {
        let host = Arc::new(self);
        handlers.register(
            "web.crawl",
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { host.handle(&payload.data).await }
            }),
        );
    }

    #[tracing::instrument(skip_all)]
    pub(super) async fn handle(&self, data: &Value) -> anyhow::Result<Value> {
        let request = parse_request(data)?;
        let settings = self
            .settings
            .as_ref()
            .map(|read| read())
            .unwrap_or_default();
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
            });
        }
        crawled.map(|crawl| crawl.to_json())
    }
}
