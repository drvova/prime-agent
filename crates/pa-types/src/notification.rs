//! The kernel `notify(...)` ping's wire vocabulary: the `kernel_notification` session event an
//! attached surface renders as a notice row (and, where its settings allow, a terminal attention
//! signal). The hub that publishes it lives in pa-core.

use serde::{Deserialize, Serialize};

/// The session event a kernel notification rides.
pub const NOTIFICATION_EVENT: &str = "kernel_notification";

/// How a notification reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    /// Something to know: progress, a finished step.
    Info,
    /// The work finished.
    Success,
    /// The user should look soon: a decision, a stalled step.
    Warning,
    /// Something failed.
    Error,
}

/// One kernel notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelNotification {
    pub message: String,
    pub level: NotificationLevel,
}
