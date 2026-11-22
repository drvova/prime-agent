//! What bounds and ends the Frida state of a session: the log's limits, a
//! stopped cell, a detach from the target's side, and every way a kernel
//! ends.

use super::*;

/// A log keeps the newest [`LOG_CAP`] messages and counts the rest, and
/// keeps the first [`DATA_CAP_BYTES`] of a message's data beside its size.
#[test]
fn a_log_keeps_the_newest_messages_and_the_head_of_their_data() {
    let log = ScriptLog::default();
    let data = vec![7_u8; DATA_CAP_BYTES + 904];
    for sent in 0..LOG_CAP + 5 {
        log.receive(
            &json!({"type": "send", "payload": sent}).to_string(),
            Some(&data),
        );
    }
    let (messages, dropped) = log.snapshot(None);
    let oldest = messages[0].json();
    assert_eq!(
        (
            messages.len(),
            dropped,
            &oldest["seq"],
            oldest["data-base64"].as_str().map(str::len),
            &oldest["data-size"],
            log.snapshot(Some(2))
                .0
                .iter()
                .map(crate::kernel::scheme::frida::log::Message::line)
                .collect::<Vec<_>>(),
        ),
        (
            LOG_CAP,
            5,
            &json!(5),
            Some(DATA_CAP_BYTES.div_ceil(3) * 4),
            &json!(5000),
            vec![
                "#1003 send: 1003 (+5000 bytes)".to_string(),
                "#1004 send: 1004 (+5000 bytes)".to_string(),
            ],
        )
    );
}

/// Run `cell` and stop it once `ready` says it is under way; its status.
async fn stopped_when(
    kernel: &dyn KernelExecutor,
    cell: &str,
    ready: impl std::future::Future<Output = ()>,
) -> ExecuteStatus {
    let signal = AbortSignal::new();
    let running = kernel.execute(
        cell,
        KernelExecuteOptions {
            signal: Some(signal.clone()),
            ..NO_OPTIONS
        },
    );
    let stop = async {
        ready.await;
        signal.cancel();
    };
    tokio::time::timeout(PATIENCE, async { tokio::join!(running, stop).0 })
        .await
        .expect("the stopped cell ends")
        .unwrap()
        .status
}

/// A cell stopped inside a Frida call that does not return ends, Frida is
/// told to cancel the call, and the script whose load was abandoned is not
/// kept. A cell stopped while it waits for a message or for an export's
/// reply ends too.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_cell_leaves_its_frida_call_and_its_waits() {
    let fake = Arc::new(Fake::default());
    let kernel = provisioner(SchemeKernelOptions::default(), &fake)
        .ensure(None, None)
        .await
        .unwrap();
    values(
        kernel.as_ref(),
        &[
            r#"(begin (define s (frida-attach 7)) (define sc (frida-script-load s "send(1)")) #t)"#,
            "(begin (frida-script-wait sc) (frida-script-wait sc) #t)",
        ],
    )
    .await;
    let in_frida = stopped_when(
        kernel.as_ref(),
        r#"(frida-script-load s "while (true) {}")"#,
        fake.blocked.notified(),
    )
    .await;
    // Nothing tells a test that a cell is inside a wait: stopped at once,
    // the cell is before its wait or in it, and ends either way.
    let in_wait = stopped_when(
        kernel.as_ref(),
        "(frida-script-wait sc #:timeout-ms 600000)",
        std::future::ready(()),
    )
    .await;
    let kept = values(kernel.as_ref(), &["(frida-sessions)"]).await;
    assert_eq!(
        (in_frida, in_wait, kept, fake.calls()[3..].to_vec()),
        (
            ExecuteStatus::Aborted,
            ExecuteStatus::Aborted,
            vec![Ok(json!([{
                "session": 1,
                "pid": RUNNING,
                "spawned": false,
                "detached-reason": false,
                "scripts": [2],
            }]))],
            [
                "create None Default while (true) {}",
                "load",
                "load cancelled"
            ]
            .map(str::to_string)
            .to_vec(),
        )
    );
}

