use std::fmt::Write as _;

use super::*;
use crate::event_stream::initial_assistant_message;
use crate::types::{AssistantContent, AssistantMessage, DoneStopReason};

fn partial() -> AssistantMessage {
    let mut message = initial_assistant_message("faux", "faux", "faux-1");
    message.timestamp = 0;
    message
}

fn text_delta(delta: &str) -> AssistantMessageEvent {
    AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: delta.to_string(),
        partial: partial(),
    }
}

fn thinking_delta(delta: &str) -> AssistantMessageEvent {
    AssistantMessageEvent::ThinkingDelta {
        content_index: 0,
        delta: delta.to_string(),
        partial: partial(),
    }
}

fn done() -> AssistantMessageEvent {
    AssistantMessageEvent::Done {
        reason: DoneStopReason::Stop,
        message: partial(),
    }
}

/// What a guard at `level` says once `text` streams through it as answer text, in token-sized
/// deltas, and the stream ends.
fn verdict(level: LoopGuardLevel, text: &str) -> Option<String> {
    let mut guard = StreamLoopGuard::new(level).expect("a watching level");
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = rest.len().min(7);
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        let (delta, tail) = rest.split_at(cut);
        if let Some(hit) = guard.observe(&text_delta(delta)) {
            return Some(hit);
        }
        rest = tail;
    }
    guard.observe(&done())
}

#[test]
fn an_exact_cycle_of_whole_lines_is_a_loop_past_a_kilobyte() {
    // 30 copies of one 41-byte line cover more than 1 KB; 20 stay under the long floor, as a
    // summarizer restating its status line does.
    let line = "Still checking the parser configuration.\n";
    assert_eq!(
        verdict(LoopGuardLevel::Exact, &line.repeat(30)),
        Some("the answer repeated a 41-byte passage 25 times in a row".to_string())
    );
    assert_eq!(verdict(LoopGuardLevel::Exact, &line.repeat(20)), None);
}

#[test]
fn a_short_stutter_needs_four_repeats_and_180_bytes() {
    let stutter = |times: usize| format!("I think {}", "so ".repeat(times));
    assert_eq!(
        verdict(LoopGuardLevel::Exact, &stutter(60)),
        Some("the answer repeated a 3-byte passage 60 times in a row".to_string())
    );
    assert_eq!(verdict(LoopGuardLevel::Exact, &stutter(58)), None);
}

#[test]
fn thinking_is_watched_until_the_answer_starts() {
    let line = "Let me verify the configuration loader once more.\n";
    let mut guard = StreamLoopGuard::new(LoopGuardLevel::Exact).unwrap();
    let hit = line
        .repeat(30)
        .as_bytes()
        .chunks(16)
        .find_map(|chunk| guard.observe(&thinking_delta(std::str::from_utf8(chunk).unwrap())));
    assert_eq!(
        hit,
        Some("the thinking repeated a 50-byte passage 23 times in a row".to_string())
    );
    // Once the answer starts, later thinking is not judged.
    let mut answered = StreamLoopGuard::new(LoopGuardLevel::Exact).unwrap();
    assert_eq!(answered.observe(&text_delta("Done.")), None);
    let after =
        line.repeat(30).as_bytes().chunks(16).find_map(|chunk| {
            answered.observe(&thinking_delta(std::str::from_utf8(chunk).unwrap()))
        });
    assert_eq!(after, None);
}

fn drifting_paragraph(i: usize) -> String {
    let verb = ["verify", "confirm", "double-check", "validate", "recheck"][i % 5];
    format!(
        "Pass {i}. I need to {verb} that the configuration loader handles the empty case correctly before I continue with the remaining checks, because a dropped default section would make every later step read stale values.\n\n"
    )
}

#[test]
fn near_duplicate_paragraphs_loop_at_the_semantic_level_only() {
    let text: String = (1..=12).map(drifting_paragraph).collect();
    assert_eq!(
        verdict(LoopGuardLevel::Semantic, &text),
        Some("the answer wrote 8 near-identical paragraphs within its last 16".to_string())
    );
    assert_eq!(verdict(LoopGuardLevel::Exact, &text), None);
}

