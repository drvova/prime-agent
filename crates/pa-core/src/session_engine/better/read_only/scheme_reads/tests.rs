use serde_json::json;

use super::*;

fn options(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(options) => options,
        other => panic!("not an object: {other}"),
    }
}

#[test]
fn a_cell_of_literal_reader_calls_parses_to_its_calls() {
    let calls = parse_eval_cell(
        "; look around\n(displayln (hash-ref (fs-read \"src/lib.rs\" (hash 'start 1 'end 40)) 'text))\n\
         (fs-grep \"fn \\\\w+\" (hash 'path \"src\" 'glob (list \"*.rs\" \"!target/**\")\n  'case \"smart\" 'word #t))\n\
         [fs-find (hash 'kind \"file\" 'max-depth 2)] (fs-find (hash 'glob \"**/*.toml\")) (display (fs-symbol \"parse\"))",
    );
    assert_eq!(
        calls,
        Ok(vec![
            FsCall {
                op: FsOp::Read,
                options: options(json!({ "path": "src/lib.rs", "start": 1, "end": 40 })),
            },
            FsCall {
                op: FsOp::Grep,
                options: options(json!({
                    "pattern": "fn \\w+",
                    "path": "src",
                    "glob": ["*.rs", "!target/**"],
                    "case": "smart",
                    "word": true,
                })),
            },
            FsCall {
                op: FsOp::Find,
                options: options(json!({ "kind": "file", "max-depth": 2 })),
            },
            FsCall {
                op: FsOp::Find,
                options: options(json!({ "glob": "**/*.toml" })),
            },
            FsCall {
                op: FsOp::Symbol,
                options: options(json!({ "name": "parse" })),
            },
        ])
    );
}

#[test]
fn anything_but_a_literal_reader_call_is_refused() {
    const SHAPE: &str = "each form must be one fs-read/fs-grep/fs-find/fs-symbol call";
    const NOT_A_READ: &str = "is not a read; only fs-read, fs-grep, fs-find, fs-symbol";
    for (code, reason) in [
        ("(define x (fs-read \"a\"))", SHAPE.to_string()),
        ("(bash \"rm -rf .\")", SHAPE.to_string()),
        ("(open-output-file \"a\")", SHAPE.to_string()),
        ("(begin (fs-read \"a\") (bash \"x\"))", SHAPE.to_string()),
        ("((lambda () (bash \"x\")))", SHAPE.to_string()),
        ("fs-read", SHAPE.to_string()),
        ("(fs-edit \"a\" (list))", format!("fs-edit {NOT_A_READ}")),
        ("(fs-write \"a\" \"x\")", format!("fs-write {NOT_A_READ}")),
        (
            "(fs-read path)",
            "`path` is a name, and only literal arguments are accepted".to_string(),
        ),
        (
            "(fs-read (bash \"echo a\"))",
            "only literal arguments are accepted, and a list is `(list ...)`".to_string(),
        ),
        (
            "(fs-read \"a\" (hash 'start (bash \"echo 1\")))",
            "only literal arguments are accepted, and a list is `(list ...)`".to_string(),
        ),
        (
            "(fs-read \"a\" options)",
            "fs-read: options are one `(hash 'name value ...)`".to_string(),
        ),
        (
            "(fs-read \"a\" (hash \"start\" 1))",
            "fs-read: an option's name is a quoted symbol".to_string(),
        ),
        ("(fs-read)", "fs-read: `path` is required".to_string()),
        ("(fs-read \"a\"", "a form is not closed".to_string()),
        (
            "(fs-read `(,x))",
            "`` ` `` is not accepted here".replace("`` ` ``", "```"),
        ),
        ("(fs-read #\\a)", "`#\\a` is not accepted here".to_string()),
        ("", "the cell holds no fs call".to_string()),
    ] {
        assert_eq!(parse_eval_cell(code), Err(reason), "{code}");
    }
}
