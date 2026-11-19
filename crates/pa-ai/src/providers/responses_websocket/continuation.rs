//! Connection-anchored continuation: the previous request and response a connection's server-side
//! state holds, and the input delta a follow-up request sends with `previous_response_id`.

use serde_json::{json, Value};

pub(crate) struct ContinuationState {
    pub last_request_body: Value,
    pub last_response_id: String,
    pub last_response_items: Vec<Value>,
    /// Id of the connection whose server-side response state this chain is anchored to.
    pub connection_id: u64,
}

/// Compute the continuation delta (input items beyond the cached baseline) for an
/// otherwise-identical request body.
pub(crate) fn get_cached_websocket_input_delta(
    body: &Value,
    continuation: &ContinuationState,
) -> Option<Vec<Value>> {
    let strip = |value: &Value| -> Value {
        let mut stripped = value.clone();
        if let Some(map) = stripped.as_object_mut() {
            map.remove("input");
            map.remove("previous_response_id");
        }
        stripped
    };
    if strip(body) != strip(&continuation.last_request_body) {
        return None;
    }
    let current_input = body.get("input").and_then(Value::as_array).cloned()?;
    let last_input = continuation
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut baseline = last_input;
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current_input.len() < baseline.len() {
        return None;
    }
    let prefix = &current_input[..baseline.len()];
    if prefix != baseline.as_slice() {
        return None;
    }
    Some(current_input[baseline.len()..].to_vec())
}

pub(crate) fn build_cached_websocket_request_body(
    continuation: Option<&ContinuationState>,
    body: &Value,
    connection_id: u64,
) -> Value {
    let Some(continuation) = continuation else {
        return body.clone();
    };
    // Continuations are anchored to the connection that produced the response; a different socket
    // cannot resolve their previous_response_id.
    if continuation.connection_id != connection_id {
        return body.clone();
    }
    let delta = get_cached_websocket_input_delta(body, continuation);
    match delta {
        Some(delta) if !continuation.last_response_id.is_empty() => {
            let mut request = body.clone();
            request["previous_response_id"] = json!(continuation.last_response_id);
            request["input"] = Value::Array(delta);
            request
        }
        _ => body.clone(),
    }
}
