//! The kernel's `event-*` functions (`kernel/scheme/host_api/events.scm`) over
//! the daemon's event bus: one host request per bus operation, each parsed
//! into a typed [`EventBusRequest`] and sent through an [`EventBusController`].
//!
//! The bus itself lives in the daemon's supervisor (every session of the
//! daemon shares it); a daemon worker registers its link-backed controller
//! over the default this module's [`NoEventBus`] gives a daemonless run, which
//! refuses every call with the reason.

use std::future::Future;
use std::sync::Arc;

use anyhow::anyhow;
use pa_types::events::EventBusRequest;
use serde_json::Value;

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Reaches the event bus on the calling session's behalf.
///
/// An implementation sends the request as the session that owns it (the bus
/// authorizes ownership by that identity) and returns the bus's answer
/// verbatim: a source or subscription record, `{eventId, matched}` for a
/// publication, `{sources, subscriptions}` for a listing. A refused request
/// is an error whose message is the bus's reason, shown to the model as is.
pub trait EventBusController: Send + Sync {
    /// Send one request to the bus and wait for its answer.
    fn request(
        &self,
        request: EventBusRequest,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send;
}

/// The controller of a run with no daemon: every request is refused.
pub struct NoEventBus;

impl EventBusController for NoEventBus {
    async fn request(&self, _request: EventBusRequest) -> anyhow::Result<Value> {
        Err(anyhow!(
            "the event bus runs in the Prime Agent daemon; this session runs without one"
        ))
    }
}

/// One `event.*` call as telemetry counts it (never a name, a payload, or a prompt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventRequestFact {
    SourceDefined,
    /// A publication, with how many subscriptions it matched.
    Published {
        matched: u64,
    },
    Subscribed,
    Unsubscribed,
    /// A source removal or a listing.
    Other,
    /// Any call the bus refused or could not answer.
    Failed,
}

/// Hears each `event.*` call.
pub(crate) type EventRequestObserver = Arc<dyn Fn(EventRequestFact) + Send + Sync>;

/// Register the `event.*` host requests over `controller`.
pub fn register_event_bus_host_handlers<C: EventBusController + 'static>(
    controller: &Arc<C>,
    handlers: &mut HostRequestHandlers,
) {
    for request_type in [
        "event.define_source",
        "event.remove_source",
        "event.publish",
        "event.subscribe",
        "event.unsubscribe",
        "event.list",
    ] {
        let controller = Arc::clone(controller);
        handlers.register(
            request_type,
            host_handler(move |payload| {
                let controller = Arc::clone(&controller);
                async move {
                    let mut fields = match payload.data {
                        Value::Object(fields) => fields,
                        Value::Null => serde_json::Map::new(),
                        other => {
                            return Err(anyhow!(
                                "{request_type}: the payload is not an object: {other}"
                            ))
                        }
                    };
                    fields.insert(
                        "op".to_string(),
                        Value::from(request_type.trim_start_matches("event.")),
                    );
                    let request: EventBusRequest = serde_json::from_value(Value::Object(fields))
                        .map_err(|error| anyhow!("{request_type}: {error}"))?;
                    controller.request(request).await
                }
            }),
        );
    }
}

/// Count each `event.*` call into `observer`: wraps the registered handlers (a no-op for a
/// request type none is registered for).
pub(crate) fn observe_event_bus_requests(
    handlers: &mut HostRequestHandlers,
    observer: &EventRequestObserver,
) {
    for request_type in [
        "event.define_source",
        "event.remove_source",
        "event.publish",
        "event.subscribe",
        "event.unsubscribe",
        "event.list",
    ] {
        let Some(handler) = handlers.get(request_type).cloned() else {
            continue;
        };
        let observer = Arc::clone(observer);
        handlers.register(
            request_type,
            host_handler(move |payload| {
                let handler = Arc::clone(&handler);
                let observer = Arc::clone(&observer);
                async move {
                    let result = handler(payload).await;
                    observer(match (&result, request_type) {
                        (Err(_), _) => EventRequestFact::Failed,
                        (Ok(_), "event.define_source") => EventRequestFact::SourceDefined,
                        (Ok(answer), "event.publish") => EventRequestFact::Published {
                            matched: answer.get("matched").and_then(Value::as_u64).unwrap_or(0),
                        },
                        (Ok(_), "event.subscribe") => EventRequestFact::Subscribed,
                        (Ok(_), "event.unsubscribe") => EventRequestFact::Unsubscribed,
                        (Ok(_), _) => EventRequestFact::Other,
                    });
                    result
                }
            }),
        );
    }
}

#[cfg(test)]
mod tests;
