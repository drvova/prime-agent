use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::parsers::{parse, ParsedLocation, Tool};
use super::{CellFailure, EnrichmentFact, ErrorContext, SourceReader};
use crate::kernel::shared::KernelCommandFailure;
use crate::kernel::spill::{SpillLookup, SpillStore};

fn at(tool: Tool, path: &str, line: u32, column: Option<u32>, message: &str) -> ParsedLocation {
    ParsedLocation {
        path: path.to_string(),
        line,
        column,
        tool,
        message: message.to_string(),
    }
}

/// Each captured real output parses to exactly the error locations it names,
/// and nothing else: warnings, notes, backtrace frames, and passing tests stay out.
#[test]
fn every_captured_sample_parses_to_its_error_locations() {
    let samples = [
        (
            include_str!("fixtures/cargo_build.txt"),
            vec![at(Tool::Rustc, "src/lib.rs", 6, Some(22), "error[E0308]: mismatched types")],
        ),
        (
            include_str!("fixtures/cargo_test.txt"),
            vec![
                at(Tool::RustPanic, "src/lib.rs", 26, Some(26), "index three is present"),
                at(Tool::RustPanic, "src/lib.rs", 11, Some(9), "assertion `left == right` failed"),
            ],
        ),
        (
            include_str!("fixtures/pytest.txt"),
            vec![
                at(Tool::Pytest, "test_calc.py", 11, None, "assert 3.5 == 3"),
                at(Tool::Pytest, "test_calc.py", 15, None, ""),
                at(Tool::Pytest, "calc.py", 6, None, "ValueError: invalid literal for int() with base 10: 'x1'"),
                at(Tool::Pytest, "test_calc.py", 19, None, "AssertionError: assert {'a': 1, 'b': 2} == {'a': 1, 'b': 3}"),
            ],
        ),
        (
            include_str!("fixtures/unittest.txt"),
            vec![
                at(Tool::Python, "/work/app/calc.py", 2, None, "ZeroDivisionError: division by zero"),
                at(Tool::Python, "/work/app/test_unit.py", 14, None, "ZeroDivisionError: division by zero"),
                at(Tool::Python, "/work/app/test_unit.py", 11, None, "AssertionError: 2.5 != 2"),
            ],
        ),
        (
            include_str!("fixtures/traceback.txt"),
            vec![
                at(Tool::Python, "/work/app/calc.py", 6, None, "ValueError: invalid literal for int() with base 10: 'three'"),
                at(Tool::Python, "/work/app/run.py", 6, None, "ValueError: invalid literal for int() with base 10: 'three'"),
                at(Tool::Python, "/work/app/run.py", 9, None, "ValueError: invalid literal for int() with base 10: 'three'"),
            ],
        ),
        (
            include_str!("fixtures/tsc.txt"),
            vec![
                at(Tool::Tsc, "src/math.ts", 5, Some(7), "TS2322: Type 'number' is not assignable to type 'string'."),
                at(Tool::Tsc, "src/math.ts", 6, Some(19), "TS2339: Property 'missing' does not exist on type 'string'."),
            ],
        ),
        (
            include_str!("fixtures/tsc_pretty.txt"),
            vec![
                at(Tool::Tsc, "src/math.ts", 5, Some(7), "TS2322: Type 'number' is not assignable to type 'string'."),
                at(Tool::Tsc, "src/math.ts", 6, Some(19), "TS2339: Property 'missing' does not exist on type 'string'."),
            ],
        ),
        (
            include_str!("fixtures/go_build.txt"),
            vec![
                at(Tool::Go, "./calc.go", 8, Some(14), "cannot use \"x\" (untyped string constant) as int value in variable declaration"),
                at(Tool::Go, "./calc.go", 9, Some(9), "cannot use n (variable of type int) as string value in return statement"),
            ],
        ),
        (
            include_str!("fixtures/go_vet.txt"),
            vec![at(Tool::Go, "./calc.go", 8, Some(14), "cannot use \"x\" (untyped string constant) as int value in variable declaration")],
        ),
        (
            include_str!("fixtures/go_test.txt"),
            vec![at(Tool::Go, "calc_test.go", 7, None, "Add(2, 2) = 5, want 4")],
        ),
        (
            include_str!("fixtures/gcc.txt"),
            vec![
                at(Tool::Cc, "main.c", 4, Some(17), "error: expected ';' before '}' token"),
                at(Tool::Cc, "main.c", 8, Some(27), "error: 'undefined_value' undeclared (first use in this function)"),
            ],
        ),
        (
            include_str!("fixtures/clang.txt"),
            vec![
                at(Tool::Cc, "main.c", 4, Some(17), "error: expected ';' after return statement"),
                at(Tool::Cc, "main.c", 8, Some(27), "error: use of undeclared identifier 'undefined_value'"),
            ],
        ),
    ];
    let parsed: Vec<Vec<ParsedLocation>> = samples.iter().map(|(text, _)| parse(text)).collect();
    let expected: Vec<Vec<ParsedLocation>> = samples
        .into_iter()
        .map(|(_, locations)| locations)
        .collect();
    assert_eq!(parsed, expected);
}

