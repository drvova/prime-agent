#![cfg(unix)]

//! tmux user-level verifier for `/better`: the real `prime-agent` binary runs in a private tmux
//! server against its own daemon, an isolated HOME, and the scripted faux provider. `/better fix
//! the parser` sends the read-only helper off (it reads `src/parser.rs` through its `eval` tool,
//! then answers with a rewrite). Its first rewrite cites `cargo test`, which the pane's `PATH`
//! does not have, so verification sends it back for one refine pass; the refined rewrite fills
//! the composer with the ready notice, and nothing is sent: the session records no user message
//! and the model is never asked to answer.
//!
//! Every wait polls the captured pane for observable state under a deadline; nothing sleeps
//! for a fixed time.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::json;

const WAIT: Duration = Duration::from_secs(60);

/// The first rewrite: it cites a program the pane's `PATH` (`/usr/bin:/bin`) does not have.
const FIRST_REWRITE: &str =
    "Fix `parse_expr` in `src/parser.rs`; prove it with `cargo test` first.";

/// The refined rewrite, after verification named `cargo test`: every reference resolves.
const REWRITE: &str = "Fix `parse_expr` in `src/parser.rs`; prove it with a failing case first.";

/// A reply the model would give only if the prompt had been sent.
const SENT_MARKER: &str = "THE PROMPT WAS SENT";

/// A private tmux server (its own socket in the test's temp dir) holding one pane that runs
/// the product; dropped, it shuts the product's daemon down and kills the server.
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

    fn send(&self, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", "main"];
        args.extend_from_slice(keys);
        assert!(self.tmux(&args).status.success(), "tmux send-keys {keys:?}");
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
        // The product's daemon outlives its client: stop it through the product's own command,
        // which stops either daemon.
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
    let project = dir.join("parser-project");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(project.join("src")).expect("project dir");
    std::fs::write(
        project.join("src/parser.rs"),
        "pub fn parse_expr(input: &str) -> Option<i64> {\n    input.trim().parse().ok()\n}\n",
    )
    .expect("parser source");
    std::fs::write(
        agent.join("settings.json"),
        json!({ "onboardingCompleted": true, "onboardingShown": true,
                "telemetry": { "noticeShown": true } })
        .to_string(),
    )
    .expect("settings");
    // The helper's turns (a read, the first rewrite, the refined one after verification), then
    // the answer the model would give if anything were sent.
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": [
            { "content": [{ "type": "toolCall", "id": "read-1", "name": "eval",
                            "arguments": { "code": "(displayln (hash-ref (fs-read \"src/parser.rs\") 'text))" } }] },
            { "text": format!("<depth>(1) a failing case first (2) the parser source (3) cargo \
                               test (4) the test</depth>\n<unresolved>\n</unresolved>\n\
                               <rewrite>\n{FIRST_REWRITE}\n</rewrite>") },
            { "text": format!("<depth>(4) a failing case</depth>\n<unresolved>\n</unresolved>\n\
                               <rewrite>\n{REWRITE}\n</rewrite>") },
            { "text": SENT_MARKER },
        ] })
        .to_string(),
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
             LANG=C.UTF-8 DO_NOT_TRACK=1 PI_OFFLINE=1{daemon_env} PRIME_AGENT_FAUX_SCRIPT={script} \
             PRIME_AGENT_DAEMON_SOCKET={socket} {binary} \"$@\"\n",
            project = quote(&project),
            home = quote(&home),
            script = quote(&script),
            socket = quote(&dir.join("daemon.sock")),
            binary = quote(Path::new(env!("CARGO_BIN_EXE_prime-agent"))),
            daemon_env = support::daemon_env(),
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
        "110",
        "-y",
        "30",
        &command,
    ]);
    assert!(
        started.status.success(),
        "tmux new-session: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    pane
}

/// Every session line under `home`'s session store that records a user message.
fn user_messages(home: &Path) -> Vec<String> {
    let agent_dir = home.join(".prime/agent");
    let sessions = pa_types::platform::canonicalize(&agent_dir)
        .unwrap_or_else(|_| agent_dir.clone())
        .join("sessions");
    support::saved_transcripts(&agent_dir)
        .into_iter()
        .filter(|(locator, _)| locator.starts_with(&sessions))
        .flat_map(|(_, bytes)| {
            String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|line| {
                    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|row| {
                        row["type"] == "message" && row["message"]["role"] == "user"
                    })
                })
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn better_fills_the_composer_and_nothing_is_sent() {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let pane = start(dir.path());
    pane.wait("the composer", |screen| screen.contains("faux"));
    pane.send(&["-l", "/better fix the parser"]);
    pane.send(&["Enter"]);
    let screen = pane.wait("the rewrite in the composer", |screen| {
        screen.contains("Improved prompt is in your input box")
    });
    let composer_line = screen
        .lines()
        .find(|line| line.contains("Fix `parse_expr`"))
        .map(str::trim);
    assert_eq!(
        composer_line.map(|line| line.trim_start_matches(['>', ' ']).trim()),
        Some(REWRITE),
        "the screen:\n{screen}"
    );
    assert!(!screen.contains(SENT_MARKER), "the screen:\n{screen}");
    assert_eq!(
        user_messages(&dir.path().join("home")),
        Vec::<String>::new()
    );
}

support::on_the_supervisor!(better_fills_the_composer_and_nothing_is_sent,);
