//! The executor where a stop, a resume or a late answer lands while the
//! control loop waits on the host. The scripted host holds one named
//! request open (a gate) so the other side acts inside that window; nothing
//! here depends on timing. Each test names the Python tests it carries over
//! (`prime-agent-runtime/test/test_factory.py`).

use std::time::Duration;

use serde_json::{json, Value};

use super::executor_tests::{fan_dag, worker};
use super::test_host::{events, instance_column, node, Fixture, Outcome};

/// Stop `started` on a task of its own, so the test can act while the
/// stop waits on the host.
fn stopping(fixture: &Fixture, started: &Value) -> tokio::task::JoinHandle<Value> {
    let executor = fixture.executor.clone();
    let run_id = started["run_id"].as_str().unwrap().to_string();
    tokio::spawn(async move { executor.stop(&run_id).await.unwrap() })
}

/// `test_concurrent_stop_runs_one_cancellation_pass`,
/// `test_stop_window_admits_no_new_spawns_and_never_finalizes`,
/// `test_stop_delete_receipts_with_cancelled_status_complete_cleanly`.
#[tokio::test(start_paused = true)]
async fn a_second_stop_inside_the_first_ones_cancellation_adds_no_delete_and_nothing_is_admitted() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::running());
    let (gate, entered) = fixture.host.gate("rlm.delete_subagent", 1);
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [worker("a"), {"id": "b", "subagent": "worker", "depends_on": ["a"]}]}),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let first = stopping(&fixture, &started);
    entered.acquire().await.unwrap().forget();
    // The first stop is inside its delete: the run reads `stopping`.
    let mut second = fixture.executor.stop(run_id).await.unwrap();
    second["run_id"] = json!("the run");
    // Even a settle now changes nothing: the instance is already claimed.
    fixture.host.set_node("a", Outcome::done("too late"));
    gate.add_permits(1);
    let first = first.await.unwrap();
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            second,
            first["cancelled"].clone(),
            fixture.host.deleted(),
            events(&status, "run_stopped").len(),
            fixture.host.calls_of("rlm.run").len(),
            status["state"].clone(),
            fixture.host.notice_kinds(),
        ),
        (
            json!({"run_id": "the run", "state": "stopping", "cancelled": []}),
            json!(["a", "b"]),
            vec!["child-1".to_string()],
            1,
            1,
            json!("stopped"),
            Vec::<String>::new(),
        )
    );
}

/// `test_concurrent_stop_during_fail_fast_deletes_each_child_once`.
#[tokio::test(start_paused = true)]
async fn a_stop_during_a_fail_fast_cascade_deletes_each_child_once_and_the_stop_wins() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::error("boom"));
    fixture.host.set_node("b", Outcome::running());
    fixture.host.set_node("c", Outcome::running());
    // The cascade's first delete is held open while the stop runs.
    let (gate, entered) = fixture.host.gate("rlm.delete_subagent", 1);
    let started = fixture
        .start(
            "dag",
            json!({
                "run": {"failure_policy": "fail_fast"},
                "nodes": [worker("a"), worker("b"), worker("c")],
            }),
        )
        .await;
    entered.acquire().await.unwrap().forget();
    let stop = stopping(&fixture, &started);
    // The stop finds one child left unclaimed and deletes it itself.
    fixture
        .until(&started, |_| fixture.host.deleted() == ["child-3"])
        .await;
    gate.add_permits(1);
    stop.await.unwrap();
    // The cascade's held delete lands once its gate opens.
    fixture
        .until(&started, |_| fixture.host.deleted().len() == 2)
        .await;
    let status = fixture.settle(&started).await;
    let mut deleted = fixture.host.deleted();
    deleted.sort();
    assert_eq!(
        (
            deleted,
            status["state"].clone(),
            instance_column(&status, "b", "status"),
            instance_column(&status, "c", "status"),
            fixture.host.notice_kinds(),
        ),
        (
            vec!["child-2".to_string(), "child-3".to_string()],
            json!("stopped"),
            vec![json!("cancelled")],
            vec![json!("cancelled")],
            Vec::<String>::new(),
        )
    );
}

/// `test_stop_during_admission_backoff_cancels_the_instance`.
#[tokio::test(start_paused = true)]
async fn a_stop_while_admission_backs_off_cancels_the_waiting_instance() {
    let fixture = Fixture::new();
    fixture.host.rate_limit("a", 999);
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let run_id = started["run_id"].as_str().unwrap();
    fixture
        .until(&started, |graph| !events(graph, "spawn_backoff").is_empty())
        .await;
    let attempts = fixture.host.spawns("a").len();
    let stopped = fixture.executor.stop(run_id).await.unwrap();
    // Well past every backoff deadline the stopped run admits nothing.
    tokio::time::sleep(Duration::from_secs(120)).await;
    let status = fixture.executor.status(run_id).unwrap();
    assert_eq!(
        (
            stopped["cancelled"].clone(),
            instance_column(&status, "a", "status"),
            fixture.host.spawns("a").len() == attempts,
            fixture.host.deleted().len(),
            status["state"].clone(),
        ),
        (
            json!(["a"]),
            vec![json!("cancelled")],
            true,
            0,
            json!("stopped")
        )
    );
}

