//! The factory: state-machine workflows of spawned child agents, offered to
//! a session's Scheme kernel.
//!
//! A factory specification is a declarative state machine of subagent
//! states: entry states, guarded transitions, bounded re-entry. The dag form
//! is sugar that compiles to machine form. [`spec`] is the write-time dry
//! run for both forms and the canonicalizer.
//!
//! Scheme reaches all of it through one primitive,
//! `(%factory-request-json operation payload-json)`, behind these functions:
//!
//! - `(factory-validate spec)` returns the list of validation sentences,
//!   empty when the spec is valid.
//! - `(factory-canonicalize spec)` returns the canonical machine form, or
//!   raises the joined sentences.
//! - `(factory-compile-dag dag)` returns `{machine, errors}`: the compiled
//!   machine or the dag-level sentences.
//! - `(factory-topological-order nodes)` returns node ids in dependency
//!   order, or raises.

use anyhow::anyhow;
use serde_json::{json, Value};
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

// STAGE-A: removed with the executor, the caller of these three.
#[cfg_attr(not(test), expect(dead_code, reason = "the executor is its caller"))]
mod binding;
#[cfg(test)]
mod corpus_tests;
#[cfg_attr(not(test), expect(dead_code, reason = "the executor is its caller"))]
mod naming;
#[allow(
    dead_code,
    reason = "the executor is the caller of the JSON text forms"
)]
mod python_text;
mod spec;
#[cfg(test)]
mod spec_tests;

/// The Scheme side of the factory API, defined in every session engine
/// after its `%factory-request-json` primitive is registered and the host
/// API (whose `%exact-integers` it uses) is loaded.
const API: &str = include_str!("factory/api.scm");

/// Answer one factory request from Scheme.
fn answer(operation: &str, payload: &Value) -> anyhow::Result<Value> {
    match operation {
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
        unknown => Err(anyhow!("unknown factory operation: {unknown}")),
    }
}

/// Define the factory API in a session's engine, whose host API is already
/// loaded.
///
/// # Errors
///
/// The Scheme definitions failed to load.
pub(super) fn register(engine: &mut Engine) -> anyhow::Result<()> {
    engine.register_fn(
        "%factory-request-json",
        |operation: String, payload: String| -> String {
            let reply = match serde_json::from_str::<Value>(&payload)
                .map_err(|error| anyhow!("the payload is not JSON: {error}"))
                .and_then(|payload| answer(&operation, &payload))
            {
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
