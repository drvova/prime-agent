//! Executable acceptance checks: the machine-readable `<acceptance>` block a rewrite or brief
//! carries, and the run of each check against the working tree.
//!
//! One check per line, `- kind: argument`:
//!
//! - `run: <command>`: the command must exit 0. It runs through the autonomous gate lane's
//!   runner (the platform shell in the session cwd, with the gate timeout and output caps).
//! - `exists: <path>`: the path must exist under the session cwd (checked without a process).
//! - `grep: <path> <literal text>`: the literal text must occur in the file, or in a file under
//!   the directory (searched without a process).
//!
//! A line that is none of these is kept as [`AcceptanceCheck::Unrecognized`] and fails with the
//! reason, so a malformed block is reported raw instead of passing silently.

use std::path::Path;

use serde_json::{json, Value};

use crate::autonomous::{
    format_process_exit, truncate_gate_output, GateCommandRunner, GateOutputCap,
};

/// The block's tag: `<acceptance>` ... `</acceptance>`.
pub(crate) const ACCEPTANCE_TAG: &str = "acceptance";

/// The longest one `run:` check may take (the autonomous gate default).
const RUN_TIMEOUT_MS: u64 = crate::autonomous::DEFAULT_GATE_TIMEOUT_MS;

/// Output tail a failed `run:` check reports: where test runners print their summary.
const FAILURE_TAIL_CHARS: usize = 1500;

/// One acceptance check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptanceCheck {
    Run {
        command: String,
    },
    Exists {
        path: String,
    },
    Grep {
        path: String,
        pattern: String,
    },
    /// A line of the block that is not a check; it fails with the reason.
    Unrecognized {
        line: String,
    },
}

impl AcceptanceCheck {
    /// The check as its block line writes it (`run: cargo test`).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Run { command } => format!("run: {command}"),
            Self::Exists { path } => format!("exists: {path}"),
            Self::Grep { path, pattern } => format!("grep: {path} {pattern}"),
            Self::Unrecognized { line } => line.clone(),
        }
    }

    /// The REPL's view: `{kind, check}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let kind = match self {
            Self::Run { .. } => "run",
            Self::Exists { .. } => "exists",
            Self::Grep { .. } => "grep",
            Self::Unrecognized { .. } => "unrecognized",
        };
        json!({ "kind": kind, "check": self.label() })
    }
}

/// How one check came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckVerdict {
    Passed,
    /// The raw reason: the exit and output tail of a command, or what was missing.
    Failed {
        reason: String,
    },
}

/// One check and its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub check: AcceptanceCheck,
    pub verdict: CheckVerdict,
}

impl CheckOutcome {
    #[must_use]
    pub fn passed(&self) -> bool {
        matches!(self.verdict, CheckVerdict::Passed)
    }

    /// The REPL's view: `{kind, check, passed, reason}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut value = self.check.to_json();
        let reason = match &self.verdict {
            CheckVerdict::Passed => Value::Null,
            CheckVerdict::Failed { reason } => Value::String(reason.clone()),
        };
        value["passed"] = Value::Bool(self.passed());
        value["reason"] = reason;
        value
    }
}

