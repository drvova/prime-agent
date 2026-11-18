//! The deferred transcript search and the highlighted session's preview row: a resting query
//! opted in with the `?` prefix matches inside conversation text with a per-session count (a plain
//! query never does), and the preview row holds the selected session's first prompt without
//! moving the list.

use super::*;

/// One idle top-level session whose file is `file`.
fn session(n: usize, name: &str, file: &std::path::Path, first: &str) -> serde_json::Value {
    roster_entry(
        &format!("s{n}"),
        "idle",
        &serde_json::json!({
            "sessionId": format!("s{n}"), "lifecycle": "live",
            "activeSessionId": format!("s{n}-live"),
            "sessionFile": file.to_string_lossy(),
            "runtimeKind": "top-level",
            "sessionName": name,
            "firstMessage": first,
            "messageCount": 2,
            "rlmDepth": 0,
            "lastActivityAt": format!("2025-01-{:02}T00:00:00.000Z", 10 - n),
        }),
    )
}

/// One session-file line holding a message with `role` and `text`.
fn message_line(role: &str, text: &str) -> String {
    serde_json::json!({
        "type": "message",
        "message": { "role": role, "content": [{ "type": "text", "text": text }] },
    })
    .to_string()
}

#[test]
fn only_a_question_prefixed_query_matches_inside_transcripts_with_a_count() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let parser = dir.path().join("a.jsonl");
    let quiet = dir.path().join("b.jsonl");
    std::fs::write(
        &parser,
        [
            message_line("user", "Split the Parser in two"),
            message_line("assistant", "The parser now has a tokenizer stage."),
            // Tool output never counts: only user and assistant text does.
            message_line("toolResult", "parser.rs"),
        ]
        .join("\n"),
    )
    .expect("write session");
    std::fs::write(&quiet, message_line("user", "write docs")).expect("write session");
    let roster = || {
        vec![
            session(1, "refactor", &parser, "Split the Parser in two"),
            session(2, "docs", &quiet, "write docs"),
        ]
    };
    // A plain query is name, id, and cwd only: it schedules no transcript scan.
    let mut plain = fresh_mode(roster());
    for ch in "parser".chars() {
        plain.handle_key(&ch.to_string());
    }
    assert!(plain.take_content_search().is_none());
    let mut mode = fresh_mode(roster());
    for ch in "?parser".chars() {
        mode.handle_key(&ch.to_string());
    }
    // The title filter answers at once: neither title holds the query.
    assert!(mode.rows.iter().all(|row| row.kind != RowKind::Agent));
    let request = mode
        .take_content_search()
        .expect("the resting query schedules a scan");
    let (generation, hits) = (request.generation, request.scan());
    mode.content_search_loaded(generation, hits);
    let (lines, _) = mode.render_frame(100, 30);
    let rows: Vec<String> = lines
        .iter()
        .map(flat)
        .filter(|line| line.contains("refactor") || line.contains("docs"))
        .collect();
    assert_eq!(rows.len(), 1, "only the transcript match lists: {rows:?}");
    assert!(
        rows[0].contains("refactor 2 matches"),
        "the row carries its count: {rows:?}"
    );
}

#[test]
fn the_preview_row_shows_the_first_prompt_without_moving_the_list() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let file = dir.path().join("a.jsonl");
    let mut mode = fresh_mode(vec![
        session(1, "refactor", &file, "Split the parser\ninto three stages"),
        session(2, "docs", &file, ""),
    ]);
    let frame = |mode: &mut AgentsViewMode| -> Vec<String> {
        mode.render_frame(100, 30).0.iter().map(flat).collect()
    };
    mode.selected = mode
        .rows
        .iter()
        .position(|row| row.title == "refactor")
        .expect("refactor row");
    let with_prompt = frame(&mut mode);
    mode.selected = mode
        .rows
        .iter()
        .position(|row| row.title == "docs")
        .expect("docs row");
    let without = frame(&mut mode);
    assert_eq!(
        with_prompt[28].trim_end(),
        "\u{276f} Split the parser into three stages"
    );
    assert_eq!(without[28].trim_end(), "");
    // The list keeps its rows: every session sits on the same frame row either way.
    let row_of = |frame: &[String], title: &str| frame.iter().position(|line| line.contains(title));
    for title in ["refactor", "docs"] {
        assert_eq!(
            row_of(&with_prompt, title),
            row_of(&without, title),
            "{title}"
        );
    }
}
