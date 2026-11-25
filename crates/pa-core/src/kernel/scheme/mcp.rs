//! MCP for a session's Scheme kernel: a generic MCP client in this process,
//! reached from a cell through the `mcp-*` functions.
//!
//! The session's [`Registry`] opens each configured server on first use and
//! keeps it open: a stdio server is a child process speaking newline-delimited
//! JSON-RPC, an HTTP server is reached over streamable HTTP. What a server is
//! and which credentials it gets is the host's answer to the `mcp.config`,
//! `mcp.refresh` and `mcp.begin_login` requests, the same handlers the Python
//! kernel's `mcp` module asked.
//!
//! - `(mcp-list-tools server)`: the server's tools, each a hash of `'name`,
//!   `'description`, `'inputSchema`; complete or an error, never partial.
//! - `(mcp-describe-tool server tool)`: one such hash.
//! - `(mcp-call-tool server tool [arguments])`: the tool's structured content
//!   when it has any, else its text, else its content blocks. A result the
//!   server flags as an error raises.
//! - `(mcp-search-tools query #:connection-id id #:limit n)`: `'tools` (each
//!   `'connectionId`, `'name`, `'description`), `'searched`, `'unavailable`,
//!   `'truncated`.
//! - `(mcp-list-connections)`, `(mcp-list-plugins #:connection-status s
//!   #:limit n #:cursor c)`, `(mcp-search-plugins query #:limit n)`: the
//!   host's inventory, with secret-looking keys dropped.
//! - `(mcp-status servers #:timeout-ms n)`: per server `'server`, `'tools`
//!   (each `'name`, `'description`) or `'error`.
//! - `(mcp-login server)`: `'server`, `'status` (`"logged_in"`,
//!   `"unavailable"`, `"failed"`), `'message`.
//! - `(mcp-reload [server])`: close one server's connection, or all of them;
//!   the next call opens it again.
//! - `(mcp-list-resources server)`, `(mcp-list-resource-templates server)`,
//!   `(mcp-read-resource server uri)`, `(mcp-list-prompts server)`,
//!   `(mcp-get-prompt server name [arguments])`: the server's replies.
//!
//! Keys of a reply are the server's own (`'inputSchema`, not
//! `'input-schema`), a JSON null is void, and whole numbers are exact
//! integers. Every failure raises an error that starts with the function's
//! name.
//!
//! A call blocks its cell. It ends when the server answers, when the server's
//! call timeout passes, or when the cell is aborted or times out; the
//! connection stays open for the next call either way. Calls to one server
//! run one at a time. `kill` on the session's kernel closes every server and
//! reaps the stdio ones.

use std::sync::{Arc, PoisonError};

use anyhow::anyhow;
use serde_json::{json, Value};
use steel::rerrs::{ErrorKind, SteelErr};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use super::native::CellSignal;
use crate::kernel::call_refusals::CallRefusal;

mod connection;
mod credentials;
mod http;
mod registry;
mod stdio;
#[cfg(test)]
mod tests;

pub(super) use registry::Registry;

/// The Scheme side of the client, defined in every session engine after its
/// `%mcp-call` primitive is registered and the host API is loaded.
const API: &str = include_str!("mcp/api.scm");

/// The MCP revision this client asks for; a server may answer with an older
/// one, which is then used.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// What went wrong, for the fixed summaries a search across connections
/// reports instead of a server's own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// The connection has no usable credentials: the user must connect it.
    CredentialsUnavailable,
    /// The server did not complete its startup handshake.
    Startup,
    /// The server or tool is not declared.
    NotDeclared,
    /// Settings or policy exclude the server or tool.
    Excluded,
    /// The server did not answer in time.
    TimedOut,
    /// The tool ran and reported an error.
    Tool,
    /// The server broke the protocol, answered with an error, or went away.
    Protocol,
    /// The call's own arguments or the server's configuration are malformed.
    Invalid,
    /// The call names an argument the tool's schema does not declare.
    UndeclaredArgument,
    /// A host request failed.
    Host,
    /// The session's connections were shut down while the call ran.
    Stopped,
}

