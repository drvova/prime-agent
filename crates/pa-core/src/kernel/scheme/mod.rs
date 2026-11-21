//! The in-process Scheme kernel: one Steel engine per session, living in
//! this process instead of a child interpreter.
//!
//! Building an engine compiles Steel's whole prelude (about 200 ms), so the
//! process keeps one spare engine built ahead of time: a session takes the
//! spare and a replacement is built in the background. Every session's
//! engine is built on its own. Copies of one engine (`Engine::clone`) are not
//! used: they share Steel's parameters and compiler state, so two sessions
//! running at once read each other's output and lose their definitions.
//!
//! A cell runs on a blocking thread, never on the async runtime. Its
//! `display` output is collected through a string port and returned with the
//! value of its last form. An abort or the per-call timeout interrupts the
//! engine at its next safepoint; a host function that is blocking at that
//! moment finishes first, since the engine cannot unwind native code.
//!
//! Scheme reaches the host through one primitive, `(host-request type
//! payload)`: the payload crosses as JSON to the session's
//! [`HostRequestHandlers`], the same handlers the Python kernel's host
//! requests reach, and the reply comes back as a Scheme value or raises.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use serde_json::{json, Value};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;
use steel::steel_vm::ThreadStateController;

use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::tools::ipython::{
    BootstrapProgressHandler, ExecuteResult, ExecuteStatus, IpythonKernelProvisioner,
    KernelErrorInfo, KernelExecuteOptions, KernelExecutor,
};
use crate::tools::tool_definition::AbortSignal;

