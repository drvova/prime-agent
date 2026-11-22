//! Typed summaries of test runs: the counts and each failure, parsed from the
//! runner's output.
//!
//! A waited command that runs a test suite (cargo test, pytest, unittest, go
//! test, jest, vitest) returns its result with `'tests`: the
//! passed/failed/skipped counts and, for each failure, its name, location,
//! assertion text (the diff the runner printed), and a handle to that
//! failure's own section of the output. The whole output stays on the result
//! and, when the session keeps a spill store, under the summary's own handle.
//! Each parser follows the Python kernel's, built from captured real output;
//! a run no parser recognizes gets no summary.

use std::collections::HashMap;
use std::sync::LazyLock;

use fancy_regex::{Captures, Regex};
use serde_json::{json, Value};

/// Failures the rendered summary shows, and how much of each message.
const SHOWN_FAILURES: usize = 10;
const MESSAGE_LINES: usize = 12;
const MESSAGE_CHARS: usize = 1000;

/// One failed test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Failure {
    pub(super) name: String,
    /// `path:line[:col]` when the output names one.
    pub(super) location: Option<String>,
    /// The assertion text the runner printed, its expected/actual diff included.
    pub(super) message: String,
    /// The failure's own section of the output.
    pub(super) output: String,
}

/// One test run. `passed` and `skipped` are `None` when the runner does not
/// print them (go test without `-v`); `failures` holds every failed test in
/// output order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) runner: &'static str,
    pub(super) passed: Option<u64>,
    pub(super) failed: u64,
    pub(super) skipped: Option<u64>,
    pub(super) failures: Vec<Failure>,
}

impl Summary {
    /// The summary as the model reads it: a hash of its fields, each failure
    /// with its handle from `failure_handles`, and `text`, the rendered form.
    pub(super) fn value(
        &self,
        output_handle: Option<&str>,
        failure_handles: &[Option<String>],
    ) -> Value {
        let handle_of = |at: usize| failure_handles.get(at).and_then(Option::as_deref);
        let mut counts = vec![format!("{} failed", self.failed)];
        if let Some(passed) = self.passed {
            counts.insert(0, format!("{passed} passed"));
        }
        if let Some(skipped) = self.skipped {
            counts.push(format!("{skipped} skipped"));
        }
        let stored = output_handle.map_or_else(String::new, |handle| format!(" · output {handle}"));
        let mut lines = vec![format!(
            "[tests: {} · {}{stored}]",
            self.runner,
            counts.join(" · ")
        )];
        for (at, failure) in self.failures.iter().take(SHOWN_FAILURES).enumerate() {
            let location = failure
                .location
                .as_ref()
                .map_or_else(String::new, |location| format!(" · {location}"));
            let handle = handle_of(at).map_or_else(String::new, |handle| format!(" · {handle}"));
            lines.push(format!("FAILED {}{location}{handle}", failure.name));
            let shown: String = failure.message.chars().take(MESSAGE_CHARS).collect();
            lines.extend(
                shown
                    .lines()
                    .take(MESSAGE_LINES)
                    .map(|line| format!("    {line}")),
            );
        }
        if self.failures.len() > SHOWN_FAILURES {
            lines.push(format!(
                "[{} more failures in the result's 'tests 'failures]",
                self.failures.len() - SHOWN_FAILURES
            ));
        }
        let failures: Vec<Value> = self
            .failures
            .iter()
            .enumerate()
            .map(|(at, failure)| {
                json!({
                    "name": failure.name,
                    "location": failure.location,
                    "message": failure.message,
                    "output": failure.output,
                    "output-handle": handle_of(at),
                })
            })
            .collect();
        json!({
            "runner": self.runner,
            "passed": self.passed,
            "failed": self.failed,
            "skipped": self.skipped,
            "failures": failures,
            "output-handle": output_handle,
            "text": lines.join("\n"),
        })
    }
}

