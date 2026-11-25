//! The auto-retry events of a turn in the session-event wire shape the
//! headless json stream carries (`auto_retry_start` / `auto_retry_end`).

use pa_core::session_engine::auto_retry::AutoRetryEvent;
use serde_json::{json, Value};

/// `event` as the json stream carries it.
#[must_use]
pub fn auto_retry_event_json(event: AutoRetryEvent) -> Value {
    match event {
        AutoRetryEvent::Start {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => json!({
            "type": "auto_retry_start",
            "attempt": attempt,
            "maxAttempts": max_attempts,
            "delayMs": delay_ms,
            "errorMessage": error_message,
        }),
        AutoRetryEvent::End {
            success,
            attempt,
            final_error,
        } => {
            let mut event = json!({
                "type": "auto_retry_end",
                "success": success,
                "attempt": attempt,
            });
            if let Some(final_error) = final_error {
                event["finalError"] = json!(final_error);
            }
            event
        }
    }
}
