//! The factory's surfaces around the run: the opt-in gate, the graph and
//! watch views, the daemon and TUI lane with its capped reply frame, the
//! machine library, python states, and the Scheme API on a live kernel.
//! Each test names the Python tests it carries over
//! (`prime-agent-runtime/test/test_factory.py`).

use std::time::Duration;

use serde_json::{json, Value};

use super::library::{
    cli_dispatch, import_machine, resolve_machine, scan_library, write_export, Overwrite,
    ResolutionError,
};
use super::report::{cap_frame, Detail, FACTORY_FRAME_CAP};
use super::store::FactoryEnvironment;
use super::test_host::{events, instance_column, node, Fixture, Outcome};
use crate::tools::ipython::{IpythonKernelProvisioner, KernelExecuteOptions, KernelExecutor};

const DISABLED: &str = "the factory is disabled; run /factory on to enable it";

fn worker(id: &str) -> Value {
    json!({"id": id, "subagent": "worker"})
}

/// A guarded machine: `a` settles a verdict that sends the run to `b`.
fn guarded_machine() -> Value {
    json!({
        "run": {"budget_ms": 60_000},
        "states": [
            {"id": "a", "entry": true, "subagent": "worker", "outputs": [{"name": "verdict", "type": "json"}]},
            {"id": "b", "subagent": {"prompt": "Next.", "model": "p/m"}},
        ],
        "transitions": [
            {"from": "a", "to": "b", "when": {"output": "verdict", "path": "ok", "op": "eq", "value": true}},
        ],
    })
}

/// `FactoryOptInGateTest`: `test_empty_settings_refuse_run_with_the_exact_message`,
/// `test_status_stop_and_resume_refuse_with_the_exact_message`,
/// `test_graph_and_watch_refuse_with_the_exact_message`,
/// `test_the_activity_lane_refuses_while_disabled`,
/// `test_run_proceeds_when_the_enabled_setting_is_written`.
#[tokio::test(start_paused = true)]
async fn every_run_call_refuses_with_one_message_while_the_factory_is_disabled() {
    let fixture = Fixture::new();
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let run_id = started["run_id"].as_str().unwrap();
    fixture.settle(&started).await;
    fixture.settings(&json!({}));
    let executor = &fixture.executor;
    let refusals = vec![
        executor.run("sw", None).await.unwrap_err().to_string(),
        executor.status(run_id).unwrap_err().to_string(),
        executor.stop(run_id).await.unwrap_err().to_string(),
        executor.resume(run_id).await.unwrap_err().to_string(),
        executor.graph(None, Detail::Full).unwrap_err().to_string(),
        executor
            .watch(run_id, 0.0, Detail::Full)
            .await
            .unwrap_err()
            .to_string(),
    ];
    let frame = executor
        .activity_reply(&json!({"id": "f1", "action": "run", "specId": "sw"}))
        .await;
    assert_eq!(
        (refusals, frame, fixture.host.calls_of("rlm.run").len()),
        (
            vec![DISABLED.to_string(); 6],
            json!({"event": "done", "id": "f1", "status": "error", "reason": DISABLED}),
            1,
        )
    );
}

/// `FactoryGraphWatchTest`: `test_graph_fuses_structure_and_live_state`,
/// `test_graph_is_status_data_plus_the_static_graph`,
/// `test_graph_machine_carries_the_declared_run_limits`,
/// `test_last_fired_marks_the_recently_fired_edges`,
/// `test_compact_snapshot_sheds_answers_and_carries_the_short_tail`.
#[tokio::test(start_paused = true)]
async fn the_graph_is_the_machine_structure_fused_with_the_status_data() {
    let fixture = Fixture::new();
    fixture
        .host
        .set_node("a", Outcome::done("{\"verdict\": {\"ok\": true}}"));
    let started = fixture.start("machine", guarded_machine()).await;
    let run_id = started["run_id"].as_str().unwrap();
    fixture.settle(&started).await;
    let graph = fixture.executor.graph(Some(run_id), Detail::Full).unwrap();
    let compact = fixture
        .executor
        .graph(Some(run_id), Detail::Compact)
        .unwrap();
    let status = fixture.executor.status(run_id).unwrap();
    let guard = json!({"output": "verdict", "path": "ok", "op": "eq", "value": true});
    assert_eq!(
        (
            graph["machine"].clone(),
            graph["nodes"] == status["nodes"],
            graph["usage"] == status["usage"],
            graph["active_nodes"].clone(),
            graph["last_fired"].clone(),
            graph["budget"]["limit_ms"].clone(),
            events(&graph, "answer_captured").len(),
            events(&compact, "answer_captured").len(),
            node(&graph, "a")["answer_preview"].clone(),
            node(&compact, "a").get("answer_preview").cloned(),
        ),
        (
            json!({
                "run": {
                    "max_parallel": 8,
                    "max_transitions": 20,
                    "failure_policy": "escalate",
                    "max_children": 10_000,
                    "budget_ms": 60_000,
                },
                "states": [
                    {"id": "a", "entry": true, "lifecycle": "task", "max_entries": 1, "retries": 0, "subagent": "worker"},
                    {"id": "b", "entry": false, "lifecycle": "task", "max_entries": 1, "retries": 0, "model": "p/m"},
                ],
                "transitions": [{"from": "a", "to": "b", "on": "settled", "when": guard}],
                "order": ["a", "b"],
            }),
            true,
            true,
            json!([]),
            json!([{"from": "a", "to": "b", "seq": 7, "when": guard}]),
            json!(60_000),
            2,
            0,
            json!("{\"verdict\": {\"ok\": true}}"),
            None,
        )
    );
}

