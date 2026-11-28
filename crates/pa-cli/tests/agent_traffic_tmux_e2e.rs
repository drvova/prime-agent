#![cfg(unix)]

//! tmux user-level verifier for the scoped agents view's Traffic pane: the real `prime-agent`
//! binary runs in a private tmux server against its own daemon, an isolated HOME, the real
//! kernel, and the scripted faux provider. The root spawns two children; each child messages
//! its sibling through `agent-send`. The test opens the scoped agents view from the
//! dock, presses `m`, and polls the pane until both directions list (never a fixed sleep).

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::json;

const WAIT: Duration = Duration::from_secs(60);

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

    fn keys(&self, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", "main"];
        args.extend_from_slice(keys);
        assert!(self.tmux(&args).status.success(), "send-keys {keys:?}");
    }

    fn type_text(&self, text: &str) {
        assert!(self
            .tmux(&["send-keys", "-t", "main", "-l", text])
            .status
            .success());
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

    fn wait_for(&self, needle: &str) -> String {
        self.wait(needle, |screen| screen.contains(needle))
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

fn cell(code: &str) -> serde_json::Value {
    json!({ "content": [{ "type": "toolCall", "name": "eval", "arguments": { "code": code } }] })
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
    let marker = dir.join("spawned.marker");
    // One script serves every session (a scripted root's children run its script): the root
    // (first, no marker yet) spawns the two children; each child polls the roster for its
    // sibling and messages it directly.
    let code = format!(
        r#"(define marker {marker:?})
(if (not (= 0 (hash-ref (bash (string-append "test -e '" marker "'")) 'exit-code)))
    (begin
      (fs-write marker "")
      (rlm-spawn "say hello to your sibling" #:name "alpha")
      (rlm-spawn "say hello to your sibling" #:name "beta"))
    (let loop ([tries 600])
      (let ([siblings (filter (lambda (agent)
                                (and (equal? (hash-try-get agent 'relationship) "sibling")
                                     (string? (hash-try-get agent 'sessionName))))
                              (hash-ref (agent-roster) 'agents))])
        (cond
          [(pair? siblings)
           (agent-send "contract ready for review"
                               #:to (hash-ref (car siblings) 'sessionName))]
          [(> tries 0) (bash "sleep 0.05") (loop (- tries 1))]
          [else (error "no sibling appeared")]))))
"#,
        marker = marker.display().to_string(),
    );
    let mut responses = vec![cell(&code), json!({ "text": "cell done" })];
    responses.extend((0..12).map(|_| json!({ "text": "noted" })));
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": responses }).to_string(),
    )
    .expect("faux script");
    // The pane's whole environment, so nothing ambient (provider keys, the user's HOME, a
    // daemon socket) reaches the product. The children's script is named twice: the supervisor's
    // children get it from the create (the client's `childScript`), the runtime daemon's from its
    // own environment.
    let launcher = dir.join("launch.sh");
    let quote = |path: &Path| format!("'{}'", path.display());
    std::fs::write(
        &launcher,
        format!(
            "cd {project}\nexec env -i PATH=/usr/bin:/bin HOME={home} TERM=xterm-256color \
             LANG=C.UTF-8 DO_NOT_TRACK=1 PI_OFFLINE=1 \
             PRIME_AGENT_FAUX_SCRIPT={script} PRIME_AGENT_FAUX_CHILD_SCRIPT={script} \
             PRIME_AGENT_DAEMON_SOCKET={socket} {binary} \"$@\"\n",
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
    pane
}

#[test]
fn two_children_messaging_each_other_show_in_the_traffic_pane() {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let pane = start(dir.path());
    pane.wait_for("faux-1");
    pane.type_text("spawn the pair");
    pane.keys(&["Enter"]);
    pane.wait_for("cell done");
    // The dock lists the children; its focus key and Enter open the scoped agents view.
    pane.wait("the subagents dock", |screen| {
        screen.contains("alpha") || screen.contains("subagent")
    });
    pane.keys(&["M-a"]);
    pane.keys(&["Enter"]);
    pane.wait_for("subagents");
    let closed = pane.wait("both children listed", |screen| {
        screen.contains("alpha") && screen.contains("beta") && screen.contains("traffic")
    });
    eprintln!("--- the scoped view, pane closed ---\n{closed}");
    pane.keys(&["m"]);
    let open = pane.wait("both directions in the Traffic pane", |screen| {
        screen.contains("alpha \u{2192} beta \u{b7} sibling")
            && screen.contains("beta \u{2192} alpha \u{b7} sibling")
    });
    eprintln!("--- the Traffic pane ---\n{open}");
    assert!(open.contains("Traffic \u{b7} 2 messages"), "{open}");
    assert!(open.contains("contract ready for review"), "{open}");
    assert!(open.contains("M hide traffic"), "{open}");
}