/// `test_resume_bumps_the_loop_generation_no_double_admission`: the loop
/// the pause ended is still inside its notice when the resume lands.
#[tokio::test(start_paused = true)]
async fn a_loop_still_winding_down_from_a_pause_never_runs_beside_the_resumed_one() {
    let fixture = Fixture::new();
    fixture.host.set_child("child-1", Outcome::error("boom"));
    let (gate, entered) = fixture.host.gate("factory.progress", 1);
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [worker("a"), {"id": "b", "subagent": "worker", "depends_on": ["a"]}]}),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    entered.acquire().await.unwrap().forget();
    let resumed = fixture.executor.resume(run_id).await.unwrap();
    gate.add_permits(1);
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            resumed["started"].clone(),
            status["state"].clone(),
            fixture.host.spawns("a").len(),
            fixture.host.spawns("b").len(),
            status["usage"]["spawns"].clone(),
        ),
        (json!(["a"]), json!("done"), 2, 1, json!(3))
    );
}

/// `test_stop_during_output_capture_preserves_cancelled_entry`,
/// `test_stop_during_missing_answer_poll_preserves_cancellation`: the stop
/// lands while the capture reads the settled child again.
#[tokio::test(start_paused = true)]
async fn a_stop_during_the_capture_retry_leaves_the_entry_cancelled_and_unsettled() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::done_without_answer());
    // The loop's collect is the first; the capture's re-read the second.
    let (gate, entered) = fixture.host.gate("rlm.collect", 2);
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [
                {"id": "a", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                {"id": "b", "subagent": "worker", "depends_on": ["a"]},
            ]}),
        )
        .await;
    entered.acquire().await.unwrap().forget();
    let stopped = stopping(&fixture, &started).await.unwrap();
    fixture.host.set_node("a", Outcome::done("late answer"));
    gate.add_permits(1);
    // The capture returns into a stopped run and must settle nothing.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            stopped["cancelled"].clone(),
            node(&status, "a")["status"].clone(),
            node(&status, "b")["entries_used"].clone(),
            events(&status, "transition_fired").len(),
            status["state"].clone(),
        ),
        (
            json!(["a", "b"]),
            json!("cancelled"),
            json!(0),
            0,
            json!("stopped")
        )
    );
}

/// `test_missing_answer_capture_has_a_bounded_poll_window`,
/// `test_capture_waits_for_the_settled_workers_answer_handoff`.
#[tokio::test(start_paused = true)]
async fn the_capture_waits_one_bounded_window_for_a_settled_childs_answer() {
    let mut outcomes = Vec::new();
    for hand_off_after in [Some(Duration::from_secs(1)), None] {
        let fixture = Fixture::new();
        fixture.host.set_node("a", Outcome::done_without_answer());
        let started = fixture
            .start(
                "dag",
                json!({"nodes": [
                    {"id": "a", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                    {
                        "id": "b",
                        "subagent": {"prompt": "Got {seen}."},
                        "inputs": [{"name": "seen", "type": "text", "from": "a.out"}],
                    },
                ]}),
            )
            .await;
        if let Some(delay) = hand_off_after {
            tokio::time::sleep(delay).await;
            fixture.host.set_node("a", Outcome::done("handed off"));
        }
        let before = tokio::time::Instant::now();
        let status = fixture.settle(&started).await;
        outcomes.push((
            status["state"].clone(),
            fixture.host.prompts("b"),
            node(&status, "a")["error"].clone(),
            before.elapsed() <= Duration::from_secs(3),
        ));
    }
    assert_eq!(
        outcomes,
        vec![
            (
                json!("done"),
                vec!["Got handed off.".to_string()],
                Value::Null,
                true
            ),
            (
                json!("paused"),
                Vec::new(),
                json!("output 'out' captured no value from the upstream answer"),
                true,
            ),
        ]
    );
}

/// `test_resume_defers_rate_limited_admissions`: a resume never sleeps in
/// the caller's turn; the loop retries the deferred admission.
#[tokio::test(start_paused = true)]
async fn a_resume_whose_readmission_is_rate_limited_defers_it_to_the_loop() {
    let fixture = Fixture::new();
    fixture.host.set_child("child-1", Outcome::error("boom"));
    // The first admission passes; the resume's and the next are limited.
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let run_id = started["run_id"].as_str().unwrap();
    fixture.settle(&started).await;
    fixture.host.rate_limit("a", 3);
    let resumed = fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            resumed["started"].clone(),
            resumed["state"].clone(),
            events(&status, "spawn_deferred").len(),
            events(&status, "spawn_backoff").len(),
            status["state"].clone(),
        ),
        (json!([]), json!("running"), 1, 1, json!("done"))
    );
}