/// `test_graph_of_a_stored_spec_returns_the_static_structure`,
/// `test_graph_lists_every_live_run_and_marks_active_nodes`,
/// `test_unscoped_graph_bounds_the_terminal_history`.
#[tokio::test(start_paused = true)]
async fn the_graph_answers_a_stored_spec_statically_and_lists_live_runs_plus_recent_history() {
    let fixture = Fixture::new();
    fixture.store("sw", "dag", json!({"nodes": [worker("a")]}));
    let spec = fixture.executor.graph(Some("sw"), Detail::Full).unwrap();
    for _ in 0..22 {
        let finished = fixture.executor.run("sw", None).await.unwrap();
        fixture.settle(&finished).await;
    }
    fixture.host.set_node("a", Outcome::running());
    let live = fixture.executor.run("sw", None).await.unwrap();
    let listed = fixture.executor.graph(None, Detail::Full).unwrap();
    let runs = listed["runs"].as_array().unwrap();
    let unknown = fixture
        .executor
        .graph(Some("ghost"), Detail::Full)
        .unwrap_err()
        .to_string();
    assert_eq!(
        (
            spec["run_id"].clone(),
            spec["spec_id"].clone(),
            spec["machine"]["order"].clone(),
            spec["nodes"].clone(),
            spec["budget"].clone(),
            runs.len(),
            runs[20]["run_id"] == live["run_id"],
            runs[20]["active_nodes"].clone(),
            unknown,
        ),
        (
            Value::Null,
            json!("sw"),
            json!(["a"]),
            json!([]),
            json!({"limit_ms": null, "consumed_ms": 0}),
            21,
            true,
            json!(["a"]),
            "unknown factory run or spec 'ghost'".to_string(),
        )
    );
}

/// `test_watch_returns_the_snapshot_when_nothing_changes`,
/// `test_watch_blocks_until_the_run_changes`,
/// `test_watch_times_out_without_a_change`,
/// `test_watch_rejects_unknown_runs_and_bad_timeouts`.
#[tokio::test(start_paused = true)]
async fn watch_returns_on_a_change_of_shape_or_at_its_timeout() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::running());
    let started = fixture.start("dag", json!({"nodes": [worker("a")]})).await;
    let run_id = started["run_id"].as_str().unwrap();
    let executor = &fixture.executor;
    let immediate = executor.watch(run_id, 0.0, Detail::Full).await.unwrap();
    let before = tokio::time::Instant::now();
    let timed_out = executor.watch(run_id, 30.0, Detail::Full).await.unwrap();
    let waited = before.elapsed();
    let settling = {
        let host = std::sync::Arc::clone(&fixture.host);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            host.set_node("a", Outcome::done("fine"));
        })
    };
    let changed = executor.watch(run_id, 30.0, Detail::Full).await.unwrap();
    settling.await.unwrap();
    let mut refusals = Vec::new();
    for (run, timeout) in [("nope", 1.0), (run_id, -1.0), (run_id, f64::NAN)] {
        refusals.push(
            executor
                .watch(run, timeout, Detail::Full)
                .await
                .unwrap_err()
                .to_string(),
        );
    }
    assert_eq!(
        (
            immediate["changed"].clone(),
            immediate["state"].clone(),
            timed_out["changed"].clone(),
            waited,
            changed["changed"].clone(),
            instance_column(&changed, "a", "status"),
            refusals,
        ),
        (
            json!(false),
            json!("running"),
            json!(false),
            Duration::from_secs(30),
            json!(true),
            vec![json!("done")],
            vec![
                "unknown factory run 'nope'".to_string(),
                "timeout must be a non-negative number of seconds".to_string(),
                "timeout must be a non-negative number of seconds".to_string(),
            ],
        )
    );
}

/// `test_activity_routes_every_action`,
/// `test_activity_reply_carries_the_wire_keys`,
/// `test_activity_reply_carries_guards_verbatim`.
#[tokio::test(start_paused = true)]
async fn the_activity_lane_routes_each_action_and_answers_in_the_wire_spelling() {
    let fixture = Fixture::new();
    fixture
        .host
        .set_node("a", Outcome::done("{\"verdict\": {\"ok\": true}}"));
    fixture.host.set_node("b", Outcome::running());
    fixture.store("sw", "machine", guarded_machine());
    let executor = &fixture.executor;
    let run = executor
        .activity_reply(&json!({"id": "f1", "action": "run", "specId": "sw"}))
        .await;
    let run_id = run["result"]["runId"].clone();
    fixture
        .until(&json!({ "run_id": run_id }), |graph| {
            node(graph, "b")["status"] == "running"
        })
        .await;
    let request =
        |action: &str| json!({"id": action, "action": action, "runId": run_id, "timeoutMs": 0});
    let graph = executor.activity_reply(&request("graph")).await;
    let status = executor.activity_reply(&request("status")).await;
    let watch = executor.activity_reply(&request("watch")).await;
    let stop = executor.activity_reply(&request("stop")).await;
    let resume = executor.activity_reply(&request("resume")).await;
    let all = executor
        .activity_reply(&json!({"id": "all", "action": "graph"}))
        .await;
    assert_eq!(
        (
            run["result"]["specId"].clone(),
            run["result"]["maxParallel"].clone(),
            graph["result"]["machine"]["transitions"][0]["when"].clone(),
            graph["result"]["lastFired"][0]["when"].clone(),
            graph["result"]["activeNodes"].clone(),
            graph["result"]["nodes"][0]["entriesUsed"].clone(),
            status["result"]["usage"]["transitionsFired"].clone(),
            watch["result"]["changed"].clone(),
            stop["result"].clone(),
            resume["status"].clone(),
            resume["reason"]
                .as_str()
                .map(|reason| reason.ends_with("is 'stopped', not paused")),
            all["result"]["runs"][0]["runId"] == run_id,
        ),
        (
            json!("sw"),
            json!(8),
            json!({"output": "verdict", "path": "ok", "op": "eq", "value": true}),
            json!({"output": "verdict", "path": "ok", "op": "eq", "value": true}),
            json!(["b"]),
            json!(1),
            json!(1),
            json!(false),
            json!({"runId": run_id, "state": "stopped", "cancelled": ["b"]}),
            json!("error"),
            Some(true),
            true,
        )
    );
}

