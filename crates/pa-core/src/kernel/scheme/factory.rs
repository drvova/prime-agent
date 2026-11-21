//! The factory: state-machine workflows of spawned child agents, offered to
//! a session's Scheme kernel.
//!
//! A factory specification is a declarative state machine of subagent
//! states: entry states, guarded transitions, bounded re-entry. The dag form
//! is sugar that compiles to machine form. `spec` is the write-time dry run
//! for both forms and the canonicalizer; `executor` runs canonical machines
//! through the session's supervisor, keeping each run alive between cells on
//! a task of its own; `library` is the `MACHINE.md` machine library.
//!
//! The factory is opt-in: while the `factory.enabled` setting is off (the
//! default; the user turns it on with `/factory on`), running, inspecting
//! and steering refuse with one message. The guide, validation and the
//! library stay available.
//!
//! Scheme reaches all of it through one primitive,
//! `(%factory-request-json operation payload-json)`, behind these functions
//! (see `factory/api.scm` for each one's reply):
//!
//! - `(factory-run spec-id #:name label)`, `(factory-status run-id)`,
//!   `(factory-stop run-id)`, `(factory-resume run-id)`,
//!   `(factory-graph [ref])`, `(factory-watch run-id [seconds])`,
//!   `(factory-help)`: the run API.
//! - `(factory-validate spec)`, `(factory-canonicalize spec)`,
//!   `(factory-compile-dag dag)`, `(factory-topological-order nodes)`: the
//!   dry run.
//! - `(factory-list-machines)`, `(factory-import-machine path)`,
//!   `(factory-export-machine target out #:overwrite #t)`: the library.

use anyhow::anyhow;
use serde_json::{json, Value};
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use super::native::CellSignal;

mod binding;
#[cfg(test)]
mod corpus_tests;
mod executor;
#[cfg(test)]
mod executor_edge_tests;
#[cfg(test)]
mod executor_tests;
mod library;
mod naming;
mod python_state;
mod python_text;
mod report;
mod run;
mod settlement;
mod spec;
#[cfg(test)]
mod spec_tests;
mod store;
#[cfg(test)]
mod surface_tests;
#[cfg(test)]
mod test_host;

pub(super) use executor::Executor;
pub use store::FactoryEnvironment;

/// The Scheme side of the factory API, defined in every session engine
/// after its `%factory-request-json` primitive is registered and the host
/// API (whose `%exact-integers` it uses) is loaded.
const API: &str = include_str!("factory/api.scm");

/// The agent-facing guide `(factory-help)` returns: authoring rules,
/// guards, joins, cycles, foreach, budgets, and the API with examples.
const HELP: &str = include_str!("factory/help.md");

impl Executor {
    /// Answer one factory request from Scheme.
    async fn answer(&self, operation: &str, payload: &Value) -> anyhow::Result<Value> {
        let text = |key: &str| {
            payload[key]
                .as_str()
                .ok_or_else(|| anyhow!("factory {operation}: {key} must be a string"))
        };
        match operation {
            "run" => {
                self.run(
                    text("spec_id")?,
                    payload["name"].as_str().map(str::to_string),
                )
                .await
            }
            "status" => self.status(text("run_id")?),
            "stop" => self.stop(text("run_id")?).await,
            "resume" => self.resume(text("run_id")?).await,
            "graph" => self.graph(payload["ref"].as_str(), report::Detail::Full),
            "watch" => {
                let timeout = payload["timeout"]
                    .as_f64()
                    .ok_or_else(|| anyhow!("timeout must be a non-negative number of seconds"))?;
                self.watch(text("run_id")?, timeout, report::Detail::Full)
                    .await
            }
            "help" => Ok(json!(HELP)),
            "validate" => Ok(json!(spec::validate_factory_spec(&payload["spec"]))),
            "canonicalize" => {
                spec::canonicalize_factory_spec(&payload["spec"]).map_err(|errors| anyhow!(errors))
            }
            "compile-dag" => Ok(match spec::compile_factory_dag(&payload["dag"]) {
                Ok(machine) => json!({"machine": machine, "errors": []}),
                Err(errors) => json!({"machine": null, "errors": errors}),
            }),
            "topological-order" => spec::topological_order(&payload["nodes"])
                .map(|order| json!(order))
                .map_err(|error| anyhow!(error)),
            "list-machines" => Ok(json!(library::scan_library(self.environment()).0)),
            "import-machine" => {
                library::import_machine(self.environment(), &self.session_path(text("path")?))
                    .map_err(|error| anyhow!(error))
            }
            "export-machine" => self.export_machine(
                text("target")?,
                text("out")?,
                if payload["overwrite"] == true {
                    library::Overwrite::Replace
                } else {
                    library::Overwrite::Refuse
                },
            ),
            unknown => Err(anyhow!("unknown factory operation: {unknown}")),
        }
    }
}

/// Define the factory API in a session's engine, whose host API is already
/// loaded. Requests are answered by `executor`; one that outlives its cell
/// (the cell was stopped) finishes on its own, so a run is never left half
/// started. Must be called from within a tokio runtime.
///
/// # Errors
///
/// The Scheme definitions failed to load.
pub(super) fn register(
    engine: &mut Engine,
    executor: Executor,
    cell: CellSignal,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    engine.register_fn(
        "%factory-request-json",
        move |operation: String, payload: String| -> String {
            let signal = cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let executor = executor.clone();
            let answered = serde_json::from_str::<Value>(&payload)
                .map_err(|error| anyhow!("the payload is not JSON: {error}"))
                .and_then(|payload| {
                    runtime.block_on(async move {
                        let work =
                            tokio::spawn(
                                async move { executor.answer(&operation, &payload).await },
                            );
                        tokio::select! {
                            biased;
                            () = signal.cancelled() => {
                                Err(anyhow!("the factory call was left: its cell was stopped"))
                            }
                            joined = work => joined
                                .map_err(|error| anyhow!("the factory call failed: {error}"))?,
                        }
                    })
                });
            let reply = match answered {
                Ok(value) => json!({ "value": value }),
                Err(error) => json!({ "error": format!("{error:#}") }),
            };
            reply.to_string()
        },
    );
    engine
        .run(API)
        .map_err(|error| anyhow!("the Scheme factory API failed to load: {error}"))?;
    Ok(())
}
