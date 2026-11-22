//! The executor against a scripted supervisor: the cases of the Python
//! kernel's `FactoryExecutorTest` (`test_factory.py` of the retired Python runtime),
//! each named in its test's comment. Time is tokio's paused clock, so a
//! poll, a backoff or a budget passes without a real wait.

use std::time::Duration;

use serde_json::{json, Value};

use super::test_host::{events, instance_column, node, Fixture, Outcome};
use crate::refinement::RefinementKind;

pub(super) fn worker(id: &str) -> Value {
    json!({"id": id, "subagent": "worker"})
}

/// A source whose JSON list fans out into one `fan` child per item.
pub(super) fn fan_dag(policy: &str, max_parallel: u64) -> Value {
    json!({
        "run": {"failure_policy": policy, "max_parallel": max_parallel},
        "nodes": [
            {"id": "src", "subagent": "worker", "outputs": [{"name": "items", "type": "json"}]},
            {
                "id": "fan",
                "subagent": {"prompt": "Expand item {items}."},
                "depends_on": ["src"],
                "inputs": [{"name": "items", "type": "json", "from": "src.items"}],
                "foreach": {"over": "items", "max": 5},
            },
        ],
    })
}

/// `test_run_starts_ready_nodes_and_reports_counts`.
#[tokio::test(start_paused = true)]
async fn run_admits_the_entry_states_up_to_max_parallel_and_the_loop_finishes_the_rest() {
    let fixture = Fixture::new();
    let mut started = fixture
        .start(
            "dag",
            json!({
                "run": {"max_parallel": 2},
                "nodes": [
                    worker("a"),
                    {"id": "b", "subagent": "worker", "depends_on": ["a"]},
                    {"id": "c", "subagent": "worker", "depends_on": ["b"]},
                    worker("d"),
                ],
            }),
        )
        .await;
    let admitted_at_once = fixture.host.calls_of("rlm.run").len();
    let status = fixture.settle(&started).await;
    started["run_id"] = json!("the run");
    let statuses: Vec<&str> = status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        (
            started,
            admitted_at_once,
            status["state"].clone(),
            statuses,
            fixture.host.notice_kinds(),
            status["usage"]["spawns"].clone(),
            status["usage"]["settled"].clone(),
        ),
        (
            json!({
                "run_id": "the run",
                "spec_id": "sw",
                "name": null,
                "nodes": 4,
                "max_parallel": 2,
                "started": ["a", "d"],
                "pending": ["b", "c"],
            }),
            2,
            json!("done"),
            vec!["done"; 4],
            vec!["finished".to_string()],
            json!(4),
            json!(4),
        )
    );
}

/// `test_resolves_subagent_by_id_and_title_with_model_settings`.
#[tokio::test(start_paused = true)]
async fn a_subagent_reference_resolves_by_id_or_title_and_carries_its_spawn_settings() {
    let fixture = Fixture::new();
    fixture.entry(
        RefinementKind::Subagent,
        "worker-md",
        "The Worker",
        "Template by title.",
        json!({}),
        json!({"model": "p/test-model", "thinking": "low"}),
    );
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [
                {"id": "x", "subagent": "The Worker"},
                {"id": "y", "subagent": "worker-md"},
            ]}),
        )
        .await;
    fixture.settle(&started).await;
    let spawned = |state: &str| {
        let mut spawns = fixture.host.spawns(state);
        let spawn = spawns.remove(0);
        (
            spawns.len(),
            spawn["prompt"].clone(),
            spawn["kwargs"]["model"].clone(),
            spawn["kwargs"]["thinking"].clone(),
            spawn["kwargs"]["factory"].clone(),
        )
    };
    let expected = |state: &str| {
        (
            0,
            json!("Template by title."),
            json!("p/test-model"),
            json!("low"),
            json!({"runId": started["run_id"], "specId": "sw", "stateId": state}),
        )
    };
    assert_eq!((spawned("x"), spawned("y")), (expected("x"), expected("y")));
}

/// `test_run_rejects_invalid_dag_and_starts_nothing`,
/// `test_run_lists_all_missing_subagent_references`,
/// `test_run_rejects_unknown_spec`.
#[tokio::test(start_paused = true)]
async fn run_validates_again_resolves_every_reference_and_starts_nothing_on_a_failure() {
    let fixture = Fixture::new();
    // A hand-edited store: nothing validated these at write time.
    fixture.store("empty", "dag", json!({"nodes": []}));
    fixture.store(
        "cyclic",
        "dag",
        json!({"nodes": [
            {"id": "a", "subagent": "worker", "depends_on": ["b"]},
            {"id": "b", "subagent": "worker", "depends_on": ["a"]},
        ]}),
    );
    fixture.store(
        "ghosts",
        "dag",
        json!({"nodes": [{"id": "a", "subagent": "ghost-a"}, {"id": "b", "subagent": "ghost-b"}]}),
    );
    let mut refusals = Vec::new();
    for spec_id in ["empty", "cyclic", "ghosts", "missing-spec"] {
        refusals.push(
            fixture
                .executor
                .run(spec_id, None)
                .await
                .unwrap_err()
                .to_string(),
        );
    }
    assert_eq!(
        (refusals, fixture.host.calls_of("rlm.run").len()),
        (
            vec![
                "factory dag must declare between 1 and 1024 nodes, got 0".to_string(),
                "factory machine requires at least one entry state".to_string(),
                "state 'a' references unknown subagent 'ghost-a'; state 'b' references unknown subagent 'ghost-b'".to_string(),
                "unknown factory spec 'missing-spec': no stored factory entry and no library machine with that name (unknown machine 'missing-spec': no MACHINE.md for it in the machine library (machines: builder, pr-manager, review-sweep))".to_string(),
            ],
            0,
        )
    );
}

