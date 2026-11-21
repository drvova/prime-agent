//! The tools a session's Scheme kernel runs itself, in this process, rather
//! than asking the session for them through `(host-request ...)`.
//!
//! - `(bash command [options])` runs a shell command in the session's working
//!   directory and returns a hash: `'exit-code`, `'output` (stdout and stderr
//!   together, in the order written), `'truncated`, `'timed-out`,
//!   `'duration-ms`. A failing command is a result, not an error. Option:
//!   `'timeout-ms` (default 120000; `#f` for none).
//! - `(fs-read path [options])` returns a file's lines under edit anchors:
//!   `'path`, `'start`, `'end`, `'total-lines`, `'lines` (each a hash of
//!   `'line`, `'anchor`, `'text`), `'truncated`, `'line-ends`, `'encoding`,
//!   `'final-newline`, `'text`. Options: `'start`, `'end` (1-based, inclusive).
//! - `(fs-grep pattern [options])` returns `'matches` (each a hash of `'path`,
//!   `'line`, `'column`, `'text`, `'extra-lines`, `'before`, `'after`),
//!   `'files-searched`, `'files-matched`, `'truncated`, `'unreadable`, `'text`.
//!   Options: `'path`, `'glob`, `'type`, `'case`, `'literal`, `'word`,
//!   `'multiline`, `'context`, `'before`, `'after`, `'hidden`, `'gitignore`,
//!   `'max-results`, `'max-per-file`, `'max-columns`.
//! - `(fs-glob pattern [options])` and `(fs-find [options])` return
//!   `'entries` (each a hash of `'path`, `'kind`, and for files `'size`),
//!   `'truncated`, `'text`. Options of both: `'path`, `'hidden`, `'gitignore`,
//!   `'max-results`; of `fs-find` also `'name`, `'kind`, `'ignore-case`,
//!   `'max-depth`.
//! - `(fs-edit path edits)` applies one atomic patch of anchored edits (one
//!   hash, or a list of them): `(hash 'replace anchor 'text new)`, with
//!   `(list first last)` for a range, `(hash 'delete anchor)`,
//!   `(hash 'insert-after anchor 'text new)`, `(hash 'insert-before ...)`,
//!   `(hash 'append text)`. Returns `'applied`, `'path`, `'edits`, `'removed`,
//!   `'added`, `'total-lines`, `'hunks`, `'text`.
//! - `fs-symbol`, `fs-anchor`, `edit`, and the rest of `fs-edit` are listed
//!   in `native/files.rs`.
//!
//! Every failure raises a Scheme error naming the tool and the reason. Paths
//! resolve against the session's working directory and print relative to it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::anyhow;
use serde_json::Value;
use steel::gc::Gc;
use steel::rerrs::{ErrorKind, SteelErr};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use crate::tools::tool_definition::AbortSignal;

mod bash;
mod edit_check;
mod edit_replace;
mod files;
mod harness;
mod hypothesis;
#[cfg(test)]
mod tests;
mod worktree;

/// The stop signal of the cell a session's engine is running: the kernel
/// installs each cell's signal as it starts and cancels it when the cell is
/// aborted or times out, so a host function that blocks can end on its own.
pub(super) type CellSignal = Arc<Mutex<AbortSignal>>;

/// A session tool's outcome as the JSON reply its Scheme wrapper decodes:
/// `{"value": ...}`, or `{"error": "<operation>: <reason>"}`.
fn reply_json(operation: &str, outcome: anyhow::Result<Value>) -> String {
    match outcome {
        Ok(value) => serde_json::json!({ "value": value }),
        Err(error) => serde_json::json!({ "error": format!("{operation}: {error:#}") }),
    }
    .to_string()
}

/// Define the primitives behind the worktree, harness, and hypotheses
/// functions of the host API, which is loaded after them. They answer from
/// `handlers`, work and store where `session` says, and end with the cell
/// that called them. Must be called from within a tokio runtime.
pub(super) fn register_session_tools(
    engine: &mut Engine,
    handlers: &crate::kernel::shared::HostRequestHandlers,
    session: &super::SessionEnvironment,
) {
    harness::register(engine, session);
    let worktrees = Arc::new(worktree::Worktrees::new(handlers.clone(), session));
    worktree::register(engine, Arc::clone(&worktrees));
    hypothesis::register(engine, handlers.clone(), worktrees);
}

