//! The request-view thinning battery: the pressure gate, the protected
//! results, the cache-stable boundary, the recall round trip, and a measured
//! before/after over a synthesized long session.
use super::*;
use crate::kernel::shared::HostRequestPayload;
use pa_agent::types::TextContent;

fn user(text: &str) -> AgentMessage {
    serde_json::from_value(json!({"role": "user", "content": text, "timestamp": 1}))
        .expect("user row")
}

fn assistant(text: Option<&str>, call: Option<&str>, tool: &str) -> AgentMessage {
    let mut content = Vec::new();
    if let Some(text) = text {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(call) = call {
        content.push(json!({
            "type": "toolCall",
            "id": call,
            "name": tool,
            "arguments": {"code": format!("run({call:?})")},
        }));
    }
    serde_json::from_value(json!({
        "role": "assistant",
        "content": content,
        "usage": {},
        "stopReason": if call.is_some() { "toolUse" } else { "stop" },
        "timestamp": 2,
    }))
    .expect("assistant row")
}

fn result(call: &str, tool: &str, text: &str, is_error: bool) -> AgentMessage {
    AgentMessage::Standard(Message::ToolResult(pa_agent::types::ToolResultMessage {
        tool_call_id: call.to_string(),
        tool_name: tool.to_string(),
        content: vec![ToolResultContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        details: None,
        is_error,
        timestamp: 3,
    }))
}

/// A realistic cell body of `chars` characters: numbered lines, as a test
/// run or a directory listing prints them.
fn cell_output(seed: usize, chars: usize) -> String {
    let mut out = String::new();
    let mut line = 0;
    while out.len() < chars {
        let _ = writeln!(out, "[{seed}] line {line}: value={}", line * 7 + seed);
        line += 1;
    }
    out.truncate(chars);
    out
}

/// One tool round: the call and its result.
fn round(index: usize, chars: usize) -> [AgentMessage; 2] {
    let call = format!("call-{index}");
    [
        assistant(None, Some(&call), "python_repl"),
        result(&call, "python_repl", &cell_output(index, chars), false),
    ]
}

fn history(rounds: usize, chars: usize) -> Vec<AgentMessage> {
    let mut messages = vec![user("fix the failing build")];
    for index in 0..rounds {
        messages.extend(round(index, chars));
    }
    messages
}

/// The thinned text the transform writes for `text` (the pointer format is
/// part of the model contract, so it is pinned literally here).
fn thinned_text(text: &str, call: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let head: String = chars[..240].iter().collect();
    let tail: String = chars[chars.len() - 120..].iter().collect();
    format!(
        "{head}\n[... {} of {} chars of this older result thinned from the request; full text: (rlm-session-tool-result \"{call}\") ...]\n{tail}",
        chars.len() - 360,
        chars.len(),
    )
}

fn with_text(message: &AgentMessage, text: String) -> AgentMessage {
    let AgentMessage::Standard(Message::ToolResult(result)) = message else {
        panic!("not a tool result");
    };
    let mut result = result.clone();
    result.content = vec![ToolResultContent::Text(TextContent {
        text,
        text_signature: None,
    })];
    AgentMessage::Standard(Message::ToolResult(result))
}

fn result_text(message: &AgentMessage) -> String {
    let AgentMessage::Standard(Message::ToolResult(result)) = message else {
        panic!("not a tool result");
    };
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.clone()),
            ToolResultContent::Image(_) => None,
        })
        .collect()
}

/// Thinning switched on, at its default threshold and protections.
fn on() -> ThinningSettings {
    ThinningSettings {
        enabled: true,
        ..ThinningSettings::default()
    }
}

/// The 0-based indexes of the requests that changed bytes inside the
/// request before them (the prompt-cache prefix breaks).
fn prefix_breaks(requests: &[Vec<Message>]) -> Vec<usize> {
    let prefixes: Vec<_> = requests
        .iter()
        .map(|messages| {
            crate::session_engine::cache_prefix::RequestPrefix::of(&pa_agent::stream::LlmContext {
                system_prompt: None,
                messages: messages.clone(),
                tools: Vec::new(),
            })
        })
        .collect();
    (1..prefixes.len())
        .filter(|at| prefixes[at - 1].broken_bytes(&prefixes[*at]).is_some())
        .collect()
}

