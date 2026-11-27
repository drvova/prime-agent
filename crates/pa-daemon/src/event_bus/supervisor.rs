//! The supervisor's `event_bus` arm: authenticate the requesting worker, apply
//! the request to the bus, and route a publication's deliveries.
//!
//! A delivery is an agent message to the subscriber (`worker_deliver_message`
//! with `messageKind` `broadcast`, so it never ends an `event-wait`),
//! from a sender named `event:<source>` that carries the publishing session's
//! id: the receiver's wake budget then holds a publisher that wakes the same
//! subscriber in a loop. A saved subscriber that is not resident is woken from
//! the catalog first. The publisher's answer does not wait for the routing:
//! it names the event and how many subscriptions matched, and each
//! subscription's `deliveredCount` shows what reached its session.

use std::sync::Arc;

use pa_types::daemon::{DaemonCommand, DaemonWorkerCommand};
use serde_json::{json, Map, Value};

use super::{Delivery, Handled, Requester, Settled};
use crate::backpressure::RouteAdmission;
use crate::messaging::{WakeOutcome, WORKER_REQUEST_TIMEOUT_MS};
use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::registry::ResidentWorker;
use crate::supervisor::Supervisor;

impl Supervisor {
    /// `event_bus`: one session's request to the bus.
    pub(crate) async fn handle_event_bus(
        self: &Arc<Self>,
        command: &DaemonCommand,
        command_id: &str,
        type_name: &str,
    ) -> DaemonResponse {
        let fail = |error: String| response_failure(Some(command_id), type_name, &error, None);
        let DaemonCommand::EventBus {
            worker_token,
            request,
            ..
        } = command
        else {
            return fail("invalid command".to_string());
        };
        let Some(resident) = self.registry.find_by_token(worker_token).await else {
            return fail("Worker authentication failed".to_string());
        };
        let summary = match self.source_worker_summary(&resident).await {
            Ok(summary) => summary,
            Err(error) => return fail(format!("{error:#}")),
        };
        let text = |key: &str| {
            summary
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let Some(session_id) = text("sessionId").or_else(|| text("activeSessionId")) else {
            return fail("the requesting session has no id yet".to_string());
        };
        let requester = Requester {
            name: text("sessionName").unwrap_or_else(|| session_id.clone()),
            cwd: text("cwd").unwrap_or_default(),
            session_id,
        };
        let handled =
            match self
                .event_bus
                .handle(&requester, request.clone(), crate::util::now_ms())
            {
                Ok(handled) => handled,
                Err(error) => return fail(format!("{error:#}")),
            };
        match handled {
            Handled::Answer(data) => response_success(Some(command_id), type_name, Some(data)),
            Handled::Publish(publication) => {
                let matched = publication.deliveries.len();
                let answer = json!({ "eventId": publication.event_id, "matched": matched });
                let supervisor = Arc::clone(self);
                tokio::spawn(async move {
                    for delivery in publication.deliveries {
                        supervisor
                            .deliver_event(&resident, &requester, delivery)
                            .await;
                    }
                });
                response_success(Some(command_id), type_name, Some(answer))
            }
        }
    }

    /// Route one event to its subscriber, waking a saved one, and settle it on the bus.
    async fn deliver_event(
        self: &Arc<Self>,
        publisher: &Arc<ResidentWorker>,
        requester: &Requester,
        delivery: Delivery,
    ) {
        let Delivery {
            subscription,
            message,
        } = delivery;
        let target = match self.registry.resolve(&subscription.session_id).await {
            Ok(target) => target,
            Err(error) => {
                let scope = json!({ "cwd": subscription.cwd });
                match self
                    .wake_saved_target(&error, &subscription.session_id, Some((publisher, &scope)))
                    .await
                {
                    WakeOutcome::Woken(target) => target,
                    WakeOutcome::Unknown => {
                        if let Err(error) = self
                            .event_bus
                            .settle(&subscription.id, Settled::SubscriberGone)
                        {
                            self.log_line(&format!(
                                "event bus: dropping a gone subscriber's subscription: {error:#}"
                            ));
                        }
                        return;
                    }
                    WakeOutcome::Failed(error) => {
                        self.log_line(&format!("event bus: subscription {}: the subscriber could not be woken: {error}", subscription.id));
                        return;
                    }
                }
            }
        };
        let delivery = DaemonWorkerCommand::WorkerDeliverMessage {
            id: None,
            target_active_session_id: target.worker_id.clone(),
            message,
            sender: json!({
                "sessionId": requester.session_id,
                "sessionName": format!("event:{}", subscription.source),
                "runtimeKind": "event",
            }),
            delivery_mode: Some(json!(subscription.delivery_mode.as_str())),
            rest: Map::from_iter([(
                "messageKind".to_string(),
                json!(pa_core::session_engine::agent_messaging::AgentMessageKind::BROADCAST_WIRE),
            )]),
        };
        let payload = match serde_json::to_value(&delivery) {
            Ok(payload) => payload,
            Err(error) => {
                self.log_line(&format!(
                    "event bus: the delivery does not serialize: {error}"
                ));
                return;
            }
        };
        let response = self
            .route_command_typed(
                &target,
                "worker_deliver_message",
                payload,
                WORKER_REQUEST_TIMEOUT_MS,
                RouteAdmission::SupervisorInternal,
            )
            .await;
        match response {
            Ok(response)
                if response.success
                    && response
                        .data
                        .as_ref()
                        .and_then(|data| data.get("deliveryStatus"))
                        .and_then(Value::as_str)
                        != Some(
                            pa_core::session_engine::agent_messaging::AGENT_MESSAGE_WAKE_REFUSED_STATUS,
                        ) =>
            {
                if let Err(error) = self.event_bus.settle(&subscription.id, Settled::Delivered) {
                    self.log_line(&format!("event bus: counting a delivery: {error:#}"));
                }
            }
            Ok(response) => self.log_line(&format!(
                "event bus: subscription {}: the subscriber did not take the event: {}",
                subscription.id,
                response.error.as_deref().unwrap_or("wake refused")
            )),
            Err(error) => self.log_line(&format!(
                "event bus: subscription {}: routing the event failed: {error:#}",
                subscription.id
            )),
        }
    }
}
