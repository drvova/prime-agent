#![cfg(unix)]

//! tmux user-level verifier for the interactive launch under the runtime-daemon opt-in
//! (`PRIME_AGENT_RUNTIME_DAEMON=1`): the real `prime-agent` binary runs in a private tmux server
//! with an isolated HOME and the scripted faux provider, and must reach its first paint on a
//! supervisor that answers on `daemon.sock`. The TUI speaks the old wire only, so the daemon it
//! spawns stays the supervisor whatever the opt-in says; before that, the spawned daemon became
//! the runtime daemon on `daemon2.sock` and the launch timed out after 30 s.

use std::io::{BufRead as _, BufReader};
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

fn start(dir: &Path) -> Pane {
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
    // daemon socket) reaches the product; the opt-in is the one addition.
    let launcher = dir.join("launch.sh");
    let quote = |path: &Path| format!("'{}'", path.display());
    std::fs::write(
        &launcher,
        format!(
            "cd {project}\nexec env -i PATH=/usr/bin:/bin HOME={home} TERM=xterm-256color \
             LANG=C.UTF-8 DO_NOT_TRACK=1 PI_OFFLINE=1 PRIME_AGENT_RUNTIME_DAEMON=1 \
             PRIME_AGENT_FAUX_SCRIPT={script} PRIME_AGENT_DAEMON_SOCKET={socket} {binary} \"$@\"\n",
            project = quote(&project),
            home = quote(&home),
            script = quote(&script),
            socket = quote(&dir.join("daemon.sock")),
            binary = quote(Path::new(env!("CARGO_BIN_EXE_prime-agent"))),
        ),
    )
    .expect("launcher");
    let pane = Pane {
        socket: dir.join("tmux.sock"),
        launcher,
    };
    let command = format!("sh {}", quote(&pane.launcher));
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
    pane
}

#[test]
fn interactive_launch_under_the_runtime_opt_in_reaches_the_supervisor() {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let pane = start(dir.path());
    // First paint on an attached session shows the scripted model; a launch that waits on a
    // socket no daemon answers ends on the startup timeout instead.
    let screen = pane.wait("the first paint or the startup error", |screen| {
        screen.contains("faux-1") || screen.contains("Timed out waiting")
    });
    assert!(screen.contains("faux-1"), "the launch failed:\n{screen}");
    // The daemon the launch spawned is the supervisor: it greets on `daemon.sock`.
    let stream = UnixStream::connect(dir.path().join("daemon.sock")).expect("daemon.sock answers");
    stream.set_read_timeout(Some(WAIT)).expect("a read timeout");
    let mut hello = String::new();
    BufReader::new(stream)
        .read_line(&mut hello)
        .expect("the daemon's hello");
    let hello: serde_json::Value = serde_json::from_str(&hello).expect("the hello is JSON");
    assert_eq!(hello["type"], json!("daemon_hello"), "{hello}");
}
