#![cfg_attr(
    unix,
    expect(
        clippy::too_many_lines,
        reason = "`interactive_fork_launch_copies_and_the_daemon_opens_the_fork` is one scenario, top to bottom"
    )
)]

//! End-to-end verifier for the interactive `--fork` launch: the fork copies
//! its source into a fresh session file client-side, the daemon opens the
//! copy — never the source — through the create `sessionPath`, and the TUI
//! renders the copied transcript and takes new turns on the fork. The source
//! stays hosted by the live runtime daemon for the whole run (TS parity: no
//! daemon-active guard on the fork arm).
#![cfg(unix)]

mod support;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg::F_SETFL, OFlag};
use nix::pty::{openpty, Winsize};
use serde_json::{json, Map, Value};

use pa_types::daemon::DaemonCommand;
use support::runtime_daemon::RuntimeDaemon;

/// The parent hands the child its launch through this env var; unset, the
/// test passes trivially (a plain `cargo test` runs only the parent).
const CHILD_ENV: &str = "PA_INTERACTIVE_FORK_CHILD";

/// The interactive client half: the same entry point the shipped binary
/// calls, over the pty the parent opened — the real fork startup. The exit
/// code lands in the outcome file for the parent to assert.
#[test]
fn interactive_fork_child_mode() {
    let Ok(config) = std::env::var(CHILD_ENV) else {
        return;
    };
    let config: Value = serde_json::from_str(&config).expect("child config");
    std::env::set_current_dir(config["cwd"].as_str().expect("cwd")).expect("child cwd");
    let args = vec![
        "--fork".to_string(),
        config["selector"].as_str().expect("selector").to_string(),
        "--daemon-socket".to_string(),
        config["socket"].as_str().expect("socket").to_string(),
    ];
    let code = pa_cli::main_with_runtime(&args, &pa_cli::PrintRuntime);
    std::fs::write(
        config["outcome"].as_str().expect("outcome path"),
        code.to_string(),
    )
    .expect("write the launch outcome");
}

/// Open the source session on the live daemon (a create with `sessionPath`
/// over the socket), so the fork runs against a session the daemon already
/// hosts. Returns the daemon's session id.
async fn make_session_active(socket: &Path, session_path: &Path, cwd: &Path) -> String {
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(socket)
        .await
        .expect("connect daemon");
    let data = client
        .request_ok(DaemonCommand::Create {
            id: None,
            session_path: Some(session_path.to_string_lossy().to_string()),
            continue_recent: None,
            no_session: None,
            name: None,
            config: Some(json!({
                "cwd": cwd.display().to_string(),
                "sessionDir": session_path.parent().expect("sessions dir").display().to_string(),
            })),
            telemetry_disabled: None,
            runtime_metadata: None,
            lifecycle: None,
            env: None,
            launch_env: None,
            rest: Map::default(),
        })
        .await
        .expect("open the source session");
    client.close();
    data.get("activeSessionId")
        .or_else(|| data.get("id"))
        .and_then(Value::as_str)
        .expect("session id")
        .to_string()
}

/// The live daemon roster rows: `sessionFile` (canonical) and
/// `activeSessionId` per hosted session.
async fn daemon_roster(socket: &Path) -> Vec<(PathBuf, String)> {
    let (client, _events) = pa_tui::daemon_client::DaemonClient::connect(socket)
        .await
        .expect("connect daemon");
    let list = client
        .request_ok(DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: None,
            rest: Map::default(),
        })
        .await
        .expect("list sessions");
    client.close();
    list.get("sessions")
        .and_then(Value::as_array)
        .map_or_default(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let file = row.get("sessionFile")?.as_str()?;
                    let id = row
                        .get("activeSessionId")
                        .or_else(|| row.get("id"))
                        .and_then(Value::as_str)?;
                    Some((PathBuf::from(file), id.to_string()))
                })
                .collect()
        })
}

/// A saved session's entries; `path` is a locator in `<agent dir>/sessions`.
fn read_entries(path: &Path) -> Vec<Value> {
    let agent_dir = path
        .parent()
        .and_then(Path::parent)
        .expect("a locator in the agent dir's sessions");
    support::saved_transcript(agent_dir, path)
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .expect("valid session entries")
}

fn session_files(dir: &Path) -> Vec<PathBuf> {
    support::session_texts(dir)
        .into_iter()
        .map(|(locator, _)| locator)
        .collect()
}

