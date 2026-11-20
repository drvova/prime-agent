use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::stream::StreamFn;
use pa_agent::types::{Message, UserContent};
use serde_json::{json, Value};

use super::*;
use crate::session::manager::SessionManager;
use crate::session_engine::rlm_usage::SessionUsageStore;

fn test_model() -> pa_agent::types::Model {
    serde_json::from_value(json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 100_000, "maxTokens": 8000
    }))
    .unwrap()
}

struct Fixture {
    provider: Arc<ScriptedProvider>,
    host: Arc<DecisionHost>,
    facts: Arc<Mutex<Vec<DecideFact>>>,
    _agent: Arc<Agent>,
    _dir: tempfile::TempDir,
}

/// A session whose decisions run on `stream_fn` (the scripted provider's when `None`), with
/// `timeout` per decision.
fn fixture(stream_fn: Option<StreamFn>, timeout: Duration) -> Fixture {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let agent = Arc::new(Agent::new(AgentOptions {
        session_id: Some("session-1".to_string()),
        initial_state: AgentInitialState {
            system_prompt: Some("main system prompt".to_string()),
            model: Some(test_model()),
            thinking_level: None,
            tools: Some(Vec::new()),
            messages: Some(Vec::new()),
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }));
    let dir = tempfile::TempDir::new().unwrap();
    let session = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
        dir.path(),
    )));
    let facts = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(DecisionHost::new(Some({
        let facts = Arc::clone(&facts);
        Arc::new(move |fact| facts.lock().unwrap().push(fact)) as DecideObserver
    })));
    let mut decider = Decider::new(
        Arc::downgrade(&agent),
        stream_fn.unwrap_or_else(|| provider.stream_fn()),
        Arc::new(SessionUsageStore(session)),
    );
    decider.timeout = timeout;
    host.bind(decider);
    Fixture {
        provider,
        host,
        facts,
        _agent: agent,
        _dir: dir,
    }
}

fn request(question: &str, cell: &str) -> Value {
    json!({
        "question": question,
        "options": ["stop", "retry with the wider fix"],
        "context": "test_parser failed: expected 3, got 2",
        "cell": cell,
    })
}

/// A cell's loop of ten decision points pays exactly one model call per decision up to the
/// bound: eight answers with their compact rows, then refusals that make no call; another cell
/// starts with its own budget.
#[tokio::test]
async fn a_cells_decisions_are_bounded_and_each_pays_one_call() {
    let fixture = fixture(None, DECIDE_TIMEOUT);
    for _ in 0..=DECISIONS_PER_CELL {
        fixture.provider.push_text_turn("2");
    }
    let mut replies = Vec::new();
    for attempt in 0..10 {
        let reply = fixture
            .host
            .answer(&request(
                &format!("attempt {attempt} failed; next?"),
                "cell-1",
            ))
            .await
            .map_err(|error| format!("{error:#}"));
        replies.push(reply);
    }
    let decided = |attempt: u32| {
        Ok(json!({
            "index": 1,
            "choice": "retry with the wider fix",
            "row": format!("[decide] attempt {attempt} failed; next? → retry with the wider fix"),
            "used": attempt + 1,
            "limit": DECISIONS_PER_CELL,
            "tokens": 0,
        }))
    };
    let refused = Err::<Value, String>(
        "rlm.decide: this cell used its 8 decisions; decide the rest in code or end the cell and decide in the next turn"
            .to_string(),
    );
    let mut expected: Vec<Result<Value, String>> = (0..8).map(decided).collect();
    expected.extend([refused.clone(), refused]);
    assert_eq!(replies, expected);
    assert_eq!(fixture.provider.calls().len(), 8);

    let other = fixture
        .host
        .answer(&request("a fresh cell; next?", "cell-2"))
        .await
        .unwrap();
    assert_eq!(other["used"], 1);
    assert_eq!(fixture.provider.calls().len(), 9);
    let mut facts = vec![DecideFact::Decided; 8];
    facts.extend([
        DecideFact::Limited,
        DecideFact::Limited,
        DecideFact::Decided,
    ]);
    assert_eq!(*fixture.facts.lock().unwrap(), facts);
}

