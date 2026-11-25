//! The kernel's `event-*` functions (pa-core's `EventBusController`) on the
//! runtime's bus: one bus for every session the runtime runs.
//!
//! The sources and subscriptions are pa-core's `EventBusState`, under its
//! rules (`EventBusState::apply`: the daemon's names, limits, schema checks,
//! ownership and refusals), kept in the agent dir's database as the daemon's
//! bus keeps its own (the `event-bus.json` partition of the agent dir; no
//! file is written).
//!
//! - A publication is a stored event on `source/<name>/published`: the
//!   payload as the schema checked it, from the source's owner (`source` its
//!   name, `session` its id), with an owner's trust.
//! - A subscription is a bus subscription named [`SUBSCRIPTION_PREFIX`] and
//!   its id, on `source/<name>/#` (or the topic filter it names), its filter
//!   the `when` (every key's value equal), routed to its session at a
//!   follow-up's or a steer's priority. Its task starts from the message the
//!   daemon delivers ([`BusEvents::deliver`]); a session's own events, and
//!   sessions this runtime does not run, get none.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use pa_core::agent_state::{
    event_delivery_message, held_event_bus_in, store_event_bus_in, EventBusChange, EventBusState,
    EventRequester, TopicSubscriptions,
};
use pa_core::session_engine::agent_messaging::{
    create_agent_session_message_prompt, AgentMessagePromptPayload,
};
use pa_core::session_engine::event_bus_host::EventBusController;
use pa_types::event_log::{
    AgentTarget, Event, NewEvent, Predicate, Priority, Route, Scope, Subscription, Topic,
    TopicFilter, Trust,
};
use pa_types::events::{EventBusRequest, EventDeliveryMode, EventSubscriptionRecord};
use serde_json::{json, Value};

use crate::lock;
use crate::session::Outbox;
use crate::subscriptions::Registry;

/// The file whose database partition keeps the bus's state, in the agent dir.
const EVENT_BUS_FILE_NAME: &str = "event-bus.json";

/// What names a kernel subscription's bus subscription: this, then its id.
pub(crate) const SUBSCRIPTION_PREFIX: &str = "event-bus/";

/// The runtime's event bus: its state, and the bus it lives on.
pub(crate) struct BusEvents {
    /// The file its partition is named by.
    path: PathBuf,
    outbox: Outbox,
    kept: Mutex<Kept>,
    /// The database and the bus's subscriptions, once the runtime has them.
    attached: OnceLock<Attached>,
}

struct Kept {
    state: EventBusState,
    /// The subscriptions on the bus, by id.
    installed: BTreeSet<String>,
    /// The sessions the runtime runs: the only ones a delivery reaches.
    open: HashSet<String>,
}

struct Attached {
    log: Arc<pa_log::Log>,
    registry: Arc<Mutex<Registry>>,
}

/// One session's `event.*` requests, as that session.
pub(crate) struct SessionEvents {
    pub(crate) events: Arc<BusEvents>,
    pub(crate) requester: EventRequester,
}

impl BusEvents {
    /// The bus of `agent_dir`'s runtime, publishing through `outbox`; it
    /// answers once [`BusEvents::attach`]ed.
    pub(crate) fn new(agent_dir: &Path, outbox: Outbox) -> Self {
        BusEvents {
            path: agent_dir.join(EVENT_BUS_FILE_NAME),
            outbox,
            kept: Mutex::new(Kept {
                state: EventBusState::default(),
                installed: BTreeSet::new(),
                open: HashSet::new(),
            }),
            attached: OnceLock::new(),
        }
    }

    /// Records that the runtime runs session `id`.
    pub(crate) fn opened(&self, id: &str) {
        lock(&self.kept).open.insert(id.to_string());
    }

    /// Records that the runtime no longer runs session `id`.
    pub(crate) fn closed(&self, id: &str) {
        lock(&self.kept).open.remove(id);
    }

    /// Reads the state `log` keeps (on the blocking pool) and puts its
    /// subscriptions on the bus through `registry`. State that cannot be read
    /// is logged, and the bus starts empty, as the daemon's does.
    pub(crate) async fn attach(&self, log: Arc<pa_log::Log>, registry: Arc<Mutex<Registry>>) {
        let held = {
            let (log, path) = (Arc::clone(&log), self.path.clone());
            tokio::task::spawn_blocking(move || held_event_bus_in(&log, &path)).await
        };
        let state = match held {
            Ok(Ok(state)) => state.unwrap_or_default(),
            Ok(Err(error)) => {
                tracing::warn!("the event bus state is unreadable ({error:#}); starting empty");
                EventBusState::default()
            }
            Err(error) => {
                tracing::warn!("reading the event bus state failed ({error}); starting empty");
                EventBusState::default()
            }
        };
        let mut kept = lock(&self.kept);
        kept.state = state;
        sync(&mut kept, &registry);
        // Attached once, by the runtime's open.
        let _ = self.attached.set(Attached { log, registry });
    }

