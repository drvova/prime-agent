#![cfg(unix)]

//! tmux user-level verifier for the kernel todo tray and question panel: the real `prime-agent`
//! binary runs in a private tmux server against its own daemon, an isolated HOME, the real
//! kernel, and the scripted faux provider. An `eval` cell calls `todo-set` / `todo-update`
//! and `ask`; the test drives the screen with keys only and asserts what the user sees (the
//! docked tray, its fold toggle, the question panel, the cell card's todo row, the finished
//! list's summary and its clearing on the next turn) and that the waiting kernel call received
//! the keyboard answer (the cell writes it to a file).
//!
//! Every wait polls the captured pane for observable state under a deadline; nothing sleeps
//! for a fixed time.

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
        // The product's daemon outlives its client: stop it through the product's own command.
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
    let plan = [
        "Read the code",
        "Wire the tray",
        "Render the panel",
        "Answer by keyboard",
        "Record the answer",
        "Write the test",
        "Ship it",
    ];
    std::fs::write(
        &script,
        json!({ "engine": "faux", "responses": [
            cell(&format!(
                r#"(todo-set (list {plan}))
(todo-update "1" #:status "done")
(todo-update "2" #:status "in_progress")
(define r (ask "Which database should the cache use?" (list "sqlite" "postgres")))
(fs-write (string-append (session-cwd) "/answer.txt") (string-append (hash-ref r 'status) ":" (hash-ref r 'answer)))"#,
                plan = plan.map(|item| format!("{item:?}")).join(" "),
            )),
            { "text": "asked" },
            cell(
                r#"(todo-set (map (lambda (item) (hash 'text (hash-ref item 'text) 'status "done"))
               (hash-ref (todo-get) 'items)))
(define checks (ask "Which checks?" (list "fmt" "clippy") #:multi #t))
(fs-write (string-append (session-cwd) "/checks.txt") (string-append (hash-ref checks 'status) ":" (string-join (hash-ref checks 'answer) ",")))"#,
            ),
            { "text": "finished" },
            { "text": "you are welcome" },
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
        "100",
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

fn dock_line<'a>(screen: &'a str, prefix: &str) -> Option<&'a str> {
    screen.lines().find(|line| line.starts_with(prefix))
}

#[test]
fn todos_and_a_question_are_driven_from_the_kernel_and_answered_by_keyboard() {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let pane = start(dir.path());
    pane.wait_for("faux-1");
    pane.type_text("plan the cache work");
    pane.keys(&["Enter"]);

    // The cell set seven items, finished one, started the next, and asked: the tray docks
    // above the open question, folded around the current item.
    let asked = pane.wait("the tray over the question panel", |screen| {
        screen.contains("Which database should the cache use?") && screen.contains("Todos 1/7")
    });
    eprintln!("--- tray over the question panel ---\n{asked}");
    assert_eq!(
        dock_line(&asked, "  Todos"),
        Some("  Todos 1/7 \u{b7} Alt+T expand")
    );
    for row in [
        "  \u{2713} Read the code",
        "  \u{25b8} Wire the tray",
        "  \u{25cb} Record the answer",
        "    +2 more",
    ] {
        assert!(asked.lines().any(|line| line == row), "{row:?} in\n{asked}");
    }
    assert!(!asked.contains("Ship it"), "folded:\n{asked}");

    // Alt+T unfolds the tray while the question waits (the panel passes the chord through).
    pane.keys(&["M-t"]);
    let expanded = pane.wait_for("Alt+T collapse");
    assert!(
        expanded.lines().any(|line| line == "  \u{25cb} Ship it"),
        "{expanded}"
    );
    assert!(expanded.contains("Which database should the cache use?"));

    // Answer by keyboard: the second choice's digit.
    assert!(
        expanded.contains("\u{203a} (\u{2022}) 1. sqlite") && expanded.contains("3. Other\u{2026}"),
        "numbered rows and the typed-answer row:\n{expanded}"
    );
    pane.keys(&["2"]);
    let answered = pane.wait(
        "the cell's todo and question rows after the answer",
        |screen| {
            screen.contains("todos \u{b7} 1/7 \u{b7} Wire the tray") && screen.contains("asked")
        },
    );
    eprintln!("--- after the answer ---\n{answered}");
    assert!(
        answered.contains(" ? Which database should the cache use? \u{2192} postgres"),
        "the transcript records the question and its answer:\n{answered}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("project/answer.txt")).expect("the cell's answer"),
        "answered:postgres",
        "the waiting kernel call received the keyboard answer"
    );

    // The next turn finishes the list and asks a multi-choice question: a digit checks a
    // choice, `Other…` takes a typed answer beside it.
    pane.type_text("finish up");
    pane.keys(&["Enter"]);
    let multi = pane.wait_for("Which checks?");
    eprintln!("--- the finished tray over a multi-choice question ---\n{multi}");
    assert_eq!(
        dock_line(&multi, "  \u{2713} Todos"),
        Some("  \u{2713} Todos 7/7 \u{b7} all done")
    );
    pane.keys(&["1"]);
    pane.wait_for("[x] 1. fmt");
    pane.keys(&["3"]);
    pane.wait_for("Type your answer");
    pane.type_text("lint");
    pane.keys(&["Enter"]);
    let finished = pane.wait_for("finished");
    eprintln!("--- finished ---\n{finished}");
    assert!(
        finished.contains(" ? Which checks? \u{2192} fmt, lint"),
        "{finished}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("project/checks.txt")).expect("the cell's answer"),
        "answered:fmt,lint"
    );

    // ...and the user's next turn clears it.
    pane.type_text("thanks");
    pane.keys(&["Enter"]);
    let cleared = pane.wait_for("you are welcome");
    assert!(!cleared.contains("Todos 7/7"), "{cleared}");
}
