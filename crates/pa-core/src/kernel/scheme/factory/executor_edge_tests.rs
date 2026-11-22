//! More of the Python kernel's `FactoryExecutorTest` cases: naming, input
//! binding over a failed or running source, resumes, worker deaths, joins,
//! residents, scale, and the capture cap. Each test names the Python tests
//! it carries over (`test_factory.py` of the retired Python runtime).

use serde_json::{json, Value};

use super::executor_tests::{fan_dag, worker};
use super::test_host::{events, instance_column, node, Fixture, Outcome};

/// `test_inline_subagent_name_labels_the_spawned_child`,
/// `test_inline_subagent_name_disambiguates_reentry_and_foreach`,
/// `test_two_runs_of_one_machine_do_not_collide_on_child_names`,
/// `test_machine_self_loop_reenters_until_guard_fails`.
#[tokio::test(start_paused = true)]
async fn a_configured_name_labels_children_per_run_and_a_self_loop_reenters_until_its_guard_fails()
{
    let fixture = Fixture::new();
    for (child, again) in [("child-1", true), ("child-2", true), ("child-3", false)] {
        fixture.host.set_child(
            child,
            Outcome::done(&format!("{{\"flag\": {{\"again\": {again}}}}}")),
        );
    }
    // The second run's one child stops the loop at once.
    fixture
        .host
        .set_node("scout", Outcome::done("{\"flag\": {\"again\": false}}"));
    let machine = json!({
        "states": [{
            "id": "a",
            "entry": true,
            "subagent": {"prompt": "Go.", "name": "scout"},
            "outputs": [{"name": "flag", "type": "json"}],
            "max_entries": 5,
        }],
        "transitions": [
            {"from": "a", "to": "a", "when": {"output": "flag", "path": "again", "op": "eq", "value": true}},
        ],
    });
    let first = fixture.start("machine", machine).await;
    let status = fixture.settle(&first).await;
    // The first run's children stay registered with the supervisor.
    let second = fixture.executor.run("sw", None).await.unwrap();
    let second_status = fixture.settle(&second).await;
    let run6 = &first["run_id"].as_str().unwrap()[..6];
    let names: Vec<Value> = fixture
        .host
        .spawns("scout")
        .iter()
        .take(3)
        .map(|spawn| spawn["kwargs"]["name"].clone())
        .collect();
    assert_eq!(
        (
            status["state"].clone(),
            names,
            node(&status, "a")["entries_used"].clone(),
            status["usage"]["transitions_fired"].clone(),
            second_status["state"].clone(),
            fixture.host.spawns("scout").len(),
        ),
        (
            json!("done"),
            vec![
                json!(format!("{run6}-scout")),
                json!(format!("{run6}-scout-i1")),
                json!(format!("{run6}-scout-i2")),
            ],
            json!(3),
            json!(2),
            json!("done"),
            4,
        )
    );
}

/// `test_machine_optional_input_over_an_errored_source_binds_the_null_sentinel`,
/// `test_machine_optional_foreach_over_an_errored_source_expands_empty`,
/// `test_machine_required_input_over_an_errored_source_fails_the_dependent_entry`.
#[tokio::test(start_paused = true)]
async fn over_a_failed_source_an_optional_input_binds_nothing_and_a_required_one_fails_its_entry() {
    let fixture = Fixture::new();
    fixture.host.set_node("src", Outcome::error("boom"));
    let input = |kind: &str, optional: bool| json!({"name": "seen", "type": kind, "from": format!("src.{kind}"), "optional": optional});
    let started = fixture
        .start(
            "machine",
            json!({
                "run": {"failure_policy": "continue"},
                "states": [
                    {
                        "id": "src", "entry": true, "subagent": "worker",
                        "outputs": [{"name": "text", "type": "text"}, {"name": "json", "type": "json"}],
                    },
                    {"id": "reader", "subagent": {"prompt": "Read: {seen}"}, "inputs": [input("text", true)]},
                    {
                        "id": "fan", "subagent": {"prompt": "Item {seen}"},
                        "inputs": [input("json", true)], "foreach": {"over": "seen", "max": 4},
                    },
                    {"id": "strict", "subagent": {"prompt": "Need {seen}"}, "inputs": [input("text", false)]},
                ],
                "transitions": [
                    {"from": "src", "to": "reader"},
                    {"from": "src", "to": "fan"},
                    {"from": "src", "to": "strict"},
                ],
            }),
        )
        .await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["state"].clone(),
            fixture.host.prompts("reader"),
            node(&status, "fan")["status"].clone(),
            fixture.host.spawns("fan").len(),
            node(&status, "strict")["error"].clone(),
            fixture.host.spawns("strict").len(),
        ),
        (
            json!("failed"),
            vec!["Read: None".to_string()],
            json!("done"),
            0,
            json!("input 'seen' from state 'src' is unavailable (latest settle status 'error')"),
            0,
        )
    );
}

