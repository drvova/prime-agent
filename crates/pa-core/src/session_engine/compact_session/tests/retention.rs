//! The compaction retention verifier: judges a compaction by what a
//! continuation can still use, not by how short the summary is.
//!
//! Each fixture under `crates/pa-core/tests/fixtures/compaction_retention/`
//! is a session file plus an `.expect.json` naming the items a continuation
//! needs, by class (the user's constraints, decisions, files touched, open
//! tasks, unfixed failures, the goal), and held-out follow-up questions. Every
//! fixture is generated, and its `provenance` says so: the repository's real
//! captures (the ACP corpus in `crates/pa-daemon/testdata/acp/`) are two-turn
//! exchanges, too short to compact, and state no constraints.
//!
//! The deterministic mode compacts each fixture through the real
//! prepare/summarize/commit path with a summarizer that writes a placeholder,
//! so what survives is exactly what the harness guarantees whatever model
//! writes the summary. Per item it reports whether the summarizer was shown
//! it (the request text) and whether the post-compaction context carries it.
//! Run `make compaction-retention` for the table.
//!
//! The model-backed mode compacts with a real model and asks each fixture's
//! follow-up questions over the post-compaction context. It runs only when
//! `PRIME_AGENT_RETENTION_MODEL=<provider>/<model-id>` names a catalog model
//! whose provider key is in the environment, and says so when it skips.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::*;

const FIXTURES: [&str; 4] = [
    "synthetic-config-refactor",
    "synthetic-retry-debug",
    "synthetic-two-compactions",
    "synthetic-correction",
];

/// The placeholder the deterministic mode's summarizer writes: nothing a
/// continuation needs can come from it.
const PLACEHOLDER_SUMMARY: &str = "## Goal\n(placeholder summary)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
enum ItemClass {
    Constraint,
    Decision,
    File,
    OpenTask,
    Failure,
    Goal,
    /// A later user message that reverses an earlier constraint or
    /// decision (Factory's correction probe): `text` is the newer words,
    /// `supersedes` the stale ones they replace.
    Correction,
}

#[derive(Debug, serde::Deserialize)]
struct Item {
    class: ItemClass,
    text: String,
    /// The stale words a correction replaces (correction items only).
    #[serde(default)]
    supersedes: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FollowUp {
    question: String,
    expect_all: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Expect {
    provenance: String,
    keep_recent_tokens: u64,
    items: Vec<Item>,
    follow_ups: Vec<FollowUp>,
}

/// One fixture row: a session entry to append, or the verifier's marker to
/// compact at that point.
enum Step {
    Row(Box<FileEntry>),
    Compact,
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compaction_retention")
}

fn load_fixture(name: &str) -> (Vec<Step>, Expect) {
    let dir = fixture_dir();
    let rows = std::fs::read_to_string(dir.join(format!("{name}.jsonl"))).unwrap();
    let steps = rows
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            if value.get("verifier").and_then(serde_json::Value::as_str) == Some("compact") {
                Step::Compact
            } else {
                Step::Row(Box::new(serde_json::from_value(value).unwrap()))
            }
        })
        .collect();
    let expect = std::fs::read_to_string(dir.join(format!("{name}.expect.json"))).unwrap();
    (steps, serde_json::from_str(&expect).unwrap())
}

/// Whitespace-insensitive, case-sensitive containment: an item is kept when
/// its words survive in order.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The context after a compaction, as the provider receives it.
struct CompactedContext {
    /// The compaction summary message (the harness blocks ride it).
    summary: String,
    /// Every retained message after it.
    tail: String,
}

fn compacted_context(session: &SessionManager) -> CompactedContext {
    let provider = messages::convert_to_llm(&rebuilt_context_after_compaction(session));
    let (head, tail) = provider.split_first().expect("a compacted context");
    CompactedContext {
        summary: normalized(&compaction_utils::serialize_conversation(
            std::slice::from_ref(head),
        )),
        tail: normalized(&compaction_utils::serialize_conversation(tail)),
    }
}

/// The placeholder summarizer, recording every request text the harness sent.
fn placeholder_registration(
    requests: Arc<Mutex<Vec<String>>>,
) -> pa_ai::faux::FauxProviderRegistration {
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "retention-placeholder".to_string(),
                name: None,
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(200_000),
                max_tokens: Some(8_192),
            }]),
            ..Default::default()
        });
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Factory(Arc::new(
        move |context, _options, _call, _model| {
            let text = context
                .messages
                .iter()
                .filter_map(|message| match message {
                    pa_types::ai::Message::User(user) => Some(user.content.text()),
                    pa_types::ai::Message::Assistant(_) | pa_types::ai::Message::ToolResult(_) => {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            requests.lock().unwrap().push(text);
            Ok(pa_ai::faux::faux_assistant_text_message(
                PLACEHOLDER_SUMMARY,
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ))
        },
    ))]);
    registration.set_repeat_last_response(true);
    registration
}

