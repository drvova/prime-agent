//! Shared by the pa-cli e2e targets that run against a real `prime-agent`
//! daemon: the wait for it to listen, and the credential scrub of the ones
//! that run the chat surface in-process.

/// Strip ambient provider credentials from THIS process, once.
///
/// The in-process client forwards its whole environment to the session
/// worker as the create's `launch_env` (by design: a user's shell keys reach
/// the daemon), so stripping them from the spawned supervisor alone does not
/// keep the worker hermetic. A `GH_TOKEN` exported on the box made GitHub
/// Copilot a signed-in provider, and its models then ranked above the
/// fixtures' in the `/model` picker. Call it before the daemon spawns and
/// before the client runs.
// Each test target compiles this module for itself, and not every one runs
// the client in-process.
#[allow(dead_code)]
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
// Each test target compiles this module for itself, and not every one reads
// the sessions it ran.
#[allow(dead_code)]
pub fn saved_transcripts(
    agent_dir: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let out = tempfile::tempdir().expect("export dir");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .args(["export-jsonl", "--all", "--out"])
        .arg(out.path())
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("DO_NOT_TRACK", "1")
        .output()
        .expect("run export-jsonl");
    assert!(
        output.status.success(),
        "export-jsonl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let agent_dir = std::fs::canonicalize(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
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
                Ok(absolute) => std::path::Path::new("/").join(absolute),
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

/// The saved sessions' transcripts in the sessions directory `dir`, by
/// locator ([`saved_transcripts`] of `agent_dir`, filtered to `dir`).
#[allow(dead_code)]
pub fn saved_transcripts_in(
    agent_dir: &std::path::Path,
    dir: &std::path::Path,
) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    saved_transcripts(agent_dir)
        .into_iter()
        .filter(|(locator, _)| locator.parent() == Some(dir.as_path()))
        .collect()
}

/// The agent dir whose database holds the transcripts under `path`: the
/// nearest ancestor with an `agent.db`, or the one a home or test root above
/// it keeps (`.prime/agent`, `agent`).
fn agent_dir_of(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .flat_map(|ancestor| {
            [
                ancestor.to_path_buf(),
                ancestor.join(".prime/agent"),
                ancestor.join("agent"),
            ]
        })
        .find(|dir| dir.join("agent.db").is_file())
}

/// The saved sessions in the directory `dir` (a sessions dir, or a child
/// session's dir under the agent dir), by locator, as text. Before any run
/// opened the agent dir's database, the session files there as they are.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub fn saved_transcript(
    agent_dir: &std::path::Path,
    locator: impl AsRef<std::path::Path>,
) -> String {
    let locator = locator.as_ref();
    let canonical = locator
        .parent()
        .and_then(|dir| std::fs::canonicalize(dir).ok())
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
// Each test target compiles this module for itself, and not every one waits
// for a daemon through it.
#[allow(dead_code)]
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
