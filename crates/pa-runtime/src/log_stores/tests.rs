use std::future::Future;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pa_log::{Log, DEFAULT_EVENT_RETENTION};
use pa_types::event_log::{EventId, NewEvent, Priority, TaskState, Topic, Trust};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::*;
use crate::bus::{Bus, BusParts, Heard};
use crate::lock;
use crate::routes::{ModelCall, ModelCaller, TaskRequest, TaskSink};
use crate::store::MemoryStateStore;
use crate::subscriptions::MemorySessionTree;
use crate::tasks::{Steered, TaskOutcome, TaskQueue, TaskRunner, INTERRUPTED_BY_RESTART};

fn open(dir: &Path) -> Arc<Log> {
    Arc::new(Log::open(dir.join("agent.db"), DEFAULT_EVENT_RETENTION).expect("the log opens"))
}

/// Takes no route's work: no test here adds a subscription.
struct Idle;

impl ModelCaller for Idle {
    fn call(&self, _call: ModelCall) -> impl Future<Output = anyhow::Result<()>> + Send {
        std::future::ready(Ok(()))
    }
}

impl TaskSink for Idle {
    fn queue(
        &self,
        _request: TaskRequest,
    ) -> impl Future<Output = anyhow::Result<Option<crate::routes::Announced>>> + Send {
        std::future::ready(Ok(None))
    }
}

fn bus(log: &Arc<Log>) -> Bus<LogEventStore, MemoryStateStore, MemorySessionTree, Idle, Idle> {
    Bus::new(BusParts {
        events: LogEventStore::new(Arc::clone(log)),
        state: MemoryStateStore::default(),
        sessions: MemorySessionTree::default(),
        model: Idle,
        tasks: Idle,
        listener_capacity: NonZeroUsize::new(16).expect("a capacity"),
    })
}

fn event(on: &str) -> NewEvent {
    NewEvent {
        topic: Topic::new(on).expect("a topic"),
        source: "test".to_string(),
        session: Some("a".to_string()),
        payload: json!({ "on": on }),
        trust: Trust::Owner,
        cause: None,
        reply: None,
    }
}

/// Events the bus publishes are stored in the database and read back the
/// same, oldest first after a sequence number and newest first before one.
#[tokio::test]
async fn events_published_through_the_bus_read_back_from_the_database() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let log = open(dir.path());
    let bus = bus(&log);
    let mut published = Vec::new();
    for on in ["agent/a/input", "agent/a/done", "timer/nightly"] {
        published.push(bus.publish(event(on)).await.expect("published").event);
    }
    let events = bus.events();
    assert_eq!(
        (
            events.since(0, 10).await.expect("read"),
            events.since(1, 1).await.expect("read"),
            events.before(u64::MAX, 10).await.expect("read"),
            events.before(3, 1).await.expect("read"),
        ),
        (
            published.clone(),
            vec![published[1].clone()],
            published.iter().rev().cloned().collect(),
            vec![published[1].clone()],
        )
    );
}

/// A live-only event reaches a listener with no sequence number and is
/// never appended: the next stored event takes the first sequence number.
#[tokio::test]
async fn a_live_only_event_reaches_listeners_and_is_not_stored() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let log = open(dir.path());
    let bus = bus(&log);
    let mut listener = bus.listen();
    let streamed = bus
        .publish(event("agent/a/stream"))
        .await
        .expect("published")
        .event;
    let stored = bus
        .publish(event("agent/a/done"))
        .await
        .expect("published")
        .event;
    let heard = [
        listener.recv().await.expect("the bus is open"),
        listener.recv().await.expect("the bus is open"),
    ];
    assert_eq!(
        (
            (streamed.seq, stored.seq),
            heard,
            bus.events().since(0, 10).await.expect("read"),
        ),
        (
            (0, 1),
            [
                Heard::Event(Arc::new(streamed)),
                Heard::Event(Arc::new(stored.clone()))
            ],
            vec![stored],
        )
    );
}

/// A run that started, and the way to end it.
type Start = (Task, oneshot::Sender<TaskOutcome>);

