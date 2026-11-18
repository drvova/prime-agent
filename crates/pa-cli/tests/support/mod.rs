//! Shared by the pa-cli e2e targets that run the chat surface in-process
//! against a real `prime-agent` daemon.

/// Strip ambient provider credentials from THIS process, once.
///
/// The in-process client forwards its whole environment to the session
/// worker as the create's `launch_env` (by design: a user's shell keys reach
/// the daemon), so stripping them from the spawned supervisor alone does not
/// keep the worker hermetic. A `GH_TOKEN` exported on the box made GitHub
/// Copilot a signed-in provider, and its models then ranked above the
/// fixtures' in the `/model` picker. Call it before the daemon spawns and
/// before the client runs.
pub fn scrub_ambient_provider_credentials() {
    static SCRUB: std::sync::Once = std::sync::Once::new();
    SCRUB.call_once(|| {
        for provider in pa_ai::models_generated::get_providers() {
            if let Some(vars) = pa_ai::env_api_keys::get_api_key_env_vars(provider) {
                for var in vars {
                    std::env::remove_var(var);
                }
            }
        }
        std::env::remove_var("PRIME_TEAM_ID");
    });
}
