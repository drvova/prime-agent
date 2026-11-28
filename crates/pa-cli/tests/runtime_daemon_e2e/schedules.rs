//! Scheduled jobs and heartbeats on the runtime daemon (phase 7d), ported
//! from the old daemon's `readoption_wake_e2e`, `startup_no_auto_resume_e2e`
//! and `session_stop_lifecycle_e2e` onto wire2, with their expectations.
//! The jobs are made by the session's own kernel (`event-define` of a heartbeat
//! and of a scheduled job), as the runtime daemon registers them:
//!
//! - a heartbeat fires as `heartbeat_prompt` rows, and keeps firing after
//!   the daemon is killed and started again, with nobody opening its idle
//!   session (the next daemon wakes it when the job is due);
//! - a heartbeat whose time passed while no daemon ran fires once at the next
//!   boot, its next run counted from that fire;
//! - a heartbeat still fires after a restart when the killed daemon left the
//!   store's state lock held: the next daemon's lane runs its failed passes
//!   again until the lock is stale;
//! - a session stopped on purpose is never woken by its due job;
//! - a closed session's job and state follow its close reason;
//! - a plain scheduled job runs its prompt as a turn.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pa_types::wire2::Since;
use serde_json::{json, Value};

use super::turns::cell;
use super::{ok, run_turn, Bed, Client};

/// How long a test waits for a fire or a state change before it fails.
const FIRE_WAIT: Duration = Duration::from_secs(60);

/// The heartbeat every test arms: every 10 s, the store's shortest.
const HEARTBEAT: &str = r#"(event-define "liveness" #f #:kind "heartbeat" #:prompt "liveness ping" #:timer (hash 'every "every 10s"))"#;

/// A session's first turn runs `code`, every later one answers plainly.
fn script(code: &str) -> Value {
    json!({
        "responses": [cell("arming", code), "armed", "the lane works on"],
        "repeatLastResponse": true,
    })
}

/// The script a restarted daemon runs: every turn answers plainly (each
/// session's engine replays its script from the start, and a woken session
/// must not arm its job again).
fn plain() -> Value {
    json!({ "responses": ["the lane works on"], "repeatLastResponse": true })
}

