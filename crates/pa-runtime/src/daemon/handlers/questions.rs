//! An open session's attached clients and its kernel questions, ported from
//! the old daemon worker's attach, detach and `answer_question` arms
//! (`pa-daemon` `worker/connection.rs`, `user_interaction.rs`):
//!
//! - `session.attach {session, clientId?, capabilities?,
//!   clientCapabilities?, resumeCursor?}`: the calling connection's client
//!   (`anonymous` when it names none) attaches, able to answer the session's
//!   kernel questions when its capabilities (a routed attach's
//!   `clientCapabilities`, else `capabilities`) name `user_questions`; the
//!   answer is the attach snapshot (`session.snapshot {shape: "attach"}`).
//! - `session.detach {session, clientId?}`: that client leaves, or every
//!   client the connection attached when it names none (the TUI's detach
//!   names none: on wire2 its connection is the client); `null`. The
//!   connection's end detaches all of its clients.
//! - `session.answer_question {session, questionId, reply}`: the reply goes
//!   to the open question; `null`, or the old refusals (`missing reply`,
//!   `invalid reply: ...`, `question "<id>" is not open`, an answer that
//!   does not fit the question).
//!
//! What a client's attach changes is `crate::session::SessionInteraction`'s:
//! the question a kernel `ask` opens waits only while such a client is
//! attached.

use anyhow::Context as _;
use serde_json::Value;

use super::{optional_string_arg, RuntimeHandlers};
use crate::session::{SessionBuilder, SessionInteraction};
use crate::wire::ConnectionId;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// The interaction surface of open session `args.session`.
    fn interaction(&self, args: &Value) -> anyhow::Result<std::sync::Arc<SessionInteraction>> {
        let id = self.open_session(args)?;
        self.runtime
            .subagents()
            .interaction(&id)
            .with_context(|| format!("session {id} is not open in the runtime daemon"))
    }

    pub(super) async fn attach(
        &self,
        args: &Value,
        connection: ConnectionId,
    ) -> anyhow::Result<Value> {
        let interaction = self.interaction(args)?;
        let capabilities: Vec<String> = ["clientCapabilities", "capabilities"]
            .iter()
            .find_map(|key| args.get(*key).and_then(Value::as_array))
            .map_or_else(Vec::new, |items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
        // The old wire's default client.
        let client =
            optional_string_arg(args, "clientId")?.unwrap_or_else(|| "anonymous".to_string());
        interaction.attached(connection, &client, &capabilities);
        self.shaped_snapshot(args, "attach").await
    }

    pub(super) fn detach(&self, args: &Value, connection: ConnectionId) -> anyhow::Result<Value> {
        self.interaction(args)?.detached(
            connection,
            optional_string_arg(args, "clientId")?.as_deref(),
        );
        Ok(Value::Null)
    }

    pub(super) fn answer_question(&self, args: &Value) -> anyhow::Result<Value> {
        self.interaction(args)?.answer(args)?;
        Ok(Value::Null)
    }
}