/// Frida can finish an attach after the cell that asked for it was stopped.
/// The session it then hands over belongs to nobody: it is detached and its
/// spawned target killed, not left suspended.
#[tokio::test(flavor = "multi_thread")]
async fn an_attach_that_outlives_its_cell_is_undone() {
    let fake = Arc::new(Fake {
        attach_outlives_its_cell: true,
        ..Fake::default()
    });
    let kernel = provisioner(SchemeKernelOptions::default(), &fake)
        .ensure(None, None)
        .await
        .unwrap();
    let stopped = stopped_when(
        kernel.as_ref(),
        r#"(frida-attach "/bin/app" #:spawn #t)"#,
        fake.blocked.notified(),
    )
    .await;
    let kept = values(kernel.as_ref(), &["(frida-sessions)"]).await;
    assert_eq!(
        (stopped, kept, fake.calls()),
        (
            ExecuteStatus::Aborted,
            vec![Ok(json!([]))],
            [
                "spawn /bin/app []",
                "attach 4242",
                "kill 4242",
                "detach 4242"
            ]
            .map(str::to_string)
            .to_vec(),
        )
    );
}

/// When the target exits, its session says why, its scripts' waits end once
/// their logs are read, and detaching does not kill a process that is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_that_exits_ends_its_session_and_is_not_killed_again() {
    let fake = Arc::new(Fake::default());
    let kernel = provisioner(SchemeKernelOptions::default(), &fake)
        .ensure(None, None)
        .await
        .unwrap();
    values(
        kernel.as_ref(),
        &[r#"(begin (define s (frida-attach "/bin/app" #:spawn #t)) (define sc (frida-script-load s "send(1)")) #t)"#],
    )
    .await;
    (fake.sessions.lock().unwrap()[0])("process-terminated");
    let returned = values(
        kernel.as_ref(),
        &[
            "(hash-ref (frida-script-wait sc #:timeout-ms 600000) 'seq)",
            "(hash-ref (frida-script-wait sc #:timeout-ms 600000) 'seq)",
            "(frida-script-wait sc #:timeout-ms 600000)",
            "(hash-ref (car (frida-sessions)) 'detached-reason)",
            "(frida-detach s)",
        ],
    )
    .await;
    assert_eq!(
        (returned, fake.calls()[5..].to_vec()),
        (
            vec![
                Ok(json!(0)),
                Ok(json!(1)),
                Ok(json!(false)),
                Ok(json!("process-terminated")),
                Ok(json!(true)),
            ],
            vec!["detach 4242".to_string()],
        )
    );
}

/// A wait that outlives its cell's window goes on as a background cell and
/// brings the message when Frida's thread delivers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_wait_past_the_cells_window_goes_on_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let (completed, completions) = mpsc::channel();
    let completed = Mutex::new(completed);
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "cell.completed",
        host_handler(move |payload: HostRequestPayload| {
            // The test may have stopped listening.
            drop(completed.lock().unwrap().send(payload.data));
            async move { Ok(json!({})) }
        }),
    );
    let kernel = provisioner(
        SchemeKernelOptions {
            host_handlers: handlers,
            cwd: dir.path().to_path_buf(),
            cell_window: CellWindow::for_session(CellWindowConfig {
                daemonless: false,
                configured_ms: Some(100),
                agent_dir: dir.path(),
                project_dir: dir.path(),
                on_armed: None,
            }),
            ..SchemeKernelOptions::default()
        },
        &fake,
    )
    .ensure(None, None)
    .await
    .unwrap();
    values(
        kernel.as_ref(),
        &[r#"(begin (define s (frida-attach 7)) (define sc (frida-script-load s "send(1)")) (frida-script-wait sc) (frida-script-wait sc) #t)"#],
    )
    .await;
    let waiting = kernel
        .execute(
            "(hash-ref (frida-script-wait sc #:timeout-ms 600000) 'payload)",
            NO_OPTIONS,
        )
        .await
        .unwrap();
    let sink = Arc::clone(&fake.scripts.lock().unwrap()[0]);
    // As Frida does: from a thread that is none of the session's.
    std::thread::spawn(move || sink(r#"{"type":"send","payload":"late"}"#, None))
        .join()
        .unwrap();
    let completion = tokio::task::spawn_blocking(move || completions.recv_timeout(PATIENCE))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            waiting.backgrounded_cell.is_some(),
            &completion["status"],
            &completion["text"],
        ),
        (true, &json!("ok"), &json!("\"late\""))
    );
}

