use super::*;
use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentTool, Message};
use serde_json::json;

pub(super) fn test_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

/// A parent session with one finished exchange and the three model-facing tools.
pub(super) fn parent_agent(provider: &Arc<ScriptedProvider>) -> Arc<Agent> {
    let tools: Vec<Arc<dyn AgentTool>> = ["bash", "edit", pa_types::ai::PYTHON_TOOL_NAME]
        .into_iter()
        .map(read_only::tests::named_tool)
        .collect();
    Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("main system prompt".to_string()),
            model: Some(test_model()),
            thinking_level: None,
            tools: Some(tools),
            messages: Some(vec![pa_agent::types::AgentMessage::user(
                "we talked about the parser",
            )]),
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }))
}

pub(super) fn host(
    provider: &Arc<ScriptedProvider>,
    parent: &Arc<Agent>,
    cwd: &Path,
    spill: Option<crate::kernel::spill::SpillStore>,
) -> BetterHost {
    let host = BetterHost::new(cwd.to_path_buf(), None, None, spill);
    host.bind(
        parent,
        provider.stream_fn(),
        Arc::new(|_| Box::pin(async {})),
    );
    host
}

pub(super) fn deepen(task: &str) -> BetterRequest {
    BetterRequest {
        task: task.to_string(),
        mode: BetterMode::Deepen,
        cwd: None,
    }
}

pub(super) fn quiet() -> BetterProgress {
    Arc::new(|_| {})
}

/// The text of every tool result the helper's context carried in the given provider call.
pub(super) fn tool_results(provider: &ScriptedProvider, call: usize) -> Vec<String> {
    provider.calls()[call]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(
                serde_json::to_value(&result.content).unwrap()[0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
            _ => None,
        })
        .collect()
}

const REPLY: &str = "<depth>(1) a regression test (2) the failing input (3) cargo test (4) the \
                     test</depth>\n<unresolved>\n</unresolved>\n<rewrite>\nFix `parse` in \
                     `parser.rs`; prove it with a failing test first.\n</rewrite>";

#[tokio::test]
async fn the_helper_reads_then_rewrites_and_the_session_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("parser.rs"), "fn parse() {}\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_tool_call_turn(
        None,
        vec![(
            "c1",
            pa_types::ai::PYTHON_TOOL_NAME,
            json!({ "code": "print(await fs.read('parser.rs'))" }),
        )],
    );
    provider.push_text_turn(REPLY);
    let parent = parent_agent(&provider);
    let labels = Arc::new(std::sync::Mutex::new(Vec::new()));
    let progress: BetterProgress = {
        let labels = Arc::clone(&labels);
        Arc::new(move |label| labels.lock().unwrap().push(label.to_string()))
    };
    let outcome = host(&provider, &parent, dir.path(), None)
        .run(&deepen("fix the parser"), &AbortSignal::never(), &progress)
        .await;
    assert_eq!(
        outcome,
        BetterOutcome::Rewritten {
            rewrite: "Fix `parse` in `parser.rs`; prove it with a failing test first.".to_string(),
            unchanged: false,
        }
    );
    assert_eq!(
        *labels.lock().unwrap(),
        vec!["fs.read parser.rs".to_string()]
    );
    // The helper's requests carry the session's system prompt and tool definitions unchanged
    // (the cached prefix), then the cloned conversation and the helper's instructions.
    let first = &provider.calls()[0];
    assert_eq!(first.system_prompt.as_deref(), Some("main system prompt"));
    assert_eq!(
        first
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["bash", "edit", "python_repl"]
    );
    let sent = serde_json::to_string(&first.messages).unwrap();
    assert!(sent.contains("we talked about the parser"), "{sent}");
    assert!(
        sent.contains("<prime_better mode=\\\"deepen\\\">"),
        "{sent}"
    );
    assert!(tool_results(&provider, 1)[0].contains("|fn parse() {}"));
    // Nothing of the run entered the session.
    assert_eq!(parent.state().await.messages.len(), 1);
}