/// The Scheme side of the native tools, defined in every session engine
/// after its `%native-call` primitive is registered.
const TOOLS: &str = r#"
(define (%native operation options)
  (let ([reply (%native-call operation options)])
    (if (hash-contains? reply 'error)
        (%native-raise (hash-ref reply 'error))
        (hash-ref reply 'value))))

(define (%options rest) (if (null? rest) (hash) (car rest)))

(define (bash command . rest) (%native "bash" (hash-insert (%options rest) 'command command)))
(define (fs-read path . rest) (%native "fs-read" (hash-insert (%options rest) 'path path)))
(define (fs-grep pattern . rest) (%native "fs-grep" (hash-insert (%options rest) 'pattern pattern)))
(define (fs-glob pattern . rest) (%native "fs-glob" (hash-insert (%options rest) 'pattern pattern)))
(define (fs-find . rest) (%native "fs-find" (%options rest)))
"#;

/// A tool's JSON reply as a Scheme value: objects become hashes keyed by
/// kebab-case symbols, arrays lists, whole numbers integers, and null `#f`.
pub(super) fn scheme_value(value: Value) -> SteelVal {
    match value {
        Value::Null => SteelVal::BoolV(false),
        Value::Bool(flag) => SteelVal::BoolV(flag),
        Value::Number(number) => number
            .as_i64()
            .and_then(|whole| isize::try_from(whole).ok())
            .map(SteelVal::IntV)
            .or_else(|| number.as_f64().map(SteelVal::NumV))
            .unwrap_or(SteelVal::Void),
        Value::String(text) => SteelVal::StringV(text.into()),
        Value::Array(items) => SteelVal::ListV(items.into_iter().map(scheme_value).collect()),
        Value::Object(fields) => SteelVal::HashMapV(
            Gc::new(
                fields
                    .into_iter()
                    .map(|(key, value)| {
                        let mut name = String::with_capacity(key.len() + 2);
                        for letter in key.chars() {
                            if letter.is_ascii_uppercase() {
                                name.push('-');
                            }
                            name.push(letter.to_ascii_lowercase());
                        }
                        (SteelVal::SymbolV(name.into()), scheme_value(value))
                    })
                    .collect::<steel::HashMap<_, _>>(),
            )
            .into(),
        ),
    }
}

/// Define the native tools in a session's engine. They work in `cwd` and
/// watch `cell` to end with the cell that called them. Must be called from
/// within a tokio runtime: commands are spawned and awaited on it.
///
/// # Errors
///
/// The Scheme definitions failed to load.
pub(super) fn register(
    engine: &mut Engine,
    cwd: PathBuf,
    cell: CellSignal,
    handlers: crate::kernel::shared::HostRequestHandlers,
    agent_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    let files = files::Files::new(cwd.clone(), handlers, runtime.clone(), agent_dir);
    engine.register_fn(
        "%native-call",
        move |operation: String, options: SteelVal| -> SteelVal {
            let outcome = Value::try_from(options)
                .map_err(|error| anyhow!("the options are not plain data: {error}"))
                .and_then(|options| match operation.as_str() {
                    "bash" => {
                        let signal = cell.lock().unwrap_or_else(PoisonError::into_inner).clone();
                        bash::run(&runtime, &cwd, &signal, &options)
                    }
                    file_tool => {
                        let signal = cell.lock().unwrap_or_else(PoisonError::into_inner).clone();
                        files.run(file_tool, &signal, &options)
                    }
                });
            scheme_value(match outcome {
                Ok(value) => serde_json::json!({ "value": value }),
                Err(error) => serde_json::json!({ "error": format!("{operation}: {error:#}") }),
            })
        },
    );
    // Scheme's own `error` prints its message as written data, escaping the
    // line breaks of a reason that spans lines; this raises it as it is.
    engine.register_value(
        "%native-raise",
        SteelVal::FuncV(|arguments| match arguments {
            [SteelVal::StringV(reason)] => {
                Err(SteelErr::new(ErrorKind::Generic, reason.to_string()))
            }
            _ => Err(SteelErr::new(
                ErrorKind::ArityMismatch,
                "%native-raise takes one string".to_string(),
            )),
        }),
    );
    engine
        .run(TOOLS)
        .map_err(|error| anyhow!("the Scheme native tools failed to load: {error}"))?;
    engine
        .run(files::TOOLS)
        .map_err(|error| anyhow!("the Scheme file tools failed to load: {error}"))?;
    Ok(())
}
