//! Learned capabilities per endpoint host: whether it serves the Responses API over HTTP and its
//! WebSocket mode, as a probe (a request's own WebSocket attempt, or background discovery) found. Verdicts live in memory and in a small cache file in the agent dir (written
//! off the request path) so a later session does not re-probe a host, and expire after a week.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::utils_inner::diagnostics::now_ms;

/// Built-in registry data: base URLs whose Responses WebSocket mode (and so the Responses route)
/// the provider's own documentation verifies. They seed the capability and are never probed;
/// every other endpoint is learned.
/// - `https://api.openai.com/v1`: <https://developers.openai.com/api/docs/guides/websocket-mode>
/// - `https://api.x.ai/v1`: <https://docs.x.ai/developers/advanced-api-usage/websocket-mode>
const VERIFIED_WEBSOCKET_BASE_URLS: &[&str] = &["https://api.openai.com/v1", "https://api.x.ai/v1"];

/// Whether the docs verify the base URL's Responses WebSocket mode.
pub(crate) fn is_verified_endpoint(base_url: &str) -> bool {
    VERIFIED_WEBSOCKET_BASE_URLS.contains(&base_url.trim_end_matches('/'))
}

/// How long a learned verdict holds before the endpoint is probed again.
const LEARNED_CAPABILITY_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// What is known about one endpoint host; `None` fields are unknown (never probed, or the probe
/// was inconclusive).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearnedEndpoint {
    /// `POST {base}/responses` is routed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_http: Option<bool>,
    /// The Responses WebSocket mode answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket: Option<bool>,
    /// When the endpoint was last checked (epoch milliseconds).
    pub checked_at: u64,
}

type Verdict = LearnedEndpoint;

#[derive(Default)]
struct Learned {
    path: Option<PathBuf>,
    loaded: bool,
    verdicts: HashMap<String, Verdict>,
}

fn learned() -> &'static Mutex<Learned> {
    static LEARNED: OnceLock<Mutex<Learned>> = OnceLock::new();
    LEARNED.get_or_init(|| Mutex::new(Learned::default()))
}

/// Persist learned verdicts at `path` (the host sets it once per process: the agent dir's
/// `responses-transport.json`). Without a path, verdicts live in memory only.
pub fn set_capability_cache_path(path: PathBuf) {
    if let Ok(mut learned) = learned().lock() {
        if learned.path.as_ref() != Some(&path) {
            learned.path = Some(path);
            learned.loaded = false;
        }
    }
}

/// The verdict key for a base URL: its host and port (never a path, a query, or a credential).
pub(crate) fn endpoint_key(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let host = url.host_str()?;
    Some(match url.port_or_known_default() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// The unexpired learned WebSocket verdict for an endpoint, if a probe recorded one.
pub(crate) fn learned_capability(key: &str) -> Option<bool> {
    learned_endpoint_by_key(key)?.websocket
}

/// The unexpired learned record for a base URL's endpoint host.
#[must_use]
pub fn learned_endpoint(base_url: &str) -> Option<LearnedEndpoint> {
    learned_endpoint_by_key(&endpoint_key(base_url)?)
}

fn learned_endpoint_by_key(key: &str) -> Option<LearnedEndpoint> {
    let mut learned = learned().lock().ok()?;
    if !learned.loaded {
        learned.loaded = true;
        if let Some(path) = learned.path.clone() {
            let stored: HashMap<String, Verdict> = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            for (host, verdict) in stored {
                learned.verdicts.entry(host).or_insert(verdict);
            }
        }
    }
    let verdict = learned.verdicts.get(key).copied()?;
    if now_ms().saturating_sub(verdict.checked_at) >= LEARNED_CAPABILITY_TTL_MS {
        learned.verdicts.remove(key);
        return None;
    }
    Some(verdict)
}

/// Record a request's WebSocket probe verdict.
pub(crate) fn record_learned_capability(key: &str, websocket: bool) {
    record_learned(key, |verdict| verdict.websocket = Some(websocket));
}

/// Update an endpoint's record (a conclusive finding replaces the field it covers; the others
/// keep their values) and stamp it checked now. The cache file is rewritten on a blocking worker,
/// never awaited.
pub(crate) fn record_learned(key: &str, update: impl FnOnce(&mut LearnedEndpoint)) {
    // The first lookup loads the file; load it before the first write so a write never drops it.
    let _ = learned_endpoint_by_key(key);
    let Ok(mut learned) = learned().lock() else {
        return;
    };
    let verdict = learned.verdicts.entry(key.to_string()).or_default();
    update(verdict);
    verdict.checked_at = now_ms();
    let Some(path) = learned.path.clone() else {
        return;
    };
    let now = now_ms();
    let snapshot: HashMap<&String, &Verdict> = learned
        .verdicts
        .iter()
        .filter(|(_, verdict)| now.saturating_sub(verdict.checked_at) < LEARNED_CAPABILITY_TTL_MS)
        .collect();
    let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) else {
        return;
    };
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    // A failed write only costs a re-probe in a later session.
    runtime.spawn_blocking(move || {
        use pa_types::platform::{Durability, FileMode, ReplaceOptions, Symlinks};
        let options = ReplaceOptions {
            durability: Durability::Unsynced,
            mode: FileMode::KeepTarget,
            symlinks: Symlinks::FollowToTarget,
        };
        if let Err(error) = pa_types::platform::replace_file(&path, &bytes, options) {
            tracing::debug!(%error, "learned Responses capability not saved");
        }
    });
}

/// Seed an endpoint's learned Responses route (tests of the paths that read it).
#[cfg(test)]
pub(crate) fn record_learned_for_test(base_url: &str, responses_http: Option<bool>) {
    record_learned(
        &endpoint_key(base_url).expect("a test base URL"),
        |learned| {
            learned.responses_http = responses_http;
        },
    );
}