#[tokio::test]
async fn a_helper_that_tries_to_edit_is_refused_and_the_file_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("parser.rs");
    std::fs::write(&file, "fn parse() {}\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_tool_call_turn(
        None,
        vec![
            (
                "c1",
                "edit",
                json!({ "path": "parser.rs", "oldText": "fn", "newText": "x" }),
            ),
            (
                "c2",
                pa_types::ai::PYTHON_TOOL_NAME,
                json!({ "code": "open('parser.rs', 'w').write('gone')" }),
            ),
            (
                "c3",
                "bash",
                json!({ "command": "sed -i s/fn/x/ parser.rs" }),
            ),
        ],
    );
    provider.push_text_turn(REPLY);
    let parent = parent_agent(&provider);
    let outcome = host(&provider, &parent, dir.path(), None)
        .run(&deepen("fix the parser"), &AbortSignal::never(), &quiet())
        .await;
    assert!(
        matches!(outcome, BetterOutcome::Rewritten { .. }),
        "{outcome:?}"
    );
    let results = tool_results(&provider, 1);
    assert_eq!(results[0], read_only::EDIT_REFUSED);
    assert_eq!(
        results
            .iter()
            .map(|text| text.starts_with("Refused: "))
            .collect::<Vec<_>>(),
        vec![true, true, true],
        "{results:#?}"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn parse() {}\n");
}

#[tokio::test]
async fn a_reply_without_a_rewrite_fails_and_keeps_the_whole_reply() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let store = crate::kernel::spill::SpillStore::for_session_dir(session_dir.path());
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let reply = "I went ahead and fixed the parser for you.";
    provider.push_text_turn(reply);
    let parent = parent_agent(&provider);
    let outcome = host(&provider, &parent, dir.path(), Some(store.clone()))
        .run(&deepen("fix the parser"), &AbortSignal::never(), &quiet())
        .await;
    let id = crate::kernel::spill::content_id(reply);
    let path = session_dir.path().join("spill").join(format!("{id}.txt"));
    assert_eq!(
        outcome,
        BetterOutcome::Failed {
            error: reply::MISSING_REWRITE.to_string(),
            reply: Some(KeptReply {
                text: reply.to_string(),
                stored: Some((format!("out:{id}"), path.clone())),
            }),
        }
    );
    // The store's writer lands it byte for byte (the lookup waits for the write).
    assert_eq!(
        store.lookup(&id).await,
        crate::kernel::spill::SpillLookup::Stored(crate::kernel::spill::StoredSpill {
            text: reply.to_string(),
            total_chars: reply.chars().count(),
            lines: 1,
        })
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), reply);
    assert_eq!(
        outcome.error_text(),
        Some(format!(
            "{}; the full reply is kept as out:{id} at {}",
            reply::MISSING_REWRITE,
            path.display()
        ))
    );
}

#[tokio::test]
async fn cancelling_a_run_ends_it_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_stalled_turn("<depth>");
    let parent = parent_agent(&provider);
    let controller = AbortController::new();
    let host = host(&provider, &parent, dir.path(), None);
    let (request, signal, progress) = (deepen("fix the parser"), controller.signal(), quiet());
    let run = host.run(&request, &signal, &progress);
    let cancel = async {
        // Cancel once the helper's request is in flight (observable readiness).
        while provider.calls().is_empty() {
            tokio::task::yield_now().await;
        }
        controller.abort();
    };
    let (outcome, ()) = tokio::join!(run, cancel);
    assert_eq!(outcome, BetterOutcome::Cancelled);
}

/// `better.run` in brief mode: the helper starts from no conversation, and a brief that needs
/// nothing comes back unchanged.
#[tokio::test]
async fn a_brief_that_needs_nothing_comes_back_unchanged_through_the_kernel_request() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("parser.rs"), "fn parse() {}\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("<unchanged/>");
    let parent = parent_agent(&provider);
    let host = Arc::new(host(&provider, &parent, dir.path(), None));
    let mut handlers = crate::kernel::shared::HostRequestHandlers::new();
    register_better_handler(&mut handlers, &host);
    let handler = handlers.get(BETTER_REQUEST_TYPE).expect("registered");
    let brief = "Add a test for `parse` in `parser.rs`.";
    let reply = handler(crate::kernel::shared::HostRequestPayload {
        data: json!({ "task": brief, "mode": "brief" }),
    })
    .await
    .unwrap();
    assert_eq!(reply, json!({ "rewrite": brief, "unchanged": true }));
    let sent = serde_json::to_string(&provider.calls()[0].messages).unwrap();
    assert!(!sent.contains("we talked about the parser"), "{sent}");
    assert!(sent.contains("<prime_better mode=\\\"brief\\\">"), "{sent}");
}

