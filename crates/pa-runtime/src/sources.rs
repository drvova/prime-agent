//! The runtime's event sources (`pa-sources`): their definitions, kept in
//! the agent's database, and the sources running for the subscriptions that
//! hear them.
//!
//! - **Definitions** are rows of the `sources` table in the partition
//!   [`SOURCES_PARTITION`], one per source, its name the row's id, its kind
//!   the row's, and `{"options", "owner"}` its spec. pa-core's scheduled
//!   jobs are timer rows too, under `cron:<path>` scopes, and the kernel's
//!   bus sources under its `event-bus.json` partition: the scopes never
//!   meet. A timer's row keeps the `firedAt` of its last fire in its claim
//!   (`{"lastFiredMs"}`), so a timer started again (in this run or after a
//!   restart) catches up under pa-sources' rule: missed fires collapse into
//!   one. Webhook definitions are refused until the daemon hosts them.
//! - **Running**: a source runs while at least one bus subscription's
//!   filter matches a topic it publishes (`timer/<name>`;
//!   `source/<name>/changed` and `source/<name>/error` for a watch): it
//!   starts when the first such subscription is added and stops when the
//!   last is removed. The bus's registry tells the manager of every
//!   subscription added and removed without blocking
//!   ([`SubscriptionChange`]), and the manager's own task starts and stops
//!   the sources, so no subscription path waits on a source.
//! - **Events** go onto the bus through its [`Intake`], which never blocks.
//!   A timer's fire also hands its `firedAt` to the manager, which stores
//!   the claim through the database's writer without waiting for it; the
//!   timer never waits on the database.
//! - **Shutdown** stops every source, waits for each, and stores the last
//!   claims before it answers. Nothing else waits on a source: a run's
//!   settle is its sessions'.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Context as _;
use pa_log::{Log, Origin, Receipt, SourceRow, SourceSet};
use pa_sources::{
    Clock, Publisher, PublisherClosed, Source, SourceHandle, SourceKind, StartContext,
    TimerOptions, TimerSource, WatchOptions,
};
use pa_types::event_log::{NewEvent, Topic, TopicFilter};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::bus::{Bus, Intake};
use crate::lock;
use crate::routes::{ModelCaller, TaskSink};
use crate::store::{EventStore, StateStore};
use crate::subscriptions::{SessionTree, SubscriptionChange};

/// The partition of the agent's database the runtime's source definitions
/// are kept in.
pub(crate) const SOURCES_PARTITION: &str = "runtime-sources";

/// Why a webhook is neither defined nor started.
const WEBHOOKS_REFUSED: &str = "webhook sources arrive with the daemon";

/// A source the runtime can run: a timer or a folder watch, by name.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceDefinition {
    /// One topic level: the source publishes on `timer/<name>` or
    /// `source/<name>/...`.
    pub name: String,
    pub kind: SourceKind,
    /// The kind's options, as `SourceKind::options_schema` describes them.
    /// A timer's events are Owner; a watch's carry its `trust` option.
    pub options: Value,
    /// The session that defined it, when one did.
    pub owner: Option<String>,
}

/// A definition as the manager holds it.
#[derive(Debug, Clone, PartialEq)]
struct Defined {
    definition: SourceDefinition,
    /// The topics it publishes on: what a subscription must match.
    topics: Vec<Topic>,
    /// The `firedAt` of a timer's last fire.
    last_fired_ms: Option<u64>,
}

/// The topics `definition`'s source publishes on, once its name and
/// options are checked.
fn checked(definition: &SourceDefinition) -> anyhow::Result<Vec<Topic>> {
    let name = &definition.name;
    if name.contains('/') {
        anyhow::bail!("a source name is one topic level, without `/`: {name:?}");
    }
    let topic = |text: String| {
        Topic::new(text).map_err(|error| anyhow::anyhow!("source name {name:?}: {error}"))
    };
    let options = definition.options.clone();
    match definition.kind {
        SourceKind::Timer => {
            serde_json::from_value::<TimerOptions>(options)
                .map_err(|error| anyhow::anyhow!("timer {name:?}: {error}"))?;
            Ok(vec![topic(format!("timer/{name}"))?])
        }
        SourceKind::Watch => {
            serde_json::from_value::<WatchOptions>(options)
                .map_err(|error| anyhow::anyhow!("watch {name:?}: {error}"))?;
            Ok(vec![
                topic(format!("source/{name}/changed"))?,
                topic(format!("source/{name}/error"))?,
            ])
        }
        SourceKind::Webhook => {
            anyhow::bail!("webhook {name:?}: {WEBHOOKS_REFUSED}")
        }
    }
}

