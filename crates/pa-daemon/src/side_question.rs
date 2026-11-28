//! The worker's live side runs (`/btw` and `/better`): the `start_side_question`,
//! `abort_side_question` and `record_better_outcome` handlers over pa-core's shared registry
//! (`pa_core::session_engine::side_runs`, which keeps the admission rules and their error
//! texts), and the `side_question_event` frames a run pushes to the supervisor.

use std::sync::Arc;
use std::time::Duration;

use pa_core::session_engine::side_runs::{better_outcome, drive, SideRuns};
use serde_json::{json, Map, Value};

use crate::engine::SessionEngine;
use crate::protocol::{response_failure, response_success, DaemonOutbound, DaemonResponse};
use crate::worker::{EventPump, OutboundFrame};

pub(crate) struct SideQuestionManager {
    engine: Arc<dyn SessionEngine>,
    events: Arc<EventPump>,
    active_session_id: String,
    runs: Arc<SideRuns>,
}

impl SideQuestionManager {
    pub(crate) fn new(
        engine: Arc<dyn SessionEngine>,
        events: Arc<EventPump>,
        active_session_id: String,
    ) -> Self {
        SideQuestionManager {
            engine,
            events,
            active_session_id,
            runs: Arc::default(),
        }
    }

    /// `start_side_question`: one run per client per session; the response
    /// acknowledges before the run answers, results stream as `side_question_event` frames.
    pub(crate) fn start(&self, payload: &Value) -> DaemonResponse {
        let admitted = match self.runs.admit(payload, &self.active_session_id) {
            Ok(admitted) => admitted,
            Err(error) => return response_failure(None, "start_side_question", &error, None),
        };
        let engine = Arc::clone(&self.engine);
        let emit: Arc<dyn Fn(Value) + Send + Sync> = {
            let (events, active_session_id) = (self.events.clone(), self.active_session_id.clone());
            Arc::new(move |event| emit_side_question_frame(&events, &active_session_id, event))
        };
        // The engine call runs on a blocking thread while the task sends the event frames and
        // owns the registry lifetime.
        tokio::spawn(drive(
            Arc::clone(&self.runs),
            admitted,
            emit,
            move |request, signal, sink| async move {
                tokio::task::spawn_blocking(move || {
                    engine.run_side_question(request, &signal, &sink)
                })
                .await
                .map_err(|join_error| join_error.to_string())
            },
        ));
        response_success(None, "start_side_question", None)
    }

    /// `abort_side_question` (TS handler): only the owner client can abort.
    pub(crate) fn abort(&self, payload: &Value) -> DaemonResponse {
        let aborted = self.runs.abort(payload);
        response_success(
            None,
            "abort_side_question",
            Some(json!({ "aborted": aborted })),
        )
    }

    /// `record_better_outcome`: count what the user did with an inserted rewrite.
    pub(crate) fn record_better_outcome(&self, payload: &Value) -> DaemonResponse {
        match better_outcome(payload) {
            Ok(outcome) => {
                self.engine.note_better_outcome(outcome);
                response_success(None, "record_better_outcome", None)
            }
            Err(error) => response_failure(None, "record_better_outcome", error, None),
        }
    }

    /// Abort every run owned by `client_id` (its detach); each still sends its terminal
    /// cancelled event.
    pub(crate) fn abort_for_client(&self, client_id: &str) {
        self.runs.abort_for_client(client_id);
    }

    /// Abort every live run and wait (bounded) for their terminal events to
    /// queue: the cancelled events must reach the client before the exit.
    pub(crate) async fn abort_all_and_settle(&self, settle_timeout: Duration) {
        self.runs.abort_all_and_settle(settle_timeout).await;
    }
}

fn emit_side_question_frame(events: &Arc<EventPump>, active_session_id: &str, event: Value) {
    let outbound = DaemonOutbound::SideQuestionEvent {
        active_session_id: active_session_id.to_string(),
        event,
        rest: Map::default(),
    };
    let payload = serde_json::to_vec(&outbound).unwrap_or_default();
    events.send(OutboundFrame::side_question_event(payload));
}
