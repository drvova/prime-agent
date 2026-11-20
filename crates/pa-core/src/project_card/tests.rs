use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::collections::BTreeMap;

use super::{derive, Derived, Inner, ProjectCard, Stored, SCHEMA};
use crate::kernel::spill::{content_id, SpillStore};

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, text).expect("write");
}

fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("repo");
    let root_path = root.path();
    write(
        root_path,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(root_path, "crates/core/src/lib.rs", "pub fn f() {}\n");
    write(
        root_path,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core\"\n",
    );
    write(root_path, "web/package.json", "{}");
    write(
        root_path,
        "package.json",
        r#"{"scripts": {"build": "tsc -p .", "test": "vitest run", "lint": "eslint ."}}"#,
    );
    write(
        root_path,
        "Makefile",
        ".PHONY: check\ncheck: fmt\n\tcargo fmt\ntest-e2e:\n\t./e2e.sh\nVERSION := 1\n",
    );
    write(
        root_path,
        ".github/workflows/ci.yml",
        "jobs:\n  ci:\n    steps:\n      - run: make check\n      - run: |\n          multi\n      - run: cargo test --workspace\n",
    );
    root
}

/// The card says what the manifests, the Makefile, and CI say, and the
/// detail keeps everything it summarizes.
#[test]
fn the_card_is_derived_from_manifests_makefile_and_ci() {
    let repo = repository();
    assert_eq!(
        derive(repo.path()),
        Derived {
            build: vec!["cargo build".to_string(), "npm run build".to_string()],
            test: vec![
                "cargo test".to_string(),
                "npm test".to_string(),
                "make test-e2e".to_string(),
            ],
            check: vec![
                "npm run lint".to_string(),
                "make check".to_string(),
                "cargo test --workspace".to_string(),
            ],
            dirs: vec![
                "crates/ (2 files)".to_string(),
                "web/ (1 files)".to_string()
            ],
            details: "npm run build: tsc -p .\nnpm run test: vitest run\nnpm run lint: eslint .\n\
make check\nmake test-e2e\nci: make check\nci: cargo test --workspace\n\
dir: crates/ (2 files)\ndir: web/ (1 files)\n"
                .to_string(),
        }
    );
}

fn card_over(root: &Path, store: &Path) -> ProjectCard {
    ProjectCard {
        inner: Arc::new(Inner {
            root: root.to_path_buf(),
            store: store.to_path_buf(),
            state: Mutex::new(Stored::default()),
            saving: Mutex::new(()),
        }),
    }
}

/// A later session reads what the last one stored: the card, plus the
/// commands its cells ran, with the detail behind a handle.
#[tokio::test]
async fn the_next_session_starts_with_the_stored_card_and_learned_commands() {
    let repo = repository();
    let agent = tempfile::tempdir().expect("agent dir");
    let store = agent.path().join("card.json");
    let first = card_over(repo.path(), &store);
    *super::lock(&first.inner.state) = Stored {
        schema: SCHEMA,
        derived: Some(derive(repo.path())),
        commands: BTreeMap::default(),
    };
    for command in [
        "cargo test -p core",
        "cargo test -p core",
        "ls -la",
        "make check",
    ] {
        first.observe_command(command);
    }
    first.inner.save();

    let next = card_over(repo.path(), &store);
    next.load(Duration::from_secs(5)).await;
    let session = tempfile::tempdir().expect("session dir");
    let spill = SpillStore::for_session_dir(session.path());
    let details = derive(repo.path()).details;
    assert_eq!(
        next.render(Some(&spill)),
        Some(format!(
            "# Project card\n\nDerived from this repository's manifests, Makefile, and CI, plus the commands sessions here ran; the full detail is out:{} (page it with ctx.page).\n\
- Build: `cargo build`, `npm run build`\n\
- Test: `cargo test`, `npm test`, `make test-e2e`\n\
- Checks: `npm run lint`, `make check`, `cargo test --workspace`\n\
- Key dirs: crates/ (2 files), web/ (1 files)\n\
- Run here before: `cargo test -p core` (2×), `make check` (1×)",
            content_id(&details)
        ))
    );
}

/// A stored card the disk never delivers (a hung mount) never holds the
/// session start: the load gives up at its budget and the session has no card.
#[cfg(unix)]
#[tokio::test]
async fn a_hanging_store_never_holds_the_session_start() {
    let agent = tempfile::tempdir().expect("agent dir");
    let store = agent.path().join("card.json");
    nix::unistd::mkfifo(&store, nix::sys::stat::Mode::S_IRWXU).expect("fifo");
    let card = card_over(agent.path(), &store);
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(5), card.load(super::LOAD_BUDGET))
        .await
        .expect("the load returned at its budget");
    assert_eq!(
        (
            started.elapsed() < Duration::from_secs(2),
            card.render(None)
        ),
        (true, None)
    );
    // Release the blocked reader so the test's blocking thread ends.
    let _ = std::fs::OpenOptions::new().write(true).open(&store);
}
