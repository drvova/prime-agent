//! Worker tests.
use super::*;

fn worker_with_journal() -> (tempfile::TempDir, Arc<Worker>) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let config = WorkerConfig {
        socket_path: dir.join("worker.sock"),
        supervisor_socket_path: PathBuf::new(),
        token: "token".to_string(),
        worker_instance_id: String::new(),
        active_session_id: "target-session".to_string(),
        agent_dir: dir.join("agent"),
        recovery_journal_path: dir.join("recovery.jsonl"),
        telemetry_disabled: None,
        script: Some(json!({ "responses": ["ack"] })),
    };
    let worker = Arc::new(Worker::new(config, None));
    // The journal is opened in `serve()`; tests open it directly so the
    // checkpoints have the same durable sink as production.
    *worker.recovery.lock().unwrap() =
        Some(WorkerRecoveryJournal::open(&worker.config.recovery_journal_path).unwrap());
    (tmp, worker)
}

async fn created_worker_with_journal() -> Arc<Worker> {
    let (_tmp, worker) = worker_with_journal();
    let created = worker
        .dispatch(
            "create",
            &json!({ "noSession": true, "cwd": "/tmp", "name": "target" }),
        )
        .await;
    assert!(created.success, "create failed: {created:?}");
    worker
}

fn latest_record(worker: &Worker) -> crate::journal::WorkerRecoveryRecord {
    WorkerRecoveryJournal::read_latest(&worker.config.recovery_journal_path)
        .unwrap()
        .into_iter()
        .find(|record| record.active_session_id == "target-session")
        .expect("session record")
}

/// The admission (not the pickup) proves the work, so a plain boot revives the worker.
#[tokio::test]
async fn idle_time_injected_admission_is_busy_evidence() {
    let worker = created_worker_with_journal().await;
    // Settle first: the create record's busy=true must not mask the
    // admission's verdict.
    worker.dispatch("clear_queue", &json!({})).await;
    assert!(
        !WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the settled session proves nothing"
    );
    let notify = Arc::new(Notify::new());
    admit_autonomous_follow_up(
        &worker.recovery,
        &worker.core,
        &notify,
        "continue the mission".to_string(),
    );
    assert!(
        WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the injected admission is live work"
    );
    let latest = latest_record(&worker);
    assert_eq!(latest.operation, "follow_up_queued");
    let (steering, follow_up, _) = WorkerRecoveryJournal::read_queue_snapshot(
        &worker.config.recovery_journal_path,
        "target-session",
    )
    .unwrap()
    .expect("the admission flushed its snapshot");
    assert!(steering.is_empty(), "steering: {steering:?}");
    assert_eq!(follow_up[0].message, "continue the mission");
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}

/// An idle session wakes on an invisible injected row; the admission is journal busy evidence.
#[tokio::test]
async fn a_bash_completion_notice_admits_the_steering_lane_with_busy_evidence() {
    let worker = created_worker_with_journal().await;
    worker.dispatch("clear_queue", &json!({})).await;
    let notify = Arc::new(Notify::new());
    admit_kernel_notice(
        &worker.recovery,
        &worker.core,
        &worker.events,
        &notify,
        crate::engine::KernelNotice::bash_completion(
            4321,
            "sleep 12; echo RW_WAKE_DONE",
            0,
            pa_core::session_engine::messages::BackgroundOutputTail::Complete(""),
        ),
        || false,
    );
    let core = worker.core.lock().unwrap();
    let item = core
        .steering
        .front()
        .expect("the notice queues on the steering lane");
    let row = item.custom_message.as_ref().expect("the injected row");
    assert_eq!(
        row.get("customType").and_then(Value::as_str),
        Some("async_bash_completion"),
        "the row is the async-bash-completion notice: {row}"
    );
    assert_eq!(
        row["details"]["pid"],
        json!(4321),
        "the notice carries its pid: {row}"
    );
    assert!(
        item.message.starts_with("[bash-done pid:4321 exit:0]"),
        "the turn runs on the notice content: {item:?}"
    );
    assert!(
        item.preview
            .as_deref()
            .is_some_and(|preview| preview.starts_with("Background command finished: ")),
        "the queue row carries the TS preview label: {item:?}"
    );
    // An idle session's wake is an invisible injected turn.
    assert!(!item.queue_visible, "the idle wake stays invisible");
    drop(core);
    assert!(
        WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the notice admission is live work"
    );
    let latest = latest_record(&worker);
    assert_eq!(latest.operation, "steer_queued");
    let (steering, _, _) = WorkerRecoveryJournal::read_queue_snapshot(
        &worker.config.recovery_journal_path,
        "target-session",
    )
    .unwrap()
    .expect("the admission flushed its snapshot");
    assert_eq!(
        steering[0].message,
        "[bash-done pid:4321 exit:0]\n\nCommand: \"sleep 12; echo RW_WAKE_DONE\"\n\nOutput: (none)"
    );
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}