/// Replay a fixture into a session, compacting at each marker and at the end.
async fn replay_and_compact(
    steps: Vec<Step>,
    keep_recent_tokens: u64,
    model: &pa_types::ai::Model,
    api_key: Option<&str>,
) -> SessionManager {
    let tmp = tempfile::tempdir().unwrap();
    let mut session = SessionManager::in_memory(tmp.path());
    let compact = |session: &mut SessionManager| {
        let options = CompactOptions {
            model: model.clone(),
            api_key: api_key.map(str::to_string),
            custom_instructions: None,
            settings: compaction::CompactionSettings {
                keep_recent_tokens,
                ..Default::default()
            },
            abort: None,
            harness_digest: None,
            auxiliary: None,
            summary_delta: None,
            semantic_edges: None,
        };
        // The session is moved in and out so the future owns no borrow
        // past the step.
        let mut owned = std::mem::replace(session, SessionManager::in_memory(tmp.path()));
        async move {
            let outcome = execute_compaction(&mut owned, options).await.unwrap();
            assert!(
                matches!(outcome, CompactOutcome::Ran(_)),
                "the fixture compacts: {outcome:?}"
            );
            owned
        }
    };
    for step in steps {
        match step {
            Step::Row(entry) => match *entry {
                FileEntry::Message { message, .. } => {
                    session.append_message(message).unwrap();
                }
                FileEntry::Custom { payload, .. } => {
                    session
                        .append_custom_entry(&payload.custom_type, payload.data)
                        .unwrap();
                }
                other => panic!("the fixtures hold messages and custom rows: {other:?}"),
            },
            Step::Compact => session = compact(&mut session).await,
        }
    }
    compact(&mut session).await
}

/// One fixture's deterministic result per class:
/// (items, items the summarizer was shown, items the context keeps).
type ClassCounts = BTreeMap<ItemClass, (usize, usize, usize)>;