/// `test_activity_validates_its_request_shape`.
#[tokio::test(start_paused = true)]
async fn the_activity_lane_refuses_a_malformed_request_with_the_reason() {
    let fixture = Fixture::new();
    let mut reasons = Vec::new();
    for request in [
        json!({"id": "1", "action": "explode"}),
        json!({"id": "2", "action": "status", "runId": 7}),
        json!({"id": "3", "action": "watch", "runId": "r", "timeoutMs": 60_001}),
        json!({"id": "4", "action": "watch", "runId": "r", "timeoutMs": "soon"}),
        json!({"id": "5", "action": "status"}),
        json!({"id": "6", "action": "run"}),
        json!({"id": "7", "action": "stop", "runId": ""}),
        json!({"id": "8", "action": "run", "specId": "review-sweep"}),
    ] {
        reasons.push(fixture.executor.activity_reply(&request).await["reason"].clone());
    }
    assert_eq!(
        reasons,
        vec![
            json!("unknown factory activity action 'explode'"),
            json!("factory activity runId must be a string when provided"),
            json!("factory activity timeoutMs must be an integer between 0 and 60000"),
            json!("factory activity timeoutMs must be an integer between 0 and 60000"),
            json!("factory activity status requires runId"),
            json!("factory activity run requires specId"),
            json!("factory activity stop requires runId"),
            // The lane starts stored entries only: no library machines.
            json!("unknown factory spec 'review-sweep'"),
        ]
    );
}

/// `FactoryFrameCapTest`: `test_an_oversized_error_frame_fails_loudly`,
/// `test_an_oversized_reply_is_trimmed_then_failed`,
/// `test_the_cap_drops_terminal_rows_before_live_ones`,
/// `test_a_live_row_never_silently_drops`.
#[test]
fn a_reply_frame_over_the_wire_cap_sheds_oldest_events_then_finished_runs_or_fails_loudly() {
    let big = "x".repeat(FACTORY_FRAME_CAP / 4);
    let event = |seq: u64| json!({"seq": seq, "detail": big});
    let row = |id: &str, state: &str, running: u64| json!({"runId": id, "state": state, "usage": {"running": running}, "events": [event(1)]});
    let capped = |mut frame: Value| {
        cap_frame(&mut frame);
        frame
    };
    let failed = json!({
        "event": "done",
        "id": "f",
        "status": "error",
        "reason": "factory activity reply exceeds the wire cap",
    });
    let trimmed = capped(json!({
        "event": "done", "id": "f", "status": "ok",
        "result": {"events": [event(1), event(2), event(3), event(4), event(5), event(6)]},
    }));
    let dropped = capped(json!({
        "event": "done", "id": "f", "status": "ok",
        "result": {"runs": [
            row("old-done", "done", 0),
            row("live", "running", 1),
            row("residents", "done", 2),
            row("new-done", "failed", 0),
            row("newest-done", "stopped", 0),
        ]},
    }));
    let kept: Vec<&str> = dropped["result"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["runId"].as_str().unwrap())
        .collect();
    let seqs: Vec<u64> = trimmed["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        (
            capped(
                json!({"event": "done", "id": "f", "status": "error", "reason": "y".repeat(FACTORY_FRAME_CAP)})
            ),
            seqs,
            kept,
            capped(json!({
                "event": "done", "id": "f", "status": "ok",
                "result": {"runs": (0..5).map(|index| row(&format!("live-{index}"), "paused", 0)).collect::<Vec<_>>()},
            })),
        ),
        (
            failed.clone(),
            vec![4, 5, 6],
            vec!["live", "residents", "newest-done"],
            failed,
        )
    );
}

/// A `MACHINE.md` named `name` whose spec is `spec`.
fn machine_text(name: &str, spec: &Value) -> String {
    format!(
        "---\nname: {name}\ndescription: A machine that sweeps.\nversion: 1\nauthor: Tester\n---\n\n# {name}\n\n```machine-spec\n{spec}\n```\n"
    )
}

