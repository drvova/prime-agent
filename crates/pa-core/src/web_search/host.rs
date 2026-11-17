//! The `web.search` host request: the kernel's `rlm.web.search(...)` (and
//! the bundled websearch skill) reach the native engine through it, like
//! `rlm.mcp` reaches the MCP manager.
//!
//! Payload: `{query, provider?, numResults?, recency?, timeoutSeconds?}`.
//! Reply: the [`SearchReport::to_json`] shape. Credentials resolve per
//! request (the auth store is re-read, so a key saved via `/mcp` mid-session
//! applies to the next search) and never leave the host.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use serde_json::Value;

use super::{
    Credentials, Recency, SearchError, SearchReport, SearchRequest, WebSearchEngine,
    WebSearchProvider, DEFAULT_TIMEOUT, MAX_TIMEOUT,
};
use crate::auth::AuthStorage;
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Observes each finished search (adoption telemetry folds these into
/// session counters); never sees the query.
pub type SearchObserver = Arc<dyn Fn(&Result<SearchReport, SearchError>) + Send + Sync>;

/// One session's `web.search` handler wiring: the shared engine, the auth
/// store credentials resolve from, and the session's default provider.
pub struct WebSearchHost {
    pub engine: Arc<WebSearchEngine>,
    pub auth: Arc<tokio::sync::Mutex<AuthStorage>>,
    /// `webSearchProvider`, used when the call names no provider.
    pub default_provider: WebSearchProvider,
    pub observer: Option<SearchObserver>,
}

impl WebSearchHost {
    /// Register the `web.search` handler (the request type the kernel's
    /// `rlm.web` client sends).
    pub fn register(self, handlers: &mut HostRequestHandlers) {
        let host = Arc::new(self);
        handlers.register(
            "web.search",
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { host.handle(&payload.data).await }
            }),
        );
    }

    #[tracing::instrument(skip_all)]
    async fn handle(&self, data: &Value) -> anyhow::Result<Value> {
        let query = data
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .ok_or_else(|| anyhow!("web.search requires a non-empty query"))?
            .to_string();
        let provider = match data.get("provider").and_then(Value::as_str) {
            None => self.default_provider,
            Some(id) => WebSearchProvider::from_id(id).ok_or_else(|| {
                let known: Vec<&str> = WebSearchProvider::all()
                    .into_iter()
                    .map(WebSearchProvider::id)
                    .collect();
                anyhow!(
                    "unknown web search provider \"{id}\"; choose one of: {}",
                    known.join(", ")
                )
            })?,
        };
        let num_results = match data.get("numResults") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|count| *count >= 1)
                    .and_then(|count| usize::try_from(count).ok())
                    .ok_or_else(|| anyhow!("web.search numResults must be a positive integer"))?,
            ),
        };
        let recency = match data.get("recency").and_then(Value::as_str) {
            None => None,
            Some(id) => Some(Recency::from_id(id).ok_or_else(|| {
                anyhow!("web.search recency must be one of day, week, month, year")
            })?),
        };
        let timeout = match data.get("timeoutSeconds").and_then(Value::as_f64) {
            None => DEFAULT_TIMEOUT,
            Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
                Duration::from_secs_f64(seconds).min(MAX_TIMEOUT)
            }
            Some(_) => return Err(anyhow!("web.search timeoutSeconds must be positive")),
        };
        let request = SearchRequest {
            query,
            provider,
            num_results,
            recency,
            timeout,
        };
        let auth_data = {
            let mut store = self.auth.lock().await;
            store.reload();
            store.get_all()
        };
        // A `!command` key runs its command: off the async runtime.
        let credentials = tokio::task::spawn_blocking(move || Credentials::resolve(&auth_data))
            .await
            .context("web search credential resolve")?;
        let outcome = self.engine.search(&request, &credentials).await;
        if let Some(observer) = &self.observer {
            observer(&outcome);
        }
        outcome
            .map(|report| report.to_json())
            .map_err(|error| anyhow!(error))
    }
}