/// Compile one of this module's own patterns.
fn pattern(source: &str) -> Regex {
    Regex::new(source).unwrap_or_else(|error| panic!("the pattern {source:?} is invalid: {error}"))
}

/// The match of `regex` in `line`; a pattern that gives up backtracking
/// reads as no match.
fn found<'t>(regex: &Regex, line: &'t str) -> Option<Captures<'t>> {
    regex.captures(line).ok().flatten()
}

/// Capture group `index` of a match, or the empty string when it took no part.
fn group<'t>(captures: &Captures<'t>, index: usize) -> &'t str {
    captures.get(index).map_or("", |matched| matched.as_str())
}

/// Lines after `start` up to (not including) the first one `stop` accepts.
fn section<'t>(lines: &[&'t str], start: usize, stop: impl Fn(&str) -> bool) -> Vec<&'t str> {
    lines[start + 1..]
        .iter()
        .copied()
        .take_while(|line| !stop(line))
        .collect()
}

/// A header line and its section as one text, without leading and trailing
/// blank lines.
fn trimmed(header: Option<&str>, lines: &[&str]) -> String {
    header
        .into_iter()
        .chain(lines.iter().copied())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_string()
}

/// `text` without the indentation all its non-blank lines share; a blank
/// line becomes empty.
fn dedent(text: &str) -> String {
    let indent = |line: &str| line.len() - line.trim_start_matches([' ', '\t']).len();
    let margin = text
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(|line| &line[..indent(line)])
        .reduce(|shared, next| {
            let same = shared
                .bytes()
                .zip(next.bytes())
                .take_while(|(left, right)| left == right)
                .count();
            &shared[..same]
        })
        .map_or(0, str::len);
    text.split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                ""
            } else {
                &line[margin..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The numbers `counts` finds in `text`, by the word each belongs to: the
/// pattern captures the number and the word in the groups named here.
fn tally(counts: &Regex, text: &str, number: usize, word: usize) -> HashMap<String, u64> {
    counts
        .captures_iter(text)
        .flatten()
        .filter_map(|pair| {
            Some((
                group(&pair, word).to_string(),
                group(&pair, number).parse().ok()?,
            ))
        })
        .collect()
}

static NUMBER_WORD: LazyLock<Regex> = LazyLock::new(|| pattern(r"(\d+) (\w+)"));

/// A command that starts a test runner, anywhere in a shell line
/// (`cd x && cargo test -p y`).
static TEST_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?:^|[\s;&|(])(?:cargo(?:\s+\+\S+)?\s+(?:test|nextest\s+run)|py\.?test|",
        r"(?:\S*/)?python[\d.]*\s+-m\s+(?:pytest|unittest)|go\s+test|jest|vitest|",
        r"(?:npm|pnpm|yarn|bun)\s+(?:run\s+)?test)(?=[\s;&|)]|$)",
    ))
});
static ANSI: LazyLock<Regex> = LazyLock::new(|| pattern(r"\x1b\[[0-9;?]*[ -/]*[@-~]"));

/// Whether `command` starts a test runner this module can summarize.
pub(super) fn looks_like_test_run(command: &str) -> bool {
    TEST_COMMAND.is_match(command).unwrap_or(false)
}

static CARGO_RESULT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored"));
static CARGO_SECTION: LazyLock<Regex> = LazyLock::new(|| pattern(r"^---- (\S+) stdout ----$"));
static RUST_PANIC: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"panicked at (?:'(.*)', )?([^\s:'][^:']*?:\d+:\d+):?\s*$"));

