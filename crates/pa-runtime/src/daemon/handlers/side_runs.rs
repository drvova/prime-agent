//! A session's side runs, `/btw` questions and `/better` rewrites, the old
//! worker's commands of the same names over pa-core's shared registry and run
//! (`pa_core::session_engine::side_runs`, with the old admission rules and
//! refusal texts):
//!
//! - `session.start_side_question {session, sideQuestionId, question,
//!   previousTurns?, kind?}` (`start_side_question`): admits the run for the
//!   calling connection (one per connection and session, each id once) and
//!   answers `null` before it answers. The run streams the old
//!   `side_question_event` payloads on the live-only
//!   `agent/<id>/side_question`: `running` as it starts, one `running` per
//!   partial answer, then `complete`, `cancelled` or `error`, the last sent
//!   as the run leaves the registry. It runs on the session's own runtime,
//!   on the engine of the lifetime it started in.
//! - `session.abort_side_question {session, sideQuestionId}`
//!   (`abort_side_question`): `{aborted}`, whether the calling connection's
//!   run of that id was aborted.
//! - `session.record_better_outcome {session, outcome}`
//!   (`record_better_outcome`): counts what the user did with a `/better`
//!   rewrite (`sent_as_is`, `edited`, `discarded`) and folds it into the
//!   project's learning; `null`.
//!
//! A connection's runs are aborted when it ends (the old daemon's detach),
//! and a session's when it stops, which waits for their cancelled events.

use std::collections::HashMap;
use std::sync::Arc;

use pa_core::session_engine::side_runs::{
    better_outcome, drive, run_side_run, SideRuns, SIDE_RUN_SETTLE_TIMEOUT,
};
use pa_types::event_log::{NewEvent, Topic, Trust};
use serde_json::{json, Value};

use super::RuntimeHandlers;
use crate::lock;
use crate::session::SessionBuilder;
use crate::wire::ConnectionId;

/// The side runs of the open sessions, by session.
pub(super) type SessionSideRuns = std::sync::Mutex<HashMap<String, Arc<SideRuns>>>;

/// The client a connection's side runs belong to: the old daemon's runs
/// belonged to a connection's client, which a wire2 connection is.
fn client_of(connection: ConnectionId) -> String {
    format!("connection-{}", connection.0)
}

/// `args` as the registry reads them: on behalf of `connection`'s client.
fn on_behalf_of(args: &Value, connection: ConnectionId) -> Value {
    let mut payload = args.clone();
    if let Some(payload) = payload.as_object_mut() {
        payload.insert("clientId".to_string(), json!(client_of(connection)));
    }
    payload
}

impl<B: SessionBuilder> RuntimeHandlers<B> {
    /// `session.start_side_question` (see the module documentation).
    pub(super) async fn start_side_question(
        &self,
        args: &Value,
        connection: ConnectionId,
    ) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let topic = Topic::new(format!("agent/{id}/side_question"))?;
        let live = self.runtime.built_engine(&id).await?;
        let runs = Arc::clone(lock(&self.side_runs).entry(id.clone()).or_default());
        let admitted = runs
            .admit(&on_behalf_of(args, connection), &id)
            .map_err(anyhow::Error::msg)?;
        // The run's events go out one at a time, in the order it sends them.
        let (events, mut sending) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let runtime = Arc::clone(&self.runtime);
        let session = id.clone();
        tokio::spawn(async move {
            while let Some(payload) = sending.recv().await {
                let published = runtime
                    .publish(NewEvent {
                        topic: topic.clone(),
                        source: "runtime".to_string(),
                        session: Some(session.clone()),
                        payload,
                        trust: Trust::Owner,
                        cause: None,
                        reply: None,
                    })
                    .await;
                if let Err(error) = published {
                    tracing::warn!(
                        "a side question event of {session} was not published: {error:#}"
                    );
                }
            }
        });
        let emit: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(move |event| {
            // The sender ends with the daemon, and the events with it.
            let _ = events.send(event);
        });
        let (engine, session_runtime) = (live.engine, live.runtime);
        tokio::spawn(drive(
            runs,
            admitted,
            emit,
            move |request, signal, sink| async move {
                // A lifetime that ends mid-run (a crash, a replacement) ends
                // the run as failed.
                session_runtime
                    .spawn(async move { run_side_run(&engine, request, &signal, &sink).await })
                    .await
                    .map_err(|join_error| join_error.to_string())
            },
        ));
        Ok(Value::Null)
    }

    /// `session.abort_side_question` (see the module documentation).
    pub(super) fn abort_side_question(
        &self,
        args: &Value,
        connection: ConnectionId,
    ) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let payload = on_behalf_of(args, connection);
        let aborted = lock(&self.side_runs)
            .get(&id)
            .is_some_and(|runs| runs.abort(&payload));
        Ok(json!({ "aborted": aborted }))
    }

    /// `session.record_better_outcome` (see the module documentation).
    pub(super) async fn record_better_outcome(&self, args: &Value) -> anyhow::Result<Value> {
        let id = self.open_session(args)?;
        let outcome = better_outcome(args).map_err(anyhow::Error::msg)?;
        let live = self.runtime.built_engine(&id).await?;
        let engine = live.engine;
        // On the session's runtime, which the learning's save is spawned on.
        live.runtime
            .spawn(async move { engine.better.note_composer_outcome(outcome) });
        Ok(Value::Null)
    }

    /// Aborts the side runs of `connection`'s client, which ended: each still
    /// sends its cancelled event.
    pub(super) fn abort_connection_side_runs(&self, connection: ConnectionId) {
        let client = client_of(connection);
        for runs in lock(&self.side_runs).values() {
            runs.abort_for_client(&client);
        }
    }

    /// Aborts session `id`'s side runs, which stops, refusing new ones, and
    /// waits (bounded) for their cancelled events.
    pub(super) async fn settle_side_runs(&self, id: &str) {
        let runs = lock(&self.side_runs).get(id).cloned();
        if let Some(runs) = runs {
            runs.abort_all_and_settle(SIDE_RUN_SETTLE_TIMEOUT).await;
        }
    }
}
