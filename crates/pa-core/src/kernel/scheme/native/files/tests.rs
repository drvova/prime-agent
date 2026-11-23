//! The file tools through a real session kernel over the session's real
//! `fs.*` handlers, on real files under temporary directories.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::fs_edit::{ContextMark, EditFact, FsEditHost, RereadFact, SeenFiles};
use crate::fs_search::{FsOperation, FsSearchHost, SearchFact};
use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::kernel::spill::SpillStore;
use crate::symbol_index::{register_symbol_handler, SymbolFact, SymbolIndex};
use crate::tools::kernel_cell::{
    ExecuteStatus, KernelExecuteOptions, KernelExecutor, KernelProvisioner,
};

async fn kernel_over(handlers: HostRequestHandlers, cwd: &Path) -> Box<dyn KernelExecutor> {
    SchemeKernelProvisioner::new(handlers, cwd)
        .ensure(None, None)
        .await
        .unwrap()
}

/// The value of `expression` as JSON, or the error it raised.
async fn value(kernel: &dyn KernelExecutor, expression: &str) -> Result<Value, String> {
    let result = kernel
        .execute(
            &format!("(display (value->jsexpr-string {expression}))"),
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        )
        .await
        .unwrap();
    match result.error {
        None => Ok(serde_json::from_str(&result.stdout).unwrap()),
        Some(error) => Err(error.evalue),
    }
}

