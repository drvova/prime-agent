//! A session's subagent roster and the cancel of one subagent's run:
//! `session.children` and `session.cancel_child`, the old daemon's
//! `get_rlm_children` and `cancel_rlm_child` contracts.
//!
//! `session.children {session}` answers the open session's direct subagents
//! in spawn order (`{"children": [...], "eventSequence"}`, rows in the old
//! daemon's roster shape, `Subagents::roster`; `eventSequence` the session
//! stream's last `meta.sequence`, read before the walk, the freshness a
//! client's settle barrier keys on). `session.cancel_child {session, child}` (the
//! child's handle, session id or name) ends the child's queued tasks and
//! aborts its running turn, and its `done` settles it as cancelled; an
//! unknown or settled child answers `cancelled: false`, not an error.

use serde_json::{json, Value};

use super::{string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) async fn children(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let sequence = self.runtime.last_event_sequence(&id).unwrap_or_default();
        let children = self.runtime.subagents().roster(&id).await;
        Ok(json!({ "session": id, "children": children, "eventSequence": sequence }))
    }

    pub(super) async fn cancel_child(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let child = string_arg(args, "child")?;
        let cancelled = self.runtime.subagents().cancel_child(&id, &child).await?;
        Ok(json!({ "session": id, "child": child, "cancelled": cancelled }))
    }
}
