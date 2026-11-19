//! Admission: the header families, the limiter against a provider that 429s above its window,
//! and foreground-first service under contention.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pa_types::control::{LimitReport, LimitWindow, Observation, Outcome, Priority};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::types::{Context, Message, Model, SimpleStreamOptions, StopReason, StreamOptions};

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

#[test]
fn each_header_family_reads_into_a_limit_report() {
    let openai = headers(&[
        ("x-ratelimit-limit-requests", "500"),
        ("x-ratelimit-remaining-requests", "499"),
        ("x-ratelimit-reset-requests", "120ms"),
        ("x-ratelimit-limit-tokens", "30000"),
        ("x-ratelimit-remaining-tokens", "29000"),
        ("x-ratelimit-reset-tokens", "2s"),
    ]);
    // The input-token window is the tightest Anthropic window (10%), so it paces.
    let anthropic = headers(&[
        ("anthropic-ratelimit-requests-limit", "50"),
        ("anthropic-ratelimit-requests-remaining", "49"),
        ("anthropic-ratelimit-requests-reset", "2999-01-01T00:00:00Z"),
        ("anthropic-ratelimit-tokens-limit", "100000"),
        ("anthropic-ratelimit-tokens-remaining", "90000"),
        ("anthropic-ratelimit-input-tokens-limit", "80000"),
        ("anthropic-ratelimit-input-tokens-remaining", "8000"),
        ("anthropic-ratelimit-output-tokens-limit", "20000"),
        ("anthropic-ratelimit-output-tokens-remaining", "19000"),
    ]);
    let retry_after = headers(&[("Retry-After", "7")]);
    let reports = [
        parse_limit_report(&openai),
        parse_limit_report(&headers(&[])),
        parse_limit_report(&retry_after),
    ];
    assert_eq!(
        reports,
        [
            LimitReport {
                requests: LimitWindow {
                    limit: Some(500),
                    remaining: Some(499),
                    reset_ms: Some(120),
                },
                tokens: LimitWindow {
                    limit: Some(30_000),
                    remaining: Some(29_000),
                    reset_ms: Some(2_000),
                },
                retry_after_ms: None,
            },
            LimitReport::default(),
            LimitReport {
                retry_after_ms: Some(7_000),
                ..LimitReport::default()
            },
        ]
    );
    // The RFC 3339 reset is a wait from now: far in the future here, so only its presence is
    // pinned; every other field compares whole.
    let anthropic = parse_limit_report(&anthropic);
    assert!(anthropic
        .requests
        .reset_ms
        .is_some_and(|wait| wait > 1_000_000));
    assert_eq!(
        LimitReport {
            requests: LimitWindow {
                reset_ms: None,
                ..anthropic.requests
            },
            ..anthropic
        },
        LimitReport {
            requests: LimitWindow {
                limit: Some(50),
                remaining: Some(49),
                reset_ms: None,
            },
            tokens: LimitWindow {
                limit: Some(80_000),
                remaining: Some(8_000),
                reset_ms: None,
            },
            retry_after_ms: None,
        }
    );
}

/// A provider allowing `WINDOW_LIMIT` requests per fixed 1s window, reporting its window in
/// OpenAI-style headers and answering 429 above it. Counts the 429s it sent.
struct WindowedProvider {
    url: String,
    rejected: Arc<AtomicUsize>,
}

const WINDOW_LIMIT: usize = 10;

