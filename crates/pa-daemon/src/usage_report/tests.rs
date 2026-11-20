//! The `get_usage` halves over fixture data: the spend aggregation over session files, and the
//! quota cards against a loopback endpoint where one provider hangs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pa_types::billing::{BillingMode, BillingSplit, CostSource, ModeSpend};
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
        // The fixture rows predate billing modes: every call reads unclassified.
        billing: BillingSplit {
            unclassified: ModeSpend {
                calls: requests,
                cost,
            },
            ..BillingSplit::default()
        },
    }
}

/// Today, ten days back, and forty days back land in their spans; a rewritten assistant row
/// counts once at its last write; a child-attribution row adds nothing (the child's own file
/// bills it); a compaction's summarizer call bills; a 429 surfaces its `x-ratelimit-reset`.
#[test]
fn spend_history_aggregates_days_spans_models_and_rate_limits() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("--home-dev-project--");
    std::fs::create_dir_all(&folder).unwrap();
    let noon = local_noon(1_795_003_200_000);
    let now = noon + 3_600_000;
    // The provider layer's own failure record for a 429 carrying only `x-ratelimit-reset`: the
    // reset the auth resolver's fallback parses is the one the observed card shows.
    let rate_limit_details =
        serde_json::to_value(pa_ai::utils::stream_failure::extract_stream_failure_info(
            &pa_ai::ProviderError::Http(pa_ai::utils::stream_failure::ProviderHttpError {
                message: "429 Too Many Requests".into(),
                status: Some(429),
                body: None,
                headers: [("x-ratelimit-reset".to_string(), "5400".to_string())].into(),
                request_id: None,
                sdk_name: None,
                retry_after_ms: None,
                provider_error_type: None,
            }),
        ))
        .unwrap();
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
                "diagnostics": [{"type": "provider_stream_failure", "details": rate_limit_details}],
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
                    mode: None,
                    today: totals(999, 999, 9.0, 1),
                    last_7_days: totals(999, 999, 9.0, 1),
                    last_30_days: totals(999, 999, 9.0, 1),
                    all_time: totals(999, 999, 9.0, 1),
                },
                ModelSpend {
                    provider: "anthropic".into(),
                    model: "claude-opus".into(),
                    mode: None,
                    today: totals(120, 12, 0.5, 1),
                    last_7_days: totals(120, 12, 0.5, 1),
                    last_30_days: totals(120, 12, 0.5, 1),
                    all_time: totals(1120, 112, 2.5, 2),
                },
                ModelSpend {
                    provider: "anthropic".into(),
                    model: "claude-haiku".into(),
                    mode: None,
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

/// Each call's stamped mode lands in its span's split and the model rows carry their latest
/// call's mode: a plan's API-equivalent value never adds to the billed share.
#[test]
fn spend_history_splits_spend_by_billing_mode() {
    let root = tempfile::tempdir().unwrap();
    let noon = local_noon(1_795_003_200_000);
    let row = |id: &str, provider: &str, model: &str, cost: f64, billing: serde_json::Value| {
        json!({
            "type": "message", "id": id,
            "timestamp": crate::util::iso_from_unix_ms(noon as u64),
            "message": {
                "role": "assistant", "provider": provider, "model": model,
                "stopReason": "stop", "timestamp": noon,
                "usage": { "input": 10, "output": 1, "totalTokens": 11,
                           "cost": { "total": cost }, "billing": billing },
            },
        })
        .to_string()
    };
    let lines = [
        row(
            "p1",
            "anthropic",
            "claude-opus",
            1.5,
            json!({"mode": "plan", "providerQuota": true}),
        ),
        row(
            "p2",
            "anthropic",
            "claude-opus",
            0.5,
            json!({"mode": "plan", "providerQuota": true}),
        ),
        row(
            "g1",
            "openrouter",
            "glm",
            0.25,
            json!({"mode": "pay_as_you_go", "source": "provider_reported"}),
        ),
        row("f1", "ollama", "qwen", 0.0, json!({"mode": "free"})),
    ];
    std::fs::write(root.path().join("s.jsonl"), lines.join("\n")).unwrap();

    let history = spend_history(root.path(), noon + 3_600_000);

    let spend = |calls: u64, cost: f64| ModeSpend { calls, cost };
    let model = |provider: &str, name: &str, mode, calls, cost, billing: BillingSplit| {
        let totals = SpendTotals {
            input_tokens: 10 * calls,
            output_tokens: calls,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost,
            requests: calls,
            billing,
        };
        ModelSpend {
            provider: provider.into(),
            model: name.into(),
            mode: Some(mode),
            today: totals,
            last_7_days: totals,
            last_30_days: totals,
            all_time: totals,
        }
    };
    assert_eq!(
        (history.all_time.billing, history.models),
        (
            BillingSplit {
                pay_as_you_go: spend(1, 0.25),
                plan: spend(2, 2.0),
                free: spend(1, 0.0),
                ..BillingSplit::default()
            },
            vec![
                model(
                    "anthropic",
                    "claude-opus",
                    BillingMode::Plan {
                        provider_quota: true
                    },
                    2,
                    2.0,
                    BillingSplit {
                        plan: spend(2, 2.0),
                        ..BillingSplit::default()
                    },
                ),
                model(
                    "openrouter",
                    "glm",
                    BillingMode::PayAsYouGo {
                        source: CostSource::ProviderReported
                    },
                    1,
                    0.25,
                    BillingSplit {
                        pay_as_you_go: spend(1, 0.25),
                        ..BillingSplit::default()
                    },
                ),
                model(
                    "ollama",
                    "qwen",
                    BillingMode::Free,
                    1,
                    0.0,
                    BillingSplit {
                        free: spend(1, 0.0),
                        ..BillingSplit::default()
                    },
                ),
            ],
        )
    );
}

/// Serve every request by path until the test ends: `/api/oauth/usage` never answers (a hung
/// provider), `/v2/quotas` answers Synthetic's documented shape, and every other path the
/// recorded `/copilot_internal/user` shape.
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
                let body = if head.starts_with("GET /v2/quotas ") {
                    r#"{"subscription":{"limit":135,"requests":135,"renewsAt":"2026-12-01T00:00:00Z"}}"#
                } else {
                    r#"{"copilot_plan":"business","quota_reset_date":"2026-12-01","quota_snapshots":{"premium_interactions":{"entitlement":300,"remaining":0,"unlimited":false}}}"#
                };
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
            // A plan sold on an API key reads its usage with the key.
            "synthetic": {"type": "api_key", "key": "syn-key"},
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
                    account: None,
                    source: QuotaSource::Provider,
                    fetched_at_ms: 1_795_003_200_000,
                    state: QuotaState::Failed {
                        error: "the usage endpoint did not answer in time".into(),
                    },
                },
                ProviderQuota {
                    provider: "github-copilot".into(),
                    plan: Some("business".into()),
                    account: None,
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
                ProviderQuota {
                    provider: "synthetic".into(),
                    plan: None,
                    account: None,
                    source: QuotaSource::Provider,
                    fetched_at_ms: 1_795_003_200_000,
                    state: QuotaState::Windows {
                        windows: vec![QuotaWindow {
                            label: "Subscription".into(),
                            used: 135.0,
                            limit: Some(135.0),
                            unit: QuotaUnit::Requests,
                            resets_at_ms: Some(1_796_083_200_000),
                        }],
                    },
                },
            ],
            pacing: Vec::new(),
        }
    );
}

