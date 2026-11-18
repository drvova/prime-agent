//! The `get_usage` halves over fixture data: the spend aggregation over session files, and the
//! quota cards against a loopback endpoint where one provider hangs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pa_types::provider_usage::{
    DailySpend, ModelSpend, ObservedRateLimit, ProviderQuota, QuotaReport, QuotaSource, QuotaState,
    QuotaUnit, QuotaWindow, SpendHistory, SpendTotals,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::history::{spend_history, HISTORY_DAYS};
use super::quotas::{quota_report, CachePolicy, QuotaFetchConfig};

const DAY_MS: i64 = 86_400_000;

/// Local noon of the day holding `base`: the fixture rows sit hours from it, never across a
/// local midnight, whatever the test machine's zone.
fn local_noon(base: i64) -> i64 {
    let civil = pa_types::platform::local_civil(base).expect("a unix zone binding");
    base - (i64::from(civil.hour) - 12) * 3_600_000 - i64::from(civil.minute) * 60_000
}

fn local_date(epoch_ms: i64) -> String {
    let civil = pa_types::platform::local_civil(epoch_ms).unwrap();
    format!("{:04}-{:02}-{:02}", civil.year, civil.month, civil.day)
}

fn assistant(id: &str, at_ms: i64, model: &str, input: u64, output: u64, cost: f64) -> String {
    json!({
        "type": "message", "id": id, "parentId": null,
        "timestamp": crate::util::iso_from_unix_ms(at_ms as u64),
        "message": {
            "role": "assistant", "provider": "anthropic", "model": model,
            "stopReason": "stop", "timestamp": at_ms,
            "usage": {
                "input": input, "output": output, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": input + output,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": cost },
            },
        },
    })
    .to_string()
}

fn totals(input: u64, output: u64, cost: f64, requests: u64) -> SpendTotals {
    SpendTotals {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost,
        requests,
    }
}

