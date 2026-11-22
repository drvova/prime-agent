//! A session that went down wears it on its roster row: a badge, and the
//! reason under the row while it is selected.

use super::*;

#[test]
fn a_faulted_session_row_shows_its_state_and_the_selected_row_its_reason() {
    let (mut mode, index) = mode_with_row("worker", "mock-1");
    mode.rows[index]
        .summary
        .as_object_mut()
        .expect("row summary is an object")
        .insert(
            "lastCrash".to_string(),
            serde_json::json!({
                "kind": "panic", "message": "boom", "location": "src/a.rs:1:2",
                "thread": "session-1", "at": "2026-01-01T00:00:00.000Z", "state": "failed",
            }),
        );
    mode.selected = index;
    let selected: Vec<String> = mode.render_list(200, 30, 0).iter().map(flat).collect();
    let row = selected
        .iter()
        .position(|line| line.contains("worker"))
        .expect("the worker row renders");
    mode.selected = 0;
    let unselected: Vec<String> = mode.render_list(200, 30, 0).iter().map(flat).collect();
    assert_eq!(
        (
            selected[row].contains("\u{2717} failed worker"),
            selected[row + 1].trim(),
            unselected.iter().any(|line| line.contains("\u{2717} failed worker")),
            unselected.iter().any(|line| line.contains("boom")),
        ),
        (
            true,
            "Session crashed (panic): boom at src/a.rs:1:2 (2026-01-01T00:00:00.000Z) \u{2014} it kept going down and was not started again; open it to start it",
            true,
            false,
        ),
        "selected: {selected:?}\nunselected: {unselected:?}"
    );
}
