//! Headless e2e for the tray model label's effort suffix (TS
//! `getModelContextLabel`): the tray renders `model:effort` while the
//! session's model supports reasoning, keeps the bare model id when not,
//! and the label follows a `/effort` switch live and a Shift+Tab thinking cycle. The slash
//! menu reads the same state: `/model` and `/effort` rows show it live, and `/effort `
//! lists the catalog model's levels with the active one current.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use std::os::unix::net::UnixListener;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use serde_json::{json, Value};

use support::{
    create_response, detach_response, options, run_headless, serve_connection, write_json, Request,
};

/// The session the mock serves: the `model` state block, the levels, and the live level (the value
/// `set_thinking_level` moves). `fail_state_after_switch` fails every state read AFTER a switch.
/// `catalog` is the `get_model_catalog` answer's model list.
struct MockSession {
    model: Value,
    levels: Vec<String>,
    level: String,
    fail_state_after_switch: bool,
    catalog: Vec<Value>,
}

/// A reasoning model at the default level (the engine's `medium`, TS `DEFAULT_THINKING_LEVEL`).
fn reasoning_session() -> MockSession {
    MockSession {
        model: json!({ "id": "glm-5.3", "name": "GLM 5.3", "provider": "zai", "reasoning": true }),
        levels: vec![
            "off".to_string(),
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ],
        level: "medium".to_string(),
        fail_state_after_switch: false,
        catalog: Vec::new(),
    }
}

/// A model without reasoning: the daemon clamps its level to "off".
fn plain_session() -> MockSession {
    MockSession {
        model: json!({
            "id": "gpt-4o-mini",
            "name": "GPT 4o mini",
            "provider": "openai",
            "reasoning": false
        }),
        levels: vec!["off".to_string()],
        level: "off".to_string(),
        fail_state_after_switch: false,
        catalog: Vec::new(),
    }
}

impl MockSession {
    fn bind(self, socket: &std::path::Path) -> MockSupervisor {
        MockSupervisor {
            listener: UnixListener::bind(socket).expect("bind mock socket"),
            session: self,
        }
    }
}

/// One mock supervisor serving a single attached session: `set_thinking_level` moves the live level
/// so the client's state re-reads observe the switch.
struct MockSupervisor {
    listener: UnixListener,
    session: MockSession,
}

impl MockSupervisor {
    fn serve(mut self) {
        let (stream, _) = self.listener.accept().expect("accept");
        // Armed by `set_thinking_level` when the session pins the failed-read fallback: every later
        // state read refuses.
        let mut state_reads_fail = false;
        serve_connection(stream, &[], |writer, Request { id, kind, command }| {
            match kind {
                "create" => {
                    write_json(writer, &create_response(id));
                }
                "attach" => {
                    write_json(writer, &self.attach_data(id));
                }
                "get_state" | "get_connection_state" => {
                    if state_reads_fail {
                        write_json(
                            writer,
                            &json!({
                                "type": "response",
                                "id": id,
                                "command": kind,
                                "success": false,
                                "error": "the state read failed",
                            }),
                        );
                    } else {
                        write_json(writer, &self.state_data(id, kind));
                    }
                }
                "set_thinking_level" => {
                    if let Some(level) = command.get("level").and_then(Value::as_str) {
                        self.session.level = level.to_string();
                    }
                    state_reads_fail = self.session.fail_state_after_switch;
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "set_thinking_level",
                            "success": true,
                            "data": null,
                        }),
                    );
                }
                // The daemon's cycle: the next supported level, wrapping; `null` without reasoning.
                "cycle_thinking_level" => {
                    let data = if self.session.model["reasoning"] == true {
                        let levels = &self.session.levels;
                        let next = levels
                            .iter()
                            .position(|level| *level == self.session.level)
                            .map_or(0, |index| (index + 1) % levels.len());
                        self.session.level.clone_from(&levels[next]);
                        json!({ "level": self.session.level })
                    } else {
                        Value::Null
                    };
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "cycle_thinking_level",
                            "success": true,
                            "data": data,
                        }),
                    );
                }
                "get_model_catalog" => {
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "get_model_catalog",
                            "success": true,
                            "data": { "models": self.session.catalog },
                        }),
                    );
                }
                "detach" => {
                    write_json(writer, &detach_response(id));
                }
                _ => return false,
            }
            true
        });
    }

    /// The slim attach result: one empty session whose state reports the model and its thinking
    /// level (the tray label's inputs).
    fn attach_data(&self, id: &str) -> Value {
        json!({
            "type": "response",
            "id": id,
            "command": "attach",
            "success": true,
            "data": {
                "protocol": { "name": "prime-agent.daemon", "version": 7 },
                "activeSessionId": "s1",
                "snapshot": {
                    "activeSessionId": "s1",
                    "summary": { "id": "s1", "cwd": "/tmp" },
                    "state": {
                        "activeSessionId": "s1",
                        "cwd": "/tmp",
                        "sessionId": "sess-1",
                        "sessionName": "effort session",
                        "model": self.session.model,
                        "thinkingLevel": self.session.level,
                        "availableThinkingLevels": self.session.levels,
                        "serviceTier": "auto",
                        "isStreaming": false,
                        "isCompacting": false,
                        "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                    },
                    "messages": [],
                    "lastEventSequence": 0,
                    "lastEventCursor": null,
                },
                "client": { "id": "mock", "capabilities": [] },
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
        })
    }

    /// The live session state (the `get_state`/`get_connection_state` data block): the model with
    /// its reasoning flag and the level `/effort` writes.
    fn state_data(&self, id: &str, command: &str) -> Value {
        json!({
            "type": "response",
            "id": id,
            "command": command,
            "success": true,
            "data": {
                "model": self.session.model,
                "thinkingLevel": self.session.level,
                "availableThinkingLevels": self.session.levels,
                "isStreaming": false,
                "isCompacting": false,
                "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
            },
        })
    }
}

fn run_plan(session: MockSession, steps: Vec<HeadlessStep>) -> Vec<String> {
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        },
        options,
        |socket| {
            let supervisor = session.bind(socket);
            move || supervisor.serve()
        },
    );
    outcome.frames
}

