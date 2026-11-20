//! `fs.read`/`fs.edit` over temporary files: anchors, stale rejection,
//! layout checks, line ends, encodings, and the host-request round trip.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};

fn target(dir: &Path, name: &str) -> Target {
    Target {
        path: dir.join(name),
        shown: name.to_string(),
    }
}

fn read_file(dir: &Path, name: &str) -> (Value, usize) {
    read(&ReadRequest {
        target: target(dir, name),
        start: 1,
        end: None,
    })
    .unwrap()
}

/// The anchor `fs.read` shows for `line` of the file.
fn anchor_at(dir: &Path, name: &str, line: usize) -> String {
    let (reply, _) = read_file(dir, name);
    reply["lines"][line - 1]["anchor"]
        .as_str()
        .unwrap()
        .to_string()
}

fn run_edit(dir: &Path, name: &str, edits: &Value) -> anyhow::Result<(Value, EditFact)> {
    let root = dir.to_string_lossy();
    let (reply, fact, _) = edit(&parse_edit(&json!({
        "path": dir.join(name).to_string_lossy(),
        "cwd": root,
        "edits": edits,
    }))?)?;
    Ok((reply, fact))
}

/// Every file name in `dir` (a failed or refused edit leaves no temp file).
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn read_tags_every_line_with_an_anchor() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "fn main() {\n    let x = 1;\n}\n",
    )
    .unwrap();
    let (reply, lines) = read_file(dir.path(), "lib.rs");
    assert_eq!(lines, 3);
    assert_eq!(
        reply,
        json!({
            "path": "lib.rs",
            "start": 1,
            "end": 3,
            "totalLines": 3,
            "lines": [
                {"line": 1, "anchor": "1:ywf", "text": "fn main() {"},
                {"line": 2, "anchor": "2:kbg", "text": "    let x = 1;"},
                {"line": 3, "anchor": "3:bno", "text": "}"},
            ],
            "truncated": false,
            "lineEnds": "lf",
            "encoding": "utf-8",
            "finalNewline": true,
            "text": "lib.rs (lines 1-3 of 3)\n1:ywf|fn main() {\n2:kbg|    let x = 1;\n3:bno|}",
        })
    );
    let (window, _) = read(&ReadRequest {
        target: target(dir.path(), "lib.rs"),
        start: 2,
        end: Some(2),
    })
    .unwrap();
    assert_eq!(
        window["text"],
        "lib.rs (lines 2-2 of 3)\n2:kbg|    let x = 1;"
    );
}

/// An anchor hashes its line with both neighbours: it moves when either
/// neighbour changes, not when a line further away does, and equal lines at
/// different places get different anchors.
#[test]
fn anchors_cover_the_line_and_its_neighbours() {
    let lines = |texts: &[&str]| -> Vec<document::Line> {
        texts
            .iter()
            .map(|text| document::Line {
                text: (*text).to_string(),
                eol: document::Eol::Lf,
            })
            .collect()
    };
    let base = lines(&["a", "b", "c", "d", "e"]);
    let anchor = |lines: &[document::Line]| Anchor::of(lines, 2);
    assert_ne!(anchor(&base), anchor(&lines(&["a", "B", "c", "d", "e"])));
    assert_ne!(anchor(&base), anchor(&lines(&["a", "b", "c", "D", "e"])));
    assert_eq!(anchor(&base), anchor(&lines(&["A", "b", "c", "d", "E"])));
    let repeated = lines(&["}", "}", "}"]);
    assert_ne!(Anchor::of(&repeated, 0), Anchor::of(&repeated, 1));
    assert_eq!(
        [
            Anchor::parse("12:abc").unwrap(),
            Anchor::parse("> 12:ABC|    let x = 1;").unwrap(),
        ],
        [Anchor {
            line: 12,
            hash: *b"abc"
        }; 2]
    );
    for bad in ["12:ab", "0:abc", "abc", "12:a1c", "x:abc"] {
        assert!(Anchor::parse(bad).is_err(), "{bad}");
    }
}

