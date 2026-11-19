//! The `notify.send` host request: the kernel's non-blocking `notify(...)` ping.
//!
//! `notify.send {message, level?}` publishes one [`KernelNotification`] to the attached surfaces
//! and replies `{delivered}` at once: `true` when a user is attached to see it, `false` when the
//! session runs headless or nobody is attached (the ping is then a recorded no-op). The publish
//! only enqueues a frame, so the kernel never waits on a client, the network, or a paint.

use std::sync::Arc;

use anyhow::{anyhow, bail};
use pa_types::notification::{KernelNotification, NotificationLevel};
use serde_json::{json, Value};

use super::{InteractionEvent, InteractionHub};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// The longest notification, in characters.
const MAX_NOTIFICATION_CHARS: usize = 500;

pub(super) fn register(hub: &Arc<InteractionHub>, handlers: &mut HostRequestHandlers) {
    let hub = Arc::clone(hub);
    handlers.register(
        "notify.send",
        host_handler(move |payload| {
            let hub = Arc::clone(&hub);
            async move {
                let notification = parse(&payload.data)?;
                Ok(json!({ "delivered": notify(&hub, notification) }))
            }
        }),
    );
}

fn parse(data: &Value) -> anyhow::Result<KernelNotification> {
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("notify requires a message string"))?
        .trim();
    if message.is_empty() {
        bail!("notify requires a non-empty message");
    }
    let count = message.chars().count();
    if count > MAX_NOTIFICATION_CHARS {
        bail!("a notification is at most {MAX_NOTIFICATION_CHARS} characters (got {count})");
    }
    let level = match data.get("level") {
        None | Some(Value::Null) => NotificationLevel::Info,
        Some(level) => serde_json::from_value(level.clone()).map_err(|_| {
            anyhow!("level must be one of info, success, warning, error, got {level}")
        })?,
    };
    Ok(KernelNotification {
        message: message.to_string(),
        level,
    })
}

/// Publish `notification`; whether a user is attached to see it.
fn notify(hub: &InteractionHub, notification: KernelNotification) -> bool {
    let delivered = hub
        .surface
        .as_ref()
        .is_some_and(|surface| surface.user_available());
    hub.publish(InteractionEvent::Notified(notification));
    if let Some(counters) = hub.counters() {
        counters.note_notification(delivered);
    }
    delivered
}