/// `test_propagation_binds_answer_into_prompt`,
/// `test_unplaced_inputs_are_appended`.
#[tokio::test(start_paused = true)]
async fn a_settled_answer_renders_into_the_dependents_prompt_or_its_inputs_section() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::done("ANSWER-A"));
    let started = fixture
        .start(
            "dag",
            json!({
                "run": {"failure_policy": "continue"},
                "nodes": [
                    {"id": "a", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                    {
                        "id": "b",
                        "subagent": {"prompt": "Summarize: {draft}"},
                        "inputs": [{"name": "draft", "type": "text", "from": "a.out"}],
                    },
                    {
                        "id": "c",
                        "subagent": {"prompt": "No placeholders here."},
                        "inputs": [{"name": "draft", "type": "text", "from": "a.out"}],
                    },
                ],
            }),
        )
        .await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["state"].clone(),
            fixture.host.prompts("b"),
            fixture.host.prompts("c"),
            node(&status, "b")["answer_preview"].clone(),
        ),
        (
            json!("done"),
            vec!["Summarize: ANSWER-A".to_string()],
            vec!["No placeholders here.\n\n## Inputs\n- draft: ANSWER-A\n".to_string()],
            json!("answer-b"),
        )
    );
}

/// `test_two_parent_fan_in_binds_both_answers`: both parents settle in one
/// collect batch and the join fires once.
#[tokio::test(start_paused = true)]
async fn a_fan_in_fires_once_when_both_parents_settle_in_one_batch() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::done("ANSWER-A"));
    fixture.host.set_node("b", Outcome::done("ANSWER-B"));
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [
                {"id": "a", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                {"id": "b", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                {
                    "id": "c",
                    "subagent": {"prompt": "Combine {left} and {right}."},
                    "depends_on": ["a", "b"],
                    "inputs": [
                        {"name": "left", "type": "text", "from": "a.out"},
                        {"name": "right", "type": "text", "from": "b.out"},
                    ],
                },
            ]}),
        )
        .await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            started["started"].clone(),
            status["state"].clone(),
            fixture.host.prompts("c"),
            node(&status, "c")["entries_used"].clone(),
            events(&status, "transition_blocked").len(),
        ),
        (
            json!(["a", "b"]),
            json!("done"),
            vec!["Combine ANSWER-A and ANSWER-B.".to_string()],
            json!(1),
            0,
        )
    );
}

/// `test_fan_in_waits_for_every_parent_across_batches`: a control-only
/// parent holds the fan-in node back too.
#[tokio::test(start_paused = true)]
async fn a_fan_in_waits_for_a_parent_that_settles_in_a_later_batch() {
    let fixture = Fixture::new();
    fixture.host.set_child("child-1", Outcome::done("go"));
    fixture.host.set_child("child-2", Outcome::done("ANSWER-B"));
    fixture.host.set_child("child-3", Outcome::running());
    let started = fixture
        .start(
            "dag",
            json!({
                "run": {"failure_policy": "continue"},
                "nodes": [
                    worker("a"),
                    {"id": "b", "subagent": "worker", "depends_on": ["a"], "outputs": [{"name": "out", "type": "text"}]},
                    {"id": "c", "subagent": "worker", "depends_on": ["a"]},
                    {
                        "id": "d",
                        "subagent": {"prompt": "Combine {left}."},
                        "depends_on": ["b", "c"],
                        "inputs": [{"name": "left", "type": "text", "from": "b.out"}],
                    },
                ],
            }),
        )
        .await;
    let waiting = fixture
        .until(&started, |graph| node(graph, "b")["status"] == "done")
        .await;
    let entered_early = (
        fixture.host.spawns("d").len(),
        node(&waiting, "d")["entries_used"].clone(),
    );
    fixture.host.set_child("child-3", Outcome::done("ANSWER-C"));
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            entered_early,
            status["state"].clone(),
            fixture.host.prompts("d"),
            node(&status, "d")["entries_used"].clone(),
        ),
        (
            (0, json!(0)),
            json!("done"),
            vec!["Combine ANSWER-B.".to_string()],
            json!(1),
        )
    );
}

/// `test_json_input_prefers_fenced_block`,
/// `test_json_input_falls_back_to_whole_text`,
/// `test_a_json_null_output_is_a_captured_value_not_a_failure`.
#[tokio::test(start_paused = true)]
async fn a_json_output_binds_from_a_fenced_block_or_the_whole_answer_and_null_is_a_value() {
    let mut prompts = Vec::new();
    for answer in [
        "Verdict text.\n```json\n{\"result\": {\"x\": 1}}\n```",
        "{\"result\": [1, 2]}",
        "```json\n{\"result\": null}\n```",
    ] {
        let fixture = Fixture::new();
        fixture.host.set_node("a", Outcome::done(answer));
        let started = fixture
            .start(
                "dag",
                json!({"nodes": [
                    {"id": "a", "subagent": "worker", "outputs": [{"name": "result", "type": "json"}]},
                    {
                        "id": "b",
                        "subagent": {"prompt": "Process {data}."},
                        "inputs": [{"name": "data", "type": "json", "from": "a.result"}],
                    },
                ]}),
            )
            .await;
        let status = fixture.settle(&started).await;
        prompts.push((status["state"].clone(), fixture.host.prompts("b")));
    }
    assert_eq!(
        prompts,
        vec![
            (json!("done"), vec!["Process {\"x\": 1}.".to_string()]),
            (json!("done"), vec!["Process [1, 2].".to_string()]),
            (json!("done"), vec!["Process null.".to_string()]),
        ]
    );
}

