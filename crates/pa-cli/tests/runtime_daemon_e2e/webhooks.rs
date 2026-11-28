//! A kernel webhook source on the runtime daemon (phase 9): a session
//! defines a webhook and subscribes to it; an authenticated `POST` runs a
//! turn in that session whose prompt is wrapped as untrusted and whose
//! `bash` is refused, and a `POST` with a wrong or missing token is
//! answered 401 and runs none.

use std::time::Duration;

use pa_types::wire2::Since;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::schedules::until;
use super::turns::cell;
use super::{ok, run_turn, Bed, Client};

/// How long a test waits for a record or a turn before it fails.
const WAIT: Duration = Duration::from_secs(90);

/// `POST`s `body` to `url` (`http://<host:port><path>`) with `authorization`
/// as its `Authorization` header when there is one; the answer's status.
async fn post(url: &str, authorization: Option<&str>, body: &str) -> u16 {
    let rest = url.strip_prefix("http://").expect("an http URL");
    let (address, path) = rest.split_at(rest.find('/').expect("a path"));
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let authorization =
        authorization.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\n{authorization}Content-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    answer
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("an HTTP answer: {answer:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authenticated_post_runs_an_untrusted_turn_and_a_bad_token_runs_none() {
    let bed = Bed::new();
    let defined = bed.dir.path().join("hook.json");
    let ran = bed.dir.path().join("ran");
    let define = format!(
        r#"(fs-write {:?} (value->jsexpr-string (event-define "hook" #f #:webhook (hash 'path "/ci"))))
(event-subscribe "hook" "Summarize the CI result.")"#,
        defined.display().to_string()
    );
    let handle = format!("(bash {:?})", format!("touch '{}'", ran.display()));
    let _daemon = bed.runtime_daemon(&json!({
        "responses": [cell("defining", &define), "defined", cell("handling", &handle), "handled"],
        "repeatLastResponse": true,
    }));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let id = ok(client.call("session.create", json!({})).await)["session"]
        .as_str()
        .unwrap()
        .to_string();
    client.subscribe(&format!("agent/{id}/#"), Since::Now).await;
    run_turn(&mut client, &id, "define the webhook").await;
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(&defined).expect("the definition"))
            .expect("a JSON record");
    let url = record["webhook"]["url"]
        .as_str()
        .expect("its URL")
        .to_string();
    let token = record["token"].as_str().expect("its generated token");
    assert_eq!(
        (&record["webhook"]["path"], &record["webhook"]["auth"]),
        (&json!("/ci"), &json!(["token"]))
    );

    // It listens once it runs: it starts as it hears the subscription.
    until(WAIT, "the webhook to listen", async || {
        let sources = ok(client.query("sources", json!({})).await);
        sources["sources"]
            .as_array()?
            .iter()
            .any(|source| source["name"] == "hook" && source["running"] == true)
            .then_some(())
    })
    .await;
    // Neither a wrong token nor none is let in.
    assert_eq!(post(&url, Some("Bearer wrong"), "{}").await, 401);
    assert_eq!(post(&url, None, "{}").await, 401);
    assert_eq!(
        post(
            &url,
            Some(&format!("Bearer {token}")),
            r#"{"build":"green"}"#
        )
        .await,
        202
    );

    let transcript = async |client: &mut Client| {
        ok(client
            .query("transcript.page", json!({ "session": id, "limit": 500 }))
            .await)["entries"]
            .as_array()
            .expect("a page of entries")
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
    };
    let rows = until(WAIT, "the delivery's turn to end", async || {
        let rows = transcript(&mut client).await;
        rows.iter()
            .any(|row| row.contains("\"handled\""))
            .then_some(rows)
    })
    .await;
    let delivered: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains("<untrusted-event source=\\\"hook\\\">"))
        .collect();
    assert_eq!(delivered.len(), 1, "one untrusted turn: {rows:#?}");
    assert!(
        delivered[0].contains("[agent-message from event hook]") && delivered[0].contains("green"),
        "{}",
        delivered[0]
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("refused for an untrusted turn")),
        "the bash cell's refusal: {rows:#?}"
    );
    assert!(!ran.exists(), "the untrusted turn's bash ran");
}
