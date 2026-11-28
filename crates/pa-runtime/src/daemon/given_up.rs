//! The release of a session its supervision gave up on: its `!` command is
//! killed, the session closed, its entry and lease dropped, its jobs
//! disarmed and its roster entry made due.
//!
//! The release hears `agent/<id>/failed` on the bus. A listener that falls
//! behind loses the oldest events, and a lost `failed` would leave its
//! session held, its lease with it, for as long as the daemon runs (a
//! create of it would wait for a release that never comes). So a lag
//! releases every held session its supervision has given up on: the
//! session state the lost events would have reported, read directly.
//!
//! Only a session whose host is given up is released. A `failed` heard
//! after the lag's sweep let its session go, and a client opened it again,
//! names a session that runs: it is left alone.

use std::sync::Arc;

use tokio::task::JoinHandle;

use super::handlers::Feeds;
use super::held::OpenSessions;
use super::schedules::Schedules;
use crate::bus::Heard;
use crate::runtime::Runtime;
use crate::session::SessionBuilder;

/// Starts the release; it runs until aborted or the bus closes.
pub(super) fn spawn<B: SessionBuilder>(
    runtime: Arc<Runtime<B>>,
    open: Arc<OpenSessions>,
    schedules: Arc<Schedules<B>>,
    feeds: Arc<Feeds>,
) -> JoinHandle<()> {
    let mut listener = runtime.listen();
    tokio::spawn(async move {
        while let Some(heard) = listener.recv().await {
            let release = to_release(
                &heard,
                || open.paths().into_keys().collect(),
                |id| open.contains(id) && runtime.given_up(id),
            );
            for id in release {
                let closing = open.bashes.close(&id).await;
                if let Err(error) = runtime.close_session(&id).await {
                    tracing::warn!("the given-up session {id} did not close cleanly: {error:#}");
                }
                open.remove(&id).await;
                drop(closing);
                schedules.disarm(&id);
                feeds.due(&id);
            }
        }
    })
}

/// The sessions `heard` says to release: the one an `agent/<id>/failed`
/// names, or after a lag every one of `open`; each only while `given_up`
/// holds for it (it is held here and its supervision gave it up).
fn to_release(
    heard: &Heard,
    open: impl FnOnce() -> Vec<String>,
    given_up: impl Fn(&str) -> bool,
) -> Vec<String> {
    match heard {
        Heard::Event(event) => {
            let mut levels = event.event.topic.levels();
            match (levels.next(), levels.next(), levels.next(), levels.next()) {
                (Some("agent"), Some(id), Some("failed"), None) if given_up(id) => {
                    vec![id.to_string()]
                }
                _ => Vec::new(),
            }
        }
        Heard::Lagged { .. } => open().into_iter().filter(|id| given_up(id)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pa_types::event_log::{Event, EventId, NewEvent, Topic, Trust};
    use serde_json::json;

    use super::to_release;
    use crate::bus::Heard;

    fn heard(topic: &str) -> Heard {
        Heard::Event(Arc::new(Event {
            id: EventId("e1".to_string()),
            seq: 1,
            ts: 0,
            event: NewEvent {
                topic: Topic::new(topic).expect("a topic"),
                source: "runtime".to_string(),
                session: None,
                payload: json!({}),
                trust: Trust::Owner,
                cause: None,
                reply: None,
            },
        }))
    }

    fn open() -> Vec<String> {
        ["given-up", "running"].map(String::from).to_vec()
    }

    fn given_up(id: &str) -> bool {
        id == "given-up"
    }

    /// A lag that may have lost a `failed` releases the held sessions given
    /// up on, and only those; a `failed` releases its session only while
    /// it is given up (a later one, for a session opened again, does not).
    #[test]
    fn a_lag_releases_the_sessions_given_up_on_and_a_failed_only_its_own() {
        assert_eq!(
            [
                to_release(&Heard::Lagged { missed: 3 }, open, given_up),
                to_release(&heard("agent/given-up/failed"), open, given_up),
                to_release(&heard("agent/running/failed"), open, given_up),
                to_release(&heard("agent/given-up/done"), open, given_up),
            ],
            [
                vec!["given-up".to_string()],
                vec!["given-up".to_string()],
                Vec::new(),
                Vec::new(),
            ]
        );
    }
}
