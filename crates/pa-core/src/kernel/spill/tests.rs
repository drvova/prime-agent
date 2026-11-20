use super::{
    content_id, SpillCopy, SpillHandle, SpillLookup, SpillOutcome, SpillStore, StoredSpill,
    SPILL_DIR,
};
use std::fmt::Write as _;

/// A store over a fresh temp dir; `cap` bounds its stored bytes.
fn store_in(dir: &tempfile::TempDir, cap: u64) -> SpillStore {
    SpillStore::unregistered(dir.path().join(SPILL_DIR), cap)
}

/// `lines` distinct lines of mixed-width text, about `lines * 40` bytes.
fn output(lines: usize, tag: &str) -> String {
    (0..lines).fold(String::new(), |mut out, n| {
        let _ = writeln!(out, "{tag} line {n:06} · é界🦀 payload");
        out
    })
}

fn stored(text: &str) -> SpillLookup {
    let copy = SpillCopy::of_text(text);
    SpillLookup::Stored(StoredSpill {
        text: text.to_string(),
        total_chars: copy.total_chars,
        lines: copy.lines(),
    })
}

#[tokio::test]
async fn a_megabyte_spill_round_trips_byte_for_byte() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = store_in(&dir, u64::MAX);
    let text = output(30_000, "big");
    assert!(text.len() > 1_000_000);

    let handle = store.spill(SpillCopy::of_text(&text));
    assert_eq!(
        handle,
        SpillHandle {
            id: content_id(&text),
            total_chars: text.chars().count(),
            lines: 30_000,
            stored_chars: text.chars().count(),
            outcome: SpillOutcome::Queued,
        }
    );
    assert_eq!(store.lookup(&handle.id).await, stored(&text));
    let on_disk = std::fs::read(
        dir.path()
            .join(SPILL_DIR)
            .join(format!("{}.txt", handle.id)),
    )
    .expect("spill file");
    assert_eq!(on_disk, text.as_bytes());
}

#[tokio::test]
async fn handles_survive_a_fresh_process() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let text = output(3_000, "resume");
    let id = {
        let store = store_in(&dir, u64::MAX);
        let handle = store.spill(SpillCopy::of_text(&text));
        assert_eq!(store.lookup(&handle.id).await, stored(&text));
        handle.id
    };
    // A restarted daemon or a resumed session opens the directory afresh.
    let reopened = store_in(&dir, u64::MAX);
    assert_eq!(
        (
            reopened.lookup(&id).await,
            reopened.lookup("0123456789").await
        ),
        (stored(&text), SpillLookup::Unknown)
    );
}

#[tokio::test]
async fn the_cap_evicts_oldest_first_and_evicted_handles_say_so() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let texts = [output(40, "a"), output(40, "b"), output(40, "c")];
    // Room for two of the three spills.
    let cap = (texts[0].len() * 2 + texts[0].len() / 2) as u64;
    let store = store_in(&dir, cap);
    let mut ids = Vec::new();
    for text in &texts {
        let handle = store.spill(SpillCopy::of_text(text));
        // Each write settles before the next spill, so the order is fixed.
        assert_eq!(store.lookup(&handle.id).await, stored(text));
        ids.push(handle.id);
    }
    let expected = vec![SpillLookup::Evicted, stored(&texts[1]), stored(&texts[2])];
    let mut answers = Vec::new();
    for id in &ids {
        answers.push(store.lookup(id).await);
    }
    assert_eq!(answers, expected);
    // The eviction is durable: a fresh process answers the same.
    let reopened = store_in(&dir, cap);
    let mut reopened_answers = Vec::new();
    for id in &ids {
        reopened_answers.push(reopened.lookup(id).await);
    }
    assert_eq!(reopened_answers, expected);
    assert!(!dir
        .path()
        .join(SPILL_DIR)
        .join(format!("{}.txt", ids[0]))
        .exists());
}

#[tokio::test]
async fn a_write_that_keeps_failing_answers_not_stored() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    // A file where the store's directory belongs: every write attempt fails.
    std::fs::write(dir.path().join(SPILL_DIR), b"not a directory").expect("blocker file");
    let store = store_in(&dir, u64::MAX);
    let handle = store.spill(SpillCopy::of_text(&output(10, "fail")));
    let answer = store.lookup(&handle.id).await;
    assert!(
        matches!(&answer, SpillLookup::NotStored(reason) if reason.starts_with("the write failed: ")),
        "{answer:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_hanging_disk_never_blocks_a_spill() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let spill_dir = dir.path().join(SPILL_DIR);
    std::fs::create_dir_all(&spill_dir).expect("spill dir");
    let first = output(10, "hang");
    // Opening a FIFO for writing blocks until a reader opens it: the writer
    // thread's temp-file create never returns.
    let fifo = spill_dir.join(format!("{}.tmp", content_id(&first)));
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(status.success());
    let store = store_in(&dir, u64::MAX);

    let started = std::time::Instant::now();
    let hung = store.spill(SpillCopy::of_text(&first));
    // The queue fills behind the hung write; once full, spills are refused
    // at once with an honest answer instead of waiting.
    let refused = (0..64)
        .map(|n| store.spill(SpillCopy::of_text(&output(2, &format!("more{n}")))))
        .find(|handle| handle.outcome != SpillOutcome::Queued)
        .expect("a full queue refuses");
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(
        (hung.outcome, refused.outcome.clone()),
        (
            SpillOutcome::Queued,
            SpillOutcome::NotStored("the spill writer is busy".to_string())
        )
    );
    assert_eq!(
        store.lookup(&refused.id).await,
        SpillLookup::NotStored("the spill writer is busy".to_string())
    );
}

#[test]
fn the_marker_names_the_handle_its_counts_and_the_store_state() {
    let handle = SpillHandle {
        id: "7f3a9c1e04".to_string(),
        total_chars: 48_211,
        lines: 1_204,
        stored_chars: 48_211,
        outcome: SpillOutcome::Queued,
    };
    let partial = SpillHandle {
        total_chars: 9_000_000,
        stored_chars: 8_388_608,
        ..handle.clone()
    };
    let refused = SpillHandle {
        outcome: SpillOutcome::NotStored("the spill writer is busy".to_string()),
        ..handle.clone()
    };
    assert_eq!(
        [handle.marker(31_002), partial.marker(31_002), refused.marker(31_002)],
        [
            "[out:7f3a9c1e04 · 48,211 chars · 1,204 lines · 31,002 chars not shown]".to_string(),
            "[out:7f3a9c1e04 · 9,000,000 chars · 1,204 lines · 31,002 chars not shown · first 8,388,608 chars stored]".to_string(),
            "[out:7f3a9c1e04 · 48,211 chars · 1,204 lines · 31,002 chars not shown · not stored: the spill writer is busy]".to_string(),
        ]
    );
}