/// `test_unparseable_output_fails_the_producer_and_never_spawns_the_dependent`,
/// `test_uncaptured_output_fails_the_producing_entry_instead_of_the_dependent`.
#[tokio::test(start_paused = true)]
async fn an_output_that_never_binds_fails_its_producer_and_the_dependent_is_never_spawned() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::done("not json at all"));
    let started = fixture
        .start(
            "dag",
            json!({
                "run": {"failure_policy": "continue"},
                "nodes": [
                    {"id": "a", "subagent": "worker", "outputs": [{"name": "result", "type": "json"}]},
                    {
                        "id": "b",
                        "subagent": {"prompt": "Process {data}."},
                        "inputs": [{"name": "data", "type": "json", "from": "a.result"}],
                    },
                    worker("c"),
                ],
            }),
        )
        .await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["state"].clone(),
            fixture.host.spawns("b").len(),
            node(&status, "a")["error"].clone(),
            node(&status, "b")["error"].clone(),
            node(&status, "c")["status"].clone(),
            status["needs_verify"].clone(),
            node(&status, "a")["entries"][0]["provisional_answer"].clone(),
            events(&status, "output_capture_failed").len(),
        ),
        (
            json!("failed"),
            0,
            json!("no JSON object containing output 'result' in the upstream answer"),
            json!("input 'data' from state 'a' is unavailable (latest settle status 'error')"),
            json!("done"),
            json!(["a"]),
            json!("not json at all"),
            1,
        )
    );
}

/// `test_capture_retry_rebinds_a_settle_that_raced_the_answer_capture`:
/// the first collect settles the child without its answer.
#[tokio::test(start_paused = true)]
async fn a_settle_that_raced_its_answer_hand_off_is_captured_on_the_retry() {
    let fixture = Fixture::new();
    fixture
        .host
        .set_child_first("child-1", Outcome::done_without_answer());
    fixture.host.set_node("a", Outcome::done("{\"result\": 7}"));
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [
                {"id": "a", "subagent": "worker", "outputs": [{"name": "result", "type": "json"}]},
                {
                    "id": "b",
                    "subagent": {"prompt": "Got {data}."},
                    "inputs": [{"name": "data", "type": "json", "from": "a.result"}],
                },
            ]}),
        )
        .await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (status["state"].clone(), fixture.host.prompts("b")),
        (json!("done"), vec!["Got 7.".to_string()])
    );
}

/// `test_foreach_expands_clamped_instances`,
/// `test_foreach_zero_items_marks_node_done`.
#[tokio::test(start_paused = true)]
async fn foreach_runs_one_child_per_item_up_to_its_max_and_none_for_an_empty_list() {
    let mut outcomes = Vec::new();
    for answer in [
        "Here.\n```json\n{\"items\": [1, 2, 3, 4, 5, 6, 7]}\n```",
        "{\"items\": []}",
    ] {
        let fixture = Fixture::new();
        fixture.host.set_node("src", Outcome::done(answer));
        let started = fixture.start("dag", fan_dag("continue", 8)).await;
        let status = fixture.settle(&started).await;
        let mut prompts = fixture.host.prompts("fan");
        prompts.sort();
        outcomes.push((
            status["state"].clone(),
            node(&status, "fan")["status"].clone(),
            prompts,
        ));
    }
    let expanded: Vec<String> = (1..=5).map(|item| format!("Expand item {item}.")).collect();
    assert_eq!(
        outcomes,
        vec![
            (json!("done"), json!("done"), expanded),
            (json!("done"), json!("done"), Vec::new()),
        ]
    );
}

/// `test_foreach_mixed_instances_escalate_pauses`,
/// `test_foreach_mixed_instances_fail_fast_cancels_siblings`,
/// `test_foreach_mixed_instances_continue_finishes_with_node_error`.
#[tokio::test(start_paused = true)]
async fn a_failed_foreach_instance_fails_its_entry_under_each_failure_policy() {
    let mut outcomes = Vec::new();
    for (policy, siblings) in [
        ("escalate", Outcome::done("fine")),
        ("fail_fast", Outcome::running()),
        ("continue", Outcome::done("fine")),
    ] {
        let fixture = Fixture::new();
        fixture
            .host
            .set_node("src", Outcome::done("{\"items\": [1, 2, 3, 4, 5]}"));
        // src is child-1; the five fan instances are child-2 to child-6.
        fixture.host.set_child("child-2", Outcome::error("boom-1"));
        for sibling in ["child-3", "child-4", "child-5", "child-6"] {
            fixture.host.set_child(sibling, siblings.clone());
        }
        let started = fixture.start("dag", fan_dag(policy, 8)).await;
        let status = fixture.settle(&started).await;
        outcomes.push((
            status["state"].clone(),
            fixture.host.notice_kinds(),
            node(&status, "fan")["status"].clone(),
            instance_column(&status, "fan", "status"),
            fixture.host.deleted().len(),
        ));
    }
    let settled = vec![
        json!("error"),
        json!("done"),
        json!("done"),
        json!("done"),
        json!("done"),
    ];
    assert_eq!(
        outcomes,
        vec![
            (
                json!("paused"),
                vec!["paused".to_string()],
                json!("error"),
                settled.clone(),
                0
            ),
            (
                json!("failed"),
                vec!["failed".to_string()],
                json!("error"),
                vec![
                    json!("error"),
                    json!("cancelled"),
                    json!("cancelled"),
                    json!("cancelled"),
                    json!("cancelled"),
                ],
                4,
            ),
            (
                json!("failed"),
                vec!["failed".to_string()],
                json!("error"),
                settled,
                0
            ),
        ]
    );
}

