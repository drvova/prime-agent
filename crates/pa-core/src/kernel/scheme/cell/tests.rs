//! Cells through a real session kernel: what they show, how they stream,
//! stop, and move to the background.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

#[cfg(unix)]
use crate::interaction::InteractionHub;
use crate::kernel::cell_window::{CellWindow, CellWindowConfig};
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::kernel::shared::{
    host_handler, BackgroundCellFact, HostRequestHandlers, HostRequestPayload,
    KernelSentAgentMessage, ReceiverRole, SentAgentMessageTarget, SentDeliveryStatus,
};
#[cfg(unix)]
use crate::kernel::shared::{KernelBashCommands, KernelBashWaits, KernelDiffDisplay};
#[cfg(unix)]
use crate::session::manager::SessionManager;
use crate::tools::kernel_cell::{
    ExecuteResult, ExecuteStatus, KernelExecuteOptions, KernelExecutor, KernelProvisioner,
};
use crate::tools::tool_definition::AbortSignal;

/// How long a test waits for something that must happen before it fails.
const PATIENCE: Duration = Duration::from_secs(30);

async fn run(kernel: &dyn KernelExecutor, code: &str) -> ExecuteResult {
    kernel
        .execute(
            code,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        )
        .await
        .unwrap()
}

/// Run one cell with a limit of its own. A session with a window reads the
/// limit as that cell's window, so the cell waits for the engine and runs to
/// its end, however slow the machine is, where [`run`] would move it to the
/// background or give up on the engine after the session's 100 ms.
async fn run_to_its_end(kernel: &dyn KernelExecutor, code: &str) -> ExecuteResult {
    kernel
        .execute(
            code,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: Some(PATIENCE.as_millis() as u64),
                on_stream: None,
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        )
        .await
        .unwrap()
}

/// A handler that answers once `release` is notified.
fn held(release: &Arc<tokio::sync::Notify>) -> crate::kernel::shared::HostHandlerFn {
    let release = Arc::clone(release);
    host_handler(move |_payload: HostRequestPayload| {
        let release = Arc::clone(&release);
        async move {
            release.notified().await;
            Ok(json!({}))
        }
    })
}

/// A handler that passes each payload it gets to the returned receiver.
fn reporting() -> (
    crate::kernel::shared::HostHandlerFn,
    tokio::sync::mpsc::UnboundedReceiver<Value>,
) {
    let (sent, received) = tokio::sync::mpsc::unbounded_channel();
    let handler = host_handler(move |payload: HostRequestPayload| {
        // The test may have stopped listening.
        let _ = sent.send(payload.data);
        async move { Ok(json!({})) }
    });
    (handler, received)
}

/// A kernel whose cells move to the background after 100 ms, and the facts
/// its background cells report.
async fn windowed_kernel(
    handlers: HostRequestHandlers,
    dir: &std::path::Path,
) -> (Box<dyn KernelExecutor>, Arc<Mutex<Vec<&'static str>>>) {
    let facts = Arc::new(Mutex::new(Vec::new()));
    let noted = Arc::clone(&facts);
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        host_handlers: handlers,
        cwd: dir.to_path_buf(),
        cell_window: CellWindow::for_session(CellWindowConfig {
            daemonless: false,
            configured_ms: Some(100),
            agent_dir: dir,
            project_dir: dir,
            on_armed: None,
        }),
        on_background_cell: Some(Arc::new(move |fact| {
            noted.lock().unwrap().push(match fact {
                BackgroundCellFact::Backgrounded => "backgrounded",
                BackgroundCellFact::EndedOk { ran_for: Some(_) } => "ended ok",
                BackgroundCellFact::EndedOk { ran_for: None } => "ended ok, untimed",
                BackgroundCellFact::EndedWithError { .. } => "ended with error",
                BackgroundCellFact::Interrupted => "interrupted",
            });
        })),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    (kernel, facts)
}

