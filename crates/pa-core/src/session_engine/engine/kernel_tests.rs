//! A session's kernel seams over real sessions: the model's turns are
//! scripted, the kernel, its shell commands and its MCP server are real.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pa_agent::scripted::ScriptedProvider;
use serde_json::{json, Value};

use super::*;

/// How long a test waits for something that must happen before it fails.
const PATIENCE: Duration = Duration::from_secs(30);

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 100_000,
        max_tokens: 1_000,
        max_tokens_explicit: false,
    }
}

/// A daemonless session in `dir` (its project in `dir/project`, its agent
/// directory in `dir/agent`) whose model is `provider`, changed by `adjust`.
async fn session(
    provider: &Arc<ScriptedProvider>,
    dir: &Path,
    adjust: impl FnOnce(&mut SessionEngineConfig),
) -> SessionEngine {
    let cwd = dir.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut config = SessionEngineConfig {
        cwd,
        agent_dir: dir.join("agent"),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        daemonless: true,
        ..Default::default()
    };
    adjust(&mut config);
    create_session(config).await.unwrap()
}

/// Run one turn in which the model evaluates `code` and then answers.
async fn run_cell(engine: &SessionEngine, provider: &ScriptedProvider, code: &str) {
    provider.push_tool_call_turn(None, vec![("cell", "eval", json!({ "code": code }))]);
    provider.push_text_turn("done");
    engine.prompt("go", PromptOptions::default()).await.unwrap();
    engine.session.agent().wait_for_idle().await;
}

/// The first `count` lines written to the named pipe at `path`, which the
/// returned reader holds open at both ends: a writer never waits for a
/// reader, and the lines are awaited, never polled for.
fn piped_lines(
    path: &Path,
) -> tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::pipe::Receiver>> {
    use tokio::io::AsyncBufReadExt as _;
    assert!(pa_types::session_scope::std_command("mkfifo")
        .arg(path)
        .status()
        .unwrap()
        .success());
    tokio::io::BufReader::new(
        tokio::net::unix::pipe::OpenOptions::new()
            .read_write(true)
            .open_receiver(path)
            .unwrap(),
    )
    .lines()
}

/// The text of each cell result in the model's latest request, in order.
fn cell_results(provider: &ScriptedProvider) -> Vec<String> {
    provider.calls().last().map_or_default(|call| {
        call.messages
            .iter()
            .filter_map(|message| match message {
                pa_agent::types::Message::ToolResult(result) => Some(
                    result
                        .content
                        .iter()
                        .map(|block| match block {
                            pa_agent::types::ToolResultContent::Text(text) => text.text.as_str(),
                            pa_agent::types::ToolResultContent::Image(_) => "",
                        })
                        .collect::<String>(),
                ),
                pa_agent::types::Message::User(_) | pa_agent::types::Message::Assistant(_) => None,
            })
            .collect()
    })
}

/// The session's kernel tool is `eval` alone, and its cells share one engine.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_has_the_scheme_kernel_as_its_only_kernel_tool() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = session(&provider, dir.path(), |_| {}).await;
    let tools: Vec<String> = engine
        .session
        .agent()
        .state()
        .await
        .tools
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    run_cell(&engine, &provider, "(define base 40) (+ base 2)").await;
    run_cell(&engine, &provider, "(displayln \"again\") (+ base 1)").await;
    assert_eq!(
        (tools, cell_results(&provider)),
        (
            vec!["eval".to_string()],
            vec!["42".to_string(), "again\n\n41".to_string()]
        )
    );
    engine.dispose_kernel().await;
}

