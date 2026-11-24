//! Elevated commands on Linux through a session's kernel, with `sudo`
//! replaced by a stand-in ([`super::RUNNER_VARIABLE`]) that asks the askpass
//! program the session found, as `sudo -A` does, and starts root's script
//! in a session of its own, beyond the reach of the job's process group, as
//! root's processes are beyond the user's signals.

use std::path::Path;

use serde_json::{json, Value};

use super::super::tests::{exited, kernel_with, next, pipe_in, read, recording, value};
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::platform::KernelShell;
use crate::tools::kernel_cell::KernelExecutor;

/// Write an executable script.
fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A kernel whose session finds the stand-in `sudo` and, when `graphical`,
/// a graphical session with the stub `ssh-askpass` on its `PATH`, which
/// records its prompt and answers with the `answer` file. Its
/// `elevation.fact` counts arrive on the returned channel.
async fn elevating_kernel(
    dir: &Path,
    graphical: bool,
) -> (
    SchemeKernelProvisioner,
    Box<dyn KernelExecutor>,
    tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
) {
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    script(
        &bin.join("sudo"),
        "#!/bin/sh\n\
         case \"$1\" in\n\
         -A) prompt=$3; shift 4\n\
             answer=$(\"$SUDO_ASKPASS\" \"$prompt\") || { echo 'sudo: no password was provided' >&2; exit 1; }\n\
             [ \"$answer\" = secret ] || { echo 'sudo: 1 incorrect password attempt' >&2; exit 1; } ;;\n\
         -n) echo 'sudo: a password is required' >&2; exit 1 ;;\n\
         esac\n\
         exec setsid -w \"$@\"\n",
    );
    script(
        &bin.join("ssh-askpass"),
        &format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > '{0}/prompt'\nexec cat '{0}/answer' 2>/dev/null\n",
            dir.display()
        ),
    );
    let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
        &std::env::var("PATH").unwrap_or_default(),
    )))
    .unwrap()
    .into_string()
    .unwrap();
    let mut env = vec![
        ("PATH".to_string(), path),
        (
            super::RUNNER_VARIABLE.to_string(),
            bin.join("sudo").display().to_string(),
        ),
        // Nothing of the test process's own desktop or askpass counts.
        ("SUDO_ASKPASS".to_string(), String::new()),
        ("WAYLAND_DISPLAY".to_string(), String::new()),
        ("DISPLAY".to_string(), String::new()),
    ];
    if graphical {
        env.push(("DISPLAY".to_string(), ":0".to_string()));
    }
    let (handlers, received) = recording(&[("elevation.fact", json!({}))]);
    let (provisioner, kernel) = kernel_with(SchemeKernelOptions {
        shell: KernelShell::BuiltIn,
        host_handlers: handlers,
        cwd: dir.to_path_buf(),
        env,
        ..SchemeKernelOptions::default()
    })
    .await;
    (provisioner, kernel, received)
}

/// A consented elevated command returns what a command returns: its exit
/// code and its own output, stdout and stderr together, run in the session's
/// working directory; the prompt names the command, and the command counts
/// as approved through the askpass route.
#[tokio::test(flavor = "multi_thread")]
async fn an_elevated_command_runs_after_consent_with_its_output_captured() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("answer"), "secret").unwrap();
    let (_provisioner, kernel, mut counted) = elevating_kernel(dir.path(), true).await;
    let ran = value(
        kernel.as_ref(),
        r#"(let ([r (bash "echo out; echo err >&2; pwd; exit 3" (hash 'elevated #t))])
             (list (hash-ref r 'exit-code) (hash-ref r 'output) (hash-ref r 'running)))"#,
    )
    .await;
    assert_eq!(
        (
            ran,
            std::fs::read_to_string(dir.path().join("prompt")).unwrap(),
            next(&mut counted).await,
        ),
        (
            format!("(3 \"out\\nerr\\n{}\\n\" #false)", dir.path().display()),
            "Prime Agent asks to run as root: echo out; echo err >&2; pwd; exit 3. Password for %p: "
                .to_string(),
            (
                "elevation.fact".to_string(),
                json!({"outcome": "approved", "route": "sudo_askpass"})
            ),
        )
    );
}

/// A dismissed password dialog and a session with no way to ask (no
/// graphical session, and `sudo` needs a password) each return the
/// documented result, not an error, and are counted; asking for an elevated
/// persistent shell is refused before anything runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_declined_or_unavailable_elevation_returns_its_error_result() {
    let cell = r#"(let ([r (bash "touch ran" (hash 'elevated #t))])
                    (list (hash-ref r 'exit-code) (hash-ref r 'output)))"#;
    let mut seen = Vec::new();
    for graphical in [true, false] {
        // No answer file: the askpass program fails, as a dismissed dialog does.
        let dir = tempfile::tempdir().unwrap();
        let (_provisioner, kernel, mut counted) = elevating_kernel(dir.path(), graphical).await;
        seen.push((
            value(kernel.as_ref(), cell).await,
            next(&mut counted).await.1,
            dir.path().join("ran").exists(),
        ));
        if !graphical {
            seen.push((
                value(
                    kernel.as_ref(),
                    r#"(bash "true" (hash 'elevated #t 'persistent #t))"#,
                )
                .await,
                Value::Null,
                false,
            ));
        }
    }
    assert_eq!(
        seen,
        [
            (
                "(126 \"elevation declined: the request to run this command as root was refused or dismissed, so it did not run. Ask the user before trying again.\\nsudo: no password was provided\\n\")".to_string(),
                json!({"outcome": "declined", "route": "sudo_askpass"}),
                false,
            ),
            (
                "(126 \"elevation unavailable: no password dialog can be shown here (no graphical session with an askpass program or a polkit agent) and sudo needs a password, so the command did not run. Give the user the exact command to run in their own terminal with sudo.\\nsudo: a password is required\\n\")".to_string(),
                json!({"outcome": "unavailable", "route": "sudo_noninteractive"}),
                false,
            ),
            (
                "Error: Generic: bash: a persistent shell cannot be elevated: run each elevated command on its own"
                    .to_string(),
                Value::Null,
                false,
            ),
        ]
    );
}

/// Killing an elevated job ends what its command started, though the
/// command runs where the job's process-group signal does not reach: its
/// lifeline closes, and root's side ends the command's group.
#[tokio::test(flavor = "multi_thread")]
async fn killing_an_elevated_job_ends_what_its_command_started() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("answer"), "secret").unwrap();
    let started = pipe_in(dir.path(), "started");
    let (_provisioner, kernel, _counted) = elevating_kernel(dir.path(), true).await;
    let begun = value(
        kernel.as_ref(),
        r#"(define job (bash "/bin/sleep 600 & echo $! > started; wait" (hash 'elevated #t 'background #t)))
           (hash-ref job 'running)"#,
    )
    .await;
    let sleeper: u32 = read(&started).await.trim().parse().unwrap();
    let killed = value(kernel.as_ref(), "(bash-kill job)").await;
    assert_eq!(
        (begun, killed, exited(sleeper).await),
        ("#true".to_string(), "#true".to_string(), true)
    );
}
