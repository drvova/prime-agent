//! A cell's cut output pages out to the session's spill store: the result
//! carries the spill's handle where the truncation marker was, the store
//! holds the whole text, and the write never delays the result.
#![cfg(unix)]
// Kernel manager futures follow the crate's existing integration-test disposition.
#![allow(clippy::large_futures)]

mod script;

use script::write_executable;
use sha2::Digest as _;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, HostRequestHandlers, KernelManagerOptions, KernelShutdownOptions,
};
use pa_core::kernel::spill::SpillStore;

/// Each execute's code is a JSON list of stdout chunks to print.
const FAKE_RUNTIME: &str = r#"#!/usr/bin/env python3
import json
import sys

print(json.dumps({"event": "ready", "protocol": 3, "python": "3.13.0"}), flush=True)
for line in sys.stdin:
    req = json.loads(line)
    if req.get("type") == "execute":
        for chunk in json.loads(req["code"]):
            print(json.dumps({"event": "stdout", "id": req["id"], "text": chunk}), flush=True)
    print(json.dumps({"event": "done", "id": req.get("id"), "status": "ok"}), flush=True)
"#;

const CAP: usize = 65_536;

fn manager(dir: &Path) -> ReplKernelManager {
    let python = dir.join("fake-kernel");
    write_executable(&python, FAKE_RUNTIME);
    ReplKernelManager::new(KernelManagerOptions {
        on_background_cell: None,
        python: Some(python),
        cwd: Some(dir.to_path_buf()),
        env: HashMap::new(),
        session_id: Some("spill-test".to_string()),
        host_handlers: HostRequestHandlers::new(),
        python_skills: Vec::new(),
        snapshot: None,
        bootstrap_code: None,
        stderr_log_path: None,
        on_background_work_settled: None,
    })
}

/// About 1 MB of ASCII lines.
fn big_output() -> String {
    (0..25_000).fold(String::new(), |mut out, n| {
        let _ = writeln!(out, "line {n:06} of a long build log, kept whole");
        out
    })
}

fn spill_id(text: &str) -> String {
    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))[..10].to_string()
}

/// The result's stdout: the stream's head and tail around the handle.
fn expected_stdout(text: &str) -> String {
    let head = &text[..CAP - CAP / 4];
    let tail = &text[text.len() - CAP / 4..];
    let lines = text.lines().count();
    let group = |n: usize| {
        let digits = n.to_string();
        let mut out = String::new();
        for (at, digit) in digits.chars().enumerate() {
            if at > 0 && (digits.len() - at).is_multiple_of(3) {
                out.push(',');
            }
            out.push(digit);
        }
        out
    };
    format!(
        "{head}\n[out:{} · {} chars · {} lines · {} chars not shown]\n{tail}",
        spill_id(text),
        group(text.len()),
        group(lines),
        group(text.len() - CAP),
    )
}

async fn run_cell(
    manager: &ReplKernelManager,
    text: &str,
    spill: SpillStore,
) -> (ExecuteStatus, String) {
    let chunks: Vec<&str> = text
        .as_bytes()
        .chunks(60_000)
        .map(|chunk| std::str::from_utf8(chunk).expect("ascii"))
        .collect();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        manager.execute(
            &serde_json::to_string(&chunks).expect("encode chunks"),
            ExecuteOptions {
                spill: Some(spill),
                ..ExecuteOptions::default()
            },
        ),
    )
    .await
    .expect("the cell result must not wait on the spill write")
    .expect("execute must succeed");
    (result.status, result.stdout)
}

/// Wait for the writer's index row naming `id` as stored (observable readiness).
async fn wait_stored(spill_dir: &Path, id: &str) {
    let needle = format!("\"id\":\"{id}\"");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let index = std::fs::read_to_string(spill_dir.join("index.jsonl")).unwrap_or_default();
            if index
                .lines()
                .any(|line| line.contains(&needle) && line.contains("\"state\":\"stored\""))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the spill must be stored");
}

async fn shutdown(manager: &ReplKernelManager) {
    tokio::time::timeout(
        Duration::from_secs(10),
        manager.shutdown(KernelShutdownOptions::default()),
    )
    .await
    .expect("shutdown must finish")
    .expect("shutdown must succeed");
}

#[tokio::test]
async fn a_megabyte_of_stdout_pages_out_whole_behind_its_handle() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let session_dir = dir.path().join("session");
    let manager = manager(dir.path());
    manager
        .start(KernelStartOptions::default())
        .await
        .expect("fake runtime must start");
    let text = big_output();
    assert!(text.len() > 1_000_000);

    let outcome = run_cell(&manager, &text, SpillStore::for_session_dir(&session_dir)).await;
    assert_eq!(outcome, (ExecuteStatus::Ok, expected_stdout(&text)));

    let spill_dir = session_dir.join("spill");
    let id = spill_id(&text);
    wait_stored(&spill_dir, &id).await;
    let stored = std::fs::read(spill_dir.join(format!("{id}.txt"))).expect("spill file");
    assert_eq!(stored, text.as_bytes());
    shutdown(&manager).await;
}

/// The cell runs on its own runtime thread: a write on the settle path would
/// block that runtime outright, and this thread still sees the result late.
#[test]
fn a_hanging_spill_write_never_delays_the_cell_result() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let session_dir = dir.path().join("session");
    let spill_dir = session_dir.join("spill");
    std::fs::create_dir_all(&spill_dir).expect("spill dir");
    let text = big_output();
    // The writer's temp file is a FIFO nobody reads: opening it for writing
    // blocks forever, like a hung disk.
    let fifo = spill_dir.join(format!("{}.tmp", spill_id(&text)));
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(status.success());

    let (sender, receiver) = std::sync::mpsc::channel();
    let cell_text = text.clone();
    let root = dir.path().to_path_buf();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let outcome = runtime.block_on(async {
            let manager = manager(&root);
            manager
                .start(KernelStartOptions::default())
                .await
                .expect("fake runtime must start");
            let outcome = run_cell(
                &manager,
                &cell_text,
                SpillStore::for_session_dir(&session_dir),
            )
            .await;
            shutdown(&manager).await;
            outcome
        });
        let _ = sender.send(outcome);
    });
    let outcome = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("the cell result must not wait on the spill write");
    assert_eq!(outcome, (ExecuteStatus::Ok, expected_stdout(&text)));
    drop(dir);
}