/// The red-first stale-file case: the file changed after the read, so the
/// anchors diverge; the whole patch is rejected with the stale anchors and
/// the current lines around them, and nothing is written.
#[test]
fn a_stale_file_is_rejected_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.py");
    std::fs::write(&path, "import os\n\ndef run():\n    return 1\n").unwrap();
    let run = anchor_at(dir.path(), "app.py", 3);
    let body = anchor_at(dir.path(), "app.py", 4);
    // Another writer inserts a line above: every anchor below it moves.
    let changed = "import os\nimport sys\n\ndef run():\n    return 1\n";
    std::fs::write(&path, changed).unwrap();
    let (reply, fact) = run_edit(
        dir.path(),
        "app.py",
        &json!([
            {"op": "replace", "anchor": body, "text": "    return 2"},
            {"op": "insert_before", "anchor": run, "text": "# runs"},
        ]),
    )
    .unwrap();
    assert_eq!(fact, EditFact::Rejected { stale: 2 });
    let now_3 = anchor_at(dir.path(), "app.py", 3);
    let now_4 = anchor_at(dir.path(), "app.py", 4);
    let rows: Vec<String> = (1..=5)
        .map(|line| anchor_at(dir.path(), "app.py", line))
        .collect();
    assert_eq!(
        reply,
        json!({
            "applied": false,
            "path": "app.py",
            "stale": [
                {"edit": 1, "anchor": body, "current": now_4},
                {"edit": 2, "anchor": run, "current": now_3},
            ],
            "lines": [
                {"line": 1, "anchor": rows[0], "text": "import os"},
                {"line": 2, "anchor": rows[1], "text": "import sys"},
                {"line": 3, "anchor": rows[2], "text": ""},
                {"line": 4, "anchor": rows[3], "text": "def run():"},
                {"line": 5, "anchor": rows[4], "text": "    return 1"},
            ],
            "text": format!(
                "fs.edit rejected, nothing was written: 2 of 2 anchors in app.py no longer match the file.\n\
                 - edit 1: {body} is now {now_4} (the line or a neighbour changed)\n\
                 - edit 2: {run} is now {now_3} (the line or a neighbour changed)\n\
                 Current lines (> marks a stale anchor's line):\n  \
                 {}|import os\n  {}|import sys\n> {}|\n> {}|def run():\n  {}|    return 1",
                rows[0], rows[1], rows[2], rows[3], rows[4]
            ),
        })
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
    assert_eq!(entries(dir.path()), ["app.py"]);
    // An anchor past the end of a shrunk file is stale too.
    std::fs::write(&path, "import os\n").unwrap();
    let (reply, _) = run_edit(
        dir.path(),
        "app.py",
        &json!([{"op": "delete", "anchor": body}]),
    )
    .unwrap();
    assert_eq!(
        reply["stale"],
        json!([{"edit": 1, "anchor": body, "current": null}])
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "import os\n");
}

/// One patch: replace a range, insert after and before, delete, append; all
/// anchors address the file as read. The reply carries fresh anchors for the
/// changed regions, and the next edit chains from them without a re-read.
#[test]
fn a_multi_edit_patch_applies_atomically_with_fresh_anchors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("list.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
    let at = |line| anchor_at(dir.path(), "list.txt", line);
    let (reply, fact) = run_edit(
        dir.path(),
        "list.txt",
        &json!([
            {"op": "replace", "anchor": at(2), "end": at(3), "text": "TWO-THREE\n"},
            {"op": "insert_before", "anchor": at(1), "text": ["zero"]},
            {"op": "delete", "anchor": at(5)},
            {"op": "insert_after", "anchor": at(6), "text": "seven\neight"},
        ]),
    )
    .unwrap();
    assert_eq!(fact, EditFact::Applied { edits: 4 });
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "zero\none\nTWO-THREE\nfour\nsix\nseven\neight\n"
    );
    let fresh: Vec<String> = (1..=7).map(at).collect();
    let row =
        |line: usize, text: &str| json!({"line": line, "anchor": fresh[line - 1], "text": text});
    assert_eq!(
        reply,
        json!({
            "applied": true,
            "path": "list.txt",
            "edits": 4,
            "removed": 3,
            "added": 4,
            "totalLines": 7,
            "hunks": [{
                "start": 1,
                "end": 7,
                "lines": [
                    row(1, "zero"), row(2, "one"), row(3, "TWO-THREE"), row(4, "four"),
                    row(5, "six"), row(6, "seven"), row(7, "eight"),
                ],
            }],
            "text": format!(
                "list.txt: 4 edits applied, -3 +4 lines, 7 total\n{}|zero\n{}|one\n{}|TWO-THREE\n{}|four\n{}|six\n{}|seven\n{}|eight",
                fresh[0], fresh[1], fresh[2], fresh[3], fresh[4], fresh[5], fresh[6]
            ),
        })
    );
    // Chain from the reply's own anchors, without a re-read.
    let (chained, _) = run_edit(
        dir.path(),
        "list.txt",
        &json!([{"op": "replace", "anchor": reply["hunks"][0]["lines"][5]["anchor"], "text": "SEVEN"}]),
    )
    .unwrap();
    assert_eq!(chained["applied"], true);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "zero\none\nTWO-THREE\nfour\nsix\nSEVEN\neight\n"
    );
    assert_eq!(entries(dir.path()), ["list.txt"]);
}

