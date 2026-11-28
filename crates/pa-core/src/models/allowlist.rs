//! The daemon-level model allowlist (settings `allowedModels`): the model
//! patterns a daemon may resolve to — a daemon policy, not a session
//! preference; an outside model fails loudly with [`ModelAllowlistRefusal`].
//! Rust-only guardrail (no TS equivalent; unset means unrestricted).

use std::path::Path;

/// A model the daemon refused to resolve: the resolved selector is outside
/// the configured allowlist, typed so enforcement seams can downcast the
/// refusal (adoption telemetry) out of the TS-parity resolution errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAllowlistRefusal {
    /// The refused model, full selector form `provider/model-id`.
    pub selector: String,
}

impl std::fmt::Display for ModelAllowlistRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Model \"{}\" is blocked by the daemon model allowlist (settings \"allowedModels\"); \
             the daemon never falls back to a different model. Allow it in the settings \
             or pick an allowed model.",
            self.selector
        )
    }
}

impl std::error::Error for ModelAllowlistRefusal {}

/// The daemon allowlist's loaded state (settings `allowedModels`, global
/// scope), which both daemons enforce. A security guardrail fails CLOSED: a settings document that
/// could not be loaded is an unknown policy, never an unrestricted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAllowlist {
    /// No `allowedModels` configured: unrestricted (TS parity).
    Unrestricted,
    /// The configured patterns.
    Allowed(Vec<String>),
    /// The global settings document could not be loaded (lock contention,
    /// read, or parse failure): the configured policy is unknown, so every
    /// resolution fails loudly instead of bypassing it.
    Unreadable(String),
}

/// Load the daemon allowlist state (settings `allowedModels`, global
/// scope).
///
/// Both daemons' model changes call it, so a malformed allowlist refuses
/// in each.
#[must_use]
pub fn load_model_allowlist(cwd: &Path, agent_dir: &Path) -> ModelAllowlist {
    let settings = crate::settings::SettingsManager::create(cwd, agent_dir);
    if let Some(error) = settings
        .errors()
        .iter()
        .find(|error| error.scope == crate::settings::SettingsScope::Global)
    {
        return ModelAllowlist::Unreadable(error.message.clone());
    }
    if let Some(patterns) = settings.get_allowed_models() {
        ModelAllowlist::Allowed(patterns)
    } else {
        // A PRESENT-but-malformed `allowedModels` (the lenient load drops
        // wrong-typed known fields to unset) fails closed: the restriction
        // was requested, so an unreadable shape is never an unrestricted
        // gate; explicit `null` stays unset (TS parity).
        let unreadable = settings.global_raw().and_then(|raw| {
            if !raw.is_object() {
                return Some("the global settings document is not a JSON object".to_string());
            }
            let malformed = match raw.get("allowedModels") {
                None | Some(serde_json::Value::Null) => false,
                Some(value) => value
                    .as_array()
                    .is_none_or(|items| items.iter().any(|item| item.as_str().is_none())),
            };
            malformed.then(|| "allowedModels is present but is not an array of strings".to_string())
        });
        match unreadable {
            Some(message) => ModelAllowlist::Unreadable(message),
            None => ModelAllowlist::Unrestricted,
        }
    }
}

/// Enforce the allowlist on a resolved selector: `Ok(())` when allowed
/// (and when no allowlist is configured), the typed refusal for an
/// off-allowlist model, and a fail-closed error when the policy is
/// unreadable.
///
/// # Errors
///
/// The model is outside the allowlist ([`ModelAllowlistRefusal`]), or the
/// allowlist could not be read or is malformed.
pub fn assert_model_allowed(allowlist: &ModelAllowlist, selector: &str) -> anyhow::Result<()> {
    match allowlist {
        ModelAllowlist::Unrestricted => Ok(()),
        ModelAllowlist::Allowed(patterns) => {
            if crate::models::model_allowed(selector, patterns) {
                Ok(())
            } else {
                Err(ModelAllowlistRefusal {
                    selector: selector.to_string(),
                }
                .into())
            }
        }
        ModelAllowlist::Unreadable(error) => Err(anyhow::anyhow!(
            "The daemon model allowlist could not be read ({error}); \
             refusing to resolve model \"{selector}\" — the daemon fails closed instead of \
             bypassing the configured allowedModels policy. Fix settings.json and retry."
        )),
    }
}