/// Reports each run as it starts and ends it when the test answers.
struct Gate(mpsc::UnboundedSender<Start>);

impl TaskRunner for Gate {
    async fn run(&self, task: Task) -> TaskOutcome {
        let (finish, outcome) = oneshot::channel();
        self.0.send((task, finish)).expect("the test listens");
        outcome.await.expect("the test answers")
    }

    fn steer(&self, _running: &Task, _steer: &Task) -> Steered {
        Steered::Declined
    }
}

/// The database's task store, recording every save the queue makes.
#[derive(Clone)]
struct Recording {
    store: LogTaskStore,
    saved: Arc<Mutex<Vec<Task>>>,
}

impl TaskStore for Recording {
    fn save(&self, task: Task) -> impl Future<Output = anyhow::Result<()>> + Send {
        lock(&self.saved).push(task.clone());
        self.store.save(task)
    }

    fn unfinished(&self, session: &str) -> impl Future<Output = anyhow::Result<Vec<Task>>> + Send {
        self.store.unfinished(session)
    }
}

fn task(id: &str, state: TaskState, attempts: u32) -> Task {
    Task {
        id: id.to_string(),
        session: "a".to_string(),
        priority: Priority::Prompt,
        state,
        trigger: EventId("evt-1".to_string()),
        prompt: format!("do {id}"),
        trust: Trust::Owner,
        attempts,
        error: None,
    }
}

/// After the database is closed and opened again, the restored queue fails
/// the task that was running when it closed and runs the queued one.
#[tokio::test]
async fn a_queued_task_survives_reopening_the_database() {
    let dir = tempfile::tempdir().expect("a temp dir");
    {
        let store = LogTaskStore::new(open(dir.path()));
        for saved in [
            task("interrupted", TaskState::Running, 1),
            task("queued", TaskState::Queued, 0),
            task("done", TaskState::Done, 1),
        ] {
            store.save(saved).await.expect("saved");
        }
    }
    let log = open(dir.path());
    let store = Recording {
        store: LogTaskStore::new(Arc::clone(&log)),
        saved: Arc::default(),
    };
    let (started, mut starts) = mpsc::unbounded_channel();
    let queue = TaskQueue::new(
        Gate(started),
        store.clone(),
        NonZeroUsize::new(1).expect("a cap"),
    );
    queue.restore("a").await.expect("restored");
    let (running, _finish) = starts.recv().await.expect("the queued task runs");
    let interrupted = Task {
        state: TaskState::Failed,
        error: Some(INTERRUPTED_BY_RESTART.to_string()),
        ..task("interrupted", TaskState::Running, 1)
    };
    let now_running = task("queued", TaskState::Running, 1);
    assert_eq!(
        (
            running,
            lock(&store.saved).clone(),
            log.unfinished_tasks("a").expect("read"),
        ),
        (
            now_running.clone(),
            vec![interrupted, now_running.clone()],
            vec![now_running],
        )
    );
}

/// A value a `Set` route writes reads back whole, per session and global
/// apart, and a later write to a key wins.
#[tokio::test]
async fn state_writes_read_back_by_key() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = LogStateStore::new(open(dir.path()));
    let write = |session: Option<&str>, value: serde_json::Value| StateWrite {
        key: StateKey {
            session: session.map(str::to_string),
            key: "inbox".to_string(),
        },
        value,
        trust: Trust::Untrusted,
        cause: EventId("evt-1".to_string()),
    };
    for written in [
        write(Some("a"), json!({ "unread": 1 })),
        write(None, json!("global")),
        write(Some("a"), json!({ "unread": 2 })),
    ] {
        state.set(written).await.expect("stored");
    }
    let read = |session: Option<&str>| {
        let key = StateKey {
            session: session.map(str::to_string),
            key: "inbox".to_string(),
        };
        let state = state.clone();
        async move { state.get(&key).await.expect("read") }
    };
    assert_eq!(
        (
            read(Some("a")).await,
            read(None).await,
            read(Some("b")).await
        ),
        (
            Some(write(Some("a"), json!({ "unread": 2 }))),
            Some(write(None, json!("global"))),
            None,
        )
    );
}