/// A command left running by a cell is what the session's liveness probes
/// see, what the user's activity lane lists, reads and stops, and what the
/// settle callback reports once it is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_command_is_live_work_the_user_can_read_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let (settled, mut settlements) = tokio::sync::mpsc::unbounded_channel();
    let engine = session(&provider, dir.path(), |config| {
        config.on_background_work_settled = Some(Arc::new(move || {
            // The test may have stopped listening.
            let _ = settled.send(());
        }));
    })
    .await;
    // The command says so through the pipe once its first line is written.
    let ready = dir.path().join("ready");
    let mut said = piped_lines(&ready);
    let command = format!(
        "echo started; echo go > '{}'; exec sleep 600",
        ready.display()
    );
    run_cell(
        &engine,
        &provider,
        &format!("(bash {} (hash 'background #t))", json!(command)),
    )
    .await;
    tokio::time::timeout(PATIENCE, said.next_line())
        .await
        .expect("the command never ran")
        .unwrap();

    let live = engine.has_background_bash();
    let listed = engine.bash_activity("list", None, 50).await.unwrap();
    let id = listed["activities"][0]["id"].as_str().unwrap().to_string();
    let row = |listed: &Value| {
        json!({
            "rows": listed["activities"].as_array().map(Vec::len),
            "command": listed["activities"][0]["command"],
            "status": listed["activities"][0]["status"],
            "persistent": listed["activities"][0]["persistent"],
        })
    };
    // The pipe says the command wrote its line, not that the kernel's reader
    // has collected it: the tail is read until the line shows.
    let tail = tokio::time::timeout(PATIENCE, async {
        loop {
            let tail = engine.bash_activity("tail", Some(&id), 5).await.unwrap();
            if tail["tail"] != "" {
                break tail;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the command's output never reached its tail");
    let unknown = engine
        .bash_activity("tail", Some("no-such-command"), 5)
        .await
        .map_err(|error| error.to_string());
    let killed = engine.bash_activity("kill", Some(&id), 50).await.unwrap();
    tokio::time::timeout(PATIENCE, settlements.recv())
        .await
        .unwrap()
        .unwrap();
    let after = engine.bash_activity("list", None, 50).await.unwrap();
    assert_eq!(
        (
            live,
            row(&listed),
            tail,
            unknown,
            killed,
            engine.has_background_bash(),
            row(&after),
        ),
        (
            true,
            json!({ "rows": 1, "command": command, "status": "running", "persistent": false }),
            json!({ "activityId": id, "tail": "started" }),
            Err("Unknown kernel bash activity".to_string()),
            json!({ "activityId": id, "killed": true }),
            false,
            json!({ "rows": 1, "command": command, "status": "finished", "persistent": false }),
        )
    );
    engine.dispose_kernel().await;
}

/// The pid of the long-running program a cell of `engine` leaves running:
/// `/bin/sleep`, by its path, whose pid the command writes into a pipe in
/// `dir`. A job's own `pid` names no process under the built-in shell, and a
/// bare `sleep` is that shell's own.
async fn left_running(engine: &SessionEngine, provider: &ScriptedProvider, dir: &Path) -> u32 {
    let started = dir.join("started");
    let mut pids = piped_lines(&started);
    let command = format!("/bin/sleep 600 & echo $! > '{}'; wait", started.display());
    run_cell(
        engine,
        provider,
        &format!("(bash {} (hash 'background #t))", json!(command)),
    )
    .await;
    tokio::time::timeout(PATIENCE, pids.next_line())
        .await
        .expect("the command never ran")
        .unwrap()
        .and_then(|pid| pid.trim().parse().ok())
        .expect("the command wrote its program's pid")
}

/// Ending a session ends the commands its cells started.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn disposing_a_session_kills_the_commands_its_cells_left_running() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = session(&provider, dir.path(), |_| {}).await;
    let pid = left_running(&engine, &provider, dir.path()).await;
    engine.dispose_kernel().await;
    tokio::time::timeout(PATIENCE, crate::platform::process::wait_for_exit(pid))
        .await
        .expect("the command outlived its session")
        .unwrap();
}

/// A session dropped without being disposed leaves no command behind either.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_session_kills_the_commands_its_cells_left_running() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = session(&provider, dir.path(), |_| {}).await;
    let pid = left_running(&engine, &provider, dir.path()).await;
    drop(engine);
    tokio::time::timeout(PATIENCE, crate::platform::process::wait_for_exit(pid))
        .await
        .expect("the command outlived its session")
        .unwrap();
}

/// How many times the kernel-reset notice is in the model's latest request.
fn reset_notices(provider: &ScriptedProvider) -> usize {
    provider.calls().last().map_or(0, |call| {
        serde_json::to_string(&call.messages)
            .unwrap()
            .matches("[kernel-state-reset]")
            .count()
    })
}

/// A resumed session starts on a clean engine, and the model is told once:
/// not again on a later resume with no cell in between.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_session_starts_clean_and_is_told_once() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&cwd).unwrap();
    let first = Arc::new(ScriptedProvider::new(model()));
    let manager = crate::session::manager::SessionManager::persisted(&cwd, &sessions);
    let engine = session(&first, dir.path(), |config| {
        config.session_manager = Some(manager);
    })
    .await;
    run_cell(&engine, &first, "(define kept 7) kept").await;
    let told_before_any_restart = reset_notices(&first);
    let file = engine
        .session
        .session_handle()
        .lock()
        .await
        .get_session_file()
        .expect("a persisted session has a file")
        .to_path_buf();
    engine.dispose_kernel().await;
    drop(engine);

    let mut told = Vec::new();
    let mut kept = Vec::new();
    for cell in [Some("kept"), None, None] {
        let provider = Arc::new(ScriptedProvider::new(model()));
        let manager = crate::session::manager::SessionManager::open(&cwd, &sessions, &file);
        let engine = session(&provider, dir.path(), |config| {
            config.session_manager = Some(manager);
        })
        .await;
        if let Some(code) = cell {
            run_cell(&engine, &provider, code).await;
            kept.push(cell_results(&provider).last().map(String::as_str) == Some("7"));
        } else {
            provider.push_text_turn("nothing to run");
            engine.prompt("go", PromptOptions::default()).await.unwrap();
            engine.session.agent().wait_for_idle().await;
        }
        told.push(reset_notices(&provider));
        engine.dispose_kernel().await;
    }
    // The first resume ran a cell after it was told, so the second resume
    // tells again: its request holds two notices. The third resume follows
    // no cell and adds none.
    assert_eq!(
        (told_before_any_restart, told, kept),
        (0, vec![1, 2, 2], vec![false])
    );
}

