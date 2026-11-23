use super::*;

fn user(text: &str) -> AgentMessage {
    AgentMessage::User(pa_types::ai::UserMessage {
        content: pa_types::ai::UserContent::Text(text.to_string()),
        timestamp: 0,
        rest: serde_json::Map::default(),
    })
}

fn cell_call(id: &str, code: &str) -> AgentMessage {
    let mut arguments = serde_json::Map::new();
    arguments.insert("code".to_string(), serde_json::json!(code));
    AgentMessage::Assistant(pa_types::ai::AssistantMessage {
        content: vec![pa_types::ai::AssistantContentBlock::ToolCall(
            pa_types::ai::ToolCall {
                id: id.to_string(),
                name: "eval".to_string(),
                arguments,
                thought_signature: None,
                rest: serde_json::Map::default(),
            },
        )],
        api: "openai-completions".to_string(),
        provider: "p".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_types::ai::Usage::default(),
        stop_reason: pa_types::ai::StopReason::ToolUse,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        rest: serde_json::Map::default(),
    })
}

fn cell_result(id: &str, error: Option<&str>) -> AgentMessage {
    AgentMessage::ToolResult(pa_types::ai::ToolResultMessage {
        tool_call_id: id.to_string(),
        tool_name: "eval".to_string(),
        content: vec![],
        details: error.map(|evalue| {
            serde_json::json!({"status": "error", "error": {"ename": "SchemeError", "evalue": evalue}})
        }),
        is_error: error.is_some(),
        timestamp: 0,
        rest: serde_json::Map::default(),
    })
}

fn bang(command: &str, output: &str, exit_code: i64) -> AgentMessage {
    AgentMessage::BashExecution(pa_types::session::BashExecutionMessage {
        command: command.to_string(),
        output: output.to_string(),
        exit_code: Some(exit_code),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        timestamp: 0,
        exclude_from_context: None,
    })
}

fn todo(id: &str, text: &str, status: TodoStatus) -> TodoItem {
    TodoItem {
        id: id.to_string(),
        text: text.to_string(),
        status,
        note: None,
    }
}

fn whole(text: &str) -> CarriedUserMessage {
    CarriedUserMessage {
        text: text.to_string(),
        form: CarriedText::Whole,
    }
}

/// The carried state puts the span's user messages, whole and newest first,
/// before the previous compaction's (a repeat keeps its newest place, an
/// older row's sentences carry on as excerpts, an expanded skill or file is
/// dropped), keeps only the active goal and the todo items not done, and
/// keeps the newest uncleared failure with the lines that name it.
#[test]
fn carried_state_is_merged_from_the_previous_state_the_span_and_the_live_work() {
    let previous = CarriedState {
        user_messages: vec![whole("Only edit crates/net.")],
        user_constraints: vec![
            "Never run `cargo update`.".to_string(),
            "Prefer small commits.".to_string(),
        ],
        goal: Some("an old goal".to_string()),
        open_todos: vec![todo("9", "an old todo", TodoStatus::Pending)],
        last_failure: Some(CarriedFailure {
            command: "(run-old)".to_string(),
            error: "old".to_string(),
        }),
    };
    let span = vec![
        user("Fix the retry loop. Don't add dependencies!\n- You must keep 3 attempts.\nShould we keep it?"),
        user("never run `cargo update`"),
        user("<skill name=\"lint\" location=\"/s/lint.md\">\nAlways lint first.\n</skill>\n<file name=\"a.md\">\nYou must not read this.\n</file>\nRun the lint skill."),
        cell_call("c1", "(run-old)"),
        cell_result("c1", None),
        bang(
            "cargo test -p net",
            "running 2 tests\ntest a ... ok\nthread 'b' panicked at src/lib.rs:3:5:\nassertion failed: x\n\nwarning: noise\ntest result: FAILED. 1 passed; 1 failed",
            101,
        ),
        cell_call("c2", "(fs-edit \"a.rs\" \"x\" \"y\")"),
        cell_result("c2", Some("fs-edit: old text not found in a.rs")),
        bang("cargo test -p net", "test result: ok", 0),
    ];
    let live = LiveWork {
        goal: Some(pa_types::goal::GoalState {
            active: true,
            objective: Some("Fix the zero-delay retry".to_string()),
            ..pa_types::goal::empty_goal_state()
        }),
        todos: Some(TodoList {
            items: vec![
                todo("1", "Reproduce", TodoStatus::Done),
                todo("2", "Make the test pass", TodoStatus::InProgress),
                todo("3", "Add a tracing span", TodoStatus::Pending),
            ],
        }),
    };
    assert_eq!(
        CarriedState::carry(Some(&previous), &span, live, 200_000),
        CarriedState {
            user_messages: vec![
                whole("Run the lint skill."),
                whole("never run `cargo update`"),
                whole("Fix the retry loop. Don't add dependencies!\n- You must keep 3 attempts.\nShould we keep it?"),
                whole("Only edit crates/net."),
                CarriedUserMessage {
                    text: "Prefer small commits.".to_string(),
                    form: CarriedText::Excerpt,
                },
            ],
            user_constraints: Vec::new(),
            goal: Some("Fix the zero-delay retry".to_string()),
            open_todos: vec![
                todo("2", "Make the test pass", TodoStatus::InProgress),
                todo("3", "Add a tracing span", TodoStatus::Pending),
            ],
            last_failure: Some(CarriedFailure {
                command: "(fs-edit \"a.rs\" \"x\" \"y\")".to_string(),
                error: "fs-edit: old text not found in a.rs".to_string(),
            }),
        }
    );
    // The failing test run is the newest failure until its command passes.
    assert_eq!(
        CarriedState::carry(None, &span[..6], LiveWork::default(), 200_000).last_failure,
        Some(CarriedFailure {
            command: "cargo test -p net".to_string(),
            error: "thread 'b' panicked at src/lib.rs:3:5:\nassertion failed: x\ntest result: FAILED. 1 passed; 1 failed".to_string(),
        })
    );
}

/// A tenth of a 1,000-token window carries 100 tokens (400 characters) of
/// user messages, newest first: a message larger than all of it keeps only
/// its instruction sentences (none: it is left out), and the first older
/// message that does not fit what is left ends the walk.
#[test]
fn carried_user_messages_keep_the_newest_within_the_window_budget() {
    let oversized = format!(
        "{}Never touch the lockfile. Thanks.",
        "Some context. ".repeat(40)
    );
    let span = vec![
        user(&"o".repeat(60)),
        user("Use tabs in this repo."),
        user(&oversized),
        user(&"z".repeat(300)),
        user(&"q".repeat(500)),
    ];
    assert_eq!(
        CarriedState::carry(None, &span, LiveWork::default(), 1_000).user_messages,
        vec![
            whole(&"z".repeat(300)),
            CarriedUserMessage {
                text: "Never touch the lockfile.".to_string(),
                form: CarriedText::Excerpt,
            },
            whole("Use tabs in this repo."),
        ]
    );
}
