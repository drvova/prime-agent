//! `(bash command (hash 'elevated #t))` on real Windows, through the built
//! binary: the session starts its elevated helper (`prime-agent
//! --elevation-helper ...`, through `ShellExecuteExW`'s `runas`), which runs
//! the session's elevated commands with their output piped back.
//!
//! GitHub's Windows runners run as Administrator with UAC off, so a session
//! there takes the already-elevated shortcut and never starts the helper.
//! The debug seam `PRIME_AGENT_ELEVATION_ALWAYS_ASK` turns the shortcut
//! off; `runas` with UAC off then starts the helper with no prompt, and
//! everything after the prompt is the code a user's approval leads to:
//! the pipe and its access list, the pid checks both ways, the helper's job
//! objects, cancellation, and the idle exit (shortened with
//! `PRIME_AGENT_ELEVATION_IDLE_MS`). A declined prompt cannot happen with
//! UAC off. The windows-runtime-triage workflow runs this file.
#![cfg(windows)]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

/// A command run by the helper prints who it runs as and which process
/// started it: the helper, when it is one, starts the session's programs.
const PROBE: &str = r#"$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$parent = (Get-CimInstance Win32_Process -Filter "ProcessId=$PID").ParentProcessId
$line = (Get-CimInstance Win32_Process -Filter "ProcessId=$parent").CommandLine
Write-Output "admin=$admin helper=$($line -like '*--elevation-helper*') parent=$parent"
exit 7
"#;

/// The cell that runs [`PROBE`] elevated.
const RUN_PROBE: &str = r#"(bash "powershell -NoProfile -ExecutionPolicy Bypass -File probe.ps1" (hash 'elevated #t 'wait-ms 300000))"#;

