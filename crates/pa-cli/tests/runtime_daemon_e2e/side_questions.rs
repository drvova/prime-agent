//! A session's side runs on the runtime daemon (phase 8d-3): a `/btw`
//! question, a `/better` rewrite and its `record_better_outcome`, then a
//! question aborted mid-run with the in-flight refusals around it (a taken
//! id, a second run for the client, an abort of an unknown id) and its
//! same-id restart, sent through pa-tui's `wire2_compat`, answer as the old
//! daemon (the `pa-daemon` supervisor) answers them on the same faux
//! script, stream the same `side_question_event` payloads (on
//! `agent/<id>/side_question`, read back through the compat layer), and
//! leave the same `/better` learning on disk. The expectations are the old
//! daemon's `supervisor_e2e` side-question test's, on the faux provider.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pa_types::daemon::DaemonCommand;
use serde_json::{json, Value};

use super::saved_list::old_list;
use super::switches::read_as_the_tui_does;
use super::turns::Attached;
use super::{Bed, FRAME_WAIT};

/// Both daemons run offline.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// How long the `/better` learning may take to reach the disk.
const LEARNING_WAIT: Duration = Duration::from_secs(30);

/// The replies: the turn before the side runs, the question's answer, the
/// rewrite, and one that never streams, which the aborted question asks for
/// (or, aborted before its request, its restart).
fn responses() -> Value {
    json!([
        "Hello back.",
        "The side answer is 42.",
        "<depth>\n(1) a greeting\n</depth>\n<unresolved>\nnone\n</unresolved>\n\
         <rewrite>\nSay hello to the user warmly.\n</rewrite>",
        { "text": "never streamed", "delayMs": 600_000 },
    ])
}

