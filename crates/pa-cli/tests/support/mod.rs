//! Shared by the pa-cli e2e targets that run against a real `prime-agent`
//! daemon: the wait for it to listen, and the credential scrub of the ones
//! that run the chat surface in-process.

/// Strip ambient provider credentials from THIS process, once.
///
/// The in-process client forwards its whole environment to the session
/// worker as the create's `launch_env` (by design: a user's shell keys reach
/// the daemon), so stripping them from the spawned supervisor alone does not
/// keep the worker hermetic. A `GH_TOKEN` exported on the box made GitHub
/// Copilot a signed-in provider, and its models then ranked above the
/// fixtures' in the `/model` picker. Call it before the daemon spawns and
/// before the client runs.
// Each test target compiles this module for itself, and not every one runs
// the client in-process.
#[allow(dead_code)]
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

/// Wait until the daemon `child` accepts connections on `socket` (the twin
/// of pa-daemon's `tests/common` helper).
///
/// The socket's file is not readiness: it appears at `bind`, before `listen`,
/// and a connect in between is refused. This connects until one is accepted;
/// the probe connection is dropped at once.
///
/// # Panics
///
/// `child` exited first, or 15 seconds passed.
// Each test target compiles this module for itself, and not every one waits
// for a daemon through it.
#[allow(dead_code)]
#[cfg(unix)]
pub fn await_listening(socket: &std::path::Path, child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "the daemon exited before it listened on {}: {status}",
                socket.display()
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never listened on {}",
            socket.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
