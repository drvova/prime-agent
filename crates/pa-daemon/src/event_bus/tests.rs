//! The bus's own rules, without a supervisor: ownership, schema validation,
//! filters, who a publication reaches, and that the state survives a reload.

use pa_types::events::{EventBusRequest, EventDeliveryMode, EventSubscriptionRecord, TimerSpec};
use serde_json::{json, Map, Value};

use super::{Delivery, EventBus, Handled, Publication, Requester, Settled, EVENT_BUS_FILE_NAME};

fn requester(session_id: &str) -> Requester {
    Requester {
        session_id: session_id.to_string(),
        name: format!("{session_id}-name"),
        cwd: "/work".to_string(),
    }
}

fn answer(handled: Handled) -> Value {
    match handled {
        Handled::Answer(value) => value,
        Handled::Publish(publication) => panic!("expected an answer, got {publication:?}"),
    }
}

fn define(bus: &EventBus, owner: &Requester) {
    answer(
        bus.handle(
            owner,
            EventBusRequest::DefineSource {
                name: "deploy.finished".to_string(),
                description: "a deploy ended".to_string(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "env": { "type": "string" },
                        "ok": { "type": "boolean" },
                    },
                    "required": ["env", "ok"],
                }),
                timer: None,
                watch: None,
                webhook: None,
            },
            1,
        )
        .unwrap(),
    );
}

fn subscribe(bus: &EventBus, who: &Requester, filter: &Value) -> EventSubscriptionRecord {
    serde_json::from_value(answer(
        bus.handle(
            who,
            EventBusRequest::Subscribe {
                source: "deploy.finished".to_string(),
                filter: filter.as_object().cloned().unwrap_or_default(),
                prompt: "check the deploy".to_string(),
                delivery_mode: EventDeliveryMode::FollowUp,
                label: None,
            },
            2,
        )
        .unwrap(),
    ))
    .unwrap()
}

fn publish(bus: &EventBus, who: &Requester, payload: Value) -> anyhow::Result<Publication> {
    match bus.handle(
        who,
        EventBusRequest::Publish {
            source: "deploy.finished".to_string(),
            payload,
        },
        3,
    )? {
        Handled::Publish(publication) => Ok(publication),
        Handled::Answer(value) => panic!("expected a publication, got {value}"),
    }
}

/// A publication reaches each other session whose filter the payload matches, and never the
/// publisher's own subscription; each delivery carries the subscription's prompt and the
/// validated payload.
#[test]
fn a_publication_reaches_the_matching_subscriptions_of_other_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::load(
        dir.path().join(EVENT_BUS_FILE_NAME),
        &pa_core::platform::daemon_endpoint::RotatingLog::new(dir.path().join("log")),
    );
    let (owner, prod, any, staging) = (
        requester("owner"),
        requester("prod"),
        requester("any"),
        requester("staging"),
    );
    define(&bus, &owner);
    subscribe(&bus, &owner, &json!({}));
    let prod_subscription = subscribe(&bus, &prod, &json!({ "env": "prod" }));
    let any_subscription = subscribe(&bus, &any, &json!({}));
    subscribe(&bus, &staging, &json!({ "env": "staging" }));

    let publication = publish(&bus, &owner, json!({ "env": "prod", "ok": true })).unwrap();
    let message = |subscription: &EventSubscriptionRecord| {
        format!(
            "Event `deploy.finished` {}, published by owner-name, for your subscription {}.\n\n\
             check the deploy\n\nPayload:\n{{\"env\":\"prod\",\"ok\":true}}",
            publication.event_id, subscription.id
        )
    };
    assert_eq!(
        publication.deliveries,
        vec![
            Delivery {
                message: message(&prod_subscription),
                subscription: prod_subscription.clone(),
            },
            Delivery {
                message: message(&any_subscription),
                subscription: any_subscription.clone(),
            },
        ]
    );
}

