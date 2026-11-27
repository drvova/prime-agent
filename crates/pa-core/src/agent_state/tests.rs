//! Each store's import: a file shaped as the store wrote it before the move
//! (the store's own bytes, written out by the export) imports whole and
//! reads back as the store kept it, its original left read-only; a second
//! import changes nothing; a file that does not parse is skipped and
//! reported, nothing of it is stored, and it stays writable.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::import::{import_state_file, StateImport};
use super::{kept_bytes, process_log, StateFile};

/// Write the bytes the store keeps for `written` as an original file at
/// `original` (as a build before the move left it), import it twice, and
/// import `corrupt` (written at a third path): the two answers, the corrupt
/// import's refusal and whether its partition exists, whether the original
/// is still byte for byte what was written, and whether the original and
/// the corrupt file are read-only.
fn import_round(
    file: StateFile,
    written: &Path,
    original: &Path,
    corrupt: &str,
) -> (
    Vec<Result<StateImport, String>>,
    bool,
    bool,
    bool,
    [bool; 2],
) {
    let log = process_log().expect("the test process keeps a database");
    let bytes = kept_bytes(file, written).expect("the store kept its state");
    std::fs::create_dir_all(original.parent().unwrap()).unwrap();
    std::fs::write(original, &bytes).unwrap();
    let answers = vec![
        import_state_file(log, file, original),
        import_state_file(log, file, original),
    ];
    let damaged = original.with_file_name(format!(
        "damaged-{}",
        original.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&damaged, corrupt).unwrap();
    let refused = import_state_file(log, file, &damaged).is_err();
    let damaged_held = log.partition(&file.scope(&damaged)).unwrap().is_some();
    let read_only = |path: &Path| std::fs::metadata(path).unwrap().permissions().readonly();
    (
        answers,
        refused,
        damaged_held,
        std::fs::read(original).unwrap() == bytes,
        [read_only(original), read_only(&damaged)],
    )
}

/// What every store's round answers.
fn imported_once() -> (
    Vec<Result<StateImport, String>>,
    bool,
    bool,
    bool,
    [bool; 2],
) {
    (
        vec![
            Ok(StateImport::Imported {
                duplicate_job_ids: Vec::new(),
            }),
            Ok(StateImport::AlreadyHeld),
        ],
        true,
        false,
        true,
        [true, false],
    )
}

#[test]
fn scheduled_jobs_import_with_their_claimed_dispatches() {
    use crate::cron::store::{input, AgentCronJobStore};
    let dir = tempfile::tempdir().unwrap();
    let written = dir.path().join("a").join("scheduled-jobs.json");
    let store = AgentCronJobStore::new(written.clone());
    let now = 1_700_000_000_000;
    store.create(&input("tick", "every 10m", now)).unwrap();
    store.create(&input("later", "every 3h", now)).unwrap();
    assert_eq!(
        store.claim_due(now + 600_000, now + 600_000).unwrap().len(),
        1
    );
    let original = dir.path().join("b").join("scheduled-jobs.json");
    let round = import_round(
        StateFile::ScheduledJobs,
        &written,
        &original,
        "{\"jobs\": [",
    );
    assert_eq!(
        (round, AgentCronJobStore::new(original).read_states()),
        (imported_once(), store.read_states())
    );
}

/// A `scheduled-jobs.json` that lists one job id twice imports keeping the
/// last job with it, in its place, and the import names the id.
#[test]
fn a_job_id_listed_twice_imports_keeping_the_last() {
    use crate::cron::store::{input, read_jobs_state_in, AgentCronJobStore};
    let dir = tempfile::tempdir().unwrap();
    let log = process_log().expect("the test process keeps a database");
    let written = dir.path().join("a").join("scheduled-jobs.json");
    let store = AgentCronJobStore::new(written.clone());
    let now = 1_700_000_000_000;
    store.create(&input("tick", "every 10m", now)).unwrap();
    store.create(&input("later", "every 3h", now)).unwrap();
    let jobs = read_jobs_state_in(Some(log), &written).jobs;
    let mut again = jobs[0].clone();
    again.prompt = "tock".to_string();
    let original = dir.path().join("b").join("scheduled-jobs.json");
    std::fs::create_dir_all(original.parent().unwrap()).unwrap();
    std::fs::write(
        &original,
        serde_json::to_string(&json!({ "jobs": [&jobs[0], &jobs[1], &again] })).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            import_state_file(log, StateFile::ScheduledJobs, &original),
            read_jobs_state_in(Some(log), &original).jobs,
        ),
        (
            Ok(StateImport::Imported {
                duplicate_job_ids: vec![jobs[0].id.clone()],
            }),
            vec![jobs[1].clone(), again],
        )
    );
}

