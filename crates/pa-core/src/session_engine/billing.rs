//! Billing-mode stamping: every assistant message the session records carries how its call is
//! paid for (`usage.billing`), from pa-ai's classifier over the serving model, the kind of the
//! credential the provider resolves to here (never the secret), and the call's recorded cost.
//! Each stamp counts into the session telemetry (`agent billing modes`).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use pa_ai::billing::{BillingInputs, BillingMode, Credential, KeyShape};

/// Observes each stamped mode (the session telemetry's count).
pub type BillingObserver = Arc<dyn Fn(BillingMode) + Send + Sync>;

/// The session's stamp: armed once by the engine wiring with the agent dir (whose auth store
/// names the credential kind) and the telemetry observer. Unarmed, messages stay unclassified.
#[derive(Default)]
pub(crate) struct BillingStamp {
    agent_dir: OnceLock<PathBuf>,
    observer: OnceLock<BillingObserver>,
}

impl BillingStamp {
    pub(crate) fn arm(&self, agent_dir: PathBuf, observer: Option<BillingObserver>) {
        let _ = self.agent_dir.set(agent_dir);
        if let Some(observer) = observer {
            let _ = self.observer.set(observer);
        }
    }

    /// Stamp `message` and count its mode (see [`BillingStamp::mode_of`]).
    pub(crate) fn stamp(
        &self,
        serving: &[pa_agent::types::Model],
        message: &mut pa_types::ai::AssistantMessage,
    ) {
        let Some(mode) = self.mode_of(serving, message) else {
            return;
        };
        message.usage.billing = Some(mode);
        if let Some(observer) = self.observer.get() {
            observer(mode);
        }
    }

    /// The mode of `message` when one of the agent's `serving` models (the session model, a
    /// per-run override) served it; `None` (unclassified) for any other message.
    fn mode_of(
        &self,
        serving: &[pa_agent::types::Model],
        message: &pa_types::ai::AssistantMessage,
    ) -> Option<BillingMode> {
        let agent_dir = self.agent_dir.get()?;
        let model = serving
            .iter()
            .find(|model| model.provider == message.provider && model.id == message.model)?;
        // The stored sign-in wins over an environment key, as in request auth.
        let key_shape = |key: &str| {
            if key.starts_with("sk-ant-oat") {
                KeyShape::AnthropicSubscriptionToken
            } else {
                KeyShape::Metered
            }
        };
        let credential = match crate::auth::AuthStorage::create(agent_dir)
            .get_all()
            .credential(&model.provider)
        {
            Some(crate::auth::AuthCredential::Oauth { .. }) => Credential::OAuth,
            Some(crate::auth::AuthCredential::TokenExchange { .. }) => Credential::TokenExchange,
            Some(crate::auth::AuthCredential::ApiKey { key, .. }) => Credential::ApiKey {
                shape: key_shape(&key),
            },
            Some(crate::auth::AuthCredential::McpStaticToken { .. }) | None => {
                match pa_ai::env_api_keys::get_env_api_key(&model.provider) {
                    Some(key) => Credential::ApiKey {
                        shape: key_shape(&key),
                    },
                    None => Credential::None,
                }
            }
        };
        let catalog_cost = pa_types::ai::ModelCost {
            input: model.cost.input.into(),
            output: model.cost.output.into(),
            cache_read: model.cost.cache_read.into(),
            cache_write: model.cost.cache_write.into(),
        };
        Some(pa_ai::billing::classify(&BillingInputs {
            provider: &model.provider,
            base_url: &model.base_url,
            credential,
            catalog_cost: &catalog_cost,
            recorded_cost: message.usage.cost.total.as_f64(),
        }))
    }
}

/// The session's billing stamp bound to its agent loop: what a host that writes its own copy of
/// a settled assistant message (the daemon worker's wire rows) stamps the copy with.
#[derive(Clone)]
pub struct SessionBilling {
    pub(crate) stamp: Arc<BillingStamp>,
    /// Weak: the agent owns the listener that holds this handle.
    agent: std::sync::Weak<pa_agent::agent::Agent>,
}

impl SessionBilling {
    pub(crate) fn new(stamp: Arc<BillingStamp>, agent: &Arc<pa_agent::agent::Agent>) -> Self {
        Self {
            stamp,
            agent: Arc::downgrade(agent),
        }
    }

    /// The agent's serving models: the session model and a per-run override.
    async fn serving(&self) -> Vec<pa_agent::types::Model> {
        let Some(agent) = self.agent.upgrade() else {
            return Vec::new();
        };
        let mut serving = vec![agent.model().await];
        serving.extend(agent.model_override().map(|run| run.model));
        serving
    }

    /// The billing mode of a settled assistant message, without counting it (the session's own
    /// record counts each call once).
    pub async fn mode_of(&self, message: &pa_types::ai::AssistantMessage) -> Option<BillingMode> {
        self.stamp.mode_of(&self.serving().await, message)
    }

    /// Stamp the session's own record of a settled assistant message and count its mode.
    pub(crate) async fn stamp(&self, message: &mut pa_types::ai::AssistantMessage) {
        self.stamp.stamp(&self.serving().await, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::SessionManager;
    use pa_types::session::{AgentMessage, FileEntry};

    fn assistant(provider: &str, model: &str) -> pa_types::ai::AssistantMessage {
        pa_types::ai::AssistantMessage {
            content: Vec::new(),
            api: "anthropic-messages".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: pa_types::ai::Usage::default(),
            stop_reason: pa_types::ai::StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 1,
            rest: pa_types::JsonMap::new(),
        }
    }

    /// A stamped mode survives the session file, and a row recorded without one (the legacy
    /// shape: no `billing` key) reads back unclassified.
    #[test]
    fn a_stamped_mode_round_trips_the_session_file_and_a_legacy_row_reads_unclassified() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("auth.json"),
            serde_json::json!({
                "anthropic": { "type": "oauth", "access": "a", "refresh": "r", "expires": 4_102_444_800_000_i64 }
            })
            .to_string(),
        )
        .unwrap();
        let stamp = BillingStamp::default();
        stamp.arm(agent_dir, None);
        let serving = pa_agent::types::Model {
            id: "claude-fable-5".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            ..pa_agent::types::Model::unknown()
        };
        let mut planned = assistant("anthropic", "claude-fable-5");
        stamp.stamp(std::slice::from_ref(&serving), &mut planned);
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut session = SessionManager::in_memory(dir.path());
        let file = session.materialize_session_file(Some(session_dir.clone()));
        session
            .append_message(AgentMessage::Assistant(planned))
            .unwrap();
        // Another provider's message: the stamp leaves it as every pre-mode row was written.
        let mut legacy = assistant("openrouter", "some-model");
        stamp.stamp(std::slice::from_ref(&serving), &mut legacy);
        session
            .append_message(AgentMessage::Assistant(legacy))
            .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let reopened = SessionManager::open(dir.path(), &session_dir, &file);
        let modes: Vec<_> = reopened
            .get_entries()
            .into_iter()
            .filter_map(|entry| {
                if let FileEntry::Message {
                    message: AgentMessage::Assistant(assistant),
                    ..
                } = entry
                {
                    Some(assistant.usage.billing)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            (modes, text.matches("\"billing\"").count()),
            (
                vec![
                    Some(BillingMode::Plan {
                        provider_quota: true
                    }),
                    None
                ],
                1
            )
        );
    }
}