/// Everything a cell's host calls show lands on its result as the Python
/// kernel's display events did, and `rlm-end-turn` marks the cell.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_cells_host_calls_show_on_its_result_and_end_turn_marks_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "one\ntwo\n").unwrap();
    let session = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
        dir.path(),
    )));
    let mut handlers = HostRequestHandlers::new();
    InteractionHub::headless().register_host_handlers(&session, &mut handlers);
    let receipt = |id: &str| {
        json!({
            "id": id,
            "message": "status?",
            "deliveryStatus": "delivered",
            "target": {"activeSessionId": "a1", "sessionId": "s1", "sessionName": "worker"},
        })
    };
    handlers.register(
        "agent_message.send",
        host_handler(move |payload: HostRequestPayload| {
            let reply = if payload.data["target"] == json!("all") {
                json!({"receipts": [receipt("am_2")]})
            } else {
                receipt("am_1")
            };
            async move { Ok(reply) }
        }),
    );
    handlers.register(
        "rlm.session.messages",
        host_handler(|_payload: HostRequestPayload| async move {
            Ok(json!({
                "messages": [{"index": 4, "id": "e5", "message": {"role": "user", "content": "hi"}}],
                "total": 5,
            }))
        }),
    );
    let kernel = SchemeKernelProvisioner::new(handlers, dir.path())
        .ensure(None, None)
        .await
        .unwrap();
    let result = run(
        kernel.as_ref(),
        r#"(todo-set (list "write the tests"))
           (ask "Which store?" (list "sqlite" "postgres") #:default "sqlite")
           (agent-message-send "status?" #:receiver-role "child" #:receiver-name "worker")
           (agent-message-broadcast "status?")
           (rlm-session-messages #:limit 1 #:role "user")
           (fs-edit "notes.txt" (hash 'old "two" 'new "three"))
           (bash "true")
           (hash-ref (rlm-end-turn) 'endedTurn)"#,
    )
    .await;
    let sent = |id: &str, receiver_role| KernelSentAgentMessage {
        id: id.to_string(),
        message: "status?".to_string(),
        delivery_status: SentDeliveryStatus::Delivered,
        receiver_role,
        target: SentAgentMessageTarget {
            active_session_id: "a1".to_string(),
            session_id: "s1".to_string(),
            session_name: Some("worker".to_string()),
        },
    };
    assert_eq!(
        (
            result.error,
            result.result,
            result.todo_list,
            result.questions,
            result.sent_agent_messages,
            result.transcript_reads,
            result.diffs,
            result.bash_commands,
            result.bash_waits,
            result.ended_turn,
        ),
        (
            None,
            Some("#true".to_string()),
            Some(json!({"items": [{"id": "1", "text": "write the tests", "status": "pending"}]})),
            vec![json!({"text": "Which store?", "status": "no_user", "answer": ["sqlite"]})],
            vec![sent("am_1", Some(ReceiverRole::Child)), sent("am_2", None)],
            vec![json!({"returned": 1, "first": 4, "total": 5, "roles": ["user"]})],
            vec![KernelDiffDisplay {
                path: pa_types::platform::canonicalize(dir.path())
                    .unwrap()
                    .join("notes.txt")
                    .to_string_lossy()
                    .into_owned(),
                old_str: "two".to_string(),
                new_str: "three".to_string(),
                start_line: Some(2),
            }],
            Some(KernelBashCommands {
                first: "true".to_string(),
                count: 1,
                lines: 1,
                elevated: false,
            }),
            KernelBashWaits {
                completed: 1,
                yielded: 0,
            },
            true,
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cell_has_one_id_and_the_next_cell_another() {
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    let first = run(kernel.as_ref(), "(display (cell-id)) (display (cell-id))").await;
    let second = run(kernel.as_ref(), "(display (cell-id))").await;
    let (once, again) = first.stdout.split_at(first.stdout.len() / 2);
    assert_eq!(
        (once == again, once.len(), once == second.stdout),
        (true, 32, false)
    );
}

/// The cell prints, then waits in the host until the stream has delivered
/// what it printed: output that only arrived with the result would leave
/// the cell waiting.
#[tokio::test(flavor = "multi_thread")]
async fn output_reaches_the_stream_while_the_cell_runs() {
    let streamed = Arc::new(tokio::sync::Notify::new());
    let mut handlers = HostRequestHandlers::new();
    handlers.register("stream.seen", held(&streamed));
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let chunks = Mutex::new(Vec::new());
    let on_stream = |text: &str, stream: &'static str| {
        chunks.lock().unwrap().push(format!("{stream}: {text}"));
        streamed.notify_one();
    };
    let result = tokio::time::timeout(
        PATIENCE,
        kernel.execute(
            r#"(display "héllo") (%host-request "stream.seen" (hash))
               (display "oops" (current-error-port)) (display "done")"#,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: Some(&on_stream),
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        ),
    )
    .await
    .expect("the cell's output was streamed while it ran")
    .unwrap();
    assert_eq!(
        (result.stdout, result.stderr, chunks.into_inner().unwrap()),
        (
            "héllodone".to_string(),
            "oops".to_string(),
            vec![
                "stdout: héllo".to_string(),
                "stderr: oops".to_string(),
                "stdout: done".to_string()
            ]
        )
    );
}

/// The engine cannot interrupt a host request: the bridge ends the wait with
/// the cell, and an open question is withdrawn from the user.
#[tokio::test(flavor = "multi_thread")]
async fn an_abort_ends_a_waiting_host_request_and_withdraws_its_question() {
    let (asked, mut was_asked) = tokio::sync::mpsc::unbounded_channel();
    let (cancel, mut cancelled) = reporting();
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "question.ask",
        host_handler(move |payload: HostRequestPayload| {
            let _ = asked.send(payload.data["id"].clone());
            std::future::pending()
        }),
    );
    handlers.register("question.cancel", cancel);
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let signal = AbortSignal::new();
    let abort = signal.clone();
    let aborting = tokio::spawn(async move {
        let id = was_asked.recv().await.unwrap();
        abort.cancel();
        id
    });
    let result = tokio::time::timeout(
        PATIENCE,
        kernel.execute(
            r#"(ask "Proceed?")"#,
            KernelExecuteOptions {
                signal: Some(signal),
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        ),
    )
    .await
    .expect("the abort ended the waiting request")
    .unwrap();
    let id = aborting.await.unwrap();
    assert_eq!(
        (result.status, cancelled.recv().await),
        (
            ExecuteStatus::Aborted,
            Some(json!({"type": "question.cancel", "id": id}))
        )
    );
}

/// A cell that outlives its window keeps the engine: the call returns with
/// its id, another cell is refused, the control forms are answered without
/// the engine, and its end is announced once and withdrawn when read.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_that_outlives_its_window_goes_on_in_the_background_and_holds_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let (completed, mut completions) = reporting();
    let (consumed, mut withdrawals) = reporting();
    let mut handlers = HostRequestHandlers::new();
    handlers.register("gate.wait", held(&release));
    handlers.register("cell.completed", completed);
    handlers.register("cell.consumed", consumed);
    let (kernel, facts) = windowed_kernel(handlers, dir.path()).await;
    let long = run(
        kernel.as_ref(),
        r#"(displayln "before") (%host-request "gate.wait" (hash)) (displayln "after") (* 6 7)"#,
    )
    .await;
    let id = long.backgrounded_cell.clone().unwrap();
    let refused = run(kernel.as_ref(), "(+ 1 2)").await;
    let running = run(kernel.as_ref(), &format!("(cell-status \"{id}\")")).await;
    release.notify_one();
    let completion = completions.recv().await;
    let after = run_to_its_end(kernel.as_ref(), "(+ 1 2)").await;
    let read = run_to_its_end(
        kernel.as_ref(),
        &format!(
            "(let ([state (cell-status \"{id}\")])
               (list (hash-ref state 'status) (hash-ref state 'text) (hash-ref state 'output)))"
        ),
    )
    .await;
    assert_eq!(
        (
            (long.status, long.stdout, long.result),
            refused.error.map(|error| (
                error.ename,
                error.evalue.contains(&format!("cell {id} still holds it"))
            )),
            running
                .result
                .is_some_and(|state| state.contains("\"running\"")),
            completion,
            after.result,
            read.result,
            withdrawals.recv().await,
            facts.lock().unwrap().clone(),
        ),
        (
            (ExecuteStatus::Ok, "before\n".to_string(), None),
            Some(("KernelBusy".to_string(), true)),
            true,
            Some(json!({
                "type": "cell.completed",
                "cell": id,
                "status": "ok",
                "text": "42",
                "output": "after",
                "outputTruncated": false,
            })),
            Some("3".to_string()),
            Some("(\"ok\" \"42\" \"after\\n\")".to_string()),
            Some(json!({"type": "cell.consumed", "cell": id})),
            vec!["backgrounded", "ended ok"],
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_background_cell_stops_it_and_frees_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "never.answers",
        host_handler(|_payload: HostRequestPayload| std::future::pending()),
    );
    let (kernel, facts) = windowed_kernel(handlers, dir.path()).await;
    let long = run(kernel.as_ref(), r#"(%host-request "never.answers" (hash))"#).await;
    let id = long.backgrounded_cell.unwrap();
    let cancelled = run_to_its_end(kernel.as_ref(), &format!("(cell-cancel \"{id}\")")).await;
    let after = run_to_its_end(kernel.as_ref(), "(+ 1 2)").await;
    assert_eq!(
        (
            cancelled
                .result
                .is_some_and(|state| state.contains("\"interrupted\"")),
            after.result,
            facts.lock().unwrap().clone(),
        ),
        (
            true,
            Some("3".to_string()),
            vec!["backgrounded", "interrupted"]
        )
    );
}

/// Steel parks the engine's thread when a host function returns to an
/// engine it reads as paused but not interrupted (an interrupt is two
/// writes), and never unparks it: a stop that only raised the interrupt left
/// such a cell on the engine for good. The host call here parks the cell's
/// thread as that safepoint does.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_wakes_a_cell_whose_thread_is_parked() {
    let (parking, mut parked) = tokio::sync::mpsc::unbounded_channel();
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "thread.park",
        host_handler(move |_payload: HostRequestPayload| {
            // The test may have stopped listening.
            let _ = parking.send(std::thread::current());
            std::thread::park();
            async move { Ok(json!({})) }
        }),
    );
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let signal = AbortSignal::new();
    let stop = signal.clone();
    let cell = kernel.execute(
        r#"(%host-request "thread.park" (hash))"#,
        KernelExecuteOptions {
            signal: Some(signal),
            timeout_ms: None,
            on_stream: None,
            on_late_sent_agent_message: None,
            tool_call_id: None,
        },
    );
    let (stopped, thread) = tokio::join!(tokio::time::timeout(PATIENCE, cell), async {
        let thread = parked.recv().await.unwrap();
        stop.cancel();
        thread
    });
    // Let go of a thread the stop left parked, so that failure ends.
    thread.unpark();
    assert_eq!(
        stopped.ok().map(|result| result.unwrap().status),
        Some(ExecuteStatus::Aborted)
    );
}

/// A message a cell sends after its call returned cannot ride that call's
/// result: it goes to the call's late-message handler.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_cells_agent_message_reaches_the_late_handler() {
    let dir = tempfile::tempdir().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let mut handlers = HostRequestHandlers::new();
    handlers.register("gate.wait", held(&release));
    handlers.register(
        "agent_message.send",
        host_handler(|_payload: HostRequestPayload| async move {
            Ok(json!({
                "id": "am_9",
                "message": "done",
                "deliveryStatus": "queued",
                "target": {"activeSessionId": "a0", "sessionId": "s0"},
            }))
        }),
    );
    let (kernel, _facts) = windowed_kernel(handlers, dir.path()).await;
    let (late, mut arrived) = tokio::sync::mpsc::unbounded_channel();
    let result = kernel
        .execute(
            r#"(%host-request "gate.wait" (hash)) (agent-message-send "done" #:receiver-role "parent")"#,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: Some(Arc::new(move |message| {
                    // The test may have stopped listening.
                    let _ = late.send(message);
                })),
                tool_call_id: None,
            },
        )
        .await
        .unwrap();
    release.notify_one();
    assert_eq!(
        (result.sent_agent_messages, arrived.recv().await),
        (
            Vec::new(),
            Some(KernelSentAgentMessage {
                id: "am_9".to_string(),
                message: "done".to_string(),
                delivery_status: SentDeliveryStatus::Queued,
                receiver_role: Some(ReceiverRole::Parent),
                target: SentAgentMessageTarget {
                    active_session_id: "a0".to_string(),
                    session_id: "s0".to_string(),
                    session_name: None,
                },
            })
        )
    );
}

/// Steel's prelude binds hundreds of names; a kernel function that reused
/// one would replace a builtin for every cell.
#[test]
fn no_kernel_function_replaces_a_steel_builtin() {
    let mut engine = steel::steel_vm::engine::Engine::new();
    let taken: Vec<&str> = [crate::kernel::scheme::HOST_BRIDGE, super::CELL_API]
        .iter()
        .flat_map(|source| source.lines())
        .filter_map(|line| line.strip_prefix("(define ("))
        .filter_map(|rest| rest.split([' ', ')']).next())
        .filter(|name| engine.run((*name).to_string()).is_ok())
        .collect();
    assert_eq!(taken, Vec::<&str>::new());
}

/// The session's shell, command prefix and environment reach every command.
/// A shell process is asked for, as a `shellPath` setting asks for one: the
/// program a command runs under (`$0`) is what the setting is for.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn commands_run_under_the_sessions_shell_prefix_and_environment() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = dir.path().join("session-7");
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        shell: crate::platform::KernelShell::System,
        shell_path: Some("/bin/sh".into()),
        command_prefix: Some("greeting=hello".to_string()),
        agent_dir: Some(dir.path().join("agent")),
        env: vec![("EXTRA_SETTING".to_string(), "on".to_string())],
        session_id: Some("active-1".to_string()),
        artifact_dir: Some(artifacts.clone()),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    let result = run(
        kernel.as_ref(),
        r#"(list (hash-ref (bash "echo $0 $greeting $EXTRA_SETTING $RLM_SESSION_ID $RLM_SESSION_DIR $PRIME_AGENT_CODING_AGENT_DIR") 'output)
                 (session-info "id")
                 (session-info "artifact-dir"))"#,
    )
    .await;
    let artifacts = artifacts.display();
    assert_eq!(
        (result.error, result.result),
        (
            None,
            Some(format!(
                "(\"/bin/sh hello on session-7 {artifacts} {}\\n\" \"active-1\" \"{artifacts}\")",
                dir.path().join("agent").display()
            ))
        )
    );
}

