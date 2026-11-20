//! The parser registry: one line-oriented parser per tool output shape, each
//! built from captured real output (the samples under `fixtures/`). A parser
//! only recognizes the lines that name an error location, so running every
//! parser over every text is safe: a pytest report never matches the rustc
//! parser, and plain program output matches none.

use std::sync::LazyLock;

use fancy_regex::Regex;

/// The tool whose output named a location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    /// `rustc`/`cargo` diagnostics (`error[E0308]: …` then ` --> path:line:col`).
    Rustc,
    /// A Rust panic (`panicked at path:line:col:`), as `cargo test` prints it.
    RustPanic,
    /// A pytest failure report (`path.py:line: AssertionError`).
    Pytest,
    /// A Python traceback frame (`File "path", line N, in f`), unittest's included.
    Python,
    /// `tsc` (`path(line,col): error TS…` and its `--pretty` form).
    Tsc,
    /// `go build`/`go vet` (`path.go:line:col: …`) and `go test` (`path_test.go:line: …`).
    Go,
    /// gcc/clang (`path.c:line:col: error: …`).
    Cc,
}

impl Tool {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Rustc => "rustc",
            Self::RustPanic => "panic",
            Self::Pytest => "pytest",
            Self::Python => "traceback",
            Self::Tsc => "tsc",
            Self::Go => "go",
            Self::Cc => "cc",
        }
    }
}

/// One error location as the tool printed it (the path unresolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedLocation {
    pub(crate) path: String,
    pub(crate) line: u32,
    pub(crate) column: Option<u32>,
    pub(crate) tool: Tool,
    /// The error's one-line message, when the output gives one.
    pub(crate) message: String,
}

/// Every parser, in the order their locations are reported.
const REGISTRY: [fn(&[&str]) -> Vec<ParsedLocation>; 7] =
    [rustc, rust_panic, pytest, python_traceback, tsc, go, cc];

/// Characters of one message kept.
const MESSAGE_CHARS: usize = 160;

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a static parser pattern compiles")
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| regex(r"\x1b\[[0-9;?]*[ -/]*[@-~]"));
static RUSTC_HEADER: LazyLock<Regex> = LazyLock::new(|| regex(r"^(error(?:\[[A-Z]\d+\])?): (.+)$"));
static RUSTC_ARROW: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*--> (.+?):(\d+):(\d+)\s*$"));
static RUST_PANIC: LazyLock<Regex> =
    LazyLock::new(|| regex(r"panicked at (?:'(.*)', )?([^\s:'][^:']*?):(\d+):(\d+):?\s*$"));
static PYTEST_FRAME: LazyLock<Regex> = LazyLock::new(|| regex(r"^([^\s:]+\.py):(\d+): ?(\w*)\s*$"));
static PYTEST_E: LazyLock<Regex> = LazyLock::new(|| regex(r"^E\s+(\S.*)$"));
static PY_FRAME: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"^\s*File "([^"]+)", line (\d+)(?:, in .+)?\s*$"#));
static PY_EXCEPTION: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^([A-Za-z_][\w.]*(?:Error|Exception|Exit|Interrupt|Warning|Failure))\b:?(.*)$")
});
static TSC_PLAIN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(.+?)\((\d+),(\d+)\): error (TS\d+): (.+)$"));
static TSC_PRETTY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(.+?):(\d+):(\d+) - error (TS\d+): (.+)$"));
static GO_BUILD: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(?:vet: )?(\S+\.go):(\d+):(\d+): (.+)$"));
static GO_TEST: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s+(\S+\.go):(\d+): (.+)$"));
static CC: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^(\S+\.(?:c|h|cc|cpp|cxx|hpp|hh|m|mm)):(\d+):(\d+): (?:fatal )?error: (.+)$")
});

/// `text` without ANSI escape sequences (colored compiler output).
pub(crate) fn strip_ansi(text: &str) -> String {
    ANSI.replace_all(text, "").into_owned()
}

/// Every location every parser finds in `text`, in registry order, then
/// output order (a Python traceback's innermost frame first).
pub(crate) fn parse(text: &str) -> Vec<ParsedLocation> {
    let clean = strip_ansi(text);
    let lines: Vec<&str> = clean.lines().collect();
    REGISTRY.iter().flat_map(|parser| parser(&lines)).collect()
}

fn captures<'t>(regex: &Regex, line: &'t str) -> Option<fancy_regex::Captures<'t>> {
    regex.captures(line).ok().flatten()
}

fn group<'t>(captures: &fancy_regex::Captures<'t>, index: usize) -> &'t str {
    captures.get(index).map_or("", |m| m.as_str())
}

fn number(captures: &fancy_regex::Captures<'_>, index: usize) -> Option<u32> {
    group(captures, index).parse().ok()
}