fn cargo(lines: &[&str]) -> Option<Summary> {
    let mut totals = [0u64; 3];
    let mut seen = false;
    for result in lines.iter().filter_map(|line| found(&CARGO_RESULT, line)) {
        seen = true;
        for (at, total) in totals.iter_mut().enumerate() {
            *total += group(&result, at + 1).parse::<u64>().unwrap_or(0);
        }
    }
    if !seen {
        return None;
    }
    let mut failures = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(header) = found(&CARGO_SECTION, line) else {
            continue;
        };
        let body = section(lines, at, |line| {
            line.starts_with("---- ") || line == "failures:" || line == "successes:"
        });
        let panic = body
            .iter()
            .enumerate()
            .find_map(|(index, text)| found(&RUST_PANIC, text).map(|panic| (index, panic)));
        let (location, message) = match panic {
            Some((index, panic)) => {
                let mut message = vec![group(&panic, 1)];
                message.extend(section(&body, index, |rest| rest.starts_with("note: ")));
                message.retain(|part| !part.is_empty());
                (Some(group(&panic, 2).to_string()), message.join("\n"))
            }
            None => (None, String::new()),
        };
        failures.push(Failure {
            name: group(&header, 1).to_string(),
            location,
            message,
            output: trimmed(Some(line), &body),
        });
    }
    Some(Summary {
        runner: "cargo",
        passed: Some(totals[0]),
        failed: totals[1],
        skipped: Some(totals[2]),
        failures,
    })
}

static PYTEST_FINAL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"^=+ (.*\d+ (?:passed|failed|skipped|error|errors|deselected|xfailed|xpassed).*)",
        r" in [\d.]+s(?: \(.*\))? =+$",
    ))
});
static PYTEST_SECTION: LazyLock<Regex> = LazyLock::new(|| pattern(r"^_{3,} (\S.*?) _{3,}$"));
static PYTEST_FRAME: LazyLock<Regex> = LazyLock::new(|| pattern(r"^([^\s:]+\.py):(\d+): ?\w*\s*$"));

fn pytest(lines: &[&str]) -> Option<Summary> {
    let last = lines
        .iter()
        .rev()
        .find_map(|line| found(&PYTEST_FINAL, line))?;
    let counts = tally(&NUMBER_WORD, group(&last, 1), 1, 2);
    let count = |word: &str| counts.get(word).copied().unwrap_or(0);
    let mut failures = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(header) = found(&PYTEST_SECTION, line) else {
            continue;
        };
        let body = section(lines, at, |line| {
            line.starts_with('=') || found(&PYTEST_SECTION, line).is_some()
        });
        let location = body
            .iter()
            .filter_map(|text| found(&PYTEST_FRAME, text))
            .next_back()
            .map(|frame| format!("{}:{}", group(&frame, 1), group(&frame, 2)));
        let errors: Vec<&str> = body
            .iter()
            .filter(|text| text.starts_with("E "))
            .map(|text| &text[1..])
            .collect();
        failures.push(Failure {
            name: group(&header, 1).to_string(),
            location,
            message: dedent(&errors.join("\n")).trim_matches('\n').to_string(),
            output: trimmed(Some(line), &body),
        });
    }
    Some(Summary {
        runner: "pytest",
        passed: Some(count("passed")),
        failed: count("failed") + count("error") + count("errors"),
        skipped: Some(count("skipped")),
        failures,
    })
}