fn request_bytes(messages: &[AgentMessage]) -> Vec<u8> {
    serde_json::to_vec(messages).expect("serializable view")
}

#[test]
fn below_the_threshold_the_view_is_unchanged() {
    let messages = history(20, 4_000);

    let view = thin_context(
        messages.clone(),
        1_000_000,
        &on(),
        &ThinnedRecall::ToolResult,
    );

    assert_eq!(
        view,
        ThinnedView {
            messages,
            stats: ThinningStats::default(),
        }
    );
}

/// At the crossing every round but the newest 8 thins, whatever a later row
/// says about it: an error result and a result the latest reply names thin
/// like the rest (exempting them would rewrite them once that row changes).
#[test]
fn at_a_crossing_every_round_but_the_newest_thins() {
    // 20 rounds of ~4k chars in a 20k-token window: the view first reaches
    // 65% (52k chars) after 14 rounds (rounds 0..6 thin), again after 19
    // (rounds 6..11 thin), and not after 20.
    let mut messages = vec![user("fix the failing build")];
    for index in 0..20 {
        match index {
            // An edit failure no later edit superseded.
            2 => messages.extend([
                assistant(None, Some("call-2"), "edit"),
                result("call-2", "edit", &cell_output(2, 4_000), true),
            ]),
            // Small results never thin.
            4 => messages.extend(round(index, 300)),
            _ => messages.extend(round(index, 4_000)),
        }
    }
    // The latest assistant text names round 3's result.
    messages.push(assistant(
        Some("the traceback in call-3 points at the parser"),
        None,
        "python_repl",
    ));

    let view = thin_context(messages.clone(), 20_000, &on(), &ThinnedRecall::ToolResult);

    let mut expected = messages.clone();
    let mut chars_saved = 0;
    for index in [0usize, 1, 2, 3, 5, 6, 7, 8, 9, 10] {
        let at = 2 + 2 * index;
        let original = result_text(&messages[at]);
        let thinned = thinned_text(&original, &format!("call-{index}"));
        chars_saved += (original.chars().count() - thinned.chars().count()) as u64;
        expected[at] = with_text(&messages[at], thinned);
    }
    assert_eq!(
        view,
        ThinnedView {
            messages: expected,
            stats: ThinningStats {
                thinned_results: 10,
                chars_saved,
            },
        }
    );
}

/// The prompt cache's view of a long thinning session: request by request,
/// the 0-based indexes of the requests that changed bytes the request
/// before them had sent. They are exactly the crossings, where the view
/// reached the threshold; every other request only appends. (A boundary
/// that stepped every 8 rounds and an exemption for the result the latest
/// reply names rewrote the prefix of nearly every request past the first
/// crossing.)
#[test]
fn a_long_session_rewrites_already_sent_results_only_at_a_crossing() {
    let sizes = [1_800, 9_000, 400, 24_000, 3_500, 65_536, 700, 12_000];
    let mut messages = vec![user("port the parser and make the suite pass")];
    let mut requests = Vec::new();
    // The requests whose view thins more results than the one before.
    let (mut crossings, mut thinned) = (Vec::new(), 0);
    for index in 0..120usize {
        let call = format!("call-{index}");
        // Each reply names a result from 20 rounds back.
        let text = format!("as call-{} showed, next", index.saturating_sub(20));
        messages.push(assistant(Some(&text), Some(&call), "eval"));
        messages.push(result(
            &call,
            "eval",
            &cell_output(index, sizes[index % sizes.len()]),
            false,
        ));
        let view = thin_context(messages.clone(), 200_000, &on(), &ThinnedRecall::ToolResult);
        if view.stats.thinned_results > thinned {
            crossings.push(index);
        }
        thinned = view.stats.thinned_results;
        requests.push(
            view.messages
                .into_iter()
                .filter_map(|message| match message {
                    AgentMessage::Standard(message) => Some(message),
                    AgentMessage::Custom(_) => None,
                })
                .collect::<Vec<_>>(),
        );
    }

    assert_eq!(
        (prefix_breaks(&requests), crossings),
        (vec![37, 67, 93, 117], vec![37, 67, 93, 117])
    );
}