/// `test_optional_input_waits_for_a_live_upstream_before_spawning`.
#[tokio::test(start_paused = true)]
async fn an_optional_input_waits_while_its_source_is_still_running() {
    let fixture = Fixture::new();
    fixture.host.set_child("child-2", Outcome::running());
    let started = fixture
        .start(
            "machine",
            json!({
                "states": [
                    {"id": "a", "entry": true, "subagent": "worker"},
                    {"id": "b", "entry": true, "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                    {
                        "id": "c", "subagent": {"prompt": "See: {seen}"},
                        "inputs": [{"name": "seen", "type": "text", "from": "b.out", "optional": true}],
                    },
                ],
                "transitions": [{"from": "a", "to": "c"}],
            }),
        )
        .await;
    let entered = fixture
        .until(&started, |graph| node(graph, "c")["entries_used"] == 1)
        .await;
    let while_b_runs = (
        node(&entered, "c")["status"].clone(),
        fixture.host.spawns("c").len(),
    );
    fixture.host.set_child("child-2", Outcome::done("B"));
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            while_b_runs,
            status["state"].clone(),
            fixture.host.prompts("c")
        ),
        (
            (json!("pending"), 0),
            json!("done"),
            vec!["See: B".to_string()]
        )
    );
}

/// `test_resume_grants_fresh_retries_and_a_fresh_respawn`.
#[tokio::test(start_paused = true)]
async fn a_resume_restarts_the_retry_budget_of_the_entry_it_readmits() {
    let fixture = Fixture::new();
    for failing in ["child-1", "child-2", "child-3"] {
        fixture.host.set_child(failing, Outcome::error("boom"));
    }
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [{"id": "a", "subagent": "worker", "retries": 1}]}),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let paused = fixture.settle(&started).await;
    fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    let last_name = fixture.host.spawns("a")[3]["kwargs"]["name"].clone();
    assert_eq!(
        (
            node(&paused, "a")["attempts"].clone(),
            status["state"].clone(),
            node(&status, "a")["attempts"].clone(),
            node(&status, "a")["entries_used"].clone(),
            events(&status, "retry").len(),
            last_name,
        ),
        (
            json!(2),
            json!("done"),
            json!(4),
            json!(1),
            2,
            json!(format!("sw-a-{}-i0-a4", &run_id[..6])),
        )
    );
}

/// `test_worker_death_recheck_adopts_a_live_settle`: the re-read after the
/// quiet period finds the child settled, so nothing is respawned.
#[tokio::test(start_paused = true)]
async fn a_worker_reported_dead_that_settles_before_the_recheck_is_not_respawned() {
    let fixture = Fixture::new();
    fixture
        .host
        .set_child_first("child-1", Outcome::error("Child worker unreachable"));
    fixture
        .host
        .set_child("child-1", Outcome::done("recovered"));
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["state"].clone(),
            node(&status, "a")["attempts"].clone(),
            node(&status, "a")["answer_preview"].clone(),
            events(&status, "infra_respawn").len(),
            fixture.host.deleted().len(),
        ),
        (json!("done"), json!(1), json!("recovered"), 0, 0)
    );
}

/// `test_late_sibling_exit_after_terminal_entry_still_marks_needs_verify`.
#[tokio::test(start_paused = true)]
async fn a_sibling_that_exits_with_text_after_its_entry_failed_still_marks_it_to_verify() {
    let fixture = Fixture::new();
    fixture
        .host
        .set_node("src", Outcome::done("{\"items\": [1, 2, 3]}"));
    fixture.host.set_child("child-2", Outcome::error("boom"));
    fixture
        .host
        .set_child("child-3", Outcome::error_with_answer("exited", "half done"));
    let started = fixture.start("dag", fan_dag("continue", 8)).await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["needs_verify"].clone(),
            node(&status, "fan")["entries"][0]["provisional_answer"].clone(),
            node(&status, "fan")["error"].clone(),
            events(&status, "needs_verify").len(),
            instance_column(&status, "fan", "status"),
        ),
        (
            json!(["fan"]),
            json!("half done"),
            json!("boom"),
            1,
            vec![json!("error"), json!("error"), json!("done")],
        )
    );
}

