//! Each fetcher against a loopback server serving its endpoint's recorded shape: the parse is
//! the shared type, whole-object, and the request carries the credential where the endpoint
//! reads it. No real network.

use base64::Engine as _;
use pa_types::provider_usage::{QuotaUnit, QuotaWindow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::oauth::ReqwestProviderHttp;

/// Serve one request with `status` and `body`; the handle yields the request head (lowercased).
async fn serve_once(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut chunk = [0u8; 4096];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "the request ended before its headers");
            head.extend_from_slice(&chunk[..read]);
        }
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&head).to_lowercase()
    });
    (base, handle)
}

#[tokio::test]
async fn anthropic_usage_parses_the_fixed_and_model_scoped_windows() {
    let (base, request) = serve_once(200, include_str!("fixtures/anthropic_usage.json")).await;
    let fetched = fetch_anthropic_quota(&ReqwestProviderHttp::new(), &base, "oat-token")
        .await
        .unwrap();
    assert_eq!(
        fetched,
        FetchedQuota {
            plan: None,
            windows: vec![
                QuotaWindow {
                    label: "5-hour".into(),
                    used: 37.0,
                    limit: Some(100.0),
                    unit: QuotaUnit::Percent,
                    resets_at_ms: Some(1_795_028_400_402),
                },
                QuotaWindow {
                    label: "Weekly".into(),
                    used: 62.0,
                    limit: Some(100.0),
                    unit: QuotaUnit::Percent,
                    resets_at_ms: Some(1_795_244_400_402),
                },
                QuotaWindow {
                    label: "Weekly (Opus)".into(),
                    used: 12.0,
                    limit: Some(100.0),
                    unit: QuotaUnit::Percent,
                    resets_at_ms: Some(1_795_244_400_000),
                },
            ],
        }
    );
    let head = request.await.unwrap();
    assert!(head.starts_with("get /api/oauth/usage "), "{head}");
    assert!(head.contains("authorization: bearer oat-token"), "{head}");
    assert!(head.contains("anthropic-beta: oauth-2025-04-20"), "{head}");
}

#[tokio::test]
async fn codex_usage_parses_both_windows_and_resolves_relative_resets() {
    let claims = serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": "acct-123" }
    });
    let token = format!(
        "h.{}.s",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let (base, request) = serve_once(200, include_str!("fixtures/openai_codex_usage.json")).await;
    let now_ms = 1_795_003_200_000;
    let fetched = fetch_openai_codex_quota(&ReqwestProviderHttp::new(), &base, &token, now_ms)
        .await
        .unwrap();
    assert_eq!(
        fetched,
        FetchedQuota {
            plan: Some("plus".into()),
            windows: vec![
                QuotaWindow {
                    label: "5-hour".into(),
                    used: 41.0,
                    limit: Some(100.0),
                    unit: QuotaUnit::Percent,
                    resets_at_ms: Some(1_795_008_600_000),
                },
                QuotaWindow {
                    label: "Weekly".into(),
                    used: 9.0,
                    limit: Some(100.0),
                    unit: QuotaUnit::Percent,
                    resets_at_ms: Some(now_ms + 400_000_000),
                },
            ],
        }
    );
    let head = request.await.unwrap();
    assert!(head.starts_with("get /wham/usage "), "{head}");
    assert!(head.contains("chatgpt-account-id: acct-123"), "{head}");
}

#[tokio::test]
async fn copilot_usage_reads_the_premium_bucket_and_skips_unlimited_ones() {
    let (base, request) = serve_once(200, include_str!("fixtures/github_copilot_user.json")).await;
    let fetched = fetch_github_copilot_quota(&ReqwestProviderHttp::new(), &base, "gho_token")
        .await
        .unwrap();
    assert_eq!(
        fetched,
        FetchedQuota {
            plan: Some("individual".into()),
            windows: vec![QuotaWindow {
                label: "Premium requests".into(),
                used: 88.0,
                limit: Some(300.0),
                unit: QuotaUnit::Requests,
                resets_at_ms: Some(1_796_083_200_000),
            }],
        }
    );
    let head = request.await.unwrap();
    assert!(head.starts_with("get /copilot_internal/user "), "{head}");
    assert!(head.contains("authorization: bearer gho_token"), "{head}");
}

/// A rejected sign-in is the card's error text, never the URL or the body.
#[tokio::test]
async fn a_rejected_sign_in_is_the_cards_error() {
    let (base, _request) = serve_once(401, r#"{"error":"secret detail"}"#).await;
    assert_eq!(
        fetch_anthropic_quota(&ReqwestProviderHttp::new(), &base, "expired").await,
        Err("the provider rejected the sign-in (HTTP 401); run /login".to_string())
    );
}

#[test]
fn iso_timestamps_parse_offsets_fractions_and_bare_dates() {
    assert_eq!(
        parse_iso_ms("2026-11-18T19:00:00.402373+00:00"),
        Some(1_795_028_400_402)
    );
    assert_eq!(
        parse_iso_ms("2026-11-18T21:30:00+02:30"),
        Some(1_795_028_400_000)
    );
    assert_eq!(
        parse_iso_ms("2026-11-18T19:00:00Z"),
        Some(1_795_028_400_000)
    );
    assert_eq!(parse_iso_ms("2026-12-01"), Some(1_796_083_200_000));
    assert_eq!(parse_iso_ms("soon"), None);
}

#[test]
fn enterprise_copilot_sign_ins_use_their_own_api_origin() {
    assert_eq!(github_api_base_url(None), "https://api.github.com");
    assert_eq!(
        github_api_base_url(Some("https://company.ghe.com/")),
        "https://api.company.ghe.com"
    );
}