#[tokio::test]
async fn the_pointer_resolves_to_the_full_result_through_the_recall_path() {
    let messages = history(17, 4_000);
    let session = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
        std::path::Path::new("/tmp"),
    )));
    for message in &messages {
        let stored: pa_types::session::AgentMessage =
            serde_json::from_value(serde_json::to_value(message).expect("wire row"))
                .expect("session row");
        session.lock().await.append_message(stored).expect("append");
    }
    let mut handlers = HostRequestHandlers::default();
    register_tool_result_recall(&mut handlers, &session);
    let recall = handlers.get("rlm.session.tool_result").expect("registered");

    let view = thin_context(messages.clone(), 20_000, &on(), &ThinnedRecall::ToolResult);
    let pointer = result_text(&view.messages[2]);
    let call = pointer
        .split("(rlm-session-tool-result \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("pointer names a tool call id");
    let reply = recall(HostRequestPayload {
        data: json!({"toolCallId": call}),
    })
    .await
    .expect("recall answers");

    assert_eq!(
        reply,
        json!({
            "found": true,
            "toolName": "python_repl",
            "isError": false,
            "text": result_text(&messages[2]),
        })
    );
}

/// The engine wires the transform: a resumed session under pressure sends
/// the provider a thinned old result, while the stored row stays whole.
#[tokio::test]
async fn the_engine_sends_the_thinned_view_and_keeps_the_transcript_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let model: pa_agent::types::Model = serde_json::from_value(json!({
        "id": "m", "name": "m", "api": "test", "provider": "test", "baseUrl": "http://localhost",
        "reasoning": false, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 20_000, "maxTokens": 100
    }))
    .unwrap();
    let messages = history(17, 4_000);
    let mut manager = SessionManager::in_memory(tmp.path());
    for message in &messages {
        manager
            .append_message(serde_json::from_value(serde_json::to_value(message).unwrap()).unwrap())
            .unwrap();
    }
    std::fs::create_dir_all(tmp.path().join("agent")).unwrap();
    std::fs::write(
        tmp.path().join("agent").join("settings.json"),
        r#"{"contextThinning": {"enabled": true}}"#,
    )
    .unwrap();
    let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(model.clone()));
    provider.push_text_turn("done");
    let engine = super::super::engine::create_session(super::super::engine::SessionEngineConfig {
        cwd: tmp.path().to_path_buf(),
        agent_dir: tmp.path().join("agent"),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(manager),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();

    engine
        .session
        .prompt("continue", super::super::PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    let sent = provider.calls()[0]
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "call-0" => Some(result.clone()),
            _ => None,
        })
        .expect("the request carries call-0");
    let original = result_text(&messages[2]);
    assert_eq!(
        sent.content,
        vec![ToolResultContent::text(thinned_text(&original, "call-0"))]
    );
    let stored = engine
        .session
        .agent()
        .state()
        .await
        .messages
        .into_iter()
        .find(|message| {
            matches!(message, AgentMessage::Standard(Message::ToolResult(result)) if result.tool_call_id == "call-0")
        })
        .expect("the live context keeps call-0");
    assert_eq!(result_text(&stored), original);
}