/// A workspace holding the sample crate's failing `src/lib.rs`.
fn sample_workspace() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(workspace.path().join("src")).expect("src");
    std::fs::write(
        workspace.path().join("src/lib.rs"),
        "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\npub fn label(n: u32) -> String {\n    let count: u32 = \"three\";\n    format!(\"{n} {count}\")\n}\n",
    )
    .expect("lib.rs");
    workspace
}

fn recorder() -> (Arc<Mutex<Vec<EnrichmentFact>>>, super::ErrorContextObserver) {
    let facts = Arc::new(Mutex::new(Vec::new()));
    let observer = {
        let facts = Arc::clone(&facts);
        Arc::new(move |fact| facts.lock().unwrap().push(fact)) as super::ErrorContextObserver
    };
    (facts, observer)
}

/// A failed `cargo build` the cell awaited (and never printed) returns its
/// error's source lines and a handle to the whole output in the same result.
#[tokio::test]
async fn a_failed_build_returns_its_error_lines_and_a_handle_to_its_output() {
    let workspace = sample_workspace();
    let session = tempfile::tempdir().expect("session dir");
    let store = SpillStore::for_session_dir(session.path());
    let (facts, observer) = recorder();
    let context = ErrorContext::for_workspace(workspace.path().to_path_buf(), Some(observer));
    let output = include_str!("fixtures/cargo_build.txt");
    let commands = [KernelCommandFailure {
        command: "cargo build".to_string(),
        exit_code: 101,
        output: output.to_string(),
    }];
    let block = context
        .enrich(
            CellFailure {
                traceback: None,
                streams: ["", ""],
                commands: &commands,
            },
            Some(&store),
        )
        .await;
    let id = crate::kernel::spill::content_id(output);
    assert_eq!(
        (block, facts.lock().unwrap().clone()),
        (
            Some(
                [
                    "[error context: 1 location · source ±3 lines]".to_string(),
                    "src/lib.rs:6:22 (rustc) error[E0308]: mismatched types".to_string(),
                    "  3 | }".to_string(),
                    "  4 | ".to_string(),
                    "  5 | pub fn label(n: u32) -> String {".to_string(),
                    "> 6 |     let count: u32 = \"three\";".to_string(),
                    "  7 |     format!(\"{n} {count}\")".to_string(),
                    "  8 | }".to_string(),
                    format!("[failed command (exit 101): cargo build · full output out:{id} · 385 chars · 11 lines]"),
                ]
                .join("\n")
            ),
            vec![EnrichmentFact {
                failed_commands: 1,
                locations: 1,
                skipped: 0,
                timed_out: false,
            }],
        )
    );
    match store.lookup(&id).await {
        SpillLookup::Stored(stored) => assert_eq!(stored.text, output),
        other => panic!("the handle names the whole output, got {other:?}"),
    }
}

