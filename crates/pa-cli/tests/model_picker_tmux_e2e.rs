#![cfg(unix)]

//! tmux user-level verifier for the `/model` picker: the real `prime-agent` binary runs in a
//! private tmux server (`tmux -L` with a per-test socket name) against its own daemon, an
//! isolated HOME, and the scripted faux provider standing in for Anthropic's endpoint, with a
//! stored Claude sign-in. The catalog holds a Claude plan, a metered provider, and a free local
//! endpoint (models.json). The run opens `/model`, waits for the daemon's billing classes to
//! land in the rows, turns the billing chip to `plan`, opens the other Claude model's action menu,
//! favorites it (the favorites group heads the list), switches to it from the menu, and checks
//! the screen, the session file's recorded model switch, and the persisted favorites.
//!
//! Every wait polls the captured pane for observable state under a deadline; nothing sleeps
//! for a fixed time.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(60);

/// A private tmux server holding one pane that runs the product; dropped, it shuts the product's
/// daemon down and kills the server.
struct Pane {
    server: String,
    launcher: PathBuf,
}

impl Pane {
    fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .arg("-L")
            .arg(&self.server)
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

/// Start the product in `dir` with the three-class catalog.
fn start(dir: &Path) -> Pane {
    let home = dir.join("home");
    let agent = home.join(".prime/agent");
    let project = dir.join("picker-chat");
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
    // billing classifier reads; the faux provider never sends it anywhere.
    std::fs::write(
        agent.join("auth.json"),
        json!({ "anthropic": { "type": "oauth", "access": "sk-ant-oat01-e2e",
                               "refresh": "e2e-refresh", "expires": 4_102_444_800_000_i64 } })
        .to_string(),
    )
    .expect("auth");
    std::fs::write(
        agent.join("models.json"),
        json!({ "providers": {
            "anthropic": {
                "api": "anthropic-messages", "baseUrl": "https://api.anthropic.com",
                "models": [{ "id": "claude-e2e-plan-two", "name": "Claude Plan Two",
                             "input": ["text", "image"], "contextWindow": 200_000,
                             "maxTokens": 8192, "cost": { "input": 3, "output": 15 } }]
            },
            "meterco": {
                "api": "openai-completions", "baseUrl": "https://meter.example.invalid/v1",
                "apiKey": "sk-e2e-meter",
                "models": [{ "id": "meter-1", "name": "Meter One", "contextWindow": 128_000,
                             "maxTokens": 4096, "cost": { "input": 0.5, "output": 1.5 } }]
            },
            "localbox": {
                "api": "openai-completions", "baseUrl": "http://127.0.0.1:9/v1",
                "apiKey": "none",
                "models": [{ "id": "local-1", "name": "Local One", "contextWindow": 32_000,
                             "maxTokens": 4096 }]
            }
        } })
        .to_string(),
    )
    .expect("models.json");
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
             LANG=C.UTF-8 DO_NOT_TRACK=1 PI_OFFLINE=1{daemon_env} PRIME_AGENT_QUOTA_BASE_URL=http://127.0.0.1:9 \
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
    let server = format!(
        "pa-model-picker-{}",
        dir.file_name()
            .and_then(|name| name.to_str())
            .expect("temp dir name")
    );
    let pane = Pane { server, launcher };
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

/// The `model_change` entries the session files under `home` recorded, as `provider/id`.
fn recorded_switches(home: &Path) -> Vec<String> {
    let agent_dir = home.join(".prime/agent");
    let sessions = pa_types::platform::canonicalize(&agent_dir)
        .unwrap_or_else(|_| agent_dir.clone())
        .join("sessions");
    let mut switches = Vec::new();
    for (locator, bytes) in support::saved_transcripts(&agent_dir) {
        if !locator.starts_with(&sessions) {
            continue;
        }
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if entry["type"] == "model_change" {
                switches.push(format!(
                    "{}/{}",
                    entry["provider"].as_str().unwrap_or_default(),
                    entry["modelId"].as_str().unwrap_or_default()
                ));
            }
        }
    }
    switches
}

/// The picker rows naming one model, trimmed.
fn row<'a>(screen: &'a str, name: &str) -> Option<&'a str> {
    screen
        .lines()
        .find(|line| line.contains(name))
        .map(str::trim)
}

