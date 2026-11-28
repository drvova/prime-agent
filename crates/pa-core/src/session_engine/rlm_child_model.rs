//! RLM child model resolution: reference resolution against the credential-backed catalog and
//! the spawn-time thinking-support check (the requested child cwd, the one-line task label and
//! the roster text caps are pa-core's, beside the notices).

use std::path::Path;

use crate::kernel::rlm_runtime::{find_rlm_model_matches, RlmModelInfo};
use anyhow::{anyhow, bail, Result};
use pa_ai::models::{get_supported_thinking_levels, thinking_level_from_str};

/// The registry a child's model resolves against: the agent dir's auth and `models.json`.
fn model_registry(agent_dir: &Path) -> crate::models::ModelRegistry {
    crate::models::ModelRegistry::create(
        crate::auth::AuthStorage::create(agent_dir),
        agent_dir.join("models.json"),
    )
}

/// Close matches listed in model-resolution errors.
const MODEL_ERROR_SUGGESTION_LIMIT: usize = 3;

/// The model catalog the RLM surface resolves against: the same
/// credential-backed list `rlm.find_models` searches (entitled `internal/*` models resolve).
#[must_use]
pub fn catalog_models(agent_dir: &Path) -> Vec<RlmModelInfo> {
    let registry = model_registry(agent_dir);
    registry
        .get_rlm_searchable_models()
        .into_iter()
        .map(|model| RlmModelInfo {
            provider: model.provider.clone(),
            id: model.id.clone(),
            name: if model.name.is_empty() {
                model.id.clone()
            } else {
                model.name.clone()
            },
            image_input: model.input.contains(&pa_types::ai::ModelInput::Image),
        })
        .collect()
}

/// Resolve the child model reference, then enforce the daemon
/// `allowedModels` allowlist on the resolved selector (inherited parent
/// model included): outside the allowlist fails loudly, never a fallback.
///
/// # Errors
/// Returns an error when the reference cannot be resolved or the selector is outside the allowlist.
pub fn resolve_child_model(
    agent_dir: &Path,
    reference: Option<&str>,
    parent_model: Option<&str>,
    target: &str,
    allowlist: &crate::models::ModelAllowlist,
) -> Result<String> {
    let model = resolve_child_model_unchecked(agent_dir, reference, parent_model, target)?;
    crate::models::assert_model_allowed(allowlist, &model)?;
    Ok(model)
}

/// The resolution before the allowlist gate: `None` inherits the parent model; a reference resolves
/// parent equality first, then an exact catalog selector, then a unique short-form match.
/// Public for the daemonless print host: print sessions do not enforce
/// `allowedModels`, so its children do not either.
///
/// # Errors
/// Returns an error when the reference cannot be resolved against the catalog.
pub fn resolve_child_model_unchecked(
    agent_dir: &Path,
    reference: Option<&str>,
    parent_model: Option<&str>,
    target: &str,
) -> Result<String> {
    let Some(reference) = reference else {
        return parent_model
            .map(str::to_string)
            .ok_or_else(|| anyhow!("No model selected. Use /model to pick one."));
    };
    let reference = reference.trim();
    let normalized = reference.to_lowercase();
    if let Some(parent) = parent_model {
        if parent.to_lowercase() == normalized {
            return Ok(parent.to_string());
        }
    }
    let candidates = catalog_models(agent_dir);
    let selector_of = |model: &RlmModelInfo| format!("{}/{}", model.provider, model.id);
    if let Some(exact) = candidates
        .iter()
        .find(|model| selector_of(model).to_lowercase() == normalized)
    {
        return Ok(selector_of(exact));
    }
    // Short form: the full selector ends with "/<reference>".
    let short_matches: Vec<&RlmModelInfo> = candidates
        .iter()
        .filter(|model| {
            selector_of(model)
                .to_lowercase()
                .ends_with(&format!("/{normalized}"))
        })
        .collect();
    match short_matches.len() {
        1 => return Ok(selector_of(short_matches[0])),
        0 => {
            if let Some(parent) = parent_model {
                if parent.to_lowercase().ends_with(&format!("/{normalized}")) {
                    return Ok(parent.to_string());
                }
            }
        }
        // A bare id several providers serve picks deterministically: the
        // parent's provider when it serves the id, else catalog order.
        _ => {
            let parent_provider = parent_model
                .and_then(|parent| parent.split_once('/'))
                .map(|(provider, _)| provider);
            if let Some(model) = crate::models::pick_bare_model_id(
                reference,
                &short_matches,
                |model| (model.provider.as_str(), model.id.as_str()),
                parent_provider.as_slice(),
            ) {
                return Ok(selector_of(model));
            }
        }
    }
    Err(model_unavailable_error(reference, target, &candidates))
}

