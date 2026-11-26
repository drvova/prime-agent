use std::time::Duration;

use pa_types::event_log::{EventId, Trust};
use tokio::sync::{mpsc, oneshot};

use super::*;

/// A run that started, and the way to end it.
type Start = (String, oneshot::Sender<TaskOutcome>);

/// Reports each run as it starts and ends it when the test answers.
struct Gate {
    started: mpsc::UnboundedSender<Start>,
    steered: mpsc::UnboundedSender<(String, String)>,
}

impl TaskRunner for Gate {
    async fn run(&self, task: Task) -> TaskOutcome {
        let (finish, outcome) = oneshot::channel();
        self.started
            .send((task.id, finish))
            .expect("the test listens");
        outcome.await.expect("the test answers")
    }

    fn steer(&self, running: &Task, steer: &Task) -> Steered {
        self.steered
            .send((running.id.clone(), steer.id.clone()))
            .expect("the test listens");
        Steered::Taken
    }

    fn cancelled(&self, _task: &Task) {}
}

struct Harness {
    queue: TaskQueue<Gate, MemoryTaskStore>,
    store: MemoryTaskStore,
    started: mpsc::UnboundedReceiver<Start>,
    steered: mpsc::UnboundedReceiver<(String, String)>,
}

fn gate() -> (
    Gate,
    mpsc::UnboundedReceiver<Start>,
    mpsc::UnboundedReceiver<(String, String)>,
) {
    let (started, started_receiver) = mpsc::unbounded_channel();
    let (steered, steered_receiver) = mpsc::unbounded_channel();
    (
        Gate { started, steered },
        started_receiver,
        steered_receiver,
    )
}

fn harness(max_concurrent_runs: usize) -> Harness {
    let (runner, started, steered) = gate();
    let store = MemoryTaskStore::default();
    let cap = NonZeroUsize::new(max_concurrent_runs).expect("a cap");
    Harness {
        queue: TaskQueue::new(runner, store.clone(), cap),
        store,
        started,
        steered,
    }
}

fn task(id: &str, session: &str, priority: Priority) -> Task {
    Task {
        id: id.to_string(),
        session: session.to_string(),
        priority,
        state: TaskState::Queued,
        trigger: EventId("e".to_string()),
        prompt: format!("do {id}"),
        trust: Trust::Owner,
        attempts: 0,
        error: None,
    }
}

impl Harness {
    async fn push(&self, id: &str, session: &str, priority: Priority) {
        self.queue
            .push(task(id, session, priority))
            .await
            .expect("queued");
    }

    /// The next task to start, and the way to end it.
    async fn next_start(&mut self) -> Start {
        self.started.recv().await.expect("a start")
    }

