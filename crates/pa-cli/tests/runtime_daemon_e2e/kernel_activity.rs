//! The agents view's kernel activity on the runtime daemon (phase 8d-4a),
//! sent through pa-tui's `wire2_compat` and answered as the old daemon (the
//! `pa-daemon` supervisor) answers it:
//!
//! - A session's kernel shell commands: a turn starts a background command
//!   (`/bin/sleep` by its path), then `list_kernel_bash`, `tail_kernel_bash`
//!   (its output once written), the refusals of a tail without a command, of
//!   one with too many lines and of a kill of an unknown command, its
//!   `kill_kernel_bash` and the list once it is reaped; then the factory
//!   lane's `factory_activity` refusals and the unscoped graph with the
//!   session's stored specs.
//! - The persistent shells: a saved session's catalog row lists its live
//!   shells and stored factory specs (the factory lane on), and
//!   `list_persistent_shells`, `tail_persistent_shell`, an unknown shell's
//!   refusal, `kill_persistent_shell` and the list after it answer the old
//!   daemon's `persistent_shells_e2e` expectations, on a fixture of shell
//!   records built for each daemon.
//!
//! Each daemon runs on a bed of its own; the ids, pids and times the beds
//! differ in compare by their form.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::os::unix::process::CommandExt;
use std::process::Command;

use pa_types::daemon::DaemonCommand;
use serde_json::{json, Value};

use super::old_reference::OldReference;
use super::saved_list::runtime_list;
use super::switches::read_as_the_tui_does;
use super::turns::{cell, Attached};
use super::{Bed, Client};

/// The runtime daemon runs offline, as the old daemon did for its golden.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// Turns the factory lane on for the daemons `bed` starts (it is opt-in).
fn factory_on(bed: &Bed) {
    std::fs::write(
        bed.agent_dir().join("settings.json"),
        json!({ "factory": { "enabled": true } }).to_string(),
    )
    .unwrap();
}

/// The turn that starts the kernel's background command.
fn responses() -> Value {
    json!([
        cell(
            "Starting it.",
            "(begin (bash \"echo started; /bin/sleep 600\" (hash 'background #t)) 'ok)"
        ),
        "started it",
    ])
}

