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

use pa_types::session::CredentialPin;

use super::manager::{OAuthIntegration, OAuthRefreshFailure};
use super::storage::{AuthStorageBackend, FileAuthStorageBackend, RefreshLock};
use super::types::{AuthCredential, AuthStorageData};

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

    /// The pin a `--credential <label|id>` selector names: the one stored credential whose id
    /// or label (case-insensitive) matches, across every provider.
    ///
    /// # Errors
    ///
    /// No stored credential matches, the label names credentials of several providers, or the
    /// document cannot be read.
    pub fn select(&self, selector: &str) -> Result<CredentialPin, String> {
        let selector = selector.trim();
        let all = self.all().map_err(|error| format!("{error:#}"))?;
        let matches: Vec<CredentialPin> = all
            .iter()
            .flat_map(|(provider, accounts)| {
                accounts
                    .iter()
                    .filter(|account| {
                        account.id == selector || account.label.eq_ignore_ascii_case(selector)
                    })
                    .map(|account| CredentialPin {
                        provider: provider.clone(),
                        id: account.id.clone(),
                        label: account.label.clone(),
                    })
            })
            .collect();
        match matches.as_slice() {
            [pin] => Ok(pin.clone()),
            [] => Err(format!(
                "no stored credential is named '{selector}'; /login and choose Add another to store one"
            )),
            [_, ..] => Err(format!(
                "the label '{selector}' names credentials of several providers; pass the credential id instead"
            )),
        }
    }

    /// The credential a new session is pinned to, in the order the daemons' creates resolve it:
    /// the profile `profile` maps the session's `provider` to (else the profile's default
    /// provider, else its one named credential), then the `credential` selector, then the pin of
    /// the session it is created under (`parent`'s header: a scoped child inherits it). `None`
    /// pins nothing.
    ///
    /// # Errors
    ///
    /// The profile does not exist or cannot be read, or the selected credential is not stored
    /// (as [`AuthAccounts::select`]): the create is refused before any transcript exists.
    pub fn new_session_pin(
        &self,
        profile: Option<&str>,
        provider: Option<&str>,
        credential: Option<&str>,
        parent: Option<&Path>,
    ) -> Result<Option<CredentialPin>, String> {
        // The agent dir is the one `auth-accounts.json` sits in.
        let agent_dir = self.path.parent().unwrap_or_else(|| Path::new(""));
        let profile = match profile {
            Some(name) => Some(
                crate::settings::load_profile(agent_dir, name)
                    .map_err(|error| format!("{error:#}"))?
                    .ok_or_else(|| {
                        format!("profile '{name}' does not exist; /profile new {name} saves one")
                    })?,
            ),
            None => None,
        };
        match profile
            .as_ref()
            .and_then(|profile| profile.credential_for(provider))
            .or(credential)
        {
            Some(selector) => self.select(selector).map(Some),
            None => Ok(parent
                .and_then(crate::session::manager::read_session_header)
                .and_then(|header| header.credential_pin())),
        }
    }

    /// The pinned credential as stored now.
    ///
    /// # Errors
    ///
    /// The document cannot be read, or the credential was deleted: the sentence that names its
    /// label and the way out (a pin never falls back to the default slot).
    pub(crate) fn stored(&self, pin: &CredentialPin) -> Result<AuthCredential, String> {
        self.list(&pin.provider)
            .map_err(|error| format!("{error:#}"))?
            .into_iter()
            .find(|account| account.id == pin.id)
            .map(|account| account.credential)
            .ok_or_else(|| {
                format!(
                    "credential '{}' for {} is no longer stored; /login or choose another",
                    pin.label, pin.provider
                )
            })
    }

    /// The request key a pinned named credential serves: an API key as stored, an OAuth sign-in's
    /// token (refreshed once it expired, or after the provider rejected it) under this
    /// credential's own refresh lock. Never the default slot, and never another credential.
    ///
    /// # Errors
    ///
    /// The user-facing sentence: the credential is no longer stored, cannot be pinned, or its
    /// refresh failed (never a credential value).
    pub(crate) fn resolve_key(
        &self,
        pin: &CredentialPin,
        oauth: &dyn OAuthIntegration,
        refresh: NamedRefresh,
    ) -> Result<NamedKey, String> {
        let CredentialPin { provider, id, .. } = pin;
        let stored = || self.stored(pin);
        let served = |credential: &AuthCredential| -> Result<NamedKey, String> {
            oauth
                .api_key_for(provider, credential)
                .map(|api_key| NamedKey {
                    api_key,
                    credential_type: "oauth",
                })
                .ok_or_else(|| {
                    format!("credential '{id}' for {provider} holds no usable token; /login again")
                })
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(i64::MAX, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            });
        let fresh = |credential: &AuthCredential| match credential {
            AuthCredential::Oauth { expires, .. } => now_ms < *expires,
            AuthCredential::ApiKey { .. }
            | AuthCredential::McpStaticToken { .. }
            | AuthCredential::TokenExchange { .. } => false,
        };
        let credential = stored()?;
        match &credential {
            AuthCredential::ApiKey { key, .. } => {
                return Ok(NamedKey {
                    api_key: key.clone(),
                    credential_type: "api_key",
                })
            }
            AuthCredential::Oauth { .. } => {}
            AuthCredential::McpStaticToken { .. } | AuthCredential::TokenExchange { .. } => {
                return Err(format!(
                    "credential '{id}' for {provider} is a {} credential; a session pins only an API key or a subscription sign-in",
                    credential.credential_type()
                ))
            }
        }
        if refresh == NamedRefresh::WhenExpired && fresh(&credential) {
            return served(&credential);
        }
        let _lock = self
            .refresh_lock(provider, id)
            .map_err(|error| format!("{error:#}"))?;
        // Re-read under the lock: a peer that refreshed first is served, never refreshed again
        // (its refresh token is single-use).
        let current = stored()?;
        let peer_refreshed = current != credential && fresh(&current);
        if peer_refreshed || (refresh == NamedRefresh::WhenExpired && fresh(&current)) {
            return served(&current);
        }
        let mut document = serde_json::Map::new();
        document.insert(
            provider.clone(),
            serde_json::to_value(&current).map_err(|error| error.to_string())?,
        );
        let refreshed = oauth
            .refresh(provider, &AuthStorageData(document))
            .map_err(|reason| {
                OAuthRefreshFailure {
                    provider: provider.clone(),
                    reason,
                }
                .to_string()
            })?;
        let mut replacement = Some(refreshed.clone());
        self.update(&mut |document| {
            let Some(account) = document
                .providers
                .get_mut(provider)
                .and_then(|accounts| accounts.iter_mut().find(|account| account.id == *id))
            else {
                return Ok(false);
            };
            match replacement.take() {
                Some(credential) => {
                    account.credential = credential;
                    Ok(true)
                }
                None => Ok(false),
            }
        })
        .map_err(|error| format!("{error:#}"))?;
        served(&refreshed)
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

/// One named credential seen as an `auth.json`-shaped document (`{<provider>: <credential>}`), so
/// a descriptor resolver ([`TokenExchange`](super::token_exchange::TokenExchange),
/// [`DescriptorOAuth`](super::descriptor_oauth::DescriptorOAuth)) reads, refreshes and writes back
/// exactly the pinned slot: under `auth-accounts.json`'s lock and that credential's own refresh
/// lock, never the default slot.
pub(crate) struct NamedSlotBackend {
    accounts: AuthAccounts,
    pin: CredentialPin,
}

impl NamedSlotBackend {
    pub(crate) fn new(agent_dir: &Path, pin: CredentialPin) -> Self {
        NamedSlotBackend {
            accounts: AuthAccounts::create(agent_dir),
            pin,
        }
    }
}

impl AuthStorageBackend for NamedSlotBackend {
    fn with_lock(
        &self,
        update: &mut dyn FnMut(Option<String>) -> Result<((), Option<String>)>,
    ) -> Result<()> {
        let CredentialPin { provider, id, .. } = &self.pin;
        self.accounts.update(&mut |document| {
            let Some(account) = document
                .providers
                .get_mut(provider)
                .and_then(|accounts| accounts.iter_mut().find(|account| account.id == *id))
            else {
                return update(None).map(|((), _)| false);
            };
            let slot = serde_json::json!({ provider.as_str(): account.credential });
            let ((), next) = update(Some(serde_json::to_string(&slot)?))?;
            let Some(next) = next else {
                return Ok(false);
            };
            let Some(credential) =
                super::storage::parse_storage_data(Some(&next))?.credential(provider)
            else {
                bail!("a named credential's slot cannot be removed through its resolver");
            };
            account.credential = credential;
            Ok(true)
        })
    }

    /// The resolver's lock key, further keyed by this credential's id: the default slot's lock
    /// stays its own.
    fn refresh_lock(&self, key: &str) -> Result<Option<RefreshLock>> {
        self.accounts.refresh_lock(key, &self.pin.id)
    }
}

/// When a pinned OAuth credential refreshes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedRefresh {
    /// Only once its access token expired.
    WhenExpired,
    /// Now: the provider rejected the token it served.
    Rejected,
}

/// The request key a pinned credential serves.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct NamedKey {
    pub(crate) api_key: String,
    /// `api_key` or `oauth` (the stored lookup's `credential_type` vocabulary).
    pub(crate) credential_type: &'static str,
}

/// The key carries the credential, so `Debug` prints its kind only.
impl std::fmt::Debug for NamedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedKey")
            .field("credential_type", &self.credential_type)
            .finish_non_exhaustive()
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
