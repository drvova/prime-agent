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

/// What `session`'s cells of its next task answered, raw (an error's text
/// included).
async fn raw_cells<B: SessionBuilder>(
    bed: &mut Bed<B>,
    session: &str,
    prompt: &str,
) -> Vec<String> {
    bed.input_to(session, prompt, None, Trust::Owner).await;
    let (stream, _) = bed.until_done_of(session).await;
    cell_answers(&stream)
}

/// Waits for the first stored event on `topic` and for `dones` of
/// `session`'s tasks to end: that event, and what the session's cells
/// answered meanwhile.
async fn heard_then_dones<B: SessionBuilder>(
    bed: &mut Bed<B>,
    topic: &str,
    session: &str,
    dones: usize,
) -> (Arc<pa_types::event_log::Event>, Vec<String>) {
    let (stream, done) = (
        format!("agent/{session}/stream"),
        format!("agent/{session}/done"),
    );
    let (mut heard, mut payloads, mut done_count) = (None, Vec::new(), 0);
    while heard.is_none() || done_count < dones {
        let next = tokio::time::timeout(std::time::Duration::from_secs(60), bed.listener.recv())
            .await
            .expect("the source and the session publish")
            .expect("the bus is open");
        let Heard::Event(event) = next else {
            panic!("the listener lagged");
        };
        let at = event.event.topic.as_str();
        if at == stream {
            payloads.push(event.event.payload.clone());
        } else if at == done {
            done_count += 1;
        } else if at == topic && heard.is_none() {
            heard = Some(event);
        }
    }
    (heard.unwrap(), cell_answers(&payloads))
}

/// The message an event of `source` delivers to `subscription`.
fn delivered(
    source: &str,
    event: &pa_types::event_log::Event,
    subscription: &str,
    prompt: &str,
) -> String {
    format!(
        "[agent-message from event {source}]\n\nEvent `{source}` {}, published by {source}, for \
         your subscription {subscription}.\n\n{prompt}\n\nPayload:\n{}",
        event.id.0, event.event.payload
    )
}

/// A timer source one session defines starts a turn in another subscribed
/// to it by name, and nothing of it is left in the agent's database once
/// the run ends.
#[tokio::test]
async fn a_timer_source_starts_a_turn_in_its_subscriber_for_the_run_only() {
    let mut bed = bed(json!([
        // An `at` time that has passed fires once, as the timer starts.
        eval_cell("(event-source-define \"tick\" #f #:timer (hash 'at \"2020-01-01T00:00:00Z\"))"),
        { "text": "defined" },
        eval_cell("(event-subscribe \"tick\" \"Tick.\")"),
        { "text": "subscribed" },
        { "text": "ticked" },
    ]))
    .await;
    let owner = bed.session.clone();
    let subscriber = second_session(&bed).await;
    turn(&mut bed, &owner, "define it").await;
    bed.input_to(&subscriber, "subscribe", None, Trust::Owner)
        .await;
    // The subscription's turn, then the fire's.
    let (fired, cells) = heard_then_dones(&mut bed, "timer/tick", &subscriber, 2).await;
    let subscription = subscription_id(&cells[0]);
    let transcript = bed.transcript_of(&subscriber);
    bed.runtime.shutdown().await;
    let log = pa_log::Log::open(
        pa_core::session::store::database_path(&bed.agent_dir),
        pa_log::DEFAULT_EVENT_RETENTION,
    )
    .unwrap();
    assert_eq!(
        (
            transcript,
            log.sources(crate::sources::SOURCES_PARTITION).unwrap(),
            pa_core::agent_state::held_event_bus_in(&log, &bed.agent_dir.join("event-bus.json"))
                .unwrap(),
        ),
        (
            pairs(&[
                ("user", "subscribe"),
                ("assistant", ""),
                ("assistant", "subscribed"),
                ("user", &delivered("tick", &fired, &subscription, "Tick.")),
                ("assistant", "ticked"),
            ]),
            pa_log::SourceSet::default(),
            Some(pa_core::agent_state::EventBusState::default()),
        )
    );
}

