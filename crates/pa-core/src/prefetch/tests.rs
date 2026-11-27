use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::{Learned, PrefetchFact, Prefetcher};
use crate::kernel::spill::{content_id, SpillStore};
use crate::symbol_index::SymbolIndex;

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, text).expect("write");
}

const STORE: &str = "mod cache;\n\npub struct Store;\n\n#[cfg(test)]\nmod tests;\n";

fn crate_repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("repo");
    let r = root.path();
    write(r, "Cargo.toml", "[package]\nname = \"app\"\n");
    write(r, "src/store.rs", STORE);
    write(r, "src/store/tests.rs", "use super::Store;\n");
    write(r, "src/store/cache.rs", "pub struct Cache;\n");
    write(r, "tests/store.rs", "use app::Store;\n");
    write(r, "src/main.rs", "fn main() {\n    let _ = Store;\n}\n");
    root
}

type Facts = Arc<Mutex<Vec<PrefetchFact>>>;

fn prefetcher(root: &Path, session: &Path) -> (Prefetcher, Facts, SymbolIndex) {
    let facts: Facts = Arc::new(Mutex::new(Vec::new()));
    let index = SymbolIndex::for_project(root, None);
    let prefetcher = Prefetcher::new(
        root,
        None,
        index.clone(),
        SpillStore::for_session_dir(session),
        Some({
            let facts = Arc::clone(&facts);
            Arc::new(move |fact| facts.lock().unwrap().push(fact))
        }),
    );
    (prefetcher, facts, index)
}

/// Reading a file pages out its test files, the module it imports, and the
/// file calling what it defines, as handles (never their text), in that order.
#[tokio::test]
async fn a_read_names_handles_to_its_tests_imports_and_callers() {
    let repo = crate_repo();
    let session = tempfile::tempdir().expect("session dir");
    let (prefetcher, facts, index) = prefetcher(repo.path(), session.path());
    index.refresh();
    index.refreshed().await;
    let (related, line) = prefetcher
        .prefetch(repo.path().join("src/store.rs"))
        .await
        .expect("neighbours");
    let handle = |text: &str| format!("out:{}", content_id(text));
    assert_eq!(
        (related, line, facts.lock().unwrap().clone()),
        (
            json!([
                {"path": "src/store/tests.rs", "why": "test", "handle": handle("use super::Store;\n")},
                {"path": "tests/store.rs", "why": "test", "handle": handle("use app::Store;\n")},
                {"path": "src/store/cache.rs", "why": "import", "handle": handle("pub struct Cache;\n")},
                {"path": "src/main.rs", "why": "caller", "handle": handle("fn main() {\n    let _ = Store;\n}\n")},
            ]),
            format!(
                "[related, paged out (read with (output-read handle)): test src/store/tests.rs {} · test tests/store.rs {} · import src/store/cache.rs {} · caller src/main.rs {}]",
                handle("use super::Store;\n"),
                handle("use app::Store;\n"),
                handle("pub struct Cache;\n"),
                handle("fn main() {\n    let _ = Store;\n}\n"),
            ),
            vec![PrefetchFact {
                issued: 4,
                used: 0,
                unused: 0,
                skipped: false,
            }],
        )
    );
}

/// An include that climbs out of the workspace with `..` names no handle: a
/// file outside the project is never paged into the session's store.
#[tokio::test]
async fn an_import_that_leaves_the_workspace_is_never_paged_out() {
    let outer = tempfile::tempdir().expect("outer");
    let repo = outer.path().join("repo");
    write(outer.path(), "secret.h", "#define TOKEN 1\n");
    write(&repo, "src/inside.h", "#define INSIDE 1\n");
    write(
        &repo,
        "src/main.c",
        "#include \"../../secret.h\"\n#include \"inside.h\"\n",
    );
    let session = tempfile::tempdir().expect("session dir");
    let (prefetcher, _facts, _index) = prefetcher(&repo, session.path());
    let (related, _line) = prefetcher
        .prefetch(repo.join("src/main.c"))
        .await
        .expect("neighbours");
    assert_eq!(
        related,
        json!([{
            "path": "src/inside.h",
            "why": "import",
            "handle": format!("out:{}", content_id("#define INSIDE 1\n")),
        }])
    );
}

