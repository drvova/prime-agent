//! The kernel's `event-*` functions on the runtime's bus, as
//! `prompts/capabilities/events.md` describes them: a source one session
//! defines, a subscription another holds, and the turn a publication starts
//! in the subscriber.

use std::collections::BTreeSet;

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