/// A recorded notice (a passed acceptance-check round) shows its row and queues no turn.
#[tokio::test]
async fn a_recorded_notice_broadcasts_its_row_and_queues_no_turn() {
    let worker = created_worker_with_journal().await;
    worker.dispatch("clear_queue", &json!({})).await;
    let mut subscription = worker.events.subscribe();
    let row = pa_types::session::CustomMessage {
        custom_type: "better_acceptance_checks".to_string(),
        content: pa_types::ai::UserContent::Text("[better-checks out:1 · 1 passed]".to_string()),
        display: true,
        details: Some(json!({ "failed": 0 })),
        timestamp: 7,
        rest: serde_json::Map::default(),
    };
    admit_kernel_notice(
        &worker.recovery,
        &worker.core,
        &worker.events,
        &Arc::new(Notify::new()),
        crate::engine::KernelNotice {
            row: row.clone(),
            preview_label: "Acceptance checks",
            delivery: pa_core::session_engine::bash_notices::NoticeDelivery::Record,
        },
        || false,
    );
    let message = serde_json::to_value(pa_types::session::AgentMessage::Custom(row)).unwrap();
    let mut events = Vec::new();
    while let Ok(frame) = subscription.try_recv() {
        let outbound: Value = serde_json::from_slice(&frame.payload).unwrap();
        events.push(outbound["event"].clone());
    }
    assert_eq!(
        events,
        vec![
            json!({ "type": "message_start", "message": message }),
            json!({ "type": "message_end", "message": message }),
        ]
    );
    let core = worker.core.lock().unwrap();
    assert_eq!(
        (core.steering.len(), core.follow_up.len()),
        (0, 0),
        "a recorded row never queues a turn"
    );
    drop(core);
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}

/// A busy session queues the notice as a visible steer row (TS `queueIfBusy`).
#[tokio::test]
async fn a_bash_completion_notice_on_a_busy_session_queues_a_visible_steer_row() {
    let worker = created_worker_with_journal().await;
    worker.core.lock().unwrap().busy = true;
    let notify = Arc::new(Notify::new());
    admit_kernel_notice(
        &worker.recovery,
        &worker.core,
        &worker.events,
        &notify,
        crate::engine::KernelNotice::bash_completion(
            99,
            "make gates",
            2,
            pa_core::session_engine::messages::BackgroundOutputTail::Complete(""),
        ),
        || false,
    );
    let core = worker.core.lock().unwrap();
    let item = core.steering.front().expect("the queued notice");
    assert!(item.queue_visible, "the busy session keeps a visible row");
    assert_eq!(item.policy, TurnPolicy::Queued);
    drop(core);
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}

