//! The supervisor's `get_usage` arm: the `/usage` page's two halves, served without a session.
//! `quotas` reads the stored subscription credentials and each provider's usage endpoint
//! ([`quotas`]); `spend` aggregates the local session files ([`history`]). Both run off the
//! client's read loop: the file scan on the blocking pool, the fetches as bounded requests.

mod history;
mod quotas;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use pa_types::daemon::DaemonCommand;
use pa_types::provider_usage::UsageSection;
use serde_json::Value;

use crate::protocol::{response_failure, response_line, response_success};
use crate::supervisor::Supervisor;

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
                serde_json::to_value(report)
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