/// Run `cells` as one print-mode turn of the built binary in `home` (also
/// its working directory, holding [`PROBE`]) with the elevated shortcut off
/// and `extra` in its environment; the turn's receipt, which the cells write
/// as JSON to `receipt.json`.
fn elevated_turn(home: &Path, cells: &[&str], extra: &[(&str, &str)]) -> Value {
    std::fs::write(home.join("probe.ps1"), PROBE).expect("write the probe");
    let mut responses: Vec<Value> = cells
        .iter()
        .map(|cell| {
            json!({ "content": [{ "type": "toolCall", "name": "eval",
                "arguments": { "code": cell, "timeout": 600 } }] })
        })
        .collect();
    responses.push(json!({ "text": "elevation checked" }));
    let output = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .args(["--mode", "json", "-p", "run the elevated checks"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PRIME_AGENT_CODING_AGENT_DIR", home.join("agent"))
        .env(
            "PRIME_AGENT_FAUX_SCRIPT",
            json!({ "responses": responses }).to_string(),
        )
        .env("PRIME_AGENT_ELEVATION_ALWAYS_ASK", "1")
        .envs(extra.iter().copied())
        .env_remove("PRIME_AGENT_SESSION_DIR")
        .env_remove("PRIME_AGENT_CODING_AGENT_SESSION_DIR")
        .env_remove("PRIME_AGENT_DAEMON_SOCKET")
        .env_remove("PRIME_API_KEY")
        .current_dir(home)
        .output()
        .expect("run prime-agent");
    let receipt = std::fs::read_to_string(home.join("receipt.json")).unwrap_or_default();
    serde_json::from_str(&receipt).unwrap_or_else(|_| {
        panic!(
            "no receipt (exit {:?})\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// [`PROBE`]'s line as its fields (`admin`, `helper`, `parent`).
fn probed(output: &Value) -> HashMap<String, String> {
    output
        .as_str()
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// An elevated command runs in the helper, with administrator rights, its
/// output captured and its exit code returned; a second one is served by
/// the same helper without asking again; and killing an elevated job ends
/// the program it started, which this process's own rights could not.
#[test]
fn the_elevated_helper_runs_captures_reuses_and_cancels() {
    let home = tempfile::tempdir().expect("home");
    let cell = format!(
        r#"(define probe-1 {RUN_PROBE})
(define probe-2 {RUN_PROBE})
(define job (bash "ping -n 600 127.0.0.1 > /dev/null & echo pid=$!; wait" (hash 'elevated #t 'background #t)))
(define (started tries)
  (let ([out (bash-output job)])
    (if (or (string-contains? out "\n") (= tries 0))
        out
        (begin (bash-wait job 1) (started (- tries 1))))))
(define pid (trim (last (split-many (started 120) "pid="))))
(define (alive? before)
  (trim (hash-ref (bash (string-append "powershell -NoProfile -Command \"" before "[bool](Get-Process -Id " pid " -ErrorAction SilentlyContinue)\"") (hash 'wait-ms 300000)) 'output)))
(define alive-before (alive? ""))
(define killed (bash-kill job))
(define ended (bash-wait job 60))
(define alive-after (alive? (string-append "Wait-Process -Id " pid " -Timeout 30 -ErrorAction SilentlyContinue; ")))
(fs-write {receipt:?} (value->jsexpr-string (hash
  'first-exit (hash-ref probe-1 'exit-code) 'first (hash-ref probe-1 'output) 'second (hash-ref probe-2 'output)
  'alive-before alive-before 'killed killed 'ended-running (hash-ref ended 'running) 'alive-after alive-after)))"#,
        receipt = home.path().join("receipt.json").display().to_string(),
    );
    let receipt = elevated_turn(home.path(), &[&cell], &[]);
    let (first, second) = (probed(&receipt["first"]), probed(&receipt["second"]));
    assert_eq!(
        (
            receipt["first-exit"].clone(),
            first.get("admin").cloned(),
            first.get("helper").cloned(),
            first.get("parent") == second.get("parent"),
            receipt["alive-before"].clone(),
            receipt["killed"].clone(),
            receipt["ended-running"].clone(),
            receipt["alive-after"].clone(),
        ),
        (
            json!(7),
            Some("True".to_string()),
            Some("True".to_string()),
            true,
            json!("True"),
            json!(true),
            json!(false),
            json!("False"),
        ),
        "receipt: {receipt}"
    );
}

/// A helper no elevated command has used for its idle time ends, and the
/// next elevated command starts a new one.
#[test]
fn an_idle_elevated_helper_ends_and_the_next_command_starts_another() {
    let home = tempfile::tempdir().expect("home");
    let cell = format!(
        r#"(define probe-1 {RUN_PROBE})
(define helper-pid (let ([field (list-ref (split-whitespace (hash-ref probe-1 'output)) 2)])
  (substring field 7 (string-length field))))
(define exited (bash (string-append "powershell -NoProfile -Command \"Wait-Process -Id " helper-pid " -Timeout 120 -ErrorAction SilentlyContinue; -not [bool](Get-Process -Id " helper-pid " -ErrorAction SilentlyContinue)\"") (hash 'wait-ms 300000)))
(define probe-2 {RUN_PROBE})
(fs-write {receipt:?} (value->jsexpr-string (hash
  'first (hash-ref probe-1 'output) 'exited (trim (hash-ref exited 'output)) 'second (hash-ref probe-2 'output))))"#,
        receipt = home.path().join("receipt.json").display().to_string(),
    );
    let receipt = elevated_turn(
        home.path(),
        &[&cell],
        &[("PRIME_AGENT_ELEVATION_IDLE_MS", "2000")],
    );
    let (first, second) = (probed(&receipt["first"]), probed(&receipt["second"]));
    assert_eq!(
        (
            first.get("helper").cloned(),
            receipt["exited"].clone(),
            second.get("helper").cloned(),
            first.get("parent") != second.get("parent"),
        ),
        (
            Some("True".to_string()),
            json!("True"),
            Some("True".to_string()),
            true,
        ),
        "receipt: {receipt}"
    );
}
