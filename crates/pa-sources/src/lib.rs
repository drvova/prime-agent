//! Prime Agent's event sources: the things outside a session that publish
//! events onto the log.
//!
//! - [`timer`]: fires on an interval, a cron expression, or once at a time,
//!   on `timer/<name>`.
//! - [`watch`]: reports changes under a folder on `source/<name>/changed`.
//! - [`webhook`]: an HTTP listener that publishes each authenticated request
//!   on `source/<name>/received`.
//!
//! A source knows nothing of the bus or the log: it hands each event to a
//! [`Publisher`] the runtime implements. The runtime starts a source at its
//! first subscription and stops it at its last; that count lives in the
//! runtime. Here a start returns a [`SourceHandle`], and stopping the handle
//! (or dropping it) ends everything the source started, promptly.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use pa_types::event_log::{NewEvent, Topic, Trust};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod timer;
pub mod watch;
pub mod webhook;

pub use timer::{Clock, SystemClock, TimerOptions, TimerSource};
pub use watch::{WatchOptions, WatchSource};
pub use webhook::{Secret, WebhookOptions, WebhookSource};

/// The runtime no longer takes events from a source: it is shutting down,
/// or it stopped the source. A source that sees this ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the runtime no longer accepts events from this source")]
pub struct PublisherClosed;

/// How a running source hands its events to the runtime.
///
/// `publish` is synchronous and must not block: it queues the event (the
/// runtime backs it with its bus, which appends to the log off the source's
/// path) and returns. Sources call it from Tokio tasks and from the threads
/// a file watcher calls back on, where nothing can be awaited, so a
/// synchronous hand-off is the one shape that fits both. Backpressure is the
/// runtime's: an implementation buffers rather than refuses, and returns
/// [`PublisherClosed`] only once it will never take another event, after
/// which the source ends (a webhook answers 503 until it is stopped).
pub trait Publisher: Send + Sync + 'static {
    /// Queues `event` for the bus.
    ///
    /// # Errors
    ///
    /// The runtime will take no more events from this source.
    fn publish(&self, event: NewEvent) -> Result<(), PublisherClosed>;
}

/// What a source is started with besides its options.
#[derive(Clone)]
pub struct StartContext {
    pub publisher: Arc<dyn Publisher>,
    /// The `scheduledFor` of the last fire the log holds for this source,
    /// when it has fired before. Only timers read it, to apply their
    /// catch-up rule ([`timer`]); other sources ignore it.
    pub last_fired_ms: Option<u64>,
}

/// A kind of event source.
///
/// `start` validates the options and installs whatever the source listens
/// with (a file watch, a bound socket, the first timer deadline) before it
/// returns, so anything that happens after `start` returns is seen. Its
/// work runs on the current Tokio runtime and must observe the handle's
/// cancellation at every wait, so a stop ends it promptly; nothing it
/// starts outlives its handle. Starting a name again after its handle
/// stopped is a fresh start with nothing carried over.
pub trait Source {
    /// The source's options, as the runtime's configuration spells them.
    type Options: DeserializeOwned;

    /// A JSON Schema (draft 2020-12) of [`Source::Options`], for the
    /// runtime's validation and docs.
    fn options_schema() -> Value;

    /// Starts the source named `name`.
    ///
    /// # Errors
    ///
    /// `name` is not one topic level, the options are not usable (a path
    /// that does not exist, a remote bind without `allow_remote`), the
    /// listener could not be installed, or there is no current Tokio
    /// runtime.
    fn start(
        &self,
        name: &str,
        options: Self::Options,
        context: StartContext,
    ) -> anyhow::Result<SourceHandle>;
}

/// The kinds of source, by the name a configuration gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Timer,
    Watch,
    Webhook,
}

impl SourceKind {
    /// The JSON Schema of this kind's options.
    #[must_use]
    pub fn options_schema(self) -> Value {
        match self {
            SourceKind::Timer => TimerSource::<SystemClock>::options_schema(),
            SourceKind::Watch => WatchSource::options_schema(),
            SourceKind::Webhook => WebhookSource::options_schema(),
        }
    }

    /// Starts a source of this kind from its options as JSON; a timer runs
    /// on the system clock.
    ///
    /// # Errors
    ///
    /// The options do not deserialize, or [`Source::start`] failed.
    pub fn start(
        self,
        name: &str,
        options: Value,
        context: StartContext,
    ) -> anyhow::Result<SourceHandle> {
        let invalid = |error: serde_json::Error| {
            anyhow::anyhow!("invalid options for {self:?} source {name:?}: {error}")
        };
        match self {
            SourceKind::Timer => TimerSource::new().start(
                name,
                serde_json::from_value(options).map_err(invalid)?,
                context,
            ),
            SourceKind::Watch => WatchSource.start(
                name,
                serde_json::from_value(options).map_err(invalid)?,
                context,
            ),
            SourceKind::Webhook => WebhookSource.start(
                name,
                serde_json::from_value(options).map_err(invalid)?,
                context,
            ),
        }
    }
}

/// A running source. [`SourceHandle::stop`] ends it and reports how its
/// work ended; dropping the handle ends it too, without waiting.
#[derive(Debug)]
pub struct SourceHandle {
    cancel: CancellationToken,
    task: Option<JoinHandle<anyhow::Result<()>>>,
    local_addr: Option<SocketAddr>,
}

impl SourceHandle {
    /// The address a webhook listens on (with the port it was given, when
    /// it bound port 0); `None` for other sources.
    #[must_use]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Stops the source and waits for its work to end.
    ///
    /// # Errors
    ///
    /// The source had already ended with an error (its publisher closed, its
    /// listener failed), or its task panicked.
    pub async fn stop(mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        match task.await {
            Ok(ended) => ended,
            Err(failure) => Err(anyhow::anyhow!("source task failed: {failure}")),
        }
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Spawns a source's work on the current Tokio runtime; the work receives
/// the token its handle cancels.
fn spawn<F>(
    local_addr: Option<SocketAddr>,
    work: impl FnOnce(CancellationToken) -> F,
) -> anyhow::Result<SourceHandle>
where
    F: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|error| anyhow::anyhow!("a source starts on a Tokio runtime: {error}"))?;
    let cancel = CancellationToken::new();
    let task = runtime.spawn(work(cancel.clone()));
    Ok(SourceHandle {
        cancel,
        task: Some(task),
        local_addr,
    })
}

/// `topic`, the topic of the source `name`, which must be a single topic
/// level.
fn source_topic(name: &str, topic: String) -> anyhow::Result<Topic> {
    if name.contains('/') {
        anyhow::bail!("a source name is one topic level, without `/`: {name:?}");
    }
    Topic::new(topic).map_err(|error| anyhow::anyhow!("source name {name:?}: {error}"))
}

/// An event a source publishes on its own behalf.
fn source_event(topic: Topic, name: &str, payload: Value, trust: Trust) -> NewEvent {
    NewEvent {
        topic,
        source: name.to_string(),
        session: None,
        payload,
        trust,
        cause: None,
        reply: None,
    }
}

#[cfg(test)]
mod test_support;
