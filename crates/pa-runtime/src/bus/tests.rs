use std::future::Future;
use std::num::NonZeroUsize;
use std::time::Duration;

use pa_types::event_log::{AgentTarget, Predicate, Priority, TopicFilter, Trust, MAX_HOPS};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::*;
use crate::store::{MemoryEventStore, MemoryStateStore};
use crate::subscriptions::MemorySessionTree;

/// Sends what it is handed to the test.
#[derive(Clone)]
struct Recorder<V>(mpsc::UnboundedSender<V>);

impl ModelCaller for Recorder<ModelCall> {
    fn call(&self, call: ModelCall) -> impl Future<Output = anyhow::Result<()>> + Send {
        std::future::ready(self.0.send(call).map_err(anyhow::Error::from))
    }
}

impl TaskSink for Recorder<TaskRequest> {
    fn queue(
        &self,
        request: TaskRequest,
    ) -> impl Future<Output = anyhow::Result<Option<crate::routes::Announced>>> + Send {
        std::future::ready(
            self.0
                .send(request)
                .map(|()| None)
                .map_err(anyhow::Error::from),
        )
    }
}

type TestBus = Bus<
    MemoryEventStore,
    MemoryStateStore,
    MemorySessionTree,
    Recorder<ModelCall>,
    Recorder<TaskRequest>,
>;

struct Fixture {
    bus: TestBus,
    events: MemoryEventStore,
    state: MemoryStateStore,
    sessions: MemorySessionTree,
    calls: mpsc::UnboundedReceiver<ModelCall>,
    tasks: mpsc::UnboundedReceiver<TaskRequest>,
}

fn fixture(listener_capacity: usize) -> Fixture {
    let (calls_sender, calls) = mpsc::unbounded_channel();
    let (tasks_sender, tasks) = mpsc::unbounded_channel();
    let events = MemoryEventStore::default();
    let state = MemoryStateStore::default();
    let sessions = MemorySessionTree::default();
    let bus = Bus::new(BusParts {
        events: events.clone(),
        state: state.clone(),
        sessions: sessions.clone(),
        model: Recorder(calls_sender),
        tasks: Recorder(tasks_sender),
        listener_capacity: NonZeroUsize::new(listener_capacity).expect("a capacity"),
    });
    Fixture {
        bus,
        events,
        state,
        sessions,
        calls,
        tasks,
    }
}

fn topic(text: &str) -> Topic {
    Topic::new(text).expect("a topic")
}

fn event(on: &str, payload: Value) -> NewEvent {
    NewEvent {
        topic: topic(on),
        source: "test".to_string(),
        session: None,
        payload,
        trust: Trust::Owner,
        cause: None,
        reply: None,
    }
}

fn agent_route() -> Route {
    Route::Agent {
        target: AgentTarget::Session {
            session: "worker".to_string(),
        },
        prompt: "handle it".to_string(),
        priority: Priority::Event,
    }
}

fn subscription(id: &str, scope: Scope, filter: &str, route: Route) -> Subscription {
    Subscription {
        id: id.to_string(),
        scope,
        filter: TopicFilter::new(filter).expect("a filter"),
        when: None,
        throttle_ms: 0,
        debounce_ms: 0,
        route,
        trust: Trust::Owner,
    }
}

fn drain<V>(receiver: &mut mpsc::UnboundedReceiver<V>) -> Vec<V> {
    let mut drained = Vec::new();
    while let Ok(value) = receiver.try_recv() {
        drained.push(value);
    }
    drained
}

