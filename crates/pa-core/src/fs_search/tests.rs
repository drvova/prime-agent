//! `fs.grep`/`fs.glob`/`fs.find` over temporary trees, ripgrep parity when
//! `rg` is installed, and the host-request round trip.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};

/// A small git project: sources, an ignored build dir, a hidden dir, a
/// binary file, and a nested crate.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (path, contents) in [
        (".gitignore", "target/\n".as_bytes()),
        ("Cargo.toml", b"[package]\nname = \"demo\"\n"),
        (
            "src/lib.rs",
            b"// lib\npub fn alpha() {}\n\npub fn beta() -> u8 {\n    7\n}\n",
        ),
        ("src/nested/deep.rs", b"fn gamma() {}\r\n"),
        ("src/notes.md", b"fn in markdown\n"),
        ("target/debug/build.rs", b"fn ignored() {}\n"),
        (".hidden/secret.rs", b"fn hidden() {}\n"),
        ("data.bin", b"fn \x00binary\n"),
        ("tools/Cargo.toml", b"[package]\nname = \"tools\"\n"),
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, contents).unwrap();
    }
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/config"), "fn not_searched() {}\n").unwrap();
    dir
}

fn grep_request(root: &Path, pattern: &str) -> GrepRequest {
    GrepRequest {
        pattern: pattern.to_string(),
        root: root.to_path_buf(),
        cwd: root.to_path_buf(),
        globs: Vec::new(),
        types: Vec::new(),
        case: Case::Sensitive,
        literal: false,
        word: false,
        multiline: false,
        before: 0,
        after: 0,
        scope: Scope::PROJECT,
        max_results: 100,
        max_per_file: usize::MAX,
        max_columns: 300,
    }
}

fn found(path: &str, line: u64, column: usize, text: &str) -> GrepMatch {
    GrepMatch {
        path: path.to_string(),
        line,
        column,
        text: text.to_string(),
        extra_lines: 0,
        before: Vec::new(),
        after: Vec::new(),
    }
}

/// The walk honours `.gitignore`, skips hidden dirs, `.git` and binary
/// files, a type filter narrows it, and context attaches to its match; CRLF
/// line ends never reach the text.
#[test]
fn grep_matches_with_context_like_ripgrep() {
    let dir = project();
    let report = grep(&GrepRequest {
        types: vec!["rust".to_string()],
        before: 1,
        after: 1,
        ..grep_request(dir.path(), r"fn \w+")
    })
    .unwrap();
    assert_eq!(
        report,
        GrepReport {
            matches: vec![
                GrepMatch {
                    before: vec!["// lib".to_string()],
                    after: vec![String::new()],
                    ..found("src/lib.rs", 2, 5, "pub fn alpha() {}")
                },
                GrepMatch {
                    after: vec!["    7".to_string()],
                    ..found("src/lib.rs", 4, 5, "pub fn beta() -> u8 {")
                },
                found("src/nested/deep.rs", 1, 1, "fn gamma() {}"),
            ],
            files_searched: 2,
            files_matched: 2,
            truncated: false,
            unreadable: 0,
        }
    );
    assert_eq!(
        report.text(100),
        "src/lib.rs\n1-// lib\n2:pub fn alpha() {}\n3-\n4:pub fn beta() -> u8 {\n5-    7\n\nsrc/nested/deep.rs\n1:fn gamma() {}\n[3 matches in 2 files (2 searched)]"
    );
    // `hidden` and `gitignore=false` widen the walk; `.git` and binaries never match.
    let wide = grep(&GrepRequest {
        scope: Scope {
            hidden: true,
            gitignore: false,
        },
        ..grep_request(dir.path(), r"^fn \w+")
    })
    .unwrap();
    let paths: Vec<&str> = wide
        .matches
        .iter()
        .map(|found| found.path.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            ".hidden/secret.rs",
            "src/nested/deep.rs",
            "src/notes.md",
            "target/debug/build.rs"
        ]
    );
}

