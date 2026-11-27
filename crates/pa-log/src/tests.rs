use std::path::{Path, PathBuf};

use pa_types::event_log::{NewEvent, Topic, TopicFilter, Trust};
use pa_types::wire2::Since;
use serde_json::json;

use super::{EventRetention, ImportOutcome, Log, DEFAULT_EVENT_RETENTION};

fn workspace_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn open(dir: &Path) -> Log {
    Log::open(dir.join("agent.db"), DEFAULT_EVENT_RETENTION).unwrap()
}

/// Captured session files go in and come back out as the same bytes, torn
/// last write and missing final newline included; a file without a session
/// header is skipped whole.
#[test]
fn session_files_round_trip_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let torn = dir.path().join("torn.jsonl");
    std::fs::write(
        &torn,
        "{\"type\":\"session\",\"version\":3,\"id\":\"s-torn\",\"timestamp\":\"t\",\"cwd\":\"/w\"}\n\
         {\"type\":\"message\",\"id\":\"m1\",\"parentId\":null}\n{\"type\":\"mess",
    )
    .unwrap();
    let files = [
        workspace_file("pa-daemon/tests/fixtures/attribution-fold-captured.jsonl"),
        workspace_file("pa-core/tests/fixtures/run_metrics/synthetic-two-runs.jsonl"),
        torn,
    ];
    let log = open(dir.path());
    let mut exported = Vec::new();
    for file in &files {
        let ImportOutcome::Imported { session, .. } =
            log.import_session_file(file).wait_blocking().unwrap()
        else {
            panic!("{} was not imported", file.display());
        };
        let mut out = Vec::new();
        log.export_session(&session, &mut out).unwrap();
        exported.push(out);
    }
    let headerless =
        workspace_file("pa-core/tests/fixtures/compaction_retention/synthetic-correction.jsonl");
    assert_eq!(
        (
            exported,
            log.import_session_file(&headerless)
                .wait_blocking()
                .unwrap(),
            log.entries("s-torn", None, 10)
                .unwrap()
                .into_iter()
                .map(|entry| (entry.seq, entry.kind, entry.entry_id))
                .collect::<Vec<_>>(),
        ),
        (
            files
                .iter()
                .map(|file| std::fs::read(file).unwrap())
                .collect::<Vec<_>>(),
            ImportOutcome::Skipped {
                reason: format!(
                    "{} does not start with a session header",
                    headerless.display()
                )
            },
            vec![
                (0, Some("session".to_string()), Some("s-torn".to_string())),
                (1, Some("message".to_string()), Some("m1".to_string())),
                (2, None, None),
            ],
        )
    );
}

/// Importing again changes nothing for an unchanged file, replaces the lines
/// of a file that grew, and refuses a second file holding a stored session id;
/// an append lands after the imported lines.
#[test]
fn reimports_are_idempotent_and_appends_follow_the_import() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.jsonl");
    let header = "{\"type\":\"session\",\"version\":3,\"id\":\"s-a\",\"timestamp\":\"t\"}";
    std::fs::write(&file, format!("{header}\n")).unwrap();
    let copy = dir.path().join("copy.jsonl");
    std::fs::write(&copy, format!("{header}\n")).unwrap();
    let log = open(dir.path());
    let mut outcomes = vec![log.import_session_file(&file).wait_blocking().unwrap()];
    outcomes.push(log.import_session_file(&file).wait_blocking().unwrap());
    std::fs::write(
        &file,
        format!("{header}\n{{\"type\":\"message\",\"id\":\"m1\"}}\n"),
    )
    .unwrap();
    outcomes.push(log.import_session_file(&file).wait_blocking().unwrap());
    outcomes.push(log.import_session_file(&copy).wait_blocking().unwrap());
    let appended = log
        .append_entry(
            "s-a",
            "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\"}",
        )
        .wait_blocking()
        .unwrap();
    let mut out = Vec::new();
    log.export_session("s-a", &mut out).unwrap();
    assert_eq!(
        (outcomes, appended, String::from_utf8(out).unwrap()),
        (
            vec![
                ImportOutcome::Imported {
                    session: "s-a".to_string(),
                    entries: 1
                },
                ImportOutcome::Unchanged {
                    session: "s-a".to_string()
                },
                ImportOutcome::Replaced {
                    session: "s-a".to_string(),
                    entries: 2
                },
                ImportOutcome::Skipped {
                    reason: format!(
                        "session s-a of {} is already stored, from {}",
                        copy.display(),
                        file.display()
                    )
                },
            ],
            2,
            format!(
                "{header}\n{{\"type\":\"message\",\"id\":\"m1\"}}\n\
                 {{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\"}}\n"
            ),
        )
    );
}