/// `test_foreach_failure_never_admits_queued_siblings_and_waits_for_running_ones`,
/// `test_graph_keeps_a_failed_foreach_entrys_stage_active_while_siblings_run`,
/// `test_graph_nodes_carry_per_stage_agent_counts`.
#[tokio::test(start_paused = true)]
async fn a_failed_foreach_entry_never_admits_its_queue_and_the_run_waits_for_running_siblings() {
    let fixture = Fixture::new();
    fixture.host.set_node(
        "src",
        Outcome::done("{\"items\": [\"a\", \"b\", \"c\", \"d\", \"e\"]}"),
    );
    // max_parallel 2 admits child-2 and child-3 first; three stay queued.
    fixture.host.set_child("child-2", Outcome::error("boom"));
    fixture.host.set_child("child-3", Outcome::running());
    let started = fixture.start("dag", fan_dag("continue", 2)).await;
    let failed = fixture
        .until(&started, |graph| node(graph, "fan")["status"] == "error")
        .await;
    let while_sibling_runs = (
        failed["state"].clone(),
        fixture.host.spawns("fan").len(),
        failed["active_nodes"].clone(),
        node(&failed, "fan")["running"].clone(),
        node(&failed, "fan")["queued"].clone(),
    );
    fixture.host.set_child("child-3", Outcome::done("ok"));
    let status = fixture.settle(&started).await;
    let cancelled: Vec<Value> = events(&status, "cancelled")
        .iter()
        .filter(|event| event["node"] == "fan")
        .map(|event| event["detail"].clone())
        .collect();
    assert_eq!(
        (
            while_sibling_runs,
            status["state"].clone(),
            instance_column(&status, "fan", "status"),
            fixture.host.spawns("fan").len(),
            fixture.host.deleted().len(),
            cancelled,
        ),
        (
            (json!("running"), 2, json!(["fan"]), json!(1), json!(0)),
            json!("failed"),
            vec![
                json!("error"),
                json!("done"),
                json!("cancelled"),
                json!("cancelled"),
                json!("cancelled"),
            ],
            2,
            0,
            vec![json!("entry failed before admission"); 3],
        )
    );
}

/// `test_retries_respawn_until_attempts_exhausted`,
/// `test_paused_run_status_carries_the_last_error_and_remedy`.
#[tokio::test(start_paused = true)]
async fn a_failing_child_is_respawned_for_each_retry_and_then_the_run_pauses_with_the_remedy() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::error("boom"));
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [{"id": "a", "subagent": "worker", "retries": 2}]}),
        )
        .await;
    let status = fixture.settle(&started).await;
    let run_id = started["run_id"].as_str().unwrap();
    let run6 = &run_id[..6];
    let names: Vec<Value> = fixture
        .host
        .spawns("a")
        .iter()
        .map(|spawn| spawn["kwargs"]["name"].clone())
        .collect();
    assert_eq!(
        (
            status["state"].clone(),
            names,
            node(&status, "a")["attempts"].clone(),
            status["pause_reason"].clone(),
            status["last_error"].clone(),
            status["remedy"].clone(),
            fixture.host.notice_kinds(),
        ),
        (
            json!("paused"),
            vec![
                json!(format!("sw-a-{run6}-i0")),
                json!(format!("sw-a-{run6}-i0-a2")),
                json!(format!("sw-a-{run6}-i0-a3")),
            ],
            json!(3),
            json!("boom"),
            json!("state 'a' failed: boom"),
            json!(format!(
                "resume with (factory-resume \"{run_id}\") to re-admit the failed state's entry as a fresh attempt (same rendered prompt, no extra max_entries); author the state with failure_policy 'continue' to run its dependents past a failure instead"
            )),
            vec!["paused".to_string()],
        )
    );
}

/// `test_escalate_resume_readmits_the_failed_entry`,
/// `test_escalate_pause_spends_no_downstream_entry`,
/// `test_a_pause_after_resume_notifies_the_parent_again`.
#[tokio::test(start_paused = true)]
async fn resume_readmits_the_entry_an_escalate_pause_held_and_each_pause_notifies_again() {
    let fixture = Fixture::new();
    fixture.host.set_child("child-1", Outcome::error("boom"));
    fixture
        .host
        .set_child("child-2", Outcome::error("boom again"));
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [worker("a"), {"id": "b", "subagent": "worker", "depends_on": ["a"]}]}),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let paused = fixture.settle(&started).await;
    let held = (
        paused["state"].clone(),
        node(&paused, "b")["entries_used"].clone(),
        events(&paused, "transition_fired").len(),
    );
    let mut resumed = fixture.executor.resume(run_id).await.unwrap();
    resumed["run_id"] = json!("the run");
    let paused_again = fixture.settle(&started).await;
    fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    let refused = fixture
        .executor
        .resume(run_id)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        (
            held,
            resumed,
            paused_again["pause_reason"].clone(),
            status["state"].clone(),
            node(&status, "a")["entries_used"].clone(),
            node(&status, "a")["attempts"].clone(),
            events(&status, "readmitted").len(),
            fixture.host.notice_kinds(),
            refused,
        ),
        (
            (json!("paused"), json!(0), 0),
            json!({"run_id": "the run", "state": "running", "started": ["a"], "pending": ["b"]}),
            json!("boom again"),
            json!("done"),
            json!(1),
            json!(3),
            2,
            vec![
                "paused".to_string(),
                "paused".to_string(),
                "finished".to_string()
            ],
            format!("factory run '{run_id}' is 'done', not paused"),
        )
    );
}

/// `test_fail_fast_cancels_running_children`,
/// `test_continue_policy_finishes_remaining_nodes`.
#[tokio::test(start_paused = true)]
async fn fail_fast_cancels_what_runs_while_continue_lets_the_other_states_finish() {
    let mut outcomes = Vec::new();
    for policy in ["fail_fast", "continue"] {
        let fixture = Fixture::new();
        fixture.host.set_node("a", Outcome::error("boom"));
        fixture.host.set_child("child-2", Outcome::running());
        let started = fixture
            .start(
                "dag",
                json!({
                    "run": {"failure_policy": policy},
                    "nodes": [worker("a"), worker("b"), {"id": "c", "subagent": "worker", "depends_on": ["b"]}],
                }),
            )
            .await;
        if policy == "continue" {
            fixture
                .until(&started, |graph| node(graph, "a")["status"] == "error")
                .await;
            fixture.host.set_child("child-2", Outcome::done("fine"));
        }
        let status = fixture.settle(&started).await;
        let statuses: Vec<Value> = ["a", "b", "c"]
            .iter()
            .map(|id| node(&status, id)["status"].clone())
            .collect();
        outcomes.push((
            status["state"].clone(),
            statuses,
            fixture.host.deleted(),
            fixture.host.notices()[0]["detail"].clone(),
        ));
    }
    assert_eq!(
        outcomes,
        vec![
            (
                json!("failed"),
                vec![json!("error"), json!("cancelled"), json!("cancelled")],
                vec!["child-2".to_string()],
                json!("state a failed: boom; cancelled 1 in-flight child(ren)"),
            ),
            (
                json!("failed"),
                vec![json!("error"), json!("done"), json!("done")],
                Vec::new(),
                json!("completed with state error(s): a"),
            ),
        ]
    );
}

