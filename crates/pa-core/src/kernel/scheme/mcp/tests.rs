//! The MCP client through a real session kernel, against a real stdio
//! server process (`stdio_fixture.sh`) and a local HTTP endpoint.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use steel::steel_vm::engine::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::tools::ipython::{
    ExecuteStatus, IpythonKernelProvisioner, KernelExecuteOptions, KernelExecutor,
};
use crate::tools::tool_definition::AbortSignal;

const NO_OPTIONS: KernelExecuteOptions<'static> = KernelExecuteOptions {
    signal: None,
    timeout_ms: None,
    on_stream: None,
    on_late_sent_agent_message: None,
};

/// The stdio fixture written into a temporary directory, and the log it
/// appends one line to per message it handles.
#[cfg(unix)]
struct StdioFixture {
    dir: tempfile::TempDir,
}

#[cfg(unix)]
impl StdioFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("server.sh"),
            include_str!("stdio_fixture.sh"),
        )
        .unwrap();
        StdioFixture { dir }
    }

    fn log_path(&self, server: &str) -> std::path::PathBuf {
        self.dir.path().join(format!("{server}.log"))
    }

    /// The `mcp.config` entry that runs the fixture as `server`, with
    /// `extra` fields and fixture arguments.
    fn config(&self, server: &str, extra: &Value, mode: &[&str]) -> Value {
        let mut args = vec![
            self.dir.path().join("server.sh").display().to_string(),
            self.log_path(server).display().to_string(),
        ];
        args.extend(mode.iter().map(ToString::to_string));
        let mut config = json!({"type": "stdio", "command": "sh", "args": args});
        config
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        config
    }

    /// What the fixture running as `server` has handled so far.
    fn log(&self, server: &str) -> Vec<String> {
        std::fs::read_to_string(self.log_path(server))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// Handlers whose `mcp.config` answers each server of `servers` (an object
/// by name) with its entry, and any other name as undeclared.
fn configured(servers: Value) -> HostRequestHandlers {
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "mcp.config",
        host_handler(move |payload: HostRequestPayload| {
            let server = payload.data["server"].as_str().unwrap_or_default();
            let config = servers.get(server).cloned().unwrap_or_else(|| json!({}));
            async move { Ok(config) }
        }),
    );
    handlers
}

/// The value of each expression as JSON, or the error it raised.
async fn values(kernel: &dyn KernelExecutor, expressions: &[&str]) -> Vec<Result<Value, String>> {
    let mut values = Vec::new();
    for expression in expressions {
        let cell = format!("(display (value->jsexpr-string {expression}))");
        let result = kernel.execute(&cell, NO_OPTIONS).await.unwrap();
        values.push(match result.error {
            None => Ok(serde_json::from_str(&result.stdout).unwrap()),
            Some(error) => Err(error.evalue),
        });
    }
    values
}