/// Polls `probe` until it answers, or fails the test after `wait`.
pub(super) async fn until<T>(
    wait: Duration,
    what: &str,
    mut probe: impl AsyncFnMut() -> Option<T>,
) -> T {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// One session of the test, by id and transcript.
struct Session {
    id: String,
    path: PathBuf,
}

impl Session {
    /// Creates a session and runs its first turn (which arms its job).
    async fn create(client: &mut Client) -> Self {
        let created = ok(client.call("session.create", json!({})).await);
        let session = Session {
            id: created["session"].as_str().expect("an id").to_string(),
            path: PathBuf::from(created["path"].as_str().expect("a path")),
        };
        client
            .subscribe(&format!("agent/{}/#", session.id), Since::Now)
            .await;
        run_turn(client, &session.id, "arm the job").await;
        session
    }

    /// The `customType`s of its transcript's custom rows equal to `kind`.
    async fn rows(&self, client: &mut Client, kind: &str) -> usize {
        let page = ok(client
            .query(
                "transcript.page",
                json!({ "session": self.id, "limit": 500 }),
            )
            .await);
        page["entries"]
            .as_array()
            .expect("a page of entries")
            .iter()
            .filter(|row| row["type"] == "custom_message" && row["customType"] == kind)
            .count()
    }

    /// The user prompts of its transcript.
    async fn prompts(&self, client: &mut Client) -> Vec<String> {
        let page = ok(client
            .query(
                "transcript.page",
                json!({ "session": self.id, "limit": 500 }),
            )
            .await);
        super::turn_rows(&page)
            .into_iter()
            .filter(|(_, role, _)| role.as_deref() == Some("user"))
            .filter_map(|(_, _, text)| text)
            .collect()
    }

    /// Its scheduled-jobs partition.
    fn partition(&self) -> PathBuf {
        self.path
            .parent()
            .and_then(std::path::Path::parent)
            .expect("a sessions dir")
            .join("session-artifacts")
            .join(&self.id)
            .join("scheduled-jobs.json")
    }

    /// Its only job, as the store keeps it.
    fn job(&self, bed: &Bed) -> pa_core::cron::AgentCronJob {
        let partition = self.partition();
        let state = pa_core::agent_state::StateDatabase::open(
            &pa_core::session::store::database_path(&bed.agent_dir()),
        )
        .expect("open the agent database");
        let mut jobs = state.scheduled_jobs(&partition);
        assert_eq!(jobs.len(), 1, "one job: {jobs:?}");
        jobs.remove(0)
    }

    /// When its job next runs.
    fn next_run(&self, bed: &Bed) -> Option<u64> {
        self.job(bed)
            .next_run_at
            .as_deref()
            .and_then(pa_core::session::manager::iso_to_unix_ms)
    }
}

fn now_ms() -> u64 {
    pa_core::autonomous::now_millis()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_heartbeat_keeps_firing_across_a_daemon_restart() {
    let bed = Bed::new();
    let daemon = bed.runtime_daemon(&script(HEARTBEAT));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let session = Session::create(&mut client).await;
    let before = until(FIRE_WAIT, "the first heartbeat", async || {
        let rows = session.rows(&mut client, "heartbeat_prompt").await;
        (rows > 0).then_some(rows)
    })
    .await;
    drop(client);
    // Killed the hard way, as a crash would.
    drop(daemon);

    // Nobody opens the session: the next daemon wakes it for its job.
    let _daemon = bed.runtime_daemon(&plain());
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    until(FIRE_WAIT, "a heartbeat after the restart", async || {
        (session.rows(&mut client, "heartbeat_prompt").await > before).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_heartbeat_fires_after_a_restart_once_the_killed_daemons_jobs_lock_is_stale() {
    let bed = Bed::new();
    let daemon = bed.runtime_daemon(&script(HEARTBEAT));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let session = Session::create(&mut client).await;
    let before = until(FIRE_WAIT, "the first heartbeat", async || {
        let rows = session.rows(&mut client, "heartbeat_prompt").await;
        (rows > 0).then_some(rows)
    })
    .await;
    drop(client);
    // Killed while it held the store's state lock (as a kill during a
    // claim or a fire's record leaves it): the next daemon's passes fail
    // until the lock is stale.
    drop(daemon);
    std::fs::create_dir_all(pa_core::platform::lock_dir::LockDir::path_for(
        &session.partition(),
    ))
    .expect("the dead daemon's lock");

    let _daemon = bed.runtime_daemon(&plain());
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    until(
        FIRE_WAIT,
        "a heartbeat once the lock is stale",
        async || (session.rows(&mut client, "heartbeat_prompt").await > before).then_some(()),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_heartbeat_that_came_due_while_the_daemon_was_down_fires_once_at_the_next_boot() {
    let bed = Bed::new();
    let daemon = bed.runtime_daemon(&script(HEARTBEAT));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let session = Session::create(&mut client).await;
    let before = until(FIRE_WAIT, "the first heartbeat", async || {
        let rows = session.rows(&mut client, "heartbeat_prompt").await;
        (rows > 0).then_some(rows)
    })
    .await;
    // The fire is settled and the next run recorded, ahead.
    let due = until(FIRE_WAIT, "the next run", async || {
        let idle = ok(client
            .query("tasks", json!({ "session": session.id }))
            .await);
        let settled =
            idle["running"].is_null() && idle["queued"].as_array().is_some_and(Vec::is_empty);
        settled
            .then(|| session.next_run(&bed))
            .flatten()
            .filter(|due| *due > now_ms())
    })
    .await;
    drop(client);
    // Killed the hard way, as a crash would.
    drop(daemon);
    // No daemon runs while the job's time passes.
    until(FIRE_WAIT, "the job's time to pass", async || {
        (now_ms() > due + 10_000).then_some(())
    })
    .await;
    assert_eq!(
        session.next_run(&bed),
        Some(due),
        "nothing moved the job while the daemon was down"
    );
    let booted = now_ms();
    let _daemon = bed.runtime_daemon(&plain());
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    until(FIRE_WAIT, "the late fire", async || {
        (session.rows(&mut client, "heartbeat_prompt").await > before).then_some(())
    })
    .await;
    let rescheduled = until(FIRE_WAIT, "the rescheduled run", async || {
        session.next_run(&bed).filter(|next| *next != due)
    })
    .await;
    assert!(
        rescheduled >= booted + 10_000,
        "the late job's next run ({rescheduled}) is a whole interval after the boot ({booted}), \
         not one of the runs missed since {due}"
    );
    assert_eq!(
        session.rows(&mut client, "heartbeat_prompt").await,
        before + 1,
        "the missed runs collapsed into one fire"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_stopped_on_purpose_is_never_woken_by_its_due_job() {
    let bed = Bed::new();
    let daemon = bed.runtime_daemon(&script(HEARTBEAT));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let stopped = Session::create(&mut client).await;
    until(
        FIRE_WAIT,
        "the stopped session's first heartbeat",
        async || (stopped.rows(&mut client, "heartbeat_prompt").await > 0).then_some(()),
    )
    .await;
    ok(client
        .call("session.stop", json!({ "session": stopped.id }))
        .await);
    let rows = stopped.rows(&mut client, "heartbeat_prompt").await;
    let job = stopped.job(&bed);
    // A sibling armed after it: its heartbeat comes due after the stopped
    // one's, so once it has fired after the restart, the stopped one would
    // have too.
    let sibling = Session::create(&mut client).await;
    let sibling_rows = until(FIRE_WAIT, "the sibling's first heartbeat", async || {
        let rows = sibling.rows(&mut client, "heartbeat_prompt").await;
        (rows > 0).then_some(rows)
    })
    .await;
    drop(client);
    // Killed the hard way, as a crash would.
    drop(daemon);

    let _daemon = bed.runtime_daemon(&plain());
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    until(
        FIRE_WAIT,
        "the sibling's heartbeat after the restart",
        async || (sibling.rows(&mut client, "heartbeat_prompt").await > sibling_rows).then_some(()),
    )
    .await;
    assert_eq!(
        (
            stopped.rows(&mut client, "heartbeat_prompt").await,
            stopped.job(&bed)
        ),
        (rows, job),
        "the stopped session's job neither fired nor moved"
    );
    let listed = ok(client.query("sessions.list", json!({})).await);
    let open: Vec<&Value> = listed["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter(|session| session["open"] == true)
        .collect();
    assert_eq!(
        open.iter()
            .map(|session| session["session"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![sibling.id.as_str()],
        "only the sibling was woken"
    );
}

/// A session closed on purpose keeps what the old worker's `kill` kept for
/// its reason: a stop cancels its job and archives it, a replacement of a
/// root archives it and keeps its job, a shutdown keeps both.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_session_cancels_and_archives_as_its_close_reason_says() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&script(HEARTBEAT));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let mut closed = Vec::new();
    for marker in [None, Some("replaced"), Some("shutdown")] {
        let session = Session::create(&mut client).await;
        let mut args = json!({ "session": session.id });
        if let Some(marker) = marker {
            args["rlmCloseReason"] = json!(marker);
        }
        ok(client.call("session.stop", args).await);
        let page = ok(client
            .query(
                "transcript.page",
                json!({ "session": session.id, "limit": 500 }),
            )
            .await);
        let state = page["entries"]
            .as_array()
            .expect("a page of entries")
            .iter()
            .rev()
            .find(|row| row["type"] == "session_state")
            .map(|row| row["state"]["status"].clone());
        closed.push((marker, session.job(&bed).status, state));
    }
    assert_eq!(
        closed,
        vec![
            (
                None,
                pa_core::cron::JobStatus::Cancelled,
                Some(json!("archived"))
            ),
            (
                Some("replaced"),
                pa_core::cron::JobStatus::Active,
                Some(json!("archived"))
            ),
            (Some("shutdown"), pa_core::cron::JobStatus::Active, None),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scheduled_job_runs_its_prompt_as_a_turn() {
    let bed = Bed::new();
    let _daemon = bed.runtime_daemon(&script(r#"(event-define "check" #f #:kind "schedule" #:prompt "the scheduled check" #:timer (hash 'at 1))"#));
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let session = Session::create(&mut client).await;
    until(FIRE_WAIT, "the scheduled turn", async || {
        let prompts = session.prompts(&mut client).await;
        (prompts.iter().any(|prompt| prompt == "the scheduled check")).then_some(())
    })
    .await;
    let job = until(FIRE_WAIT, "the job's run recorded", async || {
        let job = session.job(&bed);
        (job.run_count == 1).then_some(job)
    })
    .await;
    assert_eq!(
        (job.status, job.last_error),
        (pa_core::cron::JobStatus::Completed, None)
    );
}