/// `/effort` moves the tray's `model:effort` label live: the attach state seeds
/// `glm-5.3:medium`, and the switch's level lands as `glm-5.3:high`.
#[test]
fn effort_command_moves_the_tray_model_label() {
    let steps = vec![
        HeadlessStep::Submit("/effort high".to_string()),
        HeadlessStep::WaitRender {
            needle: "glm-5.3:high".to_string(),
            timeout_ms: 10_000,
        },
        HeadlessStep::WaitMs(200),
    ];
    let frames = run_plan(reasoning_session(), steps);
    assert!(
        frames.iter().any(|frame| frame.contains("glm-5.3:medium")),
        "the attach state seeds the level suffix:\n{}",
        frames.join("\n---\n")
    );
    let switched = frames
        .iter()
        .rev()
        .find(|frame| frame.contains("Thinking level: high"))
        .expect("the switch's status note rendered");
    assert!(
        switched.contains("glm-5.3:high"),
        "the tray label follows the switch:\n{switched}"
    );
    assert!(
        !switched.contains("glm-5.3:medium"),
        "the pre-switch label is gone:\n{switched}"
    );
}

/// A successful `/effort` switch whose state re-read fails still moves the label: the requested
/// level renders (TS `applyThinkingLevel` patches the connection state), never the stale suffix.
#[test]
fn effort_switch_with_failed_state_read_never_keeps_the_stale_suffix() {
    let mut session = reasoning_session();
    session.fail_state_after_switch = true;
    let steps = vec![
        HeadlessStep::Submit("/effort high".to_string()),
        HeadlessStep::WaitRender {
            needle: "Thinking level: high".to_string(),
            timeout_ms: 10_000,
        },
        HeadlessStep::WaitMs(200),
    ];
    let frames = run_plan(session, steps);
    let switched = frames
        .iter()
        .rev()
        .find(|frame| frame.contains("Thinking level: high"))
        .expect("the switch's status note rendered");
    assert!(
        switched.contains("glm-5.3:high"),
        "the failed read falls back to the requested level:\n{switched}"
    );
    assert!(
        !switched.contains("glm-5.3:medium"),
        "the pre-switch suffix never survives a switch:\n{switched}"
    );
}

/// The state's level is "off", but the tray never carries a suffix.
#[test]
fn tray_label_stays_bare_without_reasoning() {
    let steps = vec![HeadlessStep::WaitRender {
        needle: "gpt-4o-mini".to_string(),
        timeout_ms: 10_000,
    }];
    let frames = run_plan(plain_session(), steps);
    let all = frames.join("\n---\n");
    assert!(
        frames.iter().any(|frame| frame.contains("gpt-4o-mini")),
        "the bare model id renders in the tray:\n{all}"
    );
    assert!(
        !all.contains("gpt-4o-mini:"),
        "a model without reasoning never carries an effort suffix:\n{all}"
    );
}

