//! Deprecated aliases through a real session kernel: each sends what its
//! replacement sends and tells its notice once per kernel.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelExecutor, KernelProvisioner};

/// What each cell printed, or the error it raised.
async fn printed(kernel: &dyn KernelExecutor, cells: &[&str]) -> Vec<Result<String, String>> {
    let mut printed = Vec::new();
    for cell in cells {
        let result = kernel
            .execute(
                cell,
                KernelExecuteOptions {
                    signal: None,
                    timeout_ms: None,
                    on_stream: None,
                    on_late_sent_agent_message: None,
                    tool_call_id: None,
                },
            )
            .await
            .unwrap();
        printed.push(match result.error {
            None => Ok(result.stdout),
            Some(error) => Err(error.evalue),
        });
    }
    printed
}

/// A kernel whose `web.capture` and `web.extract` record what reaches them.
async fn recording() -> (Box<dyn KernelExecutor>, Arc<Mutex<Vec<Value>>>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = HostRequestHandlers::new();
    for request_type in ["web.capture", "web.extract"] {
        let received = Arc::clone(&received);
        handlers.register(
            request_type,
            host_handler(move |payload: HostRequestPayload| {
                received.lock().unwrap().push(payload.data);
                async { Ok(json!({})) }
            }),
        );
    }
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    (kernel, received)
}

/// `web-pdf`, `web-screenshot` and `web-extract` send exactly what the
/// `web-capture` and `web-read #:select` calls they stand for send, and each
/// prints its notice the first time only.
#[tokio::test(flavor = "multi_thread")]
async fn an_alias_sends_what_its_replacement_sends_and_tells_its_notice_once() {
    let (kernel, received) = recording().await;
    let returned = printed(
        kernel.as_ref(),
        &[
            r#"(web-pdf "https://example.org/" "a.pdf" #:background #t #:timeout 5)"#,
            r#"(web-pdf "https://example.org/" "a.pdf" #:background #t #:timeout 5)"#,
            r#"(web-capture "https://example.org/" "a.pdf" #:format "pdf" #:background #t #:timeout 5)"#,
            r#"(web-screenshot "https://example.org/" "a.png" #:full-page #t)"#,
            r#"(web-capture "https://example.org/" "a.png" #:full-page #t)"#,
            r#"(web-extract "<h1>x</h1>" (hash 'heading "h1") #:cookies #f)"#,
            r#"(web-read "<h1>x</h1>" #:select (hash 'heading "h1") #:cookies #f)"#,
        ],
    )
    .await;
    let received = received.lock().unwrap().clone();
    let notice = |old: &str, new: &str| {
        Ok(format!(
            "{old} is deprecated and goes away in the next release: use {new}\n"
        ))
    };
    assert_eq!(
        (returned, received.clone()),
        (
            vec![
                notice("web-pdf", r#"(web-capture source path #:format "pdf")"#),
                Ok(String::new()),
                Ok(String::new()),
                notice(
                    "web-screenshot",
                    r#"(web-capture source path #:format "png")"#
                ),
                Ok(String::new()),
                notice("web-extract", "(web-read source #:select selectors)"),
                Ok(String::new()),
            ],
            [2, 2, 2, 4, 4, 6, 6]
                .map(|replacement| received[replacement].clone())
                .to_vec(),
        )
    );
}

/// `web-capture` refuses an option its format does not take, and `web-read`
/// the options of the reading it is not doing, before the host hears of the
/// call.
#[tokio::test(flavor = "multi_thread")]
async fn an_option_the_format_or_reading_does_not_take_is_refused() {
    let (kernel, received) = recording().await;
    let returned = printed(
        kernel.as_ref(),
        &[
            r#"(web-capture "https://example.org/" "a.png" #:landscape #t)"#,
            r#"(web-capture "https://example.org/" "a.pdf" #:format 'pdf #:full-page #t)"#,
            r#"(web-capture "https://example.org/" "a.gif" #:format "gif")"#,
            r#"(web-read "https://example.org/" #:render #t)"#,
            r#"(web-read "https://example.org/" #:select (hash 'h "h1") #:max-chars 10)"#,
        ],
    )
    .await;
    assert_eq!(
        (returned, received.lock().unwrap().clone()),
        (
            [
                r#"web-capture: #:landscape does not apply to #:format "png""#,
                r#"web-capture: #:full-page does not apply to #:format "pdf""#,
                r#"web-capture: #:format is "png" or "pdf""#,
                "web-read: #:render applies with #:select only",
                "web-read: #:max-chars does not apply with #:select",
            ]
            .map(|reason| Err(format!("Error: Generic: {reason}")))
            .to_vec(),
            Vec::<Value>::new(),
        )
    );
}
