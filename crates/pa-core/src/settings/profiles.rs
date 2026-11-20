//! Profiles: named overlays in one agent dir, served by one daemon.
//!
//! `<agent-dir>/profiles/<name>.json` holds
//! `{version:1, credentials:{<provider>: "<credId>"|"default"}, settings:{...}}`: the credential a
//! new session pins per provider and a sparse settings overlay with the same keys as
//! `settings.json`. The overlay layers between the global and the project settings
//! (global < profile < project). No profile behaves exactly as before.
//!
//! The active profile is process-wide: the client sets it from `--profile` or
//! `PRIME_AGENT_PROFILE`, and each session worker (one process per session) from its create.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// The directory beside `settings.json` that holds the profiles.
pub const PROFILES_DIR: &str = "profiles";

/// The env var that selects a profile when no `--profile` flag does.
pub const PROFILE_ENV_VAR: &str = "PRIME_AGENT_PROFILE";

/// The document version this build reads and writes.
const PROFILE_VERSION: u32 = 1;

/// The longest profile name accepted, in characters.
const MAX_PROFILE_NAME_CHARS: usize = 40;

/// The credential value that names a provider's default slot.
pub const DEFAULT_CREDENTIAL: &str = "default";

/// One profile document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub version: u32,
    /// Provider id to a named credential id, or [`DEFAULT_CREDENTIAL`].
    pub credentials: BTreeMap<String, String>,
    /// The sparse settings overlay.
    pub settings: serde_json::Map<String, serde_json::Value>,
}

impl Profile {
    /// The named credential id this profile pins a new session to: its mapping for `provider`
    /// (the session's `--provider`, else the profile's own `defaultProvider`), else its one named
    /// credential when no provider is known. `default` mappings pin nothing.
    #[must_use]
    pub fn credential_for(&self, provider: Option<&str>) -> Option<&str> {
        let provider = provider.or_else(|| {
            self.settings
                .get("defaultProvider")
                .and_then(serde_json::Value::as_str)
        });
        let mut named = self
            .credentials
            .iter()
            .filter(|(_, id)| id.as_str() != DEFAULT_CREDENTIAL);
        if let Some(provider) = provider {
            named
                .find(|(mapped, _)| mapped.as_str() == provider)
                .map(|(_, id)| id.as_str())
        } else {
            match (named.next(), named.next()) {
                (Some((_, id)), None) => Some(id.as_str()),
                (Some(_), Some(_)) | (None, _) => None,
            }
        }
    }
}

static ACTIVE_PROFILE: RwLock<Option<String>> = RwLock::new(None);

/// Make `name` this process's active profile (`None`: no profile). Every settings read after this
/// layers its overlay.
pub fn set_active_profile(name: Option<String>) {
    *ACTIVE_PROFILE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = name;
}

/// This process's active profile.
#[must_use]
pub fn active_profile() -> Option<String> {
    ACTIVE_PROFILE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// A profile name is 1-40 ASCII letters, digits, `-` or `_`, and never `default`.
///
/// # Errors
///
/// The sentence naming what the name breaks.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() || name.chars().count() > MAX_PROFILE_NAME_CHARS {
        bail!("a profile name is 1 to {MAX_PROFILE_NAME_CHARS} characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("a profile name uses only letters, digits, '-' and '_'");
    }
    if name.eq_ignore_ascii_case(DEFAULT_CREDENTIAL) {
        bail!("\"default\" means no profile; choose another name");
    }
    Ok(())
}

/// `<agent_dir>/profiles/<name>.json`.
///
/// # Errors
///
/// The name is not a valid profile name.
pub fn profile_path(agent_dir: &Path, name: &str) -> Result<PathBuf> {
    validate_profile_name(name)?;
    Ok(agent_dir.join(PROFILES_DIR).join(format!("{name}.json")))
}

/// The stored profile `name`, `None` when it does not exist.
///
/// # Errors
///
/// The name is invalid, or the file cannot be read, parsed, or was written by a newer build.
pub fn load_profile(agent_dir: &Path, name: &str) -> Result<Option<Profile>> {
    let path = profile_path(agent_dir, name)?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let profile: Profile = serde_json::from_str(&content)?;
    if profile.version > PROFILE_VERSION {
        bail!(
            "profile '{name}' was written by a newer Prime Agent (version {}); update to use it",
            profile.version
        );
    }
    Ok(Some(profile))
}

/// Store `profile` as `name`: owner-only, under the settings lock, written atomically.
///
/// # Errors
///
/// The name is invalid, or the file cannot be locked or written.
pub fn save_profile(agent_dir: &Path, name: &str, profile: &Profile) -> Result<()> {
    let path = profile_path(agent_dir, name)?;
    let mut document = profile.clone();
    document.version = PROFILE_VERSION;
    let mut content = serde_json::to_string_pretty(&document)?;
    content.push('\n');
    super::storage::locked_write(&path, &content)
}

/// Every stored profile's name, sorted.
#[must_use]
pub fn list_profiles(agent_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(agent_dir.join(PROFILES_DIR)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".json")?.to_string();
            validate_profile_name(&stem).ok().map(|()| stem)
        })
        .collect();
    names.sort();
    names
}