/// `test_stop_cancels_children_and_pending_nodes`,
/// `test_concurrent_stop_runs_one_cancellation_pass` (the idempotent half).
#[tokio::test(start_paused = true)]
async fn stop_deletes_the_running_children_cancels_the_rest_and_a_second_stop_adds_nothing() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::running());
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [worker("a"), {"id": "b", "subagent": "worker", "depends_on": ["a"]}]}),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let mut stopped = fixture.executor.stop(run_id).await.unwrap();
    let mut again = fixture.executor.stop(run_id).await.unwrap();
    stopped["run_id"] = json!("the run");
    again["run_id"] = json!("the run");
    let status = fixture.executor.status(run_id).unwrap();
    assert_eq!(
        (
            stopped,
            again,
            fixture.host.deleted(),
            node(&status, "a")["status"].clone(),
            node(&status, "b")["status"].clone(),
            events(&status, "run_stopped").len(),
            status["state"].clone(),
        ),
        (
            json!({"run_id": "the run", "state": "stopped", "cancelled": ["a", "b"]}),
            json!({"run_id": "the run", "state": "stopped", "cancelled": []}),
            vec!["child-1".to_string()],
            json!("cancelled"),
            json!("cancelled"),
            1,
            json!("stopped"),
        )
    );
}

/// `test_failed_delete_records_cancel_failed_and_cancelled`.
#[tokio::test(start_paused = true)]
async fn a_delete_the_host_refuses_is_recorded_beside_the_cancellation() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::running());
    fixture.host.fail("rlm.delete_subagent");
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let run_id = started["run_id"].as_str().unwrap();
    fixture.executor.stop(run_id).await.unwrap();
    let status = fixture.executor.status(run_id).unwrap();
    let recorded: Vec<(Value, Value, Value)> = status["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "cancel_failed" || event["kind"] == "cancelled")
        .map(|event| {
            (
                event["kind"].clone(),
                event["error"].clone(),
                event["detail"].clone(),
            )
        })
        .collect();
    assert_eq!(
        (recorded, instance_column(&status, "a", "status")),
        (
            vec![
                (
                    json!("cancel_failed"),
                    json!("delete_subagent: child already gone"),
                    Value::Null
                ),
                (
                    json!("cancelled"),
                    Value::Null,
                    json!("slot released despite the failed delete")
                ),
            ],
            vec![json!("cancelled")],
        )
    );
}

/// `test_stop_during_in_flight_admission_deletes_the_child`: a spawn that
/// lands after the stop is retracted.
#[tokio::test(start_paused = true)]
async fn a_spawn_that_lands_after_a_stop_is_deleted_instead_of_running() {
    let fixture = Fixture::new();
    fixture.host.set_child("child-1", Outcome::done("go"));
    // The second spawn (state b) is held open while the run is stopped.
    let (gate, entered) = fixture.host.gate("rlm.run", 2);
    let started = fixture
        .start(
            "dag",
            json!({"nodes": [worker("a"), {"id": "b", "subagent": "worker", "depends_on": ["a"]}]}),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap().to_string();
    entered.acquire().await.unwrap().forget();
    let stopping = {
        let executor = fixture.executor.clone();
        let run_id = run_id.clone();
        tokio::spawn(async move { executor.stop(&run_id).await.unwrap() })
    };
    let stopped = stopping.await.unwrap();
    gate.add_permits(1);
    let retracted = fixture
        .until(&started, |_| !fixture.host.deleted().is_empty())
        .await;
    assert_eq!(
        (
            stopped["state"].clone(),
            retracted["state"].clone(),
            instance_column(&retracted, "b", "status"),
            fixture.host.deleted(),
        ),
        (
            json!("stopped"),
            json!("stopped"),
            vec![json!("cancelled")],
            vec!["child-2".to_string()]
        )
    );
}

/// `test_run_budget_pauses_and_notifies_then_resume_completes`.
#[tokio::test(start_paused = true)]
async fn the_run_budget_pauses_the_run_once_and_a_resume_runs_past_it() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::running());
    let started = fixture
        .start(
            "dag",
            json!({
                "run": {"budget_ms": 1000},
                "nodes": [worker("a"), {"id": "b", "subagent": "worker", "depends_on": ["a"]}],
            }),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let paused = fixture.settle(&started).await;
    fixture.host.set_node("a", Outcome::done("late"));
    fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            paused["state"].clone(),
            paused["pause_reason"].clone(),
            node(&paused, "a")["status"].clone(),
            fixture.host.notices()[0]["detail"].clone(),
            status["state"].clone(),
            fixture.host.notice_kinds(),
        ),
        (
            json!("paused"),
            json!("run budget exceeded"),
            json!("running"),
            json!(format!(
                "run budget_ms 1000 exceeded after 2000ms; no new spawns; resume with (factory-resume \"{run_id}\")"
            )),
            json!("done"),
            vec!["budget_exceeded".to_string(), "finished".to_string()],
        )
    );
}

