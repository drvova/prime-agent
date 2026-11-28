#![cfg(unix)]

//! tmux user-level verifier for the interactive launch on the default daemon, the runtime daemon:
//! the real `prime-agent` binary runs in a private tmux server with an isolated HOME and the
//! scripted faux provider, and must reach its first paint on the runtime daemon, which it spawns
//! on `daemon2.sock` and drives over wire2; no supervisor starts on `daemon.sock`. Before the TUI
//! spoke wire2 its launch waited on `daemon.sock` and timed out after 30 s (phase 8a then kept its
//! daemon the supervisor; phase 8e lifted that, and 8f made the runtime daemon the default).
//!
//! An idle supervisor that an earlier release left running on `daemon.sock` (the upgrade's case;
//! the test's legacy supervisor, `support::legacy_supervisor`) is stopped by the launch, off its
//! path: the launch paints on the runtime daemon, and the supervisor is asked for its sessions
//! and then to shut down.

mod support;

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::json;

const WAIT: Duration = Duration::from_secs(60);

/// A private tmux server (its own socket in the test's temp dir) holding one pane that runs
/// the product; dropped, it shuts the product's daemons down and kills the server.
struct Pane {
    socket: PathBuf,
    launcher: PathBuf,
}

impl Pane {
    fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .expect("tmux runs (install tmux to run this verifier)")
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.tmux(&["capture-pane", "-p", "-t", "main"]).stdout)
            .into_owned()
    }

    /// Poll the pane until `ready` holds; the screen it held then.
    fn wait(&self, what: &str, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let screen = self.screen();
            if ready(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the screen:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // The product's daemons outlive their client: stop them through the product's own command.
        let _ = Command::new("sh")
            .arg(&self.launcher)
            .args(["shutdown", "--force"])
            .output();
        let _ = self.tmux(&["kill-server"]);
    }
}

/// The isolated product (its agent dir, project and launcher) in `dir`, not started yet.
fn prepare(dir: &Path) -> Pane {
    let home = dir.join("home");
    let agent = home.join(".prime/agent");
    let project = dir.join("project");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(
        agent.join("settings.json"),
        json!({ "onboardingCompleted": true, "onboardingShown": true,
                "telemetry": { "noticeShown": true } })
        .to_string(),
    )
    .expect("settings");
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": [{ "text": "ready" }] }).to_string(),
    )
    .expect("faux script");
    // The pane's whole environment, so nothing ambient (provider keys, the user's HOME, a
    // daemon socket) reaches the product.
    let launcher = dir.join("launch.sh");
    let quote = |path: &Path| format!("'{}'", path.display());
    std::fs::write(
        &launcher,
        format!(
            "cd {project}\nexec env -i PATH=/usr/bin:/bin HOME={home} TERM=xterm-256color \
             LANG=C.UTF-8 DO_NOT_TRACK=1 PI_OFFLINE=1 \
             PRIME_AGENT_FAUX_SCRIPT={script} PRIME_AGENT_DAEMON_SOCKET={socket} {binary} \"$@\"\n",
            project = quote(&project),
            home = quote(&home),
            script = quote(&script),
            socket = quote(&dir.join("daemon.sock")),
            binary = quote(Path::new(env!("CARGO_BIN_EXE_prime-agent"))),
        ),
    )
    .expect("launcher");
    Pane {
        socket: dir.join("tmux.sock"),
        launcher,
    }
}

/// The product's interactive launch in the pane.
fn start(pane: &Pane) {
    let command = format!("sh '{}'", pane.launcher.display());
    let started = pane.tmux(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "120",
        "-y",
        "40",
        &command,
    ]);
    assert!(
        started.status.success(),
        "tmux new-session: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    // A launch that fails exits; the pane keeps its last screen so the failure shows it.
    pane.tmux(&["set-option", "-t", "main", "remain-on-exit", "on"]);
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir")
}

/// Waits for the first paint on an attached session, which shows the scripted model; a launch
/// that waits on a socket no daemon answers ends on the startup timeout instead.
fn await_first_paint(pane: &Pane) {
    let screen = pane.wait("the first paint or the startup error", |screen| {
        screen.contains("faux-1") || screen.contains("Timed out waiting")
    });
    assert!(screen.contains("faux-1"), "the launch failed:\n{screen}");
}

#[test]
fn an_idle_supervisor_left_running_is_stopped_by_a_launch_on_the_runtime_daemon() {
    let dir = temp_dir();
    let pane = prepare(dir.path());
    // The supervisor an earlier release started, idle (a saved session, no turn running and no
    // window attached), on the socket the launch derives its own from.
    let supervisor = support::legacy_supervisor::LegacySupervisor::start(
        &dir.path().join("daemon.sock"),
        vec![json!({ "id": "a1b2c3d4e5f6", "isSessionActive": false, "attachedClients": 0 })],
    );
    start(&pane);
    await_first_paint(&pane);
    assert!(
        supervisor.wait_stopped(WAIT),
        "the idle supervisor was never stopped: it was sent {:?}",
        supervisor.commands()
    );
    assert_eq!(
        (
            supervisor.commands(),
            UnixStream::connect(dir.path().join("daemon.sock")).is_err(),
            UnixStream::connect(dir.path().join("daemon2.sock")).is_ok()
        ),
        (vec!["list".to_string(), "shutdown".to_string()], true, true)
    );
}

#[test]
fn interactive_launch_runs_on_the_runtime_daemon_by_default() {
    let dir = temp_dir();
    let pane = prepare(dir.path());
    start(&pane);
    await_first_paint(&pane);
    // The daemon the launch spawned is the runtime daemon: it welcomes a wire2 client on
    // `daemon2.sock`, and nothing listens on `daemon.sock`.
    let mut stream =
        UnixStream::connect(dir.path().join("daemon2.sock")).expect("daemon2.sock answers");
    stream.set_read_timeout(Some(WAIT)).expect("a read timeout");
    let hello = pa_types::daemon::framing::encode_private_frame(
        &json!({ "type": "hello", "version": pa_types::wire2::WIRE2_VERSION, "client": "probe" }),
        &[],
        pa_types::daemon::framing::DEFAULT_PRIVATE_FRAME_LIMITS,
    )
    .expect("a hello frame");
    stream.write_all(&hello).expect("the hello");
    let mut decoder = pa_types::daemon::framing::PrivateFrameDecoder::new(
        pa_types::daemon::framing::DEFAULT_PRIVATE_FRAME_LIMITS,
    );
    let mut chunk = [0u8; 4096];
    let welcome = loop {
        let read = stream.read(&mut chunk).expect("the daemon's welcome");
        assert!(read > 0, "daemon2.sock closed before its welcome");
        if let Some(frame) = decoder
            .push(&chunk[..read])
            .expect("frames")
            .into_iter()
            .next()
        {
            break frame.header;
        }
    };
    assert_eq!(
        (
            welcome["type"].clone(),
            UnixStream::connect(dir.path().join("daemon.sock")).is_err()
        ),
        (json!("welcome"), true),
        "{welcome}"
    );
}