/// How a session's kernel can end while it holds Frida sessions.
#[derive(Debug, Clone, Copy)]
pub(super) enum Ending {
    /// `kill`: the session closed, or an idle one released its kernel.
    Killed,
    /// The hosted session's lifetime ended: it crashed, was stopped, or its
    /// daemon is shutting down.
    LifetimeEnded,
    /// The session was dropped without either.
    Dropped,
}

pub(super) const ENDINGS: [Ending; 3] = [Ending::Killed, Ending::LifetimeEnded, Ending::Dropped];

/// Start `provisioner`'s kernel inside a session lifetime, run `cell` in
/// it, end the kernel as `ending` says, and return what the cell returned.
pub(super) async fn ended_after(
    ending: Ending,
    provisioner: SchemeKernelProvisioner,
    cell: &str,
) -> Result<Value, String> {
    let scope = pa_types::session_scope::SessionScope::new(
        "instrumenting".to_string(),
        std::env::temp_dir(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeSet::new(),
    );
    // The caller's future stays on this thread, where the kernel starts.
    let entered = pa_types::session_scope::enter(Some(Arc::clone(&scope)));
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let returned = values(kernel.as_ref(), &[cell]).await.remove(0);
    match ending {
        Ending::Killed => provisioner.kill().await,
        Ending::LifetimeEnded => scope.end(),
        Ending::Dropped => {
            drop(kernel);
            drop(provisioner);
        }
    }
    drop(entered);
    returned
}

/// However the kernel ends, every session is detached, a target it spawned
/// is killed (one that was never resumed too), and the connection closes.
#[tokio::test(flavor = "multi_thread")]
async fn every_way_a_kernel_ends_detaches_everything_and_kills_what_it_spawned() {
    let mut left = Vec::new();
    for ending in ENDINGS {
        let fake = Arc::new(Fake::default());
        ended_after(
            ending,
            provisioner(SchemeKernelOptions::default(), &fake),
            r#"(begin (frida-attach "/bin/app" #:spawn #t #:pause #t) (frida-attach 7) #t)"#,
        )
        .await
        .unwrap();
        left.push(fake.calls()[3..].to_vec());
    }
    assert_eq!(
        left,
        vec![
            ["kill 4242", "detach 4242", "detach 7", "close"]
                .map(str::to_string)
                .to_vec();
            3
        ]
    );
}

/// A build without the `frida` feature keeps the functions, and each says
/// why it cannot work; the refused call is counted.
#[cfg(not(feature = "frida"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_build_without_frida_says_so_from_every_function() {
    let (handlers, facts) = counting();
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        host_handlers: handlers,
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    let refused = values(kernel.as_ref(), &["(frida-devices)", "(frida-attach 1)"]).await;
    assert_eq!(
        (
            refused,
            facts.recv_timeout(PATIENCE),
            facts.recv_timeout(PATIENCE)
        ),
        (
            vec![
                Err(format!(
                    "Error: Generic: frida-devices: {}",
                    crate::kernel::scheme::frida::UNAVAILABLE
                )),
                Err(format!(
                    "Error: Generic: frida-attach: {}",
                    crate::kernel::scheme::frida::UNAVAILABLE
                )),
            ],
            Ok("unavailable".to_string()),
            Ok("unavailable".to_string()),
        )
    );
}