/// The project card learns the build and test commands cells run.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_a_cell_runs_teaches_the_project_card() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = session(&provider, dir.path(), |_| {}).await;
    run_cell(
        &engine,
        &provider,
        "(bash \"make a-target-this-project-lacks\")",
    )
    .await;
    let card = crate::project_card::ProjectCard::registered(&dir.path().join("project"))
        .and_then(|card| card.render(None));
    assert!(
        card.as_deref()
            .is_some_and(|card| card.contains("make a-target-this-project-lacks")),
        "{card:?}"
    );
    engine.dispose_kernel().await;
}

/// A session that starts its kernel early opens the user's MCP servers in
/// the background with no cell asking for them, and ending the session ends
/// the server processes.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn the_users_mcp_servers_open_at_session_start_and_end_with_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let server = dir.path().join("server.sh");
    std::fs::write(
        &server,
        include_str!("../../kernel/scheme/mcp/stdio_fixture.sh"),
    )
    .unwrap();
    // The server appends a line per message it handles.
    let log = dir.path().join("log");
    let mut handled = piped_lines(&log);
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({
            "mcpServers": {
                "files": { "type": "stdio", "command": "sh", "args": [server, log] },
            },
        })
        .to_string(),
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = session(&provider, dir.path(), |config| {
        config.prewarm_kernel = Some(true);
    })
    .await;
    let opened = tokio::time::timeout(PATIENCE, async {
        let mut opened = Vec::new();
        while opened.len() < 3 {
            opened.extend(handled.next_line().await.unwrap());
        }
        opened
    })
    .await
    .expect("no cell ran, and the server was never opened");
    run_cell(
        &engine,
        &provider,
        "(display (mcp-call-tool \"files\" \"pid\"))",
    )
    .await;
    let pid: u32 = cell_results(&provider)
        .last()
        .and_then(|pid| pid.trim().parse().ok())
        .expect("the cell printed the server's pid");
    engine.dispose_kernel().await;
    let ended = tokio::time::timeout(PATIENCE, crate::platform::process::wait_for_exit(pid))
        .await
        .is_ok_and(|waited| waited.is_ok());
    assert_eq!(
        (opened, ended),
        (
            ["initialize", "initialized", "tools/list"]
                .map(str::to_string)
                .to_vec(),
            true
        )
    );
}

