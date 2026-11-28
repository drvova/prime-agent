//! The `/model` picker's catalog (both daemons' `get_model_catalog`): the current validated
//! snapshot served at once, never a network request on the answer's path, and refreshed in the
//! background (a deliberate divergence from TS, which awaits the refresh inside the request); a
//! refresh that changes the snapshot calls the daemon's `changed` hook, which tells every client
//! (the old daemon's `model_catalog_changed`, the runtime daemon's `runtime/models`).
//!
//! The answer: `models` (the full catalog minus unauthorized private Prime Inference models, in
//! canonical provider and id order), `configuredProviders` (the signed-in providers, sorted),
//! `billing` (each listed model's billing mode under the stored credentials, keyed
//! `provider/id`), `accounts` (the credential the session serves each multi-credential provider
//! with) and `accountChoices` (the named credentials a new session can pin).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

/// The catalog's background refresh, coalesced: at most one refresh with one queued trailing
/// re-arm, so concurrent picker opens cost one cycle. One per session host (the old daemon's
/// worker, the runtime daemon).
#[derive(Default)]
pub struct CatalogRefresh {
    in_flight: AtomicBool,
    queued: AtomicBool,
}

impl CatalogRefresh {
    /// The picker's catalog for a session over `agent_dir` whose credential pin is `pin` (see the
    /// module documentation), from the current snapshot; arms the background refresh, which calls
    /// `changed` when it changes the snapshot. An observed credential-scope change forces the
    /// refresh (the observation consumes the scope, so one change forces exactly once). Runs inside a
    /// Tokio runtime: the refresh is spawned on it.
    pub fn serve(
        self: &Arc<Self>,
        agent_dir: &Path,
        pin: Option<&pa_types::session::CredentialPin>,
        changed: impl Fn() + Send + Sync + 'static,
    ) -> Value {
        let models_json = agent_dir.join("models.json");
        let registry = super::ModelRegistry::create(
            crate::auth::AuthStorage::create(agent_dir),
            models_json.clone(),
        );
        let available = registry
            .get_available()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let served = catalog_payload(
            &registry,
            &available,
            &crate::auth::AuthStorage::create(agent_dir).get_all(),
        );
        let credentials = super::prime_credentials_for_dir(agent_dir);
        let catalog = super::catalog_for(Some(&models_json));
        let trigger = if catalog.credentials_changed(credentials.as_ref()) {
            super::RefreshTrigger::AuthChange
        } else {
            super::RefreshTrigger::PickerOpen
        };
        self.spawn_refresh(agent_dir.to_path_buf(), trigger, served.clone(), changed);
        let mut answer = served;
        answer["accounts"] = session_accounts(&registry.auth.get_all(), agent_dir, pin);
        answer["accountChoices"] = account_choices(&registry.auth.get_all(), agent_dir);
        answer
    }

    /// The background refresh: the same awaited chain the TS request runs, settled off the
    /// answer's path; a failure keeps the last-good snapshot and stays silent.
    fn spawn_refresh(
        self: &Arc<Self>,
        agent_dir: PathBuf,
        trigger: super::RefreshTrigger,
        served: Value,
        changed: impl Fn() + Send + Sync + 'static,
    ) {
        if self.in_flight.swap(true, Ordering::SeqCst) {
            // A refresh already runs: remember the request and return; the running task re-arms
            // exactly once when it lands.
            self.queued.store(true, Ordering::SeqCst);
            return;
        }
        let gate = Arc::clone(self);
        tokio::spawn(async move {
            let mut baseline = served;
            let mut trigger = trigger;
            loop {
                let auth = crate::auth::AuthStorage::create(&agent_dir);
                let mut registry =
                    super::ModelRegistry::create(auth, agent_dir.join("models.json"));
                let available = registry
                    .refresh_available_models_with_trigger(trigger)
                    .await;
                let refreshed = catalog_payload(
                    &registry,
                    &available,
                    &crate::auth::AuthStorage::create(&agent_dir).get_all(),
                );
                if refreshed != baseline {
                    changed();
                }
                baseline = refreshed;
                gate.in_flight.store(false, Ordering::SeqCst);
                if !gate.queued.swap(false, Ordering::SeqCst) {
                    return;
                }
                if gate.in_flight.swap(true, Ordering::SeqCst) {
                    // A later request re-armed first; its task covers the trailing refresh.
                    return;
                }
                // The trailing re-arm stays gated: the next request's own scope observation
                // re-detects and forces any flip.
                trigger = super::RefreshTrigger::PickerOpen;
            }
        });
    }
}

