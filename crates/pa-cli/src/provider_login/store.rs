//! Where a `/login` lands its credential: the provider's default slot in `auth.json` (today's
//! behavior), or a new named credential in `auth-accounts.json` beside it ("Add another").

use std::path::Path;

use pa_core::auth::{AuthAccounts, AuthCredential, AuthStorage};
use pa_tui::provider_auth::LoginTarget;

/// One login's credential destination.
pub(crate) enum LoginStore<'a> {
    /// `auth.json[provider]`, replacing what it holds.
    Default { agent_dir: &'a Path },
    /// A new named credential under `label`.
    Named { agent_dir: &'a Path, label: &'a str },
}

impl<'a> LoginStore<'a> {
    pub(crate) fn new(agent_dir: &'a Path, target: &'a LoginTarget) -> Self {
        match target {
            LoginTarget::ReplaceDefault => LoginStore::Default { agent_dir },
            LoginTarget::AddNamed { label } => LoginStore::Named { agent_dir, label },
        }
    }

    /// Store `credential` for `provider`.
    ///
    /// # Errors
    ///
    /// The store's write error (never credential material), or the named store's label refusal.
    pub(crate) fn save(&self, provider: &str, credential: AuthCredential) -> Result<(), String> {
        match self {
            LoginStore::Default { agent_dir } => {
                let mut auth = AuthStorage::create(agent_dir);
                auth.set(provider, credential);
                auth.drain_errors().pop().map_or(Ok(()), Err)
            }
            LoginStore::Named { agent_dir, label } => {
                let added_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| {
                        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
                    });
                AuthAccounts::create(agent_dir)
                    .add(provider, label, credential, added_at)
                    .map(drop)
                    .map_err(|error| format!("{error:#}"))
            }
        }
    }

    /// The status row's tail: where the credential went.
    pub(crate) fn saved_note(&self) -> String {
        match self {
            LoginStore::Default { agent_dir } => format!(
                "Credentials saved to {}",
                agent_dir.join("auth.json").display()
            ),
            LoginStore::Named { agent_dir, label } => format!(
                "Added as \"{label}\". Credentials saved to {}",
                AuthAccounts::create(agent_dir).path().display()
            ),
        }
    }
}
