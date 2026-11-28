//! Small shared helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// 12-char hex display id (port of `formatSessionDisplayId`): the last 12
/// hex characters of a random UUID.
#[must_use]
pub fn new_display_id() -> String {
    let normalized: String = uuid::Uuid::new_v4().simple().to_string().to_lowercase();
    normalized[(normalized.len() - 12)..].to_string()
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// RFC 3339 / ISO 8601 UTC timestamp, matching `new Date().toISOString()`.
#[must_use]
pub fn now_iso() -> String {
    pa_core::session::manager::iso_from_unix_ms(now_ms())
}
