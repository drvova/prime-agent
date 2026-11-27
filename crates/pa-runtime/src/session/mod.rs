//! Sessions: each runs on its own host thread ([`host`]), and the task queue
//! reaches them through the [`SessionRunner`] and the bus through the
//! [`RuntimeTaskSink`] ([`runner`]).
//!
//! What a session's engine is built from is the embedder's: the runtime
//! hands a [`SessionBuilder`] the session's id and [`SessionSpec`] on the
//! session's own thread, inside its scope, and gets the engine back.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use pa_core::session_engine::engine::SessionEngine;
use pa_types::ai::Model;
use pa_types::event_log::{NewEvent, Topic};
use tokio::sync::{mpsc, oneshot};

use crate::lock;

mod host;
mod runner;

use host::SessionHost;
pub(crate) use runner::{RuntimeTaskSink, SessionRunner};

/// A session to open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSpec {
    /// The session's id: the `<id>` of its `agent/<id>/*` topics, and the id
    /// the builder opens or creates its transcript under. `None` mints a new
    /// one.
    pub id: Option<String>,
    /// What the session is called, when an event named it (`AgentTarget::Spawn`).
    pub name: Option<String>,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session's environment over the process's own
    /// (`pa_types::session_scope`).
    pub env: BTreeMap<String, String>,
}

/// A session's built engine, with the model and request auth its turns run on.
pub struct BuiltSession {
    pub engine: SessionEngine,
    pub model: Model,
    pub api_key: Option<String>,
    /// The agent dir the engine was built over: its settings and harness
    /// state.
    pub agent_dir: PathBuf,
}

/// Builds the engine a session runs its turns on: the embedder's choice of
/// provider, settings and session manager (the composition root's headless
/// assembly in the product, a faux provider in tests).
///
/// `build` runs on the session's host thread, inside the session's scope
/// (`pa_types::session_scope`) and on its runtime, so what the engine
/// spawns belongs to the session. It is called when the session opens and
/// again after a turn panicked, each time for a fresh engine of the same
/// session: it opens the session `id`'s transcript, creating it the first
/// time. It must not copy another engine (each engine is built on its own;
/// its kernel comes from pa-core's spare pool), and an error fails the task
/// that needed the engine, with the error as its reason.
pub trait SessionBuilder: Send + Sync + 'static {
    /// Builds session `id`'s engine.
    fn build(
        &self,
        id: &str,
        spec: &SessionSpec,
    ) -> impl Future<Output = anyhow::Result<BuiltSession>> + Send;
}

/// The topics a session publishes on.
#[derive(Debug, Clone)]
pub(crate) struct SessionTopics {
    /// `agent/<id>/stream`: live only.
    pub(crate) stream: Topic,
    /// `agent/<id>/done`: stored.
    pub(crate) done: Topic,
}

impl SessionTopics {
    /// The topics of session `id`.
    ///
    /// # Errors
    ///
    /// `id` cannot be a topic level (it is empty, holds `/`, `+` or `#`, or
    /// makes a topic too long).
    pub(crate) fn of(id: &str) -> anyhow::Result<Self> {
        let topic = |kind: &str| {
            let topic = Topic::new(format!("agent/{id}/{kind}"))
                .with_context(|| format!("session id {id:?} is not a topic level"))?;
            anyhow::ensure!(
                topic.levels().count() == 3,
                "session id {id:?} is not a topic level"
            );
            Ok(topic)
        };
        Ok(SessionTopics {
            stream: topic("stream")?,
            done: topic("done")?,
        })
    }
}

/// One event on its way to the bus, and who waits for it to be published.
pub(crate) struct Outgoing {
    pub(crate) event: NewEvent,
    pub(crate) published: Option<oneshot::Sender<anyhow::Result<()>>>,
}

/// The sessions' one way onto the bus: a session's thread queues events
/// without blocking and the runtime's own task publishes them in order, so a
/// task's `done` always follows its stream, and nothing a route sets off
/// runs on a session's runtime.
#[derive(Clone)]
pub(crate) struct Outbox {
    sender: mpsc::UnboundedSender<Outgoing>,
}

impl Outbox {
    pub(crate) fn channel() -> (Outbox, mpsc::UnboundedReceiver<Outgoing>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Outbox { sender }, receiver)
    }

    /// Queues `event`; once the runtime has stopped it is dropped.
    pub(crate) fn send(&self, event: NewEvent) {
        // A closed outbox is a runtime that stopped: nobody hears the event.
        let _ = self.sender.send(Outgoing {
            event,
            published: None,
        });
    }

    /// Queues `event` and waits until it is published.
    pub(crate) async fn publish(&self, event: NewEvent) -> anyhow::Result<()> {
        let (published, answer) = oneshot::channel();
        self.sender
            .send(Outgoing {
                event,
                published: Some(published),
            })
            .map_err(|_| anyhow::anyhow!("the runtime has stopped"))?;
        answer
            .await
            .map_err(|_| anyhow::anyhow!("the runtime has stopped"))?
    }
}

/// The open sessions, and what opens more.
pub(crate) struct Sessions<B> {
    builder: Arc<B>,
    outbox: Outbox,
    hosts: Mutex<HashMap<String, Arc<SessionHost>>>,
}

impl<B: SessionBuilder> Sessions<B> {
    pub(crate) fn new(builder: B, outbox: Outbox) -> Self {
        Sessions {
            builder: Arc::new(builder),
            outbox,
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Opens a session on its own host thread, which starts building its
    /// engine; returns its id.
    ///
    /// # Errors
    ///
    /// The id is not a topic level or is open already, or the operating
    /// system refused the thread.
    pub(crate) fn open(&self, spec: SessionSpec) -> anyhow::Result<String> {
        let id = spec
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let topics = SessionTopics::of(&id)?;
        let mut hosts = lock(&self.hosts);
        anyhow::ensure!(!hosts.contains_key(&id), "session {id} is open already");
        let host = SessionHost::start(
            id.clone(),
            spec,
            topics,
            Arc::clone(&self.builder),
            self.outbox.clone(),
        )
        .context("start the session's host thread")?;
        hosts.insert(id.clone(), Arc::new(host));
        Ok(id)
    }

    pub(crate) fn host(&self, id: &str) -> Option<Arc<SessionHost>> {
        lock(&self.hosts).get(id).cloned()
    }

    pub(crate) fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    /// Closes every session and waits until their threads have let go.
    pub(crate) async fn close_all(&self) {
        let hosts: Vec<Arc<SessionHost>> =
            lock(&self.hosts).drain().map(|(_, host)| host).collect();
        for host in hosts {
            host.close().await;
        }
    }
}

#[cfg(test)]
mod tests;