/// A handle read through `output-*`, or its file read, counts as used when the
/// next read settles the batch.
#[tokio::test]
async fn used_handles_are_counted_when_the_next_read_settles() {
    let repo = crate_repo();
    let session = tempfile::tempdir().expect("session dir");
    let (prefetcher, facts, _index) = prefetcher(repo.path(), session.path());
    prefetcher.prefetch(repo.path().join("src/store.rs")).await;
    prefetcher.note_handle_read(&content_id("use app::Store;\n"));
    prefetcher
        .prefetch(repo.path().join("src/store/cache.rs"))
        .await;
    let second = facts.lock().unwrap()[1];
    assert_eq!(
        (second.used, second.unused),
        (2, 1),
        "the ctx read and the read of cache.rs itself are uses; the unit-test file is not"
    );
}

/// A project that never uses its handles stops getting them, except a
/// one-handle probe every tenth read; one that uses them keeps them.
#[test]
fn the_prefetch_scales_down_when_handles_go_unused() {
    let mut unused = Learned::default();
    let mut budgets = Vec::new();
    for _ in 0..12 {
        unused.reads += 1;
        let budget = unused.budget();
        budgets.push(budget);
        unused.settle(0, budget);
    }
    let mut used = Learned::default();
    for _ in 0..5 {
        used.settle(3, 4);
    }
    assert_eq!(
        (budgets, used.budget()),
        (vec![4, 4, 4, 0, 0, 0, 0, 0, 0, 1, 0, 0], 3)
    );
}

/// A neighbour read that never answers (a hung file system) never holds the
/// `fs-read` result: the prefetch gives up at its budget.
#[tokio::test]
async fn a_hanging_neighbour_never_holds_the_read() {
    let repo = crate_repo();
    let session = tempfile::tempdir().expect("session dir");
    // The reader blocks until the sender drops at the end of the test.
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(gate));
    let prefetcher = Prefetcher::with_reader(
        repo.path(),
        None,
        SymbolIndex::for_project(repo.path(), None),
        SpillStore::for_session_dir(session.path()),
        None,
        {
            let gate = Arc::clone(&gate);
            Arc::new(move |_path: &Path| {
                let _ = gate.lock().unwrap().recv();
                Err(std::io::Error::other("released"))
            })
        },
    );
    let answered = tokio::time::timeout(
        Duration::from_secs(5),
        prefetcher.prefetch(repo.path().join("src/store.rs")),
    )
    .await
    .expect("the prefetch returned at its budget");
    drop(release);
    assert_eq!(answered, None);
}

/// Two sessions on one project learn into one share, and it is saved: the
/// reads one session left unused leave the other's next read without handles.
#[tokio::test]
async fn sessions_on_one_project_share_what_they_learn() {
    let repo = crate_repo();
    let agent_dir = tempfile::tempdir().expect("agent dir");
    let sessions = [
        tempfile::tempdir().expect("session dir"),
        tempfile::tempdir().expect("session dir"),
    ];
    let facts: Facts = Arc::new(Mutex::new(Vec::new()));
    let [first, second] = sessions.each_ref().map(|session| {
        let facts = Arc::clone(&facts);
        Prefetcher::new(
            repo.path(),
            Some(agent_dir.path()),
            SymbolIndex::for_project(repo.path(), None),
            SpillStore::for_session_dir(session.path()),
            Some(Arc::new(move |fact| facts.lock().unwrap().push(fact))),
        )
    });
    let file = repo.path().join("src/store.rs");
    for _ in 0..4 {
        first.prefetch(file.clone()).await;
    }
    facts.lock().unwrap().clear();
    let answered = second.prefetch(file).await;
    let stored = std::fs::read_dir(agent_dir.path().join("prefetch"))
        .expect("state dir")
        .map(|entry| std::fs::read_to_string(entry.expect("entry").path()).expect("state"))
        .collect::<Vec<_>>();
    assert_eq!(
        (answered, facts.lock().unwrap().clone(), stored),
        (
            None,
            vec![PrefetchFact {
                issued: 0,
                used: 0,
                unused: 0,
                skipped: true,
            }],
            vec![r#"{"schema":1,"learned":{"share":0.0,"batches":3,"reads":5}}"#.to_string()],
        )
    );
}
