//! One open MCP server: the initialize handshake, the tool inventory taken
//! at startup, and the requests made of it afterwards.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::credentials::Settings;
use super::http::HttpTransport;
use super::stdio::StdioTransport;
use super::{Failure, FailureKind, PROTOCOL_VERSION};

/// How long a server has to start and list its tools when its configuration
/// names no `startupTimeoutMs`.
pub(super) const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long one request may take when it names no `callTimeoutMs`.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Pages of one listing that are followed: a server that paginates without
/// end must not hold discovery forever.
const MAX_PAGES: usize = 25;

/// How a server is reached.
enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

/// An initialized connection to one server.
pub(super) struct Connection {
    server: String,
    /// What the connection was opened with: it is reused only while the
    /// host still resolves the server to the same.
    settings: Settings,
    transport: Transport,
    /// The server's complete tool inventory, by name, in the server's order.
    tools: Map<String, Value>,
    call_timeout: Duration,
}

impl Connection {
    /// Open `server` as `settings` describes: start or reach it, complete the
    /// handshake, and take its tool inventory, all within its startup
    /// timeout. A stdio server that fails here is killed.
    ///
    /// # Errors
    ///
    /// The configuration is malformed, or the server did not start, answer
    /// the handshake, or list its tools in time.
    pub(super) async fn open(server: &str, settings: Settings) -> Result<Self, Failure> {
        let millis = |name: &str, default: Duration| match settings.config.get(name) {
            None | Some(Value::Null) => Ok(default),
            Some(value) => value
                .as_f64()
                .filter(|millis| *millis > 0.0)
                .map(|millis| Duration::from_secs_f64(millis / 1000.0))
                .ok_or_else(|| {
                    Failure::new(
                        FailureKind::Invalid,
                        format!("MCP server '{server}' {name} must be positive milliseconds"),
                    )
                }),
        };
        let startup = millis("startupTimeoutMs", DEFAULT_STARTUP_TIMEOUT)?;
        let call_timeout = millis("callTimeoutMs", DEFAULT_CALL_TIMEOUT)?;
        let transport = match settings.config.get("type").and_then(Value::as_str) {
            Some("http") => Transport::Http(HttpTransport::start(
                server,
                &settings.config,
                settings.bearer.as_deref(),
            )?),
            Some("stdio") => Transport::Stdio(StdioTransport::start(server, &settings.config)?),
            Some(_) | None => {
                return Err(Failure::new(
                    FailureKind::Invalid,
                    format!("MCP server '{server}' has an unsupported transport"),
                ));
            }
        };
        let mut connection = Connection {
            server: server.to_string(),
            settings,
            transport,
            tools: Map::new(),
            call_timeout,
        };
        let handshake = async {
            let hello = connection
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {"name": "prime-agent", "version": env!("CARGO_PKG_VERSION")},
                    }),
                )
                .await?;
            match &connection.transport {
                Transport::Stdio(stdio) => stdio.notify("notifications/initialized")?,
                Transport::Http(http) => {
                    if let Some(version) = hello.get("protocolVersion").and_then(Value::as_str) {
                        http.negotiated(version);
                    }
                    http.notify("notifications/initialized", call_timeout)
                        .await?;
                }
            }
            connection.list("tools/list", "tools").await
        };
        let listed = match tokio::time::timeout(startup, handshake).await {
            Ok(listed) => listed,
            Err(_) => Err(Failure::new(
                FailureKind::TimedOut,
                format!(
                    "MCP server '{server}' did not finish starting within {} ms",
                    startup.as_millis()
                ),
            )),
        };
        match (listed, &connection.transport) {
            (Ok(listed), transport) => {
                if let Transport::Stdio(stdio) = transport {
                    stdio.started();
                }
                // A tool listed twice keeps its last description.
                connection.tools = listed
                    .into_iter()
                    .filter_map(|mut tool| {
                        let name = tool.get("name")?.as_str()?.to_string();
                        let described = json!({
                            "name": name,
                            "description": tool.get("description").and_then(Value::as_str).unwrap_or_default(),
                            "inputSchema": tool
                                .get_mut("inputSchema")
                                .filter(|schema| schema.is_object())
                                .map_or_else(|| json!({}), Value::take),
                        });
                        Some((name, described))
                    })
                    .collect();
                Ok(connection)
            }
            // Dropping the connection kills the server that failed to start.
            (Err(cause), Transport::Stdio(stdio)) => Err(stdio.startup_failure(&cause).await),
            (Err(cause), Transport::Http(_)) => Err(cause),
        }
    }

    /// Whether this connection is the one `settings` would open, and still
    /// able to answer.
    pub(super) fn serves(&self, settings: &Settings) -> bool {
        self.settings == *settings
            && !match &self.transport {
                Transport::Stdio(stdio) => stdio.is_closed(),
                Transport::Http(http) => http.is_closed(),
            }
    }

    /// Whether the server's `enabledTools` / `disabledTools` let `tool` be
    /// seen and called.
    fn allows(&self, tool: &str) -> bool {
        let names = |policy: &str| {
            self.settings
                .config
                .get(policy)
                .and_then(Value::as_array)
                .map(|names| names.iter().any(|name| name.as_str() == Some(tool)))
        };
        names("enabledTools").unwrap_or(true) && !names("disabledTools").unwrap_or(false)
    }

    /// The tools policy allows, each `{name, description, inputSchema}`.
    pub(super) fn tools(&self) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|(name, _)| self.allows(name))
            .map(|(_, tool)| tool.clone())
            .collect()
    }

    /// One tool's `{name, description, inputSchema}`.
    ///
    /// # Errors
    ///
    /// The server has no such tool, or policy excludes it.
    pub(super) fn tool(&self, tool: &str) -> Result<&Value, Failure> {
        let described = self.tools.get(tool).ok_or_else(|| {
            Failure::new(
                FailureKind::NotDeclared,
                format!("MCP server '{}' has no tool '{tool}'", self.server),
            )
        })?;
        if !self.allows(tool) {
            return Err(Failure::new(
                FailureKind::Excluded,
                format!("MCP tool '{tool}' is disabled for server '{}'", self.server),
            ));
        }
        Ok(described)
    }

    /// Call `tool` and return its structured content when it has any, else
    /// its text, else its content blocks.
    ///
    /// # Errors
    ///
    /// The tool is unknown or excluded, the request failed, or the server
    /// flagged the result as an error.
    pub(super) async fn call_tool(&self, tool: &str, arguments: &Value) -> Result<Value, Failure> {
        self.tool(tool)?;
        let mut result = self
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
            })
            .filter(|texts| !texts.is_empty())
            .map(|texts| texts.join("\n"));
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(Failure::new(
                FailureKind::Tool,
                text.unwrap_or_else(|| "the MCP tool returned an error".to_string()),
            ));
        }
        // An empty structured result is still the result.
        if let Some(structured) = result
            .get_mut("structuredContent")
            .filter(|structured| !structured.is_null())
        {
            return Ok(structured.take());
        }
        if let Some(text) = text {
            return Ok(Value::String(text));
        }
        // Images and embedded resources come back as the server's blocks.
        match result
            .get("content")
            .and_then(Value::as_array)
            .filter(|blocks| !blocks.is_empty())
        {
            Some(blocks) => Ok(Value::Array(blocks.clone())),
            None => Ok(result),
        }
    }

    /// Every entry of a paginated listing (`method`'s `key` array), following
    /// the server's cursors to the end.
    ///
    /// # Errors
    ///
    /// A request failed, or the pagination cannot complete: a malformed or
    /// repeated cursor, or more pages than are followed. A partial listing
    /// is never returned.
    pub(super) async fn list(&self, method: &str, key: &str) -> Result<Vec<Value>, Failure> {
        let incomplete = |reason: &str| {
            Failure::new(
                FailureKind::Protocol,
                format!("MCP server '{}' {reason} for {method}", self.server),
            )
        };
        let mut entries = Vec::new();
        let mut followed = HashSet::new();
        let mut cursor: Option<String> = None;
        for _page in 0..MAX_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
            let mut page = self.request(method, params).await?;
            if let Some(Value::Array(found)) = page.get_mut(key).map(Value::take) {
                entries.extend(found);
            }
            let Some(next) = page.get("nextCursor").filter(|next| !next.is_null()) else {
                return Ok(entries);
            };
            let Some(next) = next.as_str().filter(|next| !next.is_empty()) else {
                return Err(incomplete("returned a malformed pagination cursor"));
            };
            if !followed.insert(next.to_string()) {
                return Err(incomplete("repeated a pagination cursor"));
            }
            cursor = Some(next.to_string());
        }
        Err(incomplete("paginated beyond the pages that are followed"))
    }

    /// Send one request and wait for its reply within the call timeout.
    ///
    /// # Errors
    ///
    /// The server answered with an error, did not answer in time, or could
    /// not be reached.
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value, Failure> {
        match &self.transport {
            Transport::Stdio(stdio) => stdio.request(method, params, self.call_timeout).await,
            Transport::Http(http) => http.request(method, params, self.call_timeout).await,
        }
    }

    /// Close the connection: a stdio server is ended and reaped, an HTTP
    /// session released.
    pub(super) async fn shutdown(self) {
        match self.transport {
            Transport::Stdio(stdio) => stdio.shutdown().await,
            Transport::Http(http) => http.shutdown().await,
        }
    }
}