/// The account this session serves each multi-credential provider with: the pinned credential's
/// label for the pinned provider, else `default`. A provider holding one credential is absent. An
/// unreadable `auth-accounts.json` lists nothing here (`/logout` reports it).
fn session_accounts(
    stored: &crate::auth::AuthStorageData,
    agent_dir: &std::path::Path,
    pin: Option<&pa_types::session::CredentialPin>,
) -> Value {
    let named = crate::auth::AuthAccounts::create(agent_dir)
        .all()
        .unwrap_or_default();
    let accounts: serde_json::Map<String, Value> = named
        .into_iter()
        .filter_map(|(provider, accounts)| {
            let pinned = pin
                .filter(|pin| pin.provider == provider)
                .and_then(|pin| accounts.iter().find(|account| account.id == pin.id));
            let count = accounts.len() + usize::from(stored.credential(&provider).is_some());
            match pinned {
                Some(account) => Some((provider, json!(account.label))),
                None if count > 1 => Some((provider, json!("default"))),
                None => None,
            }
        })
        .collect();
    Value::Object(accounts)
}

/// The named credentials a new session can pin for each multi-credential provider (the `/model`
/// action menu's account row): each one's id and label, in store order, never the secret. A
/// provider holding one credential is absent; without a pin a session serves the provider's
/// `default` credential.
fn account_choices(stored: &crate::auth::AuthStorageData, agent_dir: &std::path::Path) -> Value {
    let named = crate::auth::AuthAccounts::create(agent_dir)
        .all()
        .unwrap_or_default();
    let choices: serde_json::Map<String, Value> = named
        .into_iter()
        .filter(|(provider, accounts)| {
            accounts.len() + usize::from(stored.credential(provider).is_some()) > 1
        })
        .map(|(provider, accounts)| {
            let listed: Vec<Value> = accounts
                .iter()
                .map(|account| json!({ "id": account.id, "label": account.label }))
                .collect();
            (provider, Value::Array(listed))
        })
        .collect();
    Value::Object(choices)
}

/// The wire payload of one catalog snapshot: the full catalog minus
/// unauthorized private Prime Inference models, plus sorted providers and
/// each listed model's billing mode under the stored credentials
/// (`billing`, keyed `provider/id`: the picker's price column and filter).
fn catalog_payload(
    registry: &super::ModelRegistry,
    available: &[pa_types::ai::Model],
    credentials: &crate::auth::AuthStorageData,
) -> Value {
    let mut providers: Vec<String> = available
        .iter()
        .map(|model| model.provider.clone())
        .collect();
    providers.sort();
    providers.dedup();
    let available_keys: std::collections::HashSet<String> = available
        .iter()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect();
    // Canonical (provider, id) order: the served and refreshed snapshots
    // compare verbatim.
    let mut entries: Vec<&pa_types::ai::Model> = registry
        .get_all()
        .iter()
        .filter(|model| {
            !super::is_private_prime_inference_model(model)
                || available_keys.contains(&format!("{}/{}", model.provider, model.id))
        })
        .collect();
    entries.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    let billing: serde_json::Map<String, Value> = entries
        .iter()
        .map(|model| {
            let mode = crate::session_engine::billing::catalog_billing_mode(credentials, model);
            (
                format!("{}/{}", model.provider, model.id),
                serde_json::to_value(mode).unwrap_or(Value::Null),
            )
        })
        .collect();
    let models: Vec<Value> = entries
        .into_iter()
        .map(|model| serde_json::to_value(model).unwrap_or(Value::Null))
        .collect();
    json!({
        "models": models,
        "configuredProviders": providers,
        "billing": billing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The account row's choices: a provider with two named credentials lists both (id and
    /// label), a provider whose one named credential sits beside its `default` lists it, and a
    /// provider holding a single credential is absent.
    #[test]
    fn account_choices_list_each_multi_credential_providers_named_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = crate::auth::AuthAccounts::create(dir.path());
        let key = |key: &str| crate::auth::AuthCredential::ApiKey {
            key: key.to_string(),
            prime_team: None,
        };
        let work = accounts.add("anthropic", "work", key("a"), 0).unwrap();
        let home = accounts.add("anthropic", "home", key("b"), 1).unwrap();
        let side = accounts.add("openai", "side", key("c"), 2).unwrap();
        accounts.add("xai", "only", key("d"), 3).unwrap();
        let stored: crate::auth::AuthStorageData =
            serde_json::from_value(json!({ "openai": { "type": "api_key", "key": "main" } }))
                .unwrap();
        assert_eq!(
            account_choices(&stored, dir.path()),
            json!({
                "anthropic": [{ "id": work, "label": "work" }, { "id": home, "label": "home" }],
                "openai": [{ "id": side, "label": "side" }],
            })
        );
    }
}
