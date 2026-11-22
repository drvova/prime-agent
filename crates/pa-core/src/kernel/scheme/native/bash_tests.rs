//! `bash` and its jobs with real processes, through a session's kernel.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use steel::steel_vm::engine::Engine;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use super::{summary, SCHEME};
use crate::kernel::cell_window::{CellWindow, CellWindowConfig};
use crate::kernel::error_context::ErrorContext;
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::kernel::shared::{
    host_handler, HostRequestHandlers, HostRequestPayload, KernelBashCommands, KernelBashWaits,
};
use crate::kernel::spill::{SpillLookup, SpillStore, StoredSpill};
use crate::tools::kernel_cell::{
    ExecuteResult, KernelExecuteOptions, KernelExecutor, KernelProvisioner,
};

/// How long any one step may take before its test fails instead of hanging.
const STEP: Duration = Duration::from_secs(60);

/// Handlers answering each request type with its reply, and every request
/// they receive, as it arrives.
fn recording(
    replies: &[(&str, Value)],
) -> (HostRequestHandlers, UnboundedReceiver<(String, Value)>) {
    let (sent, received) = unbounded_channel();
    let mut handlers = HostRequestHandlers::new();
    for (request_type, reply) in replies {
        let (sent, name, reply) = (sent.clone(), (*request_type).to_string(), reply.clone());
        handlers.register(
            *request_type,
            host_handler(move |payload: HostRequestPayload| {
                // A test that has ended no longer listens.
                let _ = sent.send((name.clone(), payload.data));
                let reply = reply.clone();
                async move { Ok(reply) }
            }),
        );
    }
    (handlers, received)
}

/// The next request the handlers received.
async fn next(received: &mut UnboundedReceiver<(String, Value)>) -> (String, Value) {
    tokio::time::timeout(STEP, received.recv())
        .await
        .unwrap()
        .unwrap()
}

/// Run one cell. `limit` is the call's own time limit: a cell without a yield
/// window is stopped at it, and a session with one reads it as the window.
async fn run_within(
    kernel: &dyn KernelExecutor,
    code: &str,
    limit: Option<Duration>,
) -> ExecuteResult {
    kernel
        .execute(
            code,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: limit.map(|limit| limit.as_millis() as u64),
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        )
        .await
        .unwrap()
}

async fn run(kernel: &dyn KernelExecutor, code: &str) -> ExecuteResult {
    run_within(kernel, code, Some(STEP)).await
}

/// A cell's value as the kernel prints it, or the error it raised.
async fn value(kernel: &dyn KernelExecutor, code: &str) -> String {
    let result = run(kernel, code).await;
    result
        .result
        .or(result.error.map(|error| error.evalue))
        .unwrap_or_default()
}

/// The numbers in a cell's printed value, in order.
fn numbers(printed: &str) -> Vec<u32> {
    printed
        .split(|letter: char| !letter.is_ascii_digit())
        .filter_map(|number| number.parse().ok())
        .collect()
}

/// The process `pid` names is gone within the wait.
async fn exited(pid: u32) -> bool {
    tokio::time::timeout(STEP, crate::platform::process::wait_for_exit(pid))
        .await
        .is_ok_and(|waited| waited.is_ok())
}

/// A pipe a command and its test meet on: reading it returns once the other
/// side has written and closed it.
fn pipe_in(dir: &Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
    path
}

async fn read(pipe: &Path) -> String {
    tokio::time::timeout(STEP, tokio::fs::read_to_string(pipe))
        .await
        .unwrap()
        .unwrap()
}

async fn write(pipe: &Path, text: &str) {
    tokio::time::timeout(STEP, tokio::fs::write(pipe, text))
        .await
        .unwrap()
        .unwrap();
}

