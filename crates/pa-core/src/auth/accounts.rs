//! Named credentials beside the default slot: `<agent-dir>/auth-accounts.json`.
//!
//! `auth.json[provider]` stays exactly as it is and remains the provider's default slot; every
//! further credential the user adds for a provider lives here, under a random stable id and a
//! user-chosen label:
//!
//! ```json
//! { "version": 1, "providers": { "<provider>": [
//!   { "id": "c_<random>", "label": "work", "addedAt": 1795003200000, "credential": { ... } }
//! ] } }
//! ```
//!
//! The document goes through the same [`FileAuthStorageBackend`] as `auth.json` (owner-only mode,
//! the directory lock, atomic writes), so it is never half-written and never readable by others.
//! Only the id is meant to appear in sessions, settings and telemetry; the label is the user's own
//! text, never derived from the secret.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::storage::{AuthStorageBackend, FileAuthStorageBackend, RefreshLock};
use super::types::AuthCredential;

/// The file name beside `auth.json`.
pub const AUTH_ACCOUNTS_FILE: &str = "auth-accounts.json";

/// The document version this build reads and writes.
const AUTH_ACCOUNTS_VERSION: u32 = 1;

/// The longest label accepted, in characters.
const MAX_LABEL_CHARS: usize = 40;

/// One named credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAccount {
    /// `c_` and 16 random hex digits: random, stable, and free of secret material.
    pub id: String,
    pub label: String,
    /// Epoch milliseconds.
    pub added_at: i64,
    pub credential: AuthCredential,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AccountsDocument {
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, Vec<StoredAccount>>,
}

/// The named-credential store of one agent dir.
pub struct AuthAccounts {
    path: PathBuf,
    backend: FileAuthStorageBackend,
}

impl AuthAccounts {
    /// The store at `<agent_dir>/auth-accounts.json`; nothing is created until a read or write.
    pub fn create(agent_dir: impl AsRef<Path>) -> Self {
        let path = agent_dir.as_ref().join(AUTH_ACCOUNTS_FILE);
        AuthAccounts {
            backend: FileAuthStorageBackend::new(&path),
            path,
        }
    }

    /// The document's path (the status line names it).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every provider's named credentials, in the order they were added.
    ///
    /// # Errors
    ///
    /// The document cannot be read, is not valid JSON, or was written by a newer build.
    pub fn all(&self) -> Result<BTreeMap<String, Vec<StoredAccount>>> {
        // A read never creates the document: most agent dirs never hold a named credential.
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        Ok(parse(self.backend.read()?.as_deref())?.providers)
    }

    /// One provider's named credentials, in the order they were added.
    ///
    /// # Errors
    ///
    /// As [`Self::all`].
    pub fn list(&self, provider: &str) -> Result<Vec<StoredAccount>> {
        Ok(self.all()?.remove(provider).unwrap_or_default())
    }

    /// Store `credential` for `provider` under `label` and return its new id.
    ///
    /// # Errors
    ///
    /// The label is empty, too long, `default`, or already names one of the provider's
    /// credentials; or the document cannot be read or written.
    pub fn add(
        &self,
        provider: &str,
        label: &str,
        credential: AuthCredential,
        added_at: i64,
    ) -> Result<String> {
        let label = label.trim().to_string();
        if label.is_empty() {
            bail!("a label is required");
        }
        if label.chars().count() > MAX_LABEL_CHARS {
            bail!("the label is longer than {MAX_LABEL_CHARS} characters");
        }
        if label.eq_ignore_ascii_case("default") {
            bail!("\"default\" names the provider's main credential; choose another label");
        }
        let id = new_account_id()?;
        let mut credential = Some(credential);
        self.update(&mut |document| {
            let accounts = document.providers.entry(provider.to_string()).or_default();
            if accounts
                .iter()
                .any(|account| account.label.eq_ignore_ascii_case(&label))
            {
                bail!("a credential labelled \"{label}\" is already stored for {provider}");
            }
            let Some(credential) = credential.take() else {
                bail!("the credential was already stored");
            };
            accounts.push(StoredAccount {
                id: id.clone(),
                label: label.clone(),
                added_at,
                credential,
            });
            Ok(true)
        })?;
        Ok(id)
    }

    /// Remove one named credential; `false` when it was not stored.
    ///
    /// # Errors
    ///
    /// The document cannot be read or written.
    pub fn remove(&self, provider: &str, id: &str) -> Result<bool> {
        let mut removed = false;
        self.update(&mut |document| {
            let Some(accounts) = document.providers.get_mut(provider) else {
                return Ok(false);
            };
            let before = accounts.len();
            accounts.retain(|account| account.id != id);
            removed = accounts.len() != before;
            if accounts.is_empty() {
                document.providers.remove(provider);
            }
            Ok(removed)
        })?;
        Ok(removed)
    }

    /// The cross-process refresh single-flight for one named credential: a lock file beside this
    /// document, keyed by provider AND id, so refreshing one account never waits on another (or on
    /// the default slot, whose lock stays beside `auth.json`).
    ///
    /// # Errors
    ///
    /// The lock cannot be taken within the backend's acquisition budget.
    pub fn refresh_lock(&self, provider: &str, id: &str) -> Result<Option<RefreshLock>> {
        self.backend.refresh_lock(&format!("{provider}\u{0}{id}"))
    }

