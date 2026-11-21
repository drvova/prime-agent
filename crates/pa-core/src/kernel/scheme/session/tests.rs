//! The session-facing reads of a real kernel: its liveness, its background
//! work, and what a release loses.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::kernel::cell_window::{CellWindow, CellWindowConfig};
use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::scheme::{KernelStart, SchemeKernelOptions};
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::tools::ipython::IpythonKernelProvisioner as _;
use crate::tools::ipython::{ExecuteResult, ExecuteStatus, KernelExecuteOptions};

/// How long a test waits for something that must happen before it fails.
const PATIENCE: Duration = Duration::from_secs(30);

async fn run(provisioner: &SchemeKernelProvisioner, code: &str) -> ExecuteResult {
    provisioner
        .ensure(None, None)
        .await
        .unwrap()
        .execute(
            code,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        )
        .await
        .unwrap()
}

/// A released kernel takes its cells' definitions with it and says so;
/// the next cell starts a new one. Releasing a kernel whose cells
/// defined nothing tells no one.
#[tokio::test(flavor = "multi_thread")]
async fn a_released_kernel_starts_clean_and_reports_the_definitions_it_lost() {
    let lost = Arc::new(AtomicUsize::new(0));
    let starts = Arc::new(Mutex::new(Vec::new()));
    let (counted, noted) = (Arc::clone(&lost), Arc::clone(&starts));
    let provisioner = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        cwd: "/".into(),
        on_state_lost: Some(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })),
        on_started: Some(Arc::new(move |start: KernelStart| {
            noted.lock().unwrap().push(start.succeeded);
        })),
        ..SchemeKernelOptions::default()
    });
    let before_any_cell = (
        provisioner.has_running_kernel(),
        provisioner.defined_names(),
    );
    run(&provisioner, "(define kept 7) (define (twice n) (* 2 n))").await;
    let defined = provisioner.defined_names();
    provisioner.release().await;
    let released = provisioner.has_running_kernel();
    let after = run(&provisioner, "kept").await.status;
    let redefined = provisioner.defined_names();
    provisioner.release().await;
    assert_eq!(
        (
            before_any_cell,
            defined,
            released,
            after,
            redefined,
            lost.load(Ordering::SeqCst),
            starts.lock().unwrap().clone(),
        ),
        (
            (false, None),
            Some(vec!["kept".to_string(), "twice".to_string()]),
            false,
            ExecuteStatus::Error,
            Some(Vec::new()),
            1,
            vec![true, true],
        )
    );
}

/// A cell that outlived its call is live background work, and holds the
/// engine, until its completion notice has been handed over; then the
/// settle callback fires.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_cell_is_live_work_until_its_notice_is_handed_over() {
    let dir = tempfile::tempdir().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let (settled, mut settlements) = tokio::sync::mpsc::unbounded_channel();
    let notices = Arc::new(AtomicUsize::new(0));
    let mut handlers = HostRequestHandlers::new();
    handlers.register("gate.wait", {
        let release = Arc::clone(&release);
        host_handler(move |_payload: HostRequestPayload| {
            let release = Arc::clone(&release);
            async move {
                release.notified().await;
                Ok(json!({}))
            }
        })
    });
    handlers.register("cell.completed", {
        let notices = Arc::clone(&notices);
        host_handler(move |_payload: HostRequestPayload| {
            notices.fetch_add(1, Ordering::SeqCst);
            async move { Ok(json!({})) }
        })
    });
    let seen = Arc::clone(&notices);
    let provisioner = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        host_handlers: handlers,
        cwd: dir.path().to_path_buf(),
        cell_window: CellWindow::for_session(CellWindowConfig {
            daemonless: false,
            configured_ms: Some(100),
            agent_dir: dir.path(),
            project_dir: dir.path(),
            on_armed: None,
        }),
        on_background_work_settled: Some(Arc::new(move || {
            // The notice was handed over before the work counted as settled.
            let _ = settled.send(seen.load(Ordering::SeqCst));
        })),
        ..SchemeKernelOptions::default()
    });
    let idle = provisioner.background_job_count();
    let long = run(&provisioner, r#"(host-request "gate.wait" (hash))"#).await;
    let while_running = (
        long.backgrounded_cell.is_some(),
        provisioner.background_job_count(),
        provisioner.has_background_work(),
        provisioner.defined_names(),
    );
    release.notify_one();
    let notices_at_settle = tokio::time::timeout(PATIENCE, settlements.recv())
        .await
        .unwrap();
    assert_eq!(
        (
            idle,
            while_running,
            notices_at_settle,
            provisioner.background_job_count()
        ),
        (0, (true, 1, true, None), Some(1), 0)
    );
}