/// A cell's edits after its call returned have no card to show on: the
/// cell's completion notice names their files.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_background_cells_later_edits_are_named_by_its_completion_notice() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("words.txt"), "alpha\nbeta\n").unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let (completed, mut completions) = reporting();
    let mut handlers = HostRequestHandlers::new();
    handlers.register("gate.wait", held(&release));
    handlers.register("cell.completed", completed);
    let (kernel, _facts) = windowed_kernel(handlers, dir.path()).await;
    let long = run(
        kernel.as_ref(),
        r#"(%host-request "gate.wait" (hash)) (fs-edit "words.txt" (hash 'old "beta" 'new "BETA")) (displayln "edited")"#,
    )
    .await;
    release.notify_one();
    let notice = tokio::time::timeout(PATIENCE, completions.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            long.diffs,
            notice["status"].clone(),
            notice["output"].clone(),
            std::fs::read_to_string(dir.path().join("words.txt")).unwrap(),
        ),
        (
            Vec::new(),
            json!("ok"),
            json!(format!(
                "[files edited after the call returned: {}]\nedited",
                dir.path().join("words.txt").display()
            )),
            "alpha\nBETA\n".to_string(),
        )
    );
}

/// A host call that does not watch the cell's signal cannot be stopped: the
/// session's host is told, with the cell still on the engine.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_that_outlives_its_interrupt_is_reported_to_the_sessions_host() {
    let (stuck, mut reported) = tokio::sync::mpsc::unbounded_channel();
    let scope = pa_types::session_scope::SessionScope::new(
        "stuck-cell".to_string(),
        std::env::temp_dir(),
        std::collections::BTreeMap::from([
            (
                "PRIME_AGENT_STUCK_CELL_GRACE_MS".to_string(),
                "100".to_string(),
            ),
            // No kernel function ignores its cell's stop: the test asks for
            // the one that does.
            (
                "PRIME_AGENT_TEST_BLOCKING_CELL_SEAM".to_string(),
                "1".to_string(),
            ),
        ]),
        std::collections::BTreeSet::new(),
    );
    scope.set_slot(Arc::new(super::StuckCellSink(Box::new(move |detail| {
        let _ = stuck.send(detail.to_string());
    }))));
    // This test's future stays on this thread, where the interrupt is raised.
    let _scope = pa_types::session_scope::enter(Some(scope));
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    let signal = AbortSignal::new();
    // One native call that watches nothing: it writes its marker from inside
    // the call and stays there until the marker is removed, so no interrupt
    // reaches the cell however late the abort below is raised.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("blocked");
    let execute = kernel.execute(
        &format!("(%test-block {:?})", marker.display().to_string()),
        KernelExecuteOptions {
            signal: Some(signal.clone()),
            timeout_ms: None,
            on_stream: None,
            on_late_sent_agent_message: None,
            tool_call_id: None,
        },
    );
    tokio::pin!(execute);
    let report = tokio::time::timeout(PATIENCE, async {
        // The cell is inside the call before it is stopped.
        while !marker.exists() {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
                result = &mut execute => panic!("the cell returned: {:?}", result.map(|result| result.status)),
            }
        }
        signal.cancel();
        tokio::select! {
            report = reported.recv() => report,
            result = &mut execute => panic!("the cell returned: {:?}", result.map(|result| result.status)),
        }
    })
    .await
    .expect("the stuck cell was reported");
    // The abandoned call is let go.
    std::fs::remove_file(&marker).unwrap();
    assert_eq!(
        report.as_deref(),
        Some("a kernel cell did not stop within 100 ms of its interrupt: a host call is not observing the cell's cancellation")
    );
}

