//! Headless verifier for the `/model` picker's paint path: the daemon never answers the catalog
//! refresh (`get_model_catalog`) or a usage fetch (`get_usage`), and the picker still paints at
//! once from the startup catalog snapshot — its rows, columns, and chip row — because the open
//! path only spawns the refresh and reads the cached quota cards (AGENTS.md "Performance and the
//! critical path"). An await on either source would hold the frame past the bound.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
use pa_types::ai::Model;
use serde_json::{json, Value};

use support::{
    attach_response, create_response, detach_response, run_headless, serve_connection, write_json,
    Request,
};

/// The startup snapshot: one metered model with catalog rates.
fn catalog() -> Vec<Model> {
    vec![serde_json::from_value(json!({
        "id": "meter-1", "name": "Meter One", "api": "openai-completions", "provider": "meterco",
        "baseUrl": "https://meter.example.invalid/v1", "reasoning": false,
        "input": ["text", "image"],
        "cost": { "input": 0.5, "output": 1.5, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000, "maxTokens": 4096,
    }))
    .expect("mock model deserializes")]
}

/// The mock supervisor: the session attaches and answers its state, but the catalog refresh and
/// any usage fetch never get a reply.
fn serve(listener: &UnixListener) {
    let (stream, _) = listener.accept().expect("accept");
    serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => {
                let mut response = attach_response(id, "hanging sources");
                response["data"]["snapshot"]["state"]["model"] =
                    json!({ "id": "meter-1", "provider": "meterco" });
                write_json(writer, &response);
            }
            "get_state" | "get_connection_state" => write_json(writer, &state(id, kind)),
            // The hanging sources: handled, never answered.
            "get_model_catalog" | "get_usage" => {}
            "detach" => write_json(writer, &detach_response(id)),
            _ => return false,
        }
        true
    });
}

fn state(id: &str, command: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": command,
        "success": true,
        "data": {
            "model": { "id": "meter-1", "provider": "meterco" },
            "isStreaming": false,
            "isCompacting": false,
            "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
        },
    })
}

fn options(socket: PathBuf) -> InteractiveOptions {
    InteractiveOptions {
        model_catalog: catalog(),
        model_configured_providers: ["meterco".to_string()].into_iter().collect(),
        ..support::options(socket)
    }
}

#[test]
fn the_picker_paints_while_the_catalog_and_quota_sources_hang() {
    let outcome = run_headless(
        HeadlessPlan {
            steps: vec![
                HeadlessStep::Submit("/model".to_string()),
                HeadlessStep::WaitRender {
                    needle: "Meter One".to_string(),
                    timeout_ms: 5_000,
                },
            ],
            width: 100,
            height: 30,
        },
        options,
        |socket| {
            let listener = UnixListener::bind(socket).expect("bind mock socket");
            move || serve(&listener)
        },
    );
    let last = outcome.frames.last().expect("a frame with the picker open");
    let row = last
        .split('\n')
        .find(|row| row.contains("Meter One"))
        .expect("the snapshot's row renders");
    assert!(
        row.contains("200k") && row.contains("$0.5/$1.5"),
        "the row's columns paint from the snapshot alone: {row}"
    );
    assert!(last.contains("billing: any"), "the chip row paints: {last}");
}