/// Whether a selector is allowed by the `allowedModels` patterns (the
/// `--models` grammar, case-insensitive, matched against the full selector
/// `provider/model-id` and the bare id, whose `prime-inference` ids may
/// carry slashes): wildcards glob, plain patterns match exactly. An empty
/// pattern list allows nothing.
#[must_use]
pub fn model_allowed(selector: &str, allowlist: &[String]) -> bool {
    allowlist
        .iter()
        .any(|pattern| pattern_matches_selector(pattern, selector))
}

/// One pattern against one selector: case-insensitive exact or glob match
/// on the full selector or the bare id.
fn pattern_matches_selector(pattern: &str, selector: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    if pattern.is_empty() {
        return false;
    }
    let selector = selector.to_lowercase();
    let bare_id = selector
        .split_once('/')
        .map_or(selector.as_str(), |(_, id)| id);
    if !pattern.contains(['*', '?', '[']) {
        return pattern == selector || pattern == bare_id;
    }
    let matcher = match globset::Glob::new(&pattern) {
        Ok(glob) => glob.compile_matcher(),
        Err(_) => return false,
    };
    matcher.is_match(&selector) || matcher.is_match(bare_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELECTOR: &str = "prime-inference/internal/glm-5.3-fast";

    #[test]
    fn exact_and_bare_id_patterns_match_case_insensitively() {
        assert!(model_allowed(
            SELECTOR,
            &["prime-inference/internal/glm-5.3-fast".to_string()]
        ));
        assert!(model_allowed(
            SELECTOR,
            &["Prime-Inference/Internal/GLM-5.3-Fast".to_string()]
        ));
        // The bare id (prime-inference ids carry slashes) matches without
        // the provider.
        assert!(model_allowed(
            SELECTOR,
            &["internal/glm-5.3-fast".to_string()]
        ));
        assert!(!model_allowed(SELECTOR, &["glm-5.3-fast".to_string()]));
        assert!(!model_allowed(
            SELECTOR,
            &["prime-inference/internal/glm-5.3-turbo".to_string()]
        ));
    }

    #[test]
    fn glob_patterns_match_the_full_selector_or_bare_id() {
        let allow = |pattern: &str| vec![pattern.to_string()];
        assert!(model_allowed(SELECTOR, &allow("prime-inference/*")));
        assert!(model_allowed(
            SELECTOR,
            &allow("prime-inference/internal/*")
        ));
        assert!(model_allowed(SELECTOR, &allow("internal/*")));
        assert!(model_allowed(SELECTOR, &allow("PRIME-INFERENCE/*")));
        // The provider wildcard does not swallow other providers.
        assert!(!model_allowed("zai/glm-5.3", &allow("prime-inference/*")));
        // A question mark is a wildcard, not a plain character.
        assert!(model_allowed("zai/glm-5.3", &allow("zai/glm-5.?")));
        // An empty pattern list allows nothing (the settings getter never
        // produces one, but the matching must stay total).
        assert!(!model_allowed(SELECTOR, &[]));
        assert!(!model_allowed(SELECTOR, &["  ".to_string()]));
        assert!(!model_allowed(SELECTOR, &allow("[")));
    }

    #[test]
    fn the_refusal_error_carries_the_selector() {
        let refusal = ModelAllowlistRefusal {
            selector: "zai/glm-5.3".to_string(),
        };
        assert_eq!(refusal.selector, "zai/glm-5.3");
        let message = refusal.to_string();
        assert!(
            message.contains("Model \"zai/glm-5.3\" is blocked by the daemon model allowlist"),
            "{message}"
        );
        assert!(message.contains("never falls back"), "{message}");
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use std::path::Path;

    fn write_settings(dir: &Path, value: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("settings.json"), value).unwrap();
    }

    #[test]
    fn load_reads_the_global_scope_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent_dir = dir.path().join("agent");
        assert!(matches!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Unrestricted
        ));
        write_settings(&agent_dir, r#"{"allowedModels": ["prime-inference/*"]}"#);
        assert_eq!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Allowed(vec!["prime-inference/*".to_string()])
        );
        // A project scope cannot weaken the global pin: the project dir is
        // the temp dir itself.
        write_settings(
            &dir.path().join(".prime").join("agent"),
            r#"{"allowedModels": []}"#,
        );
        assert_eq!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Allowed(vec!["prime-inference/*".to_string()])
        );
    }

    #[test]
    fn load_trims_and_drops_empty_lists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent_dir = dir.path().join("agent");
        write_settings(
            &agent_dir,
            r#"{"allowedModels": ["  prime-inference/internal/*  ", ""]}"#,
        );
        assert_eq!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Allowed(vec!["prime-inference/internal/*".to_string()])
        );
        write_settings(&agent_dir, r#"{"allowedModels": [" ", ""]}"#);
        assert!(matches!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Unrestricted
        ));
    }

    #[test]
    fn assert_allowed_passes_without_an_allowlist_and_types_the_refusal() {
        assert_model_allowed(&ModelAllowlist::Unrestricted, "anything/model").unwrap();
        let allow = ModelAllowlist::Allowed(vec!["prime-inference/*".to_string()]);
        assert_model_allowed(&allow, "prime-inference/internal/glm-5.3-fast").unwrap();
        let error = assert_model_allowed(&allow, "zai/glm-5.3").expect_err("refused");
        let refusal = error
            .downcast_ref::<ModelAllowlistRefusal>()
            .expect("typed refusal");
        assert_eq!(refusal.selector, "zai/glm-5.3");
    }

    #[test]
    fn an_unreadable_settings_document_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("settings.json"), "{ not json").unwrap();
        let state = load_model_allowlist(dir.path(), &agent_dir);
        assert!(matches!(state, ModelAllowlist::Unreadable(_)));
        let error =
            assert_model_allowed(&state, "prime-inference/mock-1").expect_err("fail closed");
        let message = error.to_string();
        assert!(message.contains("could not be read"), "{message}");
        assert!(message.contains("fails closed"), "{message}");
        assert!(
            error.downcast_ref::<ModelAllowlistRefusal>().is_none(),
            "fail-closed is not a pattern refusal"
        );
        std::fs::remove_file(agent_dir.join("settings.json")).unwrap();
        let state = load_model_allowlist(dir.path(), &agent_dir);
        assert!(matches!(state, ModelAllowlist::Unrestricted));
    }

    #[test]
    fn a_malformed_allowed_models_value_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"allowedModels": "prime-inference/*"}"#,
        )
        .unwrap();
        assert!(matches!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Unreadable(_)
        ));
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"allowedModels": ["prime-inference/*", 42]}"#,
        )
        .unwrap();
        assert!(matches!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Unreadable(_)
        ));
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"allowedModels": null}"#,
        )
        .unwrap();
        assert!(matches!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Unrestricted
        ));
        std::fs::write(agent_dir.join("settings.json"), r#"{"allowedModels": []}"#).unwrap();
        assert!(matches!(
            load_model_allowlist(dir.path(), &agent_dir),
            ModelAllowlist::Unrestricted
        ));
    }

    /// A NON-OBJECT global document (`[]`, `"bad"`) is a corrupted
    /// document, not an absent key: the guard fails closed.
    #[test]
    fn a_non_object_global_document_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        for corrupted in ["[]", r#""bad""#, "42"] {
            std::fs::write(agent_dir.join("settings.json"), corrupted).unwrap();
            let state = load_model_allowlist(dir.path(), &agent_dir);
            assert!(
                matches!(state, ModelAllowlist::Unreadable(_)),
                "{corrupted} must fail closed, got {state:?}"
            );
            let error = assert_model_allowed(&state, "prime-inference/mock-1").expect_err("closed");
            assert!(
                error.downcast_ref::<ModelAllowlistRefusal>().is_none(),
                "fail-closed is not a pattern refusal"
            );
        }
    }
}
