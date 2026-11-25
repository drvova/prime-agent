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

/// The database holds every transcript: it and its WAL are the user's own.
#[cfg(unix)]
#[test]
fn the_database_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let log = open(dir.path());
    log.set_meta("probe", "1").wait_blocking().unwrap();
    let modes: Vec<u32> = ["agent.db", "agent.db-wal"]
        .iter()
        .map(|name| {
            std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o077
        })
        .collect();
    assert_eq!(modes, vec![0, 0]);
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
            "migrate {}: the database is at schema version 99, newer than this build's 3: it \
             was written by a newer Prime Agent",
            path.display()
        ))
    );
}

fn transcript(locator: &str, bytes: &[u8]) -> super::NewTranscript {
    super::NewTranscript {
        locator: locator.to_string(),
        dir: "/sessions".to_string(),
        bytes: bytes.to_vec(),
        created_ns: 1,
        modified_ns: 2,
        source: None,
    }
}

/// A transcript's bytes come back exactly, whole or read from any byte
/// position (captured sessions, a torn last write, bytes that are not
/// UTF-8), and its first line's session id is indexed.
#[test]
fn transcripts_read_back_byte_for_byte_from_any_position() {
    use super::PutMode;
    let dir = tempfile::tempdir().unwrap();
    let log = open(dir.path());
    let mut torn =
        b"{\"type\":\"session\",\"id\":\"s-torn\"}\n\xff\xfe not utf-8\n\n{\"type\":\"mess"
            .to_vec();
    torn.extend_from_slice(b"age\"");
    let originals = [
        std::fs::read(workspace_file(
            "pa-daemon/tests/fixtures/attribution-fold-captured.jsonl",
        ))
        .unwrap(),
        std::fs::read(workspace_file(
            "pa-core/tests/fixtures/run_metrics/synthetic-two-runs.jsonl",
        ))
        .unwrap(),
        torn,
    ];
    let mut seen = Vec::new();
    for (index, bytes) in originals.iter().enumerate() {
        let row = log
            .put_transcript(
                transcript(&format!("/sessions/{index}.jsonl"), bytes),
                PutMode::Replace,
            )
            .wait_blocking()
            .unwrap()
            .unwrap();
        let whole = log
            .transcript_bytes(row.id, 0, usize::MAX)
            .unwrap()
            .unwrap();
        // Every position reads the bytes from there on, a few at a time.
        let from_everywhere = (0..=bytes.len()).step_by(7).all(|from| {
            let mut read = Vec::new();
            let mut at = from as u64;
            loop {
                let chunk = log.transcript_bytes(row.id, at, 5).unwrap().unwrap();
                if chunk.bytes.is_empty() {
                    break;
                }
                at += chunk.bytes.len() as u64;
                read.extend(chunk.bytes);
            }
            read == bytes[from..]
        });
        seen.push((
            whole.bytes == *bytes,
            from_everywhere,
            row.len,
            row.session.is_some(),
        ));
    }
    assert_eq!(
        seen,
        originals
            .iter()
            .map(|bytes| (true, true, bytes.len() as u64, true))
            .collect::<Vec<_>>()
    );
}

/// Appends keep the transcript's identity and match a JSONL file's append:
/// a torn last line is ended before new lines (`NewLine`), or finished in
/// place (`AsIs`). A replace gives a new identity, an import never replaces,
/// and a move or remove frees the locator.
#[test]
fn transcript_appends_replaces_moves_and_removes() {
    use super::{AppendMode, MoveOutcome, PutMode};
    let dir = tempfile::tempdir().unwrap();
    let log = open(dir.path());
    let put = |bytes: &[u8], mode| {
        log.put_transcript(transcript("/sessions/a.jsonl", bytes), mode)
            .wait_blocking()
            .unwrap()
    };
    let append = |bytes: &[u8], mode| {
        log.append_transcript("/sessions/a.jsonl", bytes.to_vec(), mode, 3)
            .wait_blocking()
            .unwrap()
            .unwrap()
    };
    let read = |id| {
        log.transcript_bytes(id, 0, usize::MAX)
            .unwrap()
            .unwrap()
            .bytes
    };
    let first = put(
        b"{\"type\":\"session\",\"id\":\"s\"}\n{\"torn",
        PutMode::Replace,
    )
    .unwrap();
    let guarded = append(b"{\"a\":1}\n", AppendMode::NewLine);
    let finished = append(b"\":2}\n{\"b\":", AppendMode::AsIs);
    let completed = append(b"3}\n", AppendMode::AsIs);
    let unchanged = append(b"", AppendMode::NewLine);
    let after_appends = read(first.id);
    let kept = put(b"other\n", PutMode::IfAbsent);
    let replaced = put(b"{\"type\":\"session\",\"id\":\"t\"}\n", PutMode::Replace).unwrap();
    let old_identity = log.transcript_bytes(first.id, 0, 1).unwrap();
    let moved = log
        .move_transcript("/sessions/a.jsonl", "/archive/a.jsonl", "/archive")
        .wait_blocking()
        .unwrap();
    let missing = log
        .move_transcript("/sessions/a.jsonl", "/archive/b.jsonl", "/archive")
        .wait_blocking()
        .unwrap();
    let removed = log
        .remove_transcript("/archive/a.jsonl")
        .wait_blocking()
        .unwrap();
    let removed_again = log
        .remove_transcript("/archive/a.jsonl")
        .wait_blocking()
        .unwrap();
    assert_eq!(
        (
            [guarded.id, finished.id, completed.id, unchanged.id].map(|id| id == first.id),
            String::from_utf8(after_appends).unwrap(),
            (completed.lines, completed.version, unchanged.version),
            kept,
            (replaced.id > first.id, replaced.session.as_deref()),
            old_identity,
            matches!(moved, MoveOutcome::Moved(ref row) if row.id == replaced.id && row.dir == "/archive"),
            missing,
            (removed, removed_again, log.all_transcripts().unwrap()),
        ),
        (
            [true; 4],
            "{\"type\":\"session\",\"id\":\"s\"}\n{\"torn\n{\"a\":1}\n\":2}\n{\"b\":3}\n"
                .to_string(),
            (5, 3, 3),
            None,
            (true, Some("t")),
            None,
            true,
            MoveOutcome::Missing,
            (true, false, Vec::new()),
        )
    );
}

