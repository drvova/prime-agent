use super::*;

struct Fixture {
    _root: tempfile::TempDir,
    agent_dir: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::TempDir::new().unwrap();
        let agent_dir = root.path().join("agent");
        let workspace = root.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        Fixture {
            agent_dir,
            workspace,
            _root: root,
        }
    }

    fn open(&self) -> CheckpointStore {
        CheckpointStore::open(&self.agent_dir, &self.workspace, "session-1").unwrap()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.workspace.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.join(path)).ok()
    }
}

#[test]
fn restore_rewrites_changed_files_and_deletes_added_ones() {
    let fixture = Fixture::new();
    // The project's own repository: the checkpoint's staging never reaches it.
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&fixture.workspace)
        .status();
    assert!(init.unwrap().success());
    fixture.write("src/main.rs", "fn main() {}\n");
    fixture.write("keep.txt", "unchanged\n");
    let store = fixture.open();
    let before = store.capture().unwrap();

    // The turn edits one file, deletes one, and adds a file in a new directory.
    fixture.write("src/main.rs", "fn main() { edited(); }\n");
    std::fs::remove_file(fixture.workspace.join("keep.txt")).unwrap();
    fixture.write("new/dir/added.rs", "added\n");
    let after = store.capture().unwrap();

    let outcome = store.restore(&before, Some(&after)).unwrap();
    assert_eq!(
        outcome,
        RestoreOutcome {
            preimage: after,
            restored: 3,
            conflicts: Vec::new(),
        }
    );
    assert_eq!(
        fixture.read("src/main.rs").as_deref(),
        Some("fn main() {}\n")
    );
    assert_eq!(fixture.read("keep.txt").as_deref(), Some("unchanged\n"));
    assert!(!fixture.workspace.join("new").exists());
    assert_eq!(store.capture().unwrap(), before);
    assert!(!fixture.workspace.join(".git/index").exists());
}

#[test]
fn restore_leaves_files_changed_outside_the_session() {
    let fixture = Fixture::new();
    fixture.write("agent.txt", "v1\n");
    fixture.write("user.txt", "v1\n");
    let store = fixture.open();
    let before = store.capture().unwrap();
    fixture.write("agent.txt", "v2\n");
    fixture.write("user.txt", "v2\n");
    let settled = store.capture().unwrap();
    // After the turn settled, someone else edits user.txt.
    fixture.write("user.txt", "user edit\n");

    let outcome = store.restore(&before, Some(&settled)).unwrap();
    assert_eq!(outcome.restored, 1);
    assert_eq!(outcome.conflicts, vec!["user.txt".to_string()]);
    assert_eq!(fixture.read("agent.txt").as_deref(), Some("v1\n"));
    assert_eq!(fixture.read("user.txt").as_deref(), Some("user edit\n"));
}

#[test]
fn recorded_trees_survive_reopening_the_store() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "a\n");
    let tree = {
        let store = fixture.open();
        let tree = store.capture().unwrap();
        store.record("entry-1", Boundary::Before, &tree).unwrap();
        tree
    };
    let store = fixture.open();
    assert_eq!(store.lookup("entry-1", Boundary::Before), Some(tree));
    assert_eq!(store.lookup("entry-1", Boundary::At), None);
}