impl FailureKind {
    /// The name a per-server status or search summary leads with.
    fn name(self) -> &'static str {
        match self {
            FailureKind::CredentialsUnavailable => "CredentialsUnavailable",
            FailureKind::Startup => "StartupError",
            FailureKind::NotDeclared => "NotDeclared",
            FailureKind::Excluded => "Excluded",
            FailureKind::TimedOut => "Timeout",
            FailureKind::Tool => "ToolError",
            FailureKind::Protocol => "ProtocolError",
            FailureKind::Invalid | FailureKind::UndeclaredArgument => "InvalidRequest",
            FailureKind::Host => "HostError",
            FailureKind::Stopped => "Stopped",
        }
    }
}

/// A failed MCP operation: what kind, and the reason shown to the cell.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    kind: FailureKind,
    message: String,
}

impl Failure {
    fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Failure {
            kind,
            message: message.into(),
        }
    }
}

/// What a JSON-RPC reply settles its request with: the result, or the
/// server's error.
fn reply_outcome(mut reply: Value) -> Result<Value, Failure> {
    match reply.get("error").filter(|error| !error.is_null()) {
        Some(error) => Err(Failure::new(
            FailureKind::Protocol,
            format!(
                "the MCP server answered with error {}: {}",
                error.get("code").unwrap_or(&Value::Null),
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message"),
            ),
        )),
        None => Ok(reply.get_mut("result").map_or_default(Value::take)),
    }
}

/// The frame that answers a request the server sent to this client: a ping
/// is answered, anything else is refused as unsupported (this client offers
/// no sampling, roots, or elicitation). `None` for a message that is not a
/// request.
fn server_request_answer(message: &Value) -> Option<Value> {
    let method = message.get("method")?.as_str()?;
    let id = message.get("id").filter(|id| !id.is_null())?;
    Some(if method == "ping" {
        json!({"jsonrpc": "2.0", "id": id, "result": {}})
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("{method} is not supported by this client")},
        })
    })
}

/// Define the `mcp-*` functions in a session's engine. Their calls reach
/// `registry` and watch `cell` to end with the cell that made them. Must be
/// called from within a tokio runtime: calls are awaited on it.
///
/// # Errors
///
/// The Scheme definitions failed to load.
pub(super) fn register(
    engine: &mut Engine,
    registry: Arc<Registry>,
    cell: CellSignal,
    refusals: super::contracts::Refusals,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    engine.register_fn(
        "%mcp-call",
        move |operation: String, payload: String| -> String {
            let signal = cell.lock().unwrap_or_else(PoisonError::into_inner).clone();
            let outcome = match serde_json::from_str::<Value>(&payload) {
                Err(error) => Err(format!("the arguments are not plain data: {error}")),
                // The engine cannot interrupt this thread: the call itself
                // ends when its cell is stopped.
                Ok(payload) => runtime.block_on(async {
                    tokio::select! {
                        biased;
                        () = signal.cancelled() => {
                            Err("the call was abandoned: its cell was stopped".to_string())
                        }
                        outcome = registry.dispatch(&operation, &payload) => {
                            outcome.map_err(|failure| {
                                if failure.kind == FailureKind::UndeclaredArgument {
                                    refusals.count(CallRefusal::ToolArgument);
                                }
                                failure.message
                            })
                        }
                    }
                }),
            };
            match outcome {
                Ok(value) => json!({ "value": value }),
                Err(reason) => json!({ "error": reason }),
            }
            .to_string()
        },
    );
    // Scheme's own `error` prints its message as written data, escaping the
    // line breaks of a server's stderr tail; this raises it as it is.
    engine.register_value(
        "%mcp-raise",
        SteelVal::FuncV(|arguments| match arguments {
            [SteelVal::StringV(reason)] => {
                Err(SteelErr::new(ErrorKind::Generic, reason.to_string()))
            }
            _ => Err(SteelErr::new(
                ErrorKind::ArityMismatch,
                "%mcp-raise takes one string".to_string(),
            )),
        }),
    );
    super::contracts::load(engine, API)
        .map_err(|error| anyhow!("the Scheme MCP API failed to load: {error}"))?;
    Ok(())
}