/// Long, repetitive-looking output that is real work: none of it trips either level.
#[test]
fn benign_repetitive_output_never_trips() {
    let mut table =
        String::from("| id | name | status | owner |\n|----|------|--------|-------|\n");
    for i in 0..400 {
        let _ = writeln!(table, "| {i} | worker-{i} | ok | team-{} |", i % 7);
    }
    let mut code = String::from("```rust\n");
    for i in 0..400 {
        let _ = writeln!(
            code,
            "    registry.insert(\"key_{i}\", Entry {{ index: {i}, weight: {} }});",
            i * 3
        );
    }
    code.push_str("```\n");
    let mut list = String::new();
    for i in 1..=400 {
        let _ = writeln!(
            list,
            "{i}. Update the handler for route number {i} and rerun its test."
        );
    }
    // A long hex dump of different content: an LCG keeps the bytes varied.
    let mut hex = String::new();
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    for row in 0..600 {
        let _ = write!(hex, "{:08x}:", row * 16);
        for _ in 0..16 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let _ = write!(hex, " {:02x}", (seed >> 56) as u8);
        }
        hex.push('\n');
    }
    // A zero-filled byte array reads as data, not a stutter.
    let zeros = format!("static TABLE: [u8; 512] = [{}];\n", "0x00, ".repeat(512));
    let padding = format!("{}\n", "0000 0000 0000\n".repeat(300));
    for (name, text) in [
        ("table", &table),
        ("code", &code),
        ("list", &list),
        ("hex", &hex),
        ("zeros", &zeros),
        ("padding", &padding),
    ] {
        for level in [LoopGuardLevel::Exact, LoopGuardLevel::Semantic] {
            assert_eq!(verdict(level, text), None, "{name} at {level:?}");
        }
    }
}

#[test]
fn the_level_follows_the_model_family() {
    let levels: Vec<LoopGuardLevel> = [
        "claude-sonnet-4-5",
        "gemini-2.5-pro",
        "deepseek-v3",
        "grok-4",
        "gpt-5",
        "glm-4.6",
    ]
    .iter()
    .map(|id| default_level(id))
    .collect();
    assert_eq!(
        levels,
        vec![
            LoopGuardLevel::Off,
            LoopGuardLevel::Semantic,
            LoopGuardLevel::Semantic,
            LoopGuardLevel::Semantic,
            LoopGuardLevel::Exact,
            LoopGuardLevel::Exact,
        ]
    );
}

/// A trip cancels the provider request through the stream's abort token and replaces the event
/// with the settled `repetition_loop` failure, its looping content voided.
#[test]
fn a_trip_cancels_the_request_and_settles_a_repetition_loop_failure() {
    let model: Model = serde_json::from_value(serde_json::json!({
        "id": "faux-1", "name": "faux", "api": "faux", "provider": "faux",
        "baseUrl": "http://localhost:0", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128_000, "maxTokens": 16_384,
    }))
    .unwrap();
    let cancel = CancellationToken::new();
    let mut armed = ArmedLoopGuard::new(
        StreamLoopGuard::new(LoopGuardLevel::Exact).unwrap(),
        cancel.clone(),
        &model,
    );
    let line = "Still checking the parser configuration.\n";
    let mut settled = None;
    for _ in 0..40 {
        let mut looping = partial();
        looping
            .content
            .push(AssistantContent::Text(crate::types::TextContent {
                text: "looping".to_string(),
                text_signature: None,
                rest: serde_json::Map::default(),
            }));
        let event = armed.screen(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: line.to_string(),
            partial: looping,
        });
        if let AssistantMessageEvent::Error { .. } = event {
            settled = Some(event);
            break;
        }
    }
    assert!(cancel.is_cancelled());
    let Some(AssistantMessageEvent::Error { reason, mut error }) = settled else {
        panic!("the loop trips");
    };
    for diagnostic in error.diagnostics.iter_mut().flatten() {
        diagnostic.timestamp = 0;
    }
    let message = "Model output started repeating itself (repetition_loop): the answer repeated a 41-byte passage 28 times in a row; cut after 1148 streamed bytes";
    let mut expected = partial();
    expected.stop_reason = StopReason::Error;
    expected.error_message = Some(message.to_string());
    expected.diagnostics = Some(vec![crate::types::AssistantMessageDiagnostic {
        type_: "provider_stream_failure".to_string(),
        timestamp: 0,
        error: Some(crate::types::DiagnosticErrorInfo {
            name: Some("StreamFailureError".to_string()),
            message: message.to_string(),
            stack: None,
            code: None,
            rest: serde_json::Map::default(),
        }),
        details: serde_json::json!({
            "kind": "repetition_loop",
            "providerErrorType": "repetition_loop",
        })
        .as_object()
        .cloned(),
    }]);
    assert_eq!((reason, error), (ErrorStopReason::Error, expected));
}

