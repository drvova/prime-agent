//! The kernel's `event-*` functions on the runtime's bus, as
//! `prompts/capabilities/events.md` describes them: a source one session
//! defines, a subscription another holds, and the turn a publication starts
//! in the subscriber.

use std::collections::BTreeSet;

use pa_types::event_log::MAX_HOPS;

use super::*;

/// The source the examples define, with its schema.
const DEFINE: &str = "(event-source-define \"deploy.finished\" \
     (hash 'type \"object\" 'properties (hash 'env (hash 'type \"string\")) 'required (list \"env\")))";

/// Opens a second session in the bed's runtime.
async fn second_session<B: SessionBuilder>(bed: &Bed<B>) -> String {
    bed.runtime
        .create_session(SessionSpec {
            cwd: bed.cwd.clone(),
            ..SessionSpec::default()
        })
        .await
        .unwrap()
}

/// Runs `prompt` as a turn of `session` (its replies are the bed's next
/// faux responses); what each of its cells returned.
async fn turn<B: SessionBuilder>(bed: &mut Bed<B>, session: &str, prompt: &str) -> Vec<String> {
    bed.input_to(session, prompt, None, Trust::Owner).await;
    let (stream, done) = bed.until_done_of(session).await;
    assert_eq!(outcome(&done), (TaskState::Done, None, None));
    cell_answers(&stream)
        .iter()
        .map(|answer| {
            let answer: Value = serde_json::from_str(answer).unwrap();
            answer["details"]["result"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// The id of the subscription a cell's answer holds.
fn subscription_id(answer: &str) -> String {
    let start = answer.find("sub-").expect("the answer is a subscription");
    // `sub-` and a UUID.
    answer[start..start + 40].to_string()
}

/// Waits for the stored event on `topic`, then for the `done` of each of
/// `sessions`' next tasks; the event's id.
async fn published_then_done<B: SessionBuilder>(
    bed: &mut Bed<B>,
    topic: &str,
    sessions: &[&str],
) -> String {
    let mut waiting: BTreeSet<String> = sessions
        .iter()
        .map(|session| format!("agent/{session}/done"))
        .collect();
    let mut event_id = None;
    while event_id.is_none() || !waiting.is_empty() {
        let heard = tokio::time::timeout(std::time::Duration::from_secs(60), bed.listener.recv())
            .await
            .expect("the sessions publish")
            .expect("the bus is open");
        let Heard::Event(event) = heard else {
            panic!("the listener lagged");
        };
        if event.event.topic.as_str() == topic {
            event_id = Some(event.id.0.clone());
        }
        waiting.remove(event.event.topic.as_str());
    }
    event_id.unwrap()
}

/// A publication starts a turn in the subscribed session, which reads the
/// message the daemon delivers.
#[tokio::test]
async fn a_publication_starts_a_turn_in_the_subscribed_session() {
    let mut bed = bed(json!([
        eval_cell(DEFINE),
        { "text": "defined" },
        eval_cell("(event-subscribe \"deploy.finished\" \"Check the deploy.\" #:filter (hash 'env \"prod\"))"),
        { "text": "subscribed" },
        eval_cell("(hash-ref (event-publish \"deploy.finished\" (hash 'env \"prod\")) 'matched)"),
        // The publisher's turn and the subscriber's run side by side: either
        // takes either reply.
        { "text": "done" },
        { "text": "done" },
    ]))
    .await;
    let owner = bed.session.clone();
    let subscriber = second_session(&bed).await;
    turn(&mut bed, &owner, "define it").await;
    let subscribe_cells = turn(&mut bed, &subscriber, "subscribe").await;
    let subscription = subscription_id(&subscribe_cells[0]);
    bed.input_to(&owner, "publish it", None, Trust::Owner).await;
    let event_id = published_then_done(
        &mut bed,
        "source/deploy.finished/published",
        &[&owner, &subscriber],
    )
    .await;
    assert_eq!(
        bed.transcript_of(&subscriber),
        pairs(&[
            ("user", "subscribe"),
            ("assistant", ""),
            ("assistant", "subscribed"),
            (
                "user",
                &format!(
                    "[agent-message from event deploy.finished]\n\nEvent `deploy.finished` \
                     {event_id}, published by {owner}, for your subscription {subscription}.\n\n\
                     Check the deploy.\n\nPayload:\n{{\"env\":\"prod\"}}"
                )
            ),
            ("assistant", "done"),
        ])
    );
    bed.runtime.shutdown().await;
}

/// A payload the filter refuses reaches nobody, and a session's own
/// subscription never hears its own events: the publication matched none,
/// and neither session runs another turn.
#[tokio::test]
async fn a_publication_reaches_no_mismatched_filter_and_not_its_publisher() {
    let mut bed = bed(json!([
        eval_cell(DEFINE),
        eval_cell("(event-subscribe \"deploy.finished\" \"Mine.\")"),
        { "text": "defined" },
        eval_cell("(event-subscribe \"deploy.finished\" \"Prod only.\" #:filter (hash 'env \"prod\"))"),
        { "text": "subscribed" },
        eval_cell("(hash-ref (event-publish \"deploy.finished\" (hash 'env \"staging\")) 'matched)"),
        { "text": "published" },
    ]))
    .await;
    let owner = bed.session.clone();
    let subscriber = second_session(&bed).await;
    turn(&mut bed, &owner, "define it").await;
    turn(&mut bed, &subscriber, "subscribe").await;
    let published = turn(&mut bed, &owner, "publish it").await;
    // Routes run as the event is published, before the cell answers: a
    // delivery would be queued by now.
    bed.runtime.idle(&subscriber).await;
    bed.runtime.idle(&owner).await;
    assert_eq!(
        (
            published,
            bed.transcript_of(&subscriber).len(),
            bed.transcript().len()
        ),
        (vec!["0".to_string()], 3, 7)
    );
    bed.runtime.shutdown().await;
}

/// A subscription may name a topic filter: it hears every source's
/// publications.
#[tokio::test]
async fn a_topic_filter_subscription_hears_the_topics_it_matches() {
    let mut bed = bed(json!([
        eval_cell("(event-subscribe \"source/+/published\" \"Any source.\")"),
        { "text": "subscribed" },
        eval_cell(DEFINE),
        eval_cell("(hash-ref (event-publish \"deploy.finished\" (hash 'env \"prod\")) 'matched)"),
        { "text": "done" },
        { "text": "done" },
    ]))
    .await;
    let owner = bed.session.clone();
    let subscriber = second_session(&bed).await;
    let subscribe_cells = turn(&mut bed, &subscriber, "subscribe").await;
    let subscription = subscription_id(&subscribe_cells[0]);
    bed.input_to(&owner, "define and publish", None, Trust::Owner)
        .await;
    let event_id = published_then_done(
        &mut bed,
        "source/deploy.finished/published",
        &[&owner, &subscriber],
    )
    .await;
    assert_eq!(
        bed.transcript_of(&subscriber).get(3).cloned(),
        Some((
            "user".to_string(),
            format!(
                "[agent-message from event deploy.finished]\n\nEvent `deploy.finished` \
                 {event_id}, published by {owner}, for your subscription {subscription}.\n\n\
                 Any source.\n\nPayload:\n{{\"env\":\"prod\"}}"
            )
        ))
    );
    bed.runtime.shutdown().await;
}

/// Two sessions each subscribed to the other's `done` run a chain of tasks,
/// each caused by the `done` before it, which ends at the hop limit: the
/// task that would pass it is not queued, and one `system/loop_dropped`
/// says so.
#[tokio::test]
async fn subscriptions_to_each_others_done_stop_at_the_hop_limit() {
    let mut responses = vec![
        eval_cell("(event-subscribe \"agent/+/done\" \"Again.\")"),
        json!({ "text": "subscribed" }),
        eval_cell("(event-subscribe \"agent/+/done\" \"Again.\")"),
        json!({ "text": "subscribed" }),
    ];
    responses.extend(std::iter::repeat_n(
        json!({ "text": "again" }),
        2 * MAX_HOPS as usize,
    ));
    let mut bed = bed(Value::Array(responses)).await;
    let first = bed.session.clone();
    let second = second_session(&bed).await;
    turn(&mut bed, &first, "subscribe").await;
    // The second subscription's turn starts the chain: its `done` is the
    // first's to hear.
    bed.input_to(&second, "subscribe", None, Trust::Owner).await;
    // A chain past the limit is a loop that did not stop: it is cut off
    // where it would have, and fails the comparison.
    let mut chain = Vec::new();
    while chain.len() <= usize::from(MAX_HOPS) {
        let heard = tokio::time::timeout(std::time::Duration::from_secs(60), bed.listener.recv())
            .await
            .expect("the chain ends")
            .expect("the bus is open");
        let Heard::Event(event) = heard else {
            panic!("the listener lagged");
        };
        let topic = event.event.topic.as_str();
        if topic == "system/loop_dropped" {
            chain.push(("loop_dropped".to_string(), event.hops()));
            break;
        }
        if topic.ends_with("/done") {
            let session = event.event.session.clone().unwrap_or_default();
            chain.push((session, event.hops()));
        }
    }
    bed.runtime.cancel(&first).await.unwrap();
    bed.runtime.cancel(&second).await.unwrap();
    bed.runtime.idle(&first).await;
    bed.runtime.idle(&second).await;
    let mut expected: Vec<(String, u8)> = (1..MAX_HOPS)
        .map(|hops| {
            let session = if hops % 2 == 1 { &second } else { &first };
            (session.clone(), hops)
        })
        .collect();
    expected.push(("loop_dropped".to_string(), MAX_HOPS));
    assert_eq!(chain, expected);
    bed.runtime.shutdown().await;
}
