use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::*;
use crate::fs_edit::{edit, parse_edit, read, ReadRequest, Target};
use crate::kernel::spill::{content_id, SpillLookup};

struct Fixture {
    seen: SeenFiles,
    store: SpillStore,
    mark: Arc<Mutex<ContextMark>>,
    facts: Arc<Mutex<Vec<RereadFact>>>,
    dir: tempfile::TempDir,
}

const WINDOW_TURNS: u64 = 8;

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = SpillStore::for_session_dir(&dir.path().join("session"));
    let facts = Arc::new(Mutex::new(Vec::new()));
    let seen = SeenFiles::new(
        Some(store.clone()),
        WINDOW_TURNS,
        Some({
            let facts = Arc::clone(&facts);
            Arc::new(move |fact| facts.lock().unwrap().push(fact)) as RereadObserver
        }),
    );
    let mark = Arc::new(Mutex::new(ContextMark {
        turn: 1,
        compacted_at_ms: None,
    }));
    let probe_mark = Arc::clone(&mark);
    seen.bind(Arc::new(move || {
        let mark = *probe_mark.lock().unwrap();
        Box::pin(async move { mark })
    }));
    Fixture {
        seen,
        store,
        mark,
        facts,
        dir,
    }
}

impl Fixture {
    fn at_turn(&self, turn: u64) {
        self.mark.lock().unwrap().turn = turn;
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }

    /// `fs.read` of the whole file, shaped as the host shapes it.
    async fn read(&self, name: &str) -> Value {
        let (mut reply, _) = read(&ReadRequest {
            target: Target {
                path: self.path(name),
                shown: name.to_string(),
            },
            start: 1,
            end: None,
        })
        .unwrap();
        self.seen.shape_read(&self.path(name), &mut reply).await;
        reply
    }
}

fn numbered(lines: usize) -> String {
    (1..=lines).map(|n| format!("line {n}\n")).collect()
}

/// The reply as the plain read gives it (what code sees through `view.lines`).
fn unshaped(reply: &Value) -> Value {
    let mut reply = reply.clone();
    let object = reply.as_object_mut().unwrap();
    object.remove("text");
    object.remove("seen");
    reply
}

/// A re-read of an unchanged file shows one line naming the turn it was seen in, and a handle
/// whose stored text is the full view; the lines code reads stay complete.
#[tokio::test]
async fn an_unchanged_reread_is_one_line_with_a_handle_to_the_full_view() {
    let fixture = fixture();
    std::fs::write(fixture.path("a.py"), numbered(40)).unwrap();
    fixture.at_turn(3);
    let first = fixture.read("a.py").await;
    fixture.at_turn(5);
    let again = fixture.read("a.py").await;
    let full = first["text"].as_str().unwrap();
    let handle = format!("out:{}", content_id(full));
    assert_eq!(
        (
            again["text"].clone(),
            again["seen"].clone(),
            unshaped(&again)
        ),
        (
            Value::from(format!(
                "a.py (unchanged since turn 3, 40 lines; whole view: {handle})"
            )),
            serde_json::json!({"state": "unchanged", "turn": 3, "handle": handle}),
            unshaped(&first),
        )
    );
    let SpillLookup::Stored(stored) = fixture.store.lookup(&content_id(full)).await else {
        panic!("the full view is stored");
    };
    assert_eq!(stored.text, full);
    assert_eq!(*fixture.facts.lock().unwrap(), vec![RereadFact::Unchanged]);
}