/// A notice that only wakes an idle session (`[bash-quiet]`) never lands in a
/// turn in progress; an idle session wakes into it as into a completion.
#[tokio::test]
async fn a_wake_idle_notice_is_dropped_by_a_busy_session_and_wakes_an_idle_one() {
    use pa_core::session_engine::bash_notices::NoticeAdmission;
    let worker = created_worker_with_journal().await;
    worker.dispatch("clear_queue", &json!({})).await;
    let quiet = || crate::engine::KernelNotice {
        row: pa_core::session_engine::messages::create_async_bash_quiet_message(
            &[pa_core::session_engine::messages::QuietJob {
                job: "a1".to_string(),
                pid: 77,
                command: "sleep 600".to_string(),
                elapsed_ms: 200_000,
                quiet_ms: 180_000,
                output: String::new(),
                output_truncated: false,
            }],
            7,
        ),
        preview_label: "Background command quiet",
        delivery: pa_core::session_engine::bash_notices::NoticeDelivery::WakeIdle,
    };
    let admit = |busy: bool| {
        worker.core.lock().unwrap().busy = busy;
        let admission = admit_kernel_notice(
            &worker.recovery,
            &worker.core,
            &worker.events,
            &Arc::new(Notify::new()),
            quiet(),
            || false,
        );
        let core = worker.core.lock().unwrap();
        (
            admission,
            core.steering
                .iter()
                .map(|item| (item.policy, item.queue_visible))
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        (admit(true), admit(false)),
        (
            (NoticeAdmission::DroppedBusy, Vec::new()),
            (NoticeAdmission::Taken, vec![(TurnPolicy::Injected, false)])
        )
    );
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}

/// The withdrawal settles the busy evidence so the journal never promises a replay.
#[tokio::test]
async fn bash_consumed_withdraws_the_undelivered_notice_and_settles() {
    let worker = created_worker_with_journal().await;
    worker.dispatch("clear_queue", &json!({})).await;
    let notify = Arc::new(Notify::new());
    admit_kernel_notice(
        &worker.recovery,
        &worker.core,
        &worker.events,
        &notify,
        crate::engine::KernelNotice::bash_completion(
            4321,
            "sleep 12; echo RW_WAKE_DONE",
            0,
            pa_core::session_engine::messages::BackgroundOutputTail::Complete(""),
        ),
        || false,
    );
    assert!(
        WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the notice admission is live work"
    );
    // A different command under a reused pid must not withdraw (TS
    // `_isAsyncBashCompletionActionFor` matches both).
    withdraw_background_completion_notice(
        &worker.recovery,
        &worker.core,
        &crate::engine::BackgroundJobConsumed::Bash {
            pid: 4321,
            command: "another command".to_string(),
        },
        Some(worker.engine.as_ref()),
    );
    assert!(
        worker.core.lock().unwrap().steering.len() == 1,
        "the mismatched withdrawal kept the row"
    );
    assert!(
        WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the kept row stays live work"
    );
    withdraw_background_completion_notice(
        &worker.recovery,
        &worker.core,
        &crate::engine::BackgroundJobConsumed::Bash {
            pid: 4321,
            command: "sleep 12; echo RW_WAKE_DONE".to_string(),
        },
        /*engine*/
        None, // A retired engine must not prevent withdrawal or its checkpoint.
    );
    {
        let core = worker.core.lock().unwrap();
        assert!(
            core.steering.is_empty() && core.follow_up.is_empty(),
            "the consumed notice withdrew"
        );
    }
    assert!(
        !WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the withdrawal settled the busy evidence"
    );
    let latest = latest_record(&worker);
    assert_eq!(latest.operation, "queue_purged");
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}

/// Cancelling an owned queued admission settles the verdict: removed rows
/// leave no busy evidence and no replayable snapshot.
#[tokio::test]
async fn cancelled_admission_drop_settles_the_verdict() {
    let worker = created_worker_with_journal().await;
    worker.dispatch("clear_queue", &json!({})).await;
    let admitted = worker
        .dispatch("prompt", &json!({ "admissionId": "a1", "message": "go" }))
        .await;
    assert!(admitted.success, "prompt failed: {admitted:?}");
    assert!(
        WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the admitted prompt is live work"
    );
    let cancelled = worker
        .dispatch(
            "cancel_prompt_admission",
            &json!({ "admissionId": "a1", "cancelOwned": true }),
        )
        .await;
    assert_eq!(cancelled.data, Some(json!({ "status": "owned" })));
    assert!(
        !WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the dropped rows leave no busy evidence"
    );
    let latest = latest_record(&worker);
    assert_eq!(latest.operation, "queue_dropped");
    let (steering, follow_up, _) = WorkerRecoveryJournal::read_queue_snapshot(
        &worker.config.recovery_journal_path,
        "target-session",
    )
    .unwrap()
    .expect("the drop flushed its snapshot");
    assert!(
        steering.is_empty() && follow_up.is_empty(),
        "lanes: {steering:?} {follow_up:?}"
    );
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}

/// The in-flight turn is live work: only its own `turn_end` (after the idle flip) settles it.
#[tokio::test]
async fn mid_turn_withdrawal_keeps_the_in_flight_turn_busy() {
    let worker = created_worker_with_journal().await;
    // Mid-turn: the runner is streaming, and the withdrawal leaves
    // nothing queued behind it.
    worker.core.lock().unwrap().busy = true;
    worker.dispatch("clear_queue", &json!({})).await;
    let latest = latest_record(&worker);
    assert_eq!(latest.operation, "queue_cleared");
    assert!(
        latest.busy,
        "the in-flight turn keeps the withdrawal's verdict busy"
    );
    let (steering, follow_up, _) = WorkerRecoveryJournal::read_queue_snapshot(
        &worker.config.recovery_journal_path,
        "target-session",
    )
    .unwrap()
    .expect("the withdrawal flushed its snapshot");
    assert!(
        steering.is_empty() && follow_up.is_empty(),
        "the withdrawn rows left the snapshot: {steering:?} {follow_up:?}"
    );
    // The turn ends: the runner's idle flip precedes its settle, so the
    // same empty lanes now record busy=false.
    worker.core.lock().unwrap().busy = false;
    worker.dispatch("clear_queue", &json!({})).await;
    let latest = latest_record(&worker);
    assert_eq!(latest.operation, "queue_cleared");
    assert!(!latest.busy, "the settled turn leaves the session idle");
    assert!(
        !WorkerRecoveryJournal::read_interrupted(&worker.config.recovery_journal_path),
        "the settled session proves nothing"
    );
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}

/// A prompt admitted while the runner is between turns can be picked up
/// before its admission checkpoint is written (the prompt is acknowledged and
/// its turn starts either way). That late checkpoint must not erase the
/// pickup: a daemon that dies under the turn revives it, and from the
/// pickup's time, so the prompt the transcript saved meanwhile is not
/// prompted again.
#[tokio::test]
async fn a_late_admission_checkpoint_keeps_the_turn_its_prompt_started() {
    let worker = created_worker_with_journal().await;
    worker.dispatch("clear_queue", &json!({})).await;
    let journal_path = worker.config.recovery_journal_path.clone();
    let prompt = crate::journal::WorkerQueueItemRecord {
        message: "second turn".to_string(),
        priority: Some(QueuePriority::Human),
        preview: None,
        custom_message: None,
        queue_key: None,
        queue_visible: false,
        policy: "direct".to_string(),
        forced_batch: false,
        images: Vec::new(),
    };
    // The runner took the prompt off its lane and journaled the pickup. From
    // here to the checkpoint nothing awaits, so this worker's own runner (on
    // this current-thread runtime) cannot park the session in between.
    worker.core.lock().unwrap().busy = true;
    worker
        .recovery
        .lock()
        .unwrap()
        .as_mut()
        .expect("journal")
        .record_turn_pickup(
            "target-session",
            vec![prompt],
            /*running_steering*/ true,
            QueueLanes {
                steering: Vec::new(),
                follow_up: Vec::new(),
            },
            /*suspended*/ false,
        )
        .expect("pickup record");
    // The turn saved its prompt row after the pickup, and the admission's
    // own checkpoint lands later still.
    let pickup_at = crate::util::now_iso();
    let mut saved_at = crate::util::now_iso();
    while saved_at <= pickup_at {
        std::thread::yield_now();
        saved_at = crate::util::now_iso();
    }
    let transcript = journal_path.with_file_name("session.jsonl");
    std::fs::write(
        &transcript,
        json!({
            "type": "message",
            "timestamp": saved_at,
            "message": { "role": "user", "content": "second turn" },
        })
        .to_string(),
    )
    .expect("write the transcript");
    while crate::util::now_iso() <= saved_at {
        std::thread::yield_now();
    }
    checkpoint_queue_recovery(
        &worker.recovery,
        &worker.core,
        QueueCheckpoint::Admitted {
            operation: "prompt_accepted",
        },
    );
    // The daemon dies here; the boot settles the journal by the transcript.
    *worker.recovery.lock().unwrap() = None;
    let verdict = crate::revived_turn::settle_in_journal(&journal_path, &transcript)
        .expect("settle the journal");
    let (steering, follow_up, _) =
        WorkerRecoveryJournal::read_queue_snapshot(&journal_path, "target-session")
            .unwrap()
            .expect("the snapshot");
    let custom_types = |lane: &[crate::journal::WorkerQueueItemRecord]| {
        lane.iter()
            .map(|item| {
                item.custom_message
                    .as_ref()
                    .map(|row| row["customType"].clone())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        (verdict, custom_types(&steering), custom_types(&follow_up)),
        (
            Some(crate::revived_turn::RevivedTurn::Interrupted),
            vec![Some(json!(
                crate::revived_turn::TURN_INTERRUPTED_CUSTOM_TYPE
            ))],
            Vec::new(),
        ),
        "the revived turn continues behind its notice and its prompt is not queued again"
    );
    let _ = std::fs::remove_dir_all(worker.config.socket_path.parent().unwrap());
}