    /// The locked read-modify-write; `change` answers whether it changed the document.
    fn update(&self, change: &mut dyn FnMut(&mut AccountsDocument) -> Result<bool>) -> Result<()> {
        self.backend.with_lock(&mut |current| {
            let mut document = parse(current.as_deref())?;
            if !change(&mut document)? {
                return Ok(((), None));
            }
            document.version = AUTH_ACCOUNTS_VERSION;
            let mut text = serde_json::to_string_pretty(&document)?;
            text.push('\n');
            Ok(((), Some(text)))
        })
    }
}

/// The document, or an empty one for an absent or freshly initialized (`{}`) file.
fn parse(content: Option<&str>) -> Result<AccountsDocument> {
    let Some(content) = content.map(str::trim).filter(|content| !content.is_empty()) else {
        return Ok(AccountsDocument::default());
    };
    let value: serde_json::Value = serde_json::from_str(content)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version > u64::from(AUTH_ACCOUNTS_VERSION) {
        bail!(
            "{AUTH_ACCOUNTS_FILE} was written by a newer Prime Agent (version {version}); update to manage these credentials"
        );
    }
    if value.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(AccountsDocument::default());
    }
    Ok(serde_json::from_value(value)?)
}

/// `c_` and 64 random bits as hex.
fn new_account_id() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("no randomness: {error}"))?;
    Ok(format!("c_{:016x}", u64::from_be_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: &str) -> AuthCredential {
        AuthCredential::ApiKey {
            key: key.to_string(),
            prime_team: None,
        }
    }

    /// Add then remove round-trips through the file, the file is owner-only, and an existing
    /// `auth.json` is never touched (byte-identical, odd formatting included).
    #[test]
    fn accounts_round_trip_owner_only_and_leave_auth_json_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let auth_json = "{\n    \"openai\": {\"type\":\"api_key\",\"key\":\"sk-default\"}   }";
        std::fs::write(dir.path().join("auth.json"), auth_json).unwrap();
        let accounts = AuthAccounts::create(dir.path());
        let work = accounts
            .add("openai", " work ", key("sk-work"), 1_795_003_200_000)
            .unwrap();
        let home = accounts
            .add("openai", "home", key("sk-home"), 1_795_003_300_000)
            .unwrap();
        // A fresh store reads what the first one wrote.
        assert_eq!(
            AuthAccounts::create(dir.path()).all().unwrap(),
            BTreeMap::from([(
                "openai".to_string(),
                vec![
                    StoredAccount {
                        id: work.clone(),
                        label: "work".into(),
                        added_at: 1_795_003_200_000,
                        credential: key("sk-work"),
                    },
                    StoredAccount {
                        id: home.clone(),
                        label: "home".into(),
                        added_at: 1_795_003_300_000,
                        credential: key("sk-home"),
                    },
                ],
            )])
        );
        #[cfg(unix)]
        assert_eq!(
            crate::platform::perms::file_mode(&dir.path().join(AUTH_ACCOUNTS_FILE)),
            Some(0o600)
        );
        assert!(accounts.remove("openai", &work).unwrap());
        assert!(!accounts.remove("openai", &work).unwrap());
        assert!(accounts.remove("openai", &home).unwrap());
        assert_eq!(accounts.all().unwrap(), BTreeMap::new());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("auth.json")).unwrap(),
            auth_json
        );
    }

    /// Ids are `c_` plus 16 hex digits and never repeat; a duplicate, empty or reserved label is
    /// refused without writing.
    #[test]
    fn labels_are_unique_per_provider_and_ids_are_random() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = AuthAccounts::create(dir.path());
        let first = accounts.add("xai", "Work", key("a"), 0).unwrap();
        let second = accounts.add("openai", "work", key("b"), 0).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with("c_") && first.len() == 18, "{first}");
        assert_eq!(
            [
                accounts.add("xai", "work", key("c"), 0),
                accounts.add("xai", "  ", key("c"), 0),
                accounts.add("xai", "Default", key("c"), 0),
            ]
            .map(|result| result.map_err(|error| error.to_string())),
            [
                Err("a credential labelled \"work\" is already stored for xai".to_string()),
                Err("a label is required".to_string()),
                Err(
                    "\"default\" names the provider's main credential; choose another label"
                        .to_string()
                ),
            ]
        );
        assert_eq!(accounts.list("xai").unwrap().len(), 1);
    }

    /// A newer build's document is refused rather than rewritten in this build's shape.
    #[test]
    fn a_newer_document_is_never_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let newer = r#"{"version":2,"providers":{}}"#;
        std::fs::write(dir.path().join(AUTH_ACCOUNTS_FILE), newer).unwrap();
        let accounts = AuthAccounts::create(dir.path());
        assert!(accounts.add("openai", "work", key("k"), 0).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(AUTH_ACCOUNTS_FILE)).unwrap(),
            newer
        );
    }

    /// The refresh single-flight is per (provider, id): holding one account's lock never blocks
    /// another account of the same provider, nor the default slot's lock beside `auth.json`.
    #[test]
    fn refresh_locks_are_per_provider_and_credential() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = AuthAccounts::create(dir.path());
        let held = accounts.refresh_lock("openai", "c_a").unwrap();
        assert!(held.is_some());
        let started = std::time::Instant::now();
        let other_account = accounts.refresh_lock("openai", "c_b").unwrap();
        let default_slot = FileAuthStorageBackend::new(dir.path().join("auth.json"))
            .refresh_lock("openai")
            .unwrap();
        assert!(other_account.is_some() && default_slot.is_some());
        // A shared lock would wait out its 15 s budget before failing.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }
}