/// A re-read of a changed file shows a diff against the seen version: context and new lines
/// under fresh anchors, removed lines bare, hunks apart; the changed version is then seen.
#[tokio::test]
async fn a_changed_reread_is_a_diff_against_the_seen_version() {
    let fixture = fixture();
    std::fs::write(fixture.path("a.py"), numbered(40)).unwrap();
    fixture.at_turn(2);
    fixture.read("a.py").await;
    let changed = numbered(40)
        .replace("line 10\n", "line ten\n")
        .replace("line 30\n", "");
    std::fs::write(fixture.path("a.py"), &changed).unwrap();
    fixture.at_turn(4);
    let reply = fixture.read("a.py").await;
    let rows = reply["lines"].as_array().unwrap();
    let row = |line: usize| {
        format!(
            "{}|{}",
            rows[line - 1]["anchor"].as_str().unwrap(),
            rows[line - 1]["text"].as_str().unwrap()
        )
    };
    let handle = reply["seen"]["handle"].as_str().unwrap().to_string();
    assert_eq!(
        reply["text"].as_str().unwrap(),
        format!(
            "a.py (changed since turn 2: -2 +1 lines, 39 total; whole view: {handle})\n {}\n {}\n-line 10\n+{}\n {}\n {}\n...\n {}\n {}\n-line 30\n {}\n {}",
            row(8),
            row(9),
            row(10),
            row(11),
            row(12),
            row(28),
            row(29),
            row(30),
            row(31),
        )
    );
    fixture.at_turn(6);
    let again = fixture.read("a.py").await;
    assert_eq!(
        again["text"].as_str().unwrap(),
        format!("a.py (unchanged since turn 4, 39 lines; whole view: {handle})")
    );
    assert_eq!(
        *fixture.facts.lock().unwrap(),
        vec![RereadFact::Diff, RereadFact::Unchanged]
    );
}

/// A compaction since the read, or a read older than the window thinning protects, means the
/// model may no longer hold the content: the re-read is shown whole, and seen anew.
#[tokio::test]
async fn compaction_and_age_show_the_file_whole_again() {
    let fixture = fixture();
    std::fs::write(fixture.path("a.py"), numbered(40)).unwrap();
    fixture.at_turn(1);
    let first = fixture.read("a.py").await;
    fixture.mark.lock().unwrap().compacted_at_ms = Some(pa_agent::now_ms() + 1);
    fixture.at_turn(2);
    let after_compaction = fixture.read("a.py").await;
    // That compaction is now older than the read that followed it.
    fixture.mark.lock().unwrap().compacted_at_ms = Some(0);
    fixture.at_turn(2 + WINDOW_TURNS);
    let aged = fixture.read("a.py").await;
    fixture.at_turn(3 + WINDOW_TURNS);
    let fresh = fixture.read("a.py").await;
    assert_eq!((after_compaction, aged), (first.clone(), first));
    assert_eq!(fresh["seen"]["state"], "unchanged");
    assert_eq!(
        *fixture.facts.lock().unwrap(),
        vec![RereadFact::Full, RereadFact::Full, RereadFact::Unchanged]
    );
}

/// The model's own applied edit is seen: a re-read right after it is unchanged.
#[tokio::test]
async fn an_own_edit_is_seen() {
    let fixture = fixture();
    std::fs::write(fixture.path("a.py"), numbered(40)).unwrap();
    fixture.at_turn(1);
    let first = fixture.read("a.py").await;
    let anchor = first["lines"][4]["anchor"].clone();
    let root = fixture.dir.path().to_string_lossy().to_string();
    let (_, _, rewrite) = edit(
        &parse_edit(&serde_json::json!({
            "path": fixture.path("a.py").to_string_lossy(),
            "cwd": root,
            "edits": [{"op": "replace", "anchor": anchor, "text": "line five"}],
        }))
        .unwrap(),
    )
    .unwrap();
    let rewrite = rewrite.unwrap();
    fixture.at_turn(2);
    fixture
        .seen
        .note_edit(&fixture.path("a.py"), &rewrite.before, rewrite.after)
        .await;
    fixture.at_turn(3);
    let again = fixture.read("a.py").await;
    assert_eq!(again["seen"]["state"], "unchanged");
    assert_eq!(again["seen"]["turn"], 2);
}

/// A probe that never answers must not hold the read (a cell waits on it): past the deadline
/// the read is shown whole and nothing is recorded.
#[tokio::test(start_paused = true)]
async fn a_hanging_probe_shows_the_read_whole() {
    let fixture = fixture();
    let seen = SeenFiles::new(Some(fixture.store.clone()), WINDOW_TURNS, None);
    seen.bind(Arc::new(|| Box::pin(std::future::pending::<ContextMark>())));
    std::fs::write(fixture.path("a.py"), numbered(3)).unwrap();
    let (plain, _) = read(&ReadRequest {
        target: Target {
            path: fixture.path("a.py"),
            shown: "a.py".to_string(),
        },
        start: 1,
        end: None,
    })
    .unwrap();
    for _ in 0..2 {
        let mut reply = plain.clone();
        seen.shape_read(&fixture.path("a.py"), &mut reply).await;
        assert_eq!(reply, plain);
    }
    assert!(lock(&seen.files).is_empty());
}
