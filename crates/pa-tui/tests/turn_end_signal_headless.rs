//! Headless e2e for the turn-end signal: a run that ends, or a question that opens, while the
//! terminal reports itself unfocused queues the bell or the OSC 9 notification the
//! `terminal.notifyOnTurnEnd` setting names; a focused terminal, a terminal that never reported
//! its focus, and the `off` setting queue nothing.
#![cfg(unix)]

mod support;

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use anyhow::Result;
use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use serde_json::{json, Value};

use support::{
    attach_response, create_response, detach_response, options, run_headless, serve_connection,
    session_stats_response, write_json, MockSupervisor, Request,
};

impl MockSupervisor {
    /// Serve one connection: a prompt of `ask` opens a question mid-run and the run ends once it
    /// is answered; any other prompt runs one reply to its end.
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        serve_connection(stream, &[], |writer, Request { id, kind, command }| {
            match kind {
                "create" => write_json(writer, &create_response(id)),
                "attach" => write_json(writer, &attach_response(id, "signal session")),
                "get_session_stats" => write_json(writer, &session_stats_response(id)),
                "prompt" => {
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "prompt",
                            "success": true,
                        }),
                    );
                    stream_run(writer, command["message"] == "ask");
                }
                "answer_question" => {
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "answer_question",
                            "success": true,
                        }),
                    );
                    end_run(writer);
                }
                "detach" => write_json(writer, &detach_response(id)),
                _ => return false,
            }
            true
        });
    }
}

fn event(payload: &Value) -> Value {
    json!({ "type": "session_event", "activeSessionId": "s1", "event": payload })
}

fn end_run(writer: &mut UnixStream) {
    write_json(writer, &event(&json!({ "type": "turn_end" })));
    write_json(writer, &event(&json!({ "type": "agent_end" })));
}

/// One scripted run: a reply, then either the run's end or an open question.
fn stream_run(writer: &mut UnixStream, ask: bool) {
    write_json(writer, &event(&json!({ "type": "turn_start" })));
    write_json(
        writer,
        &event(&json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "reply" }],
                "stopReason": "stop",
                "timestamp": 1,
            },
        })),
    );
    if ask {
        write_json(
            writer,
            &event(&json!({
                "type": "question_open",
                "question": { "id": "q1", "text": "Ship it now?", "kind": "text" },
            })),
        );
        return;
    }
    end_run(writer);
}

/// The settings seam with one live value: `terminal.notifyOnTurnEnd`.
struct StubSettings {
    notify_on_turn_end: &'static str,
}