/// Non-blocking reader over the pty master, collecting the raw byte stream
/// the child writes (the login-panel harness's reader).
struct PtyReader {
    file: std::fs::File,
    output: Vec<u8>,
}

impl PtyReader {
    fn new(master: OwnedFd) -> PtyReader {
        let fd = master.as_raw_fd();
        fcntl(fd, F_SETFL(OFlag::O_NONBLOCK)).expect("pty master non-blocking");
        PtyReader {
            file: master.into(),
            output: Vec::new(),
        }
    }

    fn write(&mut self, payload: &[u8]) {
        self.file.write_all(payload).expect("write to the pty");
    }

    /// Drain the master until the needle appears in the collected stream,
    /// bounded by the harness deadline.
    fn wait_for(&mut self, needle: &str, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if find_subsequence(&self.output, needle.as_bytes()).is_some() {
                return;
            }
            let mut buffer = [0u8; 8192];
            match self.file.read(&mut buffer) {
                Ok(0) | Err(_) => {}
                Ok(n) => self.output.extend_from_slice(&buffer[..n]),
            }
            if Instant::now() > deadline {
                let text = String::from_utf8_lossy(&self.output);
                panic!(
                    "timeout waiting for {what} (needle {needle:?}); pty tail:\n{}",
                    &text[text.len().saturating_sub(4000)..]
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[tokio::test]
async fn interactive_fork_launch_copies_and_the_daemon_opens_the_fork() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    // The fork header records the launch's resolved cwd; macOS temp dirs sit
    // behind the /var -> /private/var symlink.
    let root = pa_types::platform::canonicalize(dir.path()).expect("canonical temp dir");
    let agent_dir = root.join("agent");
    let session_dir = agent_dir.join("sessions");
    let cwd = root.join("project");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    std::fs::create_dir_all(&cwd).expect("project dir");
    // The home has onboarding behind it: a completed first run is the only
    // thing that skips the flow, and the launch must open the fork, not the
    // welcome.
    pa_core::settings::SettingsManager::create(&cwd, &agent_dir)
        .set_onboarding_shown(true)
        .expect("onboarding flag");
    // The interactive launch child on a pty forks the hosted source; the
    // runtime daemon answers each session's turns from a queue of its own,
    // read from the script it was started with, so the fork's first live
    // turn takes the first response.
    let fork_script = root.join("script-fork.json");
    std::fs::write(
        &fork_script,
        json!({ "responses": ["forkfollowanswer"] }).to_string(),
    )
    .expect("write fork script");
    let daemon = RuntimeDaemon::spawn_with(&root, |command| {
        command
            .env("PRIME_AGENT_SESSION_DIR", &session_dir)
            .env("PI_OFFLINE", "1")
            .env("PRIME_AGENT_FAUX_SCRIPT", &fork_script);
    });
    let socket = daemon.daemon_socket.clone();

    // The source session: a real print-mode run of the real binary (the
    // same isolated-home contract the print runtime e2e uses). The print
    // runtime reads the script env as the script JSON itself.
    let seeded = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .args(["-p", "seedphrase4fork tell me"])
        .current_dir(&cwd)
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .env("PRIME_AGENT_SESSION_DIR", &session_dir)
        .env("PI_OFFLINE", "1")
        .env(
            "PRIME_AGENT_FAUX_SCRIPT",
            json!({ "responses": ["seedonlyanswer"] }).to_string(),
        )
        .env_remove("RLM_DEPTH")
        .output()
        .expect("seed run");
    assert!(
        seeded.status.success(),
        "the seed run failed: {}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    let files = session_files(&session_dir);
    assert_eq!(files.len(), 1, "one source session file");
    let source = files.into_iter().next().expect("the source file");
    let source_entries = read_entries(&source);
    let source_id = source_entries[0]["id"].as_str().expect("id").to_string();

    // Host the source on the live daemon: the fork must work on a session the
    // daemon already owns (the copy never touches the hosted file).
    let active_source = make_session_active(&daemon.socket, &source, &cwd).await;
    assert!(!active_source.is_empty(), "the daemon hosts the source");
    let source_before = support::saved_transcript(&agent_dir, &source);

    // The interactive launch child on a pty, forking the hosted source by its id.
    let outcome = root.join("outcome");
    let pty = openpty(
        Some(&Winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )
    .expect("open pty");
    let slave = pty.slave;
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("interactive_fork_child_mode")
        .env(
            CHILD_ENV,
            json!({
                "socket": socket.display().to_string(),
                "cwd": cwd.display().to_string(),
                "selector": source_id,
                "outcome": outcome.display().to_string(),
            })
            .to_string(),
        )
        .env("PRIME_AGENT_CODING_AGENT_DIR", &agent_dir)
        .env("PRIME_AGENT_SESSION_DIR", &session_dir)
        .env("PI_OFFLINE", "1")
        .env("PRIME_AGENT_FAUX_SCRIPT", fork_script.display().to_string())
        .env_remove("TMUX")
        .env_remove("RLM_DEPTH")
        .stdin(slave.try_clone().expect("clone pty slave"))
        .stdout(slave.try_clone().expect("clone pty slave"))
        .stderr(slave)
        .spawn()
        .expect("spawn the interactive client child");
    let mut pty_reader = PtyReader::new(pty.master);

    // The TUI renders the forked transcript: the source's exchange.
    pty_reader.wait_for("seedphrase4fork", "the copied transcript");

    // The daemon hosts the fork: a roster row naming a new session file —
    // never the source — under its own id.
    let source_canonical = pa_core::session::lease::canonical_session_path(&source);
    let deadline = Instant::now() + Duration::from_secs(45);
    let (fork_file, fork_id) = 'hosted: {
        loop {
            for (file, id) in daemon_roster(&daemon.socket).await {
                if pa_core::session::lease::canonical_session_path(&file) != source_canonical {
                    break 'hosted (file, id);
                }
            }
            assert!(
                Instant::now() < deadline,
                "the daemon never hosted the fork"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    let fork_entries = read_entries(&fork_file);
    // The roster row hosts the fork under a session id; the fork file's own
    // header id is a fresh one.
    assert!(!fork_id.is_empty(), "the fork hosts under a session id");
    assert_ne!(
        fork_entries[0]["id"].as_str(),
        Some(source_id.as_str()),
        "the fork header carries the new id"
    );
    assert_eq!(
        fork_entries[0]["parentSession"].as_str(),
        Some(source.display().to_string().as_str()),
        "the fork header parents at the source"
    );
    assert_eq!(
        fork_entries[0]["cwd"].as_str(),
        Some(cwd.display().to_string().as_str()),
        "the fork adopts the launch cwd"
    );
    // The branch copied the source's exchange.
    let texts: Vec<&str> = fork_entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .filter_map(|entry| entry["message"]["content"][0]["text"].as_str())
        .collect();
    assert!(
        texts.contains(&"seedphrase4fork tell me"),
        "the copied user row rides the fork: {texts:?}"
    );
    assert!(
        texts.contains(&"seedonlyanswer"),
        "the copied assistant row rides the fork: {texts:?}"
    );

    // The fork is live: a new turn on it answers through the scripted
    // engine.
    pty_reader.write(b"forked turn\r");
    pty_reader.wait_for("forkfollowanswer", "the new turn on the fork");

    // Exit cleanly (ctrl+d), and the launch reports success.
    pty_reader.write(b"\x04");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // Keep draining: the teardown's writes block on a full pty buffer
        // (macOS's is small) and the child never reaches its exit.
        let mut buffer = [0u8; 8192];
        if let Ok(n) = pty_reader.file.read(&mut buffer) {
            pty_reader.output.extend_from_slice(&buffer[..n]);
        }
        if let Some(status) = child.try_wait().expect("child status") {
            assert!(status.success(), "the interactive child failed: {status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the interactive child never exited"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let outcome = std::fs::read_to_string(&outcome).expect("the child wrote its outcome");
    assert_eq!(outcome, "0", "the fork launch exits 0");

    // The source is untouched and both sessions stay hosted (resident
    // sessions outlive the client).
    let source_after = support::saved_transcript(&agent_dir, &source);
    assert_eq!(source_before, source_after, "the source file is unchanged");
    let roster = daemon_roster(&daemon.socket).await;
    let canonical: Vec<_> = roster
        .iter()
        .map(|(file, _)| pa_core::session::lease::canonical_session_path(file))
        .collect();
    assert!(
        canonical.contains(&source_canonical),
        "the hosted source survives the fork"
    );
    let fork_canonical = pa_core::session::lease::canonical_session_path(&fork_file);
    assert!(
        canonical.contains(&fork_canonical),
        "the fork stays hosted after the client exits"
    );
    drop(child);
    drop(daemon);
}
