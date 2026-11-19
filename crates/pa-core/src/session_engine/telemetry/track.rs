//! The one-shot daemon/worker event trackers: the `daemon event` and
//! `model refused` surfaces the supervisor and model-allowlist seams call
//! once per lifecycle event. Counts/categories only, never session
//! payload (the module's privacy contract).
use super::{base_properties, model_category, provider_category, TelemetryClient, Value};

/// Track the supervision summary (`daemon event`, kind `summary`): the
/// frequent supervision events of one window as `<kind>_count`
/// properties. Kinds and counts only, never session payload.
pub fn track_daemon_event_summary(
    client: &TelemetryClient,
    window_ms: u64,
    counts: &std::collections::BTreeMap<String, u64>,
    saved_sessions_usage_rows_max: u64,
) {
    let mut properties = base_properties("daemon");
    properties.set("kind", Value::from("summary"));
    properties.set("window_ms", Value::from(window_ms));
    for (key, count) in counts {
        properties.set(key, Value::from(*count));
    }
    if counts.contains_key("saved_sessions_list_count") {
        properties.set(
            "saved_sessions_usage_rows_max",
            Value::from(saved_sessions_usage_rows_max),
        );
    }
    client.track("daemon event", properties);
}

/// Track a deleted subagent's durable usage capture (`daemon event`, kind
/// `deleted_child_usage_captured`): how many tombstoned edges received the usage amendment
/// that keeps a deleted child's spend billable after its transcript goes. Source and count
/// only, never the spend values.
pub fn track_deleted_child_usage_captured(client: &TelemetryClient, source: &str, count: usize) {
    let mut properties = base_properties("daemon");
    properties.set("kind", Value::from("deleted_child_usage_captured"));
    properties.set("source", Value::from(source));
    properties.set("count", Value::from(count));
    client.track("daemon event", properties);
}

/// Track a daemon model-allowlist refusal (`model refused`, schema v1): a daemon model
/// resolution refused a model outside the settings `allowedModels` allowlist. Categories and
/// surface only — never the refused selector, pattern content, or session payload.
pub fn track_model_refused(
    client: &TelemetryClient,
    surface: &str,
    provider: &str,
    model_id: &str,
) {
    let mut properties = base_properties("daemon");
    properties.set("surface", Value::from(surface));
    properties.set(
        "provider_category",
        Value::from(provider_category(Some(provider))),
    );
    properties.set("model_category", Value::from(model_category(model_id)));
    client.track("model refused", properties);
}

/// Track the boot descriptor-adoption pass's `daemon event` (schema v1, kind
/// `worker_adoption`): the boot kind and per-outcome counts, never session payload.
/// `skipped_idle` counts the dead descriptors the busy-evidence filter parked.
pub fn track_worker_adoption(
    client: &TelemetryClient,
    boot: &str,
    adopted_live: usize,
    revived: usize,
    skipped_idle: usize,
    stopped: usize,
    failed: usize,
) {
    let mut properties = base_properties("daemon");
    properties.set("kind", Value::from("worker_adoption"));
    properties.set("boot", Value::from(boot));
    properties.set("adopted_live", Value::from(adopted_live));
    properties.set("revived", Value::from(revived));
    properties.set("skipped_idle", Value::from(skipped_idle));
    properties.set("stopped", Value::from(stopped));
    properties.set("failed", Value::from(failed));
    client.track("daemon event", properties);
}

/// Track the live-catalog warm-up settle's `daemon event` (schema v1, kind
/// `catalog_refresh`): how many models the resolved no-cold-start chain serves after the
/// startup refresh. A count only, never model ids, credentials, or catalog payloads.
pub fn track_catalog_refresh(client: &TelemetryClient, count: usize) {
    let mut properties = base_properties("daemon");
    properties.set("kind", Value::from("catalog_refresh"));
    properties.set("count", Value::from(count));
    client.track("daemon event", properties);
}

/// Track the abort supervision's terminal declaration (`daemon event`,
/// schema v1, kind `compaction_abort_declared`): the supervisor declared
/// a wedged worker's compaction aborted after its abort grace expired. A
/// count only, never session payload.
pub fn track_compaction_abort_declared(client: &TelemetryClient) {
    let mut properties = base_properties("daemon");
    properties.set("kind", Value::from("compaction_abort_declared"));
    properties.set("count", Value::from(1));
    client.track("daemon event", properties);
}

/// Track the parent-death child close's `daemon event` (kind `worker_children_closed`):
/// how many resident RLM children the supervisor stopped with a hard-killed parent worker.
/// A count only, never session payload.
pub fn track_worker_children_closed(client: &TelemetryClient, count: usize) {
    let mut properties = base_properties("daemon");
    properties.set("kind", Value::from("worker_children_closed"));
    properties.set("count", Value::from(count));
    client.track("daemon event", properties);
}

