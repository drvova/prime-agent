//! The native tools through a real session kernel: real commands, real
//! processes, and real files under temporary directories.

use std::path::Path;

use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::HostRequestHandlers;
use crate::tools::kernel_cell::{
    ExecuteResult, ExecuteStatus, KernelExecuteOptions, KernelExecutor, KernelProvisioner,
};
use crate::tools::tool_definition::AbortSignal;

async fn kernel_in(cwd: &Path) -> Box<dyn KernelExecutor> {
    SchemeKernelProvisioner::new(HostRequestHandlers::new(), cwd)
        .ensure(None, None)
        .await
        .unwrap()
}

async fn run(
    kernel: &dyn KernelExecutor,
    code: &str,
    signal: Option<AbortSignal>,
    timeout_ms: Option<u64>,
) -> ExecuteResult {
    kernel
        .execute(
            code,
            KernelExecuteOptions {
                signal,
                timeout_ms,
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        )
        .await
        .unwrap()
}

/// A cell's status with its value or, for a failed cell, its error message.
fn settled(result: ExecuteResult) -> (ExecuteStatus, Option<String>) {
    (
        result.status,
        result.result.or(result.error.map(|error| error.evalue)),
    )
}

/// The process `pid` names is gone within the wait.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn exited(pid: &str) -> bool {
    use std::time::Duration;
    let pid = pid.trim().parse().unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        crate::platform::process::wait_for_exit(pid),
    )
    .await
    .is_ok_and(|waited| waited.is_ok())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_command_runs_in_the_session_cwd_and_a_failing_one_is_a_result() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let kernel = kernel_in(&cwd).await;
    let shown = "(let ([r (bash command)])
                   (list (hash-ref r 'exit-code) (hash-ref r 'output) (hash-ref r 'truncated)
                         (hash-ref r 'timed-out)))";
    let passed = run(
        kernel.as_ref(),
        &format!("(define command \"pwd; echo $TERM\") {shown}"),
        None,
        None,
    )
    .await;
    let failed = run(
        kernel.as_ref(),
        &format!("(define command \"echo partial; echo broken >&2; exit 3\") {shown}"),
        None,
        None,
    )
    .await;
    let here = format!("{}\\ndumb\\n", cwd.display());
    assert_eq!(
        (settled(passed), settled(failed)),
        (
            (
                ExecuteStatus::Ok,
                Some(format!("(0 \"{here}\" #false #false)"))
            ),
            (
                ExecuteStatus::Ok,
                Some("(3 \"partial\\nbroken\\n\" #false #false)".to_string())
            )
        )
    );
}

/// A sleeping command ends with its own timeout, or with its cell's, well
/// before it would on its own, and nothing it started survives. What it
/// starts is the program `/bin/sleep`: a bare `sleep` is the built-in
/// shell's own, with no process for `$!` to name.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_timeout_kills_the_command_and_everything_it_started() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_in(dir.path()).await;
    let started = Instant::now();
    let own = run(
        kernel.as_ref(),
        "(let ([r (bash \"/bin/sleep 30 & echo $! > own.pid; wait\" (hash 'timeout-ms 300))])
           (list (hash-ref r 'timed-out) (hash-ref r 'exit-code)))",
        None,
        None,
    )
    .await;
    let own_elapsed = started.elapsed();
    let started = Instant::now();
    let cell = run(
        kernel.as_ref(),
        "(bash \"/bin/sleep 30 & echo $! > cell.pid; wait\")",
        None,
        Some(300),
    )
    .await;
    let cell_elapsed = started.elapsed();
    let after = run(
        kernel.as_ref(),
        "(hash-ref (bash \"echo alive\") 'output)",
        None,
        None,
    )
    .await;
    let own_pid = std::fs::read_to_string(dir.path().join("own.pid")).unwrap();
    let cell_pid = std::fs::read_to_string(dir.path().join("cell.pid")).unwrap();
    let prompt = Duration::from_millis(300)..Duration::from_secs(10);
    assert_eq!(
        (
            settled(own),
            (cell.status, cell.error.map(|error| error.ename)),
            settled(after),
            prompt.contains(&own_elapsed),
            prompt.contains(&cell_elapsed),
            exited(&own_pid).await,
            exited(&cell_pid).await,
        ),
        (
            (ExecuteStatus::Ok, Some("(#true 143)".to_string())),
            (ExecuteStatus::Error, Some("Timeout".to_string())),
            (ExecuteStatus::Ok, Some("\"alive\\n\"".to_string())),
            true,
            true,
            true,
            true,
        )
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn an_abort_kills_a_running_command_and_the_kernel_stays_usable() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_in(dir.path()).await;
    // The command reports the process it started through a pipe: reading it
    // returns once the command is really running.
    let started_pipe = dir.path().join("started");
    nix::unistd::mkfifo(&started_pipe, nix::sys::stat::Mode::S_IRWXU).unwrap();
    let signal = AbortSignal::new();
    let running = run(
        kernel.as_ref(),
        "(bash \"/bin/sleep 30 & echo $! > started; wait\" (hash 'timeout-ms #f))",
        Some(signal.clone()),
        None,
    );
    let abort = async {
        let pid = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::fs::read_to_string(&started_pipe),
        )
        .await
        .unwrap()
        .unwrap();
        let aborted_at = Instant::now();
        signal.cancel();
        (pid, aborted_at)
    };
    let (aborted, (pid, aborted_at)) = tokio::join!(running, abort);
    let stopped_within = aborted_at.elapsed();
    let after = run(
        kernel.as_ref(),
        "(hash-ref (bash \"echo alive\") 'output)",
        None,
        None,
    )
    .await;
    assert_eq!(
        (
            aborted.status,
            stopped_within < Duration::from_secs(10),
            exited(&pid).await,
            settled(after),
        ),
        (
            ExecuteStatus::Aborted,
            true,
            true,
            (ExecuteStatus::Ok, Some("\"alive\\n\"".to_string())),
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn read_grep_glob_and_find_see_the_session_tree() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/alpha.rs"),
        "fn alpha() {}\nlet needle = 1;\nfn omega() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/beta.txt"), "needle in text\n").unwrap();
    std::fs::write(dir.path().join("notes.md"), "no match here\n").unwrap();
    let kernel = kernel_in(dir.path()).await;
    let seen = run(
        kernel.as_ref(),
        "(define view (fs-read \"src/alpha.rs\" (hash 'start 2 'end 3)))
         (list
           (list (hash-ref view 'path) (hash-ref view 'total-lines)
                 (map (lambda (row) (list (hash-ref row 'line) (hash-ref row 'text)))
                      (hash-ref view 'lines)))
           (map (lambda (found) (list (hash-ref found 'path) (hash-ref found 'line) (hash-ref found 'text)))
                (hash-ref (fs-grep \"needle\" (hash 'glob \"*.rs\")) 'matches))
           (map (lambda (entry) (hash-ref entry 'path)) (hash-ref (fs-glob \"**/*.txt\") 'entries))
           (map (lambda (entry) (list (hash-ref entry 'path) (hash-ref entry 'kind)))
                (hash-ref (fs-find (hash 'name \"*.md\" 'kind 'file)) 'entries)))",
        None,
        None,
    )
    .await;
    assert_eq!(
        settled(seen),
        (
            ExecuteStatus::Ok,
            Some(
                "((\"src/alpha.rs\" 3 ((2 \"let needle = 1;\") (3 \"fn omega() {}\"))) \
                 ((\"src/alpha.rs\" 2 \"let needle = 1;\")) \
                 (\"src/beta.txt\") \
                 ((\"notes.md\" \"file\")))"
                    .to_string()
            )
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_rewrites_its_file_and_a_stale_anchor_raises_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("list.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
    let kernel = kernel_in(dir.path()).await;
    let applied = run(
        kernel.as_ref(),
        "(define second (hash-ref (list-ref (hash-ref (fs-read \"list.txt\") 'lines) 1) 'anchor))
         (define done (fs-edit \"list.txt\" (hash 'replace second 'text (list \"deux\" \"zwei\"))))
         (list (hash-ref done 'applied) (hash-ref done 'removed) (hash-ref done 'added))",
        None,
        None,
    )
    .await;
    let after_edit = std::fs::read_to_string(&file).unwrap();
    let stale = run(
        kernel.as_ref(),
        "(fs-edit \"list.txt\" (hash 'delete second))",
        None,
        None,
    )
    .await;
    assert_eq!(
        (
            settled(applied),
            after_edit,
            stale.status,
            stale.error.is_some_and(
                |error| error.evalue.contains("fs-edit: ") && error.evalue.contains("stale")
            ),
            std::fs::read_to_string(&file).unwrap(),
        ),
        (
            (ExecuteStatus::Ok, Some("(#true 1 2)".to_string())),
            "one\ndeux\nzwei\nthree\n".to_string(),
            ExecuteStatus::Error,
            true,
            "one\ndeux\nzwei\nthree\n".to_string(),
        )
    );
}
