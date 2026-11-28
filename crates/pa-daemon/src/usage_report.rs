//! The supervisor's `get_usage` arm: the `/usage` page's two halves, served without a session,
//! from pa-core's `usage_report` (the quota cards with the supervisor's request pacing, and the
//! spend history of the agent dir's sessions).

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
        let agent_dir = self.options.agent_dir.clone();
        let data = match section {
            UsageSection::Quotas => serde_json::to_value(
                pa_core::usage_report::quota_page(
                    agent_dir,
                    force_refresh.unwrap_or(false),
                    self.admission.pacing(),
                )
                .await,
            )
            .map_err(anyhow::Error::from),
            UsageSection::Spend => match crate::paths::sessions_dir(&agent_dir) {
                Ok(root) => pa_core::usage_report::spend_page(&root)
                    .await
                    .and_then(|history| Ok(serde_json::to_value(history)?)),
                Err(error) => Err(error),
            },
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
                    &format!("{error:#}"),
                    None,
                ))],
                false,
            ),
        }
    }
}
