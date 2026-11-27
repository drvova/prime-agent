//! The runtime's sources on a bus with a recording task sink, their
//! definitions in a real database, timers on a clock the test moves.

use std::future::Future;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pa_sources::{Clock, SourceKind};
use pa_types::event_log::{
    AgentTarget, NewEvent, Priority, Route, Scope, Subscription, Topic, TopicFilter, Trust,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use super::{SourceDefinition, SourceManager};
use crate::bus::{Bus, BusParts};
use crate::routes::{TaskRequest, TaskSink};
use crate::runtime::NoModelCalls;
use crate::store::{MemoryEventStore, MemoryStateStore};
use crate::subscriptions::MemorySessionTree;

const MINUTE: u64 = 60_000;
/// 2026-11-25T09:30:00Z.
const NOW: u64 = 1_795_599_000_000;
/// The longest a test waits for something it expects.
const PATIENCE: Duration = Duration::from_secs(20);

/// A clock the test moves by hand.
struct ManualClock(watch::Sender<u64>);

impl ManualClock {
    fn at(now: u64) -> Arc<Self> {
        Arc::new(ManualClock(watch::Sender::new(now)))
    }

    fn set(&self, now: u64) {
        self.0.send_replace(now);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        *self.0.borrow()
    }

    async fn sleep_until(&self, unix_ms: u64) {
        let mut now = self.0.subscribe();
        // The sender lives as long as the clock, which outlives its timers.
        let _ = now.wait_for(|now| *now >= unix_ms).await;
    }
}

/// Hands each task to the test.
struct Tasks(mpsc::UnboundedSender<TaskRequest>);

impl TaskSink for Tasks {
    fn queue(&self, request: TaskRequest) -> impl Future<Output = anyhow::Result<()>> + Send {
        std::future::ready(self.0.send(request).map_err(anyhow::Error::from))
    }
}

type TestBus = Bus<MemoryEventStore, MemoryStateStore, MemorySessionTree, NoModelCalls, Tasks>;

/// Sources over the database in `dir`, as a runtime opened on it has them.
struct Bed {
    bus: TestBus,
    sources: SourceManager,
    tasks: mpsc::UnboundedReceiver<TaskRequest>,
}

impl Bed {
    fn open(dir: &Path, clock: &Arc<ManualClock>) -> Bed {
        let log = Arc::new(
            pa_log::Log::open(dir.join("agent.db"), pa_log::DEFAULT_EVENT_RETENTION).unwrap(),
        );
        let (sender, tasks) = mpsc::unbounded_channel();
        let bus = Bus::new(BusParts {
            events: MemoryEventStore::default(),
            state: MemoryStateStore::default(),
            sessions: MemorySessionTree::default(),
            model: NoModelCalls,
            tasks: Tasks(sender),
            listener_capacity: NonZeroUsize::new(64).unwrap(),
        });
        let sources = SourceManager::spawn(log, &bus, Arc::clone(clock));
        Bed {
            bus,
            sources,
            tasks,
        }
    }

    /// Subscribes the session `worker` to `filter` as `id`.
    fn subscribe(&self, id: &str, filter: &str) {
        self.bus
            .add_subscription(Subscription {
                id: id.to_string(),
                scope: Scope::Global,
                filter: TopicFilter::new(filter).unwrap(),
                when: None,
                throttle_ms: 0,
                debounce_ms: 0,
                route: Route::Agent {
                    target: AgentTarget::Session {
                        session: "worker".to_string(),
                    },
                    prompt: "handle it".to_string(),
                    priority: Priority::Event,
                },
                trust: Trust::Owner,
            })
            .unwrap();
    }

    /// Waits until exactly `names` are running.
    async fn running(&self, names: &[&str]) {
        let mut running = self.sources.running();
        tokio::time::timeout(
            PATIENCE,
            running
                .wait_for(|running| running.iter().map(String::as_str).eq(names.iter().copied())),
        )
        .await
        .expect("the sources within the test's patience")
        .unwrap();
    }

    /// The next task's subscription and triggering event.
    async fn task(&mut self) -> (String, NewEvent) {
        let task = tokio::time::timeout(PATIENCE, self.tasks.recv())
            .await
            .expect("a task within the test's patience")
            .unwrap();
        (task.subscription, task.event.event.clone())
    }
}

fn definition(name: &str, kind: SourceKind, options: Value) -> SourceDefinition {
    SourceDefinition {
        name: name.to_string(),
        kind,
        options,
        owner: Some("worker".to_string()),
    }
}

fn fire(scheduled_for: u64, fired_at: u64) -> NewEvent {
    NewEvent {
        topic: Topic::new("timer/tick").unwrap(),
        source: "tick".to_string(),
        session: None,
        payload: json!({ "scheduledFor": scheduled_for, "firedAt": fired_at }),
        trust: Trust::Owner,
        cause: None,
        reply: None,
    }
}

#[tokio::test]
async fn a_timer_runs_while_a_subscription_hears_it() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(NOW);
    let mut bed = Bed::open(dir.path(), &clock);
    bed.sources
        .define(definition(
            "tick",
            SourceKind::Timer,
            json!({ "every": "1m" }),
        ))
        .await
        .unwrap();
    bed.subscribe("ticks", "timer/tick");
    bed.running(&["tick"]).await;

    clock.set(NOW + MINUTE);
    assert_eq!(
        bed.task().await,
        ("ticks".to_string(), fire(NOW + MINUTE, NOW + MINUTE))
    );

    assert!(bed.bus.remove_subscription("ticks").is_some());
    // The source's work has ended once it is no longer running.
    bed.running(&[]).await;
    clock.set(NOW + 5 * MINUTE);
    assert!(bed.tasks.try_recv().is_err());
}

