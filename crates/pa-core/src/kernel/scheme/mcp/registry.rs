//! A session's MCP connections: one per configured server, opened on first
//! use, reused while the host still resolves the server to the same
//! configuration and credential, and closed with the session's kernel.
//!
//! Each server has its own lock, held from resolving its configuration to
//! the end of the call: calls to one server run one at a time, and a call
//! that arrives while the server is still opening joins that open instead of
//! starting a second one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use tokio::sync::{OwnedMappedMutexGuard, OwnedMutexGuard};

use super::connection::{Connection, DEFAULT_STARTUP_TIMEOUT};
use super::credentials::{self, Settings};
use super::{Failure, FailureKind};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::tools::tool_definition::AbortSignal;

/// How long the host has to answer an inventory request.
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long closing every connection may take; what is still open then is
/// killed. Below the host's own kernel shutdown deadline.
const SHUTDOWN_LIMIT: Duration = Duration::from_millis(2500);
/// A stored token this close to its expiry is refreshed before it is used,
/// so it never expires mid-request.
const EXPIRY_SKEW_MS: f64 = 30_000.0;
/// Connections a search without a named connection opens at most.
const MAX_SEARCHED_CONNECTIONS: usize = 8;
/// Parts of an inventory key that mark it as a secret. The host already
/// sends safe metadata only; this is a second line of defence, never applied
/// to a server's own schemas or results.
const SECRET_KEY_PARTS: [&str; 11] = [
    "token",
    "secret",
    "password",
    "credential",
    "authorization",
    "apikey",
    "api_key",
    "api-key",
    "privatekey",
    "private_key",
    "private-key",
];

/// One server's connection, behind the lock that serialises its calls.
type Slot = Arc<tokio::sync::Mutex<Option<Connection>>>;

/// A session's MCP client: its open connections and the host requests that
/// configure them.
pub(in crate::kernel::scheme) struct Registry {
    handlers: HostRequestHandlers,
    /// The agent directory, whose `auth.json` holds the stored credentials.
    agent_dir: OnceLock<PathBuf>,
    servers: Mutex<HashMap<String, Slot>>,
    /// Cancelled by [`Registry::shutdown`] to end the work in flight; the
    /// work after a shutdown runs under a fresh one.
    stop: Mutex<AbortSignal>,
}

/// The non-empty string `payload` carries under `key`.
fn named<'a>(payload: &'a Value, key: &str) -> Result<&'a str, Failure> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            Failure::new(
                FailureKind::Invalid,
                format!("{key} must be a non-empty string"),
            )
        })
}

/// The whole number between 1 and `maximum` that `payload` carries under
/// `key`.
fn bounded(payload: &Value, key: &str, maximum: u64) -> Result<u64, Failure> {
    payload
        .get(key)
        .and_then(Value::as_f64)
        .filter(|limit| limit.fract() == 0.0 && (1.0..=maximum as f64).contains(limit))
        .map(|limit| limit as u64)
        .ok_or_else(|| {
            Failure::new(
                FailureKind::Invalid,
                format!("{key} must be an integer between 1 and {maximum}"),
            )
        })
}

/// A copy of one inventory value without its secret-looking keys.
fn without_secrets(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(key, _)| {
                    let key = key.to_lowercase();
                    !SECRET_KEY_PARTS.iter().any(|part| key.contains(part))
                })
                .map(|(key, value)| (key.clone(), without_secrets(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_secrets).collect()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
    }
}

impl Registry {
    /// A client with no open connections, configured by `handlers`.
    pub(in crate::kernel::scheme) fn new(handlers: HostRequestHandlers) -> Self {
        Registry {
            handlers,
            agent_dir: OnceLock::new(),
            servers: Mutex::default(),
            stop: Mutex::new(AbortSignal::new()),
        }
    }

