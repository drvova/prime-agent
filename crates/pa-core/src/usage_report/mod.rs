//! The `/usage` page's two halves, served without a session (both daemons' global `get_usage`):
//! `quotas` reads the stored subscription credentials and each provider's usage endpoint
//! ([`quotas`]); `spend` aggregates the local session files ([`history`]). Both run off the
//! caller's read loop: the file scan on the blocking pool, the fetches as bounded requests. The
//! kernel's `usage.quotas` host request answers the page's `quotas` half from the same fetch and
//! cache ([`register_quota_host_handler`]).

mod history;
mod quotas;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use pa_types::provider_usage::{ProviderPacing, QuotaReport, SpendHistory};
use serde_json::Value;

use quotas::QuotaFetchConfig;

/// Now, in Unix milliseconds, as the page's windows and days read it.
fn now_ms() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// Register the kernel's `usage.quotas` host request (the REPL's `usage.get()`): the same quota
/// cards as the page's `quotas` half, from the same fetch and cache. Payload `{refresh?}` bypasses
/// the cache like the page's refresh key. The reply is the wire's `QuotaReport`: provider ids,
/// plans, and windows, never a credential.
pub fn register_quota_host_handler(
    handlers: &mut crate::kernel::shared::HostRequestHandlers,
    agent_dir: PathBuf,
) {
    register_quota_host_handler_with(handlers, agent_dir, QuotaFetchConfig::from_env());
}

/// [`register_quota_host_handler`] over `config`'s endpoints (a test's loopback origin).
fn register_quota_host_handler_with(
    handlers: &mut crate::kernel::shared::HostRequestHandlers,
    agent_dir: PathBuf,
    config: QuotaFetchConfig,
) {
    handlers.register(
        crate::usage_host::USAGE_QUOTAS_REQUEST_TYPE,
        crate::kernel::shared::host_handler(move |payload| {
            let agent_dir = agent_dir.clone();
            let config = config.clone();
            async move {
                let policy = if payload.data.get("refresh").and_then(Value::as_bool) == Some(true) {
                    quotas::CachePolicy::Bypass
                } else {
                    quotas::CachePolicy::UseFresh
                };
                let report = quotas::quota_report(agent_dir, &config, policy, now_ms()).await;
                Ok(serde_json::to_value(report)?)
            }
        }),
    );
}

/// The page's `quotas` half: a card per stored plan credential of the agent dir, from the cache
/// unless `force_refresh` (the page's refresh key) bypasses it, with the caller's request
/// admission's `pacing` rows.
pub async fn quota_page(
    agent_dir: PathBuf,
    force_refresh: bool,
    pacing: Vec<ProviderPacing>,
) -> QuotaReport {
    let policy = if force_refresh {
        quotas::CachePolicy::Bypass
    } else {
        quotas::CachePolicy::UseFresh
    };
    let report =
        quotas::quota_report(agent_dir, &QuotaFetchConfig::from_env(), policy, now_ms()).await;
    QuotaReport { pacing, ..report }
}

/// The page's `spend` half: the billed model calls of every session file under
/// `sessions_root`, by day and model, scanned on the blocking pool.
///
/// # Errors
///
/// The scan's thread panicked.
pub async fn spend_page(sessions_root: &Path) -> anyhow::Result<SpendHistory> {
    let sessions_root = sessions_root.to_path_buf();
    tokio::task::spawn_blocking(move || history::spend_history(&sessions_root, now_ms()))
        .await
        .map_err(|error| anyhow::anyhow!("the session scan failed: {error}"))
}
