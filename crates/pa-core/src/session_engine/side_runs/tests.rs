//! The side-run registry's close and restart rules (moved from pa-daemon's
//! `side_question.rs` with the registry).

use std::sync::mpsc;

use super::*;

/// A run's future, as the tests' runs return it.
type RunFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<SideQuestionOutcome, String>> + Send>>;

/// A run that parks until the abort lands, then settles cancelled with the partial answer
/// still streamed; a settle gate, when given, holds the return until the test releases it
/// (synchronization, not a timing budget).
fn parking(
    settle_gate: Option<Arc<AtomicBool>>,
) -> impl FnOnce(SideQuestionRequest, AbortSignal, SideQuestionSink) -> RunFuture {
    move |_request, signal, _sink| {
        Box::pin(async move {
            while !signal.is_aborted() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if let Some(settle_gate) = settle_gate {
                while !settle_gate.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            Ok(SideQuestionOutcome::Aborted {
                answer: "partial".to_string(),
            })
        })
    }
}

fn start_payload(id: &str, client: &str) -> Value {
    json!({ "sideQuestionId": id, "clientId": client, "question": "what?" })
}

/// Admits `id` for `client` and drives it with `run`; the events it sends arrive on the
/// receiver.
fn start(
    runs: &Arc<SideRuns>,
    id: &str,
    client: &str,
    settle_gate: Option<Arc<AtomicBool>>,
) -> mpsc::Receiver<Value> {
    let admitted = runs
        .admit(&start_payload(id, client), "sess-1")
        .expect("admitted");
    let (sender, receiver) = mpsc::channel();
    let sender = Mutex::new(sender);
    let emit: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(move |event| {
        let _ = sender.lock().unwrap().send(event);
    });
    tokio::spawn(drive(
        Arc::clone(runs),
        admitted,
        emit,
        parking(settle_gate),
    ));
    receiver
}

/// The statuses of the events sent so far.
fn statuses(receiver: &mpsc::Receiver<Value>) -> Vec<String> {
    receiver
        .try_iter()
        .map(|event| event["status"].as_str().expect("status").to_string())
        .collect()
}

/// Waits (bounded) until the registry is empty.
async fn until_drained(runs: &SideRuns) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !runs.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the run's entry never settled out of the registry"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The close aborts every live run and waits for the cancelled events: the settle returns only
/// after the terminal event was sent.
#[tokio::test]
async fn shutdown_settle_sends_the_cancelled_events() {
    let runs = Arc::new(SideRuns::default());
    let events = start(&runs, "sq-1", "client-1", None);

    runs.abort_all_and_settle(Duration::from_secs(2)).await;

    assert_eq!(
        (statuses(&events), runs.is_empty()),
        (vec!["running".to_string(), "cancelled".to_string()], true)
    );
}

/// A close owns the registry's tail: a racing start is rejected instead of being admitted into
/// a session about to close (its pane would wedge).
#[tokio::test]
async fn start_during_the_close_is_rejected() {
    let runs = Arc::new(SideRuns::default());
    let _events = start(&runs, "sq-1", "client-1", None);

    runs.abort_all_and_settle(Duration::from_secs(2)).await;

    assert_eq!(
        runs.admit(&start_payload("sq-2", "client-1"), "sess-1")
            .err(),
        Some("Active session sess-1 is closing".to_string())
    );
}

/// The detach abort keeps the registry entry until the run's terminal cancelled event was sent
/// (a detach racing a close must not empty the registry).
#[tokio::test]
async fn detach_abort_settles_the_entry_it_aborts() {
    let runs = Arc::new(SideRuns::default());
    let events = start(&runs, "sq-1", "client-1", None);

    runs.abort_for_client("client-1");
    until_drained(&runs).await;

    assert_eq!(
        statuses(&events),
        vec!["running".to_string(), "cancelled".to_string()]
    );
}

/// A same-id restart reacting to the cancelled event must never read a stale entry (the probe
/// runs without awaiting, so the multi-thread flavor is required: a current-thread runtime
/// would deadlock).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_id_restart_after_the_cancelled_event_is_admitted() {
    let runs = Arc::new(SideRuns::default());
    let settle_gate = Arc::new(AtomicBool::new(false));
    let events = start(&runs, "sq-1", "client-1", Some(Arc::clone(&settle_gate)));

    // The detach abort parks the run behind the registry hold the probe takes: the terminal
    // send needs the same registry lock that removes the entry.
    runs.abort_for_client("client-1");
    {
        let held = runs.lock();
        assert!(
            held.contains_key("sq-1"),
            "the aborted run is still registered when the hold opens"
        );
        // The run settles only from here on: the gate opens under the hold, so the send races
        // the probe, never predates it.
        settle_gate.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            assert!(
                !statuses(&events).contains(&"cancelled".to_string()),
                "the cancelled event was sent while the registry hold was taken: the terminal \
                 send no longer rides the removal's hold"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // The hold released, the terminal path settles and the cancelled event is sent.
    let cancelled = tokio::task::spawn_blocking(move || {
        events
            .iter()
            .find(|event| event["status"] == "cancelled")
            .expect("the cancelled event")
    })
    .await
    .unwrap();
    assert_eq!(cancelled["id"], json!("sq-1"));

    // The fast same-id restart reads a registry where the id is already free.
    let _restarted = start(&runs, "sq-1", "client-1", None);

    // Tidy: settle the restarted run.
    runs.abort_for_client("client-1");
    until_drained(&runs).await;
}
