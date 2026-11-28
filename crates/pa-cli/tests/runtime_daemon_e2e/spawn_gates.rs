//! What a subagent spawn admits on the runtime daemon (the supervisor's
//! `rlm_children_e2e` cases on a spawn's gates), each made by the sessions'
//! own kernels:
//!
//! - a child spawned with a relative `#:cwd` runs in that directory and
//!   loads only its project context; a missing directory refuses the spawn;
//! - a session at the recursion bound may not spawn;
//! - a name is admitted once among a parent's children: a second spawn of it
//!   is refused, and once the first child is deleted the name admits again.
//!   (The supervisor raced four spawns from separate clients; on the runtime
//!   daemon every spawn of a parent comes from its one kernel, whose host
//!   serializes them on the parent's spawn gate.)

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::turns::{cell, Attached};
use super::Bed;

/// A form that writes `call`'s value as JSON to `<dir>/<name>.json`, or its
/// error's message to `<name>.error`.
fn record(call: &str, name: &str, dir: &Path) -> String {
    let path = dir.join(format!("{name}.json")).display().to_string();
    let error = dir.join(format!("{name}.error")).display().to_string();
    format!(
        r"(with-handler
  (lambda (e) (fs-write {error:?} (if (error-object? e) (error-object-message e) (to-string e))))
  (fs-write {path:?} (value->jsexpr-string {call})))
"
    )
}

/// The record `name` wrote: its value, or `Err` with its error's message
/// (as the host raised it: the kernel's message leads with a space and
/// escapes its quotes).
fn recorded(dir: &Path, name: &str) -> Result<Value, String> {
    if let Ok(error) = std::fs::read_to_string(dir.join(format!("{name}.error"))) {
        return Err(error.trim().replace("\\\"", "\""));
    }
    let content = std::fs::read_to_string(dir.join(format!("{name}.json")))
        .unwrap_or_else(|_| panic!("{name} wrote no record"));
    Ok(serde_json::from_str(&content).expect("a JSON record"))
}

/// Runs one prompt to its `done`.
async fn run(session: &mut Attached, text: &str) {
    let task = session.publish(text, None).await;
    session.done(&task["task"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spawn_with_a_relative_cwd_starts_the_child_in_that_directory() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    let main = bed.dir.path().join("main");
    let project = bed.dir.path().join("project");
    let missing = project.join("missing");
    for (path, content) in [
        (&main, "SPAWN_CWD_PARENT_CONTEXT"),
        (&project, "SPAWN_CWD_CONTEXT"),
    ] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("AGENTS.md"), content).unwrap();
    }
    std::fs::create_dir_all(&records).unwrap();
    let spawns = format!(
        "{}\n{}",
        record(
            r#"(rlm-spawn "ship the lane" #:name "worker-cwd" #:cwd "../project")"#,
            "spawn",
            &records
        ),
        record(
            &format!(
                r#"(rlm-spawn "ship the lane" #:name "worker-missing" #:cwd {:?})"#,
                missing.display().to_string()
            ),
            "missing",
            &records,
        ),
    );
    // The child holds its turn until the test has read its prompt.
    let release = records.join("release");
    let hold = format!(
        "{}\n(bash {:?})",
        record(r#"(session-info "cwd")"#, "child-cwd", &records),
        format!("until [ -f '{}' ]; do sleep 0.05; done", release.display())
    );
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &spawns), "spawned"],
        "children": { "worker-cwd": { "responses": [cell("holding", &hold), "child answer"] } },
    }));
    let mut parent = Attached::open(&bed, &main).await;
    run(&mut parent, "delegate").await;

    // The child is running its held cell once it recorded its directory.
    super::schedules::until(
        Duration::from_secs(60),
        "the child's cwd record",
        async || {
            ["json", "error"]
                .iter()
                .any(|kind| records.join(format!("child-cwd.{kind}")).exists())
                .then_some(())
        },
    )
    .await;
    let roster = parent.query("session.children").await;
    let child = roster["children"]
        .as_array()
        .expect("a roster")
        .iter()
        .find(|row| row["sessionName"] == "worker-cwd")
        .cloned()
        .unwrap_or_else(|| panic!("the child's row: {roster}"));
    let (mut client, _) = super::Client::connect(&bed.runtime_socket()).await;
    let prompt = super::ok(
        client
            .query(
                "session.system_prompt",
                json!({ "session": child["activeSessionId"] }),
            )
            .await,
    );
    std::fs::write(&release, "go").unwrap();
    let prompt = prompt["systemPrompt"].as_str().unwrap_or_default();
    assert_eq!(
        (
            recorded(&records, "child-cwd"),
            prompt.contains("SPAWN_CWD_CONTEXT"),
            prompt.contains("SPAWN_CWD_PARENT_CONTEXT"),
            recorded(&records, "missing"),
        ),
        (
            Ok(json!(pa_types::platform::canonicalize(&project).unwrap())),
            true,
            false,
            Err(format!(
                "rlm.spawn cwd {} is not a directory",
                missing.display()
            )),
        ),
        "the child's row {child}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_at_the_recursion_bound_may_not_spawn() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    // The root spawns `mid` (depth 1), `mid` spawns `low` (depth 2, the
    // default bound), and `low` tries to spawn.
    let collect = |name: &str| format!(r"(rlm-collect {name:?} #:timeout-ms 60000)");
    let spawn = |name: &str| format!(r#"(rlm-spawn "go deeper" #:name {name:?})"#);
    let low = record(&spawn("too-deep"), "too-deep", &records);
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &format!("{}\n{}", spawn("mid"), collect("mid"))), "done"],
        "children": {
            "mid": { "responses": [cell("spawning", &format!("{}\n{}", spawn("low"), collect("low"))), "done"] },
            "low": { "responses": [cell("spawning", &low), "done"] },
        },
    }));
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut root = Attached::open(&bed, &cwd).await;
    run(&mut root, "delegate").await;
    assert_eq!(
        recorded(&records, "too-deep"),
        Err("RLM recursion depth limit reached (RLM_DEPTH=2, RLM_MAX_DEPTH=2)".to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_is_admitted_once_among_a_parents_children_until_it_is_deleted() {
    let bed = Bed::new();
    let records = bed.dir.path().join("records");
    std::fs::create_dir_all(&records).unwrap();
    let spawn = r#"(rlm-spawn "twin task" #:name "twin-worker")"#;
    let cells = format!(
        "{}\n{}\n(rlm-collect \"twin-worker\" #:timeout-ms 60000)\n{}\n{}",
        record(spawn, "first", &records),
        record(spawn, "second", &records),
        record(r#"(rlm-delete "twin-worker")"#, "delete", &records),
        record(spawn, "again", &records),
    );
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("spawning", &cells), "done"],
        "children": { "twin-worker": { "responses": ["twin answer"], "repeatLastResponse": true } },
    }));
    let cwd = bed.dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut parent = Attached::open(&bed, &cwd).await;
    run(&mut parent, "delegate").await;
    let name = |record: &str| recorded(&records, record).map(|handle| handle["name"].clone());
    assert_eq!(
        (name("first"), name("second"), recorded(&records, "delete").is_ok(), name("again")),
        (
            Ok(json!("twin-worker")),
            Err("Agent name \"twin-worker\" is unavailable: an agent of that name already exists at depth 1 under this parent".to_string()),
            true,
            Ok(json!("twin-worker")),
        )
    );
}