/// After a stop a cell leaves the engine at once, whatever it was waiting
/// in: a sleep ends with its cell, standard input is an empty port, and the
/// builtins that wait for another thread are closed.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_cell_leaves_the_engine_whatever_it_was_waiting_in() {
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    let signal = AbortSignal::new();
    let abort = signal.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let printed = Arc::clone(&started);
    tokio::spawn(async move {
        started.notified().await;
        abort.cancel();
    });
    let on_stream = move |_text: &str, _stream: &'static str| printed.notify_one();
    let slept = tokio::time::timeout(
        PATIENCE,
        kernel.execute(
            "(displayln \"sleeping\") (time/sleep-ms 600000)",
            KernelExecuteOptions {
                signal: Some(signal),
                timeout_ms: None,
                on_stream: Some(&on_stream),
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        ),
    )
    .await
    .expect("the sleeping cell ended with its stop")
    .unwrap();
    let after = run(
        kernel.as_ref(),
        "(define (refused? thunk) (with-handler (lambda (raised) #t) (begin (thunk) #f)))
         (list (eof-object? (read-line))
               (time/sleep-ms 1)
               (refused? (lambda () (channel/recv (channels-receiver (channels/new)))))
               (refused? (lambda () (lock-acquire! (mutex))))
               (refused? (lambda () (block-on 1))))",
    )
    .await;
    assert_eq!(
        (
            slept.status,
            after.result,
            after.error.map(|error| error.evalue)
        ),
        (
            ExecuteStatus::Aborted,
            Some("(#true #<void> #true #true #true)".to_string()),
            None
        )
    );
}

