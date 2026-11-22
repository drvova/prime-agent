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

fn request_bytes(messages: &[AgentMessage]) -> Vec<u8> {
    serde_json::to_vec(messages).expect("serializable view")
}

#[test]
fn below_the_threshold_the_view_is_unchanged() {
    let messages = history(20, 4_000);

    let view = thin_context(
        messages.clone(),
        1_000_000,
        &ThinningSettings::default(),
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

#[test]
fn above_the_threshold_old_big_results_thin_and_protected_ones_stay() {
    // 20 rounds: the newest 8 stay whole, and the boundary snaps to round 8.
    let mut messages = vec![user("fix the failing build")];
    for index in 0..20 {
        match index {
            // An edit failure no later edit superseded: the active fix.
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

    let view = thin_context(
        messages.clone(),
        20_000,
        &ThinningSettings::default(),
        &ThinnedRecall::ToolResult,
    );

    let mut expected = messages.clone();
    let mut chars_saved = 0;
    for index in [0usize, 1, 5, 6, 7] {
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
                thinned_results: 5,
                chars_saved,
            },
        }
    );
}

#[test]
fn the_prefix_holds_byte_identical_until_the_boundary_advances() {
    let settings = ThinningSettings::default();
    let mut previous: Option<Vec<AgentMessage>> = None;
    // 17..=23 rounds: thinnable 9..=15, so the boundary stays at round 8.
    for rounds in 17..=23 {
        let messages = history(rounds, 4_000);
        let view = thin_context(
            messages.clone(),
            20_000,
            &settings,
            &ThinnedRecall::ToolResult,
        );
        assert_eq!(
            view,
            thin_context(messages, 20_000, &settings, &ThinnedRecall::ToolResult),
            "the view is a pure function of the history"
        );
        assert_eq!(view.stats.thinned_results, 8);
        if let Some(previous) = previous {
            let prefix = request_bytes(&previous);
            let next = request_bytes(&view.messages[..previous.len()]);
            assert_eq!(next, prefix, "round {rounds} moved the cached prefix");
        }
        previous = Some(view.messages);
    }
    // Round 24 advances the boundary by a whole block: rounds 8..16 thin.
    let advanced = thin_context(
        history(24, 4_000),
        20_000,
        &settings,
        &ThinnedRecall::ToolResult,
    );
    let previous = previous.expect("prior view");
    assert_eq!(advanced.stats.thinned_results, 16);
    assert_eq!(advanced.messages[..18], previous[..18]);
    assert_ne!(advanced.messages[18], previous[18]);
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

    let view = thin_context(
        messages.clone(),
        20_000,
        &ThinningSettings::default(),
        &ThinnedRecall::ToolResult,
    );
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

/// The measured before/after: a synthesized 60-round session with realistic
/// cell sizes, replayed request by request under a 200k window. Prints the
/// per-turn request size; `--nocapture` shows the table.
#[test]
fn a_long_session_sends_fewer_request_bytes_per_turn() {
    let settings = ThinningSettings::default();
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
    // The last turn carries the whole history; thinning must cut it hard.
    let last_before = request_bytes(&messages).len();
    let last_after = request_bytes(
        &thin_context(messages, 200_000, &settings, &ThinnedRecall::ToolResult).messages,
    )
    .len();
    assert!(
        last_after * 3 < last_before,
        "thinning kept {last_after} of {last_before} bytes"
    );
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
        &ThinningSettings::default(),
        &ThinnedRecall::Spill(store.clone()),
    );

    let mut expected = messages.clone();
    let mut chars_saved = 0;
    let mut originals = Vec::new();
    for index in 0..8 {
        let at = 2 + 2 * index;
        let original = result_text(&messages[at]);
        let chars: Vec<char> = original.chars().collect();
        let head: String = chars[..240].iter().collect();
        let tail: String = chars[chars.len() - 120..].iter().collect();
        let thinned = format!(
            "{head}\n[out:{} · {} chars · {} lines · thinned from this request; ctx-page/ctx-grep read it whole]\n{tail}",
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
                thinned_results: 8,
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
        &ThinningSettings::default(),
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
        let view = thin_context(
            messages,
            20_000,
            &ThinningSettings::default(),
            &ThinnedRecall::Spill(store),
        );
        let _ = sender.send(view);
    });
    let view = receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("thinning must not wait on the spill write");
    assert_eq!(view, expected);
}
