//! Headless e2e for the empty composer's self-retiring key hint: each
//! Ctrl+L press saves the use count off the key path, and the saves land
//! one at a time and only upward, so a slow first save never overwrites
//! the later presses' counts (the hint would come back on the next launch).
#![cfg(unix)]

mod support;

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use serde_json::{json, Value};

use support::{options, run_headless, serve_connection, write_json, Request};

/// How long the stub store takes to save the first count.
const SLOW_SAVE_MS: u64 = 400;

/// The hint the empty composer shows until Ctrl+L has been used three times.
const HINT: &str = "Ctrl+L model";

struct MockSupervisor {
    listener: UnixListener,
}

impl MockSupervisor {
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            match kind {
                "create" => write_json(
                    writer,
                    &json!({
                        "type": "response",
                        "id": id,
                        "command": "create",
                        "success": true,
                        "data": {
                            "activeSessionId": "s1",
                            "id": "s1",
                            "sessionId": "sess-1",
                            "sessionFile": "/tmp/sess-1.jsonl",
                        },
                    }),
                ),
                "attach" => write_json(writer, &attach_data(id)),
                _ => return false,
            }
            true
        });
    }
}

/// The slim attach result: one empty, idle session.
fn attach_data(id: &str) -> Value {
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
                    "sessionName": "hint session",
                    "model": null,
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

/// A minimal settings seam: every getter returns its default; the hint count is stateful (the
/// settings file a fresh process re-reads), with a slow first save.
#[derive(Default)]
struct StubSettings {
    uses: Mutex<u64>,
}

impl StubSettings {
    /// The persisted count (what the settings file holds after the saves).
    fn stored(&self) -> u64 {
        *self.uses.lock().expect("hint count lock")
    }
}

impl pa_tui::client_settings::ClientSettings for StubSettings {
    fn compaction_threshold_percent(&self) -> f64 {
        95.0
    }
    fn set_compaction_threshold_percent(&self, _percent: f64) -> Result<()> {
        Ok(())
    }

    fn theme(&self) -> Option<String> {
        None
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
    fn set_theme(&self, _theme: &str) -> Result<()> {
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
    fn composer_hint_uses(&self) -> u64 {
        self.stored()
    }
    fn set_composer_hint_uses(&self, uses: u64) -> Result<()> {
        // The first save is the slow one: the later presses' saves are already queued behind
        // it when it lands, so an unordered writer lets it overwrite them.
        if uses == 1 {
            std::thread::sleep(std::time::Duration::from_millis(SLOW_SAVE_MS));
        }
        *self.uses.lock().expect("hint count lock") = uses;
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
    fn telemetry_status(&self) -> String {
        String::new()
    }
    fn set_telemetry_enabled(&self, _enabled: bool) -> Result<String> {
        Ok(String::new())
    }
    fn effective_update_channel(&self, version: &str) -> String {
        if version.contains("-beta") {
            "nightly".to_string()
        } else {
            "stable".to_string()
        }
    }
}

#[test]
fn the_hint_count_persists_in_order_and_never_goes_down() {
    let settings = Arc::new(StubSettings::default());
    let ctrl_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    let mut steps = vec![HeadlessStep::WaitRender {
        needle: HINT.to_string(),
        timeout_ms: 10_000,
    }];
    for _ in 0..3 {
        steps.push(HeadlessStep::Key(ctrl_l));
        steps.push(HeadlessStep::Key(esc));
    }
    steps.push(HeadlessStep::WaitGone {
        needle: HINT.to_string(),
        timeout_ms: 10_000,
    });
    let store = settings.clone();
    run_headless(
        HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        },
        |socket| InteractiveOptions {
            client_settings: Some(store),
            ..options(socket)
        },
        |socket| {
            let supervisor = MockSupervisor {
                listener: UnixListener::bind(socket).expect("bind mock socket"),
            };
            move || supervisor.serve()
        },
    );
    // The run's runtime finishes its blocking saves before it returns.
    assert_eq!(settings.stored(), 3);
}