/// `MachineLibraryResolutionTest`: `test_repo_dir_wins_over_user_dir`,
/// `test_user_dir_serves_names_the_repo_does_not_have`,
/// `test_the_declared_name_wins_over_the_directory_name`,
/// `test_list_machines_dedupes_repo_first_and_sorts_by_name`,
/// `test_list_machines_skips_broken_files`,
/// `test_list_machines_excludes_spec_invalid_files_with_their_errors`,
/// `test_resolve_machine_raises_exact_parse_errors_for_broken_files`,
/// `test_resolve_falls_through_parse_broken_files_to_valid_user_machines`,
/// `test_unknown_name_lists_available_machines`,
/// `test_invalid_machine_name_is_rejected_before_scanning`.
#[test]
fn the_library_resolves_shared_before_personal_and_a_broken_file_never_claims_its_name() {
    let temp = tempfile::tempdir().unwrap();
    let (repo, agent) = (temp.path().join("repo"), temp.path().join("agent"));
    let place = |root: &std::path::Path, directory: &str, text: &str| {
        std::fs::create_dir_all(root.join(directory)).unwrap();
        std::fs::write(root.join(directory).join("MACHINE.md"), text).unwrap();
    };
    let valid = json!({"nodes": [worker("a")]});
    place(&repo, "sweep", &machine_text("sweep", &valid));
    place(&repo, "misnamed", &machine_text("declared", &valid));
    place(&repo, "torn", "no frontmatter");
    place(&repo, "dead", &machine_text("dead", &json!({"nodes": []})));
    place(&repo, "fallback", "no frontmatter");
    let user = agent.join("machines");
    place(
        &user,
        "sweep",
        &machine_text("sweep", &json!({"nodes": [worker("personal")]})),
    );
    place(&user, "mine", &machine_text("mine", &valid));
    place(&user, "fallback", &machine_text("fallback", &valid));
    let environment = FactoryEnvironment {
        agent_dir: Some(agent),
        local_harness_dir: None,
        machines_dir: Some(repo.clone()),
    };
    let (listed, warnings) = scan_library(&environment);
    let rows: Vec<(&str, &str)> = listed
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap(),
                row["source"].as_str().unwrap(),
            )
        })
        .collect();
    let resolved = |name: &str| {
        resolve_machine(&environment, name)
            .map(|found| (found.machine.spec["nodes"][0]["id"].clone(), found.path))
    };
    let path = |root: &std::path::Path, directory: &str| {
        root.join(directory)
            .join("MACHINE.md")
            .to_string_lossy()
            .into_owned()
    };
    let torn = format!(
        "{}: {}: MACHINE.md must start with a `---` frontmatter block",
        path(&repo, "torn"),
        path(&repo, "torn")
    );
    let dead = format!(
        "{}: factory dag must declare between 1 and 1024 nodes, got 0",
        path(&repo, "dead")
    );
    assert_eq!(
        (
            rows,
            warnings.len(),
            resolved("sweep"),
            resolved("mine"),
            resolved("declared"),
            resolved("fallback"),
            resolved("torn"),
            resolved("dead"),
            resolved("misnamed"),
            resolved("Not A Name"),
        ),
        (
            vec![
                ("declared", "repo"),
                ("fallback", "user"),
                ("mine", "user"),
                ("sweep", "repo"),
            ],
            3,
            Ok((json!("a"), path(&repo, "sweep"))),
            Ok((json!("a"), path(&user, "mine"))),
            Ok((json!("a"), path(&repo, "misnamed"))),
            Ok((json!("a"), path(&user, "fallback"))),
            Err(ResolutionError::Broken(torn)),
            Err(ResolutionError::Broken(dead)),
            Err(ResolutionError::Unknown(
                "unknown machine 'misnamed': no MACHINE.md for it in the machine library (machines: declared, fallback, mine, sweep)".to_string()
            )),
            Err(ResolutionError::InvalidName(
                "machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".to_string()
            )),
        )
    );
}

/// `test_the_shipped_seed_machines_validate_clean`,
/// `test_the_bundled_library_ships_inside_the_runtime_package`.
#[test]
fn the_shipped_machines_are_listed_and_validate_clean() {
    let (listed, warnings) = scan_library(&FactoryEnvironment::default());
    let rows: Vec<(Value, Value, Value)> = listed
        .iter()
        .map(|row| {
            (
                row["name"].clone(),
                row["source"].clone(),
                row["path"].clone(),
            )
        })
        .collect();
    assert_eq!(
        (rows, warnings),
        (
            vec![
                (
                    json!("builder"),
                    json!("repo"),
                    json!("bundled:builder/MACHINE.md")
                ),
                (
                    json!("pr-manager"),
                    json!("repo"),
                    json!("bundled:pr-manager/MACHINE.md")
                ),
                (
                    json!("review-sweep"),
                    json!("repo"),
                    json!("bundled:review-sweep/MACHINE.md")
                ),
            ],
            Vec::<String>::new(),
        )
    );
}