#[tokio::test]
async fn scope_and_filter_decide_which_events_a_subscription_hears() {
    let mut fixture = fixture(16);
    fixture.sessions.insert("child", "parent");
    fixture.sessions.insert("grandchild", "child");
    let scopes = [
        (
            "agent",
            Scope::Agent {
                session: "parent".to_string(),
            },
        ),
        ("global", Scope::Global),
        (
            "local",
            Scope::Local {
                session: "parent".to_string(),
            },
        ),
    ];
    for (id, scope) in scopes {
        fixture
            .bus
            .add_subscription(subscription(id, scope, "agent/+/done", agent_route()))
            .expect("added");
    }
    for (session, on) in [
        (Some("parent"), "agent/parent/done"),
        (Some("grandchild"), "agent/grandchild/done"),
        (Some("other"), "agent/other/done"),
        (None, "agent/none/done"),
        (Some("parent"), "agent/parent/input"),
    ] {
        let new = NewEvent {
            session: session.map(str::to_string),
            ..event(on, json!({}))
        };
        fixture.bus.publish(new).await.expect("published");
    }
    let heard: Vec<(String, String)> = drain(&mut fixture.tasks)
        .into_iter()
        .map(|request| (request.subscription, request.event.event.topic.to_string()))
        .collect();
    let expected: Vec<(String, String)> = [
        ("agent", "agent/parent/done"),
        ("global", "agent/parent/done"),
        ("local", "agent/parent/done"),
        ("agent", "agent/grandchild/done"),
        ("global", "agent/grandchild/done"),
        ("global", "agent/other/done"),
        ("global", "agent/none/done"),
    ]
    .into_iter()
    .map(|(id, on)| (id.to_string(), on.to_string()))
    .collect();
    assert_eq!(heard, expected);
}

#[tokio::test]
async fn when_lets_through_only_payloads_that_satisfy_it() {
    let mut fixture = fixture(16);
    fixture
        .bus
        .add_subscription(Subscription {
            when: Some(Predicate::Eq {
                pointer: "/env".to_string(),
                value: json!("prod"),
            }),
            ..subscription("deploys", Scope::Global, "deploy/#", agent_route())
        })
        .expect("added");
    for env in ["staging", "prod"] {
        fixture
            .bus
            .publish(event("deploy/done", json!({"env": env})))
            .await
            .expect("published");
    }
    let payloads: Vec<Value> = drain(&mut fixture.tasks)
        .into_iter()
        .map(|request| request.event.event.payload.clone())
        .collect();
    assert_eq!(payloads, vec![json!({"env": "prod"})]);
}

#[tokio::test(start_paused = true)]
async fn throttle_drops_matches_inside_the_window() {
    let mut fixture = fixture(16);
    fixture
        .bus
        .add_subscription(Subscription {
            throttle_ms: 1000,
            ..subscription("throttled", Scope::Global, "tick", agent_route())
        })
        .expect("added");
    for n in 0..3 {
        fixture
            .bus
            .publish(event("tick", json!(n)))
            .await
            .expect("published");
        tokio::time::advance(Duration::from_millis(500)).await;
    }
    let payloads: Vec<Value> = drain(&mut fixture.tasks)
        .into_iter()
        .map(|request| request.event.event.payload.clone())
        .collect();
    assert_eq!(payloads, vec![json!(0), json!(2)]);
}

#[tokio::test(start_paused = true)]
async fn debounce_runs_once_with_the_last_match_after_the_quiet_period() {
    let mut fixture = fixture(16);
    fixture
        .bus
        .add_subscription(Subscription {
            debounce_ms: 1000,
            ..subscription("debounced", Scope::Global, "save", agent_route())
        })
        .expect("added");
    let start = Instant::now();
    fixture
        .bus
        .publish(event("save", json!(0)))
        .await
        .expect("published");
    tokio::time::advance(Duration::from_millis(600)).await;
    fixture
        .bus
        .publish(event("save", json!(1)))
        .await
        .expect("published");
    // Paused time moves to the next timer once every task waits.
    let fired = fixture.tasks.recv().await.expect("a debounced task");
    assert_eq!(
        (fired.event.event.payload.clone(), start.elapsed()),
        (json!(1), Duration::from_millis(1600))
    );
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(drain(&mut fixture.tasks).is_empty());
}

#[tokio::test]
async fn a_loop_ends_at_the_hop_limit_with_one_loop_dropped() {
    let fixture = fixture(64);
    let republish = |on: &str| Route::Publish { topic: topic(on) };
    fixture
        .bus
        .add_subscription(subscription(
            "echo",
            Scope::Global,
            "ping",
            republish("ping"),
        ))
        .expect("added");
    fixture
        .bus
        .add_subscription(subscription(
            "report",
            Scope::Global,
            "system/loop_dropped",
            republish("ping"),
        ))
        .expect("added");
    let published = fixture
        .bus
        .publish(event("ping", json!({})))
        .await
        .expect("published");
    let chain: Vec<(String, u8)> = published
        .caused
        .iter()
        .map(|event| (event.event.topic.to_string(), event.hops()))
        .collect();
    let mut expected: Vec<(String, u8)> = (1..MAX_HOPS)
        .map(|hops| ("ping".to_string(), hops))
        .collect();
    expected.push(("system/loop_dropped".to_string(), MAX_HOPS));
    assert_eq!(chain, expected);
}