/// A hosted session's lifetime can end without anyone stopping its kernel
/// (the host drops the session's runtime). The cell on the engine is
/// stopped all the same, and what is left of the lifetime writes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_lifetime_that_ends_under_a_running_cell_stops_it_and_its_writes() {
    let dir = tempfile::tempdir().unwrap();
    let scope = pa_types::session_scope::SessionScope::new(
        "ending".to_string(),
        dir.path().to_path_buf(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeSet::new(),
    );
    // This test's future stays on this thread, where the kernel is started.
    let _scope = pa_types::session_scope::enter(Some(Arc::clone(&scope)));
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        agent_dir: Some(dir.path().join("agent")),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let printed = Arc::clone(&started);
    let ending = Arc::clone(&scope);
    tokio::spawn(async move {
        started.notified().await;
        ending.end();
    });
    let on_stream = move |_text: &str, _stream: &'static str| printed.notify_one();
    // The cell spins until it is stopped, then tries to leave a file behind.
    let spun = tokio::time::timeout(
        PATIENCE,
        kernel.execute(
            "(displayln \"spinning\")
             (with-handler (lambda (stopped) (harness-create-memory \"left\" \"behind\"))
               (let spin () (spin)))",
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: Some(&on_stream),
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        ),
    )
    .await
    .expect("the cell ended with its session's lifetime")
    .unwrap();
    assert_eq!(
        (
            spun.status,
            pa_types::session_scope::ensure_live().is_err(),
            dir.path().join("agent").exists(),
        ),
        (ExecuteStatus::Error, true, false)
    );
}

/// What is left of an ended lifetime does not touch the workspace either.
/// The engine's stop reaches a cell between its steps, but a thread that is
/// already past it (inside a native call, or one that wakes into one) reaches
/// the file and command functions directly: each refuses, and the user's file
/// is as the rebuilt session finds it.
#[test]
fn a_thread_left_over_from_an_ended_lifetime_cannot_change_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let scope = pa_types::session_scope::SessionScope::new(
        "ending".to_string(),
        dir.path().to_path_buf(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeSet::new(),
    );
    std::fs::write(dir.path().join("kept.txt"), "original").unwrap();
    // The runtime's threads are the session's, as a hosted session's are.
    // The kernel is started from this thread, outside the scope, so the
    // lifetime's end does not stop its cell: the cell stands for a thread
    // the stop came too late for. It runs inside the scope, the lifetime
    // ends as it prints, and the thread that is left calls the native
    // functions.
    let runtime = {
        let _scope = pa_types::session_scope::enter(Some(Arc::clone(&scope)));
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .on_thread_start(pa_types::session_scope::inherit())
            .build()
            .unwrap()
    };
    let kernel = runtime
        .block_on(
            SchemeKernelProvisioner::with_options(SchemeKernelOptions {
                cwd: dir.path().to_path_buf(),
                ..SchemeKernelOptions::default()
            })
            .ensure(None, None),
        )
        .unwrap();
    let ending = Arc::clone(&scope);
    let on_stream = move |_text: &str, _stream: &'static str| ending.end();
    let ended = runtime
        .block_on(async {
            tokio::time::timeout(
                PATIENCE,
                kernel.execute(
                    "(define (refusal thunk)
               (with-handler (lambda (refused) (error-object-message refused)) (thunk)))
             (displayln \"ready\")
             ; The lifetime ends on another thread once this has printed: wait
             ; for it by reading, which the ended lifetime is refused too.
             (let wait ()
               (when (with-handler (lambda (refused) #f) (fs-read \"kept.txt\") #t)
                 (wait)))
             (list (refusal (lambda () (fs-write \"kept.txt\" \"overwritten\")))
                   (refusal (lambda () (fs-edit \"kept.txt\" (hash 'old \"original\" 'new \"edited\"))))
                   (refusal (lambda () (bash \"echo ran > ran.txt\"))))",
                    KernelExecuteOptions {
                        signal: None,
                        timeout_ms: None,
                        on_stream: Some(&on_stream),
                        on_late_sent_agent_message: None,
                        tool_call_id: None,
                    },
                ),
            )
            .await
        })
        .expect("the cell returned")
        .unwrap();
    let refused = |function: &str| {
        format!("{function}: session lifetime ending has ended: its remaining threads do not write")
    };
    assert_eq!(
        (
            ended.result,
            std::fs::read_to_string(dir.path().join("kept.txt")).unwrap(),
            dir.path().join("ran.txt").exists(),
        ),
        (
            Some(format!(
                "({:?} {:?} {:?})",
                refused("fs-write"),
                refused("fs-edit"),
                refused("bash")
            )),
            "original".to_string(),
            false,
        )
    );
}

/// A kernel of a saved session: its cut output pages out to the store under
/// `dir`, which the `ctx-*` functions read.
async fn paging_kernel(
    dir: &std::path::Path,
) -> (Box<dyn KernelExecutor>, crate::kernel::spill::SpillStore) {
    let store = crate::kernel::spill::SpillStore::for_session_dir(dir);
    let mut handlers = HostRequestHandlers::new();
    crate::kernel::spill::host::register_ctx_handlers(
        &mut handlers,
        Some(&store),
        /*observer*/ None,
    );
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        host_handlers: handlers,
        cwd: dir.to_path_buf(),
        artifact_dir: Some(dir.to_path_buf()),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    (kernel, store)
}

/// A cell whose long, noisy output is cut returns the lines that carry
/// information (its ends, the error and its location, each run of the
/// repeated progress line once with its count) instead of its first
/// screenfuls, then the handle; the handle still pages everything the cell
/// printed.
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_cells_output_is_previewed_by_information_and_pages_back_whole() {
    use crate::kernel::spill::{content_id, group_digits, SpillLookup, StoredSpill};

    let dir = tempfile::tempdir().unwrap();
    let (kernel, store) = paging_kernel(dir.path()).await;
    let first = "   Compiling app v0.1.0 (/work/app)";
    let progress = "    Building [=======>                 ] 8/24: serde_derive";
    let error = "error[E0308]: mismatched types";
    let location = " --> src/lib.rs:6:22";
    let last = "error: could not compile `app` (lib) due to 1 previous error";
    let noise = format!("{progress}\n").repeat(1500);
    let printed = format!("{first}\n{noise}{error}\n{location}\n{noise}{last}\n");
    let id = content_id(&printed);
    let cut = run(
        kernel.as_ref(),
        &format!(
            "(define (progress-lines n)
               (when (> n 0) (displayln {progress:?}) (progress-lines (- n 1))))
             (displayln {first:?})
             (progress-lines 1500)
             (displayln {error:?})
             (displayln {location:?})
             (progress-lines 1500)
             (displayln {last:?})"
        ),
    )
    .await;
    let paged = run(
        kernel.as_ref(),
        &format!("(display (hash-ref (ctx-page \"out:{id}\" #:start 1500 #:lines 3) 'text))"),
    )
    .await;
    let preview =
        format!("{first}\n{progress}  ×1500\n{error}\n{location}\n{progress}  ×1500\n{last}");
    let total = printed.chars().count();
    assert_eq!(
        (cut.stdout, paged.stdout, store.lookup(&id).await),
        (
            format!(
                "{preview}\n[out:{id} · {} chars · 3,004 lines · {} chars not shown]",
                group_digits(total),
                group_digits(total - preview.chars().count()),
            ),
            format!("{progress}\n{error}\n{location}\n"),
            SpillLookup::Stored(StoredSpill {
                text: printed,
                total_chars: total,
                lines: 3004,
            }),
        )
    );
}

/// A cell keeps the first 262,144 bytes of what it prints: the preview and
/// the handle cover those, and the result says how much came after them.
#[tokio::test(flavor = "multi_thread")]
async fn output_printed_past_what_a_cell_keeps_is_named_after_its_preview() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _store) = paging_kernel(dir.path()).await;
    let kept = "x".repeat(262_144);
    let cut = run(kernel.as_ref(), "(display (make-string 300000 #\\x))").await;
    assert_eq!(
        cut.stdout,
        format!(
            "{}…\n[out:{} · 262,144 chars · 1 lines · 261,943 chars not shown]\n[... 37856 more bytes were printed after the first 262144 and were not kept ...]",
            "x".repeat(200),
            crate::kernel::spill::content_id(&kept),
        )
    );
}

/// A long session redefines its functions cell after cell, and a closure
/// it kept in a vector goes on calling the definitions it was compiled
/// against. Steel collects the slots of redefined globals once a hundred
/// are shadowed, and it kept the slot such a closure calls without visiting
/// the function in it: the globals that function calls were freed, and the
/// kept closure called `#<void>` or whatever was defined into the slot next
/// (steel #735, fixed in `vendor/steel-core`).
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_closure_keeps_calling_what_it_was_compiled_against() {
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    // The padding keeps `inner` from being inlined, so `outer` calls it
    // through its global slot.
    let padding = "(set! sink (+ sink 1)) ".repeat(40);
    let definitions = |round: usize| {
        format!(
            "(define (inner x) (begin {padding} (+ x {round})))
             (define (outer x) (begin {padding} (inner x)))"
        )
    };
    run_to_its_end(
        kernel.as_ref(),
        &format!(
            "(define sink 0) (define kept (mutable-vector #f)) {}
             (vector-set! kept 0 (lambda (x) (outer x)))",
            definitions(0)
        ),
    )
    .await;
    let mut wrong = Vec::new();
    for round in 1..=300 {
        run_to_its_end(kernel.as_ref(), &definitions(round)).await;
        let called = run_to_its_end(kernel.as_ref(), "((vector-ref kept 0) 1)").await;
        if called.result.as_deref() != Some("1") {
            wrong.push((round, called.result, called.error.map(|error| error.evalue)));
        }
    }
    assert_eq!(wrong, Vec::new());
}

/// What the reference tells the model about redefinition (`scheme.md`),
/// cell by cell: a later `define` is a new binding that functions compiled
/// earlier do not see, until they are defined again; `set!` reaches them
/// only where the compiler had not inlined the old value into its caller;
/// what sits in a box changes for every reader.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_definition_does_not_change_what_earlier_functions_call() {
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    let mut answers = Vec::new();
    for code in [
        "(define (helper x) (list 'old x))
         (define (caller x) (helper x))
         (define settings (hash 'mode 'old))
         (define (mode) (hash-ref settings 'mode))
         (define handler (box helper))
         (define (handle x) ((unbox handler) x))
         (define (small x) (list 'old x))
         (define (uses-small x) (small x))
         (list (caller 1) (mode) (handle 1))",
        "(define (helper x) (list 'new x))
         (define settings (hash 'mode 'new))
         (list (helper 1) (caller 1) (mode))",
        "(define (caller x) (helper x))
         (define (mode) (hash-ref settings 'mode))
         (list (caller 1) (mode))",
        // A small function is inlined into a caller of its own cell. The
        // caller defined in a later cell, and the reader of a hash built at
        // run time, read the global.
        "(set! helper (lambda (x) (list 'set x)))
         (set! small (lambda (x) (list 'set x)))
         (set! settings (hash 'mode 'set))
         (list (caller 1) (uses-small 1) (mode))",
        "(set-box! handler (lambda (x) (list 'boxed x)))
         (handle 1)",
    ] {
        answers.push(run_to_its_end(kernel.as_ref(), code).await.result);
    }
    assert_eq!(
        answers,
        [
            "((old 1) old (old 1))",
            "((new 1) (old 1) old)",
            "((new 1) new)",
            "((set 1) (old 1) set)",
            "(boxed 1)",
        ]
        .map(|answer| Some(answer.to_string()))
    );
}

/// A question waits for the user inside its cell's call: the yield window
/// does not run while it is open, so an answer that comes after the window
/// still comes back in the same call instead of in a background cell.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_answered_after_the_window_returns_in_its_own_call() {
    let dir = tempfile::tempdir().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let (opened, mut asked) = tokio::sync::mpsc::unbounded_channel();
    let mut handlers = HostRequestHandlers::new();
    let answering = Arc::clone(&release);
    handlers.register(
        "question.ask",
        host_handler(move |_payload: HostRequestPayload| {
            let release = Arc::clone(&answering);
            // The test may have stopped listening.
            let _ = opened.send(());
            async move {
                release.notified().await;
                Ok(json!({"status": "answered", "answer": ["done"]}))
            }
        }),
    );
    let (kernel, facts) = windowed_kernel(handlers, dir.path()).await;
    let call = run(
        kernel.as_ref(),
        r#"(hash-ref (ask "Approve the prompt, then pick one" (list "done" "it failed")) 'answer)"#,
    );
    tokio::pin!(call);
    tokio::select! {
        _ = asked.recv() => {}
        result = &mut call => panic!("the cell returned before its question opened: {result:?}"),
    }
    // Five of the session's 100 ms windows pass while the user thinks; a
    // call that returns meanwhile left its question to a background cell.
    let early = tokio::time::timeout(Duration::from_millis(500), &mut call).await;
    release.notify_one();
    let result = match early {
        Ok(early) => early,
        Err(_) => tokio::time::timeout(PATIENCE, call)
            .await
            .expect("the answer ended the call"),
    };
    assert_eq!(
        (
            result.status,
            result.result,
            result.backgrounded_cell,
            facts.lock().unwrap().clone(),
        ),
        (
            ExecuteStatus::Ok,
            Some("\"done\"".to_string()),
            None,
            Vec::<&str>::new()
        )
    );
}

/// `wait-until` checks until its condition holds and returns its value, in
/// the cell's own call however long past the window that takes.
#[tokio::test(flavor = "multi_thread")]
async fn wait_until_returns_when_its_condition_holds_inside_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, facts) = windowed_kernel(HostRequestHandlers::new(), dir.path()).await;
    let result = run(
        kernel.as_ref(),
        "(define checked 0)
         (let ([waited (wait-until (lambda () (set! checked (+ checked 1)) (and (>= checked 4) checked))
                                   #:every 0.1)])
           (list (hash-ref waited 'status) (hash-ref waited 'value) (hash-ref waited 'checks)))",
    )
    .await;
    assert_eq!(
        (
            result.status,
            result.result,
            result.stdout,
            result.backgrounded_cell,
            facts.lock().unwrap().clone(),
        ),
        (
            ExecuteStatus::Ok,
            Some("(\"met\" 4 4)".to_string()),
            "waiting up to 60 s for the condition\nthe condition: met after 0 s\n".to_string(),
            None,
            Vec::<&str>::new()
        )
    );
}

/// A condition that never holds ends the wait at its timeout, with a result
/// that says so.
#[tokio::test(flavor = "multi_thread")]
async fn wait_until_times_out_with_a_result_that_says_so() {
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    let result = run(
        kernel.as_ref(),
        r#"(let ([waited (wait-until (lambda () #f) #:timeout 0.3 #:every 0.1 #:label "the flag file")])
             (list (hash-ref waited 'status) (hash-ref waited 'value)))"#,
    )
    .await;
    assert_eq!(
        (result.status, result.result, result.stdout),
        (
            ExecuteStatus::Ok,
            Some("(\"timed_out\" #false)".to_string()),
            "waiting up to 0.3 s for the flag file\nthe flag file: timed out after 0 s\n"
                .to_string()
        )
    );
}

/// A shell command is a condition too: it holds once it exits 0, here once
/// a background command has written its file.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn wait_until_takes_a_shell_command_that_holds_when_it_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _facts) = windowed_kernel(HostRequestHandlers::new(), dir.path()).await;
    let result = run_to_its_end(
        kernel.as_ref(),
        r#"(bash "sleep 0.3 && echo ready > flag" (hash 'background #t))
           (let ([waited (wait-until "test -f flag" #:every 0.05)])
             (list (hash-ref waited 'status) (hash-ref (hash-ref waited 'value) 'exit-code)))"#,
    )
    .await;
    assert_eq!(result.result, Some("(\"met\" 0)".to_string()));
}

/// A stopped cell stops its wait at once and frees the engine.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_ends_wait_until_at_once() {
    let started = Arc::new(tokio::sync::Notify::new());
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    let signal = AbortSignal::new();
    let abort = signal.clone();
    let waiting = Arc::clone(&started);
    let aborting = tokio::spawn(async move {
        waiting.notified().await;
        abort.cancel();
    });
    let on_stream = |_text: &str, _stream: &'static str| started.notify_one();
    let result = tokio::time::timeout(
        PATIENCE,
        kernel.execute(
            "(wait-until (lambda () #f) #:timeout 3600 #:every 0.05)",
            KernelExecuteOptions {
                signal: Some(signal),
                timeout_ms: None,
                on_stream: Some(&on_stream),
                on_late_sent_agent_message: None,
                tool_call_id: None,
            },
        ),
    )
    .await
    .expect("the stop ended the wait")
    .unwrap();
    aborting.await.unwrap();
    let after = run(kernel.as_ref(), "(+ 1 2)").await;
    assert_eq!(
        (result.status, after.result),
        (ExecuteStatus::Aborted, Some("3".to_string()))
    );
}