/// `ImportGateTest`: `test_import_persists_valid_files_verbatim`,
/// `test_import_preserves_crlf_and_cr_files_byte_for_byte`,
/// `test_import_rejects_invalid_spec_with_exact_errors_and_persists_nothing`,
/// `test_import_rejects_malformed_files_with_format_errors`,
/// `test_import_rejects_missing_files_and_overwrites_renamed`.
#[test]
fn import_stores_a_valid_machine_byte_for_byte_and_nothing_of_an_invalid_one() {
    let temp = tempfile::tempdir().unwrap();
    let environment = FactoryEnvironment {
        agent_dir: Some(temp.path().join("agent")),
        local_harness_dir: None,
        machines_dir: None,
    };
    let source = |name: &str, text: &str| {
        let path = temp.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    let crlf = machine_text("sweep", &json!({"nodes": [worker("a")]})).replace('\n', "\r\n");
    let stored = temp.path().join("agent/machines/sweep/MACHINE.md");
    let first = import_machine(&environment, &source("good.md", &crlf));
    let stored_bytes = std::fs::read_to_string(&stored).unwrap();
    let again = import_machine(&environment, &source("good.md", &crlf));
    let invalid = import_machine(
        &environment,
        &source("dead.md", &machine_text("dead", &json!({"nodes": []}))),
    );
    let malformed = import_machine(&environment, &source("torn.md", "# no frontmatter"));
    let missing = import_machine(&environment, &temp.path().join("absent.md"));
    assert_eq!(
        (
            first,
            stored_bytes == crlf,
            again,
            invalid,
            temp.path().join("agent/machines/dead").exists(),
            malformed.map_err(
                |error| error.ends_with("MACHINE.md must start with a `---` frontmatter block")
            ),
            missing.map_err(|error| error.starts_with("machine file not found: ")),
        ),
        (
            Ok(json!({"name": "sweep", "path": stored.to_string_lossy(), "created": true})),
            true,
            Ok(json!({"name": "sweep", "path": stored.to_string_lossy(), "created": false})),
            Err("factory dag must declare between 1 and 1024 nodes, got 0".to_string()),
            false,
            Err(true),
            Err(true),
        )
    );
}

/// `ExportMachineTest`: `test_exports_a_library_machine_verbatim`,
/// `test_export_refuses_to_silently_overwrite_the_target`,
/// `test_exports_a_stored_entry_spec_byte_pretty`,
/// `test_multiline_entry_content_exports_as_one_line`,
/// `test_exports_a_runs_canonical_machine`,
/// `test_entry_ids_that_are_not_machine_names_are_rejected`,
/// `test_out_path_must_not_be_a_directory`;
/// `MachineFileRoundTripTest.test_exported_file_imports_to_the_identical_spec`.
#[tokio::test(start_paused = true)]
async fn export_writes_an_entry_a_run_or_a_library_machine_and_never_replaces_a_file_unasked() {
    let fixture = Fixture::new();
    let spec = json!({"nodes": [worker("a")]});
    fixture.entry(
        crate::refinement::RefinementKind::Factory,
        "stored",
        "Stored",
        "Sweeps\n  the branch.",
        json!({ "dag": spec }),
        json!({}),
    );
    fixture.store("Bad Id", "dag", spec.clone());
    let started = fixture
        .executor
        .run("stored", Some("nightly".to_string()))
        .await
        .unwrap();
    let run_id = started["run_id"].as_str().unwrap();
    let executor = &fixture.executor;
    let export = |target: &str, out: &str, overwrite: Overwrite| {
        executor
            .export_machine(target, out, overwrite)
            .map(|exported| exported["source"].clone())
            .map_err(|error| error.to_string())
    };
    let outcomes = (
        export("stored", "entry.md", Overwrite::Refuse),
        export("stored", "entry.md", Overwrite::Refuse)
            .map_err(|error| error.contains("already exists (pass #:overwrite #t to replace it)")),
        export("stored", "entry.md", Overwrite::Replace),
        export(run_id, "run.md", Overwrite::Refuse),
        export("review-sweep", "library.md", Overwrite::Refuse),
        export("Bad Id", "bad.md", Overwrite::Refuse),
        export("stored", ".", Overwrite::Refuse)
            .map_err(|error| error.ends_with("is a directory (pass a file path)")),
    );
    let read = |name: &str| std::fs::read_to_string(fixture.work_dir().join(name)).unwrap();
    let entry_file = super::library::parse_machine_file(&read("entry.md"), "entry.md").unwrap();
    let run_file = super::library::parse_machine_file(&read("run.md"), "run.md").unwrap();
    assert_eq!(
        (
            outcomes,
            (entry_file.name, entry_file.description, entry_file.spec == spec),
            (run_file.name, run_file.description, run_file.spec["states"][0]["max_entries"].clone()),
            read("library.md") == include_str!("machines/review-sweep/MACHINE.md"),
        ),
        (
            (
                Ok(json!("spec")),
                Err(true),
                Ok(json!("spec")),
                Ok(json!("spec")),
                Ok(json!("library")),
                Err("machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only); the stored entry id 'Bad Id' cannot become a machine name".to_string()),
                Err(true),
            ),
            ("stored".to_string(), "Sweeps the branch.".to_string(), true),
            ("stored".to_string(), "nightly".to_string(), json!(1)),
            true,
        )
    );
}

/// `test_export_refuses_a_symlinked_target_without_following_it`.
#[cfg(unix)]
#[test]
fn an_export_onto_a_symlink_refuses_instead_of_writing_through_it() {
    let temp = tempfile::tempdir().unwrap();
    let (real, link) = (temp.path().join("real.md"), temp.path().join("link.md"));
    std::fs::write(&real, "untouched").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let refused = write_export(&link, "replaced", Overwrite::Refuse);
    assert_eq!(
        (
            refused.map_err(|error| error.contains("already exists")),
            std::fs::read_to_string(&real).unwrap()
        ),
        (Err(true), "untouched".to_string())
    );
}

/// `FactoryRunFromLibraryTest`: `test_run_resolves_machine_names_from_the_library`,
/// `test_stored_entries_win_over_library_machines`,
/// `test_run_from_library_compiles_dag_sugar`,
/// `test_an_invalid_name_still_reports_the_unknown_spec_frame`.
#[tokio::test(start_paused = true)]
async fn run_falls_back_to_the_library_machine_when_no_stored_entry_has_the_id() {
    let fixture = Fixture::new();
    let library = fixture.executor.run("review-sweep", None).await.unwrap();
    fixture.store("review-sweep", "dag", json!({"nodes": [worker("mine")]}));
    let stored = fixture.executor.run("review-sweep", None).await.unwrap();
    let invalid = fixture
        .executor
        .run("Not A Name", None)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        (
            library["spec_id"].clone(),
            library["machine"].clone(),
            library["machine_path"].clone(),
            library["started"].clone(),
            library["nodes"].clone(),
            stored["started"].clone(),
            stored.get("machine").cloned(),
            invalid,
        ),
        (
            json!("review-sweep"),
            json!("review-sweep"),
            json!("bundled:review-sweep/MACHINE.md"),
            json!(["files"]),
            json!(3),
            json!(["mine"]),
            None,
            "unknown factory spec 'Not A Name': no stored factory entry, and the id is not a valid machine name either (machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only))".to_string(),
        )
    );
}

/// The module a python state imports, written into the session's directory.
const PYTHON_TOOLS: &str = r#"
import sys, time

def plan(inputs):
    print("planning", inputs)
    return {"files": ["a.py", "b.py"], "note": "two files"}

def boom(inputs):
    raise RuntimeError("nope")

def wrong_keys(inputs):
    return {"other": 1}

def slow(inputs):
    time.sleep(600)
"#;