#[tokio::test]
async fn an_untrusted_subscription_lowers_the_trust_of_what_it_causes() {
    let mut fixture = fixture(16);
    fixture
        .bus
        .add_subscription(Subscription {
            trust: Trust::Untrusted,
            ..subscription(
                "relay",
                Scope::Global,
                "inbox",
                Route::Publish {
                    topic: topic("relayed"),
                },
            )
        })
        .expect("added");
    fixture
        .bus
        .add_subscription(subscription("act", Scope::Global, "relayed", agent_route()))
        .expect("added");
    let published = fixture
        .bus
        .publish(event("inbox", json!({})))
        .await
        .expect("published");
    let relayed = Arc::new(published.caused[0].clone());
    assert_eq!(
        drain(&mut fixture.tasks),
        vec![TaskRequest {
            subscription: "act".to_string(),
            target: AgentTarget::Session {
                session: "worker".to_string()
            },
            prompt: "handle it".to_string(),
            priority: Priority::Event,
            event: relayed,
            trust: Trust::Untrusted,
        }]
    );
}

#[tokio::test]
async fn set_writes_the_payload_under_the_subscriptions_session() {
    let fixture = fixture(16);
    fixture
        .bus
        .add_subscription(subscription(
            "goal",
            Scope::Local {
                session: "s".to_string(),
            },
            "agent/s/goal",
            Route::Set {
                key: "goal".to_string(),
            },
        ))
        .expect("added");
    let published = fixture
        .bus
        .publish(NewEvent {
            session: Some("s".to_string()),
            ..event("agent/s/goal", json!({"done": false}))
        })
        .await
        .expect("published");
    let key = StateKey {
        session: Some("s".to_string()),
        key: "goal".to_string(),
    };
    assert_eq!(
        fixture.state.get(&key),
        Some(StateWrite {
            key: key.clone(),
            value: json!({"done": false}),
            trust: Trust::Owner,
            cause: published.event.id,
        })
    );
}

#[tokio::test]
async fn a_failed_route_publishes_route_failed() {
    let mut fixture = fixture(16);
    fixture.calls.close();
    fixture
        .bus
        .add_subscription(subscription(
            "ask",
            Scope::Global,
            "question",
            Route::Call {
                prompt: "answer".to_string(),
                model: None,
            },
        ))
        .expect("added");
    let published = fixture
        .bus
        .publish(event("question", json!({})))
        .await
        .expect("published");
    let reports: Vec<(String, Value)> = published
        .caused
        .iter()
        .map(|event| (event.event.topic.to_string(), event.event.payload.clone()))
        .collect();
    assert_eq!(
        reports,
        vec![(
            "system/route_failed".to_string(),
            json!({"subscription": "ask", "error": "channel closed"}),
        )]
    );
}

#[tokio::test]
async fn a_lagging_listener_loses_the_oldest_and_catches_up_from_the_log() {
    let fixture = fixture(2);
    let mut listener = fixture.bus.listen();
    for n in 0..4 {
        fixture
            .bus
            .publish(event("tick", json!(n)))
            .await
            .expect("published");
    }
    assert_eq!(listener.recv().await, Some(Heard::Lagged { missed: 2 }));
    let Some(Heard::Event(next)) = listener.recv().await else {
        panic!("an event after the lag");
    };
    assert_eq!(next.seq, 3);
    let missed: Vec<u64> = fixture
        .events
        .since(0, 2)
        .await
        .expect("read")
        .iter()
        .map(|event| event.seq)
        .collect();
    assert_eq!(missed, vec![1, 2]);
}

#[tokio::test]
async fn an_intake_publishes_without_awaiting() {
    let fixture = fixture(16);
    let mut listener = fixture.bus.listen();
    fixture
        .bus
        .intake()
        .send(event("source/inbox/received", json!({"n": 1})))
        .expect("sent");
    let Some(Heard::Event(heard)) = listener.recv().await else {
        panic!("the event the intake took");
    };
    assert_eq!(heard.event, event("source/inbox/received", json!({"n": 1})));
}
