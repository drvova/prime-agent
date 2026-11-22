//! The process panic hook's accounting, in this process.

use super::*;

/// The crash each of `messages` left in `crashes`, without its time.
fn found(crashes: &[SessionCrash], message: &str) -> Option<(String, String, bool)> {
    crashes
        .iter()
        .find(|crash| crash.message == message)
        .map(|crash| {
            (
                crash.kind.clone(),
                crash.thread.clone(),
                crash.location.starts_with(file!()),
            )
        })
}

/// Every panic is someone's. One on a thread of no session is the daemon's:
/// kept, and appended to the crash log as kind `daemon`. One on a thread a
/// session started is that session's, wherever the thread came from. One
/// its caller catches and handles is nobody's fault.
#[test]
fn a_panic_is_the_sessions_or_the_daemons_and_a_caught_one_is_neither() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("session-crashes.jsonl");
    install_panic_hook(log.clone());
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let (daemon, session, caught) = (
        format!("daemon-{tag}"),
        format!("session-{tag}"),
        format!("caught-{tag}"),
    );

    let message = daemon.clone();
    let raised = std::thread::Builder::new()
        .name("control-plane".to_string())
        .spawn(move || panic!("{message}"))
        .unwrap()
        .join();
    assert!(raised.is_err());

    let control = Arc::new(HostControl {
        pid: HOSTED_PID_BASE,
        end: tokio::sync::Notify::new(),
        crash: Mutex::new(None),
    });
    let scope = SessionScope::new(
        "hosted".to_string(),
        dir.path().to_path_buf(),
        BTreeMap::new(),
        BTreeSet::new(),
    );
    scope.set_slot(Arc::clone(&control));
    let (message, handled) = (session.clone(), caught.clone());
    std::thread::spawn(move || {
        let _scope = session_scope::enter(Some(scope));
        // A thread the session starts for its own work.
        let raised = std::thread::Builder::new()
            .name("session-helper".to_string())
            .spawn(session_scope::propagate(move || panic!("{message}")))
            .unwrap()
            .join();
        assert!(raised.is_err());
    })
    .join()
    .unwrap();
    crashes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&HOSTED_PID_BASE);
    let caught_outcome =
        std::thread::spawn(move || session_scope::catching_panics(|| panic!("{handled}")).is_err())
            .join()
            .unwrap();

    let recorded = daemon_crashes();
    let logged: Vec<SessionCrash> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let attributed: Vec<SessionCrash> = control
        .crash
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .into_iter()
        .collect();
    let the_daemons = Some(("daemon".to_string(), "control-plane".to_string(), true));
    assert_eq!(
        (
            caught_outcome,
            [&daemon, &session, &caught].map(|message| found(&recorded, message)),
            [&daemon, &session, &caught].map(|message| found(&logged, message)),
            found(&attributed, &session),
        ),
        (
            true,
            [the_daemons.clone(), None, None],
            [the_daemons, None, None],
            Some(("panic".to_string(), "session-helper".to_string(), true)),
        )
    );
}
