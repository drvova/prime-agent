#![cfg_attr(
    unix,
    expect(
        clippy::large_futures,
        reason = "the summary-row runs stay on the stack; each test awaits one at a time"
    )
)]
#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "`summary_rows` reads linearly from the chat run to the session file"
    )
)]
#![cfg_attr(
    unix,
    expect(
        clippy::cast_precision_loss,
        reason = "token counts in a test stay far below the 52 bits an f64 holds exactly"
    )
)]
#![cfg(unix)]

//! E2E verifier for the run summary row under each prompt (`Worked for … · N tool calls · …`):
//! against a real daemon whose session runs the faux provider and the Scheme kernel, a prompt
//! that takes several `eval` cells (one slow) before its reply ends with ONE row covering the
//! whole run: every cell counted, the elapsed time from the prompt, and the tokens and cache
//! share summed over every model response of the run.

mod support;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

use pa_tui::interactive::{
    HeadlessPlan, HeadlessStep, InteractiveOptions, ModelSelection, SessionSelection, UiMode,
};
use serde_json::{json, Value};

const WIDTH: u16 = 160;

/// The slow cell's own floor: a run that waits on it takes at least this long.
const SLOW_CELL_SECS: u64 = 2;

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A faux script: one `eval` tool call per cell, each its own model response, then the reply.
fn script(cells: &[&str]) -> Value {
    let mut responses = cells
        .iter()
        .map(|code| {
            json!({ "content": [{ "type": "toolCall", "name": "eval",
                                  "arguments": { "code": code } }] })
        })
        .collect::<Vec<Value>>();
    responses.push(json!({ "text": "all done" }));
    json!({ "engine": "faux", "responses": responses })
}

/// Run `cells` through a fresh chat (the prompt `go`, then `steps_while_slow`) and return the
/// summary rows the chat drew, beside the rows the session's saved runs call for: per run (a
/// user row and what follows it), the tool calls its model responses made, the tokens and cache
/// share summed over those responses' usage, and an elapsed time the measured window allows
/// (any whole second from the slow cell's floor, for the run that waited on it, to the window
/// from the submit to the row).
async fn summary_rows(
    cells: &[&str],
    steps_while_slow: Vec<HeadlessStep>,
) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    support::scrub_ambient_provider_credentials();
    std::env::set_var("DO_NOT_TRACK", "1");
    let agent_dir = dir.path().join("agent");
    let session_dir = agent_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let socket = dir.path().join("daemon.sock");
    let script_path = dir.path().join("faux.json");
    std::fs::write(&script_path, script(cells).to_string()).expect("write the faux script");
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args(["--mode", "daemon", "--daemon-socket"])
        .arg(&socket)
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for var in [
        pa_daemon::worker::WORKER_ROLE_ENV,
        pa_daemon::worker::WORKER_TOKEN_ENV,
        pa_daemon::worker::WORKER_ACTIVE_SESSION_ID_ENV,
        pa_daemon::worker::WORKER_RECOVERY_JOURNAL_ENV,
        pa_daemon::worker::WORKER_SUPERVISOR_SOCKET_ENV,
        pa_daemon::worker::WORKER_SOCKET_ENV,
        pa_daemon::worker::WORKER_INSTANCE_ID_ENV,
        pa_daemon::worker::WORKER_SCRIPT_ENV,
    ] {
        command.env_remove(var);
    }
    let mut daemon = Daemon {
        child: command.spawn().expect("spawn prime-agent --mode daemon"),
    };
    support::await_listening(&socket, &mut daemon.child);

    let (timestamps, marks) = mpsc::channel();
    let mut steps = vec![
        HeadlessStep::Timestamp(timestamps.clone()),
        HeadlessStep::Submit("go".to_string()),
    ];
    steps.extend(steps_while_slow);
    steps.extend([
        HeadlessStep::WaitRender {
            needle: "all done".to_string(),
            timeout_ms: 60_000,
        },
        HeadlessStep::WaitIdle { timeout_ms: 60_000 },
        HeadlessStep::WaitRender {
            needle: "Worked for".to_string(),
            timeout_ms: 60_000,
        },
        HeadlessStep::Timestamp(timestamps),
    ]);
    let outcome = pa_tui::interactive::run_interactive(
        chat_options(
            socket,
            dir.path().to_path_buf(),
            session_dir.clone(),
            script_path,
        ),
        UiMode::Headless(HeadlessPlan {
            steps,
            width: WIDTH,
            height: 40,
        }),
    )
    .await
    .expect("the chat run");
    let (submitted, _) = marks.recv().expect("the submit mark");
    let (summarized, _) = marks.recv().expect("the summary mark");
    let window = summarized.duration_since(submitted).as_secs();
    let last = outcome.frames.last().expect("frames");
    let drawn = last
        .lines()
        .filter(|line| line.contains("Worked for"))
        .map(|line| line.trim().to_string())
        .collect::<Vec<String>>();

    // The runs as the session saved them, each opened by its user row: the tool calls its
    // model responses made, whether one ran the slow cell, and the usage the row sums.
    let mut runs: Vec<(usize, bool, [u64; 4])> = Vec::new();
    for entry in support::session_texts(&session_dir)
        .into_iter()
        .flat_map(|(_, text)| {
            text.lines()
                .map(|line| serde_json::from_str::<Value>(line).expect("a session row"))
                .collect::<Vec<Value>>()
        })
        .filter(|entry| entry["type"] == "message")
    {
        let message = &entry["message"];
        match message["role"].as_str() {
            Some("user") => runs.push((0, false, [0; 4])),
            Some("assistant") => {
                let (tool_calls, slow, usage) = runs.last_mut().expect("a run opens at its prompt");
                for call in message["content"]
                    .as_array()
                    .expect("the response's content")
                    .iter()
                    .filter(|block| block["type"] == "toolCall")
                {
                    *tool_calls += 1;
                    *slow |= call["arguments"]["code"]
                        .as_str()
                        .is_some_and(|code| code.contains("sleep"));
                }
                for (sum, field) in
                    usage
                        .iter_mut()
                        .zip(["totalTokens", "input", "cacheRead", "cacheWrite"])
                {
                    *sum += message["usage"][field].as_u64().unwrap_or(0);
                }
            }
            _ => {}
        }
    }
    let expected = runs
        .iter()
        .zip(drawn.iter().map(Some).chain(std::iter::repeat(None)))
        .map(
            |(&(tool_calls, slow, [total, input, cache_read, cache_write]), drawn)| {
                let prompt = input + cache_read + cache_write;
                let cache_hit = (prompt > 0)
                    .then(|| cache_read as f64 / prompt as f64 * 100.0)
                    .filter(|hit| hit.round() >= 1.0);
                let s = if tool_calls == 1 { "" } else { "s" };
                let floor = if slow { SLOW_CELL_SECS } else { 0 };
                let candidates = (floor..=window.max(floor))
                    .map(|secs| {
                        pa_tui::chat::RunSummaryRow {
                            text: format!(
                                "Worked for {} \u{b7} {tool_calls} tool call{s} \u{b7} {} tokens",
                                pa_tui::chat::format_working_elapsed(secs),
                                pa_tui::chrome::format_token_count(total),
                            ),
                            cache_hit,
                        }
                        .text_at(usize::from(WIDTH))
                        .into_owned()
                    })
                    .collect::<Vec<String>>();
                drawn
                    .and_then(|drawn| candidates.iter().find(|row| *row == drawn))
                    .unwrap_or(&candidates[0])
                    .clone()
            },
        )
        .collect();
    (drawn, expected)
}

