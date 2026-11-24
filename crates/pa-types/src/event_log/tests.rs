use serde_json::json;

use super::{Cause, Event, EventId, HopLimit, NewEvent, Predicate, Topic, Trust, MAX_HOPS};

fn root(trust: Trust) -> Event {
    Event {
        id: EventId("evt-0".to_string()),
        seq: 1,
        ts: 0,
        event: NewEvent {
            topic: Topic::new("source/hook/received").unwrap(),
            source: "hook".to_string(),
            session: None,
            payload: json!({}),
            trust,
            cause: None,
            reply: None,
        },
    }
}

/// A caused event is never more trusted than its cause, and the chain stops
/// at the hop limit.
#[test]
fn a_caused_event_keeps_the_lower_trust_and_the_chain_is_bounded() {
    let topic = Topic::new("agent/a/inbox").unwrap();
    let from_untrusted = root(Trust::Untrusted)
        .cause(topic.clone(), "runtime", json!({}), Trust::Owner)
        .unwrap();
    assert_eq!(
        (from_untrusted.trust, from_untrusted.cause),
        (
            Trust::Untrusted,
            Some(Cause {
                id: EventId("evt-0".to_string()),
                trust: Trust::Untrusted,
                hops: 0
            })
        )
    );

    let mut chain = root(Trust::Owner);
    let mut caused = 0;
    let refused = loop {
        match chain.cause(topic.clone(), "runtime", json!({}), Trust::Owner) {
            Ok(event) => {
                caused += 1;
                chain = Event {
                    id: EventId(format!("evt-{caused}")),
                    seq: chain.seq + 1,
                    ts: 0,
                    event,
                };
            }
            Err(refused) => break refused,
        }
    };
    assert_eq!(
        (caused, chain.hops(), refused),
        (
            MAX_HOPS - 1,
            MAX_HOPS - 1,
            HopLimit {
                cause: EventId(format!("evt-{}", MAX_HOPS - 1))
            }
        )
    );
}

/// `when` conditions read the payload by JSON Pointer.
#[test]
fn a_predicate_reads_the_payload_by_pointer() {
    let payload = json!({ "env": "prod", "files": [{ "path": "a.rs" }] });
    let predicate =
        |value: serde_json::Value| -> Predicate { serde_json::from_value(value).unwrap() };
    let held: Vec<bool> = [
        json!({ "op": "eq", "pointer": "/env", "value": "prod" }),
        json!({ "op": "eq", "pointer": "/env", "value": "dev" }),
        json!({ "op": "exists", "pointer": "/files/0/path" }),
        json!({ "op": "in", "pointer": "/env", "values": ["dev", "prod"] }),
        json!({ "op": "all", "all": [
            { "op": "exists", "pointer": "/env" },
            { "op": "not", "not": { "op": "exists", "pointer": "/missing" } },
        ] }),
        json!({ "op": "any", "any": [{ "op": "exists", "pointer": "/missing" }] }),
    ]
    .into_iter()
    .map(|value| predicate(value).holds(&payload))
    .collect();
    assert_eq!(held, vec![true, false, true, true, true, false]);
}