/// A traceback through installed packages, files outside the workspace, and
/// the cell itself attaches only the workspace's own files, each line once.
#[tokio::test]
async fn only_the_workspaces_own_files_get_their_lines() {
    let workspace = sample_workspace();
    let root = workspace.path();
    std::fs::create_dir_all(root.join(".venv/lib/python3.13/site-packages/lib")).expect("venv");
    std::fs::write(
        root.join(".venv/lib/python3.13/site-packages/lib/core.py"),
        "def call():\n    raise ValueError('x')\n",
    )
    .expect("package file");
    std::fs::write(
        root.join("app.py"),
        "import lib\n\n\ndef run():\n    lib.call()\n",
    )
    .expect("app");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("other.py"), "x = 1\ny = 2\n").expect("other");
    let traceback = format!(
        "Traceback (most recent call last):\n  File \"<cell-3>\", line 1, in <module>\n    run()\n  File \"{app}\", line 5, in run\n    lib.call()\n  File \"{other}\", line 2, in <module>\n    y = 2\n  File \"{package}\", line 2, in call\n    raise ValueError('x')\nValueError: x\n",
        app = root.join("app.py").display(),
        other = outside.path().join("other.py").display(),
        package = root
            .join(".venv/lib/python3.13/site-packages/lib/core.py")
            .display(),
    );
    let (facts, observer) = recorder();
    let context = ErrorContext::for_workspace(root.to_path_buf(), Some(observer));
    let block = context
        .enrich(
            CellFailure {
                traceback: Some(&traceback),
                // The same traceback echoed on stderr adds no second copy.
                streams: ["", &traceback],
                commands: &[],
            },
            /*spill*/ None,
        )
        .await;
    assert_eq!(
        (block, facts.lock().unwrap().clone()),
        (
            Some(
                [
                    "[error context: 1 location · source ±3 lines]",
                    "app.py:5 (traceback) ValueError: x",
                    "  2 | ",
                    "  3 | ",
                    "  4 | def run():",
                    "> 5 |     lib.call()",
                    "[2 more locations not shown]",
                ]
                .join("\n")
            ),
            vec![EnrichmentFact {
                failed_commands: 0,
                locations: 1,
                skipped: 2,
                timed_out: false,
            }],
        )
    );
}

/// A file system that never answers.
struct HangingReader {
    /// Held until the test ends, so the blocked worker never returns early.
    gate: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl SourceReader for HangingReader {
    fn canonical(&self, path: &Path) -> std::io::Result<PathBuf> {
        let _ = self.gate.lock().unwrap().recv();
        Ok(path.to_path_buf())
    }

    fn read(&self, _path: &Path) -> std::io::Result<String> {
        let _ = self.gate.lock().unwrap().recv();
        Err(std::io::Error::other("released"))
    }
}

/// A source read that never answers (a hung network mount) never holds the
/// cell result: enrichment gives up at its budget and the result goes out
/// without the lines.
#[tokio::test]
async fn a_hanging_source_read_never_holds_the_cell_result() {
    let (release, gate) = std::sync::mpsc::channel();
    let (facts, observer) = recorder();
    let context = ErrorContext::with_reader(
        PathBuf::from("/work/app"),
        Arc::new(HangingReader {
            gate: Mutex::new(gate),
        }),
        Duration::from_millis(50),
        Some(observer),
    );
    let commands = [KernelCommandFailure {
        command: "cargo build".to_string(),
        exit_code: 101,
        output: include_str!("fixtures/cargo_build.txt").to_string(),
    }];
    let block = tokio::time::timeout(
        Duration::from_secs(10),
        context.enrich(
            CellFailure {
                traceback: None,
                streams: ["", ""],
                commands: &commands,
            },
            /*spill*/ None,
        ),
    )
    .await
    .expect("the enrichment returned at its budget");
    drop(release);
    assert_eq!(
        (block, facts.lock().unwrap().clone()),
        (
            None,
            vec![EnrichmentFact {
                failed_commands: 1,
                locations: 0,
                skipped: 1,
                timed_out: true,
            }],
        )
    );
}

/// A cell that raised from many workspace files shows at most the location cap.
#[tokio::test]
async fn the_block_stays_within_its_location_cap() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut traceback = String::from("Traceback (most recent call last):\n");
    for n in 0..9 {
        let path = workspace.path().join(format!("m{n}.py"));
        std::fs::write(&path, "def f():\n    g()\n").expect("module");
        traceback.push_str(&format!(
            "  File \"{}\", line 2, in f\n    g()\n",
            path.display()
        ));
    }
    traceback.push_str("RecursionError: deep\n");
    let context = ErrorContext::for_workspace(workspace.path().to_path_buf(), None);
    let block = context
        .enrich(
            CellFailure {
                traceback: Some(&traceback),
                streams: ["", ""],
                commands: &[],
            },
            /*spill*/ None,
        )
        .await
        .expect("a block");
    let shown: Vec<String> = block
        .lines()
        .filter(|line| line.contains(" (traceback) "))
        .map(str::to_string)
        .collect();
    // The innermost frames first: m8 down to m3.
    let expected: Vec<String> = (3..9)
        .rev()
        .map(|n| format!("m{n}.py:2 (traceback) RecursionError: deep"))
        .collect();
    assert_eq!(
        (shown, block.lines().last()),
        (expected, Some("[3 more locations not shown]"))
    );
}