/// Only the owner publishes, redefines, or removes a source; a payload its schema refuses
/// delivers nothing; a filter may name only declared properties.
#[test]
fn the_bus_refuses_what_its_rules_do_not_allow() {
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::load(
        dir.path().join(EVENT_BUS_FILE_NAME),
        &pa_core::platform::daemon_endpoint::RotatingLog::new(dir.path().join("log")),
    );
    let (owner, other) = (requester("owner"), requester("other"));
    define(&bus, &owner);
    let refusal = |result: anyhow::Result<Handled>| match result {
        Ok(handled) => format!("accepted: {handled:?}"),
        Err(error) => format!("{error:#}")
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
    };
    let refused = vec![
        refusal(bus.handle(
            &other,
            EventBusRequest::Publish {
                source: "deploy.finished".to_string(),
                payload: json!({ "env": "prod", "ok": true }),
            },
            3,
        )),
        refusal(bus.handle(
            &other,
            EventBusRequest::DefineSource {
                name: "deploy.finished".to_string(),
                description: String::new(),
                schema: json!({ "type": "object" }),
                timer: None,
                watch: None,
                webhook: None,
            },
            3,
        )),
        refusal(bus.handle(
            &owner,
            EventBusRequest::Publish {
                source: "deploy.finished".to_string(),
                payload: json!({ "env": "prod" }),
            },
            3,
        )),
        refusal(bus.handle(
            &other,
            EventBusRequest::Subscribe {
                source: "deploy.finished".to_string(),
                filter: Map::from_iter([("region".to_string(), json!("eu"))]),
                prompt: "x".to_string(),
                delivery_mode: EventDeliveryMode::Steer,
                label: None,
            },
            3,
        )),
        refusal(bus.handle(
            &owner,
            EventBusRequest::DefineSource {
                name: "Deploy Finished".to_string(),
                description: String::new(),
                schema: json!({ "type": "object" }),
                timer: None,
                watch: None,
                webhook: None,
            },
            3,
        )),
        refusal(bus.handle(
            &owner,
            EventBusRequest::DefineSource {
                name: "tick".to_string(),
                description: String::new(),
                schema: Value::Bool(false),
                timer: Some(TimerSpec {
                    every: Some("5m".to_string()),
                    ..TimerSpec::default()
                }),
                watch: None,
                webhook: None,
            },
            3,
        )),
    ];
    assert_eq!(
        refused,
        vec![
            "event source \"deploy.finished\" belongs to session owner-name; only it publishes or \
             removes it",
            "event source \"deploy.finished\" is defined by session owner-name; only it can \
             redefine it",
            "Validation failed for tool \"event deploy.finished\":",
            "the filter names properties the source's schema does not declare: region",
            "an event source name is 1 to 64 lowercase letters, digits, `.`, `-` and `_`, \
             starting with a letter: not \"Deploy Finished\"",
            "this event bus (the daemon's) defines bus sources only: timer sources run in \
             headless runs (`prime-agent -p`) for now",
        ]
    );
}

/// Sources, subscriptions and their counts are read back by the next daemon's bus, and a
/// subscription whose session is gone is dropped when a delivery finds it so.
#[test]
fn the_bus_survives_a_reload_and_drops_gone_subscribers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENT_BUS_FILE_NAME);
    let log = pa_core::platform::daemon_endpoint::RotatingLog::new(dir.path().join("log"));
    let (owner, kept, gone) = (requester("owner"), requester("kept"), requester("gone"));
    let bus = EventBus::load(path.clone(), &log);
    define(&bus, &owner);
    let kept_subscription = subscribe(&bus, &kept, &json!({}));
    let gone_subscription = subscribe(&bus, &gone, &json!({}));
    publish(&bus, &owner, json!({ "env": "prod", "ok": true })).unwrap();
    bus.settle(&kept_subscription.id, Settled::Delivered)
        .unwrap();
    bus.settle(&gone_subscription.id, Settled::SubscriberGone)
        .unwrap();

    let reloaded = EventBus::load(path, &log);
    let listed = answer(reloaded.handle(&kept, EventBusRequest::List, 4).unwrap());
    assert_eq!(
        (
            listed["sources"][0]["publishedCount"].clone(),
            listed["subscriptions"].clone(),
            answer(reloaded.handle(&gone, EventBusRequest::List, 4).unwrap())["subscriptions"]
                .clone(),
        ),
        (
            json!(1),
            json!([EventSubscriptionRecord {
                delivered_count: 1,
                ..kept_subscription
            }]),
            json!([]),
        )
    );
}
