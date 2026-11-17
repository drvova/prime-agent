//! Headless e2e for the live thought: while the model thinks, its newest closed sentence shows
//! as one row under the loader (nc's thought under the live bar); once the turn ends only the
//! folded `• Thought` row stays in the transcript.
#![cfg(unix)]

mod support;

use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{json, Value};

use support::{
    accept_within_idle_window, attach_response, create_response, options, run_headless,
    serve_connection, write_json, write_session_event, MockSupervisor, Request,
};

/// The thinking text as it stands at each streamed frame.
const THINKING: [&str; 3] = [
    "Look at the parser first",
    "Look at the parser first. It owns three jobs",
    "Look at the parser first. It owns three jobs.\n\nSplit the tokenizer out. Then the res",
];

fn assistant(thinking: &str, text: Option<&str>) -> Value {
    let mut content = vec![json!({ "type": "thinking", "thinking": thinking })];
    content.extend(text.map(|text| json!({ "type": "text", "text": text })));
    json!({ "role": "assistant", "content": content })
}

fn serve(supervisor: &MockSupervisor) {
    let Some(stream) = accept_within_idle_window(&supervisor.listener) else {
        return;
    };
    serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
        match kind {
            "create" => write_json(writer, &create_response(id)),
            "attach" => {
                let mut response = attach_response(id, "thinking session");
                response["data"]["snapshot"]["state"]["isStreaming"] = json!(true);
                write_json(writer, &response);
                let mut events = writer.try_clone().expect("clone event socket");
                std::thread::spawn(move || {
                    let pause = || std::thread::sleep(std::time::Duration::from_millis(400));
                    pause();
                    write_session_event(&mut events, &json!({ "type": "agent_start" }));
                    for thinking in THINKING {
                        write_session_event(
                            &mut events,
                            &json!({
                                "type": "message_update",
                                "message": assistant(thinking, None),
                                "assistantMessageEvent": { "type": "thinking_delta" },
                            }),
                        );
                        pause();
                    }
                    let settled = assistant(THINKING[2], Some("Split it in three."));
                    write_session_event(
                        &mut events,
                        &json!({ "type": "message_end", "message": settled }),
                    );
                    for event_type in ["turn_end", "agent_end"] {
                        write_session_event(&mut events, &json!({ "type": event_type }));
                    }
                });
            }
            _ => return false,
        }
        true
    });
}

#[test]
fn the_latest_thought_shows_under_the_loader_until_the_turn_ends() {
    let plan = HeadlessPlan {
        steps: vec![HeadlessStep::WaitMs(2400)],
        width: 100,
        height: 30,
    };
    let outcome = run_headless(plan, options, |socket| {
        let supervisor = MockSupervisor::bind(socket);
        move || serve(&supervisor)
    });

    // The row under the `Thinking` loader line in each frame that paints one.
    let thoughts: Vec<String> = outcome
        .frames
        .iter()
        .filter_map(|frame| {
            let lines: Vec<&str> = frame.lines().collect();
            let loader = lines.iter().position(|line| line.contains("Thinking"))?;
            Some(lines.get(loader + 1)?.trim().to_string()).filter(|row| !row.is_empty())
        })
        .fold(Vec::new(), |mut seen, thought| {
            if seen.last() != Some(&thought) {
                seen.push(thought);
            }
            seen
        });
    assert_eq!(
        thoughts,
        vec![
            "Look at the parser first".to_string(),
            "Look at the parser first.".to_string(),
            "Split the tokenizer out.".to_string(),
        ]
    );
    let last = outcome.frames.last().expect("a frame painted");
    assert!(
        last.lines()
            .any(|line| line.trim().starts_with("\u{2022} Thought")),
        "the folded thought row stays:\n{last}"
    );
    assert!(!last.contains("Split the tokenizer out."), "{last}");
}