/// A folder watch under the session's directory delivers one turn for a
/// burst of writes.
#[tokio::test]
async fn a_watch_source_starts_one_turn_for_a_burst_of_writes() {
    let mut bed = bed(json!([
        eval_cell("(event-source-define \"inbox\" #f #:watch (hash 'path \"inbox\" 'debounce-ms 500))"),
        { "text": "defined" },
        eval_cell("(event-subscribe \"inbox\" \"Read the inbox.\")"),
        { "text": "subscribed" },
        { "text": "read" },
    ]))
    .await;
    let inbox = pa_types::platform::canonicalize(&bed.cwd)
        .unwrap()
        .join("inbox");
    std::fs::create_dir(&inbox).unwrap();
    let owner = bed.session.clone();
    let subscriber = second_session(&bed).await;
    turn(&mut bed, &owner, "define it").await;
    let cells = turn(&mut bed, &subscriber, "subscribe").await;
    let subscription = subscription_id(&cells[0]);
    // Running once its platform watch is installed.
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        bed.runtime
            .running_sources_changes()
            .wait_for(|running| running.contains("inbox")),
    )
    .await
    .expect("the watch starts")
    .unwrap();
    let paths = ["a.txt", "b.txt", "c.txt"].map(|file| inbox.join(file));
    for path in &paths {
        std::fs::write(path, "new").unwrap();
    }
    let (changed, _) = heard_then_dones(&mut bed, "source/inbox/changed", &subscriber, 1).await;
    assert_eq!(
        (
            changed.event.payload["paths"].clone(),
            bed.transcript_of(&subscriber).split_off(3),
        ),
        (
            json!(paths),
            pairs(&[
                (
                    "user",
                    &delivered("inbox", &changed, &subscription, "Read the inbox.")
                ),
                ("assistant", "read"),
            ]),
        )
    );
    bed.runtime.shutdown().await;
}

/// A timer or watch is refused where its rules say: a watched path outside
/// the session's directory, a name any kind holds already, both kinds at
/// once, a schema given to a timer, a publication of a timer.
#[tokio::test]
async fn timer_and_watch_definitions_are_refused_outside_their_rules() {
    let mut bed = bed(json!([
        eval_cell("(event-source-define \"outside\" #f #:watch (hash 'path \"..\"))"),
        eval_cell(DEFINE),
        eval_cell("(event-source-define \"deploy.finished\" #f #:timer (hash 'every \"5m\"))"),
        eval_cell("(event-source-define \"both\" #f #:timer (hash 'every \"5m\") #:watch (hash 'path \".\"))"),
        eval_cell("(event-source-define \"tick\" (hash 'type \"object\") #:timer (hash 'every \"5m\"))"),
        eval_cell("(event-source-define \"tick\" #f #:timer (hash 'every \"5m\"))"),
        eval_cell("(event-publish \"tick\" (hash))"),
        eval_cell("(event-source-define \"fast\" #f #:timer (hash 'every \"0s\"))"),
        { "text": "refused" },
    ]))
    .await;
    let owner = bed.session.clone();
    let answers = raw_cells(&mut bed, &owner, "define them").await;
    let expected = [
        "is not under this session's working directory",
        "deploy.finished",
        "is defined already as a bus source",
        "an event source is a timer or a watch, not both",
        "a timer source's payload schema is its kind's",
        "scheduledFor",
        "is a timer source: it publishes its own events",
        "Recurring interval must be at least 1 second",
    ];
    assert_eq!(
        answers
            .iter()
            .zip(expected)
            .map(|(answer, expected)| answer.contains(expected))
            .collect::<Vec<_>>(),
        vec![true; expected.len()],
        "{answers:#?}"
    );
    assert_eq!(answers.len(), expected.len());
    bed.runtime.shutdown().await;
}

/// Removing a running timer source stops it before the call answers.
#[tokio::test]
async fn removing_a_timer_source_stops_it() {
    let mut bed = bed(json!([
        eval_cell("(event-source-define \"tick\" #f #:timer (hash 'every \"1h\"))"),
        eval_cell("(event-subscribe \"tick\" \"Tick.\")"),
        { "text": "subscribed" },
        eval_cell("(hash-ref (event-source-remove \"tick\") 'subscriptionsRemoved)"),
        { "text": "removed" },
    ]))
    .await;
    let owner = bed.session.clone();
    turn(&mut bed, &owner, "define it").await;
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        bed.runtime
            .running_sources_changes()
            .wait_for(|running| running.contains("tick")),
    )
    .await
    .expect("the timer starts")
    .unwrap();
    let removed = turn(&mut bed, &owner, "remove it").await;
    assert_eq!(
        (removed, bed.runtime.running_sources()),
        (vec!["1".to_string()], BTreeSet::new())
    );
    bed.runtime.shutdown().await;
}