/// The activity id the first `list_kernel_bash` names.
fn first_activity(listed: &Value) -> String {
    listed["data"]["activities"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no kernel command in {listed}"))
        .to_string()
}

/// `value` with the bed's own pids and times by their form.
fn masked(mut value: Value) -> Value {
    fn mask(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, member) in object.iter_mut() {
                    match key.as_str() {
                        "pid" | "durationMs" if member.is_number() => *member = json!("<n>"),
                        "startedAt" if member.is_string() => *member = json!("<time>"),
                        _ => mask(member),
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(mask),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    mask(&mut value);
    value
}

/// `response` as the TUI reads it, [`masked`], with the kernel command's id
/// `activity` by its form.
fn normalized(response: &Value, activity: &str) -> Value {
    let response = masked(read_as_the_tui_does(response));
    serde_json::from_str(&response.to_string().replace(activity, "<activity>")).unwrap()
}

/// One kernel activity scenario step: a command (without its session),
/// sent until its response's data satisfies `done` (the command's output
/// written, its process reaped).
struct Step {
    command: Value,
    done: fn(&Value) -> bool,
}

fn steps(activity: &str) -> Vec<Step> {
    let once = |command: Value| Step {
        command,
        done: |_| true,
    };
    let tail = |lines: Value| json!({ "type": "tail_kernel_bash", "activityId": activity, "lines": lines });
    vec![
        Step {
            command: tail(json!(5)),
            done: |data| data["tail"] == "started",
        },
        once(json!({ "type": "tail_kernel_bash", "activityId": "" })),
        once(tail(json!(201))),
        once(json!({ "type": "kill_kernel_bash", "activityId": "nope" })),
        once(json!({ "type": "kill_kernel_bash", "activityId": activity })),
        Step {
            command: json!({ "type": "list_kernel_bash" }),
            done: |data| data["activities"][0]["status"] == "finished",
        },
        once(json!({ "type": "factory_activity", "action": "" })),
        once(json!({ "type": "factory_activity", "action": "status" })),
        once(json!({ "type": "factory_activity", "action": "graph" })),
    ]
}

/// What the compat layer hands the TUI for `command` (with the session's id)
/// on the runtime daemon.
pub(super) async fn runtime_answer(session: &mut Attached, command: &Value) -> Value {
    let mut command = command.clone();
    command["activeSessionId"] = json!(session.id);
    let kind = command["type"].as_str().unwrap().to_string();
    let command: DaemonCommand = serde_json::from_value(command).unwrap();
    let request = session.request_id();
    let mut frames = pa_tui::wire2_request_frames(request, command).expect("a mapped command");
    let (Some(frame), None) = (frames.pop(), frames.pop()) else {
        panic!("{kind} is one request");
    };
    let outcome = session.request(&frame).await;
    pa_tui::wire2_response_frame(&kind, outcome).unwrap()
}

/// The runtime daemon's answers, through the compat layer: the first list,
/// then [`steps`].
async fn runtime_kernel_activity(bed: &Bed) -> Vec<Value> {
    factory_on(bed);
    let _daemon = bed.runtime_daemon_with(&json!({ "responses": responses() }), OFFLINE);
    let mut session = Attached::open(bed, bed.dir.path()).await;
    let accepted = session.publish("start", None).await;
    session.done(&accepted["task"]).await;
    let listed = runtime_answer(&mut session, &json!({ "type": "list_kernel_bash" })).await;
    let activity = first_activity(&listed);
    let mut answers = vec![normalized(&listed, &activity)];
    for Step { command, done } in steps(&activity) {
        let response = loop {
            let response = runtime_answer(&mut session, &command).await;
            if done(&response["data"]) {
                break response;
            }
        };
        answers.push(normalized(&response, &activity));
    }
    answers
}

#[tokio::test(flavor = "multi_thread")]
async fn kernel_bash_and_the_factory_lane_answer_as_the_old_daemons_do() {
    let old_bed = Bed::new();
    let old: Vec<Value> = OldReference::open("kernel_activity", &old_bed).take("answers");
    let runtime = runtime_kernel_activity(&Bed::new()).await;

    let refused = |command: &str, error: &str| json!({ "type": "response", "command": command, "success": false, "error": error });
    let row = |status: &str, exit_code: Value| {
        json!({ "id": "<activity>", "command": "echo started; /bin/sleep 600", "pid": "<n>",
                "startedAt": "<time>", "durationMs": "<n>", "status": status,
                "exitCode": exit_code, "persistent": false })
    };
    let answered = |command: &str, data: Value| json!({ "type": "response", "command": command, "success": true, "data": data });
    // The old daemon's answers are the worker's: what the scenario asks of it.
    assert_eq!(
        old[..8],
        [
            answered(
                "list_kernel_bash",
                json!({ "activities": [row("running", Value::Null)] })
            ),
            answered(
                "tail_kernel_bash",
                json!({ "id": "<activity>", "tail": "started" })
            ),
            refused("tail_kernel_bash", "activityId is required"),
            refused("tail_kernel_bash", "lines must be between 1 and 200"),
            refused("kill_kernel_bash", "Unknown kernel bash activity"),
            answered(
                "kill_kernel_bash",
                json!({ "id": "<activity>", "killed": true })
            ),
            answered(
                "list_kernel_bash",
                // The kill's SIGTERM.
                json!({ "activities": [row("finished", json!(143))] })
            ),
            refused("factory_activity", "action is required"),
        ],
        "{old:#?}"
    );
    assert_eq!(
        (&old[8], &old[9]["success"], &old[9]["data"]["factorySpecs"]),
        (
            &refused("factory_activity", "factory activity status requires runId"),
            &json!(true),
            &json!(0)
        ),
        "{old:#?}"
    );
    assert_eq!(runtime, old);
}

/// The persistent shells' sleeper: its own process group, reaped by a thread
/// of its own as soon as it ends (so a kill sees its group gone, not a
/// zombie), and killed at the end.
struct Sleeper(u32);

impl Sleeper {
    fn spawn() -> Self {
        let mut child = Command::new("/bin/sleep")
            .arg("600")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        std::thread::spawn(move || child.wait());
        Sleeper(pid)
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(i32::try_from(self.0).unwrap()),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

/// The saved session the shells are assigned to.
const SESSION: &str = "11111111-2222-3333-4444-555555555555";

/// The persistent shells' fixture under `bed`: a saved session holding two
/// stored factory specs, with the factory lane on; shell `a` (a running
/// sleeper's group, with output) and `b` (a finished one) assigned to it, and
/// shell `c` (the same running group) to another session. Returns the
/// sleeper.
fn shell_fixture(bed: &Bed) -> Sleeper {
    let agent_dir = bed.agent_dir();
    let sessions = agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{SESSION}.jsonl")),
        format!(
            "{}\n",
            json!({ "type": "session", "version": 3, "id": SESSION,
                    "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/tmp" })
        ),
    )
    .unwrap();
    factory_on(bed);
    let harness = agent_dir
        .join("session-artifacts")
        .join(SESSION)
        .join(pa_core::refinement::HARNESS_STATE_DIR_NAME);
    std::fs::create_dir_all(&harness).unwrap();
    let spec = |id: &str| {
        json!({ "id": id, "kind": "factory", "title": id, "content": "", "path": "general",
                "reference": {}, "arguments": {}, "metadata": {}, "source": "agent",
                "created_at": "2026-10-16T00:00:00Z", "updated_at": "2026-10-16T00:00:00Z",
                "version": 1 })
    };
    std::fs::write(
        pa_core::refinement::get_harness_state_path(&harness),
        json!({ "schema": 1, "entries": { "factory": { "f1": spec("f1"), "f2": spec("f2") } },
                "refinements": [] })
        .to_string(),
    )
    .unwrap();
    let sleeper = Sleeper::spawn();
    let mut gone = Command::new("/bin/true").spawn().unwrap();
    gone.wait().unwrap();
    let start_id = pa_core::session::lease::get_process_start_id(sleeper.0).unwrap();
    let home = agent_dir.join("persistent-shells");
    std::fs::create_dir_all(&home).unwrap();
    for (id, pid, start_id, session) in [
        ("a", sleeper.0, start_id.as_str(), SESSION),
        ("b", gone.id(), "proc:0", SESSION),
        ("c", sleeper.0, start_id.as_str(), "another-session"),
    ] {
        let id = id.repeat(32);
        std::fs::write(
            home.join(format!("{id}.json")),
            json!({ "id": id, "pid": pid, "startId": start_id, "command": "sleep 600",
                    "startedAt": "2026-01-01T00:00:00+00:00", "sessionId": session })
            .to_string(),
        )
        .unwrap();
    }
    std::fs::write(
        home.join(format!("{}.out", "a".repeat(32))),
        "one\ntwo\nthree\n",
    )
    .unwrap();
    sleeper
}

/// The persistent-shell commands, after the catalog's.
fn shell_commands() -> Vec<Value> {
    let a = "a".repeat(32);
    vec![
        json!({ "type": "list_persistent_shells" }),
        json!({ "type": "tail_persistent_shell", "shellId": a, "lines": 2 }),
        json!({ "type": "tail_persistent_shell", "shellId": "nope", "lines": 2 }),
        json!({ "type": "kill_persistent_shell", "shellId": a }),
        json!({ "type": "list_persistent_shells" }),
    ]
}

/// What the TUI reads of the session's catalog row and of each
/// [`shell_commands`] response, the machine-wide lists sorted by shell.
fn shell_answers(catalog: &[Value], responses: &[Value]) -> Vec<Value> {
    let row = catalog.last().unwrap()["data"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == SESSION)
        .cloned()
        .expect("the session has a catalog row");
    let mut answers = vec![masked(
        json!({ "persistentShells": row["persistentShells"], "factorySpecs": row["factorySpecs"] }),
    )];
    for response in responses {
        let mut answer = masked(read_as_the_tui_does(response));
        if let Some(rows) = answer
            .pointer_mut("/data/activities")
            .and_then(Value::as_array_mut)
        {
            rows.sort_by_key(|row| row["id"].to_string());
        }
        answers.push(answer);
    }
    answers
}

fn catalog_command(bed: &Bed) -> Value {
    json!({ "type": "list_saved_sessions", "cwd": "/",
            "sessionDir": bed.agent_dir().join("sessions") })
}

#[tokio::test(flavor = "multi_thread")]
async fn persistent_shells_answer_as_the_old_daemons_do() {
    let old_bed = Bed::new();
    let old: Vec<Value> = OldReference::open("persistent_shells", &old_bed).take("answers");
    let runtime = {
        let bed = Bed::new();
        let _sleeper = shell_fixture(&bed);
        let _daemon = bed.runtime_daemon(&json!([]));
        let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
        let catalog = runtime_list(
            &mut client,
            "l1",
            &catalog_command(&bed),
            // The session's row and its progress.
            2,
        )
        .await;
        let mut responses = Vec::new();
        for (index, command) in shell_commands().into_iter().enumerate() {
            let kind = command["type"].as_str().unwrap().to_string();
            let command: DaemonCommand = serde_json::from_value(command).unwrap();
            let mut frames = pa_tui::wire2_request_frames(format!("s{index}"), command)
                .expect("a mapped command");
            let (Some(frame), None) = (frames.pop(), frames.pop()) else {
                panic!("{kind} is one request");
            };
            client.send(&frame).await;
            let outcome = client.reply(&format!("s{index}"), &mut Vec::new()).await;
            responses.push(pa_tui::wire2_response_frame(&kind, outcome).unwrap());
        }
        shell_answers(&catalog, &responses)
    };

    let shell = |id: &str, status: &str, session: &str| {
        let mut row = json!({ "id": id.repeat(32), "command": "sleep 600", "pid": "<n>",
                              "startedAt": "<time>", "status": status, "persistent": true,
                              "sessionId": session, "durationMs": "<n>" });
        // A finished shell wrote no exit status here: it has no code or end.
        if status == "finished" {
            row["exitCode"] = Value::Null;
            row["durationMs"] = Value::Null;
        }
        row
    };
    let listed = |rows: Vec<Value>| {
        json!({ "type": "response", "command": "list_persistent_shells", "success": true,
                "data": { "activities": rows } })
    };
    let a = "a".repeat(32);
    assert_eq!(
        old,
        [
            json!({ "persistentShells": [{ "id": a, "command": "sleep 600", "pid": "<n>",
                                           "startedAt": "<time>", "status": "running",
                                           "persistent": true }],
                    "factorySpecs": 2 }),
            listed(vec![
                shell("a", "running", SESSION),
                shell("b", "finished", SESSION),
                shell("c", "running", "another-session"),
            ]),
            json!({ "type": "response", "command": "tail_persistent_shell", "success": true,
                    "data": { "shellId": a, "tail": "two\nthree" } }),
            json!({ "type": "response", "command": "tail_persistent_shell", "success": false,
                    "error": "Unknown persistent shell" }),
            json!({ "type": "response", "command": "kill_persistent_shell", "success": true,
                    "data": { "shellId": a } }),
            listed(vec![
                shell("a", "finished", SESSION),
                shell("b", "finished", SESSION),
                shell("c", "finished", "another-session"),
            ]),
        ],
        "the old daemon's answers"
    );
    assert_eq!(runtime, old);
}