static UNITTEST_RAN: LazyLock<Regex> = LazyLock::new(|| pattern(r"^Ran (\d+) tests? in "));
static UNITTEST_STATUS: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(OK|FAILED)(?: \((.*)\))?$"));
static UNITTEST_SECTION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(?:ERROR|FAIL): (\S+) \((\S+)\)"));
static PY_FRAME: LazyLock<Regex> = LazyLock::new(|| pattern(r#"^\s*File "([^"]+)", line (\d+)"#));
static WORD_NUMBER: LazyLock<Regex> = LazyLock::new(|| pattern(r"(\w+)=(\d+)"));

fn unittest(lines: &[&str]) -> Option<Summary> {
    let ran = lines.iter().find_map(|line| found(&UNITTEST_RAN, line))?;
    let status = lines
        .iter()
        .rev()
        .find_map(|line| found(&UNITTEST_STATUS, line))?;
    let counts = tally(&WORD_NUMBER, group(&status, 2), 2, 1);
    let count = |word: &str| counts.get(word).copied().unwrap_or(0);
    let failed = count("failures") + count("errors");
    let skipped = count("skipped");
    let mut failures = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(header) = found(&UNITTEST_SECTION, line) else {
            continue;
        };
        let mut body = section(lines, at, |line| {
            line.starts_with("==========") || line.starts_with("Ran ")
        });
        while body
            .last()
            .is_some_and(|last| last.chars().all(|letter| letter == '-'))
        {
            body.pop();
        }
        let location = body
            .iter()
            .filter_map(|text| found(&PY_FRAME, text))
            .next_back()
            .map(|frame| format!("{}:{}", group(&frame, 1), group(&frame, 2)));
        // The exception line (and an assertEqual diff under it) ends the traceback.
        let message = body
            .iter()
            .position(|text| {
                text.chars()
                    .next()
                    .is_some_and(|first| !first.is_whitespace())
                    && !text.starts_with("Traceback")
                    && !text.starts_with('-')
            })
            .map_or_else(String::new, |tail| trimmed(None, &body[tail..]));
        failures.push(Failure {
            name: group(&header, 2).to_string(),
            location,
            message,
            output: trimmed(Some(line), &body),
        });
    }
    let ran: u64 = group(&ran, 1).parse().unwrap_or(0);
    Some(Summary {
        runner: "unittest",
        passed: Some(ran.saturating_sub(failed + skipped + count("expected_failures"))),
        failed,
        skipped: Some(skipped),
        failures,
    })
}

static GO_RESULT: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s*--- (PASS|FAIL|SKIP): (\S+)"));
static GO_RUN: LazyLock<Regex> = LazyLock::new(|| pattern(r"^=== RUN\s+(\S+)"));
static GO_PACKAGE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(?:ok|FAIL|\?)\s+\S+"));
static GO_LOCATION: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s+(\S+\.go:\d+): (.*)$"));

fn go(lines: &[&str]) -> Option<Summary> {
    let results: Vec<Captures> = lines
        .iter()
        .filter_map(|line| found(&GO_RESULT, line))
        .collect();
    if results.is_empty() && !lines.iter().any(|line| found(&GO_PACKAGE, line).is_some()) {
        return None;
    }
    let verbose = lines.iter().any(|line| found(&GO_RUN, line).is_some());
    let ended = |kind: &str| {
        results
            .iter()
            .filter(|result| group(result, 1) == kind)
            .count() as u64
    };
    // A test's messages print under `=== RUN` (-v) or under its `--- FAIL:` line.
    let mut messages: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in lines {
        if let Some(run) = found(&GO_RUN, line) {
            current = Some(group(&run, 1));
        } else if let Some(result) = found(&GO_RESULT, line) {
            current = (group(&result, 1) == "FAIL" && !verbose).then(|| group(&result, 2));
        } else if let (Some(test), true) = (current, line.starts_with("    ")) {
            messages.entry(test).or_default().push(line.trim());
        } else if !line.starts_with(' ') {
            current = None;
        }
    }
    let failures = results
        .iter()
        .filter(|result| group(result, 1) == "FAIL")
        .map(|result| {
            let name = group(result, 2);
            let body = messages.get(name).cloned().unwrap_or_default();
            let location = body.iter().find_map(|text| {
                found(&GO_LOCATION, &format!("    {text}"))
                    .map(|located| group(&located, 1).to_string())
            });
            Failure {
                name: name.to_string(),
                location,
                message: body.join("\n"),
                output: trimmed(Some(group(result, 0).trim()), &body),
            }
        })
        .collect();
    Some(Summary {
        runner: "go",
        passed: verbose.then(|| ended("PASS")),
        failed: ended("FAIL"),
        skipped: verbose.then(|| ended("SKIP")),
        failures,
    })
}

static JEST_TESTS: LazyLock<Regex> = LazyLock::new(|| pattern(r"^Tests:\s+(.*?),?\s+(\d+) total$"));
static JEST_SECTION: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s+● (.+)$"));
static JEST_AT: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s+at .*\(([^()\s]+:\d+:\d+)\)$"));
static CODE_FRAME: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s*>?\s*\d*\s*\|"));

fn jest(lines: &[&str]) -> Option<Summary> {
    let totals = lines.iter().find_map(|line| found(&JEST_TESTS, line))?;
    let counts = tally(&NUMBER_WORD, group(&totals, 1), 1, 2);
    let count = |word: &str| counts.get(word).copied().unwrap_or(0);
    let mut failures = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(header) = found(&JEST_SECTION, line) else {
            continue;
        };
        let body = section(lines, at, |line| {
            line.starts_with("Test Suites:")
                || line.starts_with("Tests:")
                || found(&JEST_SECTION, line).is_some()
        });
        let frame = body
            .iter()
            .position(|text| found(&CODE_FRAME, text).is_some())
            .unwrap_or(body.len());
        failures.push(Failure {
            name: group(&header, 1).trim().to_string(),
            location: body
                .iter()
                .find_map(|text| found(&JEST_AT, text))
                .map(|located| group(&located, 1).to_string()),
            message: dedent(&body[..frame].join("\n"))
                .trim_matches('\n')
                .to_string(),
            output: trimmed(Some(line), &body),
        });
    }
    Some(Summary {
        runner: "jest",
        passed: Some(count("passed")),
        failed: count("failed"),
        skipped: Some(count("skipped")),
        failures,
    })
}

