//! The daemon's adoption event for the settings `allowedModels` allowlist
//! (`model refused`): the worker-side model-resolution seams refuse a model
//! outside the allowlist loudly (never a fallback) through
//! `pa_core::models::{load_model_allowlist, assert_model_allowed}`, and
//! note each refusal here.
use std::path::Path;

use pa_telemetry::TelemetryClient;

/// The worker's refusal telemetry: one lazily-built client shared by the
/// worker's enforcement seams, kept for the worker lifetime (the sinks
/// batch asynchronously, so a dropped client loses the event). `None`
/// telemetry when the create command opted the session out.
pub struct ModelRefusalTelemetry {
    agent_dir: std::path::PathBuf,
    enabled: bool,
    client: std::sync::Mutex<Option<(std::path::PathBuf, TelemetryClient)>>,
    /// Already-noted `(surface, selector)` pairs: one event per distinct
    /// refusal per worker, so a polling getter that re-resolves a refused
    /// model (the connection-state surface) never spams the event.
    noted: std::sync::Mutex<std::collections::HashSet<(String, String)>>,
}

impl ModelRefusalTelemetry {
    #[must_use]
    pub fn new(agent_dir: std::path::PathBuf, telemetry_disabled: bool) -> Self {
        Self {
            agent_dir,
            enabled: !telemetry_disabled,
            client: std::sync::Mutex::new(None),
            noted: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Emit the refusal's `model refused` event (best-effort; no-op when
    /// opted out). The client binds to `cwd` — the settings posture is
    /// scoped (the enabled switch and local mirror read the project
    /// scope), so a session that moved directories rebinds instead of
    /// reporting through the old project.
    ///
    /// # Panics
    ///
    /// Panics when an internal mutex is poisoned; the client-bound expect
    /// is an internal invariant and cannot fire.
    pub fn note_refused(&self, surface: &str, selector: &str, cwd: &Path) {
        if !self.enabled {
            return;
        }
        let settings = pa_core::settings::SettingsManager::create(cwd, &self.agent_dir);
        // The same gating as the supervisor's `daemon event`: the env
        // override wins, else the merged settings' telemetry switch.
        if !pa_core::session_engine::telemetry::telemetry_switch(&settings).enabled() {
            return;
        }
        // Once per distinct (surface, selector).
        {
            let mut noted = self.noted.lock().expect("refusal noted lock");
            if !noted.insert((surface.to_string(), selector.to_string())) {
                return;
            }
        }
        let mut slot = self.client.lock().expect("refusal telemetry lock");
        if slot.as_ref().is_none_or(|(bound_cwd, _)| bound_cwd != cwd) {
            let client =
                pa_core::session_engine::telemetry::build_client(&settings, &self.agent_dir);
            *slot = Some((cwd.to_path_buf(), client));
        }
        let client = &slot.as_ref().expect("client bound").1;
        let (provider, model_id) = selector.split_once('/').unwrap_or((selector, ""));
        pa_core::session_engine::telemetry::track_model_refused(
            client, surface, provider, model_id,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "release builds send to the real endpoint"
    )]
    async fn refusal_telemetry_honors_both_opt_outs() {
        let _telemetry = crate::agent_engine::tests::telemetry_opt_in();
        let dir = tempfile::tempdir().expect("tempdir");
        let disabled = ModelRefusalTelemetry::new(dir.path().to_path_buf(), true);
        disabled.note_refused("set_model", "zai/glm-5.3", dir.path());
        assert!(
            disabled.client.lock().unwrap().is_none(),
            "no client when the create command opted out"
        );
        let agent_dir = dir.path().join("agent");
        let settings_gated = ModelRefusalTelemetry::new(agent_dir.clone(), false);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"telemetry": {"enabled": false}}"#,
        )
        .unwrap();
        settings_gated.note_refused("set_model", "zai/glm-5.3", dir.path());
        assert!(
            settings_gated.client.lock().unwrap().is_none(),
            "no client when settings disable telemetry"
        );
        let enabled = ModelRefusalTelemetry::new(dir.path().to_path_buf(), false);
        enabled.note_refused("set_model", "zai/glm-5.3", dir.path());
        assert!(
            enabled.client.lock().unwrap().as_ref().is_some(),
            "client built on first note"
        );
    }

    #[tokio::test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "release builds send to the real endpoint"
    )]
    async fn refusal_telemetry_dedupes_repeated_resolves() {
        let _telemetry = crate::agent_engine::tests::telemetry_opt_in();
        let dir = tempfile::tempdir().expect("tempdir");
        let telemetry = ModelRefusalTelemetry::new(dir.path().to_path_buf(), false);
        telemetry.note_refused("session_start", "zai/glm-5.3", dir.path());
        assert_eq!(
            telemetry.noted.lock().unwrap().len(),
            1,
            "first refusal noted"
        );
        telemetry.note_refused("session_start", "zai/glm-5.3", dir.path());
        assert_eq!(telemetry.noted.lock().unwrap().len(), 1);
        telemetry.note_refused("session_start", "zai/glm-5.3-flash", dir.path());
        telemetry.note_refused("spawn", "zai/glm-5.3", dir.path());
        assert_eq!(telemetry.noted.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "release builds send to the real endpoint"
    )]
    async fn refusal_telemetry_rebinds_when_the_cwd_moves() {
        let _telemetry = crate::agent_engine::tests::telemetry_opt_in();
        let dir = tempfile::tempdir().expect("tempdir");
        let telemetry = ModelRefusalTelemetry::new(dir.path().join("agent"), false);
        let first = dir.path().join("project-a");
        std::fs::create_dir_all(&first).unwrap();
        telemetry.note_refused("set_model", "zai/glm-5.3", &first);
        assert_eq!(
            telemetry.client.lock().unwrap().as_ref().unwrap().0,
            first,
            "client bound to the live cwd"
        );
        let second = dir.path().join("project-b");
        std::fs::create_dir_all(&second).unwrap();
        telemetry.note_refused("spawn", "zai/glm-5.3", &second);
        assert_eq!(
            telemetry.client.lock().unwrap().as_ref().unwrap().0,
            second,
            "client rebound after the cwd move"
        );
    }
}
