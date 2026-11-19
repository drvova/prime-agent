use super::watch_tests::{sessions_with_fake_supervisor, spawn_child, FakeChild, FakeKill};
use super::*;
use tokio::sync::mpsc;

/// `rlm.abort` stops the child's running task turn and keeps the child: the roster still lists
/// it (settled `cancelled`, so a later message starts a new turn), a second abort of the now idle
/// child reports `idle`, and a selector that is not a direct child is refused.
#[tokio::test]
async fn abort_stops_the_running_turn_and_keeps_the_child() {
    let (follow_up_tx, _follow_up_rx) = mpsc::unbounded_channel();
    let (_tmp, sessions, _kill_rx) =
        sessions_with_fake_supervisor(follow_up_tx, 500, FakeKill::Success, FakeChild::Healthy)
            .await;
    let handle = spawn_child(&sessions).await;
    let aborted = sessions
        .abort_child("f20-worker".to_string())
        .await
        .unwrap();
    assert_eq!(
        (
            aborted.outcome,
            aborted.subagent.rlm_child_id.as_str(),
            aborted.subagent.status
        ),
        (
            RlmAbortOutcome::Aborted,
            handle.rlm_child_id.as_str(),
            "cancelled"
        )
    );
    let roster: Vec<String> = sessions
        .list_subagents()
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.rlm_child_id)
        .collect();
    assert_eq!(roster, vec![handle.rlm_child_id.clone()]);
    let again = sessions
        .abort_child(handle.rlm_child_id.clone())
        .await
        .unwrap();
    assert_eq!(again.outcome, RlmAbortOutcome::Idle);
    assert_eq!(
        sessions
            .abort_child("someone-else".to_string())
            .await
            .unwrap_err()
            .to_string(),
        "No direct RLM subagent matches \"someone-else\" in the current parent session"
    );
}
