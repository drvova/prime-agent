//! The kernel's notices (a background command's end, loss or silence, a
//! background cell's end, a factory milestone) on the bus.
//!
//! A session's notice sink ([`kernel_notice_sink`]) admits each as the
//! daemon's worker does, at once, so a later `bash.consumed` still finds
//! the row it withdraws: a `Steer` notice steers its row onto the session's
//! agent (a turn takes it at its next step, an idle session's drive runs
//! it); a `WakeIdle` one does the same unless the session is in a turn,
//! where it is dropped; a `Record` one is only kept. Each is then published,
//! stored, on `session/<id>/bash/quiet` (a `[bash-quiet]` notice) or
//! `session/<id>/bash/done` (every other) as a [`NoticeEvent`], and the
//! runtime's notice subscription ([`super::NOTICE_SUBSCRIPTION`]) wakes the
//! session for one it steered ([`Subagents::deliver_notice`]): its drive
//! looks again, a settled child is given its next task over it, and a root
//! in the runtime daemon is woken for it (the old daemon's notice turn).

use std::sync::Arc;

use anyhow::Context as _;
use pa_core::session_engine::bash_notices::{
    KernelNotice, KernelNoticeSink, NoticeAdmission, NoticeDelivery,
};
use pa_core::session_engine::goal_boundary::custom_message_to_loop_row;
use pa_core::session_engine::messages::ASYNC_BASH_QUIET_CUSTOM_TYPE;
use pa_types::event_log::{Event, NewEvent, Topic};
use serde_json::{json, Value};

use super::{wake_on_delivery, Node, Subagents};

/// What became of a notice as its session admitted it: the `taken` of
/// what `session/<id>/bash/*` carries (`{taken, row}`, the row as the
/// transcript would keep it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeTaken {
    /// Its row is on the session's agent.
    Steered,
    /// It is only kept: no turn follows it.
    Recorded,
    /// A wake-idle notice the session's turn did not take.
    DroppedBusy,
}

impl NoticeTaken {
    const fn as_str(self) -> &'static str {
        match self {
            NoticeTaken::Steered => "steered",
            NoticeTaken::Recorded => "recorded",
            NoticeTaken::DroppedBusy => "dropped_busy",
        }
    }
}

/// The notice sink of `node`'s session: it admits each notice and
/// publishes it (see the module).
pub(crate) fn kernel_notice_sink(node: &Arc<Node>) -> KernelNoticeSink {
    let node = Arc::clone(node);
    Arc::new(move |notice: KernelNotice| {
        let (Some(engine), Some(tree)) = (node.engine(), node.tree.upgrade()) else {
            return NoticeAdmission::NotTaken;
        };
        let agent = engine.session.agent();
        let taken = match notice.delivery {
            NoticeDelivery::Record => NoticeTaken::Recorded,
            // A run in flight is a turn at work: it reaches what the row
            // reports itself.
            NoticeDelivery::WakeIdle if agent.signal().is_some() => NoticeTaken::DroppedBusy,
            NoticeDelivery::Steer | NoticeDelivery::WakeIdle => {
                let Some(row) = custom_message_to_loop_row(&notice.row) else {
                    return NoticeAdmission::NotTaken;
                };
                agent.steer(row);
                NoticeTaken::Steered
            }
        };
        let kind = if notice.row.custom_type == ASYNC_BASH_QUIET_CUSTOM_TYPE {
            "quiet"
        } else {
            "done"
        };
        let published = Topic::new(format!("session/{}/bash/{kind}", node.session_id))
            .map_err(anyhow::Error::from)
            .and_then(|topic| {
                Ok(NewEvent {
                    topic,
                    source: node.session_id.clone(),
                    session: Some(node.session_id.clone()),
                    payload: json!({
                        "taken": taken.as_str(),
                        "row": serde_json::to_value(&notice.row)?,
                    }),
                    trust: node.trust.current(),
                    cause: None,
                    reply: None,
                })
            });
        match published {
            Ok(event) => tree.outbox.send(event),
            Err(error) => tracing::error!("a kernel notice was not published: {error:#}"),
        }
        match taken {
            NoticeTaken::Steered | NoticeTaken::Recorded => NoticeAdmission::Taken,
            NoticeTaken::DroppedBusy => NoticeAdmission::DroppedBusy,
        }
    })
}

impl Subagents {
    /// Wakes the session of the notice `event` (on `session/<id>/bash/*`)
    /// when the session steered its row.
    ///
    /// # Errors
    ///
    /// The event carries no notice.
    pub(crate) async fn deliver_notice(&self, event: &Event) -> anyhow::Result<()> {
        let taken = event
            .event
            .payload
            .get("taken")
            .and_then(Value::as_str)
            .and_then(|taken| {
                [
                    NoticeTaken::Steered,
                    NoticeTaken::Recorded,
                    NoticeTaken::DroppedBusy,
                ]
                .into_iter()
                .find(|kind| kind.as_str() == taken)
            })
            .context("the event carries no kernel notice")?;
        match taken {
            NoticeTaken::Steered => {}
            NoticeTaken::Recorded | NoticeTaken::DroppedBusy => return Ok(()),
        }
        let Some(node) = event
            .event
            .topic
            .levels()
            .nth(1)
            .and_then(|id| self.node(id))
        else {
            return Ok(());
        };
        node.activity.notify_waiters();
        wake_on_delivery(&node, event.event.trust).await;
        Ok(())
    }
}
