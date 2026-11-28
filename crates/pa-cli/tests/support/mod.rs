//! Shared by the pa-cli e2e targets that run against a real `prime-agent`
//! daemon: the wait for it to listen, and the credential scrub of the ones
//! that run the chat surface in-process.

#[cfg(unix)]
pub mod runtime_daemon;

#[cfg(unix)]
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; only the launch's tmux verifier stops a legacy supervisor"
)]
pub mod legacy_supervisor;

/// Env keys a test's own environment may carry that must not reach a runtime daemon it starts:
/// the old worker's role and leases, the daemon selection, a package dir pointing outside the
/// fixture, and a sessions dir (fleet shells export one) that would put the daemon's default
/// sessions dir, and the subagent ledger resolved against it, outside the agent dir. Outside the
/// unix-only [`runtime_daemon`] module: the Windows suites start a runtime daemon too.
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; not every one starts a runtime daemon"
)]
pub const RUNTIME_DAEMON_SCRUB_ENV: [&str; 7] = [
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER",
    "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET",
    "PRIME_AGENT_INTERNAL_SESSION_LEASES",
    "PRIME_AGENT_INTERNAL_SESSION_LEASE_OWNER_ID",
    "PI_PACKAGE_DIR",
    "PRIME_AGENT_SESSION_DIR",
    "PRIME_AGENT_CODING_AGENT_SESSION_DIR",
];

/// The fixture transcript `text` (JSONL) as a session store holds one: each row after the header
/// that names no parent is linked to the row before it, and each assistant message is completed
/// with the members the agent's message type requires (`api`, `provider`, `model`, `usage`,
/// `stopReason`) where it lacks them. The runtime daemon serves a session's rows from the store,
/// walked back from the engine's leaf, so a fixture must be one the engine reads and keeps as it
/// would its own transcript.
///
/// # Panics
///
/// A line is not JSON.
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; not every one writes fixture transcripts"
)]
#[must_use]
pub fn valid_transcript(text: &str) -> String {
    let mut previous: Option<serde_json::Value> = None;
    let mut out = String::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut row: serde_json::Value = serde_json::from_str(line).expect("a JSON fixture row");
        if row["type"] != "session" {
            if row.get("parentId").is_none() {
                row["parentId"] = previous
                    .as_ref()
                    .filter(|previous| previous["type"] != "session")
                    .map_or(serde_json::Value::Null, |previous| previous["id"].clone());
            }
            let message = &mut row["message"];
            if message["role"] == "assistant" {
                let defaults = serde_json::json!({
                    "api": "faux",
                    "provider": "faux",
                    "model": "faux-1",
                    "usage": {
                        "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
                    },
                    "stopReason": "stop",
                });
                for (key, value) in defaults.as_object().expect("an object") {
                    if message.get(key).is_none() {
                        message[key] = value.clone();
                    }
                }
            }
        }
        out.push_str(&row.to_string());
        out.push('\n');
        previous = Some(row);
    }
    out
}

/// Strip ambient provider credentials from THIS process, once.
///
/// The in-process client forwards its whole environment to the session
/// worker as the create's `launch_env` (by design: a user's shell keys reach
/// the daemon), so stripping them from the spawned supervisor alone does not
/// keep the worker hermetic. A `GH_TOKEN` exported on the box made GitHub
/// Copilot a signed-in provider, and its models then ranked above the
/// fixtures' in the `/model` picker. Call it before the daemon spawns and
/// before the client runs.
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; not every one runs the client in-process"
)]
pub fn scrub_ambient_provider_credentials() {
    static SCRUB: std::sync::Once = std::sync::Once::new();
    SCRUB.call_once(|| {
        for provider in pa_ai::models_generated::get_providers() {
            if let Some(vars) = pa_ai::env_api_keys::get_api_key_env_vars(provider) {
                for var in vars {
                    std::env::remove_var(var);
                }
            }
        }
        std::env::remove_var("PRIME_TEAM_ID");
    });
}

