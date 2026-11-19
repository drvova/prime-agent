//! Learned WebSocket capability per endpoint host: what a probe of an endpoint with no declared
//! capability found. Verdicts live in memory and in a small cache file in the agent dir (written
//! off the request path) so a later session does not re-probe a host, and expire after a week.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::utils_inner::diagnostics::now_ms;

/// How long a learned verdict holds before the endpoint is probed again.
const LEARNED_CAPABILITY_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Verdict {
    websocket: bool,
    checked_at: u64,
}

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

/// The unexpired learned verdict for an endpoint, if a probe recorded one.
pub(crate) fn learned_capability(key: &str) -> Option<bool> {
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
    Some(verdict.websocket)
}

/// Record a probe's verdict; the cache file is rewritten on a blocking worker, never awaited.
pub(crate) fn record_learned_capability(key: &str, websocket: bool) {
    let Ok(mut learned) = learned().lock() else {
        return;
    };
    learned.verdicts.insert(
        key.to_string(),
        Verdict {
            websocket,
            checked_at: now_ms(),
        },
    );
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
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&temp, &bytes).is_ok() && std::fs::rename(&temp, &path).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    });
}