/// With the default settings a session's requests only append, past the
/// point thinning used to start (65% of the window), and compaction is the
/// one request that rewrites what the request before it sent.
#[tokio::test]
async fn with_the_defaults_only_compaction_rewrites_the_request_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let model: pa_agent::types::Model = serde_json::from_value(json!({
        "id": "m", "name": "m", "api": "test", "provider": "test", "baseUrl": "http://localhost",
        "reasoning": false, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 20_000, "maxTokens": 100
    }))
    .unwrap();
    // 17 rounds of ~4k chars: past 65% of the 20k-token window.
    let mut manager = SessionManager::in_memory(tmp.path());
    for message in &history(17, 4_000) {
        manager
            .append_message(serde_json::from_value(serde_json::to_value(message).unwrap()).unwrap())
            .unwrap();
    }
    let provider = Arc::new(pa_agent::scripted::ScriptedProvider::new(model.clone()));
    // Eight more tool rounds (a tool no session has: each answers with an
    // error at once), then the reply: requests 0..=8.
    for index in 0..8 {
        provider.push_tool_call_turn(
            None,
            vec![(&format!("more-{index}"), "absent_tool", json!({}))],
        );
    }
    provider.push_text_turn("done");
    // After the compaction, one more request: request 9.
    provider.push_text_turn("after");
    let engine = super::super::engine::create_session(super::super::engine::SessionEngineConfig {
        cwd: tmp.path().to_path_buf(),
        agent_dir: tmp.path().join("agent"),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(manager),
        daemonless: true,
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .session
        .prompt("continue", super::super::PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    // The summarizer is its own provider (its request is no loop request).
    let summarizer =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            api: Some(format!("faux-thinning-{:?}", std::time::SystemTime::now())),
            provider: Some("faux-thinning".to_string()),
            ..Default::default()
        });
    summarizer.set_responses(vec![
        pa_ai::faux::FauxResponseStep::Message(
            pa_ai::faux::faux_assistant_text_message(
                "## Goal\nport the parser",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            )
        );
        3
    ]);
    engine
        .session
        .set_compaction_settings(super::super::compaction::CompactionSettings {
            keep_recent_tokens: 2_000,
            ..Default::default()
        });
    let outcome = engine
        .session
        .compact(None, &summarizer.get_model(), None, None)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            super::super::compact_session::CompactOutcome::Ran(_)
        ),
        "the compaction ran"
    );
    engine
        .session
        .prompt("go on", super::super::PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    let requests: Vec<Vec<Message>> = provider
        .calls()
        .into_iter()
        .map(|context| context.messages)
        .collect();
    assert_eq!((requests.len(), prefix_breaks(&requests)), (10, vec![9]));
}

/// The measured before/after: a synthesized 60-round session with realistic
/// cell sizes, replayed request by request under a 200k window. Prints the
/// per-turn request size; `--nocapture` shows the table.
#[test]
fn a_long_session_sends_fewer_request_bytes_per_turn() {
    let settings = on();
    // Cell sizes cycle like a working session: listings, test runs, a few
    // capped dumps, and short probes.
    let sizes = [1_800, 9_000, 400, 24_000, 3_500, 65_536, 700, 12_000];
    let mut messages = vec![user("port the parser and make the suite pass")];
    let (mut before_total, mut after_total) = (0usize, 0usize);
    // The first turn whose request crosses the 95% compaction trigger
    // (chars/4 of the serialized request, the estimator's heuristic).
    let (mut compacts_before, mut compacts_after) = (None, None);
    println!("turn  before_bytes  after_bytes  est_tokens_saved");
    for index in 0..60 {
        let call = format!("call-{index}");
        messages.push(assistant(None, Some(&call), "python_repl"));
        messages.push(result(
            &call,
            "python_repl",
            &cell_output(index, sizes[index % sizes.len()]),
            false,
        ));
        let before = request_bytes(&messages).len();
        let view = thin_context(
            messages.clone(),
            200_000,
            &settings,
            &ThinnedRecall::ToolResult,
        );
        let after = request_bytes(&view.messages).len();
        before_total += before;
        after_total += after;
        if compacts_before.is_none() && before / 4 > 190_000 {
            compacts_before = Some(index + 1);
        }
        if compacts_after.is_none() && after / 4 > 190_000 {
            compacts_after = Some(index + 1);
        }
        if index % 5 == 4 {
            println!(
                "{:>4}  {before:>12}  {after:>11}  {:>16}",
                index + 1,
                view.stats.tokens_saved()
            );
        }
    }
    println!("total {before_total:>12}  {after_total:>11}");
    println!(
        "compaction trigger crossed at turn: before {compacts_before:?}, after {compacts_after:?}"
    );
    // The whole history reaches the compaction trigger; the thinned view
    // never does.
    assert_eq!((compacts_before, compacts_after), (Some(50), None));
    assert!(after_total < before_total);
}