/// Today, ten days back, and forty days back land in their spans; a rewritten assistant row
/// counts once at its last write; a child-attribution row adds nothing (the child's own file
/// bills it); a compaction's summarizer call bills; a 429 surfaces its reset.
#[test]
fn spend_history_aggregates_days_spans_models_and_rate_limits() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("--home-dev-project--");
    std::fs::create_dir_all(&folder).unwrap();
    let noon = local_noon(1_795_003_200_000);
    let now = noon + 3_600_000;
    let parent = [
        json!({"type": "session", "version": 3, "id": "s1", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/home/dev/project"}).to_string(),
        assistant("a1", noon - 2 * 3_600_000, "claude-opus", 100, 10, 0.25),
        // The same row rewritten: the second write replaces the first.
        assistant("a1", noon - 2 * 3_600_000, "claude-opus", 120, 12, 0.5),
        json!({
            "type": "child_usage_attributed", "id": "c1", "targetId": "a1",
            "timestamp": crate::util::iso_from_unix_ms((noon - 3_600_000) as u64),
            "childUsage": {"input": 999, "output": 999, "cost": {"total": 9.0}},
            "aggregateUsage": {"input": 1119, "output": 1011, "cost": {"total": 9.5}},
        })
        .to_string(),
        json!({
            "type": "compaction", "id": "k1", "summary": "…", "firstKeptEntryId": "a1",
            "timestamp": crate::util::iso_from_unix_ms((noon - 10 * DAY_MS) as u64),
            "provider": "anthropic", "modelId": "claude-haiku",
            "usage": {"input": 50, "output": 5, "cost": {"total": 0.125}},
        })
        .to_string(),
        assistant("a2", noon - 40 * DAY_MS, "claude-opus", 1000, 100, 2.0),
        json!({
            "type": "message", "id": "e1",
            "timestamp": crate::util::iso_from_unix_ms((noon - 3_600_000) as u64),
            "message": {
                "role": "assistant", "provider": "openai-codex", "model": "gpt-5", "stopReason": "error",
                "timestamp": noon - 3_600_000,
                "diagnostics": [{"type": "provider_stream_failure", "details": {"kind": "rate_limit", "status": 429, "retryAfterMs": 5_400_000}}],
                "usage": {"input": 0, "output": 0, "cost": {"total": 0}},
            },
        })
        .to_string(),
        "{torn tail".to_string(),
    ];
    std::fs::write(folder.join("parent.jsonl"), parent.join("\n")).unwrap();
    let child = [assistant(
        "b1",
        noon - 3_600_000,
        "claude-sonnet",
        999,
        999,
        9.0,
    )];
    std::fs::write(folder.join("child.jsonl"), child.join("\n")).unwrap();

    let history = spend_history(root.path(), now);

    let today = totals(120 + 999, 12 + 999, 0.5 + 9.0, 2);
    let week = today;
    let month = totals(120 + 999 + 50, 12 + 999 + 5, 0.5 + 9.0 + 0.125, 3);
    let all = totals(
        120 + 999 + 50 + 1000,
        12 + 999 + 5 + 100,
        0.5 + 9.0 + 0.125 + 2.0,
        4,
    );
    let daily = (0..HISTORY_DAYS)
        .rev()
        .map(|back| {
            let at = noon - back * DAY_MS;
            DailySpend {
                date: local_date(at),
                totals: if back == 0 {
                    today
                } else if back == 10 {
                    totals(50, 5, 0.125, 1)
                } else {
                    SpendTotals::default()
                },
            }
        })
        .collect();
    assert_eq!(
        history,
        SpendHistory {
            today,
            last_7_days: week,
            last_30_days: month,
            all_time: all,
            daily,
            models: vec![
                ModelSpend {
                    provider: "anthropic".into(),
                    model: "claude-sonnet".into(),
                    today: totals(999, 999, 9.0, 1),
                    last_7_days: totals(999, 999, 9.0, 1),
                    last_30_days: totals(999, 999, 9.0, 1),
                    all_time: totals(999, 999, 9.0, 1),
                },
                ModelSpend {
                    provider: "anthropic".into(),
                    model: "claude-opus".into(),
                    today: totals(120, 12, 0.5, 1),
                    last_7_days: totals(120, 12, 0.5, 1),
                    last_30_days: totals(120, 12, 0.5, 1),
                    all_time: totals(1120, 112, 2.5, 2),
                },
                ModelSpend {
                    provider: "anthropic".into(),
                    model: "claude-haiku".into(),
                    today: SpendTotals::default(),
                    last_7_days: SpendTotals::default(),
                    last_30_days: totals(50, 5, 0.125, 1),
                    all_time: totals(50, 5, 0.125, 1),
                },
            ],
            rate_limits: vec![ObservedRateLimit {
                provider: "openai-codex".into(),
                observed_at_ms: noon - 3_600_000,
                resets_at_ms: Some(noon - 3_600_000 + 5_400_000),
            }],
            sessions: 2,
        }
    );
}

/// Serve every request by path until the test ends: `/api/oauth/usage` never answers (a hung
/// provider), `/copilot_internal/user` answers its recorded shape.
async fn quota_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut chunk = [0u8; 4096];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => head.extend_from_slice(&chunk[..read]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                if head.starts_with("GET /api/oauth/usage ") {
                    // Hold the connection open without a reply.
                    std::future::pending::<()>().await;
                }
                let body = r#"{"copilot_plan":"business","quota_reset_date":"2026-12-01","quota_snapshots":{"premium_interactions":{"entitlement":300,"remaining":0,"unlimited":false}}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    base
}

/// A hung provider fails only its own card, inside the bound; the others fill in, and a
/// token-exchange sign-in gets no provider card.
#[tokio::test]
async fn a_hung_provider_fails_its_own_card_within_the_bound() {
    let agent_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        agent_dir.path().join("auth.json"),
        json!({
            "anthropic": {"type": "oauth", "access": "oat", "refresh": "r", "expires": 9_999_999_999_999_i64},
            "github-copilot": {"type": "oauth", "access": "copilot-api", "refresh": "gho_x", "expires": 9_999_999_999_999_i64},
            "prime-inference": {"type": "api_key", "key": "pi-key"},
            // A token-exchange sign-in has no usage endpoint: no card, no request.
            "openai-codex": {"type": "token_exchange", "refresh": "rt", "access": "at"},
        })
        .to_string(),
    )
    .unwrap();
    let config = QuotaFetchConfig {
        origin_override: Some(quota_server().await),
        timeout: Duration::from_millis(300),
        http: Arc::new(pa_ai::oauth::ReqwestProviderHttp::new()),
    };
    let started = Instant::now();
    let report = quota_report(
        agent_dir.path().to_path_buf(),
        &config,
        CachePolicy::UseFresh,
        1_795_003_200_000,
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        report,
        QuotaReport {
            providers: vec![
                ProviderQuota {
                    provider: "anthropic".into(),
                    plan: None,
                    source: QuotaSource::Provider,
                    fetched_at_ms: 1_795_003_200_000,
                    state: QuotaState::Failed {
                        error: "the usage endpoint did not answer in time".into(),
                    },
                },
                ProviderQuota {
                    provider: "github-copilot".into(),
                    plan: Some("business".into()),
                    source: QuotaSource::Provider,
                    fetched_at_ms: 1_795_003_200_000,
                    state: QuotaState::Windows {
                        windows: vec![QuotaWindow {
                            label: "Premium requests".into(),
                            used: 300.0,
                            limit: Some(300.0),
                            unit: QuotaUnit::Requests,
                            resets_at_ms: Some(1_796_083_200_000),
                        }],
                    },
                },
            ],
        }
    );
}
