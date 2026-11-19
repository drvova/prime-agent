//! User-level check of the footer's live context usage: the real `prime-agent` binary in a private
//! tmux server, over a scripted faux session (a tool round, then one long paced reply). The
//! footer's prompt-cache figure appears once the first round settles, and its context label moves
//! while the reply still streams, before the turn ends. Every wait polls the rendered pane for an
//! observable change; none sleeps a fixed interval to let the product catch up.
#![cfg(unix)]
// too_many_lines: style gate only (one linear user-level scenario).
#![allow(clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::json;

/// The streamed reply's last word: on screen only once the reply has fully streamed.
const END_MARKER: &str = "livecontextdone";

/// A private tmux server running one pane, killed (with the daemon it spawned) on drop.
struct TmuxPane {
    server: String,
    socket: PathBuf,
}

impl TmuxPane {
    fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .arg("-L")
            .arg(&self.server)
            .args(args)
            .output()
            .expect("run tmux")
    }

    fn capture(&self) -> String {
        String::from_utf8_lossy(&self.tmux(&["capture-pane", "-p", "-t", "live"]).stdout)
            .into_owned()
    }

    /// Poll the pane until `ready` accepts it, returning the accepting capture.
    fn wait_for(&self, what: &str, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let screen = self.capture();
            if ready(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the pane shows:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for TmuxPane {
    fn drop(&mut self) {
        // tmux leaves its socket file behind a killed server: remove it too.
        let tmux_socket = self.tmux(&["display-message", "-p", "#{socket_path}"]);
        let _ = self.tmux(&["kill-server"]);
        let tmux_socket = String::from_utf8_lossy(&tmux_socket.stdout)
            .trim()
            .to_string();
        if !tmux_socket.is_empty() {
            let _ = std::fs::remove_file(tmux_socket);
        }
        shutdown_daemon(&self.socket);
    }
}

/// Stop the daemon the client started on `socket`, by protocol.
fn shutdown_daemon(socket: &Path) {
    use std::io::{BufRead, BufReader, Write};
    let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return;
    };
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let _ = reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(5)));
    let mut hello = String::new();
    let _ = reader.read_line(&mut hello);
    let command = json!({
        "type": "command", "id": "test-shutdown",
        "protocol": { "name": "prime-agent.daemon", "version": 7 },
        "command": { "type": "shutdown" },
    });
    let _ = writeln!(writer, "{command}");
    let mut response = String::new();
    let _ = reader.read_line(&mut response);
}

/// The footer's context label (`[~]N/200k`) on the screen, if any.
fn context_label(screen: &str) -> Option<String> {
    screen
        .split_whitespace()
        .find(|word| word.ends_with("/200k"))
        .map(str::to_string)
}

/// The footer's `cache H%` figure on the screen, if any.
fn cache_figure(screen: &str) -> Option<String> {
    let words: Vec<&str> = screen.split_whitespace().collect();
    words
        .windows(2)
        .find(|pair| pair[0] == "cache" && pair[1].ends_with('%'))
        .map(|pair| pair[1].to_string())
}

#[test]
fn the_footer_context_moves_while_the_reply_streams() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping the live footer check");
        return;
    }
    let dir = tempfile::TempDir::new().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical temp dir");
    let (home, agent_dir, cwd) = (root.join("home"), root.join("agent"), root.join("project"));
    for path in [&home, &agent_dir, &cwd] {
        std::fs::create_dir_all(path).expect("fixture dir");
    }
    pa_core::settings::SettingsManager::create(&cwd, &agent_dir)
        .set_onboarding_shown(true)
        .expect("onboarding flag");
    // Round one calls a tool the session lacks (an error result, no side effects); round two
    // streams a long reply slowly enough for several footer repaints.
    let reply = format!(
        "{} {END_MARKER}",
        "streaming words fill the context ".repeat(600)
    );
    let script = root.join("script.json");
    std::fs::write(
        &script,
        json!({
            "engine": "faux",
            "contextWindow": 200_000,
            "tokensPerSecond": 1_000,
            "responses": [
                { "content": [
                    { "type": "toolCall", "id": "call-1", "name": "synthetic_missing_tool", "arguments": {} },
                ] },
                { "text": reply },
            ],
        })
        .to_string(),
    )
    .expect("write script");
    let socket = root.join("daemon.sock");
    let pane = TmuxPane {
        server: format!("pa-live-context-{}", std::process::id()),
        socket: socket.clone(),
    };
    let binary = env!("CARGO_BIN_EXE_prime-agent");
    let launch = format!(
        "cd '{cwd}' && exec env -i HOME='{home}' PATH='{path}' TERM=xterm-256color TMPDIR='{root}' \
         DO_NOT_TRACK=1 PI_OFFLINE=1 PRIME_AGENT_CODING_AGENT_DIR='{agent}' \
         PRIME_AGENT_SESSION_DIR='{sessions}' PRIME_AGENT_FAUX_SCRIPT='{script}' \
         '{binary}' --daemon-socket '{socket}' 'look it up'",
        cwd = cwd.display(),
        home = home.display(),
        path = std::env::var("PATH").unwrap_or_default(),
        root = root.display(),
        agent = agent_dir.display(),
        sessions = agent_dir.join("sessions").display(),
        script = script.display(),
        socket = socket.display(),
    );
    let started = pane.tmux(&[
        "new-session",
        "-d",
        "-s",
        "live",
        "-x",
        "120",
        "-y",
        "32",
        &launch,
    ]);
    assert!(
        started.status.success(),
        "tmux new-session failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    // The first round's real usage carries its cache split: the figure appears beside the
    // context label as soon as that round settles.
    pane.wait_for("the cache figure after the first round", |screen| {
        cache_figure(screen).is_some()
    });
    // While the reply streams, the label is the `~` estimate, and it moves before the reply ends.
    let streaming = pane.wait_for("a streaming estimate", |screen| {
        !screen.contains(END_MARKER)
            && context_label(screen).is_some_and(|label| label.starts_with('~'))
    });
    let first = context_label(&streaming).expect("the streaming label");
    let moved = pane.wait_for("the estimate to move", |screen| {
        screen.contains(END_MARKER)
            || context_label(screen).is_some_and(|label| label != first && label.starts_with('~'))
    });
    assert!(
        !moved.contains(END_MARKER),
        "the footer stayed at {first} until the reply ended:\n{moved}"
    );
    // The settled reply replaces the estimate with its measured usage.
    pane.wait_for("the measured reading after the reply", |screen| {
        screen.contains(END_MARKER)
            && context_label(screen).is_some_and(|label| !label.starts_with('~'))
    });
}
