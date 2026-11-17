// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]

//! Verifier integration test for the bootstrap's background default-package imports: once the
//! kernel is ready, the default packages land in `sys.modules` without any cell importing them,
//! and none of that work reaches a cell's output. The kernel Python is ambient; skipped if absent.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pa_core::kernel::bootstrap::default_rlm_extra_import_names;
use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::ExecuteOptions;

/// The kernel Python with prime-agent-runtime and the default packages installed (see
/// `kernel_lifecycle.rs`); skipped with a note when absent.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_CORE_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string(),
        |home| format!("{home}/.prime/agent/kernel-venv/bin/python"),
    ));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live background-imports test",
        candidate.display()
    );
    None
}

/// Once the kernel is ready and idle, the background thread imports every default package (its
/// finish line in the kernel stderr log is the observable readiness; no cell is in flight while it
/// runs). A later cell that imports nothing sees them all in `sys.modules`, no background output
/// reached it, and the log names no skipped or failed import.
#[tokio::test]
async fn ready_kernel_imports_the_default_packages_in_the_background() {
    let Some(python) = kernel_python() else {
        return;
    };
    let provisioner = IpythonKernelProvisioner::new(
        "/tmp",
        IpythonKernelProvisionerOptions {
            python: Some(python),
            ..Default::default()
        },
    );
    provisioner.ensure(None, None).await.expect("kernel boots");
    let manager = provisioner.manager().expect("a running kernel");
    let deadline = Instant::now() + Duration::from_mins(2);
    while !manager
        .kernel_stderr()
        .contains("rlm.repl: background imports finished")
    {
        assert!(
            Instant::now() < deadline,
            "the background imports never finished: {}",
            manager.kernel_stderr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let names = serde_json::to_string(&default_rlm_extra_import_names()).expect("names json");
    let result = manager
        .execute(
            &format!("import sys\nprint([name for name in {names} if name not in sys.modules])"),
            ExecuteOptions::default(),
        )
        .await
        .expect("the probe cell runs");
    let import_problems: Vec<String> = manager
        .kernel_stderr()
        .lines()
        .filter(|line| line.contains("rlm.repl: background import "))
        .map(str::to_string)
        .collect();
    assert_eq!(
        (
            result.stdout.as_str(),
            result.stderr.as_str(),
            result.background_output,
            import_problems
        ),
        ("[]\n", "", None, Vec::<String>::new())
    );
    provisioner.dispose(None).await;
}