#[tokio::test]
async fn a_burst_of_changes_in_a_watched_folder_is_one_task() {
    let dir = tempfile::tempdir().unwrap();
    let folder = pa_types::platform::canonicalize(dir.path())
        .unwrap()
        .join("inbox");
    std::fs::create_dir(&folder).unwrap();
    let mut bed = Bed::open(dir.path(), &ManualClock::at(NOW));
    bed.sources
        .define(definition(
            "inbox",
            SourceKind::Watch,
            json!({ "path": folder, "debounce_ms": 500 }),
        ))
        .await
        .unwrap();
    bed.subscribe("changes", "source/inbox/#");
    // Running once its platform watch is installed.
    bed.running(&["inbox"]).await;
    let paths = ["a.txt", "b.txt", "c.txt"].map(|file| folder.join(file));
    for path in &paths {
        std::fs::write(path, "new").unwrap();
    }

    let (subscription, event) = bed.task().await;
    let kinds = event.payload["kinds"].clone();
    assert!(
        kinds.as_array().unwrap().contains(&json!("create")),
        "{kinds}"
    );
    assert_eq!(
        (subscription, event),
        (
            "changes".to_string(),
            NewEvent {
                topic: Topic::new("source/inbox/changed").unwrap(),
                source: "inbox".to_string(),
                session: None,
                payload: json!({ "paths": paths, "kinds": kinds }),
                trust: Trust::Owner,
                cause: None,
                reply: None,
            }
        )
    );
    bed.sources.shutdown().await;
}

#[tokio::test]
async fn a_restart_catches_a_timer_up_once() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(NOW);
    let mut first = Bed::open(dir.path(), &clock);
    first
        .sources
        .define(definition(
            "tick",
            SourceKind::Timer,
            json!({ "every": "1m" }),
        ))
        .await
        .unwrap();
    first.subscribe("ticks", "timer/tick");
    first.running(&["tick"]).await;
    clock.set(NOW + MINUTE);
    assert_eq!(
        first.task().await,
        ("ticks".to_string(), fire(NOW + MINUTE, NOW + MINUTE))
    );
    first.sources.shutdown().await;
    drop(first);

    // Ten fires and a half missed while nothing ran.
    let back = NOW + 11 * MINUTE + MINUTE / 2;
    clock.set(back);
    let mut second = Bed::open(dir.path(), &clock);
    second.subscribe("ticks", "timer/tick");
    let caught_up = second.task().await;
    // The next fire counts from the catch-up fire.
    clock.set(back + MINUTE);
    assert_eq!(
        (caught_up, second.task().await),
        (
            ("ticks".to_string(), fire(NOW + 11 * MINUTE, back)),
            ("ticks".to_string(), fire(back + MINUTE, back + MINUTE)),
        )
    );
}

#[tokio::test]
async fn a_webhook_definition_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let bed = Bed::open(dir.path(), &ManualClock::at(NOW));
    let refused = bed
        .sources
        .define(definition(
            "hook",
            SourceKind::Webhook,
            json!({ "bind": "127.0.0.1:0" }),
        ))
        .await
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "webhook \"hook\": webhook sources arrive with the daemon"
    );
}