async fn kernel_with(
    options: SchemeKernelOptions,
) -> (SchemeKernelProvisioner, Box<dyn KernelExecutor>) {
    let provisioner = SchemeKernelProvisioner::with_options(options);
    let kernel = provisioner.ensure(None, None).await.unwrap();
    (provisioner, kernel)
}

async fn kernel_in(
    handlers: HostRequestHandlers,
    cwd: &Path,
) -> (SchemeKernelProvisioner, Box<dyn KernelExecutor>) {
    kernel_with(SchemeKernelOptions {
        host_handlers: handlers,
        cwd: cwd.to_path_buf(),
        ..SchemeKernelOptions::default()
    })
    .await
}

/// A kernel whose cells move to the background after 100 ms.
async fn windowed_kernel_in(cwd: &Path) -> (SchemeKernelProvisioner, Box<dyn KernelExecutor>) {
    kernel_with(SchemeKernelOptions {
        cwd: cwd.to_path_buf(),
        cell_window: CellWindow::for_session(CellWindowConfig {
            daemonless: false,
            configured_ms: Some(100),
            agent_dir: cwd,
            project_dir: cwd,
            on_armed: None,
        }),
        ..SchemeKernelOptions::default()
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_job_is_tailed_polled_waited_and_listed() {
    let dir = tempfile::tempdir().unwrap();
    let (started, gate) = (pipe_in(dir.path(), "started"), pipe_in(dir.path(), "gate"));
    let (_provisioner, kernel) = kernel_in(HostRequestHandlers::new(), dir.path()).await;
    let begun = value(
        kernel.as_ref(),
        "(define job (bash \"echo first; echo up > started; read -r line < gate; echo \\\"got $line\\\"\"
                           (hash 'background #t)))
         (list (hash-ref job 'running) (hash-ref job 'queued) (hash-ref job 'persistent))",
    )
    .await;
    read(&started).await;
    let running = value(
        kernel.as_ref(),
        "(define (shown) (let ([tail (bash-tail job)]) (if (equal? tail \"\") (shown) tail)))
         (list (shown) (bash-poll job) (bash-running? job)
               (map (lambda (row) (hash-ref row 'status)) (bash-jobs)))",
    )
    .await;
    write(&gate, "go\n").await;
    let ended = value(
        kernel.as_ref(),
        "(define result (bash-wait job))
         (define (ended) (if (bash-running? job) (ended) (bash-poll job)))
         (list (hash-ref result 'exit-code) (hash-ref result 'output) (hash-ref result 'running)
               (equal? (hash-ref (ended) 'output) (bash-output job))
               (bash-tail job 1)
               (map (lambda (row) (list (hash-ref row 'status) (hash-ref row 'exit-code))) (bash-jobs)))",
    )
    .await;
    assert_eq!(
        (begun.as_str(), running.as_str(), ended.as_str()),
        (
            "(#true #false #false)",
            "(\"first\" #false #true (\"running\"))",
            "(0 \"first\\ngot go\\n\" #false #true \"got go\" ((\"finished\" 0)))",
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_a_job_ends_every_process_of_its_group() {
    let dir = tempfile::tempdir().unwrap();
    let started = pipe_in(dir.path(), "started");
    let (_provisioner, kernel) = kernel_in(HostRequestHandlers::new(), dir.path()).await;
    let shell = value(
        kernel.as_ref(),
        "(define job (bash \"sleep 300 & echo $! > child.pid; echo up > started; wait\"
                           (hash 'background #t)))
         (hash-ref job 'pid)",
    )
    .await;
    read(&started).await;
    let child = std::fs::read_to_string(dir.path().join("child.pid")).unwrap();
    let killed = value(
        kernel.as_ref(),
        "(define signalled (bash-kill job))
         (define (ended) (if (bash-running? job) (ended) (bash-poll job)))
         (list signalled (hash-ref (ended) 'exit-code) (bash-kill job))",
    )
    .await;
    assert_eq!(
        (
            killed.as_str(),
            exited(shell.parse().unwrap()).await,
            exited(child.trim().parse().unwrap()).await,
        ),
        ("(#true 143 #false)", true, true)
    );
}

/// A command that outlives its wait window keeps running as a job; its end
/// is announced once, and reading its result withdraws the announcement. A
/// job its own cell waited to its end is never announced.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_outlives_its_window_runs_on_and_is_announced_when_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let gate = pipe_in(dir.path(), "gate");
    let (handlers, mut received) =
        recording(&[("bash.completed", json!({})), ("bash.consumed", json!({}))]);
    let (_provisioner, kernel) = kernel_in(handlers, dir.path()).await;
    let command = "echo begun; read -r line < gate; echo \\\"got $line\\\"; exit 4";
    let yielded = value(
        kernel.as_ref(),
        &format!(
            "(define quick (bash \"echo quick\" (hash 'background #t)))
             (define waited (bash-wait quick))
             (define slow (bash \"{command}\" (hash 'wait-ms 100)))
             (list (hash-ref waited 'output) (hash-ref slow 'running) (hash-ref slow 'exit-code)
                   (string? (hash-ref slow 'job)) (hash-ref slow 'pid))"
        ),
    )
    .await;
    let pid = numbers(&yielded).pop().unwrap();
    write(&gate, "go\n").await;
    let announced = next(&mut received).await;
    let read_later = value(
        kernel.as_ref(),
        "(let ([result (bash-wait slow)])
           (list (hash-ref result 'running) (hash-ref result 'exit-code) (hash-ref result 'output)))",
    )
    .await;
    let withdrawn = next(&mut received).await;
    let command = command.replace('\\', "");
    assert_eq!(
        (
            yielded,
            announced,
            read_later.as_str(),
            withdrawn,
            received.try_recv().ok(),
        ),
        (
            format!("(\"quick\\n\" #true #false #true {pid})"),
            (
                "bash.completed".to_string(),
                json!({
                    "pid": pid,
                    "command": command,
                    "exitCode": 4,
                    "output": "begun\ngot go",
                    "outputTruncated": false,
                })
            ),
            "(#false 4 \"begun\\ngot go\\n\")",
            (
                "bash.consumed".to_string(),
                json!({ "pid": pid, "command": command })
            ),
            None,
        )
    );
}

/// The call returns with the command's own end, not with the end of what
/// the command left running, which stays the job's to kill.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_returns_at_its_own_end_while_what_it_started_runs_on() {
    let dir = tempfile::tempdir().unwrap();
    let (_provisioner, kernel) = kernel_in(HostRequestHandlers::new(), dir.path()).await;
    let called = Instant::now();
    let returned = value(
        kernel.as_ref(),
        "(define result (bash \"sleep 300 & echo $! > left.pid; echo x\"))
         (list (hash-ref result 'output) (hash-ref result 'exit-code) (bash-running? result))",
    )
    .await;
    let returned_within = called.elapsed();
    let left = std::fs::read_to_string(dir.path().join("left.pid")).unwrap();
    let killed = value(kernel.as_ref(), "(bash-kill result (hash 'signal 'kill))").await;
    assert_eq!(
        (
            returned.as_str(),
            returned_within < STEP,
            killed.as_str(),
            exited(left.trim().parse().unwrap()).await,
        ),
        ("(\"x\\n\" 0 #true)", true, "#true", true)
    );
}

/// Killing a kernel, and dropping one, each end every job it has: a
/// background job with its children and a command that yielded.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_or_dropped_kernel_leaves_no_job_running() {
    let cell =
        "(define held (bash \"sleep 300 & echo $! > child.pid; echo up > held.started; wait\"
                             (hash 'background #t)))
                (define yielded (bash \"echo up > yielded.started; sleep 300\" (hash 'wait-ms 0)))
                (list (hash-ref held 'pid) (hash-ref yielded 'pid))";
    let mut gone = Vec::new();
    for kill in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let pipes = [
            pipe_in(dir.path(), "held.started"),
            pipe_in(dir.path(), "yielded.started"),
        ];
        let (provisioner, kernel) = kernel_in(HostRequestHandlers::new(), dir.path()).await;
        let mut pids = numbers(&value(kernel.as_ref(), cell).await);
        for pipe in &pipes {
            read(pipe).await;
        }
        pids.extend(numbers(
            &std::fs::read_to_string(dir.path().join("child.pid")).unwrap(),
        ));
        if kill {
            provisioner.kill().await;
        } else {
            drop(kernel);
            drop(provisioner);
        }
        for pid in &pids {
            gone.push(exited(*pid).await);
        }
        gone.push(pids.len() == 3);
    }
    assert_eq!(gone, [true; 8]);
}

/// A read-only command whose inputs are unchanged answers from the cache;
/// a changed input, or a command that is more than one read, runs again.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_read_only_command_is_answered_from_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "one\n").unwrap();
    let (handlers, mut received) = recording(&[("command_cache.fact", json!({}))]);
    let (_provisioner, kernel) = kernel_in(handlers, dir.path()).await;
    let first = value(
        kernel.as_ref(),
        "(define (seen command)
           (let ([result (bash command)]) (list (hash-ref result 'output) (hash-ref result 'cached))))
         (list (seen \"cat note.txt\") (seen \"cat note.txt\")
               (seen \"cat note.txt | cat\") (seen \"cat note.txt | cat\"))",
    )
    .await;
    std::fs::write(dir.path().join("note.txt"), "three\n").unwrap();
    let changed = value(kernel.as_ref(), "(seen \"cat note.txt\")").await;
    let mut hits = Vec::new();
    for _ in 0..3 {
        hits.push(next(&mut received).await.1["hit"] == json!(true));
    }
    hits.sort_unstable();
    assert_eq!(
        (
            first.as_str(),
            changed.as_str(),
            hits,
            received.try_recv().ok()
        ),
        (
            "((\"one\\n\" #false) (\"one\\n\" #true) (\"one\\n\" #false) (\"one\\n\" #false))",
            "(\"three\\n\" #false)",
            vec![false, false, true],
            None,
        )
    );
}

macro_rules! captured_run {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("fixtures/test_runs/", $name, ".txt")),
        )
    };
}

/// Each parser against the real run it was built from, rendered as the
/// model reads it, and the commands that count as test runs.
#[test]
fn every_captured_test_run_summarizes_to_its_counts_and_failures() {
    let rendered: Vec<(&str, Option<String>)> = [
        captured_run!("cargo_test"),
        captured_run!("cargo_test_ok"),
        captured_run!("go_test"),
        captured_run!("go_test_v"),
        captured_run!("jest"),
        captured_run!("pytest"),
        captured_run!("pytest_ok"),
        captured_run!("unittest"),
        captured_run!("vitest"),
        ("unrecognized", "compiling...\nerror: linker failed\n"),
    ]
    .into_iter()
    .map(|(name, output)| {
        let text = summary::summarize(output)
            .and_then(|parsed| parsed.value(None, &[])["text"].as_str().map(str::to_string));
        (name, text)
    })
    .collect();
    let commands = [
        "cargo test -p pa-core",
        "cd crates/x && cargo +1.98.1 test --lib",
        "uv run pytest -q tests/",
        "python3 -m unittest discover -s test",
        "go test ./...",
        "npx vitest run",
        "npm test",
        "cargo build",
        "cat pytest.ini",
        "git log --grep=test",
    ]
    .map(summary::looks_like_test_run);
    let expected = [
        ("cargo_test", Some("[tests: cargo · 1 passed · 2 failed · 1 skipped]\nFAILED tests::panics_with_message · src/lib.rs:26:26\n    index three is present\nFAILED tests::adds_two_numbers · src/lib.rs:11:9\n    assertion `left == right` failed\n      left: 5\n     right: 4")),
        ("cargo_test_ok", Some("[tests: cargo · 3 passed · 0 failed · 1 skipped]")),
        ("go_test", Some("[tests: go · 1 failed]\nFAILED TestAdd · calc_test.go:7\n    calc_test.go:7: Add(2, 2) = 5, want 4")),
        ("go_test_v", Some("[tests: go · 1 passed · 1 failed · 1 skipped]\nFAILED TestAdd · calc_test.go:7\n    calc_test.go:7: Add(2, 2) = 5, want 4")),
        ("jest", Some("[tests: jest · 1 passed · 2 failed · 1 skipped]\nFAILED add › adds two numbers · math.test.js:5:23\n    expect(received).toBe(expected) // Object.is equality\n    \n    Expected: 4\n    Received: 5\nFAILED add › adds objects · math.test.js:9:36\n    expect(received).toEqual(expected) // deep equality\n    \n    - Expected  - 1\n    + Received  + 1\n    \n      Object {\n        \"a\": 1,\n    -   \"b\": 0,\n    +   \"b\": 1,\n      }")),
        ("pytest", Some("[tests: pytest · 1 passed · 3 failed · 1 skipped]\nFAILED test_divide_wrong · test_calc.py:11\n    assert 3.5 == 3\n     +  where 3.5 = divide(7, 2)\nFAILED test_parse · calc.py:6\n    ValueError: invalid literal for int() with base 10: 'x1'\nFAILED test_dict · test_calc.py:19\n    AssertionError: assert {'a': 1, 'b': 2} == {'a': 1, 'b': 3}\n    \n      Omitting 1 identical items, use -vv to show\n      Differing items:\n      {'b': 2} != {'b': 3}\n      Use -v to get more diff")),
        ("pytest_ok", Some("[tests: pytest · 1 passed · 0 failed · 0 skipped]")),
        ("unittest", Some("[tests: unittest · 1 passed · 2 failed · 1 skipped]\nFAILED test_unit.DivideTest.test_zero · /work/app/calc.py:2\n    ZeroDivisionError: division by zero\nFAILED test_unit.DivideTest.test_bad · /work/app/test_unit.py:11\n    AssertionError: 2.5 != 2")),
        ("vitest", Some("[tests: vitest · 1 passed · 1 failed · 1 skipped]\nFAILED add > adds two numbers · math.spec.mjs:8:23\n    AssertionError: expected 5 to be 4 // Object.is equality\n    \n    - Expected\n    + Received\n    \n    - 4\n    + 5")),
        ("unrecognized", None),
    ]
    .map(|(name, text)| (name, text.map(str::to_string)));
    assert_eq!(
        (rendered, commands),
        (
            expected.to_vec(),
            [true, true, true, true, true, true, true, false, false, false]
        )
    );
}

/// A waited test run returns its summary with the handles the host gave its
/// outputs, and prints it with the cell's output.
#[tokio::test(flavor = "multi_thread")]
async fn a_waited_test_run_returns_and_prints_its_summary_with_handles() {
    let dir = tempfile::tempdir().unwrap();
    let (handlers, mut received) = recording(&[(
        "tests.summary",
        json!({ "output": "out:00000000ff", "failures": ["out:00000000a1", "out:00000000a2"] }),
    )]);
    let (_provisioner, kernel) = kernel_in(handlers, dir.path()).await;
    let captured = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/kernel/scheme/native/fixtures/test_runs/cargo_test.txt"
    );
    let ran = run(
        kernel.as_ref(),
        &format!(
            "(define result (bash \"cat '{captured}'; : cargo test; exit 101\"))
             (define tests (hash-ref result 'tests))
             (list (hash-ref result 'exit-code) (hash-ref tests 'runner) (hash-ref tests 'failed)
                   (hash-ref tests 'output-handle)
                   (map (lambda (failure) (hash-ref failure 'output-handle)) (hash-ref tests 'failures))
                   (hash-contains? result 'announce))"
        ),
    )
    .await;
    let (_, request) = next(&mut received).await;
    assert_eq!(
        (
            ran.result.as_deref(),
            ran.stdout.as_str(),
            &request["runner"],
            &request["failed"],
            request["failures"].as_array().map(Vec::len),
        ),
        (
            Some("(101 \"cargo\" 2 \"out:00000000ff\" (\"out:00000000a1\" \"out:00000000a2\") #false)"),
            "[tests: cargo · 1 passed · 2 failed · 1 skipped · output out:00000000ff]\n\
             FAILED tests::panics_with_message · src/lib.rs:26:26 · out:00000000a1\n    \
             index three is present\n\
             FAILED tests::adds_two_numbers · src/lib.rs:11:9 · out:00000000a2\n    \
             assertion `left == right` failed\n      left: 5\n     right: 4\n",
            &json!("cargo"),
            &json!(2),
            Some(2),
        )
    );
}

/// A large output is kept whole in the session's spill store under the
/// handle its result names; a small one has no handle.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_output_is_spilled_under_the_handle_its_result_names() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = dir.path().join("session-a");
    let (_provisioner, kernel) = kernel_with(SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        artifact_dir: Some(artifacts.clone()),
        ..SchemeKernelOptions::default()
    })
    .await;
    let handles = value(
        kernel.as_ref(),
        "(define large (bash \"seq 1 3000\"))
         (list (hash-ref (bash \"echo small\") 'handle) (hash-ref large 'truncated)
               (hash-ref large 'handle))",
    )
    .await;
    let id = handles
        .rsplit("out:")
        .next()
        .unwrap()
        .trim_end_matches(['"', ')']);
    let whole = (1..=3000).fold(String::new(), |whole, line| format!("{whole}{line}\n"));
    let store = SpillStore::for_session_dir(&artifacts);
    assert_eq!(
        (
            handles.as_str(),
            tokio::time::timeout(STEP, store.lookup(id)).await.unwrap(),
        ),
        (
            format!("(#false #false \"out:{id}\")").as_str(),
            SpillLookup::Stored(StoredSpill {
                total_chars: whole.len(),
                lines: 3000,
                text: whole,
            }),
        )
    );
}

/// Over the session's job limit a background command is held until a slot
/// frees, unless the cell that started it waits for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_over_the_job_limit_is_queued_until_a_slot_frees() {
    let dir = tempfile::tempdir().unwrap();
    let (started, gate) = (pipe_in(dir.path(), "started"), pipe_in(dir.path(), "gate"));
    let (_provisioner, kernel) = kernel_with(SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        env: vec![("PRIME_AGENT_BASH_MAX_JOBS".to_string(), "1".to_string())],
        ..SchemeKernelOptions::default()
    })
    .await;
    let queued = value(
        kernel.as_ref(),
        "(define first (bash \"echo up > started; read -r line < gate\" (hash 'background #t)))
         (define second (bash \"echo second\" (hash 'background #t)))
         (define third (bash \"echo third\" (hash 'background #t)))
         (list (hash-ref first 'queued) (hash-ref second 'queued) (hash-ref third 'queued)
               (hash-ref (bash-wait third) 'output)
               (hash-ref (cadr (bash-jobs)) 'status))",
    )
    .await;
    read(&started).await;
    write(&gate, "go\n").await;
    let freed = value(kernel.as_ref(), "(hash-ref (bash-wait second) 'output)").await;
    assert_eq!(
        (queued.as_str(), freed.as_str()),
        (
            "(#false #true #true \"third\\n\" \"queued\")",
            "\"second\\n\""
        )
    );
}

/// What a cell's commands leave on its result: the commands it started, how
/// its waits ended, and the source lines a failed command's output names.
#[tokio::test(flavor = "multi_thread")]
async fn a_cells_commands_waits_and_failures_show_on_its_result() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "fn one() {\n    1;\n}\n\npub fn label(n: u32) -> String {\n    let count: u32 = \"three\";\n    format!(\"{n} {count}\")\n}\n",
    )
    .unwrap();
    let build = include_str!("../../error_context/fixtures/cargo_build.txt");
    std::fs::write(dir.path().join("build.log"), build).unwrap();
    let (handlers, _received) = recording(&[("bash.completed", json!({}))]);
    let (_provisioner, kernel) = kernel_with(SchemeKernelOptions {
        host_handlers: handlers,
        cwd: dir.path().to_path_buf(),
        error_context: ErrorContext::for_workspace(dir.path().to_path_buf(), None),
        ..SchemeKernelOptions::default()
    })
    .await;
    let shown = run(
        kernel.as_ref(),
        "(bash \"echo one\\necho two\")
         (bash \"cat build.log; exit 101\")
         (hash-ref (bash \"sleep 300\" (hash 'wait-ms 0)) 'running)",
    )
    .await;
    let next_cell = run(kernel.as_ref(), "(+ 1 2)").await;
    assert_eq!(
        (
            shown.result.as_deref(),
            shown.bash_commands,
            shown.bash_waits,
            shown.error_context.as_deref(),
            (
                next_cell.bash_commands,
                next_cell.bash_waits,
                next_cell.error_context
            ),
        ),
        (
            Some("#true"),
            Some(KernelBashCommands {
                first: "echo one\necho two".to_string(),
                count: 3,
                lines: 4,
            }),
            KernelBashWaits {
                completed: 2,
                yielded: 1,
            },
            Some(
                "[error context: 1 location · source ±3 lines]\n\
                 src/lib.rs:6:22 (rustc) error[E0308]: mismatched types\n  \
                 3 | }\n  \
                 4 | \n  \
                 5 | pub fn label(n: u32) -> String {\n\
                 > 6 |     let count: u32 = \"three\";\n  \
                 7 |     format!(\"{n} {count}\")\n  \
                 8 | }"
            ),
            (None, KernelBashWaits::default(), None),
        )
    );
}

/// A persistent shell is recorded for its session, outlives the kernel that
/// started it, and is re-attached by the session's next kernel, which reads
/// its result and removes its files.
#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_shell_outlives_its_kernel_and_is_reattached_by_the_next() {
    let (dir, agent) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let gate = pipe_in(dir.path(), "gate");
    let of_session = || SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        agent_dir: Some(agent.path().to_path_buf()),
        artifact_dir: Some(dir.path().join("session-a")),
        ..SchemeKernelOptions::default()
    };
    // The first kernel's runtime ends with it, as a worker's does: nothing of
    // it watches the shell any more.
    let first_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let options = of_session();
    let started = first_runtime
        .spawn(async move {
            let (_provisioner, kernel) = kernel_with(options).await;
            value(
                kernel.as_ref(),
                "(define shell (bash \"echo lasting; read -r line < gate; echo \\\"got $line\\\"\"
                                     (hash 'persistent #t 'background #t)))
                 (list (hash-ref shell 'persistent) (hash-ref shell 'pid))",
            )
            .await
        })
        .await
        .unwrap();
    first_runtime.shutdown_background();
    let pid = numbers(&started).pop().unwrap();
    let home = agent.path().join("persistent-shells");
    let record: Value = std::fs::read_dir(&home)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .unwrap();
    let (_provisioner, second) = kernel_with(of_session()).await;
    let attached = value(
        second.as_ref(),
        "(define shells (bash-persistent-shells))
         (map (lambda (shell) (list (hash-ref shell 'pid) (hash-ref shell 'running))) shells)",
    )
    .await;
    write(&gate, "go\n").await;
    let ended = value(
        second.as_ref(),
        "(let ([result (bash-wait (car shells))])
           (list (hash-ref result 'exit-code) (hash-ref result 'output)))",
    )
    .await;
    assert_eq!(
        (
            started,
            (&record["pid"], &record["sessionId"], &record["command"]),
            attached,
            ended.as_str(),
            std::fs::read_dir(&home).unwrap().count(),
        ),
        (
            format!("(#true {pid})"),
            (
                &json!(pid),
                &json!("session-a"),
                &json!("echo lasting; read -r line < gate; echo \"got $line\"")
            ),
            format!("(({pid} #true))"),
            "(0 \"lasting\\ngot go\\n\")",
            0,
        )
    );
}

/// A cell that moves to the background while it waits for a command keeps
/// that command: it runs on, and its result reaches the cell.
#[tokio::test(flavor = "multi_thread")]
async fn a_backgrounded_cell_keeps_the_command_it_waits_for() {
    let dir = tempfile::tempdir().unwrap();
    let (started, gate) = (pipe_in(dir.path(), "started"), pipe_in(dir.path(), "gate"));
    let (_provisioner, kernel) = windowed_kernel_in(dir.path()).await;
    let moved = run_within(
        kernel.as_ref(),
        "(hash-ref (bash \"echo up > started; read -r line < gate; echo \\\"got $line\\\"\") 'output)",
        None,
    )
    .await;
    let cell = moved.backgrounded_cell.unwrap();
    read(&started).await;
    write(&gate, "go\n").await;
    run_within(kernel.as_ref(), &format!("(cell-wait \"{cell}\" 60)"), None).await;
    let ended = run_within(
        kernel.as_ref(),
        &format!(
            "(let ([state (cell-status \"{cell}\")])
               (list (hash-ref state 'status) (hash-ref state 'text)))"
        ),
        None,
    )
    .await;
    assert_eq!(
        ended.result.as_deref(),
        Some("(\"ok\" \"\\\"got go\\\\n\\\"\")")
    );
}

/// Cancelling a background cell kills the command it waits for, with what
/// that command started, and leaves the jobs it started in the background.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_background_cell_kills_its_command_and_keeps_its_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let started = pipe_in(dir.path(), "started");
    let (_provisioner, kernel) = windowed_kernel_in(dir.path()).await;
    let moved = run_within(
        kernel.as_ref(),
        "(define kept (bash \"sleep 300\" (hash 'background #t)))
         (bash \"sleep 300 & echo $$ $! > waited.pids; echo up > started; wait\")",
        None,
    )
    .await;
    let cell = moved.backgrounded_cell.unwrap();
    read(&started).await;
    let waited = numbers(&std::fs::read_to_string(dir.path().join("waited.pids")).unwrap());
    run_within(kernel.as_ref(), &format!("(cell-cancel \"{cell}\")"), None).await;
    let mut gone = Vec::new();
    for pid in &waited {
        gone.push(exited(*pid).await);
    }
    let after = run_within(
        kernel.as_ref(),
        &format!(
            "(list (hash-ref (cell-status \"{cell}\") 'status) (bash-running? kept)
                   (bash-kill kept (hash 'signal 'kill)))"
        ),
        None,
    )
    .await;
    assert_eq!(
        (gone, after.result.as_deref()),
        (vec![true, true], Some("(\"interrupted\" #true #true)"))
    );
}

/// Steel's prelude binds hundreds of names; a definition here that reused
/// one would replace a builtin for every cell.
#[test]
fn no_bash_name_replaces_a_steel_builtin() {
    let mut engine = Engine::new();
    let taken: Vec<&str> = SCHEME
        .lines()
        .filter_map(|line| line.strip_prefix("(define "))
        .filter_map(|rest| rest.trim_start_matches('(').split([' ', ')']).next())
        .filter(|name| engine.run((*name).to_string()).is_ok())
        .collect();
    assert_eq!(taken, Vec::<&str>::new());
}