/// `test_paused_collect_batch_preserves_the_free_infra_respawn`: a worker
/// death verified while a sibling's failure paused the run is still
/// re-spawned for free by the resume.
#[tokio::test(start_paused = true)]
async fn a_worker_death_verified_in_the_batch_that_paused_the_run_is_respawned_on_resume() {
    let fixture = Fixture::new();
    fixture.host.set_child("child-1", Outcome::error("boom"));
    fixture
        .host
        .set_child("child-2", Outcome::error("Child worker unreachable"));
    let started = fixture
        .start("dag", json!({"nodes": [worker("a"), worker("b")]}))
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let paused = fixture
        .until(&started, |graph| !events(graph, "infra_respawn").is_empty())
        .await;
    fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            paused["state"].clone(),
            instance_column(&paused, "b", "status"),
            status["state"].clone(),
            node(&status, "b")["attempts"].clone(),
            events(&status, "retry").len(),
            fixture.host.deleted(),
        ),
        (
            json!("paused"),
            vec![json!("pending")],
            json!("done"),
            json!(2),
            0,
            vec!["child-2".to_string()],
        )
    );
}

/// `test_foreach_children_count_against_the_run_max_children_budget`.
#[tokio::test(start_paused = true)]
async fn foreach_children_count_toward_max_children() {
    let fixture = Fixture::new();
    fixture
        .host
        .set_node("src", Outcome::done("{\"items\": [1, 2, 3, 4, 5]}"));
    let mut dag = fan_dag("continue", 8);
    dag["run"]["max_children"] = json!(3);
    let started = fixture.start("dag", dag).await;
    let run_id = started["run_id"].as_str().unwrap();
    let paused = fixture.settle(&started).await;
    fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            paused["pause_reason"].clone(),
            paused["usage"]["spawns"].clone(),
            status["state"].clone(),
            status["usage"]["spawns"].clone(),
        ),
        (
            json!("max_children exceeded"),
            json!(3),
            json!("done"),
            json!(6)
        )
    );
}

/// `test_machine_loop_back_into_entry_state_binds_null_then_the_real_settle`,
/// `test_long_state_ids_spawn_distinct_sibling_names`,
/// `test_resident_in_flight_stall_fails_the_run_instead_of_wedging`.
#[tokio::test(start_paused = true)]
async fn a_loop_back_into_an_entry_state_rebinds_and_long_ids_and_resident_stalls_are_handled() {
    let fixture = Fixture::new();
    let looped = fixture
        .start(
            "machine",
            json!({
                "states": [
                    {
                        "id": "a", "entry": true, "max_entries": 2,
                        "subagent": {"prompt": "A: {seen}"},
                        "inputs": [{"name": "seen", "type": "text", "from": "b.out", "optional": true}],
                    },
                    {"id": "b", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                ],
                "transitions": [{"from": "a", "to": "b"}, {"from": "b", "to": "a"}],
            }),
        )
        .await;
    let looped = fixture.settle(&looped).await;
    let long = Fixture::new();
    let (first, second) = ("aaaaaaaaaaaaaaaaaaaaaaaa1", "aaaaaaaaaaaaaaaaaaaaaaaa2");
    let named = long
        .start("dag", json!({"nodes": [worker(first), worker(second)]}))
        .await;
    let named = long.settle(&named).await;
    let labels: Vec<Value> = long
        .host
        .calls_of("rlm.run")
        .iter()
        .map(|spawn| spawn["kwargs"]["name"].clone())
        .collect();
    let stalled = Fixture::new();
    stalled.host.set_node("watcher", Outcome::running());
    let wedged = stalled
        .start(
            "machine",
            json!({
                "states": [
                    {"id": "watcher", "entry": true, "subagent": "worker", "lifecycle": "resident"},
                    {"id": "a", "entry": true, "subagent": "worker"},
                    {
                        "id": "b", "subagent": "worker",
                        "inputs": [{"name": "data", "type": "text", "from": "never.out"}],
                    },
                    {"id": "never", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                ],
                "transitions": [{"from": "a", "to": "b"}],
            }),
        )
        .await;
    let wedged = stalled.settle(&wedged).await;
    assert_eq!(
        (
            looped["state"].clone(),
            fixture.host.prompts("a"),
            named["state"].clone(),
            labels.len(),
            labels[0] != labels[1],
            labels.iter().all(|name| name.as_str().is_some_and(|name| name.len() <= 64)),
            wedged["state"].clone(),
            events(&wedged, "executor_error")[0]["error"].clone(),
        ),
        (
            json!("done"),
            vec!["A: None".to_string(), "A: answer-b".to_string()],
            json!("done"),
            2,
            true,
            true,
            json!("failed"),
            json!("control loop stalled: pending entry of state 'b' is waiting for an input source that never settled"),
        )
    );
}