/// `max_results` stops the search and says so; long lines clip around the match.
#[test]
fn grep_caps_results_and_clips_long_lines() {
    let dir = tempfile::tempdir().unwrap();
    let long = format!("{}needle{}", "x".repeat(500), "y".repeat(500));
    std::fs::write(
        dir.path().join("a.txt"),
        format!("needle\nneedle\n{long}\n"),
    )
    .unwrap();
    let report = grep(&GrepRequest {
        max_results: 2,
        ..grep_request(dir.path(), "needle")
    })
    .unwrap();
    assert_eq!((report.matches.len(), report.truncated), (2, true));
    assert!(report.text(2).ends_with(
        "[2 matches in 1 file shown; stopped at max_results=2: more exist, narrow the pattern, path, glob or type]"
    ));
    let clipped = grep(&GrepRequest {
        max_columns: 30,
        cwd: dir.path().to_path_buf(),
        ..grep_request(&dir.path().join("a.txt"), "needle")
    })
    .unwrap();
    let last = clipped.matches.last().unwrap();
    assert_eq!(
        (last.path.as_str(), last.column, last.text.as_str()),
        (
            "a.txt",
            501,
            "\u{2026}xxxxxxxxxxneedleyyyyyyyyyyyyyy\u{2026}"
        )
    );
}

/// `fs.glob` matches `pathlib`-style globs over relative paths; `fs.find`
/// filters file names by glob, kind and depth. Both sort by path.
#[test]
fn glob_and_find_list_sorted_entries() {
    let dir = project();
    let root = dir.path().to_path_buf();
    let glob = glob(&GlobRequest {
        pattern: "src/**/*.rs".to_string(),
        root: root.clone(),
        cwd: root.clone(),
        scope: Scope::PROJECT,
        max_results: 500,
    })
    .unwrap();
    assert_eq!(
        glob.entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["src/lib.rs", "src/nested/deep.rs"]
    );
    let find_request = FindRequest {
        name: Some("cargo.TOML".to_string()),
        ignore_case: true,
        kind: Some(EntryKind::File),
        max_depth: None,
        root: root.clone(),
        cwd: root,
        scope: Scope::PROJECT,
        max_results: 500,
    };
    assert_eq!(
        find(&find_request).unwrap(),
        Listing {
            entries: vec![
                FsEntry {
                    path: "Cargo.toml".to_string(),
                    kind: EntryKind::File,
                    size: Some(24),
                },
                FsEntry {
                    path: "tools/Cargo.toml".to_string(),
                    kind: EntryKind::File,
                    size: Some(25),
                },
            ],
            truncated: false,
        }
    );
    let shallow_dirs = find(&FindRequest {
        name: None,
        ignore_case: false,
        kind: Some(EntryKind::Dir),
        max_depth: Some(1),
        ..find_request
    })
    .unwrap();
    assert_eq!(shallow_dirs.text(500), "src/\ntools/");
}

/// The Windows-path case: results print `/` separators relative to the REPL's
/// cwd on every platform (absolute when the root is outside it), and on
/// Windows a root and a glob typed with `\` work as with `/`.
#[test]
fn paths_print_with_slashes_relative_to_the_cwd() {
    let dir = project();
    let cwd = dir.path().join("src");
    let nested = dir.path().join("src").join("nested");
    let report = grep(&GrepRequest {
        cwd: cwd.clone(),
        ..grep_request(&nested, "gamma")
    })
    .unwrap();
    assert_eq!(report.matches[0].path, "nested/deep.rs");
    let outside = display_path(&dir.path().join("tools").join("Cargo.toml"), &cwd);
    assert!(Path::new(&outside).is_absolute());
    assert!(outside.ends_with("/tools/Cargo.toml"), "{outside}");
    assert!(!outside.contains('\\'), "{outside}");
    #[cfg(windows)]
    {
        let backslashed = std::path::PathBuf::from(dir.path().to_string_lossy().replace('/', "\\"));
        let glob = glob(&GlobRequest {
            pattern: r"src\**\*.rs".to_string(),
            root: backslashed.clone(),
            cwd: backslashed,
            scope: Scope::PROJECT,
            max_results: 500,
        })
        .unwrap();
        assert_eq!(
            glob.entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["src/lib.rs", "src/nested/deep.rs"]
        );
        assert_eq!(
            display_path(Path::new(r"D:\elsewhere\x.rs"), Path::new(r"C:\proj")),
            "D:/elsewhere/x.rs"
        );
    }
}