/// `test_machine_join_paused_at_max_transitions_fires_after_resume`: the
/// pause leaves no mark on the join, so the resume still fires it, once.
#[tokio::test(start_paused = true)]
async fn a_join_the_max_transitions_pause_held_back_fires_once_after_the_resume() {
    let fixture = Fixture::new();
    let started = fixture
        .start(
            "machine",
            json!({
                "run": {"max_transitions": 1},
                "states": [
                    {"id": "a", "entry": true, "subagent": "worker"},
                    {"id": "b", "entry": true, "subagent": "worker"},
                    worker("c"),
                    worker("d"),
                ],
                "transitions": [{"from": "a", "to": "d"}, {"from": ["a", "b"], "to": "c"}],
            }),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let paused = fixture.settle(&started).await;
    fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            paused["state"].clone(),
            node(&paused, "c")["entries_used"].clone(),
            status["state"].clone(),
            fixture.host.spawns("c").len(),
            fixture.host.spawns("d").len(),
            status["usage"]["transitions_fired"].clone(),
        ),
        (json!("paused"), json!(0), json!("done"), 1, 1, json!(2))
    );
}

/// `test_resident_queued_instance_is_admitted_before_completion`.
#[tokio::test(start_paused = true)]
async fn a_resident_queued_behind_the_parallel_cap_is_admitted_before_the_run_ends() {
    let fixture = Fixture::new();
    fixture.host.set_node("watcher", Outcome::running());
    let started = fixture
        .start(
            "machine",
            json!({
                "run": {"max_parallel": 1},
                "states": [
                    {"id": "work", "entry": true, "subagent": "worker"},
                    {"id": "watcher", "entry": true, "subagent": "worker", "lifecycle": "resident"},
                ],
            }),
        )
        .await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            started["started"].clone(),
            status["state"].clone(),
            fixture.host.spawns("watcher").len(),
            instance_column(&status, "watcher", "status"),
        ),
        (json!(["work"]), json!("done"), 1, vec![json!("running")])
    );
}

/// `test_scale_chain_100_completes`, `test_scale_fan_1000_completes`.
#[tokio::test(start_paused = true)]
async fn a_chain_of_a_hundred_and_a_fan_of_a_thousand_states_run_to_the_end() {
    let chain: Vec<Value> = (0..100)
        .map(|index| match index {
            0 => worker("n0"),
            _ => json!({"id": format!("n{index}"), "subagent": "worker", "depends_on": [format!("n{}", index - 1)]}),
        })
        .collect();
    let mut fan: Vec<Value> = vec![worker("root")];
    fan.extend((0..999).map(
        |index| json!({"id": format!("leaf{index}"), "subagent": "worker", "depends_on": ["root"]}),
    ));
    let mut outcomes = Vec::new();
    for (nodes, max_parallel) in [(chain, 8), (fan, 64)] {
        let fixture = Fixture::new();
        let started = fixture
            .start(
                "dag",
                json!({"run": {"max_parallel": max_parallel}, "nodes": nodes}),
            )
            .await;
        let status = fixture.settle(&started).await;
        outcomes.push((
            status["state"].clone(),
            status["usage"]["spawns"].clone(),
            status["usage"]["settled"].clone(),
        ));
    }
    assert_eq!(
        outcomes,
        vec![
            (json!("done"), json!(100), json!(100)),
            (json!("done"), json!(1000), json!(1000)),
        ]
    );
}

/// `test_answer_capture_cap_slices_previews`,
/// `test_a_fenced_json_output_at_the_binding_cap_binds`,
/// `test_capture_failure_names_the_binding_cap_and_records_the_event`,
/// `test_json_output_longer_than_the_preview_cap_binds_from_the_full_answer`.
#[tokio::test(start_paused = true)]
async fn an_answer_binds_whole_up_to_the_capture_cap_and_one_cut_by_it_fails_naming_the_cap() {
    let mut outcomes = Vec::new();
    for payload_length in [60_000, 70_000] {
        let fixture = Fixture::new();
        let answer = format!(
            "```json\n{{\"result\": \"{}\"}}\n```",
            "x".repeat(payload_length)
        );
        fixture.host.set_node("a", Outcome::done(&answer));
        let started = fixture
            .start(
                "dag",
                json!({
                    "run": {"failure_policy": "continue"},
                    "nodes": [
                        {"id": "a", "subagent": "worker", "outputs": [{"name": "result", "type": "json"}]},
                        {
                            "id": "b",
                            "subagent": {"prompt": "{data}"},
                            "inputs": [{"name": "data", "type": "json", "from": "a.result"}],
                        },
                    ],
                }),
            )
            .await;
        let status = fixture.settle(&started).await;
        outcomes.push((
            status["state"].clone(),
            fixture
                .host
                .prompts("b")
                .iter()
                .map(String::len)
                .collect::<Vec<_>>(),
            node(&status, "a")["error"].clone(),
            events(&status, "answer_captured")[0]["answer"]
                .as_str()
                .map(str::len),
        ));
    }
    assert_eq!(
        outcomes,
        vec![
            (json!("done"), vec![60_002], Value::Null, Some(200)),
            (
                json!("failed"),
                Vec::new(),
                json!("no JSON object containing output 'result' in the upstream answer; the captured answer is truncated at 65536 characters - keep the fenced JSON block compact"),
                Some(200),
            ),
        ]
    );
}
