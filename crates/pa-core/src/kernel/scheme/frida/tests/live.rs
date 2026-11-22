//! The `frida-*` functions against frida-core itself, on processes these
//! tests start. Built only with the `frida` feature (`make frida-test`).
//!
//! Frida attaches through ptrace. Where the machine forbids that, each test
//! says so and returns: it cannot prove anything there.

use super::lifecycle::{ended_after, ENDINGS};
use super::*;

/// Why this machine cannot trace a process it started, if it cannot.
fn ptrace_refusal() -> Option<String> {
    let scope = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").ok()?;
    match (scope.trim(), nix::unistd::geteuid().is_root()) {
        ("3", _) => Some("kernel.yama.ptrace_scope is 3: no process may be traced".to_string()),
        ("2", false) => {
            Some("kernel.yama.ptrace_scope is 2: only an administrator may trace".to_string())
        }
        (_, _) => None,
    }
}

/// The process `pid` names is gone within the wait.
async fn exited(pid: &Value) -> bool {
    let pid = u32::try_from(pid.as_u64().unwrap()).unwrap();
    tokio::time::timeout(PATIENCE, crate::platform::process::wait_for_exit(pid))
        .await
        .is_ok_and(|waited| waited.is_ok())
}

/// A spawned process is attached, its script's `send()`, binary data and
/// `console.log` land in the log, an export answers, a posted message
/// arrives, a trace reports calls, the compiler bundles TypeScript, Frida's
/// own errors come through unchanged, and detaching ends the process.
#[tokio::test(flavor = "multi_thread")]
async fn frida_core_attaches_to_a_spawned_process_and_talks_to_its_script() {
    if let Some(refusal) = ptrace_refusal() {
        eprintln!("skipped: {refusal}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agent.ts"),
        "const answer: number = 41;\nsend(answer + 1);\n",
    )
    .unwrap();
    let project = dir.path().display();
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            "(hash-ref (car (frida-devices)) 'id)",
            r#"(begin (define s (frida-attach "/bin/sh" #:spawn #t
                                 #:args (list "-c" "while :; do sleep 0.05; done")))
                      (hash-ref s 'spawned))"#,
            r#"(begin (define sc (frida-script-load s
                 "send({hello: 1}, new Uint8Array([1, 2, 3]).buffer);
                  console.log('hi');
                  rpc.exports = { addNumbers(a, b) { return a + b; }, fail() { throw new Error('boom'); } };
                  recv('poke', m => send({got: m.x}));"))
                      (frida-script-wait sc))"#,
            "(frida-script-wait sc)",
            // Frida's loop and helper process are running in this process
            // now: its own child processes still start and are reaped.
            r#"(hash-ref (bash "echo beside-frida") 'output)"#,
            r#"(frida-script-call sc "add-numbers" (list 2 3))"#,
            r#"(frida-script-call sc "fail")"#,
            r#"(begin (frida-script-post sc (hash 'type "poke" 'x 7))
                      (hash-ref (frida-script-wait sc) 'payload))"#,
            r#"(frida-script-load s "not javascript")"#,
            r#"(begin (define tr (frida-trace s #:include (list "wait*" "*fork*")))
                      (string? (hash-ref (hash-ref (frida-script-wait tr
                         #:predicate (lambda (m) (hash-contains? (hash-ref m 'payload) 'function)))
                        'payload) 'function)))"#,
            &format!(
                r#"(string-contains? (frida-compile "{project}/agent.ts" #:project-root "{project}") "send(answer + 1)")"#
            ),
            "(frida-attach 999999999)",
            "(frida-detach s)",
        ],
    )
    .await;
    let pid = values(kernel.as_ref(), &["(hash-ref s 'pid)"])
        .await
        .remove(0)
        .unwrap();
    assert_eq!(
        (returned, exited(&pid).await),
        (
            vec![
                Ok(json!("local")),
                Ok(json!(true)),
                Ok(json!({
                    "seq": 0,
                    "type": "send",
                    "payload": {"hello": 1},
                    "data-base64": "AQID",
                    "data-size": 3,
                })),
                Ok(json!({"seq": 1, "type": "log", "payload": "hi", "level": "info"})),
                Ok(json!("beside-frida\n")),
                Ok(json!(5)),
                Err("Error: Generic: frida-script-call: boom".to_string()),
                Ok(json!({"got": 7})),
                Err(
                    "Error: Generic: frida-script-load: Script(line 1): SyntaxError: expecting ';'"
                        .to_string()
                ),
                Ok(json!(true)),
                Ok(json!(true)),
                Err(
                    "Error: Generic: frida-attach: Unable to find process with pid 999999999"
                        .to_string()
                ),
                Ok(json!(true)),
            ],
            true,
        )
    );
}

/// A script whose top level never returns cannot hold a cell: the stopped
/// cell leaves the load, and the target still ends with its session.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_cell_leaves_a_load_that_never_returns() {
    if let Some(refusal) = ptrace_refusal() {
        eprintln!("skipped: {refusal}");
        return;
    }
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions::default())
        .ensure(None, None)
        .await
        .unwrap();
    let pid = values(
        kernel.as_ref(),
        &[r#"(begin (define s (frida-attach "/bin/sleep" #:spawn #t #:args (list "300"))) (hash-ref s 'pid))"#],
    )
    .await
    .remove(0)
    .unwrap();
    let signal = AbortSignal::new();
    let loading = Arc::new(tokio::sync::Notify::new());
    let printed = Arc::clone(&loading);
    let on_stream = move |_text: &str, _stream: &'static str| printed.notify_one();
    let stopping = signal.clone();
    let stop = async {
        loading.notified().await;
        stopping.cancel();
    };
    let load = kernel.execute(
        r#"(displayln "loading") (frida-script-load s "while (true) {}")"#,
        KernelExecuteOptions {
            signal: Some(signal),
            on_stream: Some(&on_stream),
            ..NO_OPTIONS
        },
    );
    let stopped = tokio::time::timeout(PATIENCE, async { tokio::join!(load, stop).0 })
        .await
        .expect("the stopped cell ends")
        .unwrap()
        .status;
    let detached = values(kernel.as_ref(), &["(frida-detach s)"]).await;
    assert_eq!(
        (stopped, detached, exited(&pid).await),
        (ExecuteStatus::Aborted, vec![Ok(json!(true))], true)
    );
}

/// However the kernel ends, a target it spawned and never resumed is not
/// left behind suspended.
#[tokio::test(flavor = "multi_thread")]
async fn no_spawned_target_outlives_the_kernel_that_spawned_it() {
    if let Some(refusal) = ptrace_refusal() {
        eprintln!("skipped: {refusal}");
        return;
    }
    let mut gone = Vec::new();
    for ending in ENDINGS {
        let pid = ended_after(
            ending,
            SchemeKernelProvisioner::with_options(SchemeKernelOptions::default()),
            r#"(hash-ref (frida-attach "/bin/sleep" #:spawn #t #:args (list "300") #:pause #t) 'pid)"#,
        )
        .await
        .unwrap();
        gone.push(exited(&pid).await);
    }
    assert_eq!(gone, vec![true; 3]);
}