/// Overlapping ranges, an insert inside a replaced range, two inserts at one
/// place, a backward range, and a no-op are each rejected before anything is
/// written.
#[test]
fn overlapping_and_empty_patches_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    let original = "a\nb\nc\nd\n";
    std::fs::write(&path, original).unwrap();
    let at = |line| anchor_at(dir.path(), "f.txt", line);
    let cases = [
        (
            json!([
                {"op": "replace", "anchor": at(1), "end": at(3), "text": "x"},
                {"op": "delete", "anchor": at(3), "end": at(4)},
            ]),
            "edits 1 and 2 overlap (lines 1-3 and lines 3-4); merge them into one edit",
        ),
        (
            json!([
                {"op": "replace", "anchor": at(1), "end": at(3), "text": "x"},
                {"op": "insert_after", "anchor": at(2), "text": "y"},
            ]),
            "edit 2 inserts inside the lines 1-3 edit 1 replaces; merge them into one edit",
        ),
        (
            json!([
                {"op": "insert_after", "anchor": at(2), "text": "x"},
                {"op": "insert_before", "anchor": at(3), "text": "y"},
            ]),
            "edits 1 and 2 insert at the same place; merge them into one edit",
        ),
        (
            json!([{"op": "replace", "anchor": at(3), "end": at(1), "text": "x"}]),
            &*format!(
                "edit 1: the range ends ({}) before it starts ({})",
                at(1),
                at(3)
            ),
        ),
        (
            json!([{"op": "replace", "anchor": at(2), "text": "b"}]),
            "the patch changes nothing in f.txt; nothing was written",
        ),
        (
            json!([{"op": "insert_after", "anchor": at(2), "text": ""}]),
            "edit 1: insert_after needs text to insert",
        ),
    ];
    for (edits, message) in cases {
        let error = run_edit(dir.path(), "f.txt", &edits).unwrap_err();
        assert_eq!(error.to_string(), message);
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(entries(dir.path()), ["f.txt"]);
}

/// CRLF files stay CRLF (new lines included), a missing final newline stays
/// missing, and an empty file reads empty and fills by `append`.
#[test]
fn line_ends_and_the_final_newline_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let crlf = dir.path().join("win.txt");
    std::fs::write(&crlf, "alpha\r\nbeta\r\n").unwrap();
    let (reply, _) = read_file(dir.path(), "win.txt");
    assert_eq!(
        (&reply["lineEnds"], &reply["lines"][0]["text"]),
        (&json!("crlf"), &json!("alpha"))
    );
    assert!(reply["text"]
        .as_str()
        .unwrap()
        .starts_with("win.txt (lines 1-2 of 2, crlf)\n"));
    let first = anchor_at(dir.path(), "win.txt", 1);
    run_edit(
        dir.path(),
        "win.txt",
        &json!([{"op": "insert_after", "anchor": first, "text": "one\r\ntwo\n"}]),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(&crlf).unwrap(),
        b"alpha\r\none\r\ntwo\r\nbeta\r\n"
    );

    let open = dir.path().join("open.txt");
    std::fs::write(&open, "a\nb").unwrap();
    let (reply, _) = read_file(dir.path(), "open.txt");
    assert_eq!(
        (&reply["finalNewline"], &reply["text"]),
        (
            &json!(false),
            &json!(format!(
                "open.txt (lines 1-2 of 2, no final newline)\n{}|a\n{}|b",
                anchor_at(dir.path(), "open.txt", 1),
                anchor_at(dir.path(), "open.txt", 2)
            ))
        )
    );
    run_edit(
        dir.path(),
        "open.txt",
        &json!([{"op": "append", "text": "c"}]),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&open).unwrap(), "a\nb\nc");
    let last = anchor_at(dir.path(), "open.txt", 3);
    run_edit(
        dir.path(),
        "open.txt",
        &json!([{"op": "delete", "anchor": last}]),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&open).unwrap(), "a\nb");

    let empty = dir.path().join("empty.txt");
    std::fs::write(&empty, "").unwrap();
    assert_eq!(
        read_file(dir.path(), "empty.txt"),
        (
            json!({
                "path": "empty.txt",
                "start": 0,
                "end": 0,
                "totalLines": 0,
                "lines": [],
                "truncated": false,
                "lineEnds": "none",
                "encoding": "utf-8",
                "finalNewline": true,
                "text": "empty.txt (empty)",
            }),
            0
        )
    );
    let (reply, _) = run_edit(
        dir.path(),
        "empty.txt",
        &json!([{"op": "append", "text": "first"}]),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&empty).unwrap(), "first\n");
    assert_eq!(
        reply["text"],
        format!(
            "empty.txt: 1 edit applied, -0 +1 lines, 1 total\n{}|first",
            anchor_at(dir.path(), "empty.txt", 1)
        )
    );
}