/// One fixture's counts, and the compacted summary's size in tokens
/// (chars/4: the placeholder, the file lists and the carried blocks).
async fn deterministic_retention(name: &str) -> (ClassCounts, usize) {
    let (steps, expect) = load_fixture(name);
    assert_eq!(
        expect.provenance, "synthetic",
        "{name}: label the provenance"
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let registration = placeholder_registration(Arc::clone(&requests));
    let model = registration.get_model();
    let session = replay_and_compact(steps, expect.keep_recent_tokens, &model, None).await;
    registration.unregister();
    let context = compacted_context(&session);
    let shown = normalized(&requests.lock().unwrap().join("\n"));
    let mut counts = ClassCounts::new();
    for item in &expect.items {
        let text = normalized(&item.text);
        // An item the kept tail still holds would be kept by any summary:
        // the fixture must place every item before the cut.
        assert!(
            !context.tail.contains(&text),
            "{name}: item {:?} sits in the kept tail",
            item.text
        );
        let entry = counts.entry(item.class).or_default();
        entry.0 += 1;
        if let Some(stale) = item.supersedes.as_deref().map(normalized) {
            assert!(
                !context.tail.contains(&stale),
                "{name}: superseded words {stale:?} sit in the kept tail"
            );
            // The requests are chronological: the stale words may come
            // before their reversal (history), never after it.
            entry.1 += usize::from(
                shown
                    .find(&text)
                    .is_some_and(|at| !shown[at..].contains(&stale)),
            );
            // The compacted context lists what it carries newest
            // first: the newer words lead, the stale ones follow or
            // are gone.
            entry.2 += usize::from(context.summary.find(&text).is_some_and(|at| {
                context
                    .summary
                    .find(&stale)
                    .is_none_or(|stale_at| stale_at > at)
            }));
        } else {
            entry.1 += usize::from(shown.contains(&text));
            entry.2 += usize::from(context.summary.contains(&text));
        }
    }
    (counts, context.summary.chars().count() / 4)
}

/// The per-class retention table over every fixture (printed; run with
/// `--nocapture`). The totals are pinned: what the harness keeps of each
/// class whatever the model writes. Decisions are the summarizer's to keep;
/// a failure inside a cell that did not fail, and a goal that is only the
/// user's opening request, are not established by the harness. A
/// correction is shown when the stale words never follow the newer ones in
/// the requests, and kept when the newer words lead the compacted context.
#[tokio::test]
async fn compaction_retention_report() {
    let mut totals = ClassCounts::new();
    println!("compaction retention (deterministic: placeholder summary)");
    println!("fixture                      class       kept/items  shown-to-summarizer");
    for name in FIXTURES {
        let (counts, summary_tokens) = deterministic_retention(name).await;
        println!("{name:<28} summary     ~{summary_tokens} tokens");
        for (class, (items, shown, kept)) in counts {
            let class_name = format!("{class:?}");
            println!("{name:<28} {class_name:<11} {kept:>4}/{items:<6} {shown:>4}/{items}");
            let total = totals.entry(class).or_default();
            total.0 += items;
            total.1 += shown;
            total.2 += kept;
        }
    }
    for (class, (items, shown, kept)) in &totals {
        let class_name = format!("{class:?}");
        println!(
            "{:<28} {class_name:<11} {kept:>4}/{items:<6} {shown:>4}/{items}",
            "TOTAL"
        );
    }
    // (items, shown to the summarizer, kept in the compacted context)
    assert_eq!(
        totals,
        ClassCounts::from([
            (ItemClass::Constraint, (10, 10, 10)),
            (ItemClass::Decision, (5, 5, 0)),
            (ItemClass::File, (7, 7, 7)),
            (ItemClass::OpenTask, (5, 0, 5)),
            (ItemClass::Failure, (5, 4, 3)),
            (ItemClass::Goal, (2, 1, 1)),
            (ItemClass::Correction, (2, 2, 2)),
        ])
    );
}

/// The model-backed mode: compact with a real model, then ask the
/// held-out follow-ups over the compacted context.
#[tokio::test]
async fn compaction_retention_follow_ups_with_a_model() {
    let Ok(spec) = std::env::var("PRIME_AGENT_RETENTION_MODEL") else {
        println!(
            "skipped: the model-backed retention check needs PRIME_AGENT_RETENTION_MODEL=<provider>/<model-id> and that provider's API key in the environment"
        );
        return;
    };
    let (provider, model_id) = spec
        .split_once('/')
        .expect("PRIME_AGENT_RETENTION_MODEL is <provider>/<model-id>");
    let model = pa_ai::models_generated::get_model(provider, model_id)
        .unwrap_or_else(|| panic!("{spec} is not a catalog model"))
        .clone();
    let Some(api_key) = pa_ai::env_api_keys::get_env_api_key(provider) else {
        println!("skipped: no API key for provider {provider} in the environment");
        return;
    };
    println!("compaction retention (model-backed: {spec})");
    for name in FIXTURES {
        let (steps, expect) = load_fixture(name);
        let session =
            replay_and_compact(steps, expect.keep_recent_tokens, &model, Some(&api_key)).await;
        let provider_messages =
            messages::convert_to_llm(&rebuilt_context_after_compaction(&session));
        let context_text = compaction_utils::serialize_conversation(&provider_messages);
        let mut answered = 0;
        for follow_up in &expect.follow_ups {
            let prompt = format!(
                "<session>\n{context_text}\n</session>\n\nYou are the assistant in the session above, continuing its work. Answer from the session alone, in one or two sentences: {}",
                follow_up.question
            );
            let context = pa_types::ai::Context {
                system_prompt: None,
                messages: vec![pa_types::ai::Message::User(pa_types::ai::UserMessage {
                    content: UserContent::Text(prompt),
                    timestamp: 0,
                    rest: serde_json::Map::default(),
                })],
                tools: None,
            };
            let options =
                pa_ai::types::SimpleStreamOptions::from_base(pa_ai::types::StreamOptions {
                    api_key: Some(api_key.clone()),
                    max_tokens: Some(512),
                    ..Default::default()
                });
            let reply = pa_ai::complete_simple(&model, &context, Some(options))
                .await
                .unwrap();
            let answer = reply
                .content
                .iter()
                .filter_map(|block| match block {
                    pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.as_str()),
                    pa_types::ai::AssistantContentBlock::Thinking(_)
                    | pa_types::ai::AssistantContentBlock::ToolCall(_) => None,
                })
                .collect::<String>()
                .to_lowercase();
            let correct = follow_up
                .expect_all
                .iter()
                .all(|expected| answer.contains(&expected.to_lowercase()));
            answered += usize::from(correct);
            println!(
                "{name}: {} {:?} -> {answer:?}",
                if correct { "ok  " } else { "MISS" },
                follow_up.question
            );
        }
        println!(
            "{name}: {answered}/{} follow-ups answered",
            expect.follow_ups.len()
        );
    }
}