/// The Scheme side of the host bridge, defined in every session engine after
/// its `%host-request-json` primitive is registered.
const HOST_BRIDGE: &str = r"
(define (host-request type payload)
  (let ([reply (string->jsexpr (%host-request-json type (value->jsexpr-string payload)))])
    (if (hash-contains? reply 'error)
        (error (hash-ref reply 'error))
        (hash-ref reply 'value))))

;; The session's working directory.
(define (session-cwd) %session-cwd)
";

/// Why a running cell was interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Aborted,
    TimedOut,
}

/// The process's spare engine, taken by the next session to start.
static SPARE: Mutex<Option<Engine>> = Mutex::new(None);

/// Build the spare engine in the background when there is none, so the next
/// session's kernel starts without paying for the build. Must be called from
/// within a tokio runtime.
pub fn prewarm_scheme_kernel() {
    tokio::task::spawn_blocking(|| {
        if SPARE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
        {
            return;
        }
        // Built outside the lock: a session starting meanwhile builds its own.
        let engine = Engine::new();
        SPARE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(engine);
    });
}

/// One session's engine and the handle that interrupts it.
struct SessionEngine {
    engine: Mutex<Engine>,
    controller: ThreadStateController,
}

/// A session's running Scheme kernel.
pub struct SchemeKernel {
    session: Arc<SessionEngine>,
}

impl KernelExecutor for SchemeKernel {
    fn execute(
        &self,
        code: &str,
        options: KernelExecuteOptions<'_>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ExecuteResult>> + Send>> {
        let session = Arc::clone(&self.session);
        let code = code.to_string();
        let signal = options.signal;
        let timeout = options.timeout_ms.map(Duration::from_millis);
        Box::pin(async move {
            let started = Instant::now();
            if signal.as_ref().is_some_and(AbortSignal::is_cancelled) {
                return Ok(ExecuteResult {
                    status: ExecuteStatus::Aborted,
                    ..ExecuteResult::default()
                });
            }
            let running = Arc::clone(&session);
            let mut run = tokio::task::spawn_blocking(move || {
                let mut engine = running
                    .engine
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let prepared = engine.run(
                    "(define %cell-output (open-output-string)) (current-output-port %cell-output)",
                );
                let outcome = prepared.and_then(|_| engine.run(code));
                // An interrupt that stopped the cell must not also stop the
                // read of what it printed.
                running.controller.resume();
                let stdout = engine
                    .run("(current-output-port (#%default-output-port)) (get-output-string %cell-output)")
                    .ok()
                    .and_then(|values| match values.into_iter().next_back() {
                        Some(SteelVal::StringV(text)) => Some(text.as_str().to_string()),
                        Some(_) | None => None,
                    })
                    .unwrap_or_default();
                (outcome, stdout)
            });
            let stop = async {
                match (&signal, timeout) {
                    (Some(signal), Some(limit)) => tokio::select! {
                        () = signal.cancelled() => Stop::Aborted,
                        () = tokio::time::sleep(limit) => Stop::TimedOut,
                    },
                    (Some(signal), None) => {
                        signal.cancelled().await;
                        Stop::Aborted
                    }
                    (None, Some(limit)) => {
                        tokio::time::sleep(limit).await;
                        Stop::TimedOut
                    }
                    (None, None) => std::future::pending().await,
                }
            };
            let (joined, stopped) = tokio::select! {
                joined = &mut run => (joined, None),
                reason = stop => {
                    session.controller.interrupt();
                    (run.await, Some(reason))
                }
            };
            // The next cell starts uninterrupted whichever side won.
            session.controller.resume();
            let (outcome, stdout) =
                joined.map_err(|error| anyhow!("the Scheme cell's thread failed: {error}"))?;
            let settled = ExecuteResult {
                stdout,
                duration_ms: Some(started.elapsed().as_millis() as u64),
                ..ExecuteResult::default()
            };
            Ok(match (outcome, stopped) {
                (Ok(values), _) => ExecuteResult {
                    status: ExecuteStatus::Ok,
                    result: values
                        .into_iter()
                        .rev()
                        .find(|value| !matches!(value, SteelVal::Void))
                        .map(|value| value.to_string()),
                    ..settled
                },
                (Err(_), Some(Stop::Aborted)) => ExecuteResult {
                    status: ExecuteStatus::Aborted,
                    ..settled
                },
                (Err(_), Some(Stop::TimedOut)) => ExecuteResult {
                    status: ExecuteStatus::Error,
                    error: Some(KernelErrorInfo {
                        ename: "Timeout".to_string(),
                        evalue: format!(
                            "the cell was interrupted after {} ms",
                            timeout.unwrap_or_default().as_millis()
                        ),
                        traceback: Vec::new(),
                    }),
                    ..settled
                },
                (Err(error), None) => ExecuteResult {
                    status: ExecuteStatus::Error,
                    error: Some(KernelErrorInfo {
                        ename: "SchemeError".to_string(),
                        evalue: error.to_string(),
                        traceback: Vec::new(),
                    }),
                    ..settled
                },
            })
        })
    }
}

struct ProvisionerState {
    handlers: HostRequestHandlers,
    /// The session's working directory: where relative paths in host
    /// functions resolve and commands run.
    cwd: std::path::PathBuf,
    /// The session's engine once started; the async lock holds concurrent
    /// `ensure` calls to one startup.
    session: tokio::sync::Mutex<Option<Arc<SessionEngine>>>,
}

/// Owns one session's Scheme kernel: started on the first `ensure` from the
/// process's spare engine (or built on the spot), reused afterwards, and
/// dropped by `kill`.
pub struct SchemeKernelProvisioner {
    state: Arc<ProvisionerState>,
}

impl SchemeKernelProvisioner {
    /// A provisioner whose kernel answers `(host-request ...)` from `handlers`
    /// and works in `cwd`.
    #[must_use]
    pub fn new(handlers: HostRequestHandlers, cwd: impl Into<std::path::PathBuf>) -> Self {
        SchemeKernelProvisioner {
            state: Arc::new(ProvisionerState {
                handlers,
                cwd: cwd.into(),
                session: tokio::sync::Mutex::new(None),
            }),
        }
    }
}

impl IpythonKernelProvisioner for SchemeKernelProvisioner {
    fn ensure(
        &self,
        _on_progress: Option<BootstrapProgressHandler>,
        signal: Option<AbortSignal>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn KernelExecutor>>> + Send>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            if signal.as_ref().is_some_and(AbortSignal::is_cancelled) {
                return Err(anyhow!("Kernel start aborted"));
            }
            let mut slot = state.session.lock().await;
            if let Some(session) = slot.as_ref() {
                return Ok(Box::new(SchemeKernel {
                    session: Arc::clone(session),
                }) as Box<dyn KernelExecutor>);
            }
            let handlers = state.handlers.clone();
            let cwd = state.cwd.to_string_lossy().into_owned();
            let runtime = tokio::runtime::Handle::current();
            let session = tokio::task::spawn_blocking(move || {
                let spare = SPARE.lock().unwrap_or_else(PoisonError::into_inner).take();
                let mut engine = spare.unwrap_or_default();
                engine.register_fn(
                    "%host-request-json",
                    move |request_type: String, payload: String| -> String {
                        let reply = match (
                            handlers.get(&request_type),
                            serde_json::from_str::<Value>(&payload),
                        ) {
                            (None, _) => {
                                json!({ "error": format!("unknown host request: {request_type}") })
                            }
                            (Some(_), Err(error)) => {
                                json!({ "error": format!("{request_type}: payload is not JSON: {error}") })
                            }
                            (Some(handler), Ok(data)) => {
                                match runtime.block_on(handler(HostRequestPayload { data })) {
                                    Ok(value) => json!({ "value": value }),
                                    Err(error) => json!({ "error": format!("{error:#}") }),
                                }
                            }
                        };
                        reply.to_string()
                    },
                );
                engine.register_value("%session-cwd", SteelVal::StringV(cwd.into()));
                engine
                    .run(HOST_BRIDGE)
                    .map_err(|error| anyhow!("the Scheme host bridge failed to load: {error}"))?;
                let controller = engine.get_thread_state_controller();
                anyhow::Ok(Arc::new(SessionEngine {
                    engine: Mutex::new(engine),
                    controller,
                }))
            })
            .await
            .map_err(|error| anyhow!("the Scheme kernel's start thread failed: {error}"))??;
            *slot = Some(Arc::clone(&session));
            // The spare is spent: build the next session's.
            prewarm_scheme_kernel();
            Ok(Box::new(SchemeKernel { session }) as Box<dyn KernelExecutor>)
        })
    }

    fn kill(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            // A cell still running holds its own reference: interrupt it so
            // the dropped engine stops instead of running on unobserved.
            if let Some(session) = state.session.lock().await.take() {
                session.controller.interrupt();
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::shared::host_handler;

    fn options(
        signal: Option<AbortSignal>,
        timeout_ms: Option<u64>,
    ) -> KernelExecuteOptions<'static> {
        KernelExecuteOptions {
            signal,
            timeout_ms,
            on_stream: None,
            on_late_sent_agent_message: None,
        }
    }

    /// What a cell answered, for whole-value comparison.
    fn answer(result: ExecuteResult) -> (ExecuteStatus, String, Option<String>, Option<String>) {
        (
            result.status,
            result.stdout,
            result.result,
            result.error.map(|error| error.ename),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn definitions_persist_in_their_session_and_output_is_captured() {
        let first = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/");
        let other = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/");
        let kernel = first.ensure(None, None).await.unwrap();
        kernel
            .execute(
                "(define (area-of-square side) (* side side))",
                options(None, None),
            )
            .await
            .unwrap();
        let again = first.ensure(None, None).await.unwrap();
        let reused = again
            .execute(
                "(displayln \"squaring\") (area-of-square 12)",
                options(None, None),
            )
            .await
            .unwrap();
        let elsewhere = other
            .ensure(None, None)
            .await
            .unwrap()
            .execute("(area-of-square 12)", options(None, None))
            .await
            .unwrap();
        assert_eq!(
            (answer(reused), answer(elsewhere).0),
            (
                (
                    ExecuteStatus::Ok,
                    "squaring\n".to_string(),
                    Some("144".to_string()),
                    None
                ),
                ExecuteStatus::Error
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn host_requests_reach_the_session_handlers_and_their_errors_raise() {
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "echo.upper",
            host_handler(|payload: HostRequestPayload| async move {
                let text = payload.data["text"].as_str().unwrap_or_default();
                Ok(json!({ "text": text.to_uppercase() }))
            }),
        );
        handlers.register(
            "always.fails",
            host_handler(
                |_payload: HostRequestPayload| async move { Err(anyhow!("the host refused")) },
            ),
        );
        let kernel = SchemeKernelProvisioner::new(handlers, "/")
            .ensure(None, None)
            .await
            .unwrap();
        let echoed = kernel
            .execute(
                "(hash-ref (host-request \"echo.upper\" (hash 'text \"quiet\")) 'text)",
                options(None, None),
            )
            .await
            .unwrap();
        let refused = kernel
            .execute(
                "(host-request \"always.fails\" (hash))",
                options(None, None),
            )
            .await
            .unwrap();
        assert_eq!(
            (
                answer(echoed),
                refused.status,
                refused
                    .error
                    .is_some_and(|error| error.evalue.contains("the host refused"))
            ),
            (
                (
                    ExecuteStatus::Ok,
                    String::new(),
                    Some("\"QUIET\"".to_string()),
                    None
                ),
                ExecuteStatus::Error,
                true
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_abort_or_timeout_stops_a_runaway_cell_and_the_kernel_stays_usable() {
        let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
            .ensure(None, None)
            .await
            .unwrap();
        let spin = "(define (spin n) (spin (+ n 1))) (displayln \"spinning\") (spin 0)";
        let signal = AbortSignal::new();
        let abort = signal.clone();
        let aborting = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            abort.cancel();
        });
        let aborted = kernel
            .execute(spin, options(Some(signal), None))
            .await
            .unwrap();
        aborting.await.unwrap();
        let timed_out = kernel
            .execute(spin, options(None, Some(100)))
            .await
            .unwrap();
        let after = kernel
            .execute("(+ 1 2)", options(None, None))
            .await
            .unwrap();
        assert_eq!(
            (answer(aborted), answer(timed_out), answer(after)),
            (
                (ExecuteStatus::Aborted, "spinning\n".to_string(), None, None),
                (
                    ExecuteStatus::Error,
                    "spinning\n".to_string(),
                    None,
                    Some("Timeout".to_string())
                ),
                (
                    ExecuteStatus::Ok,
                    String::new(),
                    Some("3".to_string()),
                    None
                )
            )
        );
    }

    /// Sessions running at the same time keep their own output and
    /// definitions: the failure mode of engines copied from one template.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_sessions_never_see_each_others_output_or_definitions() {
        let sessions = ["alpha", "beta", "gamma", "delta"].map(|name| {
            tokio::spawn(async move {
                let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
                    .ensure(None, None)
                    .await
                    .unwrap();
                let mut seen = Vec::new();
                for round in 0..50 {
                    let cell = format!("(define mine \"{name}{round}\") (display mine)");
                    seen.push(
                        kernel
                            .execute(&cell, options(None, None))
                            .await
                            .unwrap()
                            .stdout,
                    );
                }
                (name, seen)
            })
        });
        for session in sessions {
            let (name, seen) = session.await.unwrap();
            let own: Vec<String> = (0..50).map(|round| format!("{name}{round}")).collect();
            assert_eq!(seen, own);
        }
    }
}
