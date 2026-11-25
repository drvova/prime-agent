use std::fmt::Write as _;
use std::net::SocketAddr;

use hmac::{Hmac, Mac};
use pa_types::event_log::{Topic, Trust};
use serde_json::json;
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{WebhookOptions, WebhookSource};
use crate::test_support::{context, PATIENCE};
use crate::{source_event, Source, SourceHandle};

const SECRET: &str = "signing-key";

fn options(options: serde_json::Value) -> WebhookOptions {
    serde_json::from_value(options).expect("valid webhook options")
}

fn signature(body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn address(hook: &SourceHandle) -> SocketAddr {
    hook.local_addr().expect("a webhook's bound address")
}

/// Sends a request (`body: None` sends the headers alone) and returns the
/// answer's status.
async fn request(
    address: SocketAddr,
    head: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> u16 {
    let exchange = async {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let mut text = format!("{head} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        for (name, value) in headers {
            write!(text, "{name}: {value}\r\n").unwrap();
        }
        if let Some(body) = body {
            write!(text, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        } else {
            text.push_str("\r\n");
        }
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        answer[9..12].parse().unwrap()
    };
    tokio::time::timeout(PATIENCE, exchange)
        .await
        .expect("an answer within the test's patience")
}

#[tokio::test]
async fn authenticated_bodies_are_published_untrusted() {
    let (context, mut published) = context(None);
    let hook = WebhookSource
        .start(
            "inbox",
            options(json!({ "path": "/hook", "token": "t0ken", "hmac_secret": SECRET })),
            context,
        )
        .unwrap();
    let json_body = r#"{"action":"opened"}"#;
    let status = request(
        address(&hook),
        "POST /hook",
        &[
            ("Authorization", "Bearer t0ken"),
            ("X-Signature-256", &signature(json_body)),
            ("Content-Type", "application/json"),
            ("X-GitHub-Event", "issues"),
            ("Cookie", "session=private"),
        ],
        Some(json_body),
    )
    .await;
    assert_eq!(status, 202);
    let topic = Topic::new("source/inbox/received").unwrap();
    assert_eq!(
        published.next().await,
        source_event(
            topic.clone(),
            "inbox",
            json!({
                "body": { "action": "opened" },
                "headers": { "content-type": "application/json", "x-github-event": "issues" },
            }),
            Trust::Untrusted,
        )
    );

    let text_body = "plain words";
    let status = request(
        address(&hook),
        "POST /hook",
        &[
            ("Authorization", "Bearer t0ken"),
            ("X-Signature-256", &signature(text_body)),
        ],
        Some(text_body),
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(
        published.next().await,
        source_event(
            topic,
            "inbox",
            json!({ "body": "plain words" }),
            Trust::Untrusted
        )
    );
    tokio::time::timeout(PATIENCE, hook.stop())
        .await
        .expect("a prompt stop")
        .unwrap();
}

#[tokio::test]
async fn every_configured_check_must_pass() {
    let (context, mut published) = context(None);
    let hook = WebhookSource
        .start(
            "inbox",
            options(json!({ "token": "t0ken", "hmac_secret": SECRET })),
            context,
        )
        .unwrap();
    let body = "{}";
    let signed = signature(body);
    let attempts: [&[(&str, &str)]; 3] = [
        &[("X-Signature-256", &signed)],
        &[
            ("Authorization", "Bearer wrong"),
            ("X-Signature-256", &signed),
        ],
        &[
            ("Authorization", "Bearer t0ken"),
            ("X-Signature-256", "sha256=00"),
        ],
    ];
    for headers in attempts {
        assert_eq!(
            request(address(&hook), "POST /", headers, Some(body)).await,
            401,
            "{headers:?}"
        );
    }
    assert_eq!(published.drain(), Vec::new());
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused_before_it_is_read() {
    let (context, mut published) = context(None);
    let hook = WebhookSource
        .start("inbox", options(json!({ "token": "t0ken" })), context)
        .unwrap();
    // Only the headers are sent: the answer comes without the body.
    let over_a_mebibyte = (1024 * 1024 + 1).to_string();
    let status = request(
        address(&hook),
        "POST /",
        &[
            ("Authorization", "Bearer t0ken"),
            ("Content-Length", &over_a_mebibyte),
        ],
        None,
    )
    .await;
    assert_eq!(status, 413);
    assert_eq!(published.drain(), Vec::new());
}

#[tokio::test]
async fn a_remote_bind_needs_allow_remote() {
    let (context, _published) = context(None);
    let refused = WebhookSource.start(
        "inbox",
        options(json!({ "bind": "0.0.0.0:0", "token": "t0ken" })),
        context.clone(),
    );
    assert!(refused.is_err());
    let allowed = WebhookSource
        .start(
            "inbox",
            options(json!({ "bind": "0.0.0.0:0", "token": "t0ken", "allow_remote": true })),
            context,
        )
        .unwrap();
    allowed.stop().await.unwrap();
}

#[tokio::test]
async fn a_hook_without_auth_is_refused() {
    let (context, _published) = context(None);
    let refused = WebhookSource.start("inbox", options(json!({ "token": "" })), context);
    assert!(refused.is_err());
}
