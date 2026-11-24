//! Nested context over a real session: the model's turns are scripted, its kernel and files
//! are real.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;
use serde_json::json;

use super::*;
use crate::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};
use crate::session_engine::PromptOptions;

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 200_000,
        max_tokens: 1_000,
        max_tokens_explicit: false,
    }
}

/// A project at `<tmp>/project` with root rules and `crates/foo/AGENTS.md`, an ignored
/// `vendor/AGENTS.md`, and a sibling directory outside it with its own AGENTS.md; `settings`
/// is the global settings document.
fn project(settings: &serde_json::Value) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = pa_types::platform::canonicalize(dir.path())
        .unwrap()
        .join("project");
    for (name, text) in [
        ("project/AGENTS.md", "Root rules."),
        ("project/.gitignore", "vendor/\n"),
        ("project/crates/foo/AGENTS.md", "Foo rules."),
        ("project/crates/foo/src/lib.rs", "pub fn lib() {}\n"),
        ("project/crates/foo/src/other.rs", "pub fn other() {}\n"),
        ("project/vendor/AGENTS.md", "Vendored rules."),
        ("project/vendor/dep.rs", "pub fn dep() {}\n"),
        ("outside/AGENTS.md", "Outside rules."),
        ("outside/x.rs", "pub fn x() {}\n"),
        ("agent/settings.json", &settings.to_string()),
    ] {
        let path = pa_types::platform::canonicalize(dir.path())
            .unwrap()
            .join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    (dir, cwd)
}

/// Run one turn per cell in a session in `cwd`, then the requests the model was sent.
async fn requests(dir: &Path, cwd: &Path, cells: &[&str]) -> Vec<pa_agent::stream::LlmContext> {
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine: SessionEngine = create_session(SessionEngineConfig {
        cwd: cwd.to_path_buf(),
        agent_dir: dir.join("agent"),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    for code in cells {
        provider.push_tool_call_turn(None, vec![("cell", "eval", json!({ "code": code }))]);
        provider.push_text_turn("done");
        engine.prompt("go", PromptOptions::default()).await.unwrap();
        engine.session.agent().wait_for_idle().await;
    }
    engine.dispose_kernel().await;
    provider.calls()
}

/// Each nested-context message of `request`, its timestamp left out, with the role of the
/// message before it.
fn nested_rows(request: &pa_agent::stream::LlmContext) -> Vec<serde_json::Value> {
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|message| serde_json::to_value(message).unwrap())
        .collect();
    (1..messages.len())
        .filter(|at| {
            messages[*at]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("[nested-context"))
        })
        .map(|at| {
            let mut row = messages[at].clone();
            row.as_object_mut().unwrap().remove("timestamp");
            json!({ "after": messages[at - 1]["role"], "row": row })
        })
        .collect()
}

/// The first read under `crates/foo/` appends its AGENTS.md once, right after that cell's
/// result; a second read there, a read outside the project and a read in a gitignored
/// directory add nothing. The system prompt never changes and no request rewrites the one
/// before it.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_read_under_a_subdirectory_appends_its_agents_md_once() {
    let (dir, cwd) = project(&json!({}));
    let outside = pa_types::platform::canonicalize(dir.path())
        .unwrap()
        .join("outside/x.rs");
    let requests = requests(
        dir.path(),
        &cwd,
        &[
            "(begin (fs-read \"crates/foo/src/lib.rs\") \"read\")",
            "(begin (fs-read \"crates/foo/src/other.rs\") \"read\")",
            &format!("(begin (fs-read \"{}\") \"read\")", outside.display()),
            "(begin (fs-read \"vendor/dep.rs\") \"read\")",
        ],
    )
    .await;

    let last = requests.last().unwrap();
    assert_eq!(
        nested_rows(last),
        vec![json!({
            "after": "toolResult",
            "row": {
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "[nested-context crates/foo/AGENTS.md]\n\nThe project instructions for `crates/foo/`, loaded because a cell worked there. They apply to the files under that directory, beside the project context in the system prompt.\n\nFoo rules.",
                }],
            },
        })]
    );
    let prompts: Vec<_> = requests
        .iter()
        .map(|request| request.system_prompt.clone())
        .collect();
    assert_eq!(
        prompts,
        vec![requests[0].system_prompt.clone(); requests.len()]
    );
    let prefixes: Vec<_> = requests
        .iter()
        .map(crate::session_engine::cache_prefix::RequestPrefix::of)
        .collect();
    let breaks: Vec<usize> = (1..prefixes.len())
        .filter(|at| prefixes[at - 1].broken_bytes(&prefixes[*at]).is_some())
        .collect();
    assert_eq!((requests.len(), breaks), (8, Vec::<usize>::new()));
}

/// With `nestedContextFiles` off, the same read appends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn with_nested_context_files_off_a_read_appends_nothing() {
    let (dir, cwd) = project(&json!({ "nestedContextFiles": false }));
    let requests = requests(
        dir.path(),
        &cwd,
        &["(begin (fs-read \"crates/foo/src/lib.rs\") \"read\")"],
    )
    .await;
    assert_eq!(
        nested_rows(requests.last().unwrap()),
        Vec::<serde_json::Value>::new()
    );
}

/// A file past the cap is cut at its last whole line within it, and the row says where the
/// rest starts.
#[test]
fn a_file_past_the_cap_is_cut_at_a_line_with_where_the_rest_starts() {
    let line = format!("{}\n", "x".repeat(99));
    let kept = MAX_FILE_BYTES / 100;
    let row = nested_context_row("crates/foo/AGENTS.md", &line.repeat(kept + 5), 7);
    assert_eq!(
        row,
        CustomMessage {
            custom_type: NESTED_CONTEXT_CUSTOM_TYPE.to_string(),
            content: UserContent::Text(format!(
                "[nested-context crates/foo/AGENTS.md]\n\nThe project instructions for `crates/foo/`, loaded because a cell worked there. They apply to the files under that directory, beside the project context in the system prompt.\n\n{}\n[truncated: lines 1-{kept} of {}; read the rest with `(fs-read \"crates/foo/AGENTS.md\" (hash 'start {}))`]",
                line.repeat(kept),
                kept + 5,
                kept + 1
            )),
            display: true,
            details: Some(json!({ "path": "crates/foo/AGENTS.md", "truncated": true })),
            timestamp: 7,
            rest: serde_json::Map::default(),
        }
    );
}
