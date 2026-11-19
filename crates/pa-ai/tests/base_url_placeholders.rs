use pa_ai::responses_transport::{discover_endpoints, DiscoveryTarget, DiscoveryWire};
use pa_ai::types::{SimpleStreamOptions, StreamOptions};
use pa_ai::utils::prewarm::NetworkMode;
use pa_types::ai::{Context, Model, StopReason};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The Cloudflare placeholder shape the catalog's base URLs carry, under a loopback origin.
const PLACEHOLDER_PATH: &str = "/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/gw";

/// Accept one request on a loopback listener, answer 400, and return its request line.
async fn capture_request_line() -> (std::net::SocketAddr, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        while !request.windows(4).any(|b| b == b"\r\n\r\n") {
            let count = socket.read(&mut buf).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buf[..count]);
        }
        socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .await
            .unwrap();
        String::from_utf8(request)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string()
    });
    (address, server)
}

async fn settled(server: tokio::task::JoinHandle<String>) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
}

async fn complete(api: &str, base_url: String) -> pa_ai::types::AssistantMessage {
    let model: Model = serde_json::from_value(json!({
        "id":"test-model", "name":"Test", "provider":"cloudflare-ai-gateway", "api":api,
        "baseUrl":base_url, "reasoning":false, "input":["text"],
        "contextWindow":10000, "maxTokens":100,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
    }))
    .unwrap();
    let options = SimpleStreamOptions::from_base(StreamOptions {
        api_key: Some("synthetic-only".into()),
        timeout_ms: Some(5_000),
        ..Default::default()
    });
    pa_ai::complete_simple(&model, &Context::default(), Some(options))
        .await
        .unwrap()
}

/// The catalog's Cloudflare base URLs carry `{CLOUDFLARE_ACCOUNT_ID}`/`{CLOUDFLARE_GATEWAY_ID}`
/// placeholders: every transport, and the background capability probe, sends to the path the
/// environment fills in, and a missing variable fails the call by name before anything reaches
/// the network.
#[tokio::test]
async fn cloudflare_placeholders_resolve_from_the_environment_on_every_outbound_path() {
    std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acct123");
    std::env::set_var("CLOUDFLARE_GATEWAY_ID", "gw-1");
    for (api, endpoint) in [
        ("openai-completions", "/chat/completions"),
        ("openai-responses", "/responses"),
        ("anthropic-messages", "/v1/messages"),
    ] {
        let (address, server) = capture_request_line().await;
        complete(api, format!("http://{address}{PLACEHOLDER_PATH}")).await;
        assert_eq!(
            settled(server).await,
            format!("POST /v1/acct123/gw-1/gw{endpoint} HTTP/1.1"),
            "{api}"
        );
    }

    let (address, server) = capture_request_line().await;
    discover_endpoints(
        vec![DiscoveryTarget {
            base_url: format!("http://{address}{PLACEHOLDER_PATH}"),
            model_id: "m".to_string(),
            wire: DiscoveryWire::ChatCompletions,
            headers: vec![],
        }],
        NetworkMode::Online,
    );
    assert_eq!(
        settled(server).await,
        "POST /v1/acct123/gw-1/gw/responses HTTP/1.1"
    );

    std::env::remove_var("CLOUDFLARE_GATEWAY_ID");
    // Nothing listens on the discard port: a request that escaped would fail as a connection
    // error instead of naming the variable.
    let message = complete(
        "openai-completions",
        format!("http://127.0.0.1:9{PLACEHOLDER_PATH}"),
    )
    .await;
    std::env::remove_var("CLOUDFLARE_ACCOUNT_ID");
    assert_eq!(
        (message.stop_reason, message.error_message),
        (
            StopReason::Error,
            Some(
                "cloudflare-ai-gateway needs the CLOUDFLARE_GATEWAY_ID environment variable \
                 to fill its base URL"
                    .to_string()
            )
        )
    );
}
