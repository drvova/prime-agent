//! The opt-in quota fallback: a provider whose models.json `auth` descriptor
//! declares `fallback` serves a quota-exhausted request through the first
//! listed provider whose credential resolves and serves the same model id.
//! Only that request's credential and transport change: the session keeps
//! its provider and model. A fallback's own `fallback` is never followed.

use pa_types::ai::Model;

use super::{ModelRegistry, ResolvedRequestAuth};
use crate::auth::resolver::AuthCounter;

/// What a quota-exhausted failure on a model's credential arms
/// ([`ModelRegistry::arm_quota_fallback`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaFallback {
    /// The provider declares no fallback: the failure stands as reported.
    NotDeclared,
    /// The next request resolves through `provider`'s credential; `notice`
    /// is the user-facing status line (no secret).
    Armed { provider: String, notice: String },
    /// A fallback is declared but none can serve; `reason` says why (no
    /// secret).
    Unavailable { reason: String },
}

impl ModelRegistry {
    /// The provider's declared fallback ids (empty when none).
    fn declared_fallback(&self, provider: &str) -> Vec<String> {
        self.provider_auth
            .get(provider)
            .map(|descriptor| descriptor.fallback.clone())
            .unwrap_or_default()
    }

    /// The value fingerprint of the credential serving `provider` now.
    fn serving_credential(&mut self, provider: &str) -> Option<String> {
        self.auth
            .get_api_key_with_source_token(provider, false)
            .source_token
            .map(|token| token.value_fingerprint)
    }

    /// Whether `model`'s provider declares a fallback and its serving
    /// credential is inside a noted quota exhaustion.
    pub(super) fn is_quota_exhausted(&mut self, model: &Model) -> bool {
        if self.declared_fallback(&model.provider).is_empty() {
            return false;
        }
        let credential = self.serving_credential(&model.provider);
        self.auth
            .is_quota_exhausted(&model.provider, credential.as_deref())
    }

    /// The first declared fallback that serves `model`: its provider id and
    /// its resolved request auth through the existing resolver
    /// (single-flight, bounded, invalidate-on-rejection), with its transport
    /// as the request model under the session's provider and model id. The
    /// error lists why each fallback cannot serve.
    pub(super) fn quota_fallback_target(
        &mut self,
        model: &Model,
    ) -> Result<(String, ResolvedRequestAuth), String> {
        let mut reasons = Vec::new();
        for provider in self.declared_fallback(&model.provider) {
            let Some(fallback_model) = self
                .models
                .iter()
                .find(|candidate| candidate.provider == provider && candidate.id == model.id)
                .cloned()
            else {
                reasons.push(format!("{provider} does not serve model {}", model.id));
                continue;
            };
            if !self.has_configured_auth(&fallback_model) {
                reasons.push(format!("no {provider} credential is configured"));
                continue;
            }
            let mut resolved =
                self.resolve_request_auth(&fallback_model, fallback_model.headers.as_ref());
            if !resolved.ok {
                reasons.push(format!(
                    "the {provider} credential cannot serve: {}",
                    resolved.error.as_deref().unwrap_or("unknown error")
                ));
                continue;
            }
            let transport = resolved.request_model.take().unwrap_or(fallback_model);
            let mut request_model = model.clone();
            request_model.api = transport.api;
            request_model.base_url = transport.base_url;
            request_model.compat = transport.compat;
            resolved.request_model = Some(request_model);
            return Ok((provider, resolved));
        }
        Err(reasons.join("; "))
    }

    /// Arm the quota fallback after `model`'s request failed with a quota
    /// reset `retry_after_ms` away (the retry policy's exceeds-cap rule).
    /// A declared, usable fallback notes the serving credential exhausted
    /// until the reset, so this request's retry and every later one in this
    /// process resolve through the fallback until then. A failure while the
    /// exhaustion is already noted came from the fallback itself: nothing
    /// else is tried.
    pub fn arm_quota_fallback(&mut self, model: &Model, retry_after_ms: u64) -> QuotaFallback {
        if self.declared_fallback(&model.provider).is_empty() {
            return QuotaFallback::NotDeclared;
        }
        if self.is_quota_exhausted(model) {
            if let Ok((provider, _)) = self.quota_fallback_target(model) {
                AuthCounter::QuotaFallbackExhausted.note();
                return QuotaFallback::Unavailable {
                    reason: format!(
                        "the {provider} fallback credential was tried and its usage limit is reached too"
                    ),
                };
            }
        }
        match self.quota_fallback_target(model) {
            Ok((provider, _)) => {
                AuthCounter::QuotaFallbackChosen.note();
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(i64::MAX, |elapsed| {
                        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
                    });
                let until_ms =
                    now_ms.saturating_add(i64::try_from(retry_after_ms).unwrap_or(i64::MAX));
                let credential = self.serving_credential(&model.provider);
                self.auth
                    .note_quota_exhausted(&model.provider, credential, until_ms);
                let iso = crate::session::manager::format_iso(until_ms);
                let until = iso.get(11..16).unwrap_or(&iso);
                QuotaFallback::Armed {
                    notice: format!(
                        "using {provider} credential ({} usage limit reached until {until} UTC)",
                        model.provider
                    ),
                    provider,
                }
            }
            Err(reason) => {
                AuthCounter::QuotaFallbackExhausted.note();
                QuotaFallback::Unavailable { reason }
            }
        }
    }
}
