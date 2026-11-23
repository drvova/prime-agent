//! Headless e2e for the composer's recent-model key (`app.model.cycleRecent`, default F2): with
//! the session on `prime-inference/z-ai/glm-5.3` and the settings recent list naming the
//! openrouter route of the same id (and a model the catalog no longer lists), the key switches
//! the session through `set_model` to the openrouter route without opening the picker.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use pa_types::ai::Model;
use serde_json::{json, Value};

use support::{
    attach_response, create_response, detach_response, run_headless, serve_connection, write_json,
    Request,
};

fn model(provider: &str) -> Model {
    serde_json::from_value(json!({
        "id": "z-ai/glm-5.3", "name": format!("GLM 5.3 {provider}"), "api": "openai-completions",
        "provider": provider, "baseUrl": "https://example.invalid/v1", "reasoning": false,
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 128_000, "maxTokens": 4096,
    }))
    .expect("mock model deserializes")
}

fn state(id: &str, command: &str) -> Value {
    json!({
        "type": "response", "id": id, "command": command, "success": true,
        "data": {
            "model": { "id": "z-ai/glm-5.3", "provider": "prime-inference" },
            "isStreaming": false,
            "isCompacting": false,
            "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
        },
    })
}

/// Serve one attached session and record every `set_model` as `provider/modelId`.
fn serve(listener: &UnixListener, switches: &Arc<Mutex<Vec<String>>>) {
    let (stream, _) = listener.accept().expect("accept");
    serve_connection(stream, &[], |writer, Request { id, kind, command }| {
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => {
                let mut response = attach_response(id, "recent session");
                response["data"]["snapshot"]["state"]["model"] =
                    json!({ "id": "z-ai/glm-5.3", "provider": "prime-inference" });
                write_json(writer, &response);
            }
            "get_state" | "get_connection_state" => write_json(writer, &state(id, kind)),
            "set_model" => {
                switches.lock().unwrap().push(format!(
                    "{}/{}",
                    command["provider"].as_str().unwrap_or_default(),
                    command["modelId"].as_str().unwrap_or_default()
                ));
                write_json(
                    writer,
                    &json!({ "type": "response", "id": id, "command": "set_model", "success": true }),
                );
            }
            "detach" => write_json(writer, &detach_response(id)),
            _ => return false,
        }
        true
    });
}

/// F2 from the composer switches to the newest usable recent model: the one the catalog no
/// longer lists is skipped, the picker never opens, and the switch row renders.
#[test]
fn the_recent_key_switches_to_the_previous_model() {
    let switches = Arc::new(Mutex::new(Vec::new()));
    let recorded = switches.clone();
    let outcome = run_headless(
        HeadlessPlan {
            steps: vec![
                HeadlessStep::SettleIdle,
                HeadlessStep::Key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
                HeadlessStep::WaitRender {
                    needle: "Model: z-ai/glm-5.3".to_string(),
                    timeout_ms: 10_000,
                },
            ],
            width: 100,
            height: 30,
        },
        |socket| InteractiveOptions {
            model_catalog: vec![model("prime-inference"), model("openrouter")],
            model_configured_providers: ["prime-inference", "openrouter"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            model_recent_models: vec![
                "prime-inference/z-ai/glm-5.3".to_string(),
                "gone/retired-model".to_string(),
                "openrouter/z-ai/glm-5.3".to_string(),
            ],
            ..support::options(socket)
        },
        move |socket| {
            let listener = UnixListener::bind(socket).expect("bind mock socket");
            move || serve(&listener, &recorded)
        },
    );
    assert_eq!(
        *switches.lock().unwrap(),
        vec!["openrouter/z-ai/glm-5.3".to_string()]
    );
    let last = outcome.frames.last().expect("a frame");
    assert!(
        last.contains("Model: z-ai/glm-5.3") && !last.contains("Search models"),
        "the switch row renders and the picker never opened:\n{last}"
    );
}
