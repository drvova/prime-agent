//! `prime-agent export-jsonl`: a captured session in the agent dir comes back
//! out byte for byte, by id prefix or with `--all` (which keeps the agent
//! dir's layout); a call without a selection is refused. The database path
//! is exercised by every e2e test that reads its sessions back through
//! `support::saved_transcripts`.

use std::path::Path;
use std::process::{Command, Output};

fn export(agent_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .arg("export-jsonl")
        .args(args)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("DO_NOT_TRACK", "1")
        .output()
        .expect("run export-jsonl")
}

#[test]
fn export_jsonl_writes_a_saved_session_back_byte_for_byte() {
    let root = tempfile::tempdir().expect("temp dir");
    let agent_dir = root.path().join("agent");
    let sessions = agent_dir.join("sessions").join("--work--");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    let captured = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pa-daemon/tests/fixtures/attribution-fold-captured.jsonl"),
    )
    .expect("the captured session");
    let header: serde_json::Value = serde_json::from_slice(
        captured
            .split(|byte| *byte == b'\n')
            .next()
            .expect("a header line"),
    )
    .expect("a JSON header");
    let id = header["id"].as_str().expect("a session id");
    std::fs::write(sessions.join("captured.jsonl"), &captured).expect("seed the session");

    let by_id = root.path().join("by-id");
    let one = export(
        &agent_dir,
        &["--session", &id[..8], "--out", &by_id.to_string_lossy()],
    );
    let all = root.path().join("all");
    let every = export(&agent_dir, &["--all", "--out", &all.to_string_lossy()]);
    let refused = export(&agent_dir, &["--out", &all.to_string_lossy()]);
    assert_eq!(
        (
            one.status.code(),
            std::fs::read(by_id.join("captured.jsonl")).ok(),
            every.status.code(),
            std::fs::read(all.join("sessions/--work--/captured.jsonl")).ok(),
            refused.status.code(),
        ),
        (
            Some(0),
            Some(captured.clone()),
            Some(0),
            Some(captured),
            Some(2)
        ),
        "{}{}",
        String::from_utf8_lossy(&one.stderr),
        String::from_utf8_lossy(&every.stderr)
    );
}

/// A test run never writes a real agent dir's database: with HOME at a
/// fresh dir (the agent dir the product would resolve), the store's database
/// lands under the repository's test seam (`.cargo/config.toml`), and no
/// `.prime/agent/agent.db` appears under HOME.
#[test]
fn a_test_run_never_writes_the_home_agent_database() {
    let home = tempfile::tempdir().expect("home");
    let out = home.path().join("out");
    let output = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .args(["export-jsonl", "--all", "--out"])
        .arg(&out)
        .env("HOME", home.path())
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .env("DO_NOT_TRACK", "1")
        .output()
        .expect("run export-jsonl");
    let agent_dir = home.path().join(".prime/agent");
    assert_eq!(
        (
            output.status.code(),
            agent_dir.join("agent.db").exists(),
            pa_core::session::store::database_path(&agent_dir).is_file(),
        ),
        (Some(0), false, true),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