/// A requested thinking level must be supported by the resolved model;
/// an out-of-catalog model (a scripted verification model) passes unchecked.
///
/// # Errors
/// Returns an error when the supported levels exclude the requested one (the message lists them);
/// the unchecked cases pass.
pub fn assert_thinking_supported(
    agent_dir: &Path,
    level: Option<&str>,
    selector: &str,
) -> Result<()> {
    let Some(level) = level else {
        return Ok(());
    };
    let Some((provider, id)) = selector.split_once('/') else {
        return Ok(());
    };
    let registry = model_registry(agent_dir);
    let Some(model) = registry
        .get_rlm_searchable_models()
        .into_iter()
        .find(|model| model.provider == provider && model.id == id)
    else {
        return Ok(());
    };
    let supported = get_supported_thinking_levels(model);
    let Some(requested) = thinking_level_from_str(level) else {
        return Ok(());
    };
    if supported.contains(&requested) {
        return Ok(());
    }
    let levels = supported
        .iter()
        .map(|level| level.wire_name())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "Requested thinking level \"{level}\" is not supported by model \"{selector}\"; supported levels: {levels}"
    );
}

/// Rejection message for an unresolved model reference: the
/// unavailability, the selector form, and close matches for retry.
fn model_unavailable_error(
    reference: &str,
    target: &str,
    candidates: &[RlmModelInfo],
) -> anyhow::Error {
    let base = format!(
        "Requested {target} model \"{reference}\" is unavailable, unauthenticated, or expired"
    );
    let hint =
        "selectors use the form \"provider/model-id\" (e.g. \"prime-inference/z-ai/glm-5.3\")";
    let close_matches = find_rlm_model_matches(reference, candidates, MODEL_ERROR_SUGGESTION_LIMIT);
    if close_matches.is_empty() {
        anyhow!("{base}; {hint}")
    } else {
        let selectors = close_matches
            .iter()
            .map(|match_| format!("\"{}\"", match_.selector))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow!("{base}; {hint}; close matches: {selectors}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelAllowlist;
    use serde_json::json;

    /// A models.json custom provider, like the pa-core registry tests.
    fn write_catalog(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("models.json"),
            json!({
                "providers": {
                    "test-provider": {
                        "baseUrl": "http://localhost:9",
                        "apiKey": "test-key",
                        "api": "openai-completions",
                        "models": [
                            { "id": "glm-5.3", "name": "GLM 5.3", "contextWindow": 1000, "maxTokens": 100 },
                            { "id": "glm-5.3-turbo", "name": "GLM Turbo", "contextWindow": 1000, "maxTokens": 100 }
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn resolves_parent_exact_and_short_form_references() {
        let dir = tempfile::TempDir::new().unwrap();
        write_catalog(dir.path());
        let resolved = resolve_child_model(
            dir.path(),
            None,
            Some("test-provider/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3");
        // Parent equality short-circuits even a catalog refresh miss.
        let resolved = resolve_child_model(
            dir.path(),
            Some("Test-Provider/GLM-5.3"),
            Some("test-provider/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3");
        let resolved = resolve_child_model(
            dir.path(),
            Some("test-provider/glm-5.3-turbo"),
            Some("test-provider/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3-turbo");
        let resolved = resolve_child_model(
            dir.path(),
            Some("glm-5.3-turbo"),
            Some("test-provider/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3-turbo");
        let error = resolve_child_model(
            dir.path(),
            None,
            None,
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "No model selected. Use /model to pick one."
        );
    }

    #[test]
    fn the_allowlist_refuses_resolved_models_loudly() {
        let dir = tempfile::TempDir::new().unwrap();
        write_catalog(dir.path());
        let allow = crate::models::ModelAllowlist::Allowed(vec!["prime-inference/*".to_string()]);
        // An explicit reference outside the allowlist fails with the typed refusal.
        let error = resolve_child_model(
            dir.path(),
            Some("test-provider/glm-5.3"),
            None,
            "subagent",
            &allow,
        )
        .unwrap_err();
        let refusal = error
            .downcast_ref::<crate::models::ModelAllowlistRefusal>()
            .expect("typed refusal");
        assert_eq!(refusal.selector, "test-provider/glm-5.3");
        assert!(
            error
                .to_string()
                .contains("blocked by the daemon model allowlist"),
            "{error}"
        );
        // An inherited parent model outside the allowlist refuses too:
        // inheritance is a resolution, not an exemption.
        let error = resolve_child_model(
            dir.path(),
            None,
            Some("test-provider/glm-5.3"),
            "subagent",
            &allow,
        )
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::models::ModelAllowlistRefusal>()
                .is_some(),
            "{error}"
        );
        let resolved = resolve_child_model(
            dir.path(),
            Some("test-provider/glm-5.3"),
            None,
            "subagent",
            &crate::models::ModelAllowlist::Allowed(vec!["test-provider/*".to_string()]),
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3");
        let resolved = resolve_child_model(
            dir.path(),
            Some("test-provider/glm-5.3"),
            None,
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, "test-provider/glm-5.3");
    }

    #[test]
    fn unmatched_references_carry_the_ts_error_with_close_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        write_catalog(dir.path());
        // TS parity (4649 regression suite): a prefix reference lists
        // close matches, a matching-nothing reference does not (the
        // reference prefixes the test provider, never real defaults).
        let error = resolve_child_model(
            dir.path(),
            Some("test-provi"),
            Some("test-provider/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.starts_with(
                "Requested subagent model \"test-provi\" is unavailable, unauthenticated, or expired; selectors use the form \"provider/model-id\""
            ),
            "{message}"
        );
        // Close matches list full selectors, nearest first.
        assert!(
            message.contains("close matches: \"test-provider/glm-5.3\""),
            "{message}"
        );
        let error = resolve_child_model(
            dir.path(),
            Some("zzz"),
            Some("test-provider/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap_err();
        assert!(!error.to_string().contains("close matches:"), "{error}");
    }

    #[test]
    fn thinking_support_follows_the_resolved_model() {
        let dir = tempfile::TempDir::new().unwrap();
        write_catalog(dir.path());
        // The custom catalog model is non-reasoning: only "off" is supported.
        assert_thinking_supported(dir.path(), Some("off"), "test-provider/glm-5.3").unwrap();
        let error = assert_thinking_supported(dir.path(), Some("high"), "test-provider/glm-5.3")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Requested thinking level \"high\" is not supported by model \"test-provider/glm-5.3\"; supported levels: off"
        );
        // A model outside the catalog (scripted verification models) passes.
        assert_thinking_supported(dir.path(), Some("high"), "scripted/faux-1").unwrap();
    }

    /// The child-propagation regression: a spawned child resolves an
    /// entitled private `internal/*` model through the worker's same disk caches.
    #[test]
    fn a_spawned_child_resolves_an_entitled_private_model() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        // The worker-path auth (auth.json): one Prime Inference key+team.
        std::fs::write(
            dir.path().join("auth.json"),
            json!({
                "prime-inference": {
                    "type": "api_key",
                    "key": "child-key",
                    "primeTeam": { "teamId": "team-7", "name": "Team 7" }
                }
            })
            .to_string(),
        )
        .unwrap();
        // The entitled private model: the compiled fallback lacks it, the
        // account's scoped Prime Inference snapshot carries it.
        let mut auth = crate::auth::AuthStorage::create(dir.path());
        let api_key = auth.get_api_key("prime-inference").expect("api key");
        // The snapshot below is written for whatever pair resolves (an
        // ambient `PRIME_API_KEY` on a dogfood box changes the pair).
        let team_id = auth
            .get_provider_headers("prime-inference")
            .and_then(|headers| headers.get("X-Prime-Team-ID").cloned())
            .unwrap_or_default();
        // Every compiled route, so the snapshot passes the coverage gate.
        let mut data: Vec<serde_json::Value> =
            pa_models::transports::prime_inference_offline_entries()
                .iter()
                .map(|model| {
                    json!({
                        "id": model.id,
                        "pricing": { "input_usd_per_mtok": 1.0, "output_usd_per_mtok": 1.0 },
                    })
                })
                .collect();
        data.push(json!({
            "id": "internal/glm-5.3-fast",
            "display_name": "GLM 5.3 Fast",
            "pricing": { "input_usd_per_mtok": 0.42, "output_usd_per_mtok": 2.1 },
            "specs": {
                "context_window": 400_000, "max_output_tokens": 131_072,
                "supports_reasoning": true,
                "modalities": { "input": ["text"], "output": ["text"] },
            },
        }));
        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        std::fs::write(
            dir.path().join("models/prime-inference-models-cache.json"),
            json!({
                "url": format!("{}/models", crate::models::PRIME_INFERENCE_BASE_URL),
                "scope": pa_models::prime_inference::scope_key(&api_key, &team_id),
                "fetchedAt": 1,
                "payload": { "data": data },
            })
            .to_string(),
        )
        .unwrap();

        assert!(catalog_models(dir.path())
            .iter()
            .any(|model| model.id == "internal/glm-5.3-fast"));
        let resolved = resolve_child_model(
            dir.path(),
            Some("prime-inference/internal/glm-5.3-fast"),
            Some("prime-inference/z-ai/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, "prime-inference/internal/glm-5.3-fast");
        let resolved = resolve_child_model(
            dir.path(),
            Some("internal/glm-5.3-fast"),
            Some("prime-inference/z-ai/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, "prime-inference/internal/glm-5.3-fast");
        // The spawn-time thinking check follows the resolved entitlement.
        assert_thinking_supported(
            dir.path(),
            Some("high"),
            "prime-inference/internal/glm-5.3-fast",
        )
        .unwrap();
        assert_thinking_supported(
            dir.path(),
            Some("off"),
            "prime-inference/internal/glm-5.3-fast",
        )
        .unwrap();
    }

    /// The catalog-repo child-resolution regression: a fetched entry the compiled fallback lacks
    /// resolves through the on-disk provider catalog, once its provider is auth-configured.
    #[test]
    fn a_spawned_child_resolves_a_catalog_repo_entry_the_compiled_fallback_lacks() {
        const PROBE_ID: &str = "gpt-6-probe";
        assert!(
            !pa_ai::models_generated::get_models("openai-codex")
                .iter()
                .any(|model| model.id == PROBE_ID),
            "the probe entry is compiled in; pick another id"
        );
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        // The provider auth the resolution filters on: a models.json
        // provider entry (headers alone satisfy the config validation and
        // has_configured_auth).
        std::fs::write(
            dir.path().join("models.json"),
            json!({
                "providers": {
                    "openai-codex": {
                        "apiKey": "codex-key",
                        "headers": { "X-Probe": "catalog-chain" }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        // The layer-A disk cache the startup refresh writes beside
        // models.json: one catalog-repo entry riding the compiled
        // openai-codex transport tuple (the pinning invariant).
        std::fs::write(
            dir.path()
                .join("models")
                .join("provider-model-catalog.v1.json"),
            json!({
                "url": pa_models::fetch::MODEL_CATALOG_URL,
                "scope": pa_models::cache::PUBLIC_SCOPE,
                "fetchedAt": 1,
                "payload": { "schemaVersion": 1, "models": [
                    {
                        "id": PROBE_ID, "name": "GPT-6 Probe",
                        "api": "openai-codex-responses", "provider": "openai-codex",
                        "baseUrl": "https://chatgpt.com/backend-api",
                        "reasoning": true,
                        "thinkingLevelMap": { "minimal": null, "xhigh": "xhigh", "max": "max" },
                        "input": ["text"],
                        "cost": { "input": 2, "output": 10, "cacheRead": 0.2, "cacheWrite": 2.5 },
                        "contextWindow": 272_000, "maxTokens": 128_000,
                    }
                ]}
            })
            .to_string(),
        )
        .unwrap();
        let selector = format!("openai-codex/{PROBE_ID}");
        let catalog = catalog_models(dir.path());
        let probe = catalog
            .iter()
            .find(|model| model.selector() == selector)
            .expect("the fetched entry lists for find_models");
        assert_eq!(probe.name, "GPT-6 Probe");
        let resolved = resolve_child_model(
            dir.path(),
            Some(&selector),
            Some("prime-inference/z-ai/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, selector);
        let resolved = resolve_child_model(
            dir.path(),
            Some(PROBE_ID),
            Some("prime-inference/z-ai/glm-5.3"),
            "subagent",
            &ModelAllowlist::Unrestricted,
        )
        .unwrap();
        assert_eq!(resolved, selector);
        // The spawn-time thinking check follows the fetched entry's map:
        // xhigh is explicitly mapped, minimal is explicitly nulled out.
        assert_thinking_supported(dir.path(), Some("xhigh"), &selector).unwrap();
        let error = assert_thinking_supported(dir.path(), Some("minimal"), &selector).unwrap_err();
        assert!(
            error.to_string().contains("not supported by model"),
            "{error}"
        );
    }
}
