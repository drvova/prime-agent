//! What ends a kernel's rizin sessions, and what an open says when it
//! cannot have one.

use super::*;

/// How a session's kernel can end while it holds rizin sessions.
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// `kill`: the session closed, or an idle one released its kernel.
    Killed,
    /// The hosted session's lifetime ended: it crashed, was stopped, or its
    /// daemon is shutting down.
    LifetimeEnded,
    /// The session was dropped without either.
    Dropped,
}

const ENDINGS: [Ending; 3] = [Ending::Killed, Ending::LifetimeEnded, Ending::Dropped];

/// Start `provisioner`'s kernel inside a session lifetime, run `cell` in
/// it, end the kernel as `ending` says, and return what the cell returned.
async fn ended_after(
    ending: Ending,
    provisioner: SchemeKernelProvisioner,
    cell: &str,
) -> Result<Value, String> {
    let scope = pa_types::session_scope::SessionScope::new(
        "disassembling".to_string(),
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

/// However the kernel ends, every rizin it started is killed.
#[tokio::test(flavor = "multi_thread")]
async fn every_way_a_kernel_ends_kills_its_rizins() {
    let mut left = Vec::new();
    for ending in ENDINGS {
        let fake = Arc::new(Fake::default());
        ended_after(
            ending,
            provisioner(SchemeKernelOptions::default(), &fake),
            r#"(begin (rz-open "/bin/a") (rz-open "/bin/b" #:debug #t) #t)"#,
        )
        .await
        .unwrap();
        left.push(fake.calls()[2..].to_vec());
    }
    assert_eq!(left, vec![["kill", "kill"].map(str::to_string).to_vec(); 3]);
}

/// A rizin that dies under a command is reported with how it ended and
/// what it last wrote, and one that cannot say its version is refused as
/// too old; neither is kept as a live session.
#[tokio::test(flavor = "multi_thread")]
async fn a_rizin_that_dies_or_is_too_old_is_reported_and_not_kept() {
    let fake = Arc::new(Fake::default());
    let provisioner = provisioner(SchemeKernelOptions::default(), &fake);
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let died = values(
        kernel.as_ref(),
        &[
            r#"(begin (define s (rz-open "/bin/app")) (rz-cmd s "die"))"#,
            r#"(rz-cmd s "ij")"#,
        ],
    )
    .await;
    provisioner
        .state
        .rizin
        .launch_with(Arc::new(FakeProgram(Arc::new(Fake {
            version: Some("ERROR: Command 'verj' does not exist.\n"),
            ..Fake::default()
        }))));
    let old = values(
        kernel.as_ref(),
        &[r#"(rz-open "/bin/app")"#, "(rz-sessions)"],
    )
    .await;
    let dead = "rizin ended (signal: 11 (SIGSEGV)); its last output:\nSegmentation fault";
    let mut first = opened(1, "/bin/app", false, false);
    first["alive"] = json!(false);
    first["reason"] = json!(dead);
    assert_eq!(
        (died, old),
        (
            vec![
                Err(format!("Error: Generic: rz-cmd: the session is dead: {dead}")),
                Err(format!("Error: Generic: rz-cmd: the session is dead: {dead}")),
            ],
            vec![
                Err("Error: Generic: rz-open: the installed rizin is older than 0.7.4, the oldest this can drive (`rizin -v` shows its version): upgrade it".to_string()),
                Ok(json!([first])),
            ],
        )
    );
}

/// Without a `rizin` on the session's `PATH`, an open says so and how to
/// install it, and the refusal is counted. This is the real program
/// lookup, in a hosted session whose `PATH` is an empty directory.
#[test]
fn without_rizin_on_the_sessions_path_an_open_says_how_to_install_it() {
    let empty = tempfile::tempdir().unwrap();
    let scope = pa_types::session_scope::SessionScope::new(
        "bare".to_string(),
        empty.path().to_path_buf(),
        std::collections::BTreeMap::from([(
            "PATH".to_string(),
            empty.path().to_string_lossy().into_owned(),
        )]),
        std::collections::BTreeSet::new(),
    );
    let cwd = empty.path().to_path_buf();
    // As a hosted session: its own thread and runtime, all in its scope.
    let (refused, noted) = std::thread::spawn(move || {
        let _scope = pa_types::session_scope::enter(Some(scope));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .on_thread_start(pa_types::session_scope::inherit())
            .build()
            .unwrap();
        runtime.block_on(async {
            let (handlers, facts) = counting();
            let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
                host_handlers: handlers,
                cwd,
                ..SchemeKernelOptions::default()
            })
            .ensure(None, None)
            .await
            .unwrap();
            let refused = values(
                kernel.as_ref(),
                &[r#"(rz-open "/bin/sh")"#, "(rz-sessions)"],
            )
            .await;
            (refused, counted(&facts, 1))
        })
    })
    .join()
    .unwrap();
    assert_eq!(
        (refused, noted),
        (
            vec![
                Err(format!(
                    "Error: Generic: rz-open: {}",
                    crate::kernel::scheme::rizin::NOT_INSTALLED
                )),
                Ok(json!([])),
            ],
            vec!["not_installed".to_string()],
        )
    );
}