/// Events appended from many threads at once get distinct, increasing
/// sequence numbers; reading by filter returns exactly the matching events,
/// oldest first, after a sequence number or as the last few.
#[test]
fn events_are_stored_in_order_and_read_by_topic_filter() {
    let dir = tempfile::tempdir().unwrap();
    let log = std::sync::Arc::new(open(dir.path()));
    let topics = [
        "agent/a/done",
        "agent/b/done",
        "agent/a/inbox",
        "timer/nightly",
    ];
    let threads: Vec<_> = (0..8)
        .map(|thread| {
            let log = std::sync::Arc::clone(&log);
            std::thread::spawn(move || {
                (0..25)
                    .map(|index| {
                        log.append_event(NewEvent {
                            topic: Topic::new(topics[(thread + index) % topics.len()]).unwrap(),
                            source: format!("thread-{thread}"),
                            session: None,
                            payload: json!({ "thread": thread, "index": index }),
                            trust: Trust::Owner,
                            cause: None,
                            reply: None,
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|receipt| receipt.wait_blocking().unwrap().seq)
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seqs: Vec<u64> = threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect();
    seqs.sort_unstable();
    let everything = log
        .events(
            &TopicFilter::new("#").unwrap(),
            Since::After { seq: 0 },
            1000,
        )
        .unwrap();
    let filter = TopicFilter::new("agent/+/done").unwrap();
    let done: Vec<u64> = everything
        .iter()
        .filter(|event| filter.matches(&event.event.topic))
        .map(|event| event.seq)
        .collect();
    let read = |since: Since, limit: usize| -> Vec<u64> {
        log.events(&filter, since, limit)
            .unwrap()
            .into_iter()
            .map(|event| event.seq)
            .collect()
    };
    assert_eq!(
        (
            seqs.clone(),
            everything.iter().map(|event| event.seq).collect::<Vec<_>>(),
            read(Since::After { seq: done[9] }, 1000),
            read(Since::Tail { count: 5 }, 1000),
            read(Since::After { seq: 0 }, 3),
            read(Since::Now, 1000),
        ),
        (
            (1..=200).collect::<Vec<u64>>(),
            (1..=200).collect::<Vec<u64>>(),
            done[10..].to_vec(),
            done[done.len() - 5..].to_vec(),
            done[..3].to_vec(),
            Vec::new(),
        )
    );
}

/// An event expires after its retention, and pruning deletes only expired events.
#[test]
fn pruning_deletes_only_expired_events() {
    let dir = tempfile::tempdir().unwrap();
    let log = Log::open(
        dir.path().join("agent.db"),
        EventRetention {
            keep: Some(std::time::Duration::from_secs(60)),
        },
    )
    .unwrap();
    let event = |topic: &str| NewEvent {
        topic: Topic::new(topic).unwrap(),
        source: "test".to_string(),
        session: None,
        payload: json!({}),
        trust: Trust::Untrusted,
        cause: None,
        reply: None,
    };
    let first = log.append_event(event("a")).wait_blocking().unwrap();
    let second = log.append_event(event("b")).wait_blocking().unwrap();
    let pruned_early = log.prune_events(first.ts).wait_blocking().unwrap();
    let pruned_late = log
        .prune_events(second.ts + 60_000)
        .wait_blocking()
        .unwrap();
    assert_eq!(
        (
            pruned_early,
            pruned_late,
            log.events(&TopicFilter::new("#").unwrap(), Since::After { seq: 0 }, 10)
                .unwrap()
        ),
        (0, 2, Vec::new())
    );
}

/// A database a newer build migrated further is refused, not written.
#[test]
fn a_newer_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let refused = Log::open(&path, DEFAULT_EVENT_RETENTION)
        .err()
        .map(|error| format!("{error:#}"));
    assert_eq!(
        refused,
        Some(format!(
            "migrate {}: the database is at schema version 99, newer than this build's 1: it \
             was written by a newer Prime Agent",
            path.display()
        ))
    );
}

/// Two processes opening one new database at once (a daemon and the CLI
/// command that reads it) both open it: only one of them applies each
/// migration, and the other finds it applied.
#[test]
fn concurrent_opens_of_a_new_database_both_succeed() {
    let dir = tempfile::tempdir().unwrap();
    for attempt in 0..40 {
        let path = dir.path().join(format!("agent-{attempt}.db"));
        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let opens: Vec<_> = (0..2)
            .map(|_| {
                let (path, start) = (path.clone(), std::sync::Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    Log::open(path, DEFAULT_EVENT_RETENTION)
                        .map(drop)
                        .map_err(|error| format!("{error:#}"))
                })
            })
            .collect();
        let results: Vec<_> = opens.into_iter().map(|open| open.join().unwrap()).collect();
        assert_eq!(results, vec![Ok(()), Ok(())], "attempt {attempt}");
    }
}
