//! `login <provider>` and `logout <provider>`, plus the `/login` panel flow,
//! for models.json providers whose `auth` descriptor declares `oauth`: the
//! browser (or device-code) sign-in runs here, in its own command or panel
//! task, and stores the provider's `oauth` credential in auth.json.

use std::path::Path;
use std::time::Instant;

use pa_core::auth::{
    sign_in_with_descriptor, OAuthSignInUi, SignInAttempt, SignInError, SignInFlow, SignInTiming,
};
use pa_core::session_engine::telemetry::OAuthLoginSurface;
use pa_tui::auth_panel::AuthPanelHandle;
use pa_tui::provider_auth::ProviderAuthOutcome;

use crate::config::COMMAND_NAME;

/// The terminal as the sign-in's surface: the URL (or the device code) is
/// always printed, and the browser opens beside it.
struct TerminalSignInUi;

impl OAuthSignInUi for TerminalSignInUi {
    fn show_authorization_url(&self, url: &str) {
        println!("Open this URL to sign in (opening your browser):\n\n  {url}\n");
        pa_core::platform::browser::open_in_browser(url);
    }

    fn show_device_code(&self, verification_url: &str, user_code: &str) {
        println!("Open {verification_url} on any device and enter the code:\n\n  {user_code}\n");
    }

    fn progress(&self, message: &str) {
        println!("{message}");
    }

    fn is_cancelled(&self) -> bool {
        // Ctrl-C ends the command, and the credential write with it.
        false
    }
}

/// The TUI's inline auth panel as the sign-in's surface.
struct PanelSignInUi {
    panel: AuthPanelHandle,
}

impl OAuthSignInUi for PanelSignInUi {
    fn show_authorization_url(&self, url: &str) {
        self.panel
            .auth_url(url, Some("Complete the sign-in in your browser."));
        pa_core::platform::browser::open_in_browser(url);
    }

    fn show_device_code(&self, verification_url: &str, user_code: &str) {
        self.panel.auth_url(
            verification_url,
            Some(&format!(
                "Open the URL on any device and enter the code {user_code}"
            )),
        );
    }

    fn progress(&self, message: &str) {
        self.panel.progress_line(message);
    }

    fn is_cancelled(&self) -> bool {
        self.panel.cancelled()
    }
}

/// Send the sign-in's adoption event (counts and enums only), unless
/// telemetry is off. The flush waits for delivery, so callers on a
/// user-facing path run it detached.
fn track_sign_in(
    agent_dir: &Path,
    surface: OAuthLoginSurface,
    attempt: &SignInAttempt,
    started: Instant,
) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let settings = pa_core::settings::SettingsManager::create(&cwd, agent_dir);
    if crate::mode::telemetry_disabled(&settings) {
        return;
    }
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        runtime.block_on(async {
            let client = pa_core::session_engine::telemetry::build_client(&settings, agent_dir);
            pa_core::session_engine::telemetry::track_provider_oauth_login(
                &client,
                surface,
                attempt,
                duration_ms,
            );
            let _ = client.shutdown().await;
        });
    }
}

/// `login <provider> [--device]`: the exit code.
pub(crate) fn run_login_command(args: &[String]) -> i32 {
    let (provider, flow) = match args {
        [provider] if !provider.starts_with('-') => (provider, SignInFlow::Auto),
        [provider, flag] | [flag, provider] if flag == "--device" && !provider.starts_with('-') => {
            (provider, SignInFlow::DeviceCode)
        }
        _ => {
            eprintln!("Usage: {COMMAND_NAME} login <provider> [--device]");
            return 2;
        }
    };
    let agent_dir = crate::config::get_agent_dir();
    let started = Instant::now();
    let attempt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(sign_in_with_descriptor(
            &agent_dir,
            provider,
            flow,
            &SignInTiming::default(),
            &TerminalSignInUi,
        )),
        Err(error) => SignInAttempt {
            method: None,
            result: Err(SignInError::Failed(format!(
                "the sign-in runtime could not start: {error}"
            ))),
        },
    };
    let code = match &attempt.result {
        Ok(()) => {
            println!(
                "Signed in to {provider}. Credentials saved to {}",
                agent_dir.join("auth.json").display()
            );
            0
        }
        Err(error) => {
            eprintln!("Error: could not sign in to {provider}: {error}");
            1
        }
    };
    // After the outcome is printed: the event never delays it.
    track_sign_in(&agent_dir, OAuthLoginSurface::Cli, &attempt, started);
    code
}

/// `logout <provider>`: remove the provider's stored credential (any kind,
/// like `/logout`); the exit code.
pub(crate) fn run_logout_command(args: &[String]) -> i32 {
    let [provider] = args else {
        eprintln!("Usage: {COMMAND_NAME} logout <provider>");
        return 2;
    };
    let agent_dir = crate::config::get_agent_dir();
    let mut auth = pa_core::auth::AuthStorage::create(&agent_dir);
    if auth.get_all().get(provider).is_none() {
        println!("{provider} is not signed in.");
        return 0;
    }
    auth.logout(provider);
    if let Some(error) = auth.drain_errors().pop() {
        eprintln!("Error: logout failed: {error}");
        return 1;
    }
    println!("Logged out of {provider}.");
    0
}

/// The `/login` picker's sign-in for a descriptor `oauth` row, on the
/// inline auth panel (blocking: the caller runs it off the async workers).
pub(crate) fn run_panel_sign_in(
    agent_dir: &Path,
    provider_id: &str,
    provider_name: &str,
    panel: AuthPanelHandle,
) -> ProviderAuthOutcome {
    let started = Instant::now();
    let ui = PanelSignInUi { panel };
    let attempt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(sign_in_with_descriptor(
            agent_dir,
            provider_id,
            SignInFlow::Auto,
            &SignInTiming::default(),
            &ui,
        )),
        Err(error) => SignInAttempt {
            method: None,
            result: Err(SignInError::Failed(format!(
                "the sign-in runtime could not start: {error}"
            ))),
        },
    };
    let outcome = match &attempt.result {
        Ok(()) => ProviderAuthOutcome::Status(format!(
            "Signed in to {provider_name}. Credentials saved to {}",
            agent_dir.join("auth.json").display()
        )),
        Err(SignInError::Cancelled) => ProviderAuthOutcome::Cancelled,
        Err(SignInError::Failed(reason)) => {
            ProviderAuthOutcome::Error(format!("Failed to login to {provider_name}: {reason}"))
        }
    };
    // Detached: the panel shows the outcome without waiting on delivery.
    let agent_dir = agent_dir.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("oauth-login-telemetry".to_string())
        .spawn(move || track_sign_in(&agent_dir, OAuthLoginSurface::Tui, &attempt, started));
    outcome
}
