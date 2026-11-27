use super::*;

const FORK: StateLayout = StateLayout::Distribution { variant: "fork" };

/// The boot path's contract, modeled on the startup-flush guard (moved from
/// pa-daemon's `history_import.rs`): with the copy hanging on its first
/// file, the hand-off has already returned and announced the import, and no
/// chat has landed yet; released, the import finishes, announces it, and the
/// chats are in the fork's dir. A regression that copies inline reds the
/// hand-off timeout instead of wedging the suite.
#[test]
fn a_hanging_copy_never_blocks_the_boot() {
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(hanging_copy_never_blocks_the_boot());
}

async fn hanging_copy_never_blocks_the_boot() {
    let home = tempfile::tempdir().expect("home");
    let official = StateLayout::Official
        .home_agent_dir(home.path())
        .join("sessions");
    std::fs::create_dir_all(&official).expect("official sessions");
    // The official build's chats are its JSONL files.
    let mut ids: Vec<String> = ["/repo", "/other"]
        .into_iter()
        .enumerate()
        .map(|(index, cwd)| {
            let id = format!("0000000{index}-0000-4000-8000-000000000000");
            let lines = [
                serde_json::json!({ "type": "session", "version": 3, "id": id,
                    "timestamp": "2026-09-23T00:00:00.000Z", "cwd": cwd }),
                serde_json::json!({ "type": "session_info", "id": format!("{id}-1"),
                    "parentId": null, "timestamp": "2026-09-23T00:00:01.000Z",
                    "name": "a chat" }),
            ];
            let text: String = lines.iter().map(|line| line.to_string() + "\n").collect();
            std::fs::write(official.join(format!("{id}.jsonl")), text).expect("official chat");
            id
        })
        .collect();
    ids.sort();
    let fork = FORK.home_agent_dir(home.path());

    let (frames_tx, mut frames) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release) = std::sync::mpsc::channel::<()>();
    let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
    let mut entered_tx = Some(entered_tx);
    // The hand-off runs on its own thread, so a regression to an inline copy reds the
    // bounded wait below instead of wedging the suite.
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let runtime = tokio::runtime::Handle::current();
    let agent_dir = fork.clone();
    let home_dir = home.path().to_path_buf();
    std::thread::spawn(move || {
        let _runtime = runtime.enter();
        let handle = spawn_history_import(
            FORK,
            home_dir,
            agent_dir,
            move |frame| {
                let _ = frames_tx.send(frame);
            },
            move |_| {
                if let Some(entered) = entered_tx.take() {
                    let _ = entered.send(());
                    // The copy hangs here until the test releases it.
                    let _ = release.recv();
                }
            },
        );
        let _ = handle_tx.send(handle);
    });
    let hand_off = handle_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the hand-off returns while the copy still hangs");

    tokio::time::timeout(std::time::Duration::from_secs(2), entered)
        .await
        .expect("the copy reached its first file")
        .expect("entered");
    let started = frames.recv().await.expect("started frame");
    assert_eq!(
        started,
        serde_json::json!({"type": "history_import", "phase": "started", "sessions": 2})
    );
    assert!(
        !fork.join("sessions").exists(),
        "no chat landed while the copy hangs"
    );

    release_tx.send(()).expect("release");
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), hand_off)
        .await
        .expect("the released import settles")
        .expect("join");
    let finished = frames.recv().await.expect("finished frame");
    assert_eq!(
        finished,
        serde_json::json!({"type": "history_import", "phase": "finished", "sessions": 2, "kept": 0})
    );
    assert!(
        matches!(&outcome, HistoryImportOutcome::Completed(report) if report.chats_imported == 2),
        "{outcome:?}"
    );
    // The resume picker's scan over the fork's own dir lists the imported chats.
    let mut listed: Vec<String> = crate::session::catalog::list_sessions(&fork.join("sessions"))
        .into_iter()
        .map(|info| info.id)
        .collect();
    listed.sort();
    assert_eq!(listed, ids);
}