/// `test_admission_phase_stops_at_the_run_budget_boundary`: a slow initial
/// admission must not keep launching after the budget expired.
#[tokio::test(start_paused = true)]
async fn the_admission_phase_of_run_stops_at_the_run_budget() {
    let fixture = Fixture::new();
    fixture.host.spawns_take(Duration::from_millis(600));
    let started = fixture
        .start(
            "dag",
            json!({
                "run": {"budget_ms": 1000},
                "nodes": [worker("a"), worker("b"), worker("c"), worker("d")],
            }),
        )
        .await;
    let status = fixture
        .executor
        .status(started["run_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        (started["started"].clone(), status["state"].clone()),
        (json!(["a", "b"]), json!("paused"))
    );
}

/// `test_node_budget_marks_attempt_failed`.
#[tokio::test(start_paused = true)]
async fn a_child_that_settles_past_its_state_budget_fails_without_a_retry() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::running());
    let started = fixture
        .start(
            "dag",
            json!({
                "run": {"failure_policy": "continue"},
                "nodes": [{"id": "a", "subagent": "worker", "budget_ms": 1000, "retries": 2}],
            }),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(5000)).await;
    fixture.host.set_node("a", Outcome::done("too late"));
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["state"].clone(),
            node(&status, "a")["error"].clone(),
            fixture.host.spawns("a").len(),
        ),
        (
            json!("failed"),
            json!("state budget_ms 1000 exceeded (5000ms from admission to settlement)"),
            1,
        )
    );
}

/// `test_run_max_children_pauses_at_admission_then_resume_completes`.
#[tokio::test(start_paused = true)]
async fn max_children_pauses_before_the_admission_past_it_and_a_resume_continues() {
    let fixture = Fixture::new();
    let started = fixture
        .start(
            "dag",
            json!({
                "run": {"max_children": 2},
                "nodes": [worker("a"), worker("b"), worker("c")],
            }),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let paused = fixture.executor.status(run_id).unwrap();
    fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            started["started"].clone(),
            paused["state"].clone(),
            paused["pause_reason"].clone(),
            status["state"].clone(),
            status["usage"]["spawns"].clone(),
            fixture.host.notice_kinds(),
        ),
        (
            json!(["a", "b"]),
            json!("paused"),
            json!("max_children exceeded"),
            json!("done"),
            json!(3),
            vec!["max_children_exceeded".to_string(), "finished".to_string()],
        )
    );
}

/// `test_rate_limit_at_admission_defers_to_backoff`,
/// `test_backoff_keeps_collecting_running_children`.
#[tokio::test(start_paused = true)]
async fn a_rate_limited_admission_defers_and_the_loop_retries_after_a_backoff() {
    let fixture = Fixture::new();
    fixture.host.rate_limit("b", 2);
    let started = fixture
        .start("dag", json!({"nodes": [worker("a"), worker("b")]}))
        .await;
    let status = fixture.settle(&started).await;
    let details = |kind: &str| -> Vec<Value> {
        events(&status, kind)
            .iter()
            .map(|event| event["detail"].clone())
            .collect()
    };
    assert_eq!(
        (
            started["started"].clone(),
            status["state"].clone(),
            details("spawn_deferred"),
            details("spawn_backoff"),
            node(&status, "b")["attempts"].clone(),
        ),
        (
            json!(["a"]),
            json!("done"),
            vec![json!("rate limited at admission: 429 rate limit exceeded")],
            vec![json!("rate limited; retrying in 1s")],
            json!(3),
        )
    );
}

/// `test_rate_limit_backoff_exhaustion_fails_node`.
#[tokio::test(start_paused = true)]
async fn five_rate_limited_admissions_in_a_row_fail_the_node() {
    let fixture = Fixture::new();
    fixture.host.rate_limit("a", 999);
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let status = fixture.settle(&started).await;
    let backoffs: Vec<Value> = events(&status, "spawn_backoff")
        .iter()
        .map(|event| event["detail"].clone())
        .collect();
    assert_eq!(
        (
            status["state"].clone(),
            node(&status, "a")["error"].clone(),
            backoffs,
            fixture.host.spawns("a").len(),
        ),
        (
            json!("paused"),
            json!("spawn admission failed: 429 rate limit exceeded"),
            vec![
                json!("rate limited; retrying in 1s"),
                json!("rate limited; retrying in 2s"),
                json!("rate limited; retrying in 4s"),
                json!("rate limited; retrying in 8s"),
            ],
            6,
        )
    );
}

/// `test_worker_death_respawns_once_without_spending_retries_or_max_entries`,
/// `test_second_worker_death_falls_through_retries_and_policy`.
#[tokio::test(start_paused = true)]
async fn a_verified_worker_death_is_respawned_once_for_free_and_a_second_one_fails_the_state() {
    let mut outcomes = Vec::new();
    for second in [
        Outcome::done("recovered"),
        Outcome::error("supervisor link lost"),
    ] {
        let fixture = Fixture::new();
        fixture
            .host
            .set_child("child-1", Outcome::error("Child worker unreachable"));
        fixture.host.set_child("child-2", second);
        let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
        let status = fixture.settle(&started).await;
        outcomes.push((
            status["state"].clone(),
            node(&status, "a")["entries_used"].clone(),
            node(&status, "a")["attempts"].clone(),
            events(&status, "infra_respawn").len(),
            events(&status, "retry").len(),
            fixture.host.deleted(),
            status["usage"]["settled"].clone(),
        ));
    }
    assert_eq!(
        outcomes,
        vec![
            (
                json!("done"),
                json!(1),
                json!(2),
                1,
                0,
                vec!["child-1".to_string()],
                json!(2)
            ),
            (
                json!("paused"),
                json!(1),
                json!(2),
                1,
                0,
                vec!["child-1".to_string()],
                json!(2)
            ),
        ]
    );
}

