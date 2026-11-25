//! Rizin for a session's Scheme kernel: the `rz-*` functions drive the
//! `rizin` program the user has installed. Nothing of rizin is linked into
//! this process: each open session is a `rizin` child that reads commands
//! from a pipe, so a crash inside rizin ends that one child.
//!
//! - `(rz-open target #:write #:debug #:args #:arch #:bits #:base #:raw
//!   #:analyze #:stdin #:flags #:timeout-ms)` starts a rizin on a file, a
//!   URI or a pid and returns the session: `'session`, `'target`, `'pid`,
//!   `'version`, `'debug`, `'write`, `'alive`, `'reason`, `'output`.
//! - `(rz-cmd session command #:timeout-ms)` runs one command line and
//!   returns what it printed, errors included.
//! - `(rz-cmdj session command #:timeout-ms)` runs a command's JSON form and
//!   returns the parsed value, with rizin's own keys.
//! - `(rz-sessions)` lists the kernel's sessions, dead ones included.
//! - `(rz-close session)` kills a session's rizin and forgets it.
//!
//! A command blocks its cell. It ends with rizin's reply, with its
//! `#:timeout-ms`, or with the cell: on the last two rizin is interrupted
//! (SIGINT to its process group, as Ctrl-C would) and stays usable, and a
//! rizin that does not end the command within two seconds is killed, after
//! which the session reports itself dead with the reason. A reply is read
//! in bounded memory whatever its size (`rizin/session.rs`). Every rizin,
//! and whatever it started (a debugged program, a shell command), is killed
//! when the kernel is killed, when the session's lifetime ends, and when
//! the registry is dropped.
//!
//! Rizin is looked for on the session's `PATH` at each `rz-open`, never when
//! the daemon starts: a session without it gets one error that says how to
//! install it.

use std::sync::{Arc, PoisonError};

use anyhow::anyhow;
use serde_json::Value;
use steel::gc::Gc;
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use super::native::CellSignal;

mod program;
mod registry;
mod session;
#[cfg(test)]
mod tests;

pub(crate) use registry::Registry;

/// The Scheme side, defined in every session engine after its `%rz-call`
/// primitive and the native tools are registered.
const API: &str = include_str!("rizin/api.scm");

/// The oldest rizin the pipe protocol was run against: it has the `echo`
/// and `verj` commands every session starts with.
const MINIMUM_VERSION: (u64, u64, u64) = (0, 7, 4);

/// What `rz-open` says when rizin is not installed.
const NOT_INSTALLED: &str = "rizin is not installed: there is no `rizin` on this session's PATH. \
     Ask the user to install it (https://rizin.re: `brew install rizin`, `pacman -S rizin`, \
     `apt install rizin` on Kali, or a release archive from the rizinorg/rizin releases page \
     unpacked onto PATH), then call again";

/// A reply as a Scheme value: objects become hashes keyed by symbols of
/// rizin's own names (`'PC` stays `'PC`), arrays lists, null `#f`. Every
/// unsigned 64-bit integer stays exact: a register that holds a negative
/// number, or an address in a kernel's half of the space, is over 2^63 - 1.
fn scheme_value(value: Value) -> SteelVal {
    match value {
        Value::Null => SteelVal::BoolV(false),
        Value::Bool(flag) => SteelVal::BoolV(flag),
        Value::Number(number) => number
            .as_i64()
            .and_then(|whole| isize::try_from(whole).ok())
            .map(SteelVal::IntV)
            .or_else(|| {
                number
                    .as_u64()
                    .map(|whole| SteelVal::from(u128::from(whole)))
            })
            .or_else(|| number.as_f64().map(SteelVal::NumV))
            .unwrap_or(SteelVal::Void),
        Value::String(text) => SteelVal::StringV(text.into()),
        Value::Array(items) => SteelVal::ListV(items.into_iter().map(scheme_value).collect()),
        Value::Object(fields) => SteelVal::HashMapV(
            Gc::new(
                fields
                    .into_iter()
                    .map(|(key, value)| (SteelVal::SymbolV(key.into()), scheme_value(value)))
                    .collect::<steel::HashMap<_, _>>(),
            )
            .into(),
        ),
    }
}

/// Define the `rz-*` functions in a session's engine. Their calls reach
/// `registry` and watch `cell` to end with the cell that made them. Must be
/// called from within a tokio runtime, after the native tools are defined.
///
/// # Errors
///
/// The Scheme definitions failed to load.
pub(super) fn register(
    engine: &mut Engine,
    registry: Arc<Registry>,
    cell: CellSignal,
    admission: &crate::kernel::call_policy::Admission,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    let admission = admission.clone();
    engine.register_fn(
        "%rz-call",
        move |operation: String, options: SteelVal| -> SteelVal {
            let signal = cell.lock().unwrap_or_else(PoisonError::into_inner).clone();
            // A cell left over from an ended lifetime starts no rizin and
            // sends no command: the rebuilt session could not see or end
            // what it made, and a write-mode session writes to its target.
            let outcome = match (
                pa_types::session_scope::ensure_live()
                    .map_err(anyhow::Error::from)
                    .and_then(|()| {
                        admission.admit(crate::kernel::call_policy::CallKind::Native {
                            family: crate::kernel::call_policy::NativeFamily::Rizin,
                            name: &operation,
                        })
                    }),
                crate::kernel::scheme::limits::plain_data(options),
            ) {
                (Err(error), _) => Err(error.to_string()),
                (Ok(()), Err(error)) => Err(format!("the arguments are not plain data: {error}")),
                // The engine cannot interrupt this thread: the call itself
                // watches the cell's signal and ends with it.
                (Ok(()), Ok(options)) => {
                    runtime.block_on(registry.dispatch(&operation, &options, &signal))
                }
            };
            scheme_value(match outcome {
                Ok(value) => serde_json::json!({ "value": value }),
                Err(reason) => serde_json::json!({ "error": reason }),
            })
        },
    );
    super::contracts::load(engine, API)
        .map_err(|error| anyhow!("the Scheme rizin API failed to load: {error}"))?;
    Ok(())
}
