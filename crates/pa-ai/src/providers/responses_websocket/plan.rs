//! The per-request transport choice for a provider that can speak the Responses WebSocket mode.
//!
//! Explicit `sse` / `websocket` / `websocket-cached` settings decide on their own. `auto` (the
//! default) picks the WebSocket where it pays off and the endpoint can serve it: the request must
//! be able to continue (a session whose next request reuses this one's connection state), and the
//! endpoint must speak the mode — declared by the registry, learned by an earlier probe, or probed
//! now when unknown. A session demoted to SSE after WebSocket failures stays on SSE.

use crate::types::{StreamOptions, Transport};

/// What is known about an endpoint's WebSocket mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capability {
    /// The built-in registry data declares it (docs-verified endpoints).
    Declared(bool),
    /// An earlier probe found it (see [`super::learned_capability`]).
    Learned(bool),
    /// Nothing declared or learned: `auto` probes it.
    Unknown,
}

/// Why a request rides SSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseReason {
    /// The transport setting asks for SSE.
    Requested,
    /// Nothing can continue the request (no session), so a delta can never pay off.
    OneShot,
    /// The provider's API has no WebSocket mode.
    NoWebSocketWire,
    /// The endpoint is declared or learned not to speak the WebSocket mode.
    Unsupported,
    /// The session was demoted to SSE after WebSocket failures.
    Demoted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportPlan {
    /// Open (or reuse) the WebSocket; `probe` marks an endpoint whose capability is unknown, so a
    /// failure before any output records it as unsupported and falls back silently.
    WebSocket {
        probe: bool,
    },
    Sse(SseReason),
}

/// The transport inputs of one request.
pub(crate) struct PlanInput {
    pub(crate) transport: Transport,
    pub(crate) has_websocket_wire: bool,
    pub(crate) has_session: bool,
    pub(crate) demoted: bool,
    pub(crate) capability: Capability,
}

pub(crate) fn plan_transport(input: &PlanInput) -> TransportPlan {
    if !input.has_websocket_wire {
        return TransportPlan::Sse(SseReason::NoWebSocketWire);
    }
    match input.transport {
        Transport::Sse => TransportPlan::Sse(SseReason::Requested),
        Transport::Websocket | Transport::WebsocketCached | Transport::Auto if input.demoted => {
            TransportPlan::Sse(SseReason::Demoted)
        }
        Transport::Websocket | Transport::WebsocketCached => TransportPlan::WebSocket {
            probe: input.capability == Capability::Unknown,
        },
        Transport::Auto if !input.has_session => TransportPlan::Sse(SseReason::OneShot),
        Transport::Auto => match input.capability {
            Capability::Declared(true) | Capability::Learned(true) => {
                TransportPlan::WebSocket { probe: false }
            }
            Capability::Declared(false) | Capability::Learned(false) => {
                TransportPlan::Sse(SseReason::Unsupported)
            }
            Capability::Unknown => TransportPlan::WebSocket { probe: true },
        },
    }
}

/// Count the `auto` choice of a request in its session's transport counts.
pub(crate) fn note_plan(session_id: Option<&str>, transport: Transport, plan: TransportPlan) {
    let Some(session_id) = session_id else {
        return;
    };
    super::note_counts(session_id, |counts| {
        if let TransportPlan::WebSocket { probe: true } = plan {
            counts.probes += 1;
        }
        match (transport, plan) {
            (Transport::Auto, TransportPlan::WebSocket { .. }) => counts.auto_websocket += 1,
            (Transport::Auto, TransportPlan::Sse(SseReason::OneShot)) => {
                counts.auto_sse_one_shot += 1;
            }
            (Transport::Auto, TransportPlan::Sse(SseReason::NoWebSocketWire)) => {
                counts.auto_sse_no_websocket_wire += 1;
            }
            (Transport::Auto, TransportPlan::Sse(SseReason::Unsupported)) => {
                counts.auto_sse_unsupported += 1;
            }
            (Transport::Auto, TransportPlan::Sse(SseReason::Demoted)) => {
                counts.auto_sse_demoted += 1;
            }
            (
                Transport::Websocket | Transport::WebsocketCached,
                TransportPlan::Sse(SseReason::NoWebSocketWire),
            ) => counts.websocket_requested_without_wire += 1,
            (Transport::Auto, TransportPlan::Sse(SseReason::Requested))
            | (Transport::Sse | Transport::Websocket | Transport::WebsocketCached, _) => {}
        }
    });
}

/// The APIs whose providers implement the WebSocket mode; every other API rides SSE (or its own
/// HTTP stream) whatever the transport setting asks, without an error.
const WEBSOCKET_WIRE_APIS: &[&str] = &["openai-responses", "openai-codex-responses"];

/// Count a request on a provider without a WebSocket mode (the WebSocket-capable providers count
/// their own choice).
pub(crate) fn note_dispatch(api: &str, options: Option<&StreamOptions>) {
    if WEBSOCKET_WIRE_APIS.contains(&api) {
        return;
    }
    let Some(options) = options else {
        return;
    };
    let transport = options.transport.unwrap_or(Transport::Auto);
    let plan = plan_transport(&PlanInput {
        transport,
        has_websocket_wire: false,
        has_session: options.session_id.is_some(),
        demoted: false,
        capability: Capability::Unknown,
    });
    note_plan(options.session_id.as_deref(), transport, plan);
}

/// Debug name for the transport in diagnostics.
pub(crate) fn transport_debug_name(transport: Transport) -> &'static str {
    match transport {
        Transport::Sse => "sse",
        Transport::Websocket => "websocket",
        Transport::WebsocketCached => "websocket-cached",
        Transport::Auto => "auto",
    }
}

/// Whether one parsed Responses WebSocket event can produce assistant output: lifecycle,
/// telemetry, and vendor metadata stay internal and never mark the attempt user-visible.
pub(crate) fn is_visible_response_event(event: &serde_json::Value) -> bool {
    let Some(event_type) = event.get("type").and_then(serde_json::Value::as_str) else {
        return false;
    };
    // `response.done` normalizes to `response.completed` before the TS classifier sees a mapped
    // event, so the raw-type check here must accept it too (a terminal-only stream still starts
    // visibly).
    event_type == "response.completed"
        || event_type == "response.done"
        || event_type == "response.incomplete"
        || event_type.starts_with("response.output_")
        || event_type.starts_with("response.reasoning_")
        || event_type.starts_with("response.content_")
        || event_type.starts_with("response.refusal.")
        || event_type.starts_with("response.function_")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider whose API has no WebSocket mode rides SSE under an explicit `websocket`
    /// setting instead of failing, and the request is counted as asking for a missing wire.
    #[test]
    fn websocket_setting_on_a_provider_without_a_websocket_wire_rides_sse() {
        let session = format!("no-wire-{}", std::process::id());
        let options = StreamOptions {
            transport: Some(Transport::Websocket),
            session_id: Some(session.clone()),
            ..Default::default()
        };
        note_dispatch("anthropic-messages", Some(&options));
        assert_eq!(
            super::super::session_counts(&session),
            Some(super::super::ResponsesTransportCounts {
                websocket_requested_without_wire: 1,
                ..Default::default()
            })
        );
        assert_eq!(
            plan_transport(&PlanInput {
                transport: Transport::Websocket,
                has_websocket_wire: false,
                has_session: true,
                demoted: false,
                capability: Capability::Declared(true),
            }),
            TransportPlan::Sse(SseReason::NoWebSocketWire)
        );
    }
}
