//! What a server's configuration resolves to before it is opened: the stored
//! credential bound to its endpoint, and a stdio server's environment.
//!
//! Credentials live in the agent directory's `auth.json`, written by the
//! host's login and refresh; this side only reads them.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value};

use super::{Failure, FailureKind};

/// The variables a stdio server inherits from this process; everything else
/// it gets is named by its configuration.
const INHERITED_ENV: [&str; 7] = [
    "HOME",
    "PATH",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SystemRoot",
    "WINDIR",
];

/// A server's configuration as the host resolved it, with the credential it
/// is opened with. A connection is reused only while both are unchanged.
#[derive(Clone, PartialEq)]
pub(super) struct Settings {
    /// The `mcp.config` reply.
    pub(super) config: Map<String, Value>,
    /// The bearer token an HTTP server is sent, when it has one: a rotated
    /// token reopens the connection.
    pub(super) bearer: Option<String>,
}

/// The failure of a connection that has no usable credentials.
pub(super) fn unavailable(server: &str) -> Failure {
    Failure::new(
        FailureKind::CredentialsUnavailable,
        format!(
            "MCP credentials for '{server}' are not available. Ask the user to connect it \
             (/plugins or /mcp login {server}); do not ask them to set environment variables."
        ),
    )
}

/// The stored credential of `server`, only when it is bound to exactly `url`:
/// a token that is unbound, or bound to another endpoint (a login that
/// finished after the server was retargeted), is never sent.
pub(super) async fn stored(
    agent_dir: Option<&Path>,
    server: &str,
    url: &str,
) -> Option<Map<String, Value>> {
    let text = tokio::fs::read(agent_dir?.join("auth.json")).await.ok()?;
    let mut all = serde_json::from_slice::<Map<String, Value>>(&text).ok()?;
    let Some(Value::Object(credential)) = all.remove(&format!("mcp:{server}")) else {
        return None;
    };
    (credential.get("endpoint").and_then(Value::as_str) == Some(url)).then_some(credential)
}

/// A stored key as the host reads it: a literal, or the name of a set
/// environment variable. A `!command` value is the host's to run and
/// resolves to nothing here.
pub(super) fn stored_key(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value.starts_with('!') {
        return String::new();
    }
    std::env::var(value)
        .ok()
        .filter(|resolved| !resolved.trim().is_empty())
        .map_or_else(|| value.to_string(), |resolved| resolved.trim().to_string())
}

/// A stdio server's whole environment, and the values its configuration
/// added to the inherited ones (the secrets a diagnostic must not show).
///
/// # Errors
///
/// The configuration's `env` is malformed, or names a variable this process
/// does not have.
pub(super) fn stdio_env(
    config: &Map<String, Value>,
) -> Result<(HashMap<String, String>, Vec<String>), Failure> {
    let invalid = |reason: &str| Failure::new(FailureKind::Invalid, reason.to_string());
    let mut env: HashMap<String, String> = INHERITED_ENV
        .iter()
        .filter_map(|name| Some(((*name).to_string(), std::env::var(name).ok()?)))
        .collect();
    let declared = match config.get("env") {
        None | Some(Value::Null) => return Ok((env, Vec::new())),
        Some(Value::Object(declared)) => declared,
        Some(Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_)) => {
            return Err(invalid("the MCP stdio env must be an object"));
        }
    };
    // A server the ACP client supplied carries literal values; a settings
    // entry only names variables of this process.
    let literal = config.get("credentialSource").and_then(Value::as_str) == Some("acp");
    let mut added = Vec::new();
    for (name, value) in declared {
        let resolved = if literal {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid("an ACP MCP stdio env must contain string values"))?
        } else {
            let source = value
                .as_object()
                .filter(|reference| reference.len() == 1)
                .and_then(|reference| reference.get("env"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid("MCP stdio env values must use {\"env\": \"NAME\"} references")
                })?;
            std::env::var(source).map_err(|_| {
                Failure::new(
                    FailureKind::Invalid,
                    format!("the MCP stdio environment reference for '{name}' is unavailable"),
                )
            })?
        };
        added.push(resolved.clone());
        env.insert(name.clone(), resolved);
    }
    Ok((env, added))
}