/// The definitions the agent's database keeps. A row that cannot be read
/// is logged and left out.
///
/// # Errors
///
/// The database could not be read.
fn load(log: &Log) -> anyhow::Result<Vec<Defined>> {
    let set = log
        .sources(SOURCES_PARTITION)
        .context("read the runtime's sources")?;
    let mut defined = Vec::with_capacity(set.sources.len());
    for row in set.sources {
        let read = (|| {
            let kind = match row.kind {
                pa_log::SourceKind::Timer => SourceKind::Timer,
                pa_log::SourceKind::Watch => SourceKind::Watch,
                pa_log::SourceKind::Webhook => SourceKind::Webhook,
                pa_log::SourceKind::Bus => anyhow::bail!("a bus source is not the runtime's"),
            };
            let spec: Value = serde_json::from_str(&row.spec)?;
            let definition = SourceDefinition {
                name: row.id.clone(),
                kind,
                options: spec.get("options").cloned().context("it has no options")?,
                owner: spec
                    .get("owner")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            let claim: Option<Value> =
                row.claim.as_deref().map(serde_json::from_str).transpose()?;
            anyhow::Ok(Defined {
                topics: checked(&definition)?,
                definition,
                last_fired_ms: claim.and_then(|claim| claim.get("lastFiredMs")?.as_u64()),
            })
        })();
        match read {
            Ok(read) => defined.push(read),
            Err(error) => tracing::warn!("source {:?} is not run: {error:#}", row.id),
        }
    }
    Ok(defined)
}

/// The runtime's sources: a handle on the task that runs them.
pub(crate) struct SourceManager {
    commands: mpsc::UnboundedSender<Command>,
    running: watch::Receiver<BTreeSet<String>>,
}

enum Command {
    Define {
        definition: SourceDefinition,
        topics: Vec<Topic>,
        answer: oneshot::Sender<anyhow::Result<Option<Receipt<bool>>>>,
    },
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

/// A timer fired at `fired_at`.
struct Fired {
    name: String,
    fired_at: u64,
}

/// A timer's publisher: the bus's intake, and the manager told of each
/// fire so it keeps the claim.
struct TimerFires {
    name: String,
    intake: Intake,
    fired: mpsc::UnboundedSender<Fired>,
}

impl Publisher for TimerFires {
    fn publish(&self, event: NewEvent) -> Result<(), PublisherClosed> {
        if let Some(fired_at) = event.payload.get("firedAt").and_then(Value::as_u64) {
            let fired = Fired {
                name: self.name.clone(),
                fired_at,
            };
            self.fired.send(fired).map_err(|_| PublisherClosed)?;
        }
        self.intake.publish(event)
    }
}

impl SourceManager {
    /// Runs the sources `log` defines for `bus`'s subscriptions from now on
    /// (and those on it now), timers on `clock`. The definitions are read on
    /// the blocking pool before anything else is done; ones that cannot be
    /// read are logged, and the sources start empty, as the kernel's bus
    /// does.
    ///
    /// # Panics
    ///
    /// Called outside a Tokio runtime.
    pub(crate) fn spawn<E, K, G, M, T, C>(
        log: Arc<Log>,
        bus: &Bus<E, K, G, M, T>,
        clock: Arc<C>,
    ) -> Self
    where
        E: EventStore,
        K: StateStore,
        G: SessionTree,
        M: ModelCaller,
        T: TaskSink,
        C: Clock,
    {
        let (observer, changes) = mpsc::unbounded_channel();
        lock(&bus.registry()).observe(observer);
        let (commands, commanded) = mpsc::unbounded_channel();
        let (on_fire, timer_fires) = mpsc::unbounded_channel();
        let (shown, running) = watch::channel(BTreeSet::new());
        let sources = Sources {
            log,
            intake: bus.intake(),
            clock,
            defined: BTreeMap::new(),
            filters: HashMap::new(),
            handles: BTreeMap::new(),
            on_fire,
            shown,
        };
        tokio::spawn(sources.run(commanded, changes, timer_fires));
        SourceManager { commands, running }
    }

    /// Defines `definition`'s source and stores it; it runs once a
    /// subscription hears it. Defining a name again with the same
    /// definition changes nothing.
    ///
    /// # Errors
    ///
    /// The name is not one topic level, the options are not the kind's, the
    /// kind is a webhook, the name is defined already with another
    /// definition, the definition could not be stored, or the runtime is
    /// shutting down.
    pub(crate) async fn define(&self, definition: SourceDefinition) -> anyhow::Result<()> {
        let topics = checked(&definition)?;
        let (answer, answered) = oneshot::channel();
        let stopped = || anyhow::anyhow!("the runtime's sources have stopped");
        self.commands
            .send(Command::Define {
                definition,
                topics,
                answer,
            })
            .map_err(|_| stopped())?;
        let stored = answered.await.map_err(|_| stopped())??;
        if let Some(stored) = stored {
            stored.wait().await.context("store the source definition")?;
        }
        Ok(())
    }

    /// The names of the sources running, as it changes.
    pub(crate) fn running(&self) -> watch::Receiver<BTreeSet<String>> {
        self.running.clone()
    }

    /// Stops every source and stores the last claims.
    pub(crate) async fn shutdown(&self) {
        let (done, stopped) = oneshot::channel();
        if self.commands.send(Command::Shutdown { done }).is_ok() {
            // Gone already: there is nothing left to stop.
            let _ = stopped.await;
        }
    }
}

/// What the manager's task owns.
struct Sources<C> {
    log: Arc<Log>,
    intake: Intake,
    clock: Arc<C>,
    defined: BTreeMap<String, Defined>,
    /// The bus's subscriptions' filters, by subscription id.
    filters: HashMap<String, TopicFilter>,
    handles: BTreeMap<String, SourceHandle>,
    on_fire: mpsc::UnboundedSender<Fired>,
    shown: watch::Sender<BTreeSet<String>>,
}

impl<C: Clock> Sources<C> {
    /// Runs until the manager shuts down or is dropped; dropping it drops
    /// the handles, which cancels their sources.
    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut changes: mpsc::UnboundedReceiver<SubscriptionChange>,
        mut timer_fires: mpsc::UnboundedReceiver<Fired>,
    ) {
        let log = Arc::clone(&self.log);
        match tokio::task::spawn_blocking(move || load(&log)).await {
            Ok(Ok(defined)) => {
                self.defined = defined
                    .into_iter()
                    .map(|defined| (defined.definition.name.clone(), defined))
                    .collect();
            }
            Ok(Err(error)) => {
                tracing::warn!("the runtime's sources are unreadable ({error:#}); starting empty");
            }
            Err(error) => {
                tracing::warn!("reading the runtime's sources failed ({error}); starting empty");
            }
        }
        let mut observing = true;
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None => return,
                    Some(Command::Define { definition, topics, answer }) => {
                        // A defining caller that went away does not undo it.
                        let _ = answer.send(self.define(definition, topics));
                        self.reconcile().await;
                    }
                    Some(Command::Shutdown { done }) => {
                        for (name, handle) in std::mem::take(&mut self.handles) {
                            if let Err(error) = handle.stop().await {
                                tracing::warn!("source {name:?} ended with an error: {error:#}");
                            }
                        }
                        self.shown.send_replace(BTreeSet::new());
                        while let Ok(fire) = timer_fires.try_recv() {
                            self.record(&fire);
                        }
                        if let Err(error) = self.store().wait().await {
                            tracing::warn!("the sources' last fires were not stored: {error:#}");
                        }
                        // A caller that went away does not need the answer.
                        let _ = done.send(());
                        return;
                    }
                },
                change = changes.recv(), if observing => {
                    match change {
                        None => observing = false,
                        Some(SubscriptionChange::Added { id, filter }) => {
                            self.filters.insert(id, filter);
                        }
                        Some(SubscriptionChange::Removed { id }) => {
                            self.filters.remove(&id);
                        }
                    }
                    self.reconcile().await;
                }
                Some(fire) = timer_fires.recv() => {
                    self.record(&fire);
                    let stored = self.store();
                    tokio::spawn(async move {
                        if let Err(error) = stored.wait().await {
                            tracing::warn!("a timer's fire was not stored: {error:#}");
                        }
                    });
                }
            }
        }
    }

    fn define(
        &mut self,
        definition: SourceDefinition,
        topics: Vec<Topic>,
    ) -> anyhow::Result<Option<Receipt<bool>>> {
        if let Some(defined) = self.defined.get(&definition.name) {
            if defined.definition == definition {
                return Ok(None);
            }
            anyhow::bail!(
                "a source {:?} is defined already, differently",
                definition.name
            );
        }
        self.defined.insert(
            definition.name.clone(),
            Defined {
                definition,
                topics,
                last_fired_ms: None,
            },
        );
        Ok(Some(self.store()))
    }

    fn record(&mut self, fired: &Fired) {
        if let Some(defined) = self.defined.get_mut(&fired.name) {
            defined.last_fired_ms = Some(fired.fired_at);
        }
    }

    /// Stores every definition, with its claim, through the database's
    /// writer.
    fn store(&self) -> Receipt<bool> {
        let sources = self
            .defined
            .values()
            .map(|defined| SourceRow {
                id: defined.definition.name.clone(),
                kind: match defined.definition.kind {
                    SourceKind::Timer => pa_log::SourceKind::Timer,
                    SourceKind::Watch => pa_log::SourceKind::Watch,
                    SourceKind::Webhook => pa_log::SourceKind::Webhook,
                },
                spec: json!({
                    "options": defined.definition.options,
                    "owner": defined.definition.owner,
                })
                .to_string(),
                claim: defined
                    .last_fired_ms
                    .map(|fired| json!({ "lastFiredMs": fired }).to_string()),
            })
            .collect();
        let set = SourceSet {
            sources,
            subscriptions: Vec::new(),
        };
        self.log
            .replace_sources(SOURCES_PARTITION, set, Origin::Written)
    }

    /// Starts each source a subscription hears that is not running, and
    /// stops each running one none hears any more.
    async fn reconcile(&mut self) {
        let heard: BTreeSet<String> = self
            .defined
            .values()
            .filter(|defined| {
                defined
                    .topics
                    .iter()
                    .any(|topic| self.filters.values().any(|filter| filter.matches(topic)))
            })
            .map(|defined| defined.definition.name.clone())
            .collect();
        let unheard: Vec<String> = self
            .handles
            .keys()
            .filter(|name| !heard.contains(*name))
            .cloned()
            .collect();
        for name in unheard {
            if let Some(handle) = self.handles.remove(&name) {
                if let Err(error) = handle.stop().await {
                    tracing::warn!("source {name:?} ended with an error: {error:#}");
                }
            }
        }
        for name in heard {
            let Some(defined) = self.defined.get(&name) else {
                continue;
            };
            if self.handles.contains_key(&name) {
                continue;
            }
            let options = defined.definition.options.clone();
            let started = match defined.definition.kind {
                SourceKind::Timer => serde_json::from_value(options)
                    .map_err(anyhow::Error::from)
                    .and_then(|options| {
                        let publisher = TimerFires {
                            name: name.clone(),
                            intake: self.intake.clone(),
                            fired: self.on_fire.clone(),
                        };
                        TimerSource::with_clock(Arc::clone(&self.clock)).start(
                            &name,
                            options,
                            StartContext {
                                publisher: Arc::new(publisher),
                                last_fired_ms: defined.last_fired_ms,
                            },
                        )
                    }),
                SourceKind::Watch => SourceKind::Watch.start(
                    &name,
                    options,
                    StartContext {
                        publisher: Arc::new(self.intake.clone()),
                        last_fired_ms: None,
                    },
                ),
                SourceKind::Webhook => Err(anyhow::anyhow!("webhook {name:?}: {WEBHOOKS_REFUSED}")),
            };
            match started {
                Ok(handle) => {
                    self.handles.insert(name, handle);
                }
                Err(error) => tracing::warn!("source {name:?} did not start: {error:#}"),
            }
        }
        self.shown
            .send_replace(self.handles.keys().cloned().collect());
    }
}

#[cfg(test)]
mod tests;