    /// The message `event` delivers to the subscription `id`, counted as
    /// delivered; `None` when it delivers none: the subscription is gone,
    /// the event is its own session's, or the runtime does not run that
    /// session.
    ///
    /// # Errors
    ///
    /// The count could not be stored.
    pub(crate) async fn deliver(&self, id: &str, event: &Event) -> anyhow::Result<Option<String>> {
        let (message, stored) = {
            let mut kept = lock(&self.kept);
            let Kept { state, open, .. } = &mut *kept;
            let Some(record) = state
                .subscriptions
                .iter_mut()
                .find(|record| record.id == id)
            else {
                return Ok(None);
            };
            if !delivers(open, record, event.event.session.as_deref()) {
                return Ok(None);
            }
            record.delivered_count += 1;
            let topic = &event.event.topic;
            let levels: Vec<&str> = topic.levels().collect();
            let source = match levels.as_slice() {
                ["source", name, ..] => name,
                _ => topic.as_str(),
            };
            let message = create_agent_session_message_prompt(&AgentMessagePromptPayload {
                message: event_delivery_message(
                    source,
                    &event.id.0,
                    &event.event.source,
                    record,
                    &event.event.payload,
                ),
                sender_name: format!("event:{source}"),
                from_relationship: None,
            });
            let stored = self
                .attached
                .get()
                .map(|attached| store_event_bus_in(&attached.log, &self.path, state))
                .transpose()?;
            (message, stored)
        };
        if let Some(receipt) = stored {
            receipt.wait().await.context("store the event bus")?;
        }
        Ok(Some(message))
    }
}

impl EventBusController for SessionEvents {
    async fn request(&self, request: EventBusRequest) -> anyhow::Result<Value> {
        let events = &self.events;
        let attached = events
            .attached
            .get()
            .context("the run's event bus is still opening")?;
        let saves = !matches!(request, EventBusRequest::List);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let (change, stored) = {
            let mut kept = lock(&events.kept);
            let change =
                kept.state
                    .apply(&self.requester, request, now, TopicSubscriptions::Accepted)?;
            sync(&mut kept, &attached.registry);
            let stored = saves
                .then(|| store_event_bus_in(&attached.log, &events.path, &kept.state))
                .transpose()?;
            (change, stored)
        };
        if let Some(receipt) = stored {
            receipt.wait().await.context("store the event bus")?;
        }
        match change {
            EventBusChange::Answer(answer) => Ok(answer),
            EventBusChange::Publish { source, payload } => {
                let topic = Topic::new(format!("source/{}/published", source.name))
                    .context("the source's name is no topic level")?;
                let matched = {
                    let kept = lock(&events.kept);
                    kept.state
                        .subscriptions
                        .iter()
                        .filter(|record| {
                            delivers(&kept.open, record, Some(&self.requester.session_id))
                                && heard(record).is_ok_and(|(filter, when)| {
                                    filter.matches(&topic)
                                        && when.is_none_or(|when| when.holds(&payload))
                                })
                        })
                        .count()
                };
                let published = events
                    .outbox
                    .publish(NewEvent {
                        topic,
                        source: source.owner_name,
                        session: Some(self.requester.session_id.clone()),
                        payload,
                        trust: Trust::Owner,
                        cause: None,
                        reply: None,
                    })
                    .await?;
                Ok(json!({ "eventId": published.event.id.0, "matched": matched }))
            }
        }
    }
}

/// Whether an event from session `from` reaches `record`'s session: not its
/// own, and one the runtime runs.
fn delivers(open: &HashSet<String>, record: &EventSubscriptionRecord, from: Option<&str>) -> bool {
    from != Some(record.session_id.as_str()) && open.contains(&record.session_id)
}

/// The topics `record` hears (its source's, or the filter it names) and
/// the condition on their payloads.
fn heard(record: &EventSubscriptionRecord) -> anyhow::Result<(TopicFilter, Option<Predicate>)> {
    let filter = if record.source.contains('/') {
        TopicFilter::new(record.source.as_str())
    } else {
        TopicFilter::new(format!("source/{}/#", record.source))
    }
    .with_context(|| format!("subscription {} hears no topic", record.id))?;
    let when = (!record.filter.is_empty()).then(|| Predicate::All {
        all: record
            .filter
            .iter()
            .map(|(key, value)| Predicate::Eq {
                pointer: format!("/{}", key.replace('~', "~0").replace('/', "~1")),
                value: value.clone(),
            })
            .collect(),
    });
    Ok((filter, when))
}

/// Puts the bus's subscriptions in step with the state's: each one the state
/// gained is added, each one it lost removed.
fn sync(kept: &mut Kept, registry: &Mutex<Registry>) {
    let mut registry = lock(registry);
    let current: BTreeSet<String> = kept
        .state
        .subscriptions
        .iter()
        .map(|record| record.id.clone())
        .collect();
    for gone in kept.installed.difference(&current) {
        registry.remove(&format!("{SUBSCRIPTION_PREFIX}{gone}"));
    }
    for record in &kept.state.subscriptions {
        if kept.installed.contains(&record.id) {
            continue;
        }
        let added = heard(record).and_then(|(filter, when)| {
            registry
                .add(Subscription {
                    id: format!("{SUBSCRIPTION_PREFIX}{}", record.id),
                    scope: Scope::Global,
                    filter,
                    when,
                    throttle_ms: 0,
                    debounce_ms: 0,
                    route: Route::Agent {
                        target: AgentTarget::Session {
                            session: record.session_id.clone(),
                        },
                        prompt: record.prompt.clone(),
                        priority: match record.delivery_mode {
                            EventDeliveryMode::FollowUp => Priority::FollowUp,
                            EventDeliveryMode::Steer => Priority::Steer,
                        },
                    },
                    trust: Trust::Owner,
                })
                .map_err(anyhow::Error::from)
        });
        if let Err(error) = added {
            tracing::warn!(
                "event subscription {} is not on the bus: {error:#}",
                record.id
            );
        }
    }
    kept.installed = current;
}