/// The checks of the last `<acceptance>` block in `text`; `None` when it has none (or an empty
/// one).
pub(crate) fn parse_acceptance(text: &str) -> Option<Vec<AcceptanceCheck>> {
    let open = format!("<{ACCEPTANCE_TAG}>");
    let close = format!("</{ACCEPTANCE_TAG}>");
    let start = text.rfind(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let checks: Vec<AcceptanceCheck> = text[start..end]
        .lines()
        .map(|line| line.trim().trim_start_matches(['-', '*']).trim())
        .filter(|line| !line.is_empty())
        .map(parse_line)
        .collect();
    (!checks.is_empty()).then_some(checks)
}

fn parse_line(line: &str) -> AcceptanceCheck {
    let unrecognized = || AcceptanceCheck::Unrecognized {
        line: line.to_string(),
    };
    let Some((kind, argument)) = line.split_once(':') else {
        return unrecognized();
    };
    let argument = argument.trim();
    if argument.is_empty() {
        return unrecognized();
    }
    match kind.trim() {
        "run" => AcceptanceCheck::Run {
            command: argument.to_string(),
        },
        "exists" => AcceptanceCheck::Exists {
            path: argument.to_string(),
        },
        "grep" => match argument.split_once(char::is_whitespace) {
            Some((path, pattern)) if !pattern.trim().is_empty() => AcceptanceCheck::Grep {
                path: path.to_string(),
                pattern: pattern.trim().to_string(),
            },
            _ => unrecognized(),
        },
        _ => unrecognized(),
    }
}

/// Run every check in order from `cwd`; a failing check never stops the rest.
pub(crate) async fn run_checks(
    checks: &[AcceptanceCheck],
    cwd: &Path,
    runner: &dyn GateCommandRunner,
) -> Vec<CheckOutcome> {
    let mut outcomes = Vec::with_capacity(checks.len());
    for check in checks {
        let verdict = match check {
            AcceptanceCheck::Run { command } => match runner.run_gate(command, RUN_TIMEOUT_MS).await
            {
                Err(error) => CheckVerdict::Failed {
                    reason: format!("failed to run: {error:#}"),
                },
                Ok(result) if result.status == Some(0) && result.error.is_none() && !result.timed_out => {
                    CheckVerdict::Passed
                }
                Ok(result) => {
                    let output = [result.stdout.as_str(), result.stderr.as_str()]
                        .into_iter()
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let output = truncate_gate_output(
                        output.trim(),
                        result.output_truncated,
                        GateOutputCap::Tail(FAILURE_TAIL_CHARS),
                    );
                    let output = if output.is_empty() {
                        "(no output)".to_string()
                    } else {
                        output
                    };
                    CheckVerdict::Failed {
                        reason: format!("{}; output tail:\n{output}", format_process_exit(&result)),
                    }
                }
            },
            AcceptanceCheck::Exists { path } => {
                let target = cwd.join(path);
                match tokio::task::spawn_blocking(move || target.exists()).await {
                    Ok(true) => CheckVerdict::Passed,
                    Ok(false) => CheckVerdict::Failed {
                        reason: format!("{path} does not exist"),
                    },
                    Err(error) => CheckVerdict::Failed {
                        reason: format!("the check could not finish: {error}"),
                    },
                }
            }
            AcceptanceCheck::Grep { path, pattern } => {
                let (root, cwd, needle) = (cwd.join(path), cwd.to_path_buf(), pattern.clone());
                match tokio::task::spawn_blocking(move || literal_occurs(&needle, &root, &cwd)).await
                {
                    Ok(Ok(true)) => CheckVerdict::Passed,
                    Ok(Ok(false)) => CheckVerdict::Failed {
                        reason: format!("{pattern:?} does not occur in {path}"),
                    },
                    Ok(Err(error)) => CheckVerdict::Failed {
                        reason: format!("{path}: {error:#}"),
                    },
                    Err(error) => CheckVerdict::Failed {
                        reason: format!("the check could not finish: {error}"),
                    },
                }
            }
            AcceptanceCheck::Unrecognized { line } => CheckVerdict::Failed {
                reason: format!(
                    "{line:?} is not an acceptance check (write `run: <command>`, `exists: <path>`, \
                     or `grep: <path> <text>`)"
                ),
            },
        };
        outcomes.push(CheckOutcome {
            check: check.clone(),
            verdict,
        });
    }
    outcomes
}

/// Whether `needle` occurs literally in the file `root`, or in a file under the directory `root`
/// (blocking: call off the async threads).
fn literal_occurs(needle: &str, root: &Path, cwd: &Path) -> anyhow::Result<bool> {
    if !root.exists() {
        anyhow::bail!("no such file or directory");
    }
    let payload = json!({
        "pattern": needle,
        "root": root,
        "cwd": cwd,
        "literal": true,
        "maxResults": 1,
    });
    let report = crate::fs_search::grep(&crate::fs_search::parse_grep(&payload)?)?;
    Ok(!report.matches.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomous::{ChildProcessResult, GateRunFuture};

    #[test]
    fn the_last_block_parses_one_check_per_line_and_keeps_what_is_not_a_check() {
        let text = "Quoting the format: <acceptance>run: no</acceptance>\nFix the parser.\n\
                    <acceptance>\n- run: cargo test -p parser\n* exists: src/parser.rs\n\n\
                    - grep: src/parser.rs fn parse_expr\n- make it fast\n- grep: src/lib.rs\n\
                    </acceptance>";
        assert_eq!(
            parse_acceptance(text),
            Some(vec![
                AcceptanceCheck::Run {
                    command: "cargo test -p parser".to_string()
                },
                AcceptanceCheck::Exists {
                    path: "src/parser.rs".to_string()
                },
                AcceptanceCheck::Grep {
                    path: "src/parser.rs".to_string(),
                    pattern: "fn parse_expr".to_string()
                },
                AcceptanceCheck::Unrecognized {
                    line: "make it fast".to_string()
                },
                AcceptanceCheck::Unrecognized {
                    line: "grep: src/lib.rs".to_string()
                },
            ])
        );
        assert_eq!(parse_acceptance("Fix the parser."), None);
        assert_eq!(parse_acceptance("<acceptance>\n\n</acceptance>"), None);
    }

    /// Answers each command with the exit code its text names (`exit 3`), never spawning.
    struct ExitRunner;

    impl GateCommandRunner for ExitRunner {
        fn run_gate(&self, command: &str, _timeout_ms: u64) -> GateRunFuture<'_> {
            let status = command
                .strip_prefix("exit ")
                .and_then(|code| code.parse().ok());
            Box::pin(async move {
                Ok(ChildProcessResult {
                    status,
                    stdout: String::new(),
                    stderr: if status == Some(0) {
                        String::new()
                    } else {
                        "1 test failed".to_string()
                    },
                    ..ChildProcessResult::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn each_check_runs_and_a_failure_keeps_its_raw_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/parser.rs"), "pub fn parse_expr() {}\n").unwrap();
        let checks = parse_acceptance(
            "<acceptance>\nrun: exit 0\nrun: exit 1\nexists: src/parser.rs\nexists: src/lexer.rs\n\
             grep: src/parser.rs fn parse_expr\ngrep: src fn parse_expr\n\
             grep: src/parser.rs fn parse_stmt\nnot a check\n</acceptance>",
        )
        .unwrap();
        let verdicts: Vec<CheckVerdict> = run_checks(&checks, dir.path(), &ExitRunner)
            .await
            .into_iter()
            .map(|outcome| outcome.verdict)
            .collect();
        let failed = |reason: &str| CheckVerdict::Failed {
            reason: reason.to_string(),
        };
        assert_eq!(
            verdicts,
            vec![
                CheckVerdict::Passed,
                failed("exited with code 1; output tail:\n1 test failed"),
                CheckVerdict::Passed,
                failed("src/lexer.rs does not exist"),
                CheckVerdict::Passed,
                CheckVerdict::Passed,
                failed("\"fn parse_stmt\" does not occur in src/parser.rs"),
                failed(
                    "\"not a check\" is not an acceptance check (write `run: <command>`, \
                     `exists: <path>`, or `grep: <path> <text>`)"
                ),
            ]
        );
    }
}
