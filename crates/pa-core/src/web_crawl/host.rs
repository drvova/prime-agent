//! The `web.crawl` host request: the kernel's `(web-crawl url ...)` reaches
//! the crawler through it, as `web-read` reaches the reader.
//!
//! Payload: `{url, maxPages?, maxDepth?, include?, exclude?, maxChars?,
//! subdomains?, sitemap?, render?, timeoutSeconds?}`.
//! Reply: the [`super::Crawl::to_json`] shape.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::{parse_request, CrawlOutcome, WebCrawlEngine};
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::kernel::spill::SpillStore;

/// Observes each finished crawl (adoption telemetry folds these into
/// session counters); never sees a URL or a page.
pub(crate) type CrawlObserver = Arc<dyn Fn(CrawlOutcome) + Send + Sync>;

/// One session's `web.crawl` handler wiring.
pub(crate) struct WebCrawlHost {
    pub(crate) engine: Arc<WebCrawlEngine>,
    /// Where pages past a crawl's output budget are paged out; `None` for a
    /// session that keeps no paged-out output.
    pub(crate) store: Option<SpillStore>,
    pub(crate) observer: Option<CrawlObserver>,
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
    async fn handle(&self, data: &Value) -> anyhow::Result<Value> {
        let request = parse_request(data)?;
        let started = Instant::now();
        let crawled = self.engine.crawl(&request, self.store.as_ref()).await;
        if let Some(observer) = &self.observer {
            observer(CrawlOutcome {
                render: request.render,
                pages: crawled.as_ref().ok().map(|crawl| crawl.pages.len()),
                elapsed: started.elapsed(),
            });
        }
        crawled.map(|crawl| crawl.to_json())
    }
}
