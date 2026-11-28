//! An unsuccessful threshold auto-compaction on the runtime daemon (the
//! supervisor's `compaction_outcome_e2e`): its durable `compaction_outcome`
//! row is kept once and streamed before the settled `compaction_end` that
//! carries the same failure message, and it never reaches a provider
//! request. A summarizer that answers a blank but billed reply fails the
//! same way as one that is unavailable: no compaction row is written and
//! the transcript keeps both turns.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::openai_mock::{model, small_usage, Answer, OpenAiMock, LOOPBACK};
use super::turns::Attached;
use super::Bed;

/// The fixed summarization prompt's marker, in a summarizer request.
const SUMMARIZER_MARKER: &str = "context summarization assistant";

/// How the provider answers a summarizer request.
#[derive(Clone, Copy)]
enum Summarizer {
    Answers,
    Unavailable,
    Blank,
}

fn is_summarizer_request(body: &Value) -> bool {
    body["messages"].to_string().contains(SUMMARIZER_MARKER)
}

/// A session on a provider whose second request bills a context over the
/// threshold, so the turn it answers fires an auto-compaction.
struct Run {
    session: Attached,
    provider: OpenAiMock,
    summarizer: Arc<Mutex<Summarizer>>,
    _daemon: super::Daemon,
    _bed: Bed,
}

/// Seeds a turn, then sends the crossing turn with the summarizer answering
/// as `mode`, and waits for the compaction to settle.
async fn failed_auto_compaction(mode: Summarizer) -> Run {
    let bed = Bed::new();
    let summarizer = Arc::new(Mutex::new(Summarizer::Answers));
    let answering = Arc::clone(&summarizer);
    let provider = OpenAiMock::start(move |index, body| {
        let usage = if index == 1 {
            json!({ "prompt_tokens": 126_000, "completion_tokens": 10, "total_tokens": 126_010,
                    "prompt_tokens_details": { "cached_tokens": 80 } })
        } else {
            small_usage()
        };
        let content = if is_summarizer_request(body) {
            match *answering.lock().unwrap() {
                Summarizer::Answers => "parity reply",
                Summarizer::Unavailable => return Answer::Unavailable,
                Summarizer::Blank => "",
            }
        } else {
            "parity reply"
        };
        Answer::Reply {
            content: content.to_string(),
            usage,
        }
    });
    std::fs::write(
        bed.agent_dir().join("models.json"),
        json!({ "providers": { "prime-inference": {
            "api": "openai-completions", "baseUrl": provider.url(), "apiKey": "sk-battery",
            "models": [model("mock-1", "Mock 1")],
        } } })
        .to_string(),
    )
    .unwrap();
    // A tiny reserve: the 126,010 tokens the crossing turn bills are over the
    // 128k window's threshold.
    std::fs::write(
        bed.agent_dir().join("settings.json"),
        json!({ "compaction": { "enabled": true, "reserveTokens": 500, "keepRecentTokens": 10 } })
            .to_string(),
    )
    .unwrap();
    let daemon = bed.runtime_daemon_with(&json!({ "responses": [] }), LOOPBACK);
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut session = Attached::open(&bed, &cwd).await;
    session
        .call(
            "session.set_model",
            json!({ "provider": "prime-inference", "modelId": "mock-1" }),
        )
        .await;
    let seed = session.publish("seed turn", None).await;
    session.done(&seed["task"]).await;
    *summarizer.lock().unwrap() = mode;
    let crossing = session.publish("crossing turn", None).await;
    session.done(&crossing["task"]).await;
    session.until_streamed("compaction_end").await;
    Run {
        session,
        provider,
        summarizer,
        _daemon: daemon,
        _bed: bed,
    }
}

/// What every failure shows: the outcome row's start and end streamed in
/// order before the settled `compaction_end` with the same message, and
/// one durable row with it. The failure message and the transcript's rows.
async fn unsuccessful_compaction(run: &mut Run) -> (String, Vec<Value>) {
    let stream = run.session.payloads("stream");
    let position = |matches: &dyn Fn(&Value) -> bool| stream.iter().position(matches);
    let outcome = |kind: &'static str| {
        move |event: &Value| {
            event["type"] == kind && event["message"]["customType"] == "compaction_outcome"
        }
    };
    let row_start = position(&outcome("message_start")).expect("the outcome row's message_start");
    let row_end = position(&outcome("message_end")).expect("the outcome row's message_end");
    let end_at = position(&|event: &Value| {
        event["type"] == "compaction_end" && event["errorMessage"].is_string()
    })
    .expect("the failed compaction_end");
    let row = stream[row_start]["message"].clone();
    let message = row["content"].as_str().unwrap_or_default().to_string();
    let end = &stream[end_at];
    let rows: Vec<Value> = run
        .session
        .query_with("transcript.page", json!({ "limit": 500 }))
        .await["entries"]
        .as_array()
        .expect("a page of entries")
        .clone();
    let durable: Vec<Value> = rows
        .iter()
        .filter(|row| row["customType"] == "compaction_outcome")
        .map(|row| {
            json!({ "type": row["type"], "content": row["content"],
                    "display": row["display"], "details": row["details"] })
        })
        .collect();
    let details = json!({ "reason": "threshold", "outcome": "failed" });
    assert_eq!(
        (
            row_start < row_end && row_end < end_at,
            message.starts_with("Auto-compaction failed:"),
            (
                row["role"].clone(),
                row["display"].clone(),
                row["details"].clone()
            ),
            (
                end["reason"].clone(),
                end["errorMessage"].clone(),
                end.get("errorSeverity").cloned(),
                end["aborted"].clone(),
                end["willRetry"].clone(),
            ),
            durable,
        ),
        (
            true,
            true,
            (json!("custom"), json!(true), details.clone()),
            (
                json!("threshold"),
                json!(message),
                None,
                json!(false),
                json!(false)
            ),
            vec![json!({ "type": "custom_message", "content": message,
                         "display": true, "details": details })],
        ),
        "the failed compaction's stream: {stream:?}"
    );
    (message, rows)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_auto_compaction_keeps_its_outcome_row_out_of_the_next_request() {
    let mut run = failed_auto_compaction(Summarizer::Unavailable).await;
    let (message, _) = unsuccessful_compaction(&mut run).await;

    // The next turn's compaction succeeds, and its request never carries
    // the disclosure (the cacheable prefix stays the same).
    *run.summarizer.lock().unwrap() = Summarizer::Answers;
    let before = run.provider.bodies().len();
    let next = run.session.publish("next turn", None).await;
    run.session.done(&next["task"]).await;
    let request = run.provider.bodies()[before..]
        .iter()
        .find(|body| !is_summarizer_request(body))
        .expect("the next turn reached the provider")
        .to_string();
    assert_eq!(
        (
            request.contains("compaction_outcome"),
            request.contains(&message)
        ),
        (false, false),
        "the disclosure never reaches the provider: {request}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blank_summary_fails_the_compaction_and_keeps_the_transcript() {
    let mut run = failed_auto_compaction(Summarizer::Blank).await;
    let (_, rows) = unsuccessful_compaction(&mut run).await;
    let messages = rows
        .iter()
        .filter(|row| row["type"] == "message")
        .map(Value::to_string)
        .collect::<String>();
    assert_eq!(
        (
            rows.iter().any(|row| row["type"] == "compaction"),
            ["seed turn", "crossing turn", "parity reply"].map(|text| messages.contains(text)),
        ),
        (false, [true; 3]),
        "a blank summary is never committed and the turns stay: {rows:?}"
    );
}