/// Unicode lines edit by anchor like any other; UTF-8 with a byte-order
/// mark and UTF-16 files write back in their own encoding; binary and
/// undecodable files are refused.
#[test]
fn unicode_and_encodings_round_trip_and_binary_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let unicode = dir.path().join("i18n.txt");
    std::fs::write(&unicode, "héllo wörld\n日本語の行\n🦀 crab\n").unwrap();
    let (reply, _) = read_file(dir.path(), "i18n.txt");
    assert_eq!(reply["lines"][1]["text"], "日本語の行");
    let second = anchor_at(dir.path(), "i18n.txt", 2);
    run_edit(
        dir.path(),
        "i18n.txt",
        &json!([{"op": "replace", "anchor": second, "text": "中文行 ✓"}]),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&unicode).unwrap(),
        "héllo wörld\n中文行 ✓\n🦀 crab\n"
    );

    let bom = dir.path().join("bom.txt");
    std::fs::write(&bom, b"\xEF\xBB\xBFx\ny\n").unwrap();
    let first = anchor_at(dir.path(), "bom.txt", 1);
    run_edit(
        dir.path(),
        "bom.txt",
        &json!([{"op": "replace", "anchor": first, "text": "é"}]),
    )
    .unwrap();
    assert_eq!(std::fs::read(&bom).unwrap(), b"\xEF\xBB\xBF\xC3\xA9\ny\n");

    let utf16 = dir.path().join("wide.txt");
    let encode = |text: &str| -> Vec<u8> {
        [0xFF, 0xFE]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect()
    };
    std::fs::write(&utf16, encode("ünï\r\ncode\r\n")).unwrap();
    let (reply, _) = read_file(dir.path(), "wide.txt");
    assert_eq!(
        reply["text"],
        format!(
            "wide.txt (lines 1-2 of 2, crlf, utf-16le)\n{}|ünï\n{}|code",
            anchor_at(dir.path(), "wide.txt", 1),
            anchor_at(dir.path(), "wide.txt", 2)
        )
    );
    let last = anchor_at(dir.path(), "wide.txt", 2);
    run_edit(
        dir.path(),
        "wide.txt",
        &json!([{"op": "insert_after", "anchor": last, "text": "ω"}]),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(&utf16).unwrap(),
        encode("ünï\r\ncode\r\nω\r\n")
    );

    std::fs::write(dir.path().join("blob.bin"), b"ELF\x00\x01\x02\n").unwrap();
    std::fs::write(dir.path().join("latin1.txt"), b"caf\xE9\n").unwrap();
    let refused = |name: &str| {
        read(&ReadRequest {
            target: target(dir.path(), name),
            start: 1,
            end: None,
        })
        .unwrap_err()
        .to_string()
    };
    assert_eq!(
        [
            refused("blob.bin"),
            refused("latin1.txt"),
            refused("missing.txt")
        ],
        [
            "blob.bin is a binary file".to_string(),
            "latin1.txt is not UTF-8 text (or UTF-16 with a byte-order mark)".to_string(),
            "missing.txt does not exist".to_string(),
        ]
    );
}