    /// Read stored credentials from `agent_dir`'s `auth.json`. Without it, a
    /// server that needs a stored credential reports it as unavailable. The
    /// first directory set stays.
    pub(in crate::kernel::scheme) fn set_agent_dir(&self, agent_dir: PathBuf) {
        // A second call names the same session's directory again.
        let _ = self.agent_dir.set(agent_dir);
    }

    /// One host request.
    async fn host(&self, request_type: &str, mut data: Value) -> anyhow::Result<Value> {
        let handler = self
            .handlers
            .get(request_type)
            .ok_or_else(|| anyhow::anyhow!("unknown host request: {request_type}"))?;
        // Some handlers read the type from the payload.
        data["type"] = json!(request_type);
        handler(HostRequestPayload { data }).await
    }

    /// What the host resolves `server` to right now: its configuration, and
    /// for an HTTP server the bearer token it is sent, refreshed first when
    /// the stored one is about to expire.
    async fn settings(&self, server: &str) -> Result<Settings, Failure> {
        // The host's own words can carry OAuth URLs: they are not passed on.
        let config = match tokio::time::timeout(
            DEFAULT_STARTUP_TIMEOUT,
            self.host("mcp.config", json!({ "server": server })),
        )
        .await
        {
            Ok(Ok(Value::Object(config))) if !config.is_empty() => config,
            Ok(Ok(_)) => {
                return Err(Failure::new(
                    FailureKind::NotDeclared,
                    format!("MCP server '{server}' is not declared in the user's settings"),
                ));
            }
            Ok(Err(_)) | Err(_) => {
                return Err(Failure::new(
                    FailureKind::Host,
                    format!("could not load the MCP configuration for '{server}'"),
                ));
            }
        };
        if config.get("enabled") == Some(&Value::Bool(false)) {
            return Err(Failure::new(
                FailureKind::Excluded,
                format!("MCP server '{server}' is disabled"),
            ));
        }
        let text = |key: &str| config.get(key).and_then(Value::as_str);
        // A server the ACP client supplied carries its own headers; a stdio
        // server takes no bearer.
        if text("type") != Some("http") || text("credentialSource") == Some("acp") {
            return Ok(Settings {
                config,
                bearer: None,
            });
        }
        let agent_dir = self.agent_dir.get().map(PathBuf::as_path);
        let url = text("url").unwrap_or_default();
        if text("credentialSource") == Some("static-token") {
            // A pasted token is a literal: it has no refresh, and is never
            // read as a variable name.
            let bearer = credentials::stored(agent_dir, server, url)
                .await
                .and_then(|stored| Some(stored.get("bearer")?.as_str()?.trim().to_string()))
                .filter(|bearer| !bearer.is_empty())
                .ok_or_else(|| credentials::unavailable(server))?;
            return Ok(Settings {
                config,
                bearer: Some(bearer),
            });
        }
        let variable = text("bearerTokenEnvVar");
        let oauth = config.get("oauth") == Some(&Value::Bool(true));
        let mut bearer = variable
            .and_then(|variable| pa_types::session_scope::var(variable).ok())
            .map(|token| token.trim().to_string())
            .unwrap_or_default();
        if oauth && bearer.is_empty() {
            let mut stored = credentials::stored(agent_dir, server, url).await;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |since| since.as_millis() as f64);
            let expiring = stored
                .as_ref()
                .and_then(|stored| stored.get("expires")?.as_f64())
                .is_some_and(|expires| expires <= now_ms + EXPIRY_SKEW_MS);
            if expiring {
                self.host("mcp.refresh", json!({ "server": server }))
                    .await
                    .map_err(|_| {
                        Failure::new(
                            FailureKind::Host,
                            format!("could not refresh the MCP credentials for '{server}'"),
                        )
                    })?;
                stored = credentials::stored(agent_dir, server, url).await;
            }
            bearer = stored
                .as_ref()
                .and_then(|stored| {
                    ["access", "key"]
                        .iter()
                        .find_map(|key| stored.get(*key)?.as_str())
                })
                .map(credentials::stored_key)
                .unwrap_or_default();
        }
        if bearer.is_empty() && (oauth || variable.is_some()) {
            return Err(credentials::unavailable(server));
        }
        Ok(Settings {
            config,
            bearer: Some(bearer).filter(|bearer| !bearer.is_empty()),
        })
    }

    /// The open connection to `server`, with its lock held: reused when the
    /// host still resolves the server to what it was opened with, else
    /// closed and opened again.
    async fn connect(
        &self,
        server: &str,
    ) -> Result<OwnedMappedMutexGuard<Option<Connection>, Connection>, Failure> {
        let slot = Arc::clone(
            self.servers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(server.to_string())
                .or_default(),
        );
        let mut held = slot.lock_owned().await;
        let settings = self.settings(server).await?;
        if !held
            .as_ref()
            .is_some_and(|connection| connection.serves(&settings))
        {
            if let Some(stale) = held.take() {
                stale.shutdown().await;
            }
            *held = Some(Connection::open(server, settings).await?);
        }
        OwnedMutexGuard::try_map(held, Option::as_mut).map_err(|_| {
            Failure::new(
                FailureKind::Protocol,
                format!("the connection to MCP server '{server}' was lost while it opened"),
            )
        })
    }

    /// Close the connections in `slots`, each once its call in flight ends.
    async fn close(&self, slots: Vec<Slot>) {
        futures::future::join_all(slots.into_iter().map(|slot| async move {
            let open = slot.lock().await.take();
            if let Some(connection) = open {
                connection.shutdown().await;
            }
        }))
        .await;
    }

    /// The slot of every server named so far, or of `server` alone.
    fn slots(&self, server: Option<&str>) -> Vec<Slot> {
        let servers = self.servers.lock().unwrap_or_else(PoisonError::into_inner);
        match server {
            Some(server) => servers.get(server).into_iter().cloned().collect(),
            None => servers.values().cloned().collect(),
        }
    }

    /// The work in flight ends with this signal.
    fn stopping(&self) -> AbortSignal {
        self.stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Open each of `servers` (those not open yet) and list its tools, all at
    /// once, each bounded by `per_server_timeout_ms`: one
    /// `{server, tools, error}` per server, where `tools` is a list of
    /// `{name, description}`, or null with the `error` that server failed
    /// with. This is what settles a session's servers in the background; a
    /// call that arrives meanwhile joins the open in flight.
    pub(in crate::kernel::scheme) async fn tool_listing(
        &self,
        servers: &[String],
        per_server_timeout_ms: u64,
    ) -> Vec<Value> {
        let stop = self.stopping();
        let limit = Duration::from_millis(per_server_timeout_ms.max(1));
        futures::future::join_all(servers.iter().map(|server| {
            let stop = stop.clone();
            async move {
                let listed = tokio::select! {
                    biased;
                    () = stop.cancelled() => Err(Failure::new(
                        FailureKind::Stopped,
                        "the session's MCP connections were shut down",
                    )),
                    listed = tokio::time::timeout(limit, self.connect(server)) => match listed {
                        Ok(connected) => connected.map(|connection| connection.tools()),
                        Err(_) => Err(Failure::new(
                            FailureKind::TimedOut,
                            format!("no tool listing within {} ms", limit.as_millis()),
                        )),
                    },
                };
                match listed {
                    Ok(tools) => json!({
                        "server": server,
                        "tools": tools
                            .iter()
                            .map(|tool| json!({"name": tool["name"], "description": tool["description"]}))
                            .collect::<Vec<_>>(),
                        "error": null,
                    }),
                    Err(failure) => json!({
                        "server": server,
                        "tools": null,
                        "error": format!("{}: {}", failure.kind.name(), failure.message),
                    }),
                }
            }
        }))
        .await
    }

    /// Close every connection and end the work in flight: stdio servers are
    /// ended and reaped, and whatever is still open after the shutdown limit
    /// is killed. The client stays usable: a later call opens its server
    /// again.
    pub(in crate::kernel::scheme) async fn shutdown(&self) {
        std::mem::replace(
            &mut *self.stop.lock().unwrap_or_else(PoisonError::into_inner),
            AbortSignal::new(),
        )
        .cancel();
        let slots: Vec<Slot> = self
            .servers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        // Past the limit the closing is dropped, and each connection it had
        // not finished with is killed by its own drop.
        let _ = tokio::time::timeout(SHUTDOWN_LIMIT, self.close(slots)).await;
    }

    /// Run one operation of the Scheme API (see the module docs) to its
    /// reply.
    ///
    /// # Errors
    ///
    /// The operation's own failure, or [`FailureKind::Stopped`] when the
    /// client was shut down while it ran.
    pub(super) async fn dispatch(
        &self,
        operation: &str,
        payload: &Value,
    ) -> Result<Value, Failure> {
        let given = |key: &str| {
            payload
                .get(key)
                .filter(|value| !matches!(value, Value::Null | Value::Bool(false)))
        };
        // The arguments of a call or a prompt: a hash, none by default.
        let arguments = || match given("arguments") {
            None => Ok(json!({})),
            Some(given) => given
                .as_object()
                .map(|arguments| Value::Object(arguments.clone()))
                .ok_or_else(|| Failure::new(FailureKind::Invalid, "arguments must be a hash")),
        };
        // One sanitised page of a host inventory: its entries under `key`,
        // and the rest of the reply.
        let inventory = |request_type: &'static str, request: Value, key: &'static str| async move {
            let malformed = || {
                Failure::new(
                    FailureKind::Host,
                    format!("the MCP {key} inventory from the host is malformed"),
                )
            };
            // The host's own words can carry credential-bearing URLs: only
            // the outcome is passed on.
            let reply =
                match tokio::time::timeout(INVENTORY_TIMEOUT, self.host(request_type, request))
                    .await
                {
                    Ok(Ok(Value::Object(reply))) => reply,
                    Ok(Ok(_)) => return Err(malformed()),
                    Ok(Err(_)) => {
                        return Err(Failure::new(
                            FailureKind::Host,
                            format!("the MCP {request_type} request failed"),
                        ));
                    }
                    Err(_) => {
                        return Err(Failure::new(
                            FailureKind::TimedOut,
                            format!("the MCP {request_type} request timed out"),
                        ));
                    }
                };
            let entries = reply
                .get(key)
                .and_then(Value::as_array)
                .filter(|entries| entries.iter().all(Value::is_object))
                .ok_or_else(malformed)?
                .iter()
                .map(without_secrets)
                .collect::<Vec<_>>();
            Ok((entries, reply))
        };
        // A page of the plugin catalog as the cell sees it.
        let plugin_page = |(plugins, reply): (Vec<Value>, Map<String, Value>)| {
            let next = reply.get("nextCursor").filter(|next| !next.is_null());
            if next.is_some_and(|next| next.as_str().is_none_or(str::is_empty)) {
                return Err(Failure::new(
                    FailureKind::Host,
                    "the MCP plugins inventory from the host is malformed",
                ));
            }
            Ok(json!({ "plugins": plugins, "nextCursor": next }))
        };
        // The user's connections, each with its `connectionId`.
        let connections = || async {
            let (connections, _) =
                inventory("mcp.list_connections", json!({}), "connections").await?;
            if connections.iter().any(|entry| {
                entry
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            }) {
                return Err(Failure::new(
                    FailureKind::Host,
                    "the MCP connections inventory from the host is malformed",
                ));
            }
            Ok(connections)
        };
        // The tools of one connection whose name or description contains
        // `needle`, at most `limit` of them.
        let matching = |connection: &Connection, id: &str, needle: &str, limit: usize| {
            connection
                .tools()
                .into_iter()
                .filter(|tool| {
                    ["name", "description"].iter().any(|field| {
                        tool[*field]
                            .as_str()
                            .is_some_and(|text| text.to_lowercase().contains(needle))
                    })
                })
                .take(limit)
                .map(|tool| json!({"connectionId": id, "name": tool["name"], "description": tool["description"]}))
                .collect::<Vec<_>>()
        };
        // Every entry of one of a server's paginated listings.
        let listing = |method: &'static str, key: &'static str| async move {
            let connection = self.connect(named(payload, "server")?).await?;
            connection.list(method, key).await.map(Value::Array)
        };
        let work = async {
            match operation {
                "list_tools" => Ok(Value::Array(
                    self.connect(named(payload, "server")?).await?.tools(),
                )),
                "describe_tool" => {
                    let connection = self.connect(named(payload, "server")?).await?;
                    connection.tool(named(payload, "tool")?).cloned()
                }
                "call_tool" => {
                    let (tool, arguments) = (named(payload, "tool")?, arguments()?);
                    let connection = self.connect(named(payload, "server")?).await?;
                    connection.call_tool(tool, &arguments).await
                }
                "list_resources" => listing("resources/list", "resources").await,
                "list_resource_templates" => {
                    listing("resources/templates/list", "resourceTemplates").await
                }
                "list_prompts" => listing("prompts/list", "prompts").await,
                "read_resource" => {
                    let uri = named(payload, "uri")?;
                    let connection = self.connect(named(payload, "server")?).await?;
                    connection
                        .request("resources/read", json!({ "uri": uri }))
                        .await
                }
                "get_prompt" => {
                    let (name, arguments) = (named(payload, "name")?, arguments()?);
                    let connection = self.connect(named(payload, "server")?).await?;
                    connection
                        .request("prompts/get", json!({"name": name, "arguments": arguments}))
                        .await
                }
                "search_tools" => {
                    let needle = named(payload, "query")?.trim().to_lowercase();
                    let limit = bounded(payload, "limit", 50)? as usize;
                    if given("connection_id").is_some() {
                        let id = named(payload, "connection_id")?;
                        let tools = matching(&*self.connect(id).await?, id, &needle, limit);
                        return Ok(json!({
                            "truncated": tools.len() >= limit,
                            "tools": tools,
                            "searched": [id],
                            "unavailable": [],
                        }));
                    }
                    let connected: Vec<String> = connections()
                        .await?
                        .iter()
                        .filter(|entry| {
                            entry.get("status").and_then(Value::as_str) == Some("connected")
                        })
                        .filter_map(|entry| Some(entry.get("connectionId")?.as_str()?.to_string()))
                        .collect();
                    let (mut tools, mut searched, mut unavailable) =
                        (Vec::new(), Vec::new(), Vec::new());
                    for id in connected.iter().take(MAX_SEARCHED_CONNECTIONS) {
                        match self.connect(id).await {
                            Ok(connection) => {
                                let room = limit.saturating_sub(tools.len());
                                tools.extend(matching(&connection, id, &needle, room));
                                searched.push(id.clone());
                            }
                            // A server's own words can carry credentials: a
                            // fixed summary of what failed stands in.
                            Err(failure) => {
                                let summary = match failure.kind {
                                    FailureKind::CredentialsUnavailable => {
                                        "credentials for this connection are not available; the user must connect it"
                                    }
                                    FailureKind::Startup => "the MCP server failed during startup",
                                    FailureKind::NotDeclared => "the connection is not declared",
                                    FailureKind::Excluded => "settings exclude this connection",
                                    FailureKind::TimedOut => "opening the connection timed out",
                                    FailureKind::Tool
                                    | FailureKind::Protocol
                                    | FailureKind::Invalid
                                    | FailureKind::UndeclaredArgument
                                    | FailureKind::Host
                                    | FailureKind::Stopped => "the connection could not be opened or searched",
                                };
                                unavailable.push(json!({
                                    "connectionId": id,
                                    "error": format!("{}: {summary}", failure.kind.name()),
                                }));
                            }
                        }
                    }
                    Ok(json!({
                        "truncated": tools.len() >= limit || connected.len() > MAX_SEARCHED_CONNECTIONS,
                        "tools": tools,
                        "searched": searched,
                        "unavailable": unavailable,
                    }))
                }
                "list_connections" => connections().await.map(Value::Array),
                "list_plugins" => {
                    let mut request = json!({ "limit": bounded(payload, "limit", 200)? });
                    if given("connection_status").is_some() {
                        let status = named(payload, "connection_status")?;
                        if !matches!(status, "connected" | "not_connected") {
                            return Err(Failure::new(
                                FailureKind::Invalid,
                                "connection-status must be \"connected\" or \"not_connected\"",
                            ));
                        }
                        request["connectionStatus"] = json!(status);
                    }
                    if given("cursor").is_some() {
                        let cursor = named(payload, "cursor")?;
                        if cursor.chars().count() > 512 {
                            return Err(Failure::new(
                                FailureKind::Invalid,
                                "cursor must be at most 512 characters",
                            ));
                        }
                        request["cursor"] = json!(cursor);
                    }
                    plugin_page(inventory("mcp.list_plugins", request, "plugins").await?)
                }
                "search_plugins" => {
                    let request = json!({
                        "query": named(payload, "query")?.trim(),
                        "limit": bounded(payload, "limit", 50)?,
                    });
                    plugin_page(inventory("mcp.search_plugins", request, "plugins").await?)
                }
                "status" => {
                    let servers = given("servers")
                        .and_then(Value::as_array)
                        .and_then(|servers| {
                            servers
                                .iter()
                                .map(|server| server.as_str().map(str::to_string))
                                .collect::<Option<Vec<_>>>()
                        })
                        .ok_or_else(|| {
                            Failure::new(FailureKind::Invalid, "servers must be a list of names")
                        })?;
                    let limit = bounded(payload, "timeout_ms", 600_000)?;
                    Ok(Value::Array(self.tool_listing(&servers, limit).await))
                }
                "login" => {
                    let server = named(payload, "server")?;
                    let outcome = |status: &str, message: String| {
                        Ok(json!({"server": server, "status": status, "message": message}))
                    };
                    // The host's own words can carry OAuth URLs and
                    // parameters: a failed login says only that it failed.
                    let Ok(reply) = self
                        .host("mcp.begin_login", json!({ "server": server }))
                        .await
                    else {
                        return outcome(
                            "failed",
                            format!(
                                "the login did not complete; ask the user to run /mcp login {server} in Prime Agent"
                            ),
                        );
                    };
                    match reply.get("status").and_then(Value::as_str) {
                        Some("unavailable") => outcome(
                            "unavailable",
                            reply
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        ),
                        Some("logged_in") => {
                            // A connection opened before the credential
                            // existed is not the one to keep.
                            self.close(self.slots(Some(server))).await;
                            outcome("logged_in", format!("logged in to {server}"))
                        }
                        Some(_) | None => Err(Failure::new(
                            FailureKind::Host,
                            "the host's answer to the login is not one this client knows",
                        )),
                    }
                }
                "reload" => {
                    let server = match given("server") {
                        Some(_) => Some(named(payload, "server")?),
                        None => None,
                    };
                    self.close(self.slots(server)).await;
                    Ok(Value::Null)
                }
                unknown => Err(Failure::new(
                    FailureKind::Invalid,
                    format!("unknown MCP operation: {unknown}"),
                )),
            }
        };
        let stop = self.stopping();
        tokio::select! {
            biased;
            () = stop.cancelled() => Err(Failure::new(
                FailureKind::Stopped,
                "the session's MCP connections were shut down",
            )),
            outcome = work => outcome,
        }
    }
}