/// The surface a descriptor `oauth` sign-in ran on (`agent provider oauth
/// login` `surface`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthLoginSurface {
    /// `login <provider>`.
    Cli,
    /// The TUI's `/login` picker.
    Tui,
}

/// Track a settled descriptor `oauth` sign-in (`agent provider oauth login`, schema v2): the
/// surface, the flow it ran, how it ended, and its duration. Never the provider, a URL, a client
/// id, or a token.
pub fn track_provider_oauth_login(
    client: &TelemetryClient,
    surface: OAuthLoginSurface,
    attempt: &crate::auth::SignInAttempt,
    duration_ms: u64,
) {
    let (mode, surface) = match surface {
        OAuthLoginSurface::Cli => ("cli", "cli"),
        OAuthLoginSurface::Tui => ("interactive", "tui"),
    };
    let flow = match attempt.method {
        Some(crate::auth::SignInMethod::Browser) => "browser",
        Some(crate::auth::SignInMethod::DeviceCode) => "device_code",
        None => "none",
    };
    let outcome = match &attempt.result {
        Ok(()) => "completed",
        Err(crate::auth::SignInError::Cancelled) => "canceled",
        Err(crate::auth::SignInError::Failed(_)) => "failed",
    };
    let mut properties = base_properties(mode);
    properties.set("surface", Value::from(surface));
    properties.set("flow", Value::from(flow));
    properties.set("outcome", Value::from(outcome));
    properties.set("duration_ms", Value::from(duration_ms));
    client.track("agent provider oauth login", properties);
}

/// Track a settled history import (`agent sessions imported`, schema v2): who ran it, how it
/// ended, the counts, and the copied bytes as a coarse bucket. Never a path or a session id.
/// `trigger` is `automatic` (the daemon's boot run, execution mode `daemon`) or `command`
/// (`import-sessions`, execution mode `cli`).
pub fn track_sessions_imported(
    client: &TelemetryClient,
    trigger: &str,
    outcome: &str,
    report: &crate::settings::HistoryImportReport,
    duration_ms: u64,
) {
    const MB: u64 = 1024 * 1024;
    let bucket = match report.bytes_imported {
        bytes if bytes < MB => "lt_1mb",
        bytes if bytes < 100 * MB => "lt_100mb",
        bytes if bytes < 1024 * MB => "lt_1gb",
        bytes if bytes < 10 * 1024 * MB => "lt_10gb",
        _ => "ge_10gb",
    };
    let mode = if trigger == "command" {
        "cli"
    } else {
        "daemon"
    };
    let mut properties = base_properties(mode);
    properties.set("trigger", Value::from(trigger));
    properties.set("outcome", Value::from(outcome));
    properties.set(
        "sessions_imported_count",
        Value::from(report.chats_imported),
    );
    properties.set("sessions_kept_count", Value::from(report.kept.len()));
    properties.set("files_imported_count", Value::from(report.files_imported));
    properties.set(
        "config_entries_added_count",
        Value::from(report.config_added.len()),
    );
    properties.set("bytes_bucket", Value::from(bucket));
    properties.set("duration_ms", Value::from(duration_ms));
    client.track("agent sessions imported", properties);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pa_telemetry::{MockSink, TelemetryClient, TelemetryClientConfig};

    use super::*;

    /// The sign-in event carries exactly its catalogued enums and duration
    /// (the catalog drops an unknown key or value, so a typo here would
    /// silently lose the property).
    #[tokio::test]
    async fn a_descriptor_sign_in_tracks_its_catalogued_properties() {
        let mock = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.sinks = vec![mock.clone() as Arc<dyn pa_telemetry::TelemetrySink>];
        let client = TelemetryClient::spawn(config).expect("spawn client");
        track_provider_oauth_login(
            &client,
            OAuthLoginSurface::Tui,
            &crate::auth::SignInAttempt {
                method: Some(crate::auth::SignInMethod::DeviceCode),
                result: Err(crate::auth::SignInError::Cancelled),
            },
            1_234,
        );
        client.shutdown().await.expect("flush");
        let events = mock.events();
        let properties = serde_json::to_value(&events[0].properties).expect("serialize");
        let picked: serde_json::Map<String, serde_json::Value> =
            ["surface", "flow", "outcome", "duration_ms"]
                .iter()
                .map(|key| ((*key).to_string(), properties[*key].clone()))
                .collect();
        assert_eq!(
            (events[0].name.as_str(), serde_json::Value::Object(picked)),
            (
                "agent provider oauth login",
                serde_json::json!({ "surface": "tui", "flow": "device_code",
                    "outcome": "canceled", "duration_ms": 1234 })
            )
        );
    }
}
