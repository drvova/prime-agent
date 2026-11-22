//! What a session's settings and identity hand its kernel: the shell, the
//! command prefix, and the environment its commands run under.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A shell file the resolver accepts: existing, and executable on unix.
fn executable_shell(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// The scripted session both shell-setting tests build: a model and
/// `stream_fn` the settings validation never reaches, no kernel prewarm.
async fn shell_test_session(
    cwd: PathBuf,
    agent_dir: PathBuf,
) -> anyhow::Result<crate::session_engine::engine::SessionEngine> {
    use crate::session_engine::engine::{create_session, SessionEngineConfig};

    let model = pa_agent::types::Model {
        id: "shell-test".into(),
        name: "Shell test".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 1_000,
        max_tokens: 100,
        max_tokens_explicit: false,
    };
    let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(model.clone()));
    create_session(SessionEngineConfig {
        cwd,
        agent_dir,
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        prewarm_kernel: Some(false),
        ..Default::default()
    })
    .await
}

#[tokio::test]
async fn project_settings_cannot_replace_the_kernel_shell() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
    // A repository cannot choose the executable that wraps every shell command.
    let global = executable_shell(dir.path(), "global-bash");
    let project = executable_shell(dir.path(), "project-bash");
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::json!({ "shellPath": global.to_string_lossy() }).to_string(),
    )
    .unwrap();
    std::fs::write(
        cwd.join(".prime/agent/settings.json"),
        serde_json::json!({ "shellPath": format!("  {}  ", project.to_string_lossy()) })
            .to_string(),
    )
    .unwrap();
    let engine = shell_test_session(cwd, agent_dir).await.unwrap();
    assert_eq!(engine.provisioner.environment().shell_path, Some(global));
}

#[tokio::test]
async fn session_settings_reach_the_kernel_command_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::json!({ "shellCommandPrefix": "shopt -s expand_aliases" }).to_string(),
    )
    .unwrap();
    std::fs::write(
        cwd.join(".prime/agent/settings.json"),
        serde_json::json!({ "shellCommandPrefix": "attacker-command" }).to_string(),
    )
    .unwrap();
    let engine = shell_test_session(cwd, agent_dir).await.unwrap();
    assert_eq!(
        engine.provisioner.environment().command_prefix.as_deref(),
        Some("shopt -s expand_aliases")
    );
}

#[tokio::test]
async fn the_kernel_env_names_the_selected_provider_and_carries_no_key() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::json!({ "webSearchProvider": "eco" }).to_string(),
    )
    .unwrap();
    // The stored key enables the skill; the host resolves it per search,
    // so it never rides the kernel env.
    std::fs::write(
        agent_dir.join("auth.json"),
        r#"{ "eco": { "type": "api_key", "key": "!printf eco-key-from-cmd" } }"#,
    )
    .unwrap();
    let engine = shell_test_session(cwd, agent_dir).await.unwrap();
    let websearch_env: std::collections::BTreeMap<&str, &str> = engine
        .provisioner
        .environment()
        .env
        .iter()
        .filter(|(key, _)| key.contains("WEBSEARCH") || key.ends_with("_API_KEY"))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        websearch_env,
        std::collections::BTreeMap::from([("PRIME_AGENT_WEBSEARCH_PROVIDER", "eco")])
    );
}

/// The kernel's commands learn the session's agent dir.
#[tokio::test]
async fn the_kernel_env_names_the_agent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
    let engine = shell_test_session(cwd, agent_dir.clone()).await.unwrap();
    let layout_env: std::collections::BTreeMap<&str, &str> = engine
        .provisioner
        .environment()
        .env
        .iter()
        .filter(|(key, _)| key.starts_with("PRIME_AGENT_CODING"))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let agent_dir = agent_dir.to_string_lossy().to_string();
    assert_eq!(
        layout_env,
        std::collections::BTreeMap::from([("PRIME_AGENT_CODING_AGENT_DIR", agent_dir.as_str(),)])
    );
}

#[tokio::test]
async fn an_unsaved_session_blanks_the_kernel_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
    let engine = shell_test_session(cwd, agent_dir).await.unwrap();
    assert_eq!(
        engine
            .provisioner
            .environment()
            .env
            .iter()
            .find(|(name, _)| name == "RLM_SESSION_ID")
            .map(|(_, value)| value.as_str()),
        Some(""),
        "an inherited RLM_SESSION_ID must not reach an unsaved session's kernel"
    );
}

#[tokio::test]
async fn session_settings_reject_a_missing_shell_path() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
    let missing = cwd.join("no-such-shell");
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::json!({ "shellPath": missing.to_string_lossy() }).to_string(),
    )
    .unwrap();
    let Err(err) = shell_test_session(cwd, agent_dir).await else {
        panic!("create_session must reject a missing shellPath");
    };
    let message = err.to_string();
    assert!(
        message.contains("shellPath"),
        "error names the setting: {message}"
    );
    assert!(
        message.contains(missing.to_string_lossy().as_ref()),
        "error names the path: {message}"
    );
}
