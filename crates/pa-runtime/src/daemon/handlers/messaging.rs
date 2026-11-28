//! `session.send`: a client's message to a session (the CLI's `send`),
//! delivered as the old daemon's `send_message` delivers it: today's
//! `agent_message` row on the session's inbox, from the client
//! (`{clientId}`, the `client` argument) or from another open session
//! (`from`), steered or (`"delivery": "follow_up"`) queued behind the running
//! turn, an idle session woken for it. A saved session is opened for its
//! message first: the old daemon's saved-session wake, which restores a
//! session older releases archived (`sessions-archive/`, pa-core's
//! `resolve_saved_session`) before it opens it. The answer is the old
//! daemon's receipt, with `woken` saying whether the session was opened.

use anyhow::Context as _;
use pa_core::session::catalog::resolve_saved_session;
use pa_core::session::store::store;
use serde_json::{json, Value};

use super::{optional_string_arg, string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;
use crate::subagents::{Delivery, MessageSender};
use crate::wire::ConnectionId;

/// The sender a message without `from` or `client` is from.
const DEFAULT_CLIENT: &str = "client";

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) async fn send(
        &self,
        args: &Value,
        connection: ConnectionId,
    ) -> anyhow::Result<Value> {
        let (target, path, open) = self.session(args)?;
        let message = string_arg(args, "message")?;
        let delivery = match optional_string_arg(args, "delivery")?.as_deref() {
            None | Some("steer") => Delivery::Steer,
            Some("follow_up") => Delivery::FollowUp,
            Some(other) => {
                anyhow::bail!("unknown delivery {other:?}: give `steer` or `follow_up`")
            }
        };
        let sender = match optional_string_arg(args, "from")? {
            Some(from) => {
                anyhow::ensure!(self.open.contains(&from), "Unknown active session: {from}");
                MessageSender::Session(from)
            }
            None => MessageSender::Client(
                optional_string_arg(args, "client")?.unwrap_or_else(|| DEFAULT_CLIENT.to_string()),
            ),
        };
        if !open {
            if !store().exists(&path) {
                // Archived by an older release: restored before the wake.
                let (sessions_dir, archive_dir) = (
                    self.session_dir.clone(),
                    self.agent_dir.join("sessions-archive"),
                );
                let (selector, cwd) = (target.clone(), self.cwd.display().to_string());
                let restored = tokio::task::spawn_blocking(move || {
                    resolve_saved_session(&sessions_dir, &archive_dir, &selector, &cwd)
                })
                .await
                .context("the saved-session lookup panicked")??;
                anyhow::ensure!(
                    restored.is_some_and(|info| info.path == path),
                    "Unknown active session: {target}"
                );
            }
            self.create(json!({ "session": target }), connection)
                .await?;
        }
        let mut receipt = self
            .runtime
            .subagents()
            .send_message(&target, &message, sender, delivery)
            .await?;
        receipt["woken"] = json!(!open);
        Ok(receipt)
    }
}
