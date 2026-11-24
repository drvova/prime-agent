//! `web.automate` against a local page and a local OpenAI-compatible mock:
//! no network, no real model.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::auth::{AuthStorage, AuthStorageData, NoOAuth};
use crate::settings::{WebCrawlAiSettings, WebCrawlSettings};
use crate::web_crawl::tests::{browsers_left, Site};
use crate::web_crawl::{browser, CookieJar, CrawlSettings, WebCrawlEngine, WebCrawlHost};

const CHROMIUM: &str = "/opt/pw-browsers/chromium";
const KEY: &str = "sk-mock-0123456789abcdef";

/// How the mock answers its `n`th request (from 0): a status and a body, or
/// never.
type Answer = fn(usize, &str) -> Option<(u16, String)>;

/// A local chat-completions endpoint recording each request's head
/// (lowercased) and answering as `answer` says.
struct Endpoint {
    url: String,
    heads: Arc<Mutex<Vec<String>>>,
    asked: Arc<tokio::sync::Notify>,
}

impl Endpoint {
    async fn start(answer: Answer) -> Endpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("addr")
        );
        let heads = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::new(tokio::sync::Notify::new());
        let (recorded, notify) = (Arc::clone(&heads), Arc::clone(&asked));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (recorded, notify) = (Arc::clone(&recorded), Arc::clone(&notify));
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let head_end = loop {
                        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            break at + 4;
                        }
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .and_then(|length| length.trim().parse().ok())
                        .unwrap_or(0);
                    while buffer.len() < head_end + length {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let index = {
                        let mut heads = recorded.lock().unwrap();
                        heads.push(head.clone());
                        heads.len() - 1
                    };
                    notify.notify_one();
                    let Some((status, body)) = answer(index, &head) else {
                        std::future::pending::<()>().await;
                        return;
                    };
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Endpoint { url, heads, asked }
    }

    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }
}

/// A chat completion whose message is `plan`, with some usage.
fn completion(plan: &serde_json::Value) -> String {
    json!({
        "choices": [{"message": {"role": "assistant", "content": plan.to_string()}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110},
    })
    .to_string()
}

/// A page whose button adds a paragraph.
async fn site() -> Site {
    Site::serving(
        HashMap::from([(
            "/",
            (
                "text/html",
                "<!doctype html><html><head><title>Shop</title></head><body><main id=\"m\"><h1>Shop</h1><p>The front page of the fixture shop, with enough prose to be read.</p></main><button id=\"more\" onclick=\"document.getElementById('m').insertAdjacentHTML('beforeend','<p>Order 42 is confirmed for a total of 9 euros.</p>')\">order</button></body></html>".to_string(),
            ),
        )]),
        false,
    )
    .await
}

/// A session's host with the user's `web.crawl.ai` pointing at `endpoint`
/// and a credential for the provider it names.
fn host(endpoint: Option<&Endpoint>) -> WebCrawlHost {
    let mut auth =
        AuthStorage::in_memory_without_env(&AuthStorageData::default(), Arc::new(NoOAuth));
    auth.set_runtime_api_key("mockai", KEY.to_string());
    let ai = endpoint.map(|endpoint| WebCrawlAiSettings {
        endpoint: Some(endpoint.url.clone()),
        model: Some("mock-model".to_string()),
        key_from: Some("mockai".to_string()),
    });
    WebCrawlHost {
        engine: Arc::new(WebCrawlEngine::direct()),
        store: None,
        observer: None,
        capture_observer: None,
        extract_observer: None,
        settings: Some(Arc::new(move || WebCrawlSettings {
            ai: ai.clone(),
            ..Default::default()
        }) as CrawlSettings),
        cookies: CookieJar::default(),
        credentials: Some(Arc::new(tokio::sync::Mutex::new(auth))),
        automate_observer: None,
    }
}

fn scope(name: &str) -> Arc<pa_types::session_scope::SessionScope> {
    pa_types::session_scope::SessionScope::new(
        name.to_string(),
        std::env::temp_dir(),
        BTreeMap::from([(browser::BROWSER_PATH_VAR.to_string(), CHROMIUM.to_string())]),
        BTreeSet::new(),
    )
}

/// The engine runs the steps the model answers with on the page until the
/// model says it is done: the click's paragraph is in the final page, the
/// extraction and the usage come back, the user's credential for the named
/// provider went to the endpoint, and no browser is left.
#[tokio::test(flavor = "multi_thread")]
async fn the_model_drives_the_page_to_its_goal_with_the_users_credential() {
    if !std::path::Path::new(CHROMIUM).exists() {
        eprintln!("skipped: no Chromium at {CHROMIUM}");
        return;
    }
    let site = site().await;
    let endpoint = Endpoint::start(|index, _| {
        Some((
            200,
            completion(&match index {
                0 => json!({"label": "place the order", "done": false, "steps": [{"Click": "#more"}]}),
                _ => json!({"label": "order placed", "done": true, "steps": [], "extracted": {"order": 42}}),
            }),
        ))
    })
    .await;
    let host = host(Some(&endpoint));
    let reply = {
        let _scope = pa_types::session_scope::enter(Some(scope("automate")));
        host.automate(&json!({
            "url": format!("{}/", site.base),
            "goal": "place the order",
            "extract": "the order number",
        }))
        .await
        .unwrap()
    };
    let heads = endpoint.heads();
    assert_eq!(
        (
            reply["success"].clone(),
            reply["label"].clone(),
            reply["steps"].clone(),
            reply["extracted"].clone(),
            reply["usage"]["llmCalls"].clone(),
            reply["markdown"]
                .as_str()
                .unwrap_or_default()
                .contains("Order 42 is confirmed"),
            heads.len(),
            heads
                .iter()
                .all(|head| head.contains(&format!("authorization: bearer {KEY}"))),
            reply.to_string().contains(KEY),
            browsers_left().await,
        ),
        (
            json!(true),
            json!("order placed"),
            json!(1),
            json!({"order": 42}),
            json!(2),
            true,
            2,
            true,
            false,
            Vec::new(),
        )
    );
}

/// With `web.crawl.ai` unset nothing runs: no browser starts and the call
/// says only the user can turn it on.
#[tokio::test]
async fn without_the_users_setting_it_is_refused() {
    let error = host(None)
        .automate(&json!({"url": "https://example.org/", "goal": "anything"}))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("web-automate is off: it spends model tokens on the user's account, so only the user can turn it on"),
        "{error}"
    );
}

