//! The process panic hook's accounting, in this process.

use super::*;
use pa_core::session::hosted::HOSTED_PID_BASE;

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
    let logged: Vec<SessionCrash> = pa_core::agent_state::crash_records(&log)
        .unwrap()
        .into_iter()
        .map(|record| serde_json::from_value(record).unwrap())
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

/// A hosted lifetime registered as running, and the scope its threads carry.
fn lifetime(pid: u32) -> Arc<SessionScope> {
    let control = Arc::new(HostControl {
        pid,
        end: tokio::sync::Notify::new(),
        crash: Mutex::new(None),
    });
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(pid, Arc::clone(&control));
    hosted::register(pid, format!("hosted:test:{pid}"));
    let scope = SessionScope::new(
        format!("lifetime-{pid}"),
        std::env::temp_dir(),
        BTreeMap::new(),
        BTreeSet::new(),
    );
    scope.set_slot(control);
    scope.set_slot(Arc::new(hosted::HostedId(pid)));
    scope
}

/// A session is ended while a thread of it still holds its recovery journal
/// (a stuck call), and rebuilt. The rebuilt lifetime takes the journal
/// over; when the old thread wakes, its write is refused and the journal
/// holds only what the old lifetime wrote before it ended and what the new
/// one wrote since. Once the host has ended the old scope, every durable
/// write from its threads is refused.
#[test]
fn a_thread_of_an_ended_lifetime_cannot_write_the_journal_its_successor_took_over() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker.recovery.jsonl");
    let (old_pid, new_pid) = (HOSTED_PID_BASE + 0x3fff_0001, HOSTED_PID_BASE + 0x3fff_0002);
    let on = |scope: &Arc<SessionScope>, work: Box<dyn FnOnce() -> String + Send>| {
        let scope = Arc::clone(scope);
        std::thread::spawn(move || {
            let _scope = session_scope::enter(Some(scope));
            work()
        })
        .join()
        .unwrap()
    };
    let old = lifetime(old_pid);
    let stale = Arc::new(Mutex::new(None));
    let (journal_path, kept) = (path.clone(), Arc::clone(&stale));
    on(
        &old,
        Box::new(move || {
            let mut journal = crate::journal::WorkerRecoveryJournal::open(&journal_path).unwrap();
            journal
                .record("active", "session", None, true, "prompt")
                .unwrap();
            // The thread that holds it is stuck from here on.
            *kept.lock().unwrap() = Some(journal);
            String::new()
        }),
    );
    // The host gives the old lifetime up; the session is rebuilt.
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&old_pid);
    hosted::unregister(old_pid);
    let journal_path = path.clone();
    on(
        &lifetime(new_pid),
        Box::new(move || {
            let mut journal = crate::journal::WorkerRecoveryJournal::open(&journal_path).unwrap();
            journal
                .record("active", "session", None, false, "idle")
                .unwrap();
            String::new()
        }),
    );
    // The stuck thread wakes and writes.
    let woken = Arc::clone(&stale);
    let refused = on(
        &old,
        Box::new(move || {
            let mut journal = woken.lock().unwrap().take().unwrap();
            journal
                .record("active", "session", None, true, "late")
                .map_or_else(|error| format!("{error:#}"), |()| "written".to_string())
        }),
    );
    old.end();
    let after_end = on(
        &old,
        Box::new(|| {
            session_scope::ensure_live()
                .map_or_else(|error| error.to_string(), |()| "live".to_string())
        }),
    );
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&new_pid);
    hosted::unregister(new_pid);
    let operations: Vec<String> = pa_core::session::store::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["operation"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(
        (
            refused.contains("belongs to a later lifetime of its session"),
            after_end,
            operations,
        ),
        (
            true,
            format!(
                "session lifetime lifetime-{old_pid} has ended: its remaining threads do not write"
            ),
            vec!["idle".to_string()],
        )
    );
}
