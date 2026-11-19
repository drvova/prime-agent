//! The supervisor's `get_usage` arm: the `/usage` page's two halves, served without a session.
//! `quotas` reads the stored subscription credentials and each provider's usage endpoint
//! ([`quotas`]); `spend` aggregates the local session files ([`history`]). Both run off the
//! client's read loop: the file scan on the blocking pool, the fetches as bounded requests.

mod history;
pub(crate) mod quotas;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use pa_types::daemon::DaemonCommand;
use pa_types::provider_usage::{QuotaReport, UsageSection};
use serde_json::Value;

use crate::protocol::{response_failure, response_line, response_success};
use crate::supervisor::Supervisor;

/// Register the worker's `usage.quotas` kernel host request (the REPL's `usage.get()`): the
/// same quota cards as the page's `quotas` half, from the same fetch and cache. Payload
/// `{refresh?}` bypasses the cache like the page's refresh key. The reply is the wire's
/// `QuotaReport`: provider ids, plans, and windows, never a credential.
pub(crate) fn register_quota_host_handler(
    handlers: &mut pa_core::kernel::shared::HostRequestHandlers,
    agent_dir: std::path::PathBuf,
    config: quotas::QuotaFetchConfig,
) {
    handlers.register(
        pa_core::usage_host::USAGE_QUOTAS_REQUEST_TYPE,
        pa_core::kernel::shared::host_handler(move |payload| {
            let agent_dir = agent_dir.clone();
            let config = config.clone();
            async move {
                let policy = if payload.data.get("refresh").and_then(Value::as_bool) == Some(true) {
                    quotas::CachePolicy::Bypass
                } else {
                    quotas::CachePolicy::UseFresh
                };
                let now_ms = i64::try_from(crate::util::now_ms()).unwrap_or(i64::MAX);
                let report = quotas::quota_report(agent_dir, &config, policy, now_ms).await;
                Ok(serde_json::to_value(report)?)
            }
        }),
    );
}

impl Supervisor {
    /// `get_usage`: one half of the page, as the wire's `QuotaReport` or `SpendHistory`.
    pub(crate) async fn handle_get_usage(
        self: &Arc<Self>,
        command: &DaemonCommand,
        command_id: &str,
        type_name: &str,
    ) -> (Vec<Value>, bool) {
        let DaemonCommand::GetUsage {
            section,
            force_refresh,
            ..
        } = command
        else {
            return (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    "invalid command",
                    None,
                ))],
                false,
            );
        };
        let now_ms = i64::try_from(crate::util::now_ms()).unwrap_or(i64::MAX);
        let agent_dir = self.options.agent_dir.clone();
        let data = match section {
            UsageSection::Quotas => {
                let policy = if force_refresh.unwrap_or(false) {
                    quotas::CachePolicy::Bypass
                } else {
                    quotas::CachePolicy::UseFresh
                };
                let report = quotas::quota_report(
                    agent_dir,
                    &quotas::QuotaFetchConfig::from_env(),
                    policy,
                    now_ms,
                )
                .await;
                serde_json::to_value(QuotaReport {
                    pacing: self.admission.pacing(),
                    ..report
                })
            }
            UsageSection::Spend => {
                let sessions_root = match crate::paths::sessions_dir(&agent_dir) {
                    Ok(root) => root,
                    Err(error) => {
                        return (
                            vec![response_line(&response_failure(
                                Some(command_id),
                                type_name,
                                &format!("{error:#}"),
                                None,
                            ))],
                            false,
                        );
                    }
                };
                match tokio::task::spawn_blocking(move || {
                    history::spend_history(&sessions_root, now_ms)
                })
                .await
                {
                    Ok(history) => serde_json::to_value(history),
                    Err(error) => {
                        return (
                            vec![response_line(&response_failure(
                                Some(command_id),
                                type_name,
                                &format!("the session scan failed: {error}"),
                                None,
                            ))],
                            false,
                        );
                    }
                }
            }
        };
        match data {
            Ok(data) => (
                vec![response_line(&response_success(
                    Some(command_id),
                    type_name,
                    Some(data),
                ))],
                false,
            ),
            Err(error) => (
                vec![response_line(&response_failure(
                    Some(command_id),
                    type_name,
                    &error.to_string(),
                    None,
                ))],
                false,
            ),
        }
    }
}