/// An endpoint that echoes the request's `Authorization` into its error
/// cannot put the key into the result: the error names it redacted.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_the_endpoint_echoes_never_reaches_the_result() {
    if !std::path::Path::new(CHROMIUM).exists() {
        eprintln!("skipped: no Chromium at {CHROMIUM}");
        return;
    }
    let site = site().await;
    let endpoint = Endpoint::start(|_, head| {
        let echoed = head
            .lines()
            .find(|line| line.starts_with("authorization:"))
            .unwrap_or_default()
            .to_string();
        Some((
            401,
            json!({"error": {"message": format!("refused {echoed}")}}).to_string(),
        ))
    })
    .await;
    let host = host(Some(&endpoint));
    let error = {
        let _scope = pa_types::session_scope::enter(Some(scope("automate-echo")));
        host.automate(
            &json!({"url": format!("{}/", site.base), "goal": "place the order", "maxSteps": 1}),
        )
        .await
    }
    .map_or_else(|error| format!("{error:#}"), |reply| reply.to_string());
    assert_eq!(
        (
            endpoint.heads().is_empty(),
            error.contains(KEY),
            error.contains("[redacted key]"),
        ),
        (false, false, true),
        "{error}"
    );
}

/// A stopped cell stops its automation at once, the model request in
/// flight and the browser with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_cell_stops_its_automation() {
    use crate::kernel::shared::HostRequestHandlers;
    use crate::tools::kernel_cell::{ExecuteStatus, KernelExecuteOptions, KernelProvisioner};
    use crate::tools::tool_definition::AbortSignal;

    if !std::path::Path::new(CHROMIUM).exists() {
        eprintln!("skipped: no Chromium at {CHROMIUM}");
        return;
    }
    let site = site().await;
    let endpoint = Endpoint::start(|_, _| None).await;
    let mut handlers = HostRequestHandlers::new();
    host(Some(&endpoint)).register(&mut handlers);
    let _scope = pa_types::session_scope::enter(Some(scope("automate-stop")));
    let kernel = crate::kernel::scheme::SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let signal = AbortSignal::new();
    let cell = format!(
        "(web-automate \"{}/\" \"place the order\" #:timeout 600)",
        site.base
    );
    let running = kernel.execute(
        &cell,
        KernelExecuteOptions {
            signal: Some(signal.clone()),
            timeout_ms: None,
            on_stream: None,
            on_late_sent_agent_message: None,
            tool_call_id: None,
        },
    );
    let stop = async {
        // The model has been asked once the endpoint saw a request.
        while endpoint.heads().is_empty() {
            endpoint.asked.notified().await;
        }
        signal.cancel();
        Instant::now()
    };
    let (result, stopped) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(running, stop)
    })
    .await
    .expect("the stopped cell ends");
    assert_eq!(
        (
            result.unwrap().status,
            stopped.elapsed() < Duration::from_secs(1),
            browsers_left().await,
        ),
        (ExecuteStatus::Aborted, true, Vec::new())
    );
}