/// A persistent session's thinned results carry their spill handle instead
/// of the transcript pointer, and each handle reads the whole result back:
/// thinning loses nothing.
#[tokio::test]
async fn with_a_spill_store_thinned_results_page_out_behind_their_handle() {
    use crate::kernel::spill::{content_id, SpillLookup, SpillStore};

    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = SpillStore::for_session_dir(dir.path());
    let messages = history(20, 4_000);

    let view = thin_context(
        messages.clone(),
        20_000,
        &on(),
        &ThinnedRecall::Spill(store.clone()),
    );

    let mut expected = messages.clone();
    let mut chars_saved = 0;
    let mut originals = Vec::new();
    for index in 0..10 {
        let at = 2 + 2 * index;
        let original = result_text(&messages[at]);
        let chars: Vec<char> = original.chars().collect();
        // The result's surprise-weighted preview (pinned by the preview's own
        // tests), within the same 360-char budget, then its handle.
        let preview = crate::kernel::spill::preview::build(
            &original,
            360,
            &crate::kernel::spill::noise::NoiseSet::default(),
        )
        .expect("a 4,000-char result previews")
        .text;
        let thinned = format!(
            "{preview}\n[out:{} · {} chars · {} lines · thinned from this request; ctx-page/ctx-grep read it whole]",
            content_id(&original),
            crate::kernel::spill::group_digits(chars.len()),
            original.lines().count(),
        );
        chars_saved += (chars.len() - thinned.chars().count()) as u64;
        expected[at] = with_text(&messages[at], thinned);
        originals.push(original);
    }
    assert_eq!(
        view,
        ThinnedView {
            messages: expected,
            stats: ThinningStats {
                thinned_results: 10,
                chars_saved,
            },
        }
    );
    let mut paged_back = Vec::new();
    for original in &originals {
        match store.lookup(&content_id(original)).await {
            SpillLookup::Stored(spill) => paged_back.push(spill.text),
            other => panic!("not stored: {other:?}"),
        }
    }
    assert_eq!(paged_back, originals);
}

/// The request path never waits on the spill write: with the store's writer
/// hung on its first file, thinning returns at once, and its view is the one
/// a working store gives (the handle is the content hash, not a store state).
#[cfg(unix)]
#[test]
fn a_hanging_spill_writer_never_delays_the_thinned_view() {
    use crate::kernel::spill::{content_id, SpillStore};

    let messages = history(20, 4_000);
    let working = tempfile::TempDir::new().expect("temp dir");
    let expected = thin_context(
        messages.clone(),
        20_000,
        &on(),
        &ThinnedRecall::Spill(SpillStore::for_session_dir(working.path())),
    );
    let hung = tempfile::TempDir::new().expect("temp dir");
    let spill_dir = hung.path().join("spill");
    std::fs::create_dir_all(&spill_dir).expect("spill dir");
    // Opening a FIFO for writing blocks until a reader opens it: the writer
    // thread's temp-file create for the first thinned result never returns.
    let fifo = spill_dir.join(format!("{}.tmp", content_id(&result_text(&messages[2]))));
    let status = pa_types::session_scope::std_command("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(status.success());
    let store = SpillStore::for_session_dir(hung.path());

    // On its own thread, so a thinning that waited on the write fails this
    // test instead of hanging it.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let view = thin_context(messages, 20_000, &on(), &ThinnedRecall::Spill(store));
        let _ = sender.send(view);
    });
    let view = receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("thinning must not wait on the spill write");
    assert_eq!(view, expected);
}