/// A decision is one short standalone request: the fixed instruction, one user message with the
/// question, the context and the numbered options; no tools and no conversation.
#[tokio::test]
async fn a_decision_is_one_standalone_request() {
    let fixture = fixture(None, DECIDE_TIMEOUT);
    fixture.provider.push_text_turn("Option 1.");
    let reply = fixture
        .host
        .answer(&request("the suite still fails; next?", "cell-1"))
        .await
        .unwrap();
    assert_eq!(reply["choice"], "stop");
    let calls = fixture.provider.calls();
    let shapes: Vec<(Option<String>, Vec<String>, usize)> = calls
        .iter()
        .map(|call| {
            let texts = call
                .messages
                .iter()
                .map(|message| match message {
                    Message::User(user) => match &user.content {
                        UserContent::Text(text) => text.clone(),
                        UserContent::Parts(_) => "<parts>".to_string(),
                    },
                    Message::Assistant(_) => "<assistant>".to_string(),
                    Message::ToolResult(_) => "<tool result>".to_string(),
                })
                .collect();
            (call.system_prompt.clone(), texts, call.tools.len())
        })
        .collect();
    assert_eq!(
        shapes,
        vec![(
            Some(DECIDE_INSTRUCTION.to_string()),
            vec![
                "Question: the suite still fails; next?\n\nContext:\ntest_parser failed: expected 3, got 2\n\nOptions:\n1. stop\n2. retry with the wider fix\n\nReply with the number of one option."
                    .to_string()
            ],
            0,
        )]
    );
}

/// A provider that never answers fails the decision at its deadline instead of hanging the cell;
/// the failure counts, and so does the decision it used.
#[tokio::test]
async fn a_hanging_provider_fails_the_decision_at_its_deadline() {
    let hanging: StreamFn = Arc::new(|_model, _context, _options| {
        Box::pin(std::future::pending::<
            anyhow::Result<Box<dyn pa_agent::stream::ModelStream>>,
        >())
    });
    let fixture = fixture(Some(hanging), Duration::from_millis(50));
    let started = std::time::Instant::now();
    let error = fixture
        .host
        .answer(&request("next?", "cell-1"))
        .await
        .map_err(|error| format!("{error:#}"));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        error,
        Err("rlm.decide: the model did not answer within 50ms".to_string())
    );
    assert_eq!(*fixture.facts.lock().unwrap(), vec![DecideFact::Failed]);
}

/// The provider's error reaches the cell verbatim, and an answer that names no option is an
/// error, never a guess.
#[tokio::test]
async fn provider_errors_and_unparseable_answers_fail_raw() {
    let fixture = fixture(None, DECIDE_TIMEOUT);
    fixture
        .provider
        .push_stream_failure_turn("", "429 rate limited by upstream");
    fixture.provider.push_text_turn("I would rather not say.");
    let mut errors = Vec::new();
    for _ in 0..2 {
        errors.push(
            fixture
                .host
                .answer(&request("next?", "cell-1"))
                .await
                .map_err(|error| format!("{error:#}")),
        );
    }
    assert_eq!(
        errors,
        vec![
            Err("rlm.decide: 429 rate limited by upstream".to_string()),
            Err("rlm.decide: the model's answer \"I would rather not say.\" names none of the 2 options".to_string()),
        ]
    );
    assert_eq!(
        *fixture.facts.lock().unwrap(),
        vec![DecideFact::Failed, DecideFact::Failed]
    );
}

#[test]
fn answers_name_an_option_by_number_or_text() {
    let options = vec![
        "stop".to_string(),
        "retry".to_string(),
        "revert".to_string(),
    ];
    let parsed: Vec<Option<usize>> = [
        "2",
        " 3\n",
        "Option 1.",
        "`retry`",
        "REVERT",
        "7",
        "maybe",
        "0",
    ]
    .iter()
    .map(|answer| parse_choice(answer, &options))
    .collect();
    assert_eq!(
        parsed,
        vec![
            Some(1),
            Some(2),
            Some(0),
            Some(1),
            Some(2),
            None,
            None,
            None
        ]
    );
}

#[test]
fn invalid_requests_are_refused_before_any_call() {
    let errors: Vec<String> = [
        json!({"question": " ", "options": ["a", "b"]}),
        json!({"question": "q", "options": ["a"]}),
        json!({"question": "q", "options": ["a", "a"]}),
        json!({"question": "q", "options": ["a", 2]}),
        json!({"question": "q", "options": ["a", "b"], "context": 3}),
    ]
    .iter()
    .map(|data| format!("{:#}", parse_request(data).unwrap_err()))
    .collect();
    assert_eq!(
        errors,
        vec![
            "rlm.decide: question must be non-empty",
            "rlm.decide: options must hold 2 to 16 choices (got 1)",
            "rlm.decide: options must be distinct",
            "rlm.decide: every option must be a string",
            "rlm.decide: context must be a string, got 3",
        ]
    );
}
