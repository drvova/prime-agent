use std::path::Path;

use super::*;

/// Two sessions of one daemon recording uses of the same entry at once
/// lose none of them: each read-modify-write holds the ledger's locks.
#[test]
fn concurrent_sessions_recording_at_once_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let key = EntryKey::Skill("deploy-notes".to_string());
    let creator = Recorder::new(dir.path(), "session-a", Path::new("/project"));
    creator
        .record(Event::Created(Creation::New), std::slice::from_ref(&key))
        .unwrap();
    std::thread::scope(|scope| {
        for session in ["session-b", "session-c"] {
            let key = key.clone();
            let recorder = Recorder::new(dir.path(), session, Path::new("/project"));
            scope.spawn(move || {
                for _ in 0..50 {
                    recorder
                        .record(Event::Offered, std::slice::from_ref(&key))
                        .unwrap();
                    recorder
                        .record(Event::Used, std::slice::from_ref(&key))
                        .unwrap();
                }
            });
        }
    });
    let record = load(dir.path()).record(&key).cloned().unwrap();
    let mut recent: Vec<(String, bool, u64)> = record
        .recent
        .iter()
        .map(|mark| (mark.session.clone(), mark.offered, mark.uses))
        .collect();
    recent.sort();
    assert_eq!(
        (
            record.offered_sessions,
            record.offered_elsewhere,
            record.uses,
            record.uses_elsewhere,
            record.used_elsewhere,
            recent
        ),
        (
            2,
            2,
            100,
            100,
            2,
            vec![
                ("session-b".to_string(), true, 50),
                ("session-c".to_string(), true, 50)
            ]
        )
    );
}

/// An entry offered to later sessions that none of them used is dead
/// weight once enough of them have seen it; the creator's own offers and
/// uses never count toward that.
#[test]
fn an_entry_only_its_creator_used_turns_unused_after_later_sessions_pass_it_by() {
    let dir = tempfile::tempdir().unwrap();
    let key = EntryKey::Harness(RefinementKind::Memory, "dev_port".to_string());
    let creator = Recorder::new(dir.path(), "session-a", Path::new("/project"));
    creator
        .record(Event::Created(Creation::New), std::slice::from_ref(&key))
        .unwrap();
    creator
        .record(Event::Offered, std::slice::from_ref(&key))
        .unwrap();
    creator
        .record(Event::Used, std::slice::from_ref(&key))
        .unwrap();
    let mut standings = vec![load(dir.path()).record(&key).unwrap().standing()];
    for session in ["session-b", "session-c", "session-d"] {
        Recorder::new(dir.path(), session, Path::new("/elsewhere"))
            .record(Event::Offered, std::slice::from_ref(&key))
            .unwrap();
        standings.push(load(dir.path()).record(&key).unwrap().standing());
    }
    assert_eq!(
        standings,
        vec![
            Standing::New,
            Standing::New,
            Standing::New,
            Standing::Unused
        ]
    );
}