/// The atomic replace writes through a symlink (the link survives) and keeps
/// the file's permissions.
#[cfg(unix)]
#[test]
fn edits_write_through_symlinks_and_keep_permissions() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.sh");
    std::fs::write(&real, "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink("real.sh", dir.path().join("link.sh")).unwrap();
    let echo = anchor_at(dir.path(), "link.sh", 2);
    run_edit(
        dir.path(),
        "link.sh",
        &json!([{"op": "replace", "anchor": echo, "text": "echo bye"}]),
    )
    .unwrap();
    assert!(std::fs::symlink_metadata(dir.path().join("link.sh"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        (
            std::fs::read_to_string(&real).unwrap(),
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777
        ),
        ("#!/bin/sh\necho bye\n".to_string(), 0o750)
    );
    assert_eq!(entries(dir.path()), ["link.sh", "real.sh"]);
}

/// The Windows-path case: replies show the path relative to the REPL's cwd
/// with `/` on every platform, and on Windows a backslashed path reads and
/// edits like a slashed one.
#[test]
fn paths_show_relative_to_the_cwd_with_slashes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src").join("nested")).unwrap();
    let file = dir.path().join("src").join("nested").join("mod.rs");
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    let request = |path: &Path, cwd: &Path| {
        parse_read(&json!({"path": path.to_string_lossy(), "cwd": cwd.to_string_lossy()})).unwrap()
    };
    let (reply, _) = read(&request(&file, dir.path())).unwrap();
    assert_eq!(reply["path"], "src/nested/mod.rs");
    #[cfg(windows)]
    {
        let backslashed = std::path::PathBuf::from(file.to_string_lossy().replace('/', "\\"));
        let cwd = std::path::PathBuf::from(dir.path().to_string_lossy().replace('/', "\\"));
        let (reply, _) = read(&request(&backslashed, &cwd)).unwrap();
        assert_eq!(reply["path"], "src/nested/mod.rs");
        let anchor = reply["lines"][0]["anchor"].as_str().unwrap().to_string();
        let edited = edit(
            &parse_edit(&json!({
                "path": backslashed.to_string_lossy(),
                "cwd": cwd.to_string_lossy(),
                "edits": [{"op": "replace", "anchor": anchor, "text": "pub fn g() {}"}],
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(edited.0["path"], "src/nested/mod.rs");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "pub fn g() {}\n");
    }
    assert_eq!(
        parse_read(&json!({"path": "src/x.rs", "cwd": dir.path().to_string_lossy()}))
            .unwrap_err()
            .to_string(),
        "path must be an absolute path"
    );
}

/// The `fs.read`/`fs.edit` handlers parse the payload, run off the runtime,
/// prefix errors with the operation, and report a count-only fact per call.
#[tokio::test]
async fn the_host_requests_round_trip_and_report_facts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\ny\n").unwrap();
    let facts: Arc<Mutex<Vec<EditFact>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&facts);
    let mut handlers = HostRequestHandlers::new();
    FsEditHost {
        observer: Some(Arc::new(move |fact| recorded.lock().unwrap().push(fact))),
        seen: None,
    }
    .register(&mut handlers);
    let root = dir.path().to_string_lossy().to_string();
    let path = dir.path().join("a.txt").to_string_lossy().to_string();
    let call =
        |request: &str, data: Value| (handlers.get(request).unwrap())(HostRequestPayload { data });
    let read_reply = call("fs.read", json!({"path": path, "cwd": root}))
        .await
        .unwrap();
    let anchor = read_reply["lines"][1]["anchor"].clone();
    let edit_reply = call(
        "fs.edit",
        json!({"path": path, "cwd": root, "edits": [{"op": "replace", "anchor": anchor, "text": "z"}]}),
    )
    .await
    .unwrap();
    assert_eq!(edit_reply["applied"], true);
    let stale_reply = call(
        "fs.edit",
        json!({"path": path, "cwd": root, "edits": [{"op": "delete", "anchor": anchor}]}),
    )
    .await
    .unwrap();
    assert_eq!(stale_reply["applied"], false);
    let error = call(
        "fs.edit",
        json!({"path": path, "cwd": root, "edits": [{"op": "rename", "anchor": anchor}]}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "fs.edit: edit 1: unknown op \"rename\" (replace, delete, insert_after, insert_before, append)"
    );
    assert_eq!(
        *facts.lock().unwrap(),
        [
            EditFact::Read { lines: 2 },
            EditFact::Applied { edits: 1 },
            EditFact::Rejected { stale: 1 },
            EditFact::Failed {
                operation: FsEditOperation::Edit
            },
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "x\nz\n"
    );
}
