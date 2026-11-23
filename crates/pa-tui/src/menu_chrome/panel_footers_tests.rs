//! The footer grammar across the panels: every picker, pane and prompt closes with the ONE hint
//! row (`key verb` hints joined by ` · ` at the chrome indent), and every list names its arrows
//! the same way. A panel that hand-rolls a footer, or words the same key differently, fails here.

use super::CHROME_INDENT;
use crate::keybindings::KeybindingsManager;
use crate::theme::{ColorMode, Theme};
use crate::Line;
use pa_types::question::{Question, QuestionKind};
use serde_json::json;

/// Wide enough that no panel drops a hint: the rows below are each panel's whole footer.
const WIDTH: usize = 200;

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

/// The footer of a rendered frame: its last row that carries text.
fn footer(frame: &[Line]) -> String {
    frame
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .rfind(|row| !row.trim().is_empty() && !row.starts_with('\u{2500}'))
        .unwrap_or_default()
}

fn question(kind: QuestionKind) -> Question {
    Question {
        id: "q".to_string(),
        text: "Which store?".to_string(),
        kind,
        choices: vec!["sqlite".to_string(), "redis".to_string()],
        default: Vec::new(),
        timeout_seconds: None,
        allow_other: false,
        expires_at_ms: None,
    }
}

/// Each panel's footer over the default bindings.
fn footers() -> Vec<(&'static str, String)> {
    let kb = KeybindingsManager::new();
    let theme = theme();
    let now = std::time::SystemTime::UNIX_EPOCH;
    let levels = ["low".to_string(), "high".to_string()];
    let tree = json!({
        "flatNodes": [{"entry": {
            "type": "message", "id": "n0", "parentId": null,
            "timestamp": "2024-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": "hello", "timestamp": 0},
        }}],
        "leafId": "n0",
    });
    vec![
        (
            "confirm",
            footer(&crate::confirm::ConfirmPanel::yes_no("Import", "Replace?").render(
                &theme, WIDTH, &kb,
            )),
        ),
        (
            "effort",
            footer(
                &crate::choice_picker::ChoicePicker::effort(&levels, Some("low"))
                    .render(&theme, WIDTH, &kb),
            ),
        ),
        (
            "fork",
            footer(
                &crate::user_message_selector::UserMessageSelector::new(vec![
                    crate::user_message_selector::UserMessageItem {
                        id: "e".to_string(),
                        text: "hello".to_string(),
                    },
                ])
                .render(&theme, WIDTH, &kb),
            ),
        ),
        (
            "history",
            footer(
                &crate::history_search::HistorySearch::new(&[crate::editor::HistoryEntry {
                    text: "hello".to_string(),
                    at: now,
                }])
                .render(&theme, WIDTH, &kb, now),
            ),
        ),
        (
            "question (one choice)",
            footer(
                &crate::question_prompt::QuestionPrompt::new(question(QuestionKind::Single))
                    .render(&theme, WIDTH, &kb, 0),
            ),
        ),
        (
            "question (several)",
            footer(
                &crate::question_prompt::QuestionPrompt::new(question(QuestionKind::Multi))
                    .render(&theme, WIDTH, &kb, 0),
            ),
        ),
        (
            "bash",
            footer(
                &crate::bash_view::BashView::new(
                    crate::bash_view::parse_bash_activities(&json!({"activities": [
                        {"id": "a", "command": "cargo build", "status": "running"},
                    ]})),
                    24,
                )
                .render(&theme, WIDTH, &kb),
            ),
        ),
        (
            "heartbeats",
            footer(
                &crate::heartbeats_picker::HeartbeatsPicker::new(
                    crate::heartbeats_picker::parse_heartbeats(&json!({"heartbeats": [{
                        "job": {
                            "id": "h", "status": "active", "source": "heartbeat",
                            "activeSessionId": "live", "sessionId": "s", "prompt": "tick",
                            "schedule": {"kind": "interval", "expression": "every 10m", "intervalMs": 600_000},
                        },
                    }]})),
                    None,
                    None,
                    24,
                )
                .render(&theme, WIDTH, &kb),
            ),
        ),
        (
            "mcp",
            footer(
                &crate::mcp_view::McpView::from_response(
                    &json!({"connections": [], "services": [{
                        "serviceId": "notion", "label": "Notion",
                        "connectionStatus": "connected", "connectable": false,
                        "usesOAuth": true, "source": "catalog",
                        "connectionIds": ["notion"], "pasteToken": false,
                    }]}),
                    19,
                )
                .render(&theme, WIDTH, &kb),
            ),
        ),
        (
            "tree",
            footer(
                &crate::tree_selector::TreeSelector::new(
                    &tree,
                    24,
                    /*skip_summarize_prompt*/ false,
                    crate::tree_list::FilterMode::Default,
                )
                .expect("a selector over one node")
                .render(&theme, WIDTH, &kb),
            ),
        ),
    ]
}

/// Every footer is the shared row: the chrome indent, then `key verb` hints joined by ` · `,
/// with the list arrows reading `↑/↓ navigate` on every panel that has them.
#[test]
fn every_panel_footer_is_the_shared_hint_row() {
    let footers = footers();
    for (panel, row) in &footers {
        let hints = row
            .strip_prefix(CHROME_INDENT)
            .unwrap_or_else(|| panic!("{panel}: the footer starts at the chrome indent: {row:?}"));
        for hint in hints.split(" \u{b7} ") {
            let (key, verb) = hint
                .split_once(' ')
                .unwrap_or_else(|| panic!("{panel}: {hint:?} is not `key verb` in {row:?}"));
            assert!(
                !key.is_empty() && !verb.is_empty() && !hint.ends_with(' ') && !key.contains(','),
                "{panel}: {hint:?} is not `key verb` in {row:?}"
            );
            if key == "\u{2191}/\u{2193}" {
                assert_eq!(verb, "navigate", "{panel}: the list arrows in {row:?}");
            }
        }
    }
    assert_eq!(
        footers
            .iter()
            .filter(|(_, row)| row.contains("\u{2191}/\u{2193} navigate"))
            .count(),
        footers.len(),
        "every panel here is a list: {footers:?}"
    );
}
