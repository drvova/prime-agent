//! Ephemeral sessions and the client that owns them: the old daemon's
//! client-owned `noSession` worker (`--no-session`).
//!
//! `session.create {"ephemeral": true}` opens a new session that keeps
//! nothing: its transcript is in memory only, no options row is written for
//! it, it holds no lease (there is no file), and it is not kept among the
//! held sessions, so no daemon revives it. It belongs to the client name
//! the creating connection said `hello` with. It ends when it is stopped
//! (`session.stop`, the old `complete_owned_session`), or, as the old
//! daemon's owned worker does, [`OWNER_GRACE`] after the owner's last
//! connection ended: an owner that connects again within the grace keeps
//! it.

use std::time::Duration;

use serde_json::json;

use super::RuntimeHandlers;
use crate::session::SessionBuilder;
use crate::wire::ConnectionId;

/// How long an ephemeral session outlives its owner's last connection (the
/// old daemon's `OWNED_WORKER_DISCONNECT_GRACE`).
const OWNER_GRACE: Duration = Duration::from_secs(30);

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `connection` ended: when it was its client's last one, the client's
    /// ephemeral sessions are stopped once the grace passed without the
    /// client connecting again.
    pub(super) async fn end_owned_sessions(&self, connection: ConnectionId) {
        let owner = self.attached.client_of(connection);
        if self.open.owned_by(&owner).is_empty() || self.attached.others_of(&owner, connection) > 0
        {
            return;
        }
        tokio::time::sleep(OWNER_GRACE).await;
        if self.attached.others_of(&owner, connection) > 0 {
            return;
        }
        for id in self.open.owned_by(&owner) {
            if let Err(error) = self.stop(&json!({ "session": id })).await {
                tracing::warn!("the ephemeral session {id} did not stop: {error:#}");
            }
        }
    }
}