/// One step of the scenario.
enum Step {
    /// A command (without its session), whose response is recorded.
    Send(Value),
    /// Wait until run `id` has ended this many times.
    Ended(&'static str, usize),
}

fn start(id: &str, question: &str) -> Value {
    json!({ "type": "start_side_question", "sideQuestionId": id, "question": question,
            "previousTurns": [] })
}

fn abort(id: &str) -> Value {
    json!({ "type": "abort_side_question", "sideQuestionId": id })
}

fn steps() -> Vec<Step> {
    let mut better = start("b1", "say hi");
    better["kind"] = json!("better");
    vec![
        Step::Send(start("q1", "what is the answer?")),
        Step::Ended("q1", 1),
        Step::Send(better),
        Step::Ended("b1", 1),
        Step::Send(json!({ "type": "record_better_outcome", "outcome": "edited" })),
        // In flight: its id is taken, its client has a run, an unknown id
        // aborts nothing, then its abort ends it.
        Step::Send(start("q2", "still there?")),
        Step::Send(start("q2", "same id?")),
        Step::Send(start("q3", "second?")),
        Step::Send(abort("never-started")),
        Step::Send(abort("q2")),
        Step::Ended("q2", 1),
        // The ended id starts again at once.
        Step::Send(start("q2", "again?")),
        Step::Send(abort("q2")),
        Step::Ended("q2", 2),
    ]
}

/// How many times run `id` ended among `events`.
fn ended(events: &[Value], id: &str) -> usize {
    events
        .iter()
        .filter(|event| event["id"] == id && event["status"] != "running")
        .count()
}

/// `events` cut into their runs, in the order the runs ended: each run's
/// first and last event, and whether every event between is a `running` one
/// whose answer the last one's begins with (how many partials a stream
/// coalesces into depends on timing).
fn runs(events: &[Value]) -> Vec<Value> {
    let mut open: Vec<(String, Vec<Value>)> = Vec::new();
    let mut ended = Vec::new();
    for event in events {
        let id = event["id"].as_str().unwrap().to_string();
        let index = open
            .iter()
            .position(|(open_id, _)| *open_id == id)
            .unwrap_or_else(|| {
                open.push((id, Vec::new()));
                open.len() - 1
            });
        open[index].1.push(event.clone());
        if event["status"] != "running" {
            let (_, run) = open.remove(index);
            let last = run.last().unwrap();
            let answer = last["answer"].as_str().unwrap();
            let prefixes = run[1..run.len() - 1].iter().all(|event| {
                event["status"] == "running"
                    && answer.starts_with(event["answer"].as_str().unwrap())
            });
            ended.push(json!({ "first": run[0], "last": last, "partialsArePrefixes": prefixes }));
        }
    }
    ended
}

/// The `/better` learning the outcome left under `agent_dir`, once it is
/// on disk.
fn learning(agent_dir: &Path) -> Value {
    let dir = agent_dir.join("better-learning");
    let deadline = Instant::now() + LEARNING_WAIT;
    loop {
        let stored = std::fs::read_dir(&dir).ok().and_then(|mut entries| {
            let path = entries.next()?.ok()?.path();
            serde_json::from_slice::<Value>(&std::fs::read(path).ok()?).ok()
        });
        if let Some(stored) = stored {
            return stored;
        }
        assert!(
            Instant::now() < deadline,
            "no learning in {}",
            dir.display()
        );
        std::thread::yield_now();
    }
}

/// What one daemon did: its responses as the TUI reads them, its runs'
/// events, and the learning it kept.
#[derive(Debug, PartialEq)]
struct Recorded {
    responses: Vec<Value>,
    runs: Vec<Value>,
    learning: Value,
}

/// An old-wire connection that keeps every frame it reads.
struct OldClient {
    writer: UnixStream,
    frames: mpsc::Receiver<Value>,
    seen: Vec<Value>,
}

impl OldClient {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).expect("connect to the old daemon");
        let reader = BufReader::new(writer.try_clone().unwrap());
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                    if sender.send(frame).is_err() {
                        break;
                    }
                }
            }
        });
        OldClient {
            writer,
            frames,
            seen: Vec::new(),
        }
    }

    /// Reads frames until `done` holds for what has arrived.
    fn until(&mut self, done: impl Fn(&[Value]) -> bool) {
        while !done(&self.seen) {
            let frame = self
                .frames
                .recv_timeout(FRAME_WAIT)
                .expect("a frame from the old daemon");
            self.seen.push(frame);
        }
    }

    /// Sends `command` as request `id`; its response.
    fn send(&mut self, id: &str, command: &Value) -> Value {
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        self.writer
            .write_all(format!("{envelope}\n").as_bytes())
            .unwrap();
        let answered = |frame: &Value| frame["type"] == "response" && frame["id"] == id;
        self.until(|seen| seen.iter().any(answered));
        self.seen
            .iter()
            .find(|frame| answered(frame))
            .unwrap()
            .clone()
    }

    /// The side-question events so far, for session `active`.
    fn side_events(&self, active: &str) -> Vec<Value> {
        self.seen
            .iter()
            .filter(|frame| frame["type"] == "side_question_event")
            .map(|frame| {
                assert_eq!(frame["activeSessionId"], active, "{frame}");
                frame["event"].clone()
            })
            .collect()
    }
}

/// The old daemon's run of [`steps`] after one turn.
fn old_record(bed: &Bed) -> Recorded {
    let script = bed.dir.path().join("old-script.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "modelId": "faux-1", "modelName": "Faux Model",
                "reasoning": false, "contextWindow": 128_000, "responses": responses() })
        .to_string(),
    )
    .unwrap();
    let _daemon = bed.old_daemon_with(OFFLINE);
    let socket = bed.old_socket();
    let created = old_list(
        &socket,
        "create",
        &json!({ "type": "create", "config": {
            "cwd": bed.dir.path(), "sessionDir": bed.agent_dir().join("sessions"),
            "script": script,
        } }),
    );
    let created = created.last().unwrap();
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["id"].as_str().unwrap().to_string();
    let mut client = OldClient::connect(&socket);
    let with_session = |mut command: Value| {
        command["activeSessionId"] = json!(active);
        command
    };
    let attached = client.send("attach", &with_session(json!({ "type": "attach" })));
    assert_eq!(attached["success"], true, "{attached}");
    let ran = client.send(
        "turn",
        &with_session(json!({ "type": "prompt_and_wait", "message": "hello" })),
    );
    assert_eq!(ran["success"], true, "{ran}");
    let mut responses = Vec::new();
    for (index, step) in steps().into_iter().enumerate() {
        match step {
            Step::Send(command) => {
                let response = client.send(&format!("s{index}"), &with_session(command));
                responses.push(read_as_the_tui_does(&response));
            }
            Step::Ended(id, count) => {
                let active = active.clone();
                client.until(|seen| {
                    let events: Vec<Value> = seen
                        .iter()
                        .filter(|frame| {
                            frame["type"] == "side_question_event"
                                && frame["activeSessionId"] == active.as_str()
                        })
                        .map(|frame| frame["event"].clone())
                        .collect();
                    ended(&events, id) >= count
                });
            }
        }
    }
    Recorded {
        responses,
        runs: runs(&client.side_events(&active)),
        learning: learning(&bed.agent_dir()),
    }
}