    /// Ends every run as it starts until the queue is idle; the ids started.
    async fn run_out(&mut self) -> Vec<String> {
        let mut started = Vec::new();
        loop {
            tokio::select! {
                () = self.queue.idle() => return started,
                Some((id, finish)) = self.started.recv() => {
                    started.push(id);
                    finish.send(TaskOutcome::Done).expect("the run waits");
                }
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn lanes_are_serial_per_session_and_parallel_up_to_the_cap() {
    let mut harness = harness(2);
    harness.push("a1", "a", Priority::Prompt).await;
    harness.push("a2", "a", Priority::Prompt).await;
    harness.push("b1", "b", Priority::Prompt).await;
    harness.push("c1", "c", Priority::Prompt).await;
    let (first, finish_first) = harness.next_start().await;
    let (second, finish_second) = harness.next_start().await;
    // Paused time moves on only once every task waits: a third start would
    // be in the channel by now.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(harness.started.try_recv().is_err());
    finish_first.send(TaskOutcome::Done).expect("the run waits");
    let (third, finish_third) = harness.next_start().await;
    finish_second
        .send(TaskOutcome::Done)
        .expect("the run waits");
    finish_third.send(TaskOutcome::Done).expect("the run waits");
    let rest = harness.run_out().await;
    assert_eq!(
        (vec![first, second, third], rest),
        (
            vec!["a1".to_string(), "b1".to_string(), "c1".to_string()],
            vec!["a2".to_string()]
        )
    );
}

#[tokio::test]
async fn a_lane_runs_the_highest_priority_first_then_the_oldest() {
    let mut harness = harness(1);
    harness.push("first", "a", Priority::Prompt).await;
    let (first, finish) = harness.next_start().await;
    for (id, priority) in [
        ("background", Priority::Background),
        ("event", Priority::Event),
        ("prompt", Priority::Prompt),
        ("follow-up", Priority::FollowUp),
        ("event-2", Priority::Event),
    ] {
        harness.push(id, "a", priority).await;
    }
    finish.send(TaskOutcome::Done).expect("the run waits");
    let mut order = vec![first];
    order.extend(harness.run_out().await);
    assert_eq!(
        order,
        [
            "first",
            "prompt",
            "follow-up",
            "event",
            "event-2",
            "background"
        ]
        .map(str::to_string)
    );
}

#[tokio::test]
async fn a_steer_is_offered_to_the_running_task_instead_of_waiting() {
    let mut harness = harness(1);
    harness.push("turn", "a", Priority::Prompt).await;
    let (_, finish) = harness.next_start().await;
    harness.push("steer", "a", Priority::Steer).await;
    assert_eq!(
        harness.steered.try_recv(),
        Ok(("turn".to_string(), "steer".to_string()))
    );
    finish.send(TaskOutcome::Done).expect("the run waits");
    assert_eq!(harness.run_out().await, Vec::<String>::new());
    assert_eq!(
        harness.store.tasks(),
        vec![
            Task {
                state: TaskState::Done,
                attempts: 1,
                ..task("turn", "a", Priority::Prompt)
            },
            Task {
                state: TaskState::Done,
                ..task("steer", "a", Priority::Steer)
            },
        ]
    );
}

#[tokio::test]
async fn cancel_queued_cancels_the_waiting_tasks_and_not_the_running_one() {
    let mut harness = harness(1);
    harness.push("running", "a", Priority::Prompt).await;
    let (_, finish) = harness.next_start().await;
    harness.push("later", "a", Priority::Event).await;
    harness.push("sooner", "a", Priority::Prompt).await;
    let cancelled = harness.queue.cancel_queued("a").await.expect("cancelled");
    finish.send(TaskOutcome::Done).expect("the run waits");
    assert_eq!(harness.run_out().await, Vec::<String>::new());
    let cancelled_task = |id, priority| Task {
        state: TaskState::Cancelled,
        ..task(id, "a", priority)
    };
    assert_eq!(
        cancelled,
        vec![
            cancelled_task("sooner", Priority::Prompt),
            cancelled_task("later", Priority::Event),
        ]
    );
}

#[tokio::test]
async fn restore_requeues_queued_tasks_and_fails_the_interrupted_one() {
    let store = MemoryTaskStore::default();
    let before = [
        Task {
            state: TaskState::Done,
            attempts: 1,
            ..task("done", "a", Priority::Prompt)
        },
        Task {
            state: TaskState::Running,
            attempts: 1,
            ..task("interrupted", "a", Priority::Prompt)
        },
        task("queued-1", "a", Priority::Event),
        task("queued-2", "a", Priority::Event),
    ];
    for task in before {
        store.save(task).await.expect("saved");
    }
    let (runner, started, steered) = gate();
    let cap = NonZeroUsize::new(1).expect("a cap");
    let queue = TaskQueue::new(runner, store.clone(), cap);
    queue.restore("a").await.expect("restored");
    let mut harness = Harness {
        queue,
        store,
        started,
        steered,
    };
    assert_eq!(
        harness.run_out().await,
        ["queued-1", "queued-2"].map(str::to_string)
    );
    let done = |id| Task {
        state: TaskState::Done,
        attempts: 1,
        ..task(id, "a", Priority::Event)
    };
    assert_eq!(
        harness.store.tasks(),
        vec![
            Task {
                state: TaskState::Done,
                attempts: 1,
                ..task("done", "a", Priority::Prompt)
            },
            Task {
                state: TaskState::Failed,
                attempts: 1,
                error: Some(INTERRUPTED_BY_RESTART.to_string()),
                ..task("interrupted", "a", Priority::Prompt)
            },
            done("queued-1"),
            done("queued-2"),
        ]
    );
}

/// A held lane starts nothing until its last hold is released, and resuming
/// it from a suspension leaves it held.
#[tokio::test]
async fn a_held_lane_starts_nothing_until_every_hold_is_released() {
    let mut harness = harness(1);
    harness.queue.hold("a", "first");
    harness.queue.hold("a", "second");
    harness.queue.suspend("a");
    let admission = harness
        .queue
        .push(task("waiting", "a", Priority::Prompt))
        .await
        .expect("queued");
    let resumed = harness.queue.resume("a");
    let first = harness.queue.release("a", "first");
    let lane = harness.queue.lane("a");
    let released = harness.queue.release("a", "second");
    assert_eq!(
        (
            admission,
            resumed,
            first,
            lane.holds,
            lane.queued.len(),
            released,
            harness.run_out().await
        ),
        (
            Admission::Parked,
            true,
            true,
            1,
            1,
            true,
            vec!["waiting".to_string()]
        )
    );
}

/// `cancel_task` takes one queued task out of its lane and leaves the rest,
/// and answers nothing for the running one.
#[tokio::test]
async fn cancel_task_cancels_one_queued_task() {
    let mut harness = harness(1);
    harness.push("running", "a", Priority::Prompt).await;
    let (_, finish) = harness.next_start().await;
    harness.push("kept", "a", Priority::Prompt).await;
    harness.push("cancelled", "a", Priority::Prompt).await;
    let running = harness
        .queue
        .cancel_task("a", "running")
        .await
        .expect("read");
    let cancelled = harness
        .queue
        .cancel_task("a", "cancelled")
        .await
        .expect("saved");
    finish.send(TaskOutcome::Done).expect("the run waits");
    assert_eq!(
        (running, cancelled, harness.run_out().await),
        (
            None,
            Some(Task {
                state: TaskState::Cancelled,
                ..task("cancelled", "a", Priority::Prompt)
            }),
            vec!["kept".to_string()]
        )
    );
}