/// The process `pid` names is gone within the wait.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn exited(pid: &Value) -> bool {
    let pid = pid.as_str().unwrap().parse().unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        crate::platform::process::wait_for_exit(pid),
    )
    .await
    .is_ok_and(|waited| waited.is_ok())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_stdio_server_is_opened_once_and_its_tools_resources_and_prompts_answer() {
    let fixture = StdioFixture::new();
    let handlers = configured(json!({
        "files": fixture.config("files", &json!({"disabledTools": ["secret"]}), &[]),
    }));
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            r#"(mcp-list-tools "files")"#,
            r#"(mcp-describe-tool "files" "pid")"#,
            r#"(mcp-call-tool "files" "echo" (hash 'message "hello"))"#,
            r#"(mcp-call-tool "files" "shape")"#,
            r#"(mcp-call-tool "files" "ask")"#,
            r#"(equal? (mcp-call-tool "files" "pid") (mcp-call-tool "files" "pid"))"#,
            r#"(mcp-list-resources "files")"#,
            r#"(mcp-list-resource-templates "files")"#,
            r#"(mcp-read-resource "files" "memo://one")"#,
            r#"(mcp-list-prompts "files")"#,
            r#"(mcp-get-prompt "files" "greet" (hash 'who "Ada"))"#,
        ],
    )
    .await;
    let echo = json!({
        "name": "echo",
        "description": "Echo a message",
        "inputSchema": {
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"],
        },
    });
    let pid = json!({"name": "pid", "description": "The server process id", "inputSchema": {}});
    let bare = |name: &str| json!({"name": name, "description": "", "inputSchema": {}});
    assert_eq!(
        (returned, fixture.log("files")),
        (
            vec![
                Ok(json!([
                    echo,
                    pid,
                    bare("fail"),
                    bare("hang"),
                    bare("shape"),
                    bare("ask")
                ])),
                Ok(pid),
                Ok(json!("hello")),
                Ok(json!({"rows": 2, "tags": ["a", "b"]})),
                Ok(json!("answered")),
                Ok(json!(true)),
                Ok(json!([{"uri": "memo://one", "name": "one"}])),
                Ok(json!([{"uriTemplate": "memo://{name}", "name": "memo"}])),
                Ok(json!({"contents": [{"uri": "memo://one", "text": "the first memo"}]})),
                Ok(json!([{"name": "greet", "arguments": [{"name": "who", "required": true}]}])),
                Ok(json!({"messages": [
                    {"role": "user", "content": {"type": "text", "text": "Greet Ada"}},
                ]})),
            ],
            [
                "initialize",
                "initialized",
                "tools/list",
                "tools/list",
                "call echo",
                "call shape",
                "call ask",
                "call pid",
                "call pid",
                "resources/list",
                "resources/templates/list",
                "resources/read",
                "prompts/list",
                "prompts/get",
            ]
            .map(str::to_string)
            .to_vec()
        )
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_error_a_server_error_and_a_refused_call_raise() {
    let fixture = StdioFixture::new();
    let handlers = configured(json!({
        "files": fixture.config("files", &json!({"disabledTools": ["secret"]}), &[]),
        "off": fixture.config("off", &json!({"enabled": false}), &[]),
    }));
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            r#"(mcp-call-tool "files" "fail")"#,
            r#"(mcp-read-resource "files" "memo://missing")"#,
            r#"(mcp-call-tool "files" "secret")"#,
            r#"(mcp-call-tool "files" "absent")"#,
            r#"(mcp-call-tool "files" "echo" 5)"#,
            r#"(mcp-list-tools "ghost")"#,
            r#"(mcp-list-tools "off")"#,
        ],
    )
    .await;
    assert_eq!(
        returned,
        [
            "mcp-call-tool: the tool refused",
            "mcp-read-resource: the MCP server answered with error -32002: Resource not found",
            "mcp-call-tool: MCP tool 'secret' is disabled for server 'files'",
            "mcp-call-tool: MCP server 'files' has no tool 'absent'",
            "mcp-call-tool: arguments must be a hash",
            "mcp-list-tools: MCP server 'ghost' is not declared in the user's settings",
            "mcp-list-tools: MCP server 'off' is disabled",
        ]
        .map(|reason| Err(format!("Error: Generic: {reason}")))
        .to_vec()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_call_the_server_never_answers_times_out_and_the_server_stays_usable() {
    let fixture = StdioFixture::new();
    let handlers = configured(json!({
        "slow": fixture.config("slow", &json!({"callTimeoutMs": 1500}), &[]),
    }));
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            r#"(mcp-call-tool "slow" "hang")"#,
            r#"(mcp-call-tool "slow" "echo" (hash 'message "still here"))"#,
        ],
    )
    .await;
    // The echo is answered after the cancellation the timeout sent: the
    // server reads its messages in order.
    assert_eq!(
        (returned, fixture.log("slow")),
        (
            vec![
                Err(
                    "Error: Generic: mcp-call-tool: MCP server 'slow' did not answer tools/call \
                     within 1500 ms"
                        .to_string()
                ),
                Ok(json!("still here")),
            ],
            [
                "initialize",
                "initialized",
                "tools/list",
                "tools/list",
                "call hang",
                "cancelled 4",
                "call echo",
            ]
            .map(str::to_string)
            .to_vec()
        )
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_stopped_mid_call_returns_at_once_and_kill_reaps_the_server() {
    let fixture = StdioFixture::new();
    let handlers = configured(json!({ "files": fixture.config("files", &json!({}), &[]) }));
    let provisioner = SchemeKernelProvisioner::new(handlers, "/");
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let pid = r#"(mcp-call-tool "files" "pid")"#;
    let before = values(kernel.as_ref(), &[pid]).await;
    let signal = AbortSignal::new();
    let stopped = kernel.execute(
        r#"(mcp-call-tool "files" "hang")"#,
        KernelExecuteOptions {
            signal: Some(signal.clone()),
            ..NO_OPTIONS
        },
    );
    let stop = async {
        // The call is in flight once the server has logged it.
        while fixture.log("files").last().map(String::as_str) != Some("call hang") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        signal.cancel();
    };
    // Far inside the call's own 60 s timeout.
    let (stopped, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(stopped, stop)
    })
    .await
    .unwrap();
    let after = values(kernel.as_ref(), &[pid]).await;
    provisioner.kill().await;
    let reaped = exited(before[0].as_ref().unwrap()).await;
    assert_eq!(
        (stopped.unwrap().status, &after, reaped),
        (ExecuteStatus::Aborted, &before, true)
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_dies_while_starting_reports_its_redacted_stderr() {
    let fixture = StdioFixture::new();
    let handlers = configured(json!({
        "broken": fixture.config("broken", &json!({}), &["crash"]),
    }));
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    assert_eq!(
        values(kernel.as_ref(), &[r#"(mcp-list-tools "broken")"#]).await,
        vec![Err(
            "Error: Generic: mcp-list-tools: MCP stdio server 'broken' failed during startup \
             (MCP server 'broken' closed its connection). Stderr tail:\n\
             warming up\n\
             fixture: cannot open [REDACTED]"
                .to_string()
        )]
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_never_finishes_starting_is_killed_at_its_startup_timeout() {
    let fixture = StdioFixture::new();
    let handlers = configured(json!({
        "quiet": fixture.config("quiet", &json!({"startupTimeoutMs": 3000}), &["mute"]),
    }));
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let returned = values(kernel.as_ref(), &[r#"(mcp-list-tools "quiet")"#]).await;
    // The mute fixture logs its process id as it starts.
    let killed = exited(&json!(fixture.log("quiet").concat())).await;
    assert_eq!(
        (returned, killed),
        (
            vec![Err(
                "Error: Generic: mcp-list-tools: MCP stdio server 'quiet' failed during startup \
                 (MCP server 'quiet' did not finish starting within 3000 ms)."
                    .to_string()
            )],
            true
        )
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_login_reports_the_hosts_outcome_and_a_reload_reopens_the_server() {
    let fixture = StdioFixture::new();
    let mut handlers = configured(json!({ "files": fixture.config("files", &json!({}), &[]) }));
    handlers.register(
        "mcp.begin_login",
        host_handler(|payload: HostRequestPayload| async move {
            match payload.data["server"].as_str() {
                Some("files") => Ok(json!({"status": "logged_in"})),
                Some("headless") => {
                    Ok(json!({"status": "unavailable", "message": "run /mcp login headless"}))
                }
                Some(_) | None => Err(anyhow::anyhow!("https://login.example/?code=secret failed")),
            }
        }),
    );
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let pid = r#"(mcp-call-tool "files" "pid")"#;
    let mut returned = values(
        kernel.as_ref(),
        &[
            pid,
            r#"(mcp-login "files")"#,
            pid,
            r#"(begin (mcp-reload) "reloaded")"#,
            pid,
            r#"(mcp-login "headless")"#,
            r#"(mcp-login "denied")"#,
        ],
    )
    .await;
    let pids = [returned.remove(4), returned.remove(2), returned.remove(0)].map(Result::unwrap);
    let closed = (exited(&pids[2]).await, exited(&pids[1]).await);
    assert_eq!(
        (returned, closed, pids[0] != pids[1] && pids[1] != pids[2]),
        (
            vec![
                Ok(
                    json!({"server": "files", "status": "logged_in", "message": "logged in to files"})
                ),
                Ok(json!("reloaded")),
                Ok(
                    json!({"server": "headless", "status": "unavailable", "message": "run /mcp login headless"})
                ),
                Ok(json!({
                    "server": "denied",
                    "status": "failed",
                    "message": "the login did not complete; ask the user to run /mcp login denied in Prime Agent",
                })),
            ],
            (true, true),
            true
        )
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_inventory_is_the_hosts_without_secrets_and_a_search_reports_its_scope() {
    let fixture = StdioFixture::new();
    let mut handlers = configured(json!({ "files": fixture.config("files", &json!({}), &[]) }));
    let received = Arc::new(Mutex::new(Vec::new()));
    for (request_type, reply) in [
        (
            "mcp.list_connections",
            json!({"connections": [
                {"connectionId": "files", "status": "connected", "accessToken": "s3cret"},
                {"connectionId": "ghost", "status": "connected"},
                {"connectionId": "idle", "status": "not_connected"},
            ]}),
        ),
        (
            "mcp.list_plugins",
            json!({
                "plugins": [{"id": "notion", "auth": {"apiKey": "k", "kind": "oauth"}}],
                "nextCursor": "page-2",
            }),
        ),
        (
            "mcp.search_plugins",
            json!({"plugins": [{"id": "notion", "client_secret": "k"}], "nextCursor": "page-2"}),
        ),
    ] {
        let received = Arc::clone(&received);
        handlers.register(
            request_type,
            host_handler(move |payload: HostRequestPayload| {
                received.lock().unwrap().push(payload.data);
                let reply = reply.clone();
                async move { Ok(reply) }
            }),
        );
    }
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            "(mcp-list-connections)",
            r#"(mcp-list-plugins #:connection-status "not_connected" #:limit 5.0 #:cursor "page-1")"#,
            r#"(mcp-search-plugins " notion ")"#,
            r#"(mcp-search-tools "ECHO")"#,
            r#"(mcp-search-tools "a" #:connection-id "files" #:limit 2)"#,
            "(mcp-list-plugins #:limit 500)",
        ],
    )
    .await;
    let echo = json!({"connectionId": "files", "name": "echo", "description": "Echo a message"});
    assert_eq!(
        (returned, received.lock().unwrap().clone()),
        (
            vec![
                Ok(json!([
                    {"connectionId": "files", "status": "connected"},
                    {"connectionId": "ghost", "status": "connected"},
                    {"connectionId": "idle", "status": "not_connected"},
                ])),
                Ok(
                    json!({"plugins": [{"id": "notion", "auth": {"kind": "oauth"}}], "nextCursor": "page-2"})
                ),
                Ok(json!({"plugins": [{"id": "notion"}], "nextCursor": "page-2"})),
                Ok(json!({
                    "truncated": false,
                    "tools": [echo],
                    "searched": ["files"],
                    "unavailable": [
                        {"connectionId": "ghost", "error": "NotDeclared: the connection is not declared"},
                    ],
                })),
                Ok(json!({
                    "truncated": true,
                    "tools": [
                        echo,
                        {"connectionId": "files", "name": "fail", "description": ""},
                    ],
                    "searched": ["files"],
                    "unavailable": [],
                })),
                Err(
                    "Error: Generic: mcp-list-plugins: limit must be an integer between 1 and 200"
                        .to_string()
                ),
            ],
            vec![
                json!({"type": "mcp.list_connections"}),
                json!({
                    "limit": 5,
                    "connectionStatus": "not_connected",
                    "cursor": "page-1",
                    "type": "mcp.list_plugins",
                }),
                json!({"query": "notion", "limit": 10, "type": "mcp.search_plugins"}),
                json!({"type": "mcp.list_connections"}),
            ]
        )
    );
}

/// The background settle and a cell reach one connection: the listing
/// opens the server, and the cell that follows reuses it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_background_listing_opens_each_server_once_and_reports_a_failing_one() {
    let fixture = StdioFixture::new();
    let handlers = configured(json!({ "files": fixture.config("files", &json!({}), &[]) }));
    let provisioner = SchemeKernelProvisioner::new(handlers, "/");
    let settled = provisioner
        .mcp_tool_listing(&["files".to_string(), "ghost".to_string()], 20_000)
        .await;
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let from_a_cell = values(
        kernel.as_ref(),
        &[r#"(mcp-status (list "files" "ghost") #:timeout-ms 20000)"#],
    )
    .await;
    let listing = json!([
        {
            "server": "files",
            "tools": [
                {"name": "echo", "description": "Echo a message"},
                {"name": "pid", "description": "The server process id"},
                {"name": "fail", "description": ""},
                {"name": "hang", "description": ""},
                {"name": "shape", "description": ""},
                {"name": "ask", "description": ""},
                {"name": "secret", "description": ""},
            ],
            "error": null,
        },
        {
            "server": "ghost",
            "tools": null,
            "error": "NotDeclared: MCP server 'ghost' is not declared in the user's settings",
        },
    ]);
    assert_eq!(
        (Value::Array(settled), from_a_cell, fixture.log("files")),
        (
            listing.clone(),
            vec![Ok(listing)],
            ["initialize", "initialized", "tools/list", "tools/list"]
                .map(str::to_string)
                .to_vec()
        )
    );
}

/// What the HTTP fixture saw of one request.
fn seen(verb: &str, method: Option<&str>, session: Option<&str>) -> Value {
    json!({
        "verb": verb,
        "method": method,
        "authorization": "Bearer fresh",
        "key": "k-1",
        "session": session,
    })
}

/// A streamable-HTTP MCP endpoint on a local port: `initialize` assigns a
/// session, `tools/list` answers over an event stream that first pings the
/// client, the `hang` tool never answers. It records every request, and
/// signals each cancellation notice.
async fn http_fixture() -> (String, Arc<Mutex<Vec<Value>>>, Arc<tokio::sync::Notify>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let (recorded, noticed) = (Arc::clone(&requests), Arc::clone(&cancelled));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (recorded, noticed) = (Arc::clone(&recorded), Arc::clone(&noticed));
            tokio::spawn(async move {
                let mut received = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let text = String::from_utf8_lossy(&received).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length: ")?
                                    .parse()
                                    .ok()
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break (head.to_string(), body.to_string());
                        }
                    }
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => received.extend_from_slice(&chunk[..read]),
                    }
                };
                let header = |name: &str| {
                    head.lines().find_map(|line| {
                        let (key, value) = line.split_once(": ")?;
                        (key.to_lowercase() == name).then(|| value.to_string())
                    })
                };
                let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let method = message["method"].as_str();
                recorded.lock().unwrap().push(json!({
                    "verb": head.split(' ').next(),
                    "method": method,
                    "authorization": header("authorization"),
                    "key": header("x-api-key"),
                    "session": header("mcp-session-id"),
                }));
                let reply = |result: Value| {
                    json!({"jsonrpc": "2.0", "id": message["id"], "result": result}).to_string()
                };
                let plain = |status: &str, extra: &str, body: String| {
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let response = match (method, message["params"]["name"].as_str()) {
                    (Some("initialize"), _) => plain(
                        "200 OK",
                        "Mcp-Session-Id: s-1\r\n",
                        reply(json!({"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}})),
                    ),
                    (Some("tools/list"), _) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
                         : keep-alive\n\n\
                         event: message\ndata: {}\n\n\
                         event: message\r\ndata: {}\r\n\r\n",
                        json!({"jsonrpc": "2.0", "id": 77, "method": "ping"}),
                        reply(json!({"tools": [{"name": "echo"}, {"name": "hang"}]})),
                    ),
                    (Some("tools/call"), Some("echo")) => plain(
                        "200 OK",
                        "",
                        reply(json!({"content": [
                            {"type": "text", "text": message["params"]["arguments"]["message"]},
                        ]})),
                    ),
                    (Some("tools/call"), _) => std::future::pending().await,
                    (Some("notifications/cancelled"), _) => {
                        noticed.notify_one();
                        plain("202 Accepted", "", String::new())
                    }
                    (Some(_) | None, _) => plain("202 Accepted", "", String::new()),
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            });
        }
    });
    (url, requests, cancelled)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http_server_is_sent_the_refreshed_bearer_its_session_and_a_cancellation() {
    let (url, requests, cancelled) = http_fixture().await;
    let agent_dir = tempfile::tempdir().unwrap();
    let auth = agent_dir.path().join("auth.json");
    let credential = |access: &str, expires: u64| {
        json!({"mcp:web": {"type": "oauth", "access": access, "expires": expires, "endpoint": url}})
            .to_string()
    };
    std::fs::write(&auth, credential("stale", 1)).unwrap();
    let mut handlers = configured(json!({
        "web": {
            "type": "http",
            "url": url,
            "oauth": true,
            "headers": {"X-Api-Key": "k-1", "Authorization": "Bearer configured"},
            "callTimeoutMs": 2000,
        },
    }));
    let fresh = credential("fresh", 4_102_444_800_000);
    handlers.register(
        "mcp.refresh",
        host_handler(move |_payload: HostRequestPayload| {
            let written = std::fs::write(&auth, &fresh);
            async move { Ok(json!({ "refreshed": written.is_ok() })) }
        }),
    );
    let provisioner = SchemeKernelProvisioner::new(handlers, "/").with_agent_dir(agent_dir.path());
    let kernel = provisioner.ensure(None, None).await.unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            r#"(mcp-list-tools "web")"#,
            r#"(mcp-call-tool "web" "echo" (hash 'message "over http"))"#,
            r#"(mcp-call-tool "web" "hang")"#,
        ],
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), cancelled.notified())
        .await
        .unwrap();
    provisioner.kill().await;
    let bare = |name: &str| json!({"name": name, "description": "", "inputSchema": {}});
    assert_eq!(
        (returned, requests.lock().unwrap().clone()),
        (
            vec![
                Ok(json!([bare("echo"), bare("hang")])),
                Ok(json!("over http")),
                Err(
                    "Error: Generic: mcp-call-tool: MCP server 'web' did not answer tools/call \
                     within 2000 ms"
                        .to_string()
                ),
            ],
            vec![
                seen("POST", Some("initialize"), None),
                seen("POST", Some("notifications/initialized"), Some("s-1")),
                seen("POST", Some("tools/list"), Some("s-1")),
                // The answer to the server's ping.
                seen("POST", None, Some("s-1")),
                seen("POST", Some("tools/call"), Some("s-1")),
                seen("POST", Some("tools/call"), Some("s-1")),
                seen("POST", Some("notifications/cancelled"), Some("s-1")),
                seen("DELETE", None, Some("s-1")),
            ]
        )
    );
}

/// A credential is sent only to the endpoint it was stored for, and a server
/// that needs one and has none is not contacted at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_a_credential_bound_to_its_endpoint_is_not_contacted() {
    let (url, requests, _cancelled) = http_fixture().await;
    let agent_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        agent_dir.path().join("auth.json"),
        json!({
            "mcp:moved": {"type": "oauth", "access": "t", "expires": 4_102_444_800_000_u64, "endpoint": "https://old.example/mcp"},
            "mcp:pasted": {"type": "mcp_static_token", "bearer": " ", "endpoint": url},
        })
        .to_string(),
    )
    .unwrap();
    let handlers = configured(json!({
        "moved": {"type": "http", "url": url, "oauth": true},
        "pasted": {"type": "http", "url": url, "credentialSource": "static-token"},
    }));
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .with_agent_dir(agent_dir.path())
        .ensure(None, None)
        .await
        .unwrap();
    let returned = values(
        kernel.as_ref(),
        &[
            r#"(mcp-list-tools "moved")"#,
            r#"(mcp-list-tools "pasted")"#,
        ],
    )
    .await;
    let unavailable = |server: &str| {
        Err(format!(
            "Error: Generic: mcp-list-tools: MCP credentials for '{server}' are not available. \
             Ask the user to connect it (/plugins or /mcp login {server}); do not ask them to \
             set environment variables."
        ))
    };
    assert_eq!(
        (returned, requests.lock().unwrap().clone()),
        (
            vec![unavailable("moved"), unavailable("pasted")],
            Vec::new()
        )
    );
}

/// Steel's prelude binds hundreds of names; a definition here that reused
/// one would replace a builtin for every cell.
#[test]
fn no_mcp_name_replaces_a_steel_builtin() {
    let mut engine = Engine::new();
    let taken: Vec<&str> = super::API
        .lines()
        .filter_map(|line| line.strip_prefix("(define "))
        .filter_map(|rest| rest.trim_start_matches('(').split([' ', ')']).next())
        .filter(|name| engine.run((*name).to_string()).is_ok())
        .collect();
    assert_eq!(taken, Vec::<&str>::new());
}