/// `test_child_exit_captures_a_provisional_answer_and_marks_needs_verify`.
#[tokio::test(start_paused = true)]
async fn a_child_exit_keeps_its_last_words_as_a_provisional_answer_to_verify() {
    let fixture = Fixture::new();
    fixture.host.set_node(
        "a",
        Outcome::error_with_answer("child exited", "pushed the branch"),
    );
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["needs_verify"].clone(),
            node(&status, "a")["needs_verify"].clone(),
            node(&status, "a")["entries"][0].clone(),
            events(&status, "needs_verify")[0]["detail"].clone(),
        ),
        (
            json!(["a"]),
            json!(true),
            json!({
                "index": 0,
                "status": "error",
                "error": "child exited",
                "needs_verify": true,
                "provisional_answer": "pushed the branch",
            }),
            json!("child exit captured a provisional answer - verify the remote state"),
        )
    );
}

/// `test_status_marks_events_delivered_and_unknown_run_raises`,
/// `test_milestone_notices_carry_validated_payloads`,
/// `test_dead_bridge_keeps_the_milestone_in_the_ledger`.
#[tokio::test(start_paused = true)]
async fn a_milestone_is_shown_through_the_host_or_stays_in_the_ledger_when_the_bridge_is_dead() {
    let mut outcomes = Vec::new();
    for bridge_dead in [false, true] {
        let fixture = Fixture::new();
        if bridge_dead {
            fixture.host.fail("factory.progress");
        }
        let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
        let status = fixture.settle(&started).await;
        let mut stages: Vec<&str> = status["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["stage"].as_str().unwrap())
            .collect();
        stages.dedup();
        let mut notices = fixture.host.notices();
        for notice in &mut notices {
            notice["run_id"] = json!(notice["run_id"] == started["run_id"]);
        }
        outcomes.push((
            status["state"].clone(),
            stages.join(" "),
            events(&status, "milestone")[0]["milestone"].clone(),
            notices,
        ));
    }
    let unknown = Fixture::new()
        .executor
        .status("nope")
        .unwrap_err()
        .to_string();
    assert_eq!(
        (outcomes, unknown),
        (
            vec![
                (
                    json!("done"),
                    "delivered shown".to_string(),
                    json!("finished"),
                    vec![json!({
                        "type": "factory.progress",
                        "run_id": true,
                        "kind": "finished",
                        "detail": "run complete: 1 state(s), 0 transition(s) fired",
                    })],
                ),
                (
                    json!("done"),
                    "delivered".to_string(),
                    json!("finished"),
                    Vec::new()
                ),
            ],
            "unknown factory run 'nope'".to_string(),
        )
    );
}

/// The review/fix loop machine: `collect` feeds `reviewing`, whose verdict
/// either sends the work to `fixing` (which loops back) or ends the run.
fn review_loop(reviewing_max_entries: u64) -> Value {
    json!({
        "states": [
            {"id": "collect", "entry": true, "subagent": "worker"},
            {
                "id": "reviewing",
                "subagent": {"prompt": "Review; fix: {fix}"},
                "inputs": [{"name": "fix", "type": "text", "from": "fixing.report", "optional": true}],
                "outputs": [{"name": "verdict", "type": "json"}],
                "max_entries": reviewing_max_entries,
            },
            {
                "id": "fixing",
                "subagent": "worker",
                "outputs": [{"name": "report", "type": "text"}],
                "max_entries": 3,
            },
        ],
        "transitions": [
            {"from": "collect", "to": "reviewing"},
            {
                "from": "reviewing",
                "to": "fixing",
                "when": {"output": "verdict", "path": "approved", "op": "eq", "value": false},
            },
            {"from": "fixing", "to": "reviewing"},
        ],
    })
}

/// `test_machine_three_round_review_loop`,
/// `test_machine_optional_input_binds_null_then_the_real_settle`,
/// `test_machine_guard_switch_fires_only_the_matching_branch`.
#[tokio::test(start_paused = true)]
async fn a_guarded_loop_reenters_until_the_verdict_approves_and_rebinds_its_optional_input() {
    let fixture = Fixture::new();
    // collect, review 1, fix 1, review 2, fix 2, review 3.
    for rejected in ["child-2", "child-4"] {
        fixture.host.set_child(
            rejected,
            Outcome::done("{\"verdict\": {\"approved\": false}}"),
        );
    }
    fixture.host.set_child(
        "child-6",
        Outcome::done("{\"verdict\": {\"approved\": true}}"),
    );
    let started = fixture.start("machine", review_loop(4)).await;
    let status = fixture.settle(&started).await;
    let fired: Vec<Value> = events(&status, "transition_fired")
        .iter()
        .map(|event| event["detail"].clone())
        .collect();
    assert_eq!(
        (
            status["state"].clone(),
            fixture.host.prompts("reviewing"),
            node(&status, "reviewing")["entries_used"].clone(),
            node(&status, "fixing")["entries_used"].clone(),
            status["usage"]["transitions_fired"].clone(),
            fired,
        ),
        (
            json!("done"),
            vec![
                "Review; fix: None".to_string(),
                "Review; fix: answer-fixing".to_string(),
                "Review; fix: answer-fixing".to_string(),
            ],
            json!(3),
            json!(2),
            json!(5),
            vec![
                json!("'collect' -> 'reviewing'"),
                json!("'reviewing' -> 'fixing'"),
                json!("'fixing' -> 'reviewing'"),
                json!("'reviewing' -> 'fixing'"),
                json!("'fixing' -> 'reviewing'"),
            ],
        )
    );
}

/// `test_machine_max_entries_blocked_transition_quiesces_done`.
#[tokio::test(start_paused = true)]
async fn a_transition_onto_a_state_out_of_entries_is_blocked_and_the_run_still_ends_done() {
    let fixture = Fixture::new();
    fixture.host.set_node(
        "reviewing",
        Outcome::done("{\"verdict\": {\"approved\": false}}"),
    );
    let started = fixture.start("machine", review_loop(1)).await;
    let status = fixture.settle(&started).await;
    let blocked: Vec<Value> = events(&status, "transition_blocked")
        .iter()
        .map(|event| event["detail"].clone())
        .collect();
    assert_eq!(
        (status["state"].clone(), blocked),
        (
            json!("done"),
            vec![json!(
                "state 'reviewing' is at max_entries 1; transition 'fixing' -> 'reviewing' blocked"
            )],
        )
    );
}

/// `test_machine_max_transitions_pauses_once_then_resumes`,
/// `test_machine_max_transitions_pause_mid_settle_does_not_refire_on_resume`.
#[tokio::test(start_paused = true)]
async fn max_transitions_pauses_mid_settle_and_a_resume_fires_only_what_had_not_fired() {
    let fixture = Fixture::new();
    let started = fixture
        .start(
            "machine",
            json!({
                "run": {"max_transitions": 1},
                "states": [
                    {"id": "a", "entry": true, "subagent": "worker"},
                    worker("b"),
                    worker("c"),
                ],
                "transitions": [{"from": "a", "to": "b"}, {"from": "a", "to": "c"}],
            }),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let paused = fixture.settle(&started).await;
    let at_pause = (
        paused["state"].clone(),
        paused["pause_reason"].clone(),
        node(&paused, "b")["entries_used"].clone(),
        node(&paused, "c")["entries_used"].clone(),
    );
    fixture.executor.resume(run_id).await.unwrap();
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            at_pause,
            status["state"].clone(),
            fixture.host.spawns("b").len(),
            fixture.host.spawns("c").len(),
            status["usage"]["transitions_fired"].clone(),
            fixture.host.notice_kinds(),
        ),
        (
            (
                json!("paused"),
                json!("max_transitions exceeded"),
                json!(1),
                json!(0)
            ),
            json!("done"),
            1,
            1,
            json!(2),
            vec![
                "max_transitions_exceeded".to_string(),
                "finished".to_string()
            ],
        )
    );
}