fn message(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MESSAGE_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn location(
    tool: Tool,
    path: &str,
    line: Option<u32>,
    column: Option<u32>,
    text: &str,
) -> Option<ParsedLocation> {
    let line = line.filter(|line| *line > 0)?;
    Some(ParsedLocation {
        path: path.trim().to_string(),
        line,
        column,
        tool,
        message: message(text),
    })
}

/// `error[E0308]: mismatched types` followed (a few lines on) by
/// ` --> src/lib.rs:6:22`. Warnings are not failures and are skipped.
fn rustc(lines: &[&str]) -> Vec<ParsedLocation> {
    let mut found = Vec::new();
    let mut pending: Option<String> = None;
    for line in lines {
        if let Some(header) = captures(&RUSTC_HEADER, line) {
            pending = Some(format!("{}: {}", group(&header, 1), group(&header, 2)));
            continue;
        }
        if line.starts_with("warning") {
            pending = None;
            continue;
        }
        let Some(arrow) = captures(&RUSTC_ARROW, line) else {
            continue;
        };
        if let Some(text) = pending.take() {
            found.extend(location(
                Tool::Rustc,
                group(&arrow, 1),
                number(&arrow, 2),
                number(&arrow, 3),
                &text,
            ));
        }
    }
    found
}

/// `thread 't' panicked at src/lib.rs:11:9:` with the message on the next
/// line (the older one-line form carries it quoted before the location).
fn rust_panic(lines: &[&str]) -> Vec<ParsedLocation> {
    let mut found = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(panic) = captures(&RUST_PANIC, line) else {
            continue;
        };
        let quoted = group(&panic, 1);
        let text = if quoted.is_empty() {
            lines.get(at + 1).copied().unwrap_or_default()
        } else {
            quoted
        };
        found.extend(location(
            Tool::RustPanic,
            group(&panic, 2),
            number(&panic, 3),
            number(&panic, 4),
            text,
        ));
    }
    found
}

/// pytest's report: `E   assert 3.5 == 3` lines, then the frame line
/// `test_calc.py:11: AssertionError` (an intermediate frame has no type).
fn pytest(lines: &[&str]) -> Vec<ParsedLocation> {
    let mut found = Vec::new();
    let mut last_e: Option<&str> = None;
    for line in lines {
        if let Some(e) = captures(&PYTEST_E, line) {
            if last_e.is_none() {
                last_e = Some(group(&e, 1));
            }
            continue;
        }
        if line.starts_with('_') || line.starts_with('=') {
            last_e = None;
            continue;
        }
        let Some(frame) = captures(&PYTEST_FRAME, line) else {
            continue;
        };
        let kind = group(&frame, 3);
        let text = match (last_e.take(), kind.is_empty()) {
            (Some(e), _) => e.to_string(),
            (None, false) => kind.to_string(),
            (None, true) => String::new(),
        };
        found.extend(location(
            Tool::Pytest,
            group(&frame, 1),
            number(&frame, 2),
            None,
            &text,
        ));
    }
    found
}

/// A traceback's `File "…", line N` frames, innermost first, each carrying
/// the exception line that ends the traceback.
fn python_traceback(lines: &[&str]) -> Vec<ParsedLocation> {
    let mut found = Vec::new();
    let mut frames: Vec<(String, Option<u32>)> = Vec::new();
    let mut flush = |frames: &mut Vec<(String, Option<u32>)>, text: &str| {
        for (path, line) in frames.drain(..).rev() {
            found.extend(location(Tool::Python, &path, line, None, text));
        }
    };
    for line in lines {
        if let Some(frame) = captures(&PY_FRAME, line) {
            frames.push((group(&frame, 1).to_string(), number(&frame, 2)));
            continue;
        }
        if frames.is_empty() || line.starts_with(char::is_whitespace) {
            continue;
        }
        if captures(&PY_EXCEPTION, line).is_some() {
            flush(&mut frames, line);
        }
    }
    flush(&mut frames, "");
    found
}

/// `src/math.ts(5,7): error TS2322: …` and `src/math.ts:5:7 - error TS2322: …`.
fn tsc(lines: &[&str]) -> Vec<ParsedLocation> {
    lines
        .iter()
        .filter_map(|line| {
            let found = captures(&TSC_PLAIN, line).or_else(|| captures(&TSC_PRETTY, line))?;
            location(
                Tool::Tsc,
                group(&found, 1),
                number(&found, 2),
                number(&found, 3),
                &format!("{}: {}", group(&found, 4), group(&found, 5)),
            )
        })
        .collect()
}

/// `./calc.go:8:14: cannot use …` (build, vet) and `    calc_test.go:7: …` (test).
fn go(lines: &[&str]) -> Vec<ParsedLocation> {
    lines
        .iter()
        .filter_map(|line| {
            if let Some(build) = captures(&GO_BUILD, line) {
                return location(
                    Tool::Go,
                    group(&build, 1),
                    number(&build, 2),
                    number(&build, 3),
                    group(&build, 4),
                );
            }
            let test = captures(&GO_TEST, line)?;
            location(
                Tool::Go,
                group(&test, 1),
                number(&test, 2),
                None,
                group(&test, 3),
            )
        })
        .collect()
}

/// `main.c:4:17: error: expected ';' before '}' token` (notes and warnings skipped).
fn cc(lines: &[&str]) -> Vec<ParsedLocation> {
    lines
        .iter()
        .filter_map(|line| {
            let found = captures(&CC, line)?;
            location(
                Tool::Cc,
                group(&found, 1),
                number(&found, 2),
                number(&found, 3),
                &format!("error: {}", group(&found, 4)),
            )
        })
        .collect()
}