fn python_machine(callable: &str, extra: Value) -> Value {
    let mut plan = json!({
        "id": "plan",
        "entry": true,
        "python": {"import": "factory_tools", "callable": callable},
        "outputs": [{"name": "files", "type": "json"}, {"name": "note", "type": "text"}],
    });
    if let (Value::Object(plan), Value::Object(extra)) = (&mut plan, extra) {
        plan.extend(extra);
    }
    json!({
        "run": {"failure_policy": "continue"},
        "states": [
            plan,
            {
                "id": "review",
                "subagent": {"prompt": "Review {file} ({note})."},
                "inputs": [
                    {"name": "file", "type": "json", "from": "plan.files"},
                    {"name": "note", "type": "text", "from": "plan.note"},
                ],
                "foreach": {"over": "file", "max": 8},
            },
        ],
        "transitions": [{"from": "plan", "to": "review"}],
    })
}

/// `FactoryPythonStateTest`: `test_python_states_route_and_bind_like_agent_states`,
/// `test_failed_calls_retry_then_apply_the_failure_policy`,
/// `test_a_call_past_its_time_limit_is_killed_without_a_retry`. These run a
/// real `python3`, on real time.
#[tokio::test(flavor = "multi_thread")]
async fn a_python_state_binds_its_returned_dict_and_a_failing_or_slow_call_fails_the_state() {
    let mut outcomes = Vec::new();
    for (callable, extra) in [
        ("plan", json!({})),
        ("boom", json!({"retries": 1})),
        ("wrong_keys", json!({})),
        ("slow", json!({"budget_ms": 300, "retries": 2})),
    ] {
        let fixture = Fixture::new();
        std::fs::write(fixture.work_dir().join("factory_tools.py"), PYTHON_TOOLS).unwrap();
        let started = fixture
            .start("machine", python_machine(callable, extra))
            .await;
        let status = fixture.settle(&started).await;
        let error = node(&status, "plan")["error"].as_str().map(|error| {
            let last_line = error.lines().last().unwrap_or_default();
            format!(
                "{} ... {last_line}",
                error.lines().next().unwrap_or_default()
            )
        });
        outcomes.push((
            status["state"].clone(),
            events(&status, "started").len(),
            error,
            fixture.host.prompts("review"),
            status["usage"]["spawns"].clone(),
        ));
    }
    assert_eq!(
        outcomes,
        vec![
            (
                json!("done"),
                1,
                None,
                vec![
                    "Review a.py (two files).".to_string(),
                    "Review b.py (two files).".to_string()
                ],
                json!(2),
            ),
            (
                json!("failed"),
                2,
                Some("python state exited with code 1: Traceback (most recent call last): ... RuntimeError: nope".to_string()),
                Vec::new(),
                json!(0),
            ),
            (
                json!("failed"),
                1,
                Some("python state returned keys ['other'] but declares outputs ['files', 'note'] ... python state returned keys ['other'] but declares outputs ['files', 'note']".to_string()),
                Vec::new(),
                json!(0),
            ),
            (
                json!("failed"),
                1,
                Some("python state time limit 300ms exceeded ... python state time limit 300ms exceeded".to_string()),
                Vec::new(),
                json!(0),
            ),
        ]
    );
}

/// `test_stop_kills_a_running_call`.
#[tokio::test(flavor = "multi_thread")]
async fn stop_kills_a_running_python_call() {
    let fixture = Fixture::new();
    std::fs::write(fixture.work_dir().join("factory_tools.py"), PYTHON_TOOLS).unwrap();
    let started = fixture
        .start("machine", python_machine("slow", json!({})))
        .await;
    let run_id = started["run_id"].as_str().unwrap();
    let stopped = fixture.executor.stop(run_id).await.unwrap();
    let status = fixture.executor.status(run_id).unwrap();
    assert_eq!(
        (
            stopped["cancelled"].clone(),
            instance_column(&status, "plan", "status"),
            events(&status, "cancelled").len(),
        ),
        (json!(["plan", "review"]), vec![json!("cancelled")], 1)
    );
}

const NO_OPTIONS: KernelExecuteOptions<'static> = KernelExecuteOptions {
    signal: None,
    timeout_ms: None,
    on_stream: None,
    on_late_sent_agent_message: None,
};

/// What a cell displayed as JSON, or the error it raised.
async fn displayed(kernel: &dyn KernelExecutor, expression: &str) -> Result<Value, String> {
    let cell = format!("(display (value->jsexpr-string {expression}))");
    let result = kernel.execute(&cell, NO_OPTIONS).await.unwrap();
    match result.error {
        None => Ok(serde_json::from_str(&result.stdout).unwrap()),
        Some(error) => Err(error.evalue),
    }
}