/// Parity with the `rg` binary on this repository's own sources, when `rg`
/// is installed: the same (path, line) set for a regex, a type filter and a
/// glob. Prints both timings (in-process search vs spawning `rg`).
#[test]
fn matches_ripgrep_on_the_workspace_sources() {
    let Ok(probe) = pa_types::session_scope::std_command("rg")
        .arg("--version")
        .output()
    else {
        eprintln!("rg is not installed; skipping the ripgrep parity check");
        return;
    };
    if !probe.status.success() {
        eprintln!("rg --version failed; skipping the ripgrep parity check");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let pattern = r"fn [a-z_]+_search\(";
    let started = std::time::Instant::now();
    let ours = grep(&GrepRequest {
        types: vec!["rust".to_string()],
        globs: vec!["!**/tests.rs".to_string()],
        max_results: GREP_MAX_RESULTS,
        cwd: root.clone(),
        ..grep_request(&root, pattern)
    })
    .unwrap();
    let in_process = started.elapsed();
    let started = std::time::Instant::now();
    let output = pa_types::session_scope::std_command("rg")
        .args([
            "--line-number",
            "--no-heading",
            "--color",
            "never",
            "-t",
            "rust",
            "-g",
            "!**/tests.rs",
            pattern,
            ".",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    let spawned = started.elapsed();
    let mut theirs: Vec<(String, u64)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ':');
            let path = parts.next()?.trim_start_matches("./").replace('\\', "/");
            Some((path, parts.next()?.parse().ok()?))
        })
        .collect();
    theirs.sort();
    let mut ours: Vec<(String, u64)> = ours
        .matches
        .iter()
        .map(|found| (found.path.clone(), found.line))
        .collect();
    ours.sort();
    assert!(!ours.is_empty());
    assert_eq!(ours, theirs);
    eprintln!(
        "fs.grep in-process {in_process:?} vs rg spawned {spawned:?} ({} matches)",
        ours.len()
    );
}

/// The `fs.*` handlers parse the payload, run off the runtime, and report a
/// count-only fact per call.
#[tokio::test]
async fn the_host_requests_round_trip_and_report_counts() {
    let dir = project();
    let facts: Arc<Mutex<Vec<SearchFact>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&facts);
    let mut handlers = HostRequestHandlers::new();
    FsSearchHost {
        observer: Some(Arc::new(move |fact| recorded.lock().unwrap().push(fact))),
    }
    .register(&mut handlers);
    let root = dir.path().to_string_lossy().to_string();
    let grep_reply = (handlers.get("fs.grep").unwrap())(HostRequestPayload {
        data: json!({"pattern": "alpha", "root": root, "cwd": root, "globs": "*.rs"}),
    })
    .await
    .unwrap();
    assert_eq!(
        grep_reply,
        json!({
            "matches": [{"path": "src/lib.rs", "line": 2, "column": 8, "text": "pub fn alpha() {}", "extraLines": 0, "before": [], "after": []}],
            "filesSearched": 2,
            "filesMatched": 1,
            "truncated": false,
            "unreadable": 0,
            "text": "src/lib.rs\n2:pub fn alpha() {}\n[1 match in 1 file (2 searched)]",
        })
    );
    let error = (handlers.get("fs.find").unwrap())(HostRequestPayload {
        data: json!({"root": "relative/dir", "cwd": root}),
    })
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "fs: root must be an absolute path");
    let facts: Vec<(FsOperation, Option<usize>, bool)> = facts
        .lock()
        .unwrap()
        .iter()
        .map(|fact| (fact.operation, fact.results, fact.truncated))
        .collect();
    assert_eq!(
        facts,
        [
            (FsOperation::Grep, Some(1), false),
            (FsOperation::Find, None, false)
        ]
    );
}