fn imported(from: &str) -> super::Origin {
    super::Origin::Imported {
        from: from.to_string(),
        sha256: format!("sha-of-{from}"),
    }
}

fn source(id: &str, kind: super::SourceKind, claim: Option<&str>) -> super::SourceRow {
    super::SourceRow {
        id: id.to_string(),
        kind,
        spec: format!("{{\"id\":\"{id}\"}}"),
        claim: claim.map(str::to_string),
    }
}

/// A partition's sources and subscriptions come back in the order they were
/// stored; an import stores a partition only while it is not held, and a
/// write that fails (two sources with one id) stores nothing, not even the
/// partition's record.
#[test]
fn source_partitions_are_replaced_whole_and_imported_once() {
    use super::{Origin, SourceKind, SourceSet, SubscriptionRow};
    let dir = tempfile::tempdir().unwrap();
    let log = open(dir.path());
    let file_set = SourceSet {
        sources: vec![
            source("z", SourceKind::Timer, Some("{\"id\":\"d1\"}")),
            source("a", SourceKind::Timer, None),
        ],
        subscriptions: Vec::new(),
    };
    let first_import = log
        .replace_sources("cron:/a.json", file_set.clone(), imported("/a.json"))
        .wait_blocking()
        .unwrap();
    let written = SourceSet {
        sources: vec![source("a", SourceKind::Bus, None)],
        subscriptions: vec![SubscriptionRow {
            id: "s1".to_string(),
            source: "a".to_string(),
            spec: "{}".to_string(),
        }],
    };
    let second_import = log
        .replace_sources("cron:/a.json", written.clone(), imported("/a.json"))
        .wait_blocking()
        .unwrap();
    let after_imports = log.sources("cron:/a.json").unwrap();
    let write = log
        .replace_sources("cron:/a.json", written.clone(), Origin::Written)
        .wait_blocking()
        .unwrap();
    let duplicate = SourceSet {
        sources: vec![
            source("x", SourceKind::Timer, None),
            source("x", SourceKind::Timer, None),
        ],
        subscriptions: Vec::new(),
    };
    let refused = log
        .replace_sources("cron:/b.json", duplicate, Origin::Written)
        .wait_blocking()
        .is_err();
    let partition = log.partition("cron:/a.json").unwrap().unwrap();
    assert_eq!(
        (
            first_import,
            second_import,
            after_imports,
            write,
            log.sources("cron:/a.json").unwrap(),
            refused,
            log.partition("cron:/b.json").unwrap(),
            (partition.imported_from, partition.source_sha256),
        ),
        (
            true,
            false,
            file_set,
            true,
            written,
            true,
            None,
            (
                Some("/a.json".to_string()),
                Some("sha-of-/a.json".to_string())
            ),
        )
    );
}

/// Values and crash records follow the same partition rules: an import of a
/// held partition changes nothing, a write always lands, crash records keep
/// their order, and partitions are listed by scope prefix.
#[test]
fn values_and_crash_records_are_kept_by_partition() {
    use super::Origin;
    let dir = tempfile::tempdir().unwrap();
    let log = open(dir.path());
    let put = |scope: &str, value: &str, origin: Origin| {
        log.put_value(scope, "document", value.to_string(), origin)
            .wait_blocking()
            .unwrap()
    };
    let value_answers = [
        put("harness:/h.json", "from the file", imported("/h.json")),
        put("harness:/h.json", "imported again", imported("/h.json")),
        put("harness:/h.json", "written", Origin::Written),
        put("harness:/other.json", "only written", Origin::Written),
    ];
    let append = |records: &[&str], origin: Origin| {
        log.append_crashes(
            "crashes:/c.jsonl",
            records.iter().map(ToString::to_string).collect(),
            origin,
        )
        .wait_blocking()
        .unwrap()
    };
    let crash_answers = [
        append(&["{\"n\":1}", "{\"n\":2}"], imported("/c.jsonl")),
        append(&["{\"n\":9}"], imported("/c.jsonl")),
        append(&["{\"n\":3}"], Origin::Written),
    ];
    assert_eq!(
        (
            value_answers,
            log.value("harness:/h.json", "document").unwrap(),
            log.value("harness:/h.json", "other key").unwrap(),
            crash_answers,
            log.crashes("crashes:/c.jsonl")
                .unwrap()
                .into_iter()
                .map(|row| row.record)
                .collect::<Vec<_>>(),
            log.partitions("harness:")
                .unwrap()
                .into_iter()
                .map(|partition| partition.scope)
                .collect::<Vec<_>>(),
        ),
        (
            [true, false, true, true],
            Some("written".to_string()),
            None,
            [true, false, true],
            vec![
                "{\"n\":1}".to_string(),
                "{\"n\":2}".to_string(),
                "{\"n\":3}".to_string()
            ],
            vec![
                "harness:/h.json".to_string(),
                "harness:/other.json".to_string()
            ],
        )
    );
}
