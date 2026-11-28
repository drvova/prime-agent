//! A deliberate stop never tears a frame a slow reader is still reading (the
//! supervisor's `shutdown_flush_e2e`): `daemon.shutdown` answers first, and
//! every connection's queue is flushed, ending with the live-only
//! `system/closing`, before the daemon exits. A reader that reads nothing
//! until the reply came keeps megabytes of frames in flight past it on every
//! platform.

use pa_types::event_log::Topic;
use pa_types::wire2::{ClientFrame, ServerFrame, Since};
use serde_json::json;

use super::{ok, Bed, Client};

/// How many large inputs the reader has queued when the stop comes, and the
/// size of each (under the frame limit's megabyte): together far more than a
/// socket's buffers hold.
const FRAMES: usize = 12;
const FRAME_TEXT: usize = 900 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn the_shutdown_exit_waits_for_a_slow_readers_in_flight_frames() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&json!({ "responses": ["ok"], "repeatLastResponse": true }));
    let (mut control, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(control.call("session.create", json!({})).await);
    let id = created["session"].as_str().unwrap().to_string();
    let input = format!("agent/{id}/input");
    let (mut reader, _) = Client::connect(&bed.runtime_socket()).await;
    reader.subscribe(&input, Since::Now).await;

    for index in 0..FRAMES {
        let request = control.id();
        control
            .send(&ClientFrame::Publish {
                id: request.clone(),
                topic: Topic::new(input.clone()).unwrap(),
                payload: json!({ "text": format!("{index}:{}", "x".repeat(FRAME_TEXT)) }),
            })
            .await;
        ok(control.reply(&request, &mut Vec::new()).await);
    }
    // The stop is answered while the reader has read none of its frames.
    ok(control.call("daemon.shutdown", json!({})).await);

    let mut heard = Vec::new();
    let closing = loop {
        match reader.frame().await {
            ServerFrame::Event { event, .. } if event.event.topic.as_str() == input => {
                let text = event.event.payload["text"].as_str().unwrap_or_default();
                heard.push((
                    text.split(':').next().unwrap_or_default().to_string(),
                    text.len(),
                ));
            }
            ServerFrame::Closing { reason } => break reason,
            other => panic!("unexpected frame before the close: {other:?}"),
        }
    };
    let sent: Vec<(String, usize)> = (0..FRAMES)
        .map(|index| {
            let prefix = index.to_string();
            let length = prefix.len() + 1 + FRAME_TEXT;
            (prefix, length)
        })
        .collect();
    assert_eq!(
        (heard, closing.is_empty()),
        (sent, false),
        "every input frame reached the slow reader whole, then the close"
    );
}
