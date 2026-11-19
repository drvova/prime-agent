//! The `web.read` host request: the kernel's `rlm.web.read(...)` reaches the
//! reader through it, like `web.search` reaches the search engine.
//!
//! Payload: `{url, maxChars?, section?, timeoutSeconds?}`.
//! Reply: the [`WebPage::to_json`] shape.

use std::sync::Arc;

use anyhow::anyhow;
use serde_json::Value;

use super::{parse_request, ReadError, WebPage, WebReadEngine};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Observes each finished read (adoption telemetry folds these into
/// session counters); never sees the URL or the content.
pub(crate) type ReadObserver = Arc<dyn Fn(&Result<WebPage, ReadError>) + Send + Sync>;

/// One session's `web.read` handler wiring.
pub(crate) struct WebReadHost {
    pub(crate) engine: Arc<WebReadEngine>,
    pub(crate) observer: Option<ReadObserver>,
}

impl WebReadHost {
    /// Register the `web.read` handler.
    pub(crate) fn register(self, handlers: &mut HostRequestHandlers) {
        let host = Arc::new(self);
        handlers.register(
            "web.read",
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { host.handle(&payload.data).await }
            }),
        );
    }

    #[tracing::instrument(skip_all)]
    async fn handle(&self, data: &Value) -> anyhow::Result<Value> {
        let request = parse_request(data)?;
        let outcome = self.engine.read(&request).await;
        if let Some(observer) = &self.observer {
            observer(&outcome);
        }
        outcome
            .map(|page| page.to_json())
            .map_err(|error| anyhow!(error.message))
    }
}
