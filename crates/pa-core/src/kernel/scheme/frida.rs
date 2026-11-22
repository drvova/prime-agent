//! Frida (dynamic instrumentation) for a session's Scheme kernel: frida-core
//! linked into this process, reached from a cell through the `frida-*`
//! functions.
//!
//! - `(frida-devices)`, `(frida-add-remote address ...)`: `'id`, `'name`,
//!   `'type`.
//! - `(frida-ps #:device d #:applications #t)`: `'pid`, `'name` (and
//!   `'identifier` for applications).
//! - `(frida-kill target #:device d)`.
//! - `(frida-attach target #:device d #:spawn #t #:args (list ...) #:pause
//!   #t)`: a session, `'session` (its id), `'pid`, `'spawned`.
//! - `(frida-sessions)`, `(frida-resume session)`, `(frida-detach session)`.
//! - `(frida-script-load session source #:name n #:runtime r)`: a script,
//!   `'script` (its id), `'session`, `'name`.
//! - `(frida-script-messages script [n])`, `(frida-script-tail script [n])`,
//!   `(frida-script-wait script #:predicate p #:timeout-ms n)`,
//!   `(frida-script-call script export [args] #:timeout-ms n)`,
//!   `(frida-script-post script message #:data-base64 d)`,
//!   `(frida-script-unload script)`.
//! - `(frida-trace session #:include ... )`, `(frida-compile entrypoint
//!   #:project-root p)`.
//!
//! # Where Frida runs, and what that costs
//!
//! frida-core is a C library. It is linked into the daemon, which hosts
//! every session, so a fault inside it (a crash in its injector, its
//! JavaScript engines or its `GLib`, or an abort on an internal assertion) is
//! not a panic a session can be rebuilt from: it ends the daemon and every
//! session in it. Nothing here can contain that; the restart owner brings
//! the daemon back and the sessions are revived from their transcripts.
//! After such a death nobody detaches: Frida's agent unloads itself from an
//! attached process when its connection drops, and a target a session had
//! spawned is no longer killed (one still suspended starts running).
//!
//! frida-core runs one main loop on one thread for the whole process and
//! delivers every callback there. That thread belongs to no session: the
//! callbacks only append to the log of the script they belong to (a script
//! belongs to one session's [`Registry`]) and never read per-session state.
//! A panic in one is caught at the boundary. Each session has its own
//! device manager; sessions share nothing else of Frida but that loop.
//!
//! # Stopping
//!
//! Every call into Frida carries a cancellable that the cell's stop cancels,
//! and runs on a blocking thread while the cell's own thread waits for
//! either: a stopped cell leaves the engine within [`STOP_GRACE`] even when
//! Frida does not answer the cancellation. A call that outlives its cell
//! undoes what it made when it returns. Waits on messages and `rpc.exports`
//! replies are this module's own and end at once.
//!
//! Every session is tracked until it detaches. A spawned target is killed
//! when its session detaches. Everything is detached, and the connection
//! closed, when the kernel is killed or released, when the session's
//! lifetime ends (a crash included) and when the registry is dropped.
//!
//! # Builds without Frida
//!
//! frida-core comes as a prebuilt static library per platform, so linking it
//! is the `frida` cargo feature. A build without it still defines every
//! function: each raises an error that says Frida is not in this build.

use std::sync::{Arc, PoisonError};
use std::time::Duration;

use anyhow::anyhow;
use serde_json::{json, Value};
use steel::rerrs::{ErrorKind, SteelErr};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use super::native::CellSignal;
use crate::kernel::shared::HostRequestHandlers;

mod backend;
mod cancel;
#[cfg(feature = "frida")]
mod core;
mod log;
mod registry;
#[cfg(test)]
mod tests;

pub(crate) use registry::Registry;

/// The Scheme side, defined in every session engine after its `%frida-call`
/// primitive is registered and the host API is loaded.
const API: &str = include_str!("frida/api.scm");

/// How long a stopped cell waits for Frida to end the call it was in before
/// it leaves the call behind.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Why a build without the `frida` feature has no Frida.
#[cfg(not(feature = "frida"))]
const UNAVAILABLE: &str = "Frida is not available in this build of Prime Agent: \
     it was compiled without the `frida` feature, which links frida-core \
     (prebuilt for Linux, macOS and Windows on x86_64 and arm64)";

/// The Frida state of a session whose kernel answers host requests from
/// `handlers`: frida-core when this build links it, else a registry whose
/// every call says why it cannot.
pub(super) fn registry(handlers: HostRequestHandlers) -> Registry {
    Registry::new(
        Box::new(|| {
            #[cfg(feature = "frida")]
            let connected = Ok(Arc::new(core::Core::open()) as Arc<dyn backend::Backend>);
            #[cfg(not(feature = "frida"))]
            let connected = Err(UNAVAILABLE.to_string());
            connected
        }),
        handlers,
    )
}

/// Define the `frida-*` functions in a session's engine. Their calls reach
/// `registry` and watch `cell` to end with the cell that made them. Must be
/// called from within a tokio runtime: calls run on its blocking threads.
///
/// # Errors
///
/// The Scheme definitions failed to load.
pub(super) fn register(
    engine: &mut Engine,
    registry: Arc<Registry>,
    cell: CellSignal,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    engine.register_fn(
        "%frida-call",
        move |operation: String, payload: String| -> String {
            let signal = cell.lock().unwrap_or_else(PoisonError::into_inner).clone();
            // A cell left over from an ended lifetime attaches to nothing:
            // the rebuilt session could not see or end what it made.
            let outcome = match (
                pa_types::session_scope::ensure_live(),
                serde_json::from_str::<Value>(&payload),
            ) {
                (Err(error), _) => Err(error.to_string()),
                (Ok(()), Err(error)) => Err(format!("the arguments are not plain data: {error}")),
                (Ok(()), Ok(payload)) => {
                    let stop = cancel::Cancel::default();
                    // The engine cannot interrupt this thread, and nothing
                    // can interrupt a thread inside Frida: the call runs on
                    // another, and this one waits for it or for the cell.
                    let mut running = runtime.spawn_blocking({
                        let registry = Arc::clone(&registry);
                        let stop = stop.clone();
                        move || registry.dispatch(&operation, &payload, &stop)
                    });
                    runtime.block_on(async {
                        tokio::select! {
                            biased;
                            () = signal.cancelled() => {
                                stop.cancel();
                                // Frida usually ends the call now; one it
                                // does not end is left to finish alone.
                                drop(tokio::time::timeout(STOP_GRACE, &mut running).await);
                                Err("the call was abandoned: its cell was stopped".to_string())
                            }
                            done = &mut running => done
                                .map_err(|error| format!("the call's thread failed: {error}"))
                                .and_then(std::convert::identity),
                        }
                    })
                }
            };
            match outcome {
                Ok(value) => json!({ "value": value }),
                Err(reason) => json!({ "error": reason }),
            }
            .to_string()
        },
    );
    // Scheme's own `error` prints its message as written data, escaping the
    // line breaks of Frida's messages; this raises it as it is.
    engine.register_value(
        "%frida-raise",
        SteelVal::FuncV(|arguments| match arguments {
            [SteelVal::StringV(reason)] => {
                Err(SteelErr::new(ErrorKind::Generic, reason.to_string()))
            }
            _ => Err(SteelErr::new(
                ErrorKind::ArityMismatch,
                "%frida-raise takes one string".to_string(),
            )),
        }),
    );
    engine
        .run(API)
        .map_err(|error| anyhow!("the Scheme Frida API failed to load: {error}"))?;
    Ok(())
}
