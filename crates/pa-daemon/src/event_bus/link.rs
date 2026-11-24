//! The worker's side of the bus: the `event.*` kernel requests of a daemon
//! session go to the supervisor's bus as `event_bus` commands, authenticated
//! by the worker's token.

use std::sync::Arc;
use std::time::Duration;

use pa_core::session_engine::event_bus_host::EventBusController;
use pa_types::events::EventBusRequest;
use serde_json::{json, Value};

use crate::supervisor_link::SupervisorLink;

/// How long one bus request may take on the supervisor link. A publication
/// answers before its deliveries are routed, so no request waits on another
/// session.
const EVENT_BUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The event bus controller of a daemon worker.
pub(crate) struct LinkEventBusController {
    link: Arc<SupervisorLink>,
    worker_token: String,
}

impl LinkEventBusController {
    pub(crate) fn new(link: Arc<SupervisorLink>, worker_token: String) -> Self {
        LinkEventBusController { link, worker_token }
    }
}

impl EventBusController for LinkEventBusController {
    async fn request(&self, request: EventBusRequest) -> anyhow::Result<Value> {
        self.link
            .request_success(
                json!({
                    "type": "event_bus",
                    "workerToken": self.worker_token,
                    "request": request,
                }),
                EVENT_BUS_REQUEST_TIMEOUT,
            )
            .await
    }
}