/// What the session's telemetry observers heard of one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Heard {
    Search(FsOperation, Option<usize>),
    Edit(EditFact),
    Reread(RereadFact),
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_reach_the_session_handlers_which_shape_rereads_and_count_each_call() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    let lines = |second: &str| {
        let rest: Vec<String> = (3..=30).map(|number| format!("line {number}\n")).collect();
        format!("line 1\n{second}\n{}", rest.concat())
    };
    std::fs::write(&file, lines("line 2")).unwrap();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let hear = |heard: &Arc<Mutex<Vec<Heard>>>| {
        let heard = Arc::clone(heard);
        move |fact| heard.lock().unwrap().push(fact)
    };
    let mut handlers = HostRequestHandlers::new();
    let search = hear(&heard);
    FsSearchHost {
        observer: Some(Arc::new(move |fact: SearchFact| {
            search(Heard::Search(fact.operation, fact.results));
        })),
    }
    .register(&mut handlers);
    let reread = hear(&heard);
    let seen = Arc::new(SeenFiles::new(
        Some(SpillStore::for_session_dir(session_dir.path())),
        /*window_turns*/ 8,
        Some(Arc::new(move |fact| reread(Heard::Reread(fact)))),
    ));
    seen.bind(Arc::new(|| {
        Box::pin(async {
            ContextMark {
                turn: 3,
                compacted_at_ms: None,
            }
        })
    }));
    let edited = hear(&heard);
    FsEditHost {
        observer: Some(Arc::new(move |fact| edited(Heard::Edit(fact)))),
        seen: Some(seen),
        prefetch: None,
    }
    .register(&mut handlers);
    let kernel = kernel_over(handlers, dir.path()).await;
    // What a read shows of a file seen before, and the lines it still holds.
    let read = "(let ([view (fs-read \"notes.txt\")])
                  (list (if (hash-contains? view 'seen) (hash-ref (hash-ref view 'seen) 'state) #f)
                        (length (hash-ref view 'lines))))";
    let first = value(kernel.as_ref(), read).await;
    let again = value(kernel.as_ref(), read).await;
    std::fs::write(&file, lines("line two")).unwrap();
    let changed = value(kernel.as_ref(), read).await;
    let found = value(
        kernel.as_ref(),
        "(length (hash-ref (fs-grep \"line two\") 'matches))",
    )
    .await;
    let applied = value(
        kernel.as_ref(),
        "(hash-ref (fs-edit \"notes.txt\" (hash 'replace (fs-anchor (fs-read \"notes.txt\" (hash 'end 2)) 2) 'text \"line 2\")) 'applied)",
    )
    .await;
    let after_own_edit = value(kernel.as_ref(), read).await;
    assert_eq!(
        (
            [first, again, changed, found, applied, after_own_edit],
            heard.lock().unwrap().clone(),
        ),
        (
            [
                Ok(json!([false, 30])),
                Ok(json!(["unchanged", 30])),
                Ok(json!(["changed", 30])),
                Ok(json!(1)),
                Ok(json!(true)),
                Ok(json!(["unchanged", 30])),
            ],
            vec![
                Heard::Edit(EditFact::Read { lines: 30 }),
                Heard::Reread(RereadFact::Unchanged),
                Heard::Edit(EditFact::Read { lines: 30 }),
                Heard::Reread(RereadFact::Diff),
                Heard::Edit(EditFact::Read { lines: 30 }),
                Heard::Search(FsOperation::Grep, Some(1)),
                Heard::Edit(EditFact::Read { lines: 2 }),
                Heard::Edit(EditFact::Applied { edits: 1 }),
                Heard::Reread(RereadFact::Unchanged),
                Heard::Edit(EditFact::Read { lines: 30 }),
            ],
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_symbol_is_found_in_the_session_index_and_the_query_is_counted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn quux_marker() {}\n\nfn caller() {\n    quux_marker();\n}\n",
    )
    .unwrap();
    let index = SymbolIndex::for_project(dir.path(), None);
    index.refresh();
    index.refreshed().await;
    let counted = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = HostRequestHandlers::new();
    register_symbol_handler(
        &mut handlers,
        index,
        /*store*/ None,
        Some({
            let counted = Arc::clone(&counted);
            Arc::new(move |fact| counted.lock().unwrap().push(fact))
        }),
    );
    let kernel = kernel_over(handlers, dir.path()).await;
    let found = value(
        kernel.as_ref(),
        "(fs-symbol \"quux_marker\" (hash 'max-references 5))",
    )
    .await;
    let refused = value(kernel.as_ref(), "(fs-symbol \"two words\")").await;
    assert_eq!(
        (found, refused, counted.lock().unwrap().clone()),
        (
            Ok(json!({
                "name": "quux_marker",
                "definitions": [{
                    "path": "src/lib.rs",
                    "line": 1,
                    "text": "pub fn quux_marker() {}",
                    "kind": "fn",
                    "handle": false,
                }],
                "references": [{"path": "src/lib.rs", "line": 4, "text": "quux_marker();"}],
                "truncated": false,
                "partial": false,
                "text": "quux_marker: 1 definitions, 1 references\n\
                         def src/lib.rs:1 [fn] pub fn quux_marker() {}\n\
                         ref src/lib.rs:4 quux_marker();",
            })),
            Err(
                "Error: Generic: fs-symbol: name must be one identifier, like \"SpillStore\" or \"load\""
                    .to_string()
            ),
            vec![SymbolFact {
                definitions: 1,
                references: 1,
                partial: false,
            }],
        )
    );
}

/// A session handler that never answers must not hold a stopped cell: the
/// engine cannot interrupt the wait, so the tool ends it.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_ends_with_its_cell_and_the_kernel_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("here.txt"), "x\n").unwrap();
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "fs.grep",
        host_handler(|_: HostRequestPayload| std::future::pending()),
    );
    let kernel = kernel_over(handlers, dir.path()).await;
    let started = Instant::now();
    let stopped = kernel
        .execute(
            "(fs-grep \"x\")",
            KernelExecuteOptions {
                signal: None,
                timeout_ms: Some(300),
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        )
        .await
        .unwrap();
    let stopped_within = started.elapsed();
    let after = value(
        kernel.as_ref(),
        "(map (lambda (entry) (hash-ref entry 'path)) (hash-ref (fs-glob \"*\") 'entries))",
    )
    .await;
    assert_eq!(
        (
            stopped.status,
            stopped.error.map(|error| error.ename),
            stopped_within < Duration::from_secs(10),
            after,
        ),
        (
            ExecuteStatus::Error,
            Some("Timeout".to_string()),
            true,
            Ok(json!(["here.txt"])),
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_takes_rows_as_anchors_and_can_return_a_stale_patch_instead_of_raising() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("list.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
    let kernel = kernel_over(HostRequestHandlers::new(), dir.path()).await;
    let outcome = value(
        kernel.as_ref(),
        "(let* ([view (fs-read \"list.txt\")]
                [done (fs-edit \"list.txt\"
                               (hash 'replace (list (car (hash-ref view 'lines)) (fs-anchor view 2))
                                     'text \"uno\"))]
                [stale (fs-edit \"list.txt\" (hash 'delete (fs-anchor view 3)) (hash 'on-stale 'return))])
           (list (hash-ref done 'applied) (hash-ref done 'check)
                 (hash-ref stale 'applied)
                 (map (lambda (entry) (hash-ref entry 'edit)) (hash-ref stale 'stale))))",
    )
    .await;
    let no_such_line = value(kernel.as_ref(), "(fs-anchor (fs-read \"list.txt\") 9)").await;
    assert_eq!(
        (
            outcome,
            no_such_line.is_err(),
            std::fs::read_to_string(&file).unwrap()
        ),
        (
            Ok(json!([true, false, false, [1]])),
            true,
            "uno\nthree\n".to_string(),
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_unique_string_is_replaced_and_a_miss_says_where_it_diverges() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("words.txt");
    std::fs::write(&file, "alpha\r\nbeta\r\ngamma\r\n").unwrap();
    let kernel = kernel_over(HostRequestHandlers::new(), dir.path()).await;
    let replaced = value(kernel.as_ref(), "(edit \"words.txt\" \"beta\" \"BETA\")").await;
    let missed = value(kernel.as_ref(), "(edit \"words.txt\" \"BETx\" \"q\")").await;
    let repeated = value(kernel.as_ref(), "(edit \"words.txt\" \"a\" \"b\")").await;
    let shown = file.canonicalize().unwrap();
    assert_eq!(
        (replaced, missed, repeated, std::fs::read_to_string(&file).unwrap()),
        (
            Ok(json!({
                "path": shown.to_string_lossy(),
                "start-line": 2,
                "check": false,
                "text": format!("Edited {}", shown.display()),
            })),
            Err("Error: Generic: edit: string not found in words.txt: old-str matches its first 3 of 4 characters \
                 and diverges at line 2, column 4:\n  file continues    \"A\\r\\ngamma\\r\\n\"\n  \
                 old-str continues \"x\""
                .to_string()),
            Err("Error: Generic: edit: found 4 occurrences in words.txt, need exactly 1 — widen the snippet to make it unique".to_string()),
            "alpha\r\nBETA\r\ngamma\r\n".to_string(),
        )
    );
}

/// A TypeScript project under `root` whose `tsc` is `script`. A child process
/// writes it, so no thread of this one can hold it open for writing while it
/// is being started.
#[cfg(unix)]
fn typescript_project(root: &Path, script: &str) {
    std::fs::write(root.join("tsconfig.json"), "{}\n").unwrap();
    std::fs::write(root.join("app.ts"), "let total = 1\n").unwrap();
    let bin = root.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    let written = pa_types::session_scope::std_command("sh")
        .arg("-c")
        .arg("printf '%s\\n' \"$1\" > tsc && chmod +x tsc")
        .arg("sh")
        .arg(format!("#!/bin/sh\n{script}"))
        .current_dir(&bin)
        .status()
        .unwrap();
    assert!(written.success());
}

/// Handlers that take what the edit checks send: each `edit_check.fact`
/// outcome and each `bash.completed` notice, in order.
#[cfg(unix)]
fn check_handlers() -> (
    HostRequestHandlers,
    tokio::sync::mpsc::UnboundedReceiver<Value>,
) {
    let (sent, received) = tokio::sync::mpsc::unbounded_channel();
    let mut handlers = HostRequestHandlers::new();
    for request in ["edit_check.fact", "bash.completed"] {
        let sent = sent.clone();
        handlers.register(
            request,
            host_handler(move |payload: HostRequestPayload| {
                // The receiver outlives every kernel of its test.
                let _ = sent.send(payload.data);
                async { Ok(json!({})) }
            }),
        );
    }
    (handlers, received)
}

/// The next payload the edit checks sent.
#[cfg(unix)]
async fn sent_next(received: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(30), received.recv())
        .await
        .unwrap()
        .unwrap()
}

#[cfg(unix)]
const EDIT_APP: &str = "(let ([done (fs-edit \"app.ts\" (hash 'append \"total += 1\"))])
                          (list (hash-ref done 'check) (ends-with? (hash-ref done 'text) (hash-ref done 'check))))";

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_carries_its_files_check_and_the_outcome_is_counted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    typescript_project(
        &root,
        "echo \"app.ts(2,1): error TS1005: ';' expected.\"\nexit 2",
    );
    let (handlers, mut received) = check_handlers();
    let kernel = kernel_over(handlers, &root).await;
    let checked = value(kernel.as_ref(), EDIT_APP).await.unwrap();
    let fact = sent_next(&mut received).await;
    // The check's line ends in how long it took.
    let (headline, diagnostics) = checked[0].as_str().unwrap().split_once('\n').unwrap();
    let headline: Vec<&str> = headline.split(" · ").take(2).collect();
    assert_eq!(
        (headline, diagnostics, &checked[1], fact),
        (
            vec![
                format!(
                    "[check: {0}/node_modules/.bin/tsc --noEmit --pretty false -p {0}",
                    root.display()
                )
                .as_str(),
                "exit 2",
            ],
            "app.ts(2,1): error TS1005: ';' expected.",
            &json!(true),
            json!({"type": "edit_check.fact", "outcome": "found", "kind": "tsc"}),
        )
    );
}

/// The check blocks on a pipe the test opens only after the edit returned:
/// the edit never waits for it, and its diagnostics follow as a notice.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_check_lets_the_edit_return_and_reports_later_as_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let gate = root.join("gate");
    nix::unistd::mkfifo(&gate, nix::sys::stat::Mode::S_IRWXU).unwrap();
    typescript_project(
        &root,
        &format!(
            "read go < '{}'\necho \"app.ts(2,1): error TS1005: ';' expected.\"\nexit 2",
            gate.display()
        ),
    );
    let (handlers, mut received) = check_handlers();
    let kernel = kernel_over(handlers, &root).await;
    let checked = value(kernel.as_ref(), EDIT_APP).await.unwrap();
    let late = sent_next(&mut received).await;
    tokio::time::timeout(Duration::from_secs(30), tokio::fs::write(&gate, "go\n"))
        .await
        .unwrap()
        .unwrap();
    let found = sent_next(&mut received).await;
    let mut notice = sent_next(&mut received).await;
    let pid = notice["pid"].take();
    let tsc = format!(
        "{0}/node_modules/.bin/tsc --noEmit --pretty false -p {0}",
        root.display()
    );
    assert_eq!(
        (checked, late, found, pid.as_u64().is_some_and(|pid| pid > 0), notice),
        (
            json!([
                format!("[check: {tsc} · still running after 1.5 s; any diagnostics arrive as a [bash-done] notice]"),
                true,
            ]),
            json!({"type": "edit_check.fact", "outcome": "late", "kind": "tsc"}),
            json!({"type": "edit_check.fact", "outcome": "found", "kind": "tsc"}),
            true,
            json!({
                "type": "bash.completed",
                "pid": null,
                "command": format!("{tsc}  # check after editing app.ts"),
                "exitCode": 2,
                "output": "app.ts(2,1): error TS1005: ';' expected.",
                "outputTruncated": false,
            }),
        )
    );
}

/// `fs-write` creates a file (with its directories) and replaces one whole,
/// through the session's `fs.write` handler: each write is counted, a file
/// the model had read is then seen as what it wrote, and a directory is
/// refused.
#[tokio::test(flavor = "multi_thread")]
async fn fs_write_creates_and_replaces_files_through_the_session_handler() {
    let dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("old.txt"), "one\ntwo\n").unwrap();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(SeenFiles::new(
        Some(SpillStore::for_session_dir(session_dir.path())),
        /*window_turns*/ 8,
        /*observer*/ None,
    ));
    seen.bind(Arc::new(|| {
        Box::pin(async {
            ContextMark {
                turn: 3,
                compacted_at_ms: None,
            }
        })
    }));
    let mut handlers = HostRequestHandlers::new();
    let edited = Arc::clone(&heard);
    FsEditHost {
        observer: Some(Arc::new(move |fact| edited.lock().unwrap().push(fact))),
        seen: Some(seen),
        prefetch: None,
    }
    .register(&mut handlers);
    let kernel = kernel_over(handlers, dir.path()).await;
    let created = value(
        kernel.as_ref(),
        "(let ([done (fs-write \"deep/er/new.txt\" \"alpha\\nbeta\\n\")])
           (list (hash-ref done 'path) (hash-ref done 'created) (hash-ref done 'total-lines)))",
    )
    .await;
    let replaced = value(
        kernel.as_ref(),
        "(begin (fs-read \"old.txt\")
                (fs-write \"old.txt\" \"three\\n\")
                (hash-ref (hash-ref (fs-read \"old.txt\") 'seen) 'state))",
    )
    .await;
    let refused = value(kernel.as_ref(), "(fs-write \"deep\" \"x\")").await;
    assert_eq!(
        (
            [created, replaced, refused],
            std::fs::read_to_string(dir.path().join("deep/er/new.txt")).unwrap(),
            std::fs::read_to_string(dir.path().join("old.txt")).unwrap(),
            heard.lock().unwrap().clone(),
        ),
        (
            [
                Ok(json!(["deep/er/new.txt", true, 2])),
                Ok(json!("unchanged")),
                Err("Error: Generic: fs-write: deep is a directory".to_string()),
            ],
            "alpha\nbeta\n".to_string(),
            "three\n".to_string(),
            vec![
                EditFact::Written { created: true },
                EditFact::Read { lines: 2 },
                EditFact::Written { created: false },
                EditFact::Read { lines: 1 },
                EditFact::Failed {
                    operation: crate::fs_edit::FsEditOperation::Write
                },
            ],
        )
    );
}

/// A read's reply names the file's neighbours as handles, `(ctx-page ...)`
/// reads one, and both that read and an `fs-read` of the other neighbour
/// count as uses when the next read settles them.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_carries_related_handles_that_ctx_page_reads() {
    use crate::kernel::spill::content_id;
    use crate::kernel::spill::host::{register_ctx_handlers, CtxObserver};
    use crate::prefetch::{PrefetchFact, Prefetcher};
    let dir = tempfile::tempdir().unwrap();
    let session_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/store")).unwrap();
    for (name, text) in [
        ("src/store.rs", "mod cache;\n\npub struct Store;\n"),
        ("src/store/tests.rs", "use super::Store;\n"),
        ("src/store/cache.rs", "pub struct Cache;\n"),
    ] {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let store = SpillStore::for_session_dir(session_dir.path());
    let facts = Arc::new(Mutex::new(Vec::new()));
    let prefetch = Prefetcher::new(
        dir.path(),
        /*agent_dir*/ None,
        SymbolIndex::for_project(dir.path(), /*agent_dir*/ None),
        store.clone(),
        Some({
            let facts = Arc::clone(&facts);
            Arc::new(move |fact| facts.lock().unwrap().push(fact))
        }),
    )
    .without_budget();
    let mut handlers = HostRequestHandlers::new();
    FsEditHost {
        observer: None,
        seen: None,
        prefetch: Some(prefetch.clone()),
    }
    .register(&mut handlers);
    let observer: CtxObserver = Arc::new(move |_read, id: &str| prefetch.note_handle_read(id));
    register_ctx_handlers(&mut handlers, Some(&store), Some(&observer));
    let kernel = kernel_over(handlers, dir.path()).await;
    let read = value(
        kernel.as_ref(),
        "(let* ([view (fs-read \"src/store.rs\")]
                [related (hash-ref view 'related)])
           (list related
                 (hash-ref (ctx-page (hash-ref (car related) 'handle)) 'text)
                 (hash-ref view 'text)))",
    )
    .await
    .unwrap();
    let settled = value(
        kernel.as_ref(),
        "(hash-contains? (fs-read \"src/store/cache.rs\") 'related)",
    )
    .await;
    let handle = |text: &str| format!("out:{}", content_id(text));
    let (tests, cache) = (handle("use super::Store;\n"), handle("pub struct Cache;\n"));
    assert_eq!(
        (
            read[0].clone(),
            read[1].clone(),
            read[2].as_str().unwrap().lines().last().map(str::to_string),
            settled,
            facts.lock().unwrap().clone(),
        ),
        (
            json!([
                {"path": "src/store/tests.rs", "why": "test", "handle": tests},
                {"path": "src/store/cache.rs", "why": "import", "handle": cache},
            ]),
            json!("use super::Store;\n"),
            Some(format!(
                "[related, paged out (read with (ctx-page handle)): test src/store/tests.rs {tests} · import src/store/cache.rs {cache}]"
            )),
            Ok(json!(false)),
            vec![
                PrefetchFact {
                    issued: 2,
                    used: 0,
                    unused: 0,
                    skipped: false,
                },
                PrefetchFact {
                    issued: 0,
                    used: 2,
                    unused: 0,
                    skipped: false,
                },
            ],
        )
    );
}