/// The Scheme API on a live kernel: a run started in one cell finishes
/// between cells, and later cells read and steer it
/// (`FactoryHelpTest.test_help_advertises_only_calls_the_namespace_has`,
/// `FactoryOptInGateTest.test_help_answers_while_disabled`).
#[tokio::test(flavor = "multi_thread")]
async fn scheme_starts_a_run_that_finishes_between_cells_and_reads_it_back() {
    let fixture = Fixture::new();
    fixture.store(
        "sw",
        "dag",
        json!({"nodes": [worker("a"), {"id": "b", "subagent": "worker", "depends_on": ["a"]}]}),
    );
    let provisioner = fixture.provisioner();
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let kernel = kernel.as_ref();
    let started = displayed(
        kernel,
        r#"(begin (define started (factory-run "sw" #:name "nightly")) started)"#,
    )
    .await
    .unwrap();
    // No cell runs while the control loop takes the run to its end; the
    // daemon lane follows it there.
    loop {
        let watched = provisioner
            .factory_activity(&json!({
                "id": "w", "action": "watch", "runId": started["run_id"], "timeoutMs": 5000,
            }))
            .await
            .unwrap();
        if watched["result"]["state"] != "running" {
            break;
        }
    }
    let status = displayed(kernel, "(factory-status (hash-ref started 'run_id))")
        .await
        .unwrap();
    let watched = displayed(kernel, "(factory-watch (hash-ref started 'run_id) 0.5)")
        .await
        .unwrap();
    let graphs = displayed(
        kernel,
        "(list (hash-ref (hash-ref (factory-graph (hash-ref started 'run_id)) 'machine) 'order) (length (hash-ref (factory-graph) 'runs)))",
    )
    .await
    .unwrap();
    let machines = displayed(
        kernel,
        "(map (lambda (row) (hash-ref row 'name)) (factory-list-machines))",
    )
    .await
    .unwrap();
    let refusals = (
        displayed(kernel, "(factory-resume (hash-ref started 'run_id))").await,
        displayed(kernel, r#"(factory-status "nope")"#).await,
    );
    fixture.settings(&json!({"factory": {"enabled": false}}));
    let disabled = displayed(kernel, r#"(factory-run "sw")"#).await;
    let help = displayed(kernel, "(factory-help)").await.unwrap();
    let help = help.as_str().unwrap();
    let advertised: Vec<&str> = [
        "factory-run",
        "factory-status",
        "factory-stop",
        "factory-resume",
        "factory-graph",
        "factory-watch",
        "factory-validate",
        "factory-canonicalize",
        "factory-compile-dag",
        "factory-list-machines",
        "factory-import-machine",
        "factory-export-machine",
    ]
    .into_iter()
    .filter(|name| !help.contains(&format!("({name}")) && !help.contains(&format!("`{name}`")))
    .collect();
    assert_eq!(
        (
            started["name"].clone(),
            started["started"].clone(),
            status["state"].clone(),
            status["usage"]["spawns"].clone(),
            watched["changed"].clone(),
            graphs,
            machines,
            refusals
                .0
                .map_err(|error| error.contains("is 'done', not paused")),
            refusals
                .1
                .map_err(|error| error.contains("unknown factory run 'nope'")),
            disabled.map_err(|error| error.contains(DISABLED)),
            help.starts_with("# Factory\n"),
            advertised,
        ),
        (
            json!("nightly"),
            json!(["a"]),
            json!("done"),
            json!(2),
            json!(false),
            json!([["a", "b"], 1]),
            json!(["builder", "pr-manager", "review-sweep"]),
            Err(true),
            Err(true),
            Err(true),
            true,
            Vec::<&str>::new(),
        )
    );
}

/// Killing the kernel ends its runs' control loops (the collect a loop was
/// waiting on is abandoned), and the daemon lane then has no kernel to ask.
#[tokio::test(flavor = "multi_thread")]
async fn killing_the_kernel_ends_its_control_loops_and_leaves_the_children_to_the_supervisor() {
    let fixture = Fixture::new();
    fixture.host.set_node("a", Outcome::running());
    fixture.store("sw", "dag", json!({"nodes": [worker("a")]}));
    let provisioner = fixture.provisioner();
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let started = displayed(kernel.as_ref(), r#"(factory-run "sw")"#)
        .await
        .unwrap();
    let request = json!({"id": "f1", "action": "status", "runId": started["run_id"]});
    let while_alive = provisioner.factory_activity(&request).await.unwrap();
    provisioner.kill().await;
    fixture
        .host
        .abandoned_collects
        .acquire()
        .await
        .unwrap()
        .forget();
    let after_kill = provisioner
        .factory_activity(&request)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        (
            while_alive["result"]["state"].clone(),
            while_alive["result"]["usage"]["running"].clone(),
            after_kill,
            fixture.host.deleted(),
        ),
        (
            json!("running"),
            json!(1),
            "the Scheme kernel is not running: it has no factory runs".to_string(),
            Vec::<String>::new(),
        )
    );
}

/// `FactoryOptInGateTest`: `test_create_factory_refuses_while_disabled`,
/// `test_factory_entry_updates_refuse_while_disabled`,
/// `test_delete_stays_available_while_disabled`; and the write-time dry run
/// of every factory write.
#[tokio::test(flavor = "multi_thread")]
async fn a_factory_entry_is_stored_only_validated_and_only_while_the_factory_is_enabled() {
    let fixture = Fixture::new();
    let provisioner = fixture.provisioner();
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let kernel = kernel.as_ref();
    let dag = r#"(hash 'nodes (list (hash 'id "a" 'subagent "worker")))"#;
    let create = |id: &str, spec: &str| {
        format!(
            r#"(hash-ref (harness-create-factory "Factory" "Factory content" #:id "{id}" {spec}) 'id)"#
        )
    };
    let stored = displayed(kernel, &create("sw", &format!("#:dag {dag}"))).await;
    let started = displayed(kernel, r#"(hash-ref (factory-run "sw") 'started)"#).await;
    let invalid = displayed(kernel, &create("bad", "#:dag (hash 'nodes (list))")).await;
    let both = displayed(
        kernel,
        &create("both", &format!("#:dag {dag} #:machine {dag}")),
    )
    .await;
    let neither = displayed(kernel, &create("none", "")).await;
    let updated = displayed(
        kernel,
        r#"(hash-ref (harness-update-factory "sw" "Factory" "Factory content" #:machine (hash 'states (list (hash 'id "b" 'entry #t 'subagent "worker")))) 'version)"#,
    )
    .await;
    fixture.settings(&json!({"factory": {"enabled": false}}));
    let while_disabled = [
        displayed(kernel, &create("off", &format!("#:dag {dag}"))).await,
        // The refusal comes before the spec is looked at.
        displayed(kernel, &create("off-bad", "#:dag (hash 'nodes (list))")).await,
        displayed(
            kernel,
            r#"(harness-update-factory "sw" "Factory" "content updated")"#,
        )
        .await,
    ];
    let kept = displayed(
        kernel,
        "(hash-ref (hash-ref (hash-ref (hash-ref (harness-state) (quote entries)) (quote factory)) (quote sw)) (quote content))",
    )
    .await;
    let deleted = displayed(kernel, r#"(harness-delete-factory "sw")"#).await;
    let refused = |outcome: &Result<Value, String>| {
        outcome
            .as_ref()
            .map_err(|error| error.contains(DISABLED))
            .cloned()
    };
    assert_eq!(
        (
            stored,
            started,
            invalid.map_err(|error| error.contains("factory entry 'bad' rejected: factory dag must declare between 1 and 1024 nodes, got 0")),
            both.map_err(|error| error.contains("pass either #:dag or #:machine, not both")),
            neither.map_err(|error| error.contains("factory entry 'none' rejected: factory entries require a dag or machine object in arguments")),
            updated,
            while_disabled.iter().map(refused).collect::<Vec<_>>(),
            kept,
            deleted,
        ),
        (
            Ok(json!("sw")),
            Ok(json!(["a"])),
            Err(true),
            Err(true),
            Err(true),
            Ok(json!(2)),
            vec![Err(true), Err(true), Err(true)],
            Ok(json!("Factory content")),
            Ok(json!(true)),
        )
    );
}

/// `MachineCliDispatchTest`, all eight:
/// `test_import_dispatch_persists_into_the_user_library`,
/// `test_import_dispatch_surfaces_gate_errors_as_data`,
/// `test_export_dispatch_resolves_library_machines`,
/// `test_export_dispatch_refuses_to_overwrite_the_target`,
/// `test_export_dispatch_resolves_the_library_only`,
/// `test_export_dispatch_reports_unknown_machines`,
/// `test_list_dispatch_lists_the_library_with_warnings`,
/// `test_dispatch_rejects_bad_payloads`.
#[test]
fn the_command_line_facade_lists_imports_and_exports_with_every_failure_as_data() {
    let temp = tempfile::tempdir().unwrap();
    let (repo, agent) = (temp.path().join("repo"), temp.path().join("agent"));
    let environment = FactoryEnvironment {
        agent_dir: Some(agent.clone()),
        // A stored entry of the same name never intercepts an export.
        local_harness_dir: Some(temp.path().join("harness")),
        machines_dir: Some(repo.clone()),
    };
    let write = |path: std::path::PathBuf, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    };
    let text = machine_text("sweep", &json!({"nodes": [worker("a")]}));
    let source = write(temp.path().join("sources/machine.MACHINE.md"), &text);
    let bad = write(
        temp.path().join("sources/bad.MACHINE.md"),
        &machine_text(
            "bad",
            &json!({"run": {"max_parallel": null}, "states": [{"id": "a", "entry": true, "subagent": {"prompt": "P."}}]}),
        ),
    );
    let dispatch = |payload: Value| cli_dispatch(&payload, &environment);
    let out = |name: &str| {
        temp.path()
            .join("out")
            .join(name)
            .to_string_lossy()
            .into_owned()
    };
    let before_any_machine =
        dispatch(json!({"op": "export", "name": "sweep", "out": out("early.md")}));
    let gate_error = dispatch(json!({"op": "import", "path": bad}));
    let nothing_stored = !agent.join("machines").exists();
    let imported = dispatch(json!({"op": "import", "path": source}));
    let stored = agent.join("machines/sweep/MACHINE.md");
    write(
        repo.join("shared/MACHINE.md"),
        &machine_text("shared", &json!({"nodes": [worker("a")]})),
    );
    write(agent.join("machines/broken/MACHINE.md"), "no frontmatter");
    let listed = dispatch(json!({"op": "list"}));
    let exported = dispatch(json!({"op": "export", "name": "sweep", "out": out("sweep.md")}));
    let kept = write(temp.path().join("out/kept.md"), "keep me");
    let refused = dispatch(json!({"op": "export", "name": "sweep", "out": kept}));
    let names: Vec<(Value, Value)> = listed["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["name"].clone(), row["source"].clone()))
        .collect();
    let first_error = |result: &Value| result["errors"][0].as_str().unwrap_or_default().to_string();
    assert_eq!(
        (
            first_error(&before_any_machine),
            (
                gate_error["ok"].clone(),
                first_error(&gate_error),
                nothing_stored
            ),
            imported,
            std::fs::read_to_string(&stored).unwrap() == text,
            (
                listed["ok"].clone(),
                names,
                listed["warnings"].as_array().map(Vec::len)
            ),
            exported,
            std::fs::read_to_string(out("sweep.md")).unwrap() == text,
            (
                refused["ok"].clone(),
                first_error(&refused).contains("already exists"),
                std::fs::read_to_string(&kept).unwrap()
            ),
            vec![
                cli_dispatch(&json!("nope"), &environment),
                dispatch(json!({"op": "import"})),
                dispatch(json!({"op": "export", "out": "x"})),
                dispatch(json!({"op": "export", "name": "sweep"})),
                dispatch(json!({"op": "wat"})),
            ],
        ),
        (
            "unknown machine 'sweep': no MACHINE.md for it in the machine library (machines: none)"
                .to_string(),
            (
                json!(false),
                "run max_parallel must be an integer between 1 and 64".to_string(),
                true
            ),
            json!({"ok": true, "name": "sweep", "path": stored.to_string_lossy(), "created": true}),
            true,
            (
                json!(true),
                vec![
                    (json!("shared"), json!("repo")),
                    (json!("sweep"), json!("user"))
                ],
                Some(1)
            ),
            json!({"ok": true, "name": "sweep", "path": out("sweep.md"), "source": "library"}),
            true,
            (json!(false), true, "keep me".to_string()),
            vec![
                json!({"ok": false, "errors": ["factory cli payload must be a JSON object"]}),
                json!({"ok": false, "errors": ["factory import requires a `path` string"]}),
                json!({"ok": false, "errors": ["factory export requires a `name` string"]}),
                json!({"ok": false, "errors": ["factory export requires an `out` string"]}),
                json!({"ok": false, "errors": ["unknown factory cli op 'wat' (expected 'list', 'import' or 'export')"]}),
            ],
        )
    );
}
