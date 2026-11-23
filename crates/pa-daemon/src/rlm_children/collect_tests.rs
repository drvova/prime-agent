//! `collect` reads the registry's records and asks no child: a child that cannot
//! answer must not hold it up.
use super::watch_tests::{sessions_with_fake_supervisor, spawn_child, FakeChild, FakeKill};
use super::*;

/// A child whose worker never answers a state read (wedged, rebuilding after a
/// crash, its route full) is the hanging sink: the snapshot and the timed wait
/// both come back from the records, the child still `running`. Before, each
/// collect queued a state read of its own behind the 30-second route timeout.
#[tokio::test]
async fn a_child_that_never_answers_does_not_hold_up_a_collect() {
    let (follow_up_tx, _follow_up_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_tmp, sessions, _kill_rx) =
        sessions_with_fake_supervisor(follow_up_tx, 0, FakeKill::Success, FakeChild::Silent).await;
    spawn_child(&sessions).await;
    sessions.notify_turn_done();
    // Readiness: the task prompt admitted, so the settle watcher is reading the child.
    let kickoff = Arc::clone(&sessions.inner.children.lock().await[0].lock().await.kickoff);
    tokio::time::timeout(
        Duration::from_secs(10),
        kickoff
            .subscribe()
            .wait_for(|kickoff| *kickoff == Kickoff::Finished),
    )
    .await
    .expect("the task prompt admits")
    .expect("the kickoff feed stays open");

    let snapshot = tokio::time::timeout(Duration::from_secs(2), sessions.collect(Vec::new(), 0))
        .await
        .expect("a snapshot waits for no child")
        .expect("collect");
    let waited = tokio::time::timeout(Duration::from_secs(3), sessions.collect(Vec::new(), 300))
        .await
        .expect("a timed collect ends at its timeout")
        .expect("a timeout is never an error");
    let rows = |results: &[RlmChildResult]| {
        results
            .iter()
            .map(|row| (row.status, row.settled))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        (rows(&snapshot), rows(&waited)),
        (vec![("running", false)], vec![("running", false)])
    );
}