static VITEST_TESTS: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s+Tests\s+(.*?)\s+\((\d+)\)$"));
static VITEST_SECTION: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s*FAIL\s+(\S+ > .+)$"));
static VITEST_AT: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s*❯ (\S+:\d+:\d+)$"));

fn vitest(lines: &[&str]) -> Option<Summary> {
    let totals = lines.iter().find_map(|line| found(&VITEST_TESTS, line))?;
    let counts = tally(&NUMBER_WORD, group(&totals, 1), 1, 2);
    let count = |word: &str| counts.get(word).copied().unwrap_or(0);
    let mut failures = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(header) = found(&VITEST_SECTION, line) else {
            continue;
        };
        let body = section(lines, at, |line| {
            line.starts_with('⎯') || found(&VITEST_SECTION, line).is_some()
        });
        let located_at = body
            .iter()
            .position(|text| found(&VITEST_AT, text).is_some());
        failures.push(Failure {
            name: group(&header, 1)
                .split_once(" > ")
                .map_or("", |(_file, name)| name)
                .to_string(),
            location: located_at
                .and_then(|index| found(&VITEST_AT, body[index]))
                .map(|located| group(&located, 1).to_string()),
            message: trimmed(None, &body[..located_at.unwrap_or(body.len())]),
            output: trimmed(Some(line), &body),
        });
    }
    Some(Summary {
        runner: "vitest",
        passed: Some(count("passed")),
        failed: count("failed"),
        skipped: Some(count("skipped")),
        failures,
    })
}

/// Reads one runner's output, given as its lines without colour codes.
type Parser = fn(&[&str]) -> Option<Summary>;

/// The parsers, most specific first: jest and vitest print lines a looser
/// parser could misread.
const PARSERS: [Parser; 6] = [cargo, pytest, unittest, jest, vitest, go];

/// The summary of one test run's output, or `None` when no parser recognizes it.
pub(super) fn summarize(output: &str) -> Option<Summary> {
    let plain = ANSI.replace_all(output, "");
    let lines: Vec<&str> = plain.lines().collect();
    PARSERS.iter().find_map(|parser| parser(&lines))
}
