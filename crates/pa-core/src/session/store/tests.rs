//! The SQLite store against the file store: the same operations give the
//! same bytes; the import of session files; `export-jsonl`.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::export::export_from;
use super::file::FileTranscriptStore;
use super::sqlite::{OriginalImport, SqliteTranscriptStore};
use super::{import_trees, ExportReport, ExportSelection, TranscriptImportReport, TranscriptStore};

/// The captured sessions every store test runs over.
fn captured() -> Vec<Vec<u8>> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    [
        manifest.join("../pa-daemon/tests/fixtures/attribution-fold-captured.jsonl"),
        manifest.join("tests/fixtures/run_metrics/synthetic-two-runs.jsonl"),
    ]
    .iter()
    .map(|path| std::fs::read(path).unwrap())
    .collect()
}

fn sqlite(dir: &Path) -> SqliteTranscriptStore {
    SqliteTranscriptStore::open(&dir.join("agent.db")).unwrap()
}

/// What a store shows of a transcript: its bytes, read whole and through a
/// reader from several positions back from the end (as the windowed reverse
/// reads do), and its length.
fn seen(store: &dyn TranscriptStore, locator: &Path) -> (Vec<u8>, Vec<Vec<u8>>, u64) {
    let bytes = store.read(locator).unwrap();
    let mut reader = store.open(locator).unwrap();
    let tails = [1_i64, 100, 4096, 70_000]
        .iter()
        .map(|back| {
            let back = (*back).min(i64::try_from(bytes.len()).unwrap());
            reader.seek(SeekFrom::End(-back)).unwrap();
            let mut tail = Vec::new();
            reader.read_to_end(&mut tail).unwrap();
            tail
        })
        .collect();
    (bytes, tails, store.stat(locator).unwrap().len())
}

/// Every write a session makes, run on both stores from a torn captured
/// session: the bytes, the tail reads and the lengths match the file store's
/// exactly, and each write moves the generation while a read does not.
#[test]
fn the_sqlite_store_writes_what_the_file_store_writes() {
    let dir = tempfile::tempdir().unwrap();
    let database = sqlite(dir.path());
    for (index, captured) in captured().into_iter().enumerate() {
        let torn = &captured[..captured.len() - 7];
        let mut views = Vec::new();
        let mut generations = Vec::new();
        for (name, store) in [
            ("files", &FileTranscriptStore as &dyn TranscriptStore),
            ("sqlite", &database),
        ] {
            let locator = dir.path().join(name).join(format!("{index}.jsonl"));
            store
                .create(&locator, &mut |out| out.write_all(torn))
                .unwrap();
            let created = store.stat(&locator).unwrap().generation;
            let appended = store
                .append(&locator, b"{\"type\":\"custom\",\"id\":\"a1\"}\n")
                .unwrap();
            let mut completion = store.open_for_completion(&locator).unwrap();
            completion.write_all(b"{\"type\":\"cus").unwrap();
            completion.write_all(b"tom\",\"id\":\"a2\"}\n").unwrap();
            completion.sync().unwrap();
            drop(completion);
            store.repair_tail(&locator);
            let completed = store.stat(&locator).unwrap().generation;
            store.sync(&locator).unwrap();
            generations.push((
                created != appended,
                appended != completed,
                completed == store.stat(&locator).unwrap().generation,
                appended.same_transcript(&completed),
            ));
            views.push(seen(store, &locator));
        }
        assert_eq!(views[0], views[1], "captured session {index}");
        assert_eq!(generations, vec![(true, true, true, true); 2]);
    }
}

/// A session file the database does not hold is read as it is; its first
/// write imports it whole, after which the original keeps its bytes, is
/// read-only, and the transcript carries on in the database. A damaged one is
/// repaired in the database, never in the file.
#[test]
fn a_first_write_imports_the_original_and_never_writes_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = sqlite(dir.path());
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut results = Vec::new();
    for (index, captured) in captured().into_iter().enumerate() {
        let original = sessions.join(format!("{index}.jsonl"));
        let mut damaged = captured.clone();
        damaged.extend_from_slice(b"{\"type\":\"mess");
        std::fs::write(&original, &damaged).unwrap();
        let read_through = store.read(&original).unwrap() == damaged;
        let listed = store.list(&sessions).unwrap().transcripts.len();
        store.repair_tail(&original);
        store.append(&original, b"{\"type\":\"custom\"}\n").unwrap();
        let mut expected = captured.clone();
        expected.extend_from_slice(b"{\"type\":\"custom\"}\n");
        results.push((
            read_through,
            listed,
            store.read(&original).unwrap() == expected,
            std::fs::read(&original).unwrap() == damaged,
            std::fs::metadata(&original)
                .unwrap()
                .permissions()
                .readonly(),
        ));
    }
    assert_eq!(
        results,
        vec![(true, 1, true, true, true), (true, 2, true, true, true)]
    );
}