/// The session transcripts the `prime-agent` runs of a test kept for the
/// agent dir `agent_dir`, by locator (`<sessions dir>/<id>.jsonl`), with
/// their bytes. Sessions live in the agent dir's database, not in files:
/// this asks `prime-agent export-jsonl --all` for them, so it also checks
/// what that command writes.
///
/// # Panics
///
/// The export failed.
#[allow(
    dead_code,
    reason = "each test target compiles this module for itself, and not every one reads the sessions it ran"
)]
pub fn saved_transcripts(
    agent_dir: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let out = tempfile::tempdir().expect("export dir");
    let mut export = std::process::Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    export
        .args(["export-jsonl", "--all", "--out"])
        .arg(out.path())
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("DO_NOT_TRACK", "1");
    // A process the test did not start with this process's environment (a
    // tmux pane's, which the tmux server gives its own) keeps the database in
    // the agent dir itself: read that one.
    if database_of(agent_dir).as_deref() == Some(agent_dir.join("agent.db").as_path()) {
        export.env_remove(pa_core::session::store::DATABASE_DIR_ENV);
    }
    let output = export.output().expect("run export-jsonl");
    assert!(
        output.status.success(),
        "export-jsonl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let agent_dir =
        pa_types::platform::canonicalize(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
    let mut transcripts = std::collections::BTreeMap::new();
    let mut dirs = vec![out.path().to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read the export").flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let relative = path.strip_prefix(out.path()).expect("under the export");
            let locator = match relative.strip_prefix("other") {
                Ok(absolute) => outside_root(absolute),
                Err(_) => agent_dir.join(relative),
            };
            transcripts.insert(
                locator,
                std::fs::read(&path).expect("read an exported session"),
            );
        }
    }
    transcripts
}

/// A locator outside the agent dir from its `other/` export path: the root
/// (on Windows, the drive export-jsonl keeps as a directory of its letter)
/// joined with the rest.
fn outside_root(absolute: &std::path::Path) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let mut parts = absolute.components();
        if let Some(drive) = parts.next() {
            return std::path::PathBuf::from(format!("{}:\\", drive.as_os_str().to_string_lossy()))
                .join(parts.as_path());
        }
    }
    std::path::Path::new("/").join(absolute)
}

/// The saved sessions' transcripts in the sessions directory `dir`, by
/// locator ([`saved_transcripts`] of `agent_dir`, filtered to `dir`).
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; not every one calls saved_transcripts_in"
)]
pub fn saved_transcripts_in(
    agent_dir: &std::path::Path,
    dir: &std::path::Path,
) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let canonical = pa_types::platform::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    // Each locator spelled under `dir` as the caller spells it (the spelling
    // the product reports, e.g. a Windows short name), not the export's
    // canonical one.
    saved_transcripts(agent_dir)
        .into_iter()
        .filter(|(locator, _)| locator.parent() == Some(canonical.as_path()))
        .map(|(locator, bytes)| (dir.join(locator.file_name().unwrap_or_default()), bytes))
        .collect()
}

/// The transcript database a product process keeps for `agent_dir`: the
/// test seam's (`pa_core::session::store::database_path`, the one this
/// process's environment selects), or `<agent dir>/agent.db` (a process
/// started without the seam); none before any process opened one.
fn database_of(agent_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    [
        pa_core::session::store::database_path(agent_dir),
        agent_dir.join("agent.db"),
    ]
    .into_iter()
    .find(|database| database.is_file())
}

/// The agent dir whose database holds the transcripts under `path`: the
/// nearest ancestor whose database exists
/// (`pa_core::session::store::database_path`), or the one a home or test
/// root above it keeps (`.prime/agent`, `agent`).
fn agent_dir_of(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .flat_map(|ancestor| {
            [
                ancestor.to_path_buf(),
                ancestor.join(".prime/agent"),
                ancestor.join("agent"),
            ]
        })
        .find(|dir| database_of(dir).is_some())
}

