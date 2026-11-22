//! The `rz-*` functions against the `rizin` on this machine's `PATH`. Each
//! test skips, saying so, when there is none: `cargo test` reaches them
//! everywhere and they prove something only where rizin is installed.

use super::*;

/// A kernel over the installed rizin with a saved session's spill store,
/// or `None` (after saying why) when no rizin is installed.
async fn installed(
    test: &str,
) -> Option<(
    SchemeKernelProvisioner,
    Box<dyn KernelExecutor>,
    tempfile::TempDir,
)> {
    if let Err(error) = pa_types::session_scope::std_command("rizin")
        .arg("-v")
        .output()
    {
        println!("{test} skipped: no rizin on PATH ({error})");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let provisioner = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        artifact_dir: Some(dir.path().to_path_buf()),
        ..SchemeKernelOptions::default()
    });
    let kernel = provisioner.ensure(None, None).await.unwrap();
    Some((provisioner, kernel, dir))
}

/// The installed rizin loads a binary and answers over the pipe: text with
/// a NUL in it, JSON, `RzIL`. A reply of many megabytes comes back cut, and a
/// rizin that is killed under a command is reported with its exit status.
#[tokio::test(flavor = "multi_thread")]
async fn the_installed_rizin_answers_over_its_pipe_and_its_death_is_reported() {
    let Some((_provisioner, kernel, _dir)) = installed("the pipe protocol").await else {
        return;
    };
    let returned = values(
        kernel.as_ref(),
        &[
            r#"(begin (define s (rz-open "/bin/sh"))
                      (list (hash-ref s 'alive) (hash-ref s 'debug) (string? (hash-ref s 'version))))"#,
            r#"(rz-cmd s "pr 8 @ 0")"#,
            r#"(hash-ref (hash-ref (rz-cmdj s "ij") 'core) 'file)"#,
            r#"(starts-with? (rz-cmd s "aoi 1 @ entry0") "0x")"#,
            r#"(let* ([text (rz-cmd s "px 4000000 @ 0")]
                      [marks (filter (lambda (line) (starts-with? line "[")) (split-many text "\n"))])
                 (list (< (string-length text) 800000)
                       (starts-with? text "- offset -")
                       (map (lambda (mark) (substring mark 0 5)) marks)))"#,
            r#"(rz-cmd s (string-append "!kill -KILL " (number->string (hash-ref s 'pid))))"#,
            "(hash-ref (car (rz-sessions)) 'alive)",
        ],
    )
    .await;
    assert_eq!(
        returned,
        vec![
            Ok(json!([true, false, true])),
            Ok(json!("\u{7f}ELF\u{2}\u{1}\u{1}\u{0}\n")),
            Ok(json!("/bin/sh")),
            Ok(json!(true)),
            Ok(json!([true, true, ["[out:", "[rizi"]])),
            Err(
                "Error: Generic: rz-cmd: the session is dead: rizin ended (signal: 9 (SIGKILL))"
                    .to_string()
            ),
            Ok(json!(false)),
        ]
    );
}

/// The state letter and the parent of process `pid`, from `/proc`: `None`
/// once it is reaped.
#[cfg(target_os = "linux")]
fn process(pid: u64) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.rsplit_once(") ")?.1.split(' ');
    Some((fields.next()?.chars().next()?, fields.next()?.parse().ok()?))
}

/// A shell command and a debugged program that never return are
/// interrupted by a limit and by a stopped cell, the sessions stay usable,
/// and the kernel's end kills each rizin and the program it debugs.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_running_command_and_a_debuggee_are_interrupted_and_die_with_the_kernel() {
    let Some((provisioner, kernel, _dir)) = installed("interrupts and teardown").await else {
        return;
    };
    // A program that runs without forking, started with one plain
    // argument: every supported rizin passes that through unchanged.
    let Some(sleep) = ["/bin/sleep", "/usr/bin/sleep"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        println!("interrupts and teardown skipped: no sleep program to debug");
        return;
    };
    let opened = values(
        kernel.as_ref(),
        &[&format!(
            r#"(begin (define s (rz-open "/bin/sh"))
                      (define d (rz-open "{sleep}" #:debug #t #:args (list "100")))
                      (list (hash-ref s 'pid) (hash-ref d 'pid) (hash-ref (car (rz-cmdj d "dpj")) 'pid)))"#
        )],
    )
    .await
    .remove(0);
    let pids: Vec<u64> = serde_json::from_value(opened.unwrap()).unwrap();
    let (shell, debuggee) = (pids[0], pids[2]);
    let limited_at = Instant::now();
    let limited = values(
        kernel.as_ref(),
        &[r#"(rz-cmd s "!sleep 100" #:timeout-ms 300)"#],
    )
    .await;
    let limited_took = limited_at.elapsed();
    // Stopped once rizin has started the command's program.
    let (in_command, command_stop_took) = stopped_when(
        kernel.as_ref(),
        r#"(rz-cmd s "!sleep 100" #:timeout-ms #f)"#,
        async {
            while !std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .parse()
                    .ok()
                    .and_then(process)
                    .is_some_and(|(_, parent)| parent == shell)
            }) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        },
    )
    .await;
    // Stopped once the debugged program has left its tracing stop: it runs.
    let (in_debuggee, debuggee_stop_took) = stopped_when(
        kernel.as_ref(),
        r#"(rz-cmd d "dc" #:timeout-ms #f)"#,
        async {
            while process(debuggee).is_none_or(|(state, _)| state == 't') {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        },
    )
    .await;
    let usable = values(
        kernel.as_ref(),
        &[
            r#"(rz-cmd s "echo alive")"#,
            r#"(hash? (rz-cmdj d "drj"))"#,
            r"(map (lambda (session) (hash-ref session 'alive)) (rz-sessions))",
        ],
    )
    .await;
    assert_eq!(
        (limited, in_command, in_debuggee, usable),
        (
            vec![Err("Error: Generic: rz-cmd: no reply within 300 ms: the command was interrupted and the session is still usable".to_string())],
            ExecuteStatus::Aborted,
            ExecuteStatus::Aborted,
            vec![
                Ok(json!("alive\n")),
                Ok(json!(true)),
                Ok(json!([true, true])),
            ],
        )
    );
    let ending = Instant::now();
    provisioner.kill().await;
    tokio::time::timeout(PATIENCE, async {
        // A zombie has ended: only its parent's wait is missing.
        while !pids
            .iter()
            .all(|pid| process(*pid).is_none_or(|(state, _)| state == 'Z'))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("both rizins and the debugged program are gone");
    println!(
        "limit: interrupted {:?} after its 300 ms; a cell stopped in a shell command left in \
         {command_stop_took:?}, in a running debuggee in {debuggee_stop_took:?}; everything \
         gone {:?} after the kill",
        limited_took.saturating_sub(Duration::from_millis(300)),
        ending.elapsed(),
    );
    // Interrupted, not killed after the grace.
    assert!(
        [limited_took, command_stop_took, debuggee_stop_took]
            .iter()
            .all(|took| *took < INTERRUPT_GRACE),
        "limit {limited_took:?}, stops {command_stop_took:?} and {debuggee_stop_took:?}"
    );
}
