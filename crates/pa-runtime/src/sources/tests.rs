//! The runtime's sources on a bus with a recording task sink, their
//! definitions in a real database, timers on a clock the test moves.

use std::future::Future;
use std::net::SocketAddr;
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

use super::{SourceDefinition, SourceLifetime, SourceManager, WebhookCredentials, WebhookSecrets};
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
        let sources = SourceManager::spawn(
            log,
            &bus,
            Arc::clone(clock),
            WebhookSecrets::in_agent_dir(dir),
        );
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
        lifetime: SourceLifetime::Kept,
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

/// A webhook `hook` kept on `bind` (a loopback address, its port 0 for
/// one the test picks), its token `token`.
fn webhook(bind: &str) -> (SourceDefinition, WebhookCredentials) {
    let mut bind: SocketAddr = bind.parse().unwrap();
    if bind.port() == 0 {
        bind = std::net::TcpListener::bind(bind)
            .unwrap()
            .local_addr()
            .unwrap();
    }
    (
        definition(
            "hook",
            SourceKind::Webhook,
            json!({
                "bind": bind,
                "path": "/in",
                "max_body_bytes": 1024,
                "secret": "webhook:hook",
            }),
        ),
        WebhookCredentials {
            token: Some("s3cret".to_string()),
            hmac_secret: None,
        },
    )
}

/// `POST`s `body` to `address`'s `/in` with `token`; the answer's status.
async fn post(address: SocketAddr, token: &str, body: &str) -> u16 {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let request = format!(
        "POST /in HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

#[tokio::test]
async fn a_webhook_a_run_would_not_keep_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let bed = Bed::open(dir.path(), &ManualClock::at(NOW));
    let (mut definition, credentials) = webhook("127.0.0.1:0");
    definition.lifetime = SourceLifetime::Run;
    let refused = bed
        .sources
        .define_webhook(definition, credentials)
        .await
        .unwrap_err();
    assert_eq!(
        (
            refused.to_string(),
            WebhookSecrets::in_agent_dir(dir.path())
                .read("webhook:hook")
                .unwrap()
        ),
        (
            "webhook \"hook\": a webhook source needs a long-lived listener: it runs in the \
             runtime daemon, not in a headless run (`prime-agent -p`, `--mode json`)"
                .to_string(),
            None
        )
    );
}

#[tokio::test]
async fn a_kept_webhook_publishes_untrusted_events_and_keeps_its_secret_apart() {
    let dir = tempfile::tempdir().unwrap();
    let mut bed = Bed::open(dir.path(), &ManualClock::at(NOW));
    let (definition, credentials) = webhook("127.0.0.1:0");
    let address: SocketAddr = definition.options["bind"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    bed.sources
        .define_webhook(definition, credentials.clone())
        .await
        .unwrap();
    let secrets = WebhookSecrets::in_agent_dir(dir.path());
    let row = pa_log::Log::open(dir.path().join("agent.db"), pa_log::DEFAULT_EVENT_RETENTION)
        .unwrap()
        .sources(super::SOURCES_PARTITION)
        .unwrap()
        .sources
        .remove(0);
    assert_eq!(
        (
            row.spec.contains("s3cret"),
            secrets.read("webhook:hook").unwrap()
        ),
        (false, Some(credentials))
    );
    bed.subscribe("hooks", "source/hook/received");
    bed.running(&["hook"]).await;

    assert_eq!(post(address, "wrong", "{}").await, 401);
    assert_eq!(post(address, "s3cret", r#"{"ok":true}"#).await, 202);
    let (subscription, event) = bed.task().await;
    assert_eq!(
        (subscription, event),
        (
            "hooks".to_string(),
            NewEvent {
                topic: Topic::new("source/hook/received").unwrap(),
                source: "hook".to_string(),
                session: None,
                payload: json!({
                    "body": { "ok": true },
                    "headers": { "content-type": "application/json" },
                }),
                trust: Trust::Untrusted,
                cause: None,
                reply: None,
            }
        )
    );

    bed.sources.undefine("hook").await.unwrap();
    assert_eq!(secrets.read("webhook:hook").unwrap(), None);
}

#[tokio::test]
async fn a_restarted_webhook_whose_port_is_taken_records_the_one_it_moved_to() {
    let dir = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(NOW);
    let first = Bed::open(dir.path(), &clock);
    let (definition, credentials) = webhook("127.0.0.1:0");
    let own: SocketAddr = definition.options["bind"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    first
        .sources
        .define_webhook(definition, credentials)
        .await
        .unwrap();
    first.sources.shutdown().await;
    drop(first);

    let _taken = std::net::TcpListener::bind(own).unwrap();
    let second = Bed::open(dir.path(), &clock);
    second.subscribe("hooks", "source/hook/received");
    second.running(&["hook"]).await;
    let moved = second.sources.webhook_addresses()["hook"];
    assert_ne!(moved, own);
    assert_eq!(post(moved, "s3cret", "{}").await, 202);
    second.sources.shutdown().await;
    drop(second);

    let third = Bed::open(dir.path(), &clock);
    let mut listening = third.sources.webhooks.clone();
    let recorded = tokio::time::timeout(
        PATIENCE,
        listening.wait_for(|listening| listening.contains_key("hook")),
    )
    .await
    .expect("the webhook read within the test's patience")
    .unwrap()["hook"];
    assert_eq!(recorded, moved);
}