impl pa_tui::client_settings::ClientSettings for StubSettings {
    fn notify_on_turn_end(&self) -> String {
        self.notify_on_turn_end.to_string()
    }
    fn compaction_threshold_percent(&self) -> f64 {
        95.0
    }
    fn set_compaction_threshold_percent(&self, _percent: f64) -> Result<()> {
        Ok(())
    }
    fn theme(&self) -> Option<String> {
        None
    }
    fn set_theme(&self, _theme: &str) -> Result<()> {
        Ok(())
    }
    fn image_model(&self) -> Option<String> {
        None
    }
    fn default_service_tier(&self) -> String {
        "default".to_string()
    }
    fn set_default_service_tier(&self, _tier: &str) -> Result<()> {
        Ok(())
    }
    fn show_images(&self) -> bool {
        true
    }
    fn set_show_images(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn clear_on_shrink(&self) -> bool {
        false
    }
    fn set_clear_on_shrink(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn show_terminal_progress(&self) -> bool {
        false
    }
    fn set_show_terminal_progress(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn image_auto_resize(&self) -> bool {
        true
    }
    fn set_image_auto_resize(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn block_images(&self) -> bool {
        false
    }
    fn set_block_images(&self, _blocked: bool) -> Result<()> {
        Ok(())
    }
    fn enable_skill_commands(&self) -> bool {
        true
    }
    fn set_enable_skill_commands(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn enable_builtin_skills(&self) -> bool {
        true
    }
    fn set_enable_builtin_skills(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn show_hardware_cursor(&self) -> bool {
        false
    }
    fn set_show_hardware_cursor(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn editor_padding_x(&self) -> u64 {
        0
    }
    fn set_editor_padding_x(&self, _padding: u64) -> Result<()> {
        Ok(())
    }
    fn autocomplete_max_visible(&self) -> u64 {
        5
    }
    fn set_autocomplete_max_visible(&self, _max_visible: u64) -> Result<()> {
        Ok(())
    }
    fn quiet_startup(&self) -> bool {
        false
    }
    fn set_quiet_startup(&self, _quiet: bool) -> Result<()> {
        Ok(())
    }
    fn idle_eviction_minutes(&self) -> String {
        "90".to_string()
    }
    fn set_idle_eviction_minutes(&self, _value: &str) -> Result<()> {
        Ok(())
    }
    fn mermaid_rendering_mode(&self) -> String {
        "streaming".to_string()
    }
    fn set_mermaid_rendering_mode(&self, _mode: &str) -> Result<()> {
        Ok(())
    }
    fn tree_filter_mode(&self) -> String {
        "user-only".to_string()
    }
    fn set_tree_filter_mode(&self, _mode: &str) -> Result<()> {
        Ok(())
    }
    fn chat_detail(&self) -> String {
        "overview".to_string()
    }
    fn set_chat_detail(&self, _detail: &str) -> Result<()> {
        Ok(())
    }
    fn factory_enabled(&self) -> bool {
        false
    }
    fn set_factory_enabled(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn warnings_anthropic_extra_usage(&self) -> bool {
        true
    }
    fn set_warnings_anthropic_extra_usage(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn update_channel(&self) -> Option<String> {
        None
    }
    fn set_update_channel(&self, _channel: &str) -> Result<()> {
        Ok(())
    }
    fn effective_update_channel(&self, _version: &str) -> String {
        "stable".to_string()
    }
    fn telemetry_status(&self) -> String {
        String::new()
    }
    fn set_telemetry_enabled(&self, _enabled: bool) -> Result<String> {
        Ok(String::new())
    }
}

/// What the terminal said about its focus before the prompt was sent.
#[derive(Clone, Copy)]
enum Focus {
    NeverReported,
    Gained,
    Lost,
}

/// Run one prompt under the setting `signal`, hold until a frame shows `settled` (the run
/// summary, or the open question), play `then`, and return the attention signals the run queued.
fn signals(
    signal: &'static str,
    focus: Focus,
    prompt: &str,
    settled: &str,
    then: Vec<HeadlessStep>,
) -> Vec<String> {
    let mut steps = match focus {
        Focus::NeverReported => Vec::new(),
        Focus::Gained => vec![HeadlessStep::Focus(false), HeadlessStep::Focus(true)],
        Focus::Lost => vec![HeadlessStep::Focus(false)],
    };
    steps.push(HeadlessStep::Submit(prompt.to_string()));
    steps.push(HeadlessStep::WaitRender {
        needle: settled.to_string(),
        timeout_ms: 10_000,
    });
    steps.extend(then);
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width: 80,
            height: 24,
        },
        |socket| InteractiveOptions {
            client_settings: Some(Arc::new(StubSettings {
                notify_on_turn_end: signal,
            })),
            ..options(socket)
        },
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || supervisor.serve()
        },
    );
    outcome.attention_signals
}

/// A run that ends while the terminal is unfocused rings the bell or sends the notification,
/// per the setting; `off` sends nothing.
#[test]
fn a_run_ending_unfocused_signals_per_the_setting() {
    let by_setting = ["off", "bell", "notification"]
        .map(|signal| signals(signal, Focus::Lost, "go", "Worked for", Vec::new()));
    assert_eq!(
        by_setting,
        [
            Vec::<String>::new(),
            vec!["\u{7}".to_string()],
            vec!["\u{1b}]9;Prime Agent finished its turn\u{7}".to_string()],
        ]
    );
}

/// A question that opens while the terminal is unfocused signals too, with its own text; the
/// user comes back to answer it, so the run's end is not signalled.
#[test]
fn a_question_opening_unfocused_signals() {
    let answered = vec![
        HeadlessStep::Focus(true),
        HeadlessStep::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )),
        HeadlessStep::WaitRender {
            needle: "Worked for".to_string(),
            timeout_ms: 10_000,
        },
    ];
    assert_eq!(
        signals("notification", Focus::Lost, "ask", "Ship it now?", answered),
        vec!["\u{1b}]9;Prime Agent is waiting for your answer\u{7}".to_string()]
    );
}

/// A focused terminal is never signalled: one that regained its focus, and one that never
/// reported any (focus reports unsupported).
#[test]
fn a_focused_terminal_is_never_signalled() {
    let by_focus = [Focus::Gained, Focus::NeverReported]
        .map(|focus| signals("bell", focus, "go", "Worked for", Vec::new()));
    assert_eq!(by_focus, [Vec::<String>::new(), Vec::<String>::new()]);
}
