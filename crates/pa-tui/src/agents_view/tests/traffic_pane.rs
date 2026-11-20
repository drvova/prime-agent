//! The scoped view's Traffic pane: unread `✉N` badges and the dock counter while it is closed,
//! the pane's entries once it opens (clearing the unread counts), and the `waiting on` label.

use super::*;

fn entry(to: &str, preview: &str) -> pa_types::agent_traffic::AgentTrafficEntry {
    pa_types::agent_traffic::AgentTrafficEntry {
        root_active_session_id: "p-live".to_string(),
        from: "worker one".to_string(),
        from_active_session_id: Some("c-live".to_string()),
        to: "p name".to_string(),
        to_active_session_id: to.to_string(),
        relationship: Some("child".to_string()),
        kind: "direct".to_string(),
        outcome: "queued".to_string(),
        chars: 11,
        preview: preview.to_string(),
        timestamp_ms: 1,
        rest: serde_json::Map::default(),
    }
}

fn scoped_family() -> AgentsViewMode {
    let mut waiting = child_summary("c", "p", "worker one");
    waiting["waitingOn"] = serde_json::json!("d-live");
    scoped_mode(
        None,
        vec![
            roster_entry("p", "idle", &parent_summary("p")),
            roster_entry("c", "running", &waiting),
            roster_entry("d", "idle", &child_summary("d", "p", "worker two")),
        ],
    )
}

fn frame(mode: &mut AgentsViewMode) -> Vec<String> {
    let (lines, _) = mode.render_frame(120, 30);
    lines.iter().map(flat).collect()
}

#[test]
fn closed_traffic_counts_unread_and_the_open_pane_lists_it() {
    let mut mode = scoped_family();
    mode.traffic.push(entry("c-live", "first note"));
    mode.traffic.push(entry("c-live", "second note"));
    let closed = frame(&mut mode);
    let worker = closed
        .iter()
        .find(|line| line.contains("worker one"))
        .expect("the worker row renders");
    assert!(worker.contains("\u{2709}2 worker one"), "{closed:#?}");
    assert!(
        worker.contains("\u{29d7} waiting on worker two"),
        "{closed:#?}"
    );
    assert!(
        closed
            .last()
            .is_some_and(|hints| hints.contains("M traffic \u{2709}2")),
        "{closed:#?}"
    );

    mode.handle_key("m");
    assert!(
        frame(&mut mode)
            .iter()
            .any(|line| line.contains("Traffic \u{b7} connecting")),
        "the pane opens before the subscription lands"
    );
    // The subscription lands with an empty ring: the two pushed entries stay.
    assert!(mode.traffic.take_subscribe());
    mode.traffic.subscribed(Ok(Vec::new()));
    let open = frame(&mut mode);
    let pane: Vec<&String> = open
        .iter()
        .skip_while(|line| !line.contains("Traffic \u{b7} 2 messages"))
        .take(3)
        .collect();
    assert_eq!(
        pane.iter().map(|line| line.trim_end()).collect::<Vec<_>>()[1..],
        [
            "  worker one \u{2192} p name \u{b7} child \u{b7} queued \u{b7} 11 chars  first note",
            "  worker one \u{2192} p name \u{b7} child \u{b7} queued \u{b7} 11 chars  second note",
        ],
        "{open:#?}"
    );
    assert!(
        !open.iter().any(|line| line.contains('\u{2709}')),
        "opening the pane read every message: {open:#?}"
    );
    assert!(
        open.last()
            .is_some_and(|hints| hints.contains("M hide traffic")),
        "{open:#?}"
    );
    // While the pane is open, new traffic lists without an unread badge.
    mode.traffic.push(entry("c-live", "third note"));
    assert!(!frame(&mut mode)
        .iter()
        .any(|line| line.contains('\u{2709}')));
}

/// The pane key opts in to the feed once: the first open dispatches the subscribe, a re-open
/// while it is live does not.
#[test]
fn the_first_open_subscribes_once() {
    let mut mode = scoped_family();
    mode.handle_key("m");
    assert!(mode.traffic.take_subscribe());
    mode.traffic.subscribed(Ok(Vec::new()));
    mode.handle_key("m");
    mode.handle_key("m");
    assert!(!mode.traffic.take_subscribe());
}
