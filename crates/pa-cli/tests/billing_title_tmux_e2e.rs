#![cfg(unix)]

//! tmux user-level verifier for the title's billing label: the real `prime-agent` binary runs in
//! a private tmux server against its own daemon, an isolated HOME, and the scripted faux provider
//! standing in for Anthropic's endpoint, with a stored Claude sign-in (an OAuth credential). After
//! one answered turn the title beside the chat name reads `plan`, never a dollar figure: the
//! subscription's calls carry API list prices that are never billed.
//!
//! A second check opens `/usage` against a loopback Claude usage fixture: the spend half splits
//! the plan's calls out as API-equivalent value (not billed), the quota card keeps its reset
//! countdown, and the title then shows the cached 5-hour window instead of `plan`.
//!
//! A third walks the `/usage` page into its details view and back: the quota card keeps its
//! gauge row cell for cell, the details add the card's age, and Esc returns to the overview.
//!
//! Every wait polls the captured pane for observable state under a deadline; nothing sleeps
//! for a fixed time.

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

/// Start the product; `quota_origin` points every provider usage endpoint at a loopback
/// fixture (`PRIME_AGENT_QUOTA_BASE_URL`), or at a closed port when `None`.
fn start(dir: &Path, quota_origin: &str) -> Pane {
    let home = dir.join("home");
    let agent = home.join(".prime/agent");
    let project = dir.join("billing-chat");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(
        agent.join("settings.json"),
        json!({ "onboardingCompleted": true, "onboardingShown": true,
                "telemetry": { "noticeShown": true },
                "defaultProvider": "anthropic", "defaultModel": "claude-fable-5" })
        .to_string(),
    )
    .expect("settings");
    // A Claude sign-in that never expires during the test: the credential's kind is all the
    // classifier reads; the faux provider never sends it anywhere.
    std::fs::write(
        agent.join("auth.json"),
        json!({ "anthropic": { "type": "oauth", "access": "sk-ant-oat01-e2e",
                               "refresh": "e2e-refresh", "expires": 4_102_444_800_000_i64 } })
        .to_string(),
    )
    .expect("auth");
    let script = dir.join("faux.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "provider": "anthropic", "modelId": "claude-fable-5",
                "baseUrl": "https://api.anthropic.com",
                "responses": [{ "text": "planned answer" }] })
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
             LANG=C.UTF-8 DO_NOT_TRACK=1 PI_OFFLINE=1{daemon_env} PRIME_AGENT_QUOTA_BASE_URL={quota_origin} \
             PRIME_AGENT_FAUX_SCRIPT={script} PRIME_AGENT_DAEMON_SOCKET={socket} {binary} \"$@\"\n",
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
        "100",
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

#[test]
fn a_subscription_session_titles_its_plan_without_a_dollar_figure() {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let pane = start(dir.path(), "http://127.0.0.1:9");
    pane.wait("the composer", |screen| screen.contains("claude-fable-5"));
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "-l", "hello"])
        .status
        .success());
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "Enter"])
        .status
        .success());
    pane.wait("the answer", |screen| screen.contains("planned answer"));
    let title_line = |screen: &str| {
        screen
            .lines()
            .find(|line| line.trim_start().starts_with("billing-chat"))
            .map(|line| line.trim().to_string())
    };
    let screen = pane.wait("the plan title", |screen| {
        title_line(screen).is_some_and(|line| line.ends_with("plan"))
    });
    assert_eq!(
        title_line(&screen),
        Some("billing-chat  plan".to_string()),
        "the screen:\n{screen}"
    );
}

/// A loopback Claude usage endpoint: `GET /api/oauth/usage` answers a 5-hour window 42% used,
/// resetting 80 minutes from the start; the server runs until the test process ends.
fn quota_fixture() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the quota fixture");
    let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
    let resets_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        + 80 * 60;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = json!({
                "five_hour": { "utilization": 42, "resets_at": iso_seconds(resets_at) },
            })
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    origin
}