/// The saved sessions in the directory `dir` (a sessions dir, or a child
/// session's dir under the agent dir), by locator, as text. Before any run
/// opened the agent dir's database, the session files there as they are.
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; not every one calls session_texts"
)]
pub fn session_texts(dir: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    if let Some(agent_dir) = agent_dir_of(dir) {
        return saved_transcripts_in(&agent_dir, dir)
            .into_iter()
            .map(|(locator, bytes)| (locator, String::from_utf8_lossy(&bytes).into_owned()))
            .collect();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(std::path::PathBuf, String)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| Some((path.clone(), std::fs::read_to_string(&path).ok()?)))
        .collect();
    files.sort();
    files
}

/// The saved session at `locator` as text, from the database of the agent
/// dir above it (or the file itself before any run opened that database).
///
/// # Panics
///
/// No saved session is at `locator`.
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; not every one calls saved_text"
)]
pub fn saved_text(locator: &std::path::Path) -> String {
    match agent_dir_of(locator) {
        Some(agent_dir) => saved_transcript(&agent_dir, locator),
        None => std::fs::read_to_string(locator)
            .unwrap_or_else(|error| panic!("no saved session at {}: {error}", locator.display())),
    }
}

/// The saved transcript at `locator` as text ([`saved_transcripts`] of
/// `agent_dir`).
///
/// # Panics
///
/// No saved session is at `locator`, or it is not UTF-8.
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; not every one calls saved_transcript"
)]
pub fn saved_transcript(
    agent_dir: &std::path::Path,
    locator: impl AsRef<std::path::Path>,
) -> String {
    let locator = locator.as_ref();
    let canonical = locator
        .parent()
        .and_then(|dir| pa_types::platform::canonicalize(dir).ok())
        .map_or_else(
            || locator.to_path_buf(),
            |dir| dir.join(locator.file_name().unwrap_or_default()),
        );
    let mut transcripts = saved_transcripts(agent_dir);
    let bytes = transcripts.remove(&canonical).unwrap_or_else(|| {
        panic!(
            "no saved session at {}: {:?}",
            locator.display(),
            transcripts.keys()
        )
    });
    String::from_utf8(bytes).expect("a UTF-8 transcript")
}

/// Wait until the daemon `child` accepts connections on `socket` (the twin
/// of pa-daemon's `tests/common` helper).
///
/// The socket's file is not readiness: it appears at `bind`, before `listen`,
/// and a connect in between is refused. This connects until one is accepted;
/// the probe connection is dropped at once.
///
/// # Panics
///
/// `child` exited first, or 15 seconds passed.
#[allow(
    dead_code,
    reason = "each test target compiles this module itself; not every one waits for a daemon through it"
)]
#[cfg(unix)]
pub fn await_listening(socket: &std::path::Path, child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "the daemon exited before it listened on {}: {status}",
                socket.display()
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never listened on {}",
            socket.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Stop the runtime daemon a test's product started in `agent_dir`, if one runs: its restart
/// owner (or, unkept, the daemon) named in the agent dir's runtime-daemon lock is sent the
/// deliberate stop, then the test waits for the lock's holder to be gone (bounded): the cleanup
/// of a test that stops its daemons without the product's `shutdown`.
#[allow(dead_code, reason = "each test target compiles this module itself")]
#[cfg(unix)]
pub fn stop_runtime_daemon(agent_dir: &std::path::Path) {
    let lock = agent_dir.join("runtime-daemon.lock");
    let Some(holder) = std::fs::read_to_string(lock)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return;
    };
    let pids: Vec<u64> = [&holder["keeper"], &holder["pid"]]
        .into_iter()
        .filter_map(serde_json::Value::as_u64)
        .collect();
    let Some(first) = pids.first() else {
        return;
    };
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &first.to_string()])
        .status();
    let alive = |pid: &u64| std::path::Path::new(&format!("/proc/{pid}")).exists();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && pids.iter().any(alive) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