/// The reasoning session's catalog entry (the composition-root seed and the daemon's catalog
/// answer): the slash menu's `/effort` choices come from it, never a per-keystroke daemon read.
fn reasoning_catalog_entry() -> Value {
    json!({
        "id": "glm-5.3",
        "name": "GLM 5.3",
        "api": "openai-completions",
        "provider": "zai",
        "baseUrl": "https://api.z.ai/api/paas/v4",
        "reasoning": true,
        "input": ["text"],
        "cost": { "input": 1, "output": 3, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000,
        "maxTokens": 64_000
    })
}

/// Type `typed` into the attached reasoning session and return the frame the open menu renders,
/// each row's cells collapsed to single spaces.
fn menu_frame(typed: &str, needle: &str) -> Vec<String> {
    let steps = vec![
        HeadlessStep::WaitRender {
            needle: "glm-5.3:medium".to_string(),
            timeout_ms: 10_000,
        },
        HeadlessStep::Type(typed.to_string()),
        HeadlessStep::SettleIdle,
        HeadlessStep::WaitRender {
            needle: needle.to_string(),
            timeout_ms: 10_000,
        },
    ];
    let mut session = reasoning_session();
    session.catalog = vec![reasoning_catalog_entry()];
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        },
        |socket| InteractiveOptions {
            model_catalog: vec![
                serde_json::from_value(reasoning_catalog_entry()).expect("catalog model")
            ],
            ..options(socket)
        },
        |socket| {
            let supervisor = session.bind(socket);
            move || supervisor.serve()
        },
    );
    let frame = outcome
        .frames
        .iter()
        .rev()
        .find(|frame| frame.contains(needle))
        .unwrap_or_else(|| {
            panic!(
                "no frame shows {needle:?}:\n{}",
                outcome.frames.join("\n---\n")
            )
        });
    frame
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// The `/` menu's state-bearing rows read the session's live model and level instead of their
/// static descriptions.
#[test]
fn slash_menu_rows_show_the_live_model_and_effort() {
    let rows = menu_frame("/", "glm-5.3 \u{b7} medium");
    let first = rows
        .iter()
        .position(|row| row.starts_with("\u{203a} settings"))
        .expect("the menu opens on settings");
    assert_eq!(
        rows[first + 1..first + 3],
        [
            "model glm-5.3 \u{b7} medium \u{2502}",
            "effort medium \u{2502}"
        ]
    );
}

/// `/effort ` lists the levels the catalog model supports, the active one marked `(current)` and
/// preselected.
#[test]
fn effort_argument_choices_mark_and_preselect_the_current_level() {
    let rows = menu_frame("/effort ", "(current)");
    let first = rows
        .iter()
        .position(|row| row.starts_with("off "))
        .expect("the choices open");
    assert_eq!(
        rows[first..first + 5],
        [
            "off No reasoning",
            "minimal Very brief reasoning",
            "low Light reasoning",
            "\u{203a} medium Moderate reasoning (current)",
            "high Deep reasoning",
        ]
    );
}

/// Shift+Tab in the composer sends `cycle_thinking_level` and folds the answer silently: the
/// tray's glyph and suffix move from `medium` to `high`, then wrap from `high` to `off`, with no
/// status row (oh-my-pi's instant, toast-free change).
#[test]
fn shift_tab_cycles_the_thinking_level_into_the_tray() {
    let shift_tab = || HeadlessStep::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    let steps = vec![
        HeadlessStep::WaitRender {
            needle: "glm-5.3:medium".to_string(),
            timeout_ms: 10_000,
        },
        shift_tab(),
        HeadlessStep::WaitRender {
            needle: "\u{25cf} glm-5.3:high".to_string(),
            timeout_ms: 10_000,
        },
        shift_tab(),
        HeadlessStep::WaitRender {
            needle: "\u{25cb} glm-5.3:off".to_string(),
            timeout_ms: 10_000,
        },
    ];
    let frames = run_plan(reasoning_session(), steps);
    let all = frames.join("\n---\n");
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("\u{25cf} glm-5.3:high")),
        "the first press lands high with its glyph:\n{all}"
    );
    assert!(
        frames
            .last()
            .is_some_and(|frame| frame.contains("\u{25cb} glm-5.3:off")),
        "the second press wraps to off:\n{all}"
    );
    assert!(
        !all.contains("Thinking level:"),
        "the cycle is silent, no status row:\n{all}"
    );
}

/// A model without reasoning answers the cycle with `null`: the composer says so in the
/// `/effort` command's words, and the tray stays bare.
#[test]
fn shift_tab_without_reasoning_says_the_model_cannot_think() {
    let steps = vec![
        HeadlessStep::WaitRender {
            needle: "gpt-4o-mini".to_string(),
            timeout_ms: 10_000,
        },
        HeadlessStep::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
        HeadlessStep::WaitRender {
            needle: "Current model does not support thinking".to_string(),
            timeout_ms: 10_000,
        },
    ];
    let frames = run_plan(plain_session(), steps);
    let all = frames.join("\n---\n");
    assert!(
        frames
            .last()
            .is_some_and(|frame| frame.contains("Current model does not support thinking")),
        "the null answer leaves the unsupported note:\n{all}"
    );
    assert!(!all.contains("gpt-4o-mini:"), "the tray stays bare:\n{all}");
}