/// `YYYY-MM-DDTHH:MM:SSZ` for epoch seconds (civil-from-days).
fn iso_seconds(epoch: u64) -> String {
    let days = i64::try_from(epoch / 86_400).expect("days");
    let of_day = epoch % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

#[test]
fn the_usage_page_splits_plan_spend_and_the_title_shows_the_cached_quota() {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let pane = start(dir.path(), &quota_fixture());
    pane.wait("the composer", |screen| screen.contains("claude-fable-5"));
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "-l", "hello"])
        .status
        .success());
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "Enter"])
        .status
        .success());
    pane.wait("the answer", |screen| screen.contains("planned answer"));
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "-l", "/usage"])
        .status
        .success());
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "Enter"])
        .status
        .success());
    let usage_screen = pane.wait("the split spend and the quota card", |screen| {
        screen.contains("API-equivalent value")
            && screen.contains("5-hour")
            && screen.contains("resets")
    });
    let plan_row = usage_screen
        .lines()
        .find(|line| line.contains("Plan (not billed)"))
        // The table's column padding depends on the token counts; the cells are what matter.
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "));
    assert_eq!(
        plan_row,
        Some("Plan (not billed) $0.00 1 call API-equivalent value".to_string()),
        "the page:\n{usage_screen}"
    );
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "Escape"])
        .status
        .success());
    // The cached card now names the plan's tightest window in the title, with its countdown.
    pane.wait("the cached quota in the title", |screen| {
        screen.lines().any(|line| {
            let line = line.trim();
            line.starts_with("billing-chat  5h 42% \u{b7} 1h")
        })
    });
}

/// The details view reuses the overview's card: the 5-hour gauge row is the same cells in both
/// views, the details title the card with its age, and Esc comes back to the same overview.
#[test]
fn the_usage_details_keep_the_overview_card_and_esc_returns_to_it() {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let pane = start(dir.path(), &quota_fixture());
    pane.wait("the composer", |screen| screen.contains("claude-fable-5"));
    for (keys, ready) in [
        (["-l", "hello"], "planned answer"),
        (["-l", "/usage"], "Esc close"),
    ] {
        assert!(pane
            .tmux(&["send-keys", "-t", "main", keys[0], keys[1]])
            .status
            .success());
        assert!(pane
            .tmux(&["send-keys", "-t", "main", "Enter"])
            .status
            .success());
        pane.wait(ready, |screen| screen.contains(ready));
    }
    // A row up to its reset countdown, which may tick between two captures.
    let row = |screen: &str, needle: &str| {
        screen
            .lines()
            .find(|line| line.contains(needle))
            .map(|line| {
                line.split("resets")
                    .next()
                    .unwrap_or(line)
                    .trim_end()
                    .to_string()
            })
    };
    let overview = pane.wait("the overview's quota card", |screen| {
        screen.contains("Esc close")
            && row(screen, "5-hour").is_some_and(|row| row.contains('\u{2501}'))
    });
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "Enter"])
        .status
        .success());
    let details = pane.wait("the details view", |screen| {
        screen.contains("Usage \u{203a} Details") && screen.contains("Esc back")
    });
    assert_eq!(
        (
            row(&details, "5-hour"),
            row(&details, "Anthropic").map(|title| title.contains("fetched")),
        ),
        (row(&overview, "5-hour"), Some(true)),
        "the overview:\n{overview}\nthe details:\n{details}"
    );
    assert!(pane
        .tmux(&["send-keys", "-t", "main", "Escape"])
        .status
        .success());
    let back = pane.wait("the overview again", |screen| screen.contains("Esc close"));
    assert_eq!(
        (row(&back, "5-hour"), row(&back, "Anthropic")),
        (row(&overview, "5-hour"), row(&overview, "Anthropic")),
        "the overview:\n{overview}\nback:\n{back}"
    );
}

support::on_the_runtime_daemon!(
    a_subscription_session_titles_its_plan_without_a_dollar_figure,
    the_usage_page_splits_plan_spend_and_the_title_shows_the_cached_quota,
    the_usage_details_keep_the_overview_card_and_esc_returns_to_it,
);
