//! A kept webhook's secrets: its token and HMAC key, held in the agent dir's
//! `auth.json` (0o600, written under its lock, as every credential there),
//! each under `webhook:<name>`, apart from the provider and `mcp:` entries,
//! which are never read or changed here. An entry is
//! `{"type": "webhook_secret", "token"?, "hmacSecret"?}`: no provider
//! credential shape, so the login and logout lists never show it. The
//! source's row in the agent's database names its entry, never its value.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use pa_core::auth::{AuthStorageBackend as _, FileAuthStorageBackend};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// What a webhook's `auth.json` entry is named by: this, then its name.
pub(crate) const SECRET_PREFIX: &str = "webhook:";

/// A webhook's token and HMAC key; at least one is set. `Debug` never
/// prints them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebhookCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hmac_secret: Option<String>,
}

impl fmt::Debug for WebhookCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebhookCredentials")
            .field("token", &self.token.as_ref().map(|_| ".."))
            .field("hmac_secret", &self.hmac_secret.as_ref().map(|_| ".."))
            .finish()
    }
}

/// The `auth.json` webhook secrets are kept in. Its calls block on the
/// file's lock: the runtime makes them on the blocking pool.
#[derive(Debug, Clone)]
pub(crate) struct WebhookSecrets {
    auth_path: PathBuf,
}

impl WebhookSecrets {
    /// The secrets in `agent_dir`'s `auth.json`.
    pub(crate) fn in_agent_dir(agent_dir: &Path) -> Self {
        WebhookSecrets {
            auth_path: agent_dir.join("auth.json"),
        }
    }

    /// Runs `change` on the document's entries under its lock, writing them
    /// back when it says so.
    fn change(
        &self,
        mut change: impl FnMut(&mut Map<String, Value>) -> anyhow::Result<bool>,
    ) -> anyhow::Result<()> {
        FileAuthStorageBackend::new(&self.auth_path)
            .with_lock(&mut |current| {
                let mut entries: Map<String, Value> =
                    serde_json::from_str(current.as_deref().unwrap_or("{}"))
                        .context("auth.json is not a JSON object")?;
                if !change(&mut entries)? {
                    return Ok(((), None));
                }
                let written =
                    serde_json::to_string_pretty(&entries).context("serialize auth.json")?;
                Ok(((), Some(written)))
            })
            .context("update auth.json")
    }

    /// Keeps `credentials` as the entry `key`.
    pub(crate) fn store(&self, key: &str, credentials: &WebhookCredentials) -> anyhow::Result<()> {
        let mut entry = serde_json::to_value(credentials)?;
        if let Value::Object(fields) = &mut entry {
            fields.insert("type".to_string(), Value::from("webhook_secret"));
        }
        self.change(|entries| {
            entries.insert(key.to_string(), entry.clone());
            Ok(true)
        })
    }

    /// The entry `key`, when `auth.json` holds it.
    pub(crate) fn read(&self, key: &str) -> anyhow::Result<Option<WebhookCredentials>> {
        let content = FileAuthStorageBackend::new(&self.auth_path)
            .read()
            .context("read auth.json")?;
        let mut entries: Map<String, Value> =
            serde_json::from_str(content.as_deref().unwrap_or("{}"))
                .context("auth.json is not a JSON object")?;
        let Some(mut entry) = entries.remove(key) else {
            return Ok(None);
        };
        if let Value::Object(fields) = &mut entry {
            fields.remove("type");
        }
        serde_json::from_value(entry)
            .with_context(|| format!("the auth.json entry {key:?} is not a webhook's"))
            .map(Some)
    }

    /// Drops the entry `key`; one absent changes nothing.
    pub(crate) fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.change(|entries| Ok(entries.remove(key).is_some()))
    }
}