/// The side-question events session `session` has received so far, as the
/// compat layer hands them to the TUI.
fn runtime_side_events(session: &Attached) -> Vec<Value> {
    pa_tui::wire2_live_frames(&session.events("side_question"))
        .unwrap()
        .into_iter()
        .map(|frame| {
            assert_eq!(
                (&frame["type"], &frame["activeSessionId"]),
                (&json!("side_question_event"), &json!(session.id)),
                "{frame}"
            );
            frame["event"].clone()
        })
        .collect()
}

/// The runtime daemon's run of [`steps`] after one turn, through the compat
/// layer.
async fn runtime_record(bed: &Bed) -> Recorded {
    let _daemon = bed.runtime_daemon_with(&json!({ "responses": responses() }), OFFLINE);
    let mut session = Attached::open(bed, bed.dir.path()).await;
    let accepted = session.publish("hello", None).await;
    session.done(&accepted["task"]).await;
    let mut responses = Vec::new();
    for (index, step) in steps().into_iter().enumerate() {
        match step {
            Step::Send(mut command) => {
                command["activeSessionId"] = json!(session.id);
                let kind = command["type"].as_str().unwrap().to_string();
                let command: DaemonCommand = serde_json::from_value(command).unwrap();
                let mut frames = pa_tui::wire2_request_frames(format!("s{index}"), command)
                    .expect("a mapped command");
                let (Some(call), None) = (frames.pop(), frames.pop()) else {
                    panic!("{kind} is one call");
                };
                // The run's first events can come before the reply: the
                // session keeps them.
                let outcome = session.request(&call).await;
                responses.push(read_as_the_tui_does(
                    &pa_tui::wire2_response_frame(&kind, outcome).unwrap(),
                ));
            }
            Step::Ended(id, count) => {
                session
                    .until(|session| ended(&runtime_side_events(session), id) >= count)
                    .await;
            }
        }
    }
    Recorded {
        responses,
        runs: runs(&runtime_side_events(&session)),
        learning: learning(&bed.agent_dir()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn side_runs_answer_and_stream_as_the_old_daemons_do() {
    let old = old_record(&Bed::new());
    let runtime = runtime_record(&Bed::new()).await;

    let answered =
        |command: &str| json!({ "type": "response", "command": command, "success": true });
    let refused = |error: &str| {
        json!({ "type": "response", "command": "start_side_question", "success": false,
                "error": error })
    };
    let aborted = |aborted: bool| {
        json!({ "type": "response", "command": "abort_side_question", "success": true,
                "data": { "aborted": aborted } })
    };
    let event = |id: &str, question: &str, answer: &str, status: &str| json!({ "id": id, "question": question, "answer": answer, "status": status });
    let run = |id: &str, question: &str, answer: &str, status: &str| {
        json!({ "first": event(id, question, "", "running"),
                "last": event(id, question, answer, status), "partialsArePrefixes": true })
    };
    assert_eq!(
        (&old.responses, &old.runs),
        (
            &vec![
                answered("start_side_question"),
                answered("start_side_question"),
                answered("record_better_outcome"),
                answered("start_side_question"),
                refused("Side question already exists: q2"),
                refused("A side question is already running for this client and session"),
                aborted(false),
                aborted(true),
                answered("start_side_question"),
                aborted(true),
            ],
            &vec![
                run(
                    "q1",
                    "what is the answer?",
                    "The side answer is 42.",
                    "complete"
                ),
                run("b1", "say hi", "Say hello to the user warmly.", "complete"),
                run("q2", "still there?", "", "cancelled"),
                run("q2", "again?", "", "cancelled"),
            ]
        )
    );
    assert!(old.learning["estimate"].is_object(), "{}", old.learning);
    assert_eq!(runtime, old);
}