async fn windowed_provider() -> WindowedProvider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let rejected = Arc::new(AtomicUsize::new(0));
    let window = Arc::new(Mutex::new((std::time::Instant::now(), 0usize)));
    let counter = Arc::clone(&rejected);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let window = Arc::clone(&window);
            let rejected = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 8192];
                // Read the head and the declared body.
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let length = text[..head_end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if request.len() >= head_end + 4 + length {
                            break;
                        }
                    }
                }
                let (allowed, remaining, reset_ms) = {
                    let mut window = window.lock().unwrap();
                    if window.0.elapsed() >= std::time::Duration::from_secs(1) {
                        *window = (std::time::Instant::now(), 0);
                    }
                    window.1 += 1;
                    let reset_ms = 1000u128
                        .saturating_sub(window.0.elapsed().as_millis())
                        .max(1);
                    (
                        window.1 <= WINDOW_LIMIT,
                        WINDOW_LIMIT.saturating_sub(window.1),
                        reset_ms,
                    )
                };
                // Hold the request a moment, so parallel requests overlap.
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                let limit_headers = format!(
                    "x-ratelimit-limit-requests: 600\r\nx-ratelimit-remaining-requests: {remaining}\r\nx-ratelimit-reset-requests: {reset_ms}ms\r\n"
                );
                let response = if allowed {
                    let chunk = |delta: &str, finish: &str| {
                        format!(
                            "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
                        )
                    };
                    let body = format!(
                        "{}{}data: [DONE]\n\n",
                        chunk("{\"role\":\"assistant\",\"content\":\"ok\"}", "null"),
                        chunk("{}", "\"stop\"")
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{limit_headers}Connection: close\r\n\r\n{body}"
                    )
                } else {
                    rejected.fetch_add(1, Ordering::SeqCst);
                    let body =
                        "{\"error\":{\"message\":\"Rate limit reached\",\"type\":\"rate_limit_exceeded\"}}";
                    format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n{limit_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    WindowedProvider { url, rejected }
}

fn windowed_model(provider: &str, url: &str) -> Model {
    serde_json::from_value(serde_json::json!({
        "id": "windowed-1", "name": "Windowed", "api": "openai-completions",
        "provider": provider, "baseUrl": url, "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 128_000, "maxTokens": 256
    }))
    .unwrap()
}

fn hello() -> Context {
    Context {
        system_prompt: None,
        messages: vec![serde_json::from_value::<Message>(serde_json::json!({
            "role": "user", "content": "hi", "timestamp": 0
        }))
        .unwrap()],
        tools: None,
    }
}

fn keyed() -> SimpleStreamOptions {
    SimpleStreamOptions::from_base(StreamOptions {
        api_key: Some("sk-windowed".to_string()),
        ..StreamOptions::default()
    })
}

/// 30 parallel requests against a provider whose window holds 10: sent straight, the burst
/// draws 429s; through the facade the limiter paces to the reported window and draws none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_limiter_keeps_a_parallel_burst_under_the_reported_window() {
    const REQUESTS: usize = 30;

    let unpaced = windowed_provider().await;
    let model = windowed_model("windowed-unpaced", &unpaced.url);
    crate::registry::ensure_builtins();
    let provider = crate::registry::get_api_provider("openai-completions").unwrap();
    let burst: Vec<_> = (0..REQUESTS)
        .map(|_| {
            let provider = Arc::clone(&provider);
            let model = model.clone();
            tokio::spawn(async move {
                provider
                    .stream_simple(&model, &hello(), Some(&keyed()))
                    .result()
                    .await
                    .stop_reason
            })
        })
        .collect();
    for request in burst {
        request.await.unwrap();
    }

    let paced = windowed_provider().await;
    let model = windowed_model("windowed-paced", &paced.url);
    let burst: Vec<_> = (0..REQUESTS)
        .map(|_| {
            let model = model.clone();
            tokio::spawn(async move {
                crate::complete_simple(&model, &hello(), Some(keyed()))
                    .await
                    .unwrap()
                    .stop_reason
            })
        })
        .collect();
    let mut stops = Vec::new();
    for request in burst {
        stops.push(request.await.unwrap());
    }

    let unpaced_rejections = unpaced.rejected.load(Ordering::SeqCst);
    assert!(
        unpaced_rejections > 0,
        "the unpaced burst must exceed the window"
    );
    assert_eq!(
        (paced.rejected.load(Ordering::SeqCst), stops),
        (0, vec![StopReason::Stop; REQUESTS]),
        "the paced burst stays under the window ({unpaced_rejections} rejections unpaced)"
    );
}

/// Under contention the foreground waiter is served before a background one that queued
/// earlier but has not waited out its aging offset.
#[tokio::test]
async fn foreground_is_served_before_background_under_contention() {
    let pool = LocalAdmission::new(ControllerConfig {
        initial_concurrency: 1,
        max_concurrency: 1,
        ..provider_controller_config()
    });
    let key = AdmissionKey {
        provider: "contended".to_string(),
        credential: String::new(),
        scope: "m".to_string(),
    };
    let now = now_ms();
    let request = |priority, enqueued_at_ms| AdmissionRequest {
        key: key.clone(),
        priority,
        enqueued_at_ms,
    };
    let held = pool.acquire(request(Priority::Foreground, now)).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut waiters = Vec::new();
    for (name, priority, enqueued_at_ms) in [
        (
            "background",
            Priority::Background,
            now.saturating_sub(1_000),
        ),
        ("foreground", Priority::Foreground, now),
    ] {
        let mut waiter = pool.acquire(request(priority, enqueued_at_ms));
        // One poll queues the waiter (the slot is held, so it cannot be granted yet).
        assert!(futures::poll!(&mut waiter).is_pending());
        let order = Arc::clone(&order);
        waiters.push(tokio::spawn(async move {
            let grant = waiter.await;
            order.lock().unwrap().push(name);
            let _cut = grant.finish(Observation {
                outcome: Outcome::Success,
                latency_ms: None,
                limits: LimitReport::default(),
            });
        }));
    }
    let _cut = held.finish(Observation {
        outcome: Outcome::Success,
        latency_ms: None,
        limits: LimitReport::default(),
    });
    for waiter in waiters {
        waiter.await.unwrap();
    }
    assert_eq!(*order.lock().unwrap(), vec!["foreground", "background"]);
}