/// The last user message the helper sent in the given provider call.
fn last_prompt(provider: &ScriptedProvider, call: usize) -> String {
    provider.calls()[call]
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(serde_json::to_string(&user.content).unwrap()),
            _ => None,
        })
        .unwrap_or_default()
}

fn reply_citing(rewrite: &str, unresolved: &str) -> String {
    format!("<depth>(4) a test</depth>\n<unresolved>\n{unresolved}\n</unresolved>\n<rewrite>\n{rewrite}\n</rewrite>")
}

/// A rewrite that cites a path the repository does not have goes back for one refine pass, and
/// the refined rewrite (which dropped it) is what the user gets.
#[tokio::test]
async fn an_invented_path_is_dropped_by_verification() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("parser.rs"), "fn parse_expr() {}\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn(&reply_citing(
        "Fix `parse_expr` in `parser.rs` and `src/lexer.rs`.",
        "",
    ));
    provider.push_text_turn(&reply_citing("Fix `parse_expr` in `parser.rs`.", ""));
    let parent = parent_agent(&provider);
    let outcome = host(&provider, &parent, dir.path(), None)
        .run(&deepen("fix the parser"), &AbortSignal::never(), &quiet())
        .await;
    assert_eq!(
        outcome,
        BetterOutcome::Rewritten {
            rewrite: "Fix `parse_expr` in `parser.rs`.".to_string(),
            unchanged: false,
        }
    );
    let refine = last_prompt(&provider, 1);
    assert!(refine.contains("- `src/lexer.rs`"), "{refine}");
    assert!(!refine.contains("- `parser.rs`"), "{refine}");
}

/// A reference the refine pass keeps is named under the rewrite; verification never runs a
/// second refine pass.
#[tokio::test]
async fn a_reference_that_still_does_not_resolve_is_reported_under_the_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    for _ in 0..2 {
        provider.push_text_turn(&reply_citing("Run `no-such-tool-xyz --check` first.", ""));
    }
    let parent = parent_agent(&provider);
    let outcome = host(&provider, &parent, dir.path(), None)
        .run(&deepen("check it"), &AbortSignal::never(), &quiet())
        .await;
    assert_eq!(
        outcome,
        BetterOutcome::Rewritten {
            rewrite: format!(
                "Run `no-such-tool-xyz --check` first.\n\n{UNVERIFIED_LABEL} `no-such-tool-xyz --check`"
            ),
            unchanged: false,
        }
    );
    assert_eq!(provider.calls().len(), 2);
}

/// The budget grows only while references stay unresolved: a reply listing one earns a
/// grounding pass with more reads, and a reply listing none ends the run.
#[tokio::test]
async fn unresolved_references_earn_a_grounding_pass_and_resolved_ones_end_the_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lexer.rs"), "fn lex() {}\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn(&reply_citing("Fix the parser.", "the lexer source"));
    provider.push_tool_call_turn(
        None,
        vec![(
            "c1",
            pa_types::ai::PYTHON_TOOL_NAME,
            json!({ "code": "print(await fs.read('lexer.rs'))" }),
        )],
    );
    provider.push_text_turn(&reply_citing(
        "Fix the parser; the tokens come from `lexer.rs`.",
        "",
    ));
    let parent = parent_agent(&provider);
    let outcome = host(&provider, &parent, dir.path(), None)
        .run(&deepen("fix the parser"), &AbortSignal::never(), &quiet())
        .await;
    assert_eq!(
        outcome,
        BetterOutcome::Rewritten {
            rewrite: "Fix the parser; the tokens come from `lexer.rs`.".to_string(),
            unchanged: false,
        }
    );
    let grounding = last_prompt(&provider, 1);
    assert!(grounding.contains("the lexer source"), "{grounding}");
    assert!(
        grounding.contains(&format!("You have {GROWTH_STEP} more reads")),
        "{grounding}"
    );
    assert_eq!(provider.calls().len(), 3);
}
