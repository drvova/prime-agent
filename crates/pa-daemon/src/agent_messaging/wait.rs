//! `agent_message.wait` for daemon workers: park on this session's receiving side, publish
//! `waitingOn` through the roster, and refuse a wait that would close a cycle.

use pa_core::session_engine::agent_messaging::{
    wait_for_arrival, AgentMessageWaitOutcome, AgentMessageWaitRequest,
};

use super::message::roster_summaries;
use super::LinkAgentMessageController;

impl LinkAgentMessageController {
    pub(super) async fn wait_for_message(
        &self,
        request: AgentMessageWaitRequest,
    ) -> anyhow::Result<AgentMessageWaitOutcome> {
        let own_session_id = self.family_identity().session_id;
        let mut own_ids = vec![self.active_session_id.as_str()];
        own_ids.extend(own_session_id.as_deref());
        wait_for_arrival(
            &self.waits,
            request,
            &own_ids,
            roster_summaries(&self.link, &self.worker_token),
        )
        .await
    }
}