/// The scripted runaway, end to end through the shared stream entry: the faux provider streams
/// one line over and over up to the model's output limit; the guard cuts it early and the stream
/// settles as a `repetition_loop` failure. Reports the measured saving (tokens streamed before
/// the trip against the output limit the loop would have spent).
#[tokio::test]
async fn a_runaway_stream_is_cut_early_through_the_stream_entry() {
    use crate::providers::faux::{
        faux_assistant_message, faux_text, register_faux_provider, FauxAssistantMessageOptions,
        FauxResponseStep, RegisterFauxProviderOptions,
    };
    let registration = register_faux_provider(RegisterFauxProviderOptions::default());
    let model = registration.get_model();
    let max_tokens = model.max_tokens;
    // A runaway that would fill the output limit (chars/4 tokens).
    let line = "I should double-check the configuration loader before continuing.\n";
    let runaway = line.repeat(usize::try_from(max_tokens).unwrap() * 4 / line.len());
    registration.set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
        vec![faux_text(&runaway)],
        FauxAssistantMessageOptions::default(),
    ))]);
    let context = crate::types::Context {
        system_prompt: None,
        messages: Vec::new(),
        tools: None,
    };
    // The faux provider streams its fixtures unguarded by default; the request opts in.
    let options = crate::types::SimpleStreamOptions::from_base(crate::types::StreamOptions {
        loop_guard: Some(LoopGuardLevel::Exact),
        ..Default::default()
    });
    let message = crate::stream::complete_simple(&model, &context, Some(options))
        .await
        .unwrap();
    registration.unregister();

    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message.content.is_empty());
    let error = message.error_message.unwrap();
    let streamed_bytes: u64 = error
        .rsplit("cut after ")
        .next()
        .and_then(|rest| rest.split(' ').next())
        .and_then(|bytes| bytes.parse().ok())
        .expect("the failure reports the streamed bytes");
    let streamed_tokens = streamed_bytes.div_ceil(4);
    eprintln!(
        "loop guard measurement: {streamed_tokens} tokens streamed before the trip vs {max_tokens} max_tokens ({} saved)",
        max_tokens - streamed_tokens
    );
    assert!(
        streamed_tokens * 20 < max_tokens,
        "the trip lands within the first 5% of the runaway: {streamed_tokens}/{max_tokens}"
    );
}

/// The detector is pure CPU on the streaming path and O(n) amortized: 4x the bytes costs about
/// 4x the time (a quadratic scan would cost 16x). Measured relative to itself, so the budget
/// holds on any machine and build profile.
#[test]
fn the_detector_stays_linear_over_a_megabyte_stream() {
    /// Varied prose: words drawn from a large pool by an LCG, so no paragraph repeats another.
    fn benign_stream(bytes: usize) -> String {
        let mut text = String::with_capacity(bytes + 128);
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut words = 0usize;
        while text.len() < bytes {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let mut index = (seed >> 40) % 20_000;
            for _ in 0..4 {
                text.push(char::from(b'a' + u8::try_from(index % 26).unwrap()));
                index /= 26;
            }
            words += 1;
            text.push_str(match words % 60 {
                0 => ".\n\n",
                n if n % 12 == 0 => ". ",
                _ => " ",
            });
        }
        text
    }
    fn time_guard(level: LoopGuardLevel, text: &str) -> std::time::Duration {
        let mut guard = StreamLoopGuard::new(level).unwrap();
        let events: Vec<AssistantMessageEvent> = text
            .as_bytes()
            .chunks(16)
            .map(|chunk| text_delta(std::str::from_utf8(chunk).unwrap()))
            .collect();
        let started = std::time::Instant::now();
        for event in &events {
            assert_eq!(guard.observe(event), None);
        }
        started.elapsed()
    }
    let quarter = benign_stream(256 * 1024);
    let full = benign_stream(1024 * 1024);
    for level in [LoopGuardLevel::Exact, LoopGuardLevel::Semantic] {
        // Warm up the allocator and regex caches, then take the best of three for each size.
        let _ = time_guard(level, &quarter);
        let best = |text: &str| (0..3).map(|_| time_guard(level, text)).min().unwrap();
        let (small, large) = (best(&quarter), best(&full));
        eprintln!("loop guard measurement ({level:?}): 256 KiB in {small:?}, 1 MiB in {large:?}");
        assert!(
            large < small * 8,
            "4x the stream must cost well under 16x the time at {level:?}: {small:?} -> {large:?}"
        );
    }
}