/// `test_machine_stall_fails_with_the_pending_entry_reason`.
#[tokio::test(start_paused = true)]
async fn an_entry_waiting_on_a_source_that_never_runs_fails_the_run_instead_of_wedging_it() {
    let fixture = Fixture::new();
    let started = fixture
        .start(
            "machine",
            json!({
                "states": [
                    {"id": "a", "entry": true, "subagent": "worker"},
                    {
                        "id": "b",
                        "subagent": "worker",
                        "inputs": [{"name": "data", "type": "text", "from": "never.out"}],
                    },
                    {"id": "never", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                ],
                "transitions": [{"from": "a", "to": "b"}],
            }),
        )
        .await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["state"].clone(),
            events(&status, "executor_error")[0]["error"].clone(),
        ),
        (
            json!("failed"),
            json!("control loop stalled: pending entry of state 'b' is waiting for an input source that never settled"),
        )
    );
}

/// `test_resident_node_spawns_stays_alive_and_stops`,
/// `test_a_done_run_with_children_in_flight_reports_live`.
#[tokio::test(start_paused = true)]
async fn a_resident_outlives_its_finished_run_until_the_run_is_stopped() {
    let fixture = Fixture::new();
    fixture.host.set_node("watcher", Outcome::running());
    let started = fixture
        .start(
            "machine",
            json!({
                "states": [
                    {"id": "a", "entry": true, "subagent": "worker"},
                    {"id": "watcher", "subagent": "worker", "lifecycle": "resident"},
                ],
                "transitions": [{"from": "a", "to": "watcher"}],
            }),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let status = fixture.settle(&started).await;
    let listed = fixture
        .executor
        .graph(None, super::report::Detail::Full)
        .unwrap();
    let stopped = fixture.executor.stop(run_id).await.unwrap();
    assert_eq!(
        (
            status["state"].clone(),
            node(&status, "watcher")["status"].clone(),
            fixture.host.notices()[0]["detail"].clone(),
            listed["runs"][0]["active_nodes"].clone(),
            stopped["cancelled"].clone(),
            fixture.host.deleted(),
        ),
        (
            json!("done"),
            json!("running"),
            json!(format!(
                "run complete: 2 state(s), 1 transition(s) fired; 1 resident state(s) still running (stop with (factory-stop \"{run_id}\"))"
            )),
            json!(["watcher"]),
            json!(["watcher"]),
            vec!["child-2".to_string()],
        )
    );
}

/// `test_resident_saturated_cap_fails_the_run_instead_of_wedging`.
#[tokio::test(start_paused = true)]
async fn residents_holding_every_parallel_slot_fail_the_run_with_work_still_queued() {
    let fixture = Fixture::new();
    fixture.host.set_node("first", Outcome::running());
    let started = fixture
        .start(
            "machine",
            json!({
                "run": {"max_parallel": 1},
                "states": [
                    {"id": "first", "entry": true, "subagent": "worker", "lifecycle": "resident"},
                    {"id": "second", "entry": true, "subagent": "worker"},
                ],
            }),
        )
        .await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["state"].clone(),
            events(&status, "executor_error")[0]["error"].clone(),
        ),
        (
            json!("failed"),
            json!("control loop stalled: resident instances hold every max_parallel 1 slot; queued instances can never be admitted"),
        )
    );
}

/// A host failure in the control loop fails the run with the reason in the
/// ledger (the Python loop's `executor_error` arm).
#[tokio::test(start_paused = true)]
async fn a_collect_the_host_refuses_fails_the_run_instead_of_wedging_it() {
    let fixture = Fixture::new();
    fixture.host.fail("rlm.collect");
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let status = fixture.settle(&started).await;
    assert_eq!(
        (
            status["state"].clone(),
            events(&status, "executor_error")[0]["error"].clone(),
            fixture.host.notices()[0]["detail"].clone(),
        ),
        (
            json!("failed"),
            json!("collect: the bridge is gone"),
            json!("executor error: collect: the bridge is gone"),
        )
    );
}