/// The repeated-call loop guard over a real session and kernel: a run whose
/// model issues the same cell six times, each returning the same result, is
/// ended at the sixth with a row saying the loop was cut and the end reason
/// `loop_guard` (no seventh request is made). A run repeating one cell whose
/// result changes each time is nudged from the third round on and never
/// stopped.
#[tokio::test(flavor = "multi_thread")]
async fn six_identical_cells_with_identical_results_end_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&cwd).unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let manager = crate::session::manager::SessionManager::persisted(&cwd, &sessions);
    let engine = session(&provider, dir.path(), |config| {
        config.session_manager = Some(manager);
    })
    .await;
    let mut requests = Vec::new();
    for _ in 0..6 {
        provider.push_tool_call_turn(
            None,
            vec![("cell", "eval", json!({ "code": "(define n 0) (+ 1 1)" }))],
        );
    }
    engine.prompt("go", PromptOptions::default()).await.unwrap();
    engine.session.agent().wait_for_idle().await;
    requests.push(provider.calls().len());
    for _ in 0..7 {
        provider.push_tool_call_turn(
            None,
            vec![("cell", "eval", json!({ "code": "(set! n (+ n 1)) n" }))],
        );
    }
    provider.push_text_turn("done");
    engine
        .prompt("again", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    requests.push(provider.calls().len());
    let file = engine
        .session
        .session_handle()
        .lock()
        .await
        .get_session_file()
        .expect("a persisted session has a file")
        .to_path_buf();
    engine.dispose_kernel().await;
    let transcript = std::fs::read_to_string(file).unwrap();
    let results: Vec<Vec<String>> = transcript
        .lines()
        .filter_map(|line| {
            let entry: Value = serde_json::from_str(line).ok()?;
            match serde_json::from_value(entry.get("message")?.clone()).ok()? {
                pa_agent::types::Message::ToolResult(result) => Some(
                    result
                        .content
                        .iter()
                        .map(|block| match block {
                            pa_agent::types::ToolResultContent::Text(text) => text.text.clone(),
                            pa_agent::types::ToolResultContent::Image(_) => String::new(),
                        })
                        .collect(),
                ),
                pa_agent::types::Message::User(_) | pa_agent::types::Message::Assistant(_) => None,
            }
        })
        .collect();
    let end_reasons: Vec<_> =
        crate::session_engine::run_metrics::transcript_run_metrics(&transcript)
            .into_iter()
            .map(|run| run.end_reason)
            .collect();

    let nudge = |round: usize| {
        format!(
            "[loop guard] This is round {round} in a row calling `eval` with identical arguments. If its result is not changing, do not run it again unchanged: change the input or take a different approach."
        )
    };
    let result = |value: &str, round: usize| {
        let mut blocks = vec![value.to_string()];
        if round >= 3 {
            blocks.push(nudge(round));
        }
        blocks
    };
    let mut expected: Vec<Vec<String>> = (1..=6).map(|round| result("2", round)).collect();
    expected[5].push(
        "[loop guard] Turn stopped: this run has issued this same cell 6 times and it returned the same result every time, so the loop was cut here. Do not run it again unchanged: say what is blocking you, or take a different approach."
            .to_string(),
    );
    expected.extend((1..=7).map(|round| result(&round.to_string(), round)));
    assert_eq!(
        (requests, end_reasons, results),
        (
            vec![6, 6 + 8],
            vec![
                crate::session_engine::run_metrics::TurnEndReason::LoopGuard,
                crate::session_engine::run_metrics::TurnEndReason::Stop,
            ],
            expected,
        )
    );
}
