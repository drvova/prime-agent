use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::types::ThinkingLevel;
use serde_json::{json, Value};

use super::{PromptInputs, ReloadInputs, ResourceReload};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::session::manager::SessionManager;
use crate::session_engine::turn_boundary::{ModelInfo, TurnBoundaryRequests, TurnBoundaryRuntime};

fn agent() -> Arc<Agent> {
    Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("creation prompt".to_string()),
            model: Some(pa_agent::types::Model {
                id: "faux-1".to_string(),
                name: "Faux".to_string(),
                api: "test".to_string(),
                provider: "faux".to_string(),
                base_url: "http://localhost".to_string(),
                reasoning: false,
                cost: pa_agent::types::UsageCost::default(),
                context_window: 100_000,
                max_tokens: 1_000,
                max_tokens_explicit: false,
            }),
            thinking_level: Some(ThinkingLevel::Off),
            tools: None,
            messages: None,
        },
        stream_fn: None,
        ..Default::default()
    }))
}

fn write_skill(root: &std::path::Path, name: &str, python: bool) {
    let dir = root.join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Does {name} things. Use when testing reloads.\n---\n\n# {name}\n"),
    )
    .unwrap();
    if python {
        let import = name.replace('-', "_");
        std::fs::create_dir_all(dir.join("src").join(&import)).unwrap();
        std::fs::write(
            dir.join("src").join(&import).join("__init__.py"),
            "def run():\n    return 1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
    }
}

async fn reload(handlers: &HostRequestHandlers) -> Value {
    let handler = handlers.get("rlm.reload").unwrap().clone();
    let mut reply = handler(HostRequestPayload { data: json!({}) })
        .await
        .unwrap();
    // The discovered totals depend on the bundled skills the build ships.
    for key in ["skills", "promptTemplates", "contextFiles", "diagnostics"] {
        reply.as_object_mut().unwrap().remove(key);
    }
    reply
}

/// A skill written after the session started becomes visible on reload: the reply names it, the
/// model's system prompt lists it from the next call, `/skill:` and the catalog read it, a Python
/// skill comes back for the kernel client to bind, and a second reload changes nothing.
#[tokio::test]
async fn a_skill_written_mid_session_is_usable_after_reload() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("project");
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let requests = Arc::new(TurnBoundaryRequests::new());
    let agent = agent();
    requests.bind(TurnBoundaryRuntime {
        agent: Arc::clone(&agent),
        session: Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(&cwd))),
        context_window: Some(100_000),
        model_info: ModelInfo {
            id: "faux-1".to_string(),
            provider: "faux".to_string(),
            input: Vec::new(),
        },
    });
    let added = Arc::new(AtomicUsize::new(0));
    let reload_state = ResourceReload::new(
        &requests,
        Some({
            let added = Arc::clone(&added);
            Arc::new(move |count| {
                added.fetch_add(count, Ordering::SeqCst);
            })
        }),
    );
    let mut handlers = HostRequestHandlers::default();
    reload_state.register(&mut handlers);
    reload_state.seed(
        ReloadInputs {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            extra_builtin_skill_overrides: Vec::new(),
            additional_skill_paths: Vec::new(),
            additional_prompt_paths: Vec::new(),
            custom_system_prompt: None,
            prompt_guidelines: Vec::new(),
            hidden_skills: Vec::new(),
            prompt: PromptInputs {
                model_prompt_extras: None,
                messages_path: None,
                selected_tools: vec!["python_repl".to_string()],
                allow_recursion: None,
                rlm_depth: Some(0),
                factory_child: false,
                daemonless: true,
                can_spawn: false,
                generic_mcp_servers: Vec::new(),
                project_card: None,
            },
        },
        Vec::new(),
        Vec::new(),
        "creation prompt".to_string(),
        Vec::new(),
    );
    // The baseline: the bundled skills the build ships, if any.
    let baseline = reload(&handlers).await;
    assert_eq!(baseline["skillsRemoved"], json!([]));
    assert_eq!(baseline["systemPromptChanged"], json!(true));
    added.store(0, Ordering::SeqCst);

    write_skill(&agent_dir, "fresh-notes", false);
    write_skill(&agent_dir, "word-count", true);
    let package_path = agent_dir.join("skills").join("word-count");
    assert_eq!(
        reload(&handlers).await,
        json!({
            "skillsAdded": ["fresh-notes", "word-count"],
            "skillsRemoved": [],
            "systemPromptChanged": true,
            "pythonSkills": [{
                "name": "word-count",
                "importName": "word_count",
                "packagePath": package_path.display().to_string(),
            }],
        })
    );
    assert_eq!(added.load(Ordering::SeqCst), 2);
    let prompt = agent.state().await.system_prompt;
    assert!(
        prompt.contains("fresh-notes") && prompt.contains("word-count"),
        "{prompt}"
    );
    assert_eq!(reload_state.base_prompt(), prompt);
    assert!(reload_state
        .skills()
        .iter()
        .any(|skill| skill.name == "fresh-notes"));

    assert_eq!(
        reload(&handlers).await,
        json!({
            "skillsAdded": [],
            "skillsRemoved": [],
            "systemPromptChanged": false,
            "pythonSkills": [],
        })
    );
}