fn chat_options(
    socket: PathBuf,
    cwd: PathBuf,
    session_dir: PathBuf,
    script_path: PathBuf,
) -> InteractiveOptions {
    InteractiveOptions {
        models: None,
        socket_path: socket,
        cwd,
        session_dir: Some(session_dir),
        script_path: Some(script_path),
        model_selection: ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: SessionSelection::New,
        initial_message: None,
        show_images: true,
        fullscreen_mouse: true,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        traces: None,
        provider_auth: None,
        update_commands: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: None,
    }
}

/// The cells of both tests: a quick one, the slow one, and a pure-Scheme one.
const CELLS: [&str; 3] = ["(bash \"echo one\")", "(bash \"sleep 2\")", "(+ 1 2)"];

/// The row counts every `eval` cell of the run and its time from the prompt, not only the
/// model response after the last cell: the daemon opens each model response of the run with a
/// `turn_start`, and none of them starts the run over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_summary_row_covers_every_cell_of_the_run() {
    let (drawn, expected) = summary_rows(&CELLS, Vec::new()).await;
    assert_eq!(drawn, expected);
}

/// A steer typed while the slow cell runs is acknowledged mid-run, and the acknowledgement does
/// not start the run's clock and tallies over. (The daemon delivers the steer at the next
/// boundary as a run of its own, which ends with its own row.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_typed_mid_run_keeps_the_runs_tallies() {
    let steer = vec![
        HeadlessStep::WaitRender {
            needle: "sleep 2".to_string(),
            timeout_ms: 60_000,
        },
        HeadlessStep::Submit("steer".to_string()),
    ];
    let (drawn, expected) = summary_rows(&CELLS, steer).await;
    assert_eq!(drawn, expected);
}