/// The worker's `usage.quotas` kernel request answers the page's quota cards from the same
/// fetch, and its reply never carries the stored token.
#[tokio::test]
async fn the_kernel_quota_request_answers_the_page_cards_without_the_token() {
    let agent_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        agent_dir.path().join("auth.json"),
        json!({
            "github-copilot": {"type": "oauth", "access": "copilot-api", "refresh": "gho_secret_token", "expires": 9_999_999_999_999_i64},
        })
        .to_string(),
    )
    .unwrap();
    let config = QuotaFetchConfig {
        origin_override: Some(quota_server().await),
        timeout: Duration::from_millis(2_000),
        http: Arc::new(pa_ai::oauth::ReqwestProviderHttp::new()),
    };
    let mut handlers = pa_core::kernel::shared::HostRequestHandlers::default();
    super::register_quota_host_handler(&mut handlers, agent_dir.path().to_path_buf(), config);
    let handler = handlers
        .get(pa_core::usage_host::USAGE_QUOTAS_REQUEST_TYPE)
        .unwrap()
        .clone();
    let reply = handler(pa_core::kernel::shared::HostRequestPayload {
        data: json!({"refresh": true}),
    })
    .await
    .unwrap();
    assert!(!reply.to_string().contains("gho_secret_token"), "{reply}");
    let mut report: QuotaReport = serde_json::from_value(reply).unwrap();
    for card in &mut report.providers {
        card.fetched_at_ms = 0;
    }
    assert_eq!(
        report,
        QuotaReport {
            providers: vec![ProviderQuota {
                provider: "github-copilot".into(),
                plan: Some("business".into()),
                account: None,
                source: QuotaSource::Provider,
                fetched_at_ms: 0,
                state: QuotaState::Windows {
                    windows: vec![QuotaWindow {
                        label: "Premium requests".into(),
                        used: 300.0,
                        limit: Some(300.0),
                        unit: QuotaUnit::Requests,
                        resets_at_ms: Some(1_796_083_200_000),
                    }],
                },
            }],
            pacing: Vec::new(),
        }
    );
}

/// Every stored credential of a plan provider gets its own card, labelled with the credential.
#[tokio::test]
async fn each_named_credential_gets_its_own_quota_card() {
    let agent_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        agent_dir.path().join("auth.json"),
        json!({ "synthetic": {"type": "api_key", "key": "syn-default"} }).to_string(),
    )
    .unwrap();
    pa_core::auth::AuthAccounts::create(agent_dir.path())
        .add(
            "synthetic",
            "work",
            pa_core::auth::AuthCredential::ApiKey {
                key: "syn-work".into(),
                prime_team: None,
            },
            0,
        )
        .unwrap();
    let config = QuotaFetchConfig {
        origin_override: Some(quota_server().await),
        timeout: Duration::from_millis(2_000),
        http: Arc::new(pa_ai::oauth::ReqwestProviderHttp::new()),
    };
    let report = quota_report(
        agent_dir.path().to_path_buf(),
        &config,
        CachePolicy::UseFresh,
        1_795_003_200_000,
    )
    .await;
    let card = |account: &str| ProviderQuota {
        provider: "synthetic".into(),
        plan: None,
        account: Some(account.into()),
        source: QuotaSource::Provider,
        fetched_at_ms: 1_795_003_200_000,
        state: QuotaState::Windows {
            windows: vec![QuotaWindow {
                label: "Subscription".into(),
                used: 135.0,
                limit: Some(135.0),
                unit: QuotaUnit::Requests,
                resets_at_ms: Some(1_796_083_200_000),
            }],
        },
    };
    assert_eq!(
        report,
        QuotaReport {
            providers: vec![card("default"), card("work")],
            pacing: Vec::new(),
        }
    );
}