#[test]
fn the_picker_filters_by_billing_favorites_and_switches_the_session() {
    let dir = tempfile::TempDir::new_in(
        pa_types::platform::canonicalize(std::env::temp_dir()).expect("temp root"),
    )
    .expect("temp dir");
    let pane = start(dir.path());
    pane.wait("the composer", |screen| screen.contains("claude-fable-5"));
    // One answered turn materializes the session file the switch is recorded in.
    pane.type_text("hello");
    pane.keys(&["Enter"]);
    pane.wait("the answer", |screen| screen.contains("planned answer"));
    pane.type_text("/model");
    pane.keys(&["Enter"]);
    // The daemon's billing classes land in the rows: the plan, the metered rates, the free box.
    let opened = pane.wait("the classified rows", |screen| {
        screen.contains("Search models")
            && row(screen, "Claude Plan Two").is_some_and(|line| line.contains("plan"))
            && row(screen, "Meter One").is_some_and(|line| line.contains("$0.5/$1.5"))
            && row(screen, "Local One").is_some_and(|line| line.contains("free"))
    });
    println!("--- /model opened ---\n{opened}");
    pane.keys(&["M-2"]);
    let filtered = pane.wait("the plan filter", |screen| {
        screen.contains("billing: plan")
            && !screen.contains("Meter One")
            && !screen.contains("Local One")
            && screen.contains("Claude Plan Two")
    });
    println!("--- billing: plan ---\n{filtered}");
    // The fuzzy search composes with the chip: the query's first match is selected.
    pane.type_text("Plan Two");
    pane.wait("the selection on Claude Plan Two", |screen| {
        screen.contains("enter switches to Claude Plan Two")
    });
    // The action menu over the highlighted model: its rows, then the favorite toggle.
    pane.keys(&["Tab"]);
    let menu = pane.wait("the action menu", |screen| {
        screen.contains("Model \u{b7} Claude Plan Two")
            && screen.contains("Set as default for new sessions")
            && screen.contains("Add to favorites")
    });
    println!("--- actions ---\n{menu}");
    pane.keys(&["Down", "Down", "Enter"]);
    pane.wait("the favorite toggle", |screen| {
        screen.contains("Remove from favorites")
    });
    // Esc steps back to the list, a second Esc clears the query: the grouped view, the
    // favorite heading it and still selected.
    pane.keys(&["Escape"]);
    pane.wait("the list", |screen| {
        screen.contains("enter switches to Claude Plan Two")
            && !screen.contains("Remove from favorites")
    });
    pane.keys(&["Escape"]);
    let favorited = pane.wait("the favorites group", |screen| {
        screen.contains("enter switches to Claude Plan Two")
            // The header row ends in the list's scrollbar cell.
            && screen.lines().any(|line| {
                line.trim()
                    .trim_end_matches(['\u{2588}', '\u{2502}'])
                    .trim_end()
                    == "favorites"
            })
            && row(screen, "Claude Plan Two").is_some_and(|line| line.contains("\u{2605}"))
    });
    println!("--- favorited ---\n{favorited}");
    // Switch from the menu: the explicit set_model path, never an implicit one.
    pane.keys(&["Tab"]);
    pane.wait("the action menu again", |screen| {
        screen.contains("Switch this session to it")
    });
    pane.keys(&["Enter"]);
    let switched = pane.wait("the switch", |screen| {
        screen.contains("Model: claude-e2e-plan-two")
    });
    println!("--- switched ---\n{switched}");
    let home = dir.path().join("home");
    let deadline = Instant::now() + WAIT;
    loop {
        let recorded = recorded_switches(&home);
        if recorded.contains(&"anthropic/claude-e2e-plan-two".to_string()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session file never recorded the switch: {recorded:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".prime/agent/settings.json")).expect("settings"),
    )
    .expect("settings parse");
    assert_eq!(
        settings["favoriteModels"],
        json!(["anthropic/claude-e2e-plan-two"])
    );
}

support::on_the_runtime_daemon!(the_picker_filters_by_billing_favorites_and_switches_the_session,);
