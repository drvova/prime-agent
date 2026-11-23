//! The re-read after a compaction, over a real session: the model's turns
//! are scripted, its kernel and files are real.
#![cfg(unix)]

use std::path::Path;
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

/// One turn in which the model evaluates `code`, then answers.
async fn run_cell(engine: &SessionEngine, provider: &ScriptedProvider, code: &str) {
    provider.push_tool_call_turn(None, vec![("cell", "eval", json!({ "code": code }))]);
    provider.push_text_turn("done");
    engine.prompt("go", PromptOptions::default()).await.unwrap();
    engine.session.agent().wait_for_idle().await;
}

/// The view `fs-read` shows of `name` in `cwd` now.
fn view(cwd: &Path, name: &str) -> serde_json::Value {
    let request = crate::fs_edit::ReadRequest {
        target: crate::fs_edit::Target {
            path: cwd.join(name),
            shown: name.to_string(),
        },
        start: 1,
        end: None,
    };
    crate::fs_edit::read(&request).unwrap().0
}

/// After a compaction the session shows the files its cells read or edited
/// last before the cut, newest first and as they are on disk now: four at
/// most, none the kept tail reads, a file past the budget as a note. The
/// next turn edits with an anchor from that view and no read in between,
/// and the only request that rewrote the one before it is the first after
/// the compaction.
#[tokio::test(flavor = "multi_thread")]
async fn after_a_compaction_the_recent_files_are_shown_with_fresh_anchors() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let numbered = |prefix: &str, count: usize| {
        (1..=count).fold(String::new(), |mut text, line| {
            let _ = writeln!(text, "fn {prefix}_{line}() -> usize {{ {line} }}");
            text
        })
    };
    for (name, text) in [
        ("five.rs", numbered("five", 3)),
        ("old.rs", numbered("old", 5)),
        ("big.rs", numbered("big", 2_500)),
        ("mid.rs", numbered("mid", 4)),
        ("a.rs", numbered("a", 6)),
        ("tail.rs", numbered("tail", 3)),
    ] {
        std::fs::write(cwd.join(name), text).unwrap();
    }
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = create_session(SessionEngineConfig {
        cwd: cwd.clone(),
        agent_dir: dir.path().join("agent"),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    for code in [
        "(begin (fs-read \"five.rs\") \"read\")",
        "(begin (fs-read \"old.rs\") \"read\")",
        "(begin (fs-read \"big.rs\") \"read\")",
        "(let ([v (fs-read \"mid.rs\")]) (fs-edit \"mid.rs\" (hash 'replace (fs-anchor v 1) 'text \"fn mid_1() -> usize { 10 }\")) \"edited\")",
        "(begin (fs-read \"a.rs\") \"read\")",
        "(make-string 4000 #\\x)",
        "(make-string 4000 #\\y)",
        "(begin (fs-read \"tail.rs\") \"read\")",
    ] {
        run_cell(&engine, &provider, code).await;
    }
    // Changed on disk after the model read it: the re-read shows it now.
    std::fs::write(
        cwd.join("a.rs"),
        numbered("a", 6).replace("fn a_2() -> usize { 2 }", "fn a_2() -> usize { 20 }"),
    )
    .unwrap();

    let summarizer =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            api: Some(format!(
                "faux-recent-files-{:?}",
                std::time::SystemTime::now()
            )),
            provider: Some("faux-recent-files".to_string()),
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "recent-files-summarizer".to_string(),
                name: None,
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(200_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    summarizer.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "## Goal\nedit the files",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    // A split turn makes a second summarizer call.
    summarizer.set_repeat_last_response(true);
    engine
        .session
        .set_compaction_settings(super::super::compaction::CompactionSettings {
            keep_recent_tokens: 1_500,
            ..Default::default()
        });
    let outcome = engine
        .session
        .compact(None, &summarizer.get_model(), None, None)
        .await
        .unwrap();
    summarizer.unregister();
    assert!(
        matches!(
            outcome,
            super::super::compact_session::CompactOutcome::Ran(_)
        ),
        "the compaction ran"
    );

    let row = engine
        .session
        .agent()
        .state()
        .await
        .messages
        .last()
        .and_then(|message| serde_json::to_value(message).ok())
        .expect("a last message");
    let a = view(&cwd, "a.rs");
    let big = view(&cwd, "big.rs");
    let big_text = big["text"].as_str().unwrap();
    let expected = format!(
        "[recent-files]\n\nThe files your eval cells read or edited last before the compaction, newest first, read again from disk after it. Their anchors are current: `fs-edit` takes them without another `fs-read` while a file stays as shown here.\n\n{}\n\n{}\n\n{}\n\n- big.rs: 2500 lines, about {} tokens, too large to show here; `(fs-read \"big.rs\")` it again when you need it.",
        a["text"].as_str().unwrap(),
        view(&cwd, "mid.rs")["text"].as_str().unwrap(),
        view(&cwd, "old.rs")["text"].as_str().unwrap(),
        big_text.chars().count() / 4,
    );
    println!("recent-files row: ~{} tokens", expected.chars().count() / 4);
    assert_eq!(
        (row["customType"].clone(), row["content"].clone()),
        (json!(RECENT_FILES_CUSTOM_TYPE), json!(expected))
    );

    // The next turn edits line 2 with the anchor the row showed.
    let anchor = a["lines"][1]["anchor"].as_str().unwrap().to_string();
    assert!(expected.contains(&format!("{anchor}|fn a_2() -> usize {{ 20 }}")));
    run_cell(
        &engine,
        &provider,
        &format!("(begin (fs-edit \"a.rs\" (hash 'replace \"{anchor}\" 'text \"fn a_2() -> usize {{ 200 }}\")) \"edited\")"),
    )
    .await;
    let requests = provider.calls();
    let last_result = requests
        .last()
        .and_then(|request| request.messages.last())
        .and_then(|message| serde_json::to_value(message).ok())
        .expect("the edit's result");
    assert_eq!(
        (
            last_result["isError"].clone(),
            std::fs::read_to_string(cwd.join("a.rs")).unwrap()
        ),
        (
            json!(false),
            numbered("a", 6).replace("fn a_2() -> usize { 2 }", "fn a_2() -> usize { 200 }")
        )
    );

    let prefixes: Vec<_> = requests
        .iter()
        .map(crate::session_engine::cache_prefix::RequestPrefix::of)
        .collect();
    let breaks: Vec<usize> = (1..prefixes.len())
        .filter(|at| prefixes[at - 1].broken_bytes(&prefixes[*at]).is_some())
        .collect();
    // Eight cells before the compaction, two requests each; the ninth's
    // first request is the compaction's one break.
    assert_eq!((requests.len(), breaks), (18, vec![16]));
    engine.dispose_kernel().await;
}