#[test]
fn harness_stores_import_as_their_documents() {
    use crate::refinement::{
        get_harness_state_path, load_harness_state, update_harness_state, HarnessScope,
    };
    let dir = tempfile::tempdir().unwrap();
    let written = dir.path().join("a").join("harness");
    update_harness_state(&written, HarnessScope::Global, |state| state.schema = 1).unwrap();
    let original = dir.path().join("b").join("harness");
    let round = import_round(
        StateFile::HarnessState,
        &get_harness_state_path(&written),
        &get_harness_state_path(&original),
        "[]",
    );
    assert_eq!(
        (round, load_harness_state(&original, HarnessScope::Global)),
        (
            imported_once(),
            load_harness_state(&written, HarnessScope::Global)
        )
    );
}

#[test]
fn the_reuse_ledger_imports_as_its_document() {
    use crate::reuse::{load, Creation, EntryKey, Event, Recorder};
    let dir = tempfile::tempdir().unwrap();
    let (written, original) = (dir.path().join("a"), dir.path().join("b"));
    let key = EntryKey::Skill("deploy-notes".to_string());
    let recorder = Recorder::new(&written, "session-a", Path::new("/project"));
    recorder
        .record(Event::Created(Creation::New), std::slice::from_ref(&key))
        .unwrap();
    recorder
        .record(Event::Used, std::slice::from_ref(&key))
        .unwrap();
    let round = import_round(
        StateFile::ReuseLedger,
        &crate::reuse::ledger_path(&written),
        &crate::reuse::ledger_path(&original),
        "{\"schema\": 1,",
    );
    assert_eq!((round, load(&original)), (imported_once(), load(&written)));
}

#[test]
fn the_event_bus_imports_its_sources_and_subscriptions() {
    use pa_types::events::{EventDeliveryMode, EventSourceRecord, EventSubscriptionRecord};
    let dir = tempfile::tempdir().unwrap();
    let written = dir.path().join("a").join("event-bus.json");
    let state = super::EventBusState {
        sources: [(
            "deploys".to_string(),
            EventSourceRecord {
                name: "deploys".to_string(),
                description: "Finished deploys".to_string(),
                schema: json!({ "type": "object" }),
                owner_session_id: "s-owner".to_string(),
                owner_name: "owner".to_string(),
                defined_at: 1,
                published_count: 3,
                timer: None,
                watch: None,
            },
        )]
        .into_iter()
        .collect(),
        subscriptions: vec![EventSubscriptionRecord {
            id: "sub-1".to_string(),
            source: "deploys".to_string(),
            filter: serde_json::Map::new(),
            prompt: "Check the deploy".to_string(),
            delivery_mode: EventDeliveryMode::FollowUp,
            label: None,
            session_id: "s-listener".to_string(),
            cwd: "/w".to_string(),
            created_at: 2,
            delivered_count: 1,
        }],
    };
    assert!(super::store_event_bus(&written, &state).unwrap());
    let original: PathBuf = dir.path().join("b").join("event-bus.json");
    let round = import_round(StateFile::EventBus, &written, &original, "{\"sources\": 7}");
    assert_eq!(
        (round, super::held_event_bus(&original).unwrap()),
        (imported_once(), Some(state))
    );
}

#[test]
fn the_crash_log_imports_its_records_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let written = dir.path().join("a").join("session-crashes.jsonl");
    let records = [
        json!({ "kind": "daemon", "message": "first", "at": "t1" }),
        json!({ "kind": "panic", "message": "second", "at": "t2" }),
    ];
    for record in &records {
        super::append_crash_record(&written, record).unwrap();
    }
    let original = dir.path().join("b").join("session-crashes.jsonl");
    let round = import_round(
        StateFile::CrashLog,
        &written,
        &original,
        "{\"kind\":\"daemon\"}\nnot a record\n{\"kind\":\"panic\"}\n",
    );
    assert_eq!(
        (round, super::crash_records(&original).unwrap()),
        (imported_once(), records.to_vec())
    );
}