/// The background import brings in each session file once, whole: a second
/// pass does nothing, the originals keep their bytes and are read-only, and a
/// `.jsonl` file that is no transcript is left alone and writable.
#[test]
fn session_trees_import_once_and_leave_the_originals_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = sqlite(dir.path());
    let sessions = dir.path().join("sessions");
    let archive = dir.path().join("sessions-archive");
    let captured = captured();
    let originals = [
        sessions.join("--work--").join("a.jsonl"),
        archive.join("b.jsonl"),
    ];
    for (original, bytes) in originals.iter().zip(&captured) {
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(original, bytes).unwrap();
    }
    let journal = sessions.join("recovery.jsonl");
    std::fs::write(&journal, "{\"op\":\"x\"}\n").unwrap();
    let roots = [sessions.clone(), archive.clone()];
    let first = import_trees(&store, &roots);
    let second = import_trees(&store, &roots);
    let again = store.import_original(&originals[0]).unwrap();
    let untouched: Vec<(bool, bool, bool)> = originals
        .iter()
        .zip(&captured)
        .map(|(original, bytes)| {
            (
                std::fs::read(original).unwrap() == *bytes,
                std::fs::metadata(original)
                    .unwrap()
                    .permissions()
                    .readonly(),
                store.read(original).unwrap() == *bytes,
            )
        })
        .collect();
    assert_eq!(
        (
            first,
            second,
            again,
            untouched,
            std::fs::metadata(&journal)
                .unwrap()
                .permissions()
                .readonly(),
        ),
        (
            Some(TranscriptImportReport {
                imported: 2,
                bytes: captured.iter().map(|bytes| bytes.len() as u64).sum(),
                already_stored: 0,
                failed: Vec::new(),
            }),
            None,
            OriginalImport::AlreadyStored,
            vec![(true, true, true); 2],
            false,
        )
    );
}

/// `export-jsonl` writes an imported session back as its original's bytes
/// and a session created in the database as what was written, keeps the
/// agent dir's layout for `--all`, never overwrites, and finds one session
/// by an id prefix.
#[test]
fn export_jsonl_writes_the_stored_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let store = sqlite(dir.path());
    let agent = dir.path().join("agent");
    let sessions = agent.join("sessions").join("--work--");
    std::fs::create_dir_all(&sessions).unwrap();
    let captured = captured();
    let imported = sessions.join("imported.jsonl");
    std::fs::write(&imported, &captured[0]).unwrap();
    store.append(&imported, b"").unwrap();
    let created = sessions.join("created.jsonl");
    let header = "{\"type\":\"session\",\"version\":3,\"id\":\"created-1\",\"timestamp\":\"t\",\"cwd\":\"/work\"}\n";
    store
        .create(&created, &mut |out| out.write_all(header.as_bytes()))
        .unwrap();
    let out = dir.path().join("out");
    let export = |selection| {
        export_from(
            &store,
            store.locators().unwrap(),
            &[agent.join("sessions")],
            &agent,
            selection,
            &out,
        )
        .unwrap()
    };
    let all = export(ExportSelection::All);
    let exported: Vec<Vec<u8>> = ["imported.jsonl", "created.jsonl"]
        .iter()
        .map(|name| std::fs::read(out.join("sessions/--work--").join(name)).unwrap())
        .collect();
    let repeated = export(ExportSelection::All);
    let one = export(ExportSelection::Session("created"));
    let written = |names: &[&str]| -> Vec<PathBuf> {
        names
            .iter()
            .map(|name| out.join("sessions/--work--").join(name))
            .collect()
    };
    assert_eq!(
        (all, exported, repeated, one),
        (
            ExportReport {
                written: written(&["created.jsonl", "imported.jsonl"]),
                ..ExportReport::default()
            },
            vec![captured[0].clone(), header.as_bytes().to_vec()],
            ExportReport {
                unchanged: written(&["created.jsonl", "imported.jsonl"]),
                ..ExportReport::default()
            },
            ExportReport {
                written: vec![out.join("created.jsonl")],
                ..ExportReport::default()
            },
        )
    );
}
