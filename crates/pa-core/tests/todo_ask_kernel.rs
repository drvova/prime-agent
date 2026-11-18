// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! Verifier: the kernel `todo.*` and `ask(...)` APIs over a REAL kernel and the full
//! session-engine tool path. Session one sets and updates a todo list and asks a question a
//! scripted surface answers; session two reopens the session file in a fresh kernel (no
//! surface: headless), reads the same list back, and its `ask` settles as `no_user` at once.
//! No model call: the `ScriptedProvider` answers from the pushed turns.
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pa_agent::scripted::{tool_call_turn_steps, ScriptedProvider, ScriptedTurn};
use pa_core::interaction::{InteractionEvent, InteractionHub, InteractionSurface};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};
use pa_core::session_engine::PromptOptions;
use pa_types::question::{Question, QuestionKind, QuestionReply};
use serde_json::json;

/// The kernel Python with prime-agent-runtime installed. Skipped (with a note) on machines
/// without a live install; `PA_CORE_KERNEL_PYTHON` points at an explicit one.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_CORE_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string(),
        |home| format!("{home}/.prime/agent/kernel-venv/bin/python"),
    ));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel test",
        candidate.display()
    );
    None
}

fn scripted_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "test", "provider": "faux",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4_000
    }))
    .expect("faux loop model")
}

/// A user who is always attached: every opened question is forwarded to the test.
struct AttachedUser {
    opened: tokio::sync::mpsc::UnboundedSender<Question>,
}

impl InteractionSurface for AttachedUser {
    fn user_available(&self) -> bool {
        true
    }

    fn persisted_todos(&self) -> Option<pa_types::todo::TodoList> {
        None
    }

    fn publish(&self, event: InteractionEvent) {
        if let InteractionEvent::QuestionOpened(question) = event {
            let _ = self.opened.send(question);
        }
    }
}

fn config(
    cwd: PathBuf,
    agent_dir: PathBuf,
    session: SessionManager,
    provider: &Arc<ScriptedProvider>,
    interaction: Option<Arc<InteractionHub>>,
) -> SessionEngineConfig {
    SessionEngineConfig {
        cwd,
        agent_dir,
        model: Some(scripted_model()),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(session),
        interaction,
        daemonless: true,
        ..SessionEngineConfig::default()
    }
}

/// One `python_repl` turn running `code`, then a closing text turn; returns the cell's output.
async fn run_cell(engine: &SessionEngine, provider: &ScriptedProvider, code: &str) -> String {
    let model = scripted_model();
    provider.push_turn(ScriptedTurn::Events(tool_call_turn_steps(
        &model,
        None,
        vec![("call-1", "python_repl", json!({ "code": code }))],
    )));
    provider.push_text_turn("done");
    engine
        .prompt("go", PromptOptions::default())
        .await
        .expect("prompt");
    let state = engine.session.agent().state().await;
    state
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::ToolResult(
                result,
            )) => Some(
                result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        pa_agent::types::ToolResultContent::Text(text) => Some(text.text.clone()),
                        pa_agent::types::ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn todo_and_ask_round_trip_through_a_real_kernel_and_survive_a_reload() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    std::env::set_var("PRIME_AGENT_KERNEL_PYTHON", &kernel_python);
    let dir = tempfile::tempdir().expect("temp dir");
    let agent_dir = dir.path().join("agent");
    let sessions_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).expect("project dir");

    // Session one: a user is attached and answers the question.
    let mut session = SessionManager::in_memory(&cwd);
    let session_file = session.materialize_session_file(Some(sessions_dir.clone()));
    let (opened_tx, mut opened) = tokio::sync::mpsc::unbounded_channel();
    let hub = InteractionHub::with_surface(Arc::new(AttachedUser { opened: opened_tx }));
    let answering = tokio::spawn({
        let hub = Arc::clone(&hub);
        async move {
            let question = opened.recv().await.expect("a question opens");
            hub.answer(
                &question.id,
                QuestionReply::Answered {
                    answer: vec!["postgres".to_string()],
                },
            )
            .expect("the answer fits");
            question
        }
    });
    let provider = Arc::new(ScriptedProvider::new(scripted_model()));
    let engine = create_session(config(
        cwd.clone(),
        agent_dir.clone(),
        session,
        &provider,
        Some(Arc::clone(&hub)),
    ))
    .await
    .expect("create session one");
    let output = run_cell(
        &engine,
        &provider,
        "await todo.set(['read the schema', {'text': 'migrate', 'status': 'in_progress'}])\n\
         todos = await todo.update('1', status='done', note='3 tables')\n\
         answer = await ask('Which database?', ['sqlite', 'postgres'], default='sqlite')\n\
         print(todos)\n\
         print(answer.status, answer.answer)",
    )
    .await;
    assert_eq!(
        output.trim(),
        "todo list: 1/2 done\n[x] 1. read the schema (3 tables)\n[~] 2. migrate\nanswered postgres"
    );
    let asked = tokio::time::timeout(Duration::from_secs(5), answering)
        .await
        .expect("the answer task settles")
        .expect("the answer task");
    assert_eq!(
        asked,
        Question {
            // The kernel mints the id; the rest is what the cell asked.
            id: asked.id.clone(),
            text: "Which database?".to_string(),
            kind: QuestionKind::Single,
            choices: vec!["sqlite".to_string(), "postgres".to_string()],
            default: vec!["sqlite".to_string()],
            timeout_seconds: None,
        }
    );
    engine.dispose_kernel().await;
    drop(engine);

    // Session two: the reopened file in a fresh kernel, nobody attached.
    let reopened = SessionManager::open(&cwd, &sessions_dir, &session_file);
    let provider = Arc::new(ScriptedProvider::new(scripted_model()));
    let engine = create_session(config(cwd, agent_dir, reopened, &provider, None))
        .await
        .expect("create session two");
    let output = run_cell(
        &engine,
        &provider,
        "import time\n\
         print(await todo.get())\n\
         started = time.monotonic()\n\
         answer = await ask('Still there?', ['yes', 'no'], default='yes', timeout=600)\n\
         print(answer.status, answer.answer, time.monotonic() - started < 5)",
    )
    .await;
    assert_eq!(
        output.trim(),
        "todo list: 1/2 done\n[x] 1. read the schema (3 tables)\n[~] 2. migrate\nno_user yes True"
    );
    engine.dispose_kernel().await;
}
