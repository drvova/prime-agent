//! A session's cells against Steel's own ways out of the kernel, through a
//! real session kernel.

use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::HostRequestHandlers;
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelProvisioner as _};

/// What `code` did as one cell: what it printed, or the error it raised.
async fn cell(provisioner: &SchemeKernelProvisioner, code: &str) -> Result<String, String> {
    let result = provisioner
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
        .unwrap();
    match result.error {
        None => Ok(result.stdout),
        Some(error) => Err(error.evalue),
    }
}

/// Every way Steel itself offers to read or write a file, list or change a
/// directory, start a process, load a file as code, read the environment, or
/// build an engine without these limits fails in a cell, and leaves no
/// trace; the kernel's own functions do the same work.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_reaches_files_and_processes_only_through_the_kernel_functions() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "the secret\n").unwrap();
    let code = dir.path().join("code.scm");
    std::fs::write(&code, "(define loaded-from-a-file 1)\n").unwrap();
    let made = dir.path().join("made");
    let (secret, code, made) = (
        secret.to_string_lossy().into_owned(),
        code.to_string_lossy().into_owned(),
        made.to_string_lossy().into_owned(),
    );
    let provisioner = SchemeKernelProvisioner::new(HostRequestHandlers::new(), dir.path());
    let before = std::env::current_dir().unwrap();
    let mut open = Vec::new();
    for attempt in [
        // Files and directories.
        format!("(read-port-to-string (open-input-file {secret:?}))"),
        format!("(#%prim.open-input-file {secret:?})"),
        format!("(call-with-input-file {secret:?} read-line)"),
        format!("(with-input-from-file {secret:?} read-line)"),
        format!("(open-output-file {made:?})"),
        format!("(call-with-output-file {made:?} (lambda (port) (write-string \"x\" port)))"),
        format!("(read-dir {:?})", dir.path()),
        format!("(path-exists? {secret:?})"),
        format!("(is-file? {secret:?})"),
        format!("(create-directory! {made:?})"),
        format!("(delete-file! {secret:?})"),
        format!("(begin (require-builtin steel/filesystem as fs.) (fs.read-dir {:?}))", dir.path()),
        format!("(begin (require-builtin steel/ports as ports.) (ports.open-input-file {secret:?}))"),
        "(current-directory)".to_string(),
        format!("(change-current-directory! {:?})", dir.path()),
        format!("(set-current-dir! (command \"true\" (list)) {:?})", dir.path()),
        // Processes.
        format!("(spawn-process (command \"touch\" (list {made:?})))"),
        format!("(#%prim.spawn-process (#%prim.command \"touch\" (list {made:?})))"),
        format!(
            "(begin (require-builtin steel/process as process.)
                    (process.spawn-process (process.command \"touch\" (list {made:?}))))"
        ),
        format!(
            "((%module-get% %-builtin-module-steel/process \"spawn-process\")
              ((%module-get% %-builtin-module-steel/process \"command\") \"touch\" (list {made:?})))"
        ),
        "(which \"sh\")".to_string(),
        format!("(begin (require-builtin steel/git as git.) (git.git-clone \"https://example.invalid/x\" {made:?}))"),
        "(begin (require-builtin steel/tcp as tcp.) (tcp.tcp-connect \"127.0.0.1:9\"))".to_string(),
        // Files as code, and engines without these limits.
        format!("(load {code:?})"),
        format!("(#%prim.load {code:?})"),
        format!("(require {code:?})"),
        format!("(eval '(require {code:?}))"),
        format!("(eval-string \"(load \\\"{code}\\\")\")"),
        format!("(run! (Engine::new) \"(spawn-process (command \\\"touch\\\" (list \\\"{made}\\\")))\")"),
        format!(
            "(run! ((%module-get% %-builtin-module-steel/meta \"Engine::new\")) \"(read-dir \\\"{}\\\")\")",
            dir.path().display()
        ),
        "((%module-get% %-builtin-module-steel/base \"Engine::new\"))".to_string(),
        // The process's environment and streams.
        "(env-var \"HOME\")".to_string(),
        "(maybe-get-env-var \"HOME\")".to_string(),
        "(set-env-var! \"PA_CONFINEMENT_TEST\" \"1\")".to_string(),
        "(write-string \"x\" (stdout))".to_string(),
        "(read-line (stdin))".to_string(),
    ] {
        if cell(&provisioner, &attempt).await.is_ok() {
            open.push(attempt);
        }
    }
    let defined = cell(&provisioner, "(display loaded-from-a-file)").await;
    let through_the_kernel = cell(
        &provisioner,
        "(fs-write \"made\" \"by the kernel\\n\")
         (display (list (hash-ref (bash \"cat secret.txt\") 'output)
                        (length (hash-ref (fs-read \"made\") 'lines))
                        (eval '(+ 1 2))
                        (with-output-to-string (lambda () (display \"captured\")))))",
    )
    .await;
    assert_eq!(
        (
            open,
            defined.is_err(),
            std::env::current_dir().unwrap(),
            std::env::var_os("PA_CONFINEMENT_TEST"),
            through_the_kernel,
        ),
        (
            Vec::new(),
            true,
            before,
            None,
            Ok("(the secret\n 1 3 captured)".to_string()),
        )
    );
}
