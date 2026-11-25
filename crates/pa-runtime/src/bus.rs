//! The bus: every event goes through `publish`, which stores it, fans it out
//! to listeners, and runs the routes of the subscriptions it matches.
//!
//! Fan-out is a bounded broadcast. Publishing never waits for a listener: a
//! listener that falls more than its capacity behind loses the oldest
//! events it has not read and is told how many ([`Heard::Lagged`]); it
//! catches up from the log with [`EventStore::since`] from the last
//! sequence number it saw. Listeners see events in sequence order.
//!
//! Routes run in the order their events were stored, breadth first. An
//! event a route causes is built with `Event::cause`, so it carries at most
//! its cause's trust (and the subscription's), and a chain of causes ends at
//! `MAX_HOPS`: the event that would pass it is dropped and
//! `system/loop_dropped` is published in its place, itself at the end of the
//! chain, so what it would cause is dropped without another report. A route
//! that fails publishes `system/route_failed`.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, Weak};

use pa_types::event_log::{Cause, Event, NewEvent, Route, Scope, Subscription, Topic};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use crate::lock;
use crate::routes::{ModelCall, ModelCaller, TaskRequest, TaskSink};
use crate::store::{EventStore, StateKey, StateStore, StateWrite};
use crate::subscriptions::{DebounceStep, DuplicateSubscription, Matched, Registry, SessionTree};

/// What the bus is built from.
pub struct BusParts<E, K, G, M, T> {
    pub events: E,
    pub state: K,
    pub sessions: G,
    pub model: M,
    pub tasks: T,
    /// How many events a listener may fall behind before it loses the
    /// oldest.
    pub listener_capacity: NonZeroUsize,
}

/// A handle on the bus; clones share it.
pub struct Bus<E, K, G, M, T> {
    inner: Arc<Inner<E, K, G, M, T>>,
}

impl<E, K, G, M, T> Clone for Bus<E, K, G, M, T> {
    fn clone(&self) -> Self {
        Bus {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<E, K, G, M, T> {
    events: E,
    state: K,
    sessions: G,
    model: M,
    tasks: T,
    registry: Mutex<Registry>,
    /// Held across an append and its fan-out, so listeners see sequence
    /// order.
    appending: tokio::sync::Mutex<()>,
    fan_out: broadcast::Sender<Arc<Event>>,
}

/// An event as `publish` stored it, and what its routes published.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub event: Event,
    /// Every event the routes it set off published, in the order stored.
    /// A debounced route runs later and is not here.
    pub caused: Vec<Event>,
}

/// What a [`Listener`] hears next.
#[derive(Debug, Clone, PartialEq)]
pub enum Heard {
    Event(Arc<Event>),
    /// The listener fell behind and lost this many events; the next one it
    /// hears is the oldest the bus still holds.
    Lagged {
        missed: u64,
    },
}

/// A live view of every event the bus stores from now on.
pub struct Listener {
    receiver: broadcast::Receiver<Arc<Event>>,
}

impl Listener {
    /// The next event, or how many were lost; `None` once the bus is gone.
    pub async fn recv(&mut self) -> Option<Heard> {
        match self.receiver.recv().await {
            Ok(event) => Some(Heard::Event(event)),
            Err(broadcast::error::RecvError::Lagged(missed)) => Some(Heard::Lagged { missed }),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

/// The bus takes no more events through this intake: it is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the bus no longer takes events")]
pub struct BusClosed;

/// A synchronous way onto the bus for code that cannot await (a source's
/// file-watch callback): `send` queues the event without blocking and a
/// task publishes queued events in order. It buffers without bound and
/// fails only once the bus is gone. `pa_sources::Publisher` is implemented
/// on this.
#[derive(Debug, Clone)]
pub struct Intake {
    sender: mpsc::UnboundedSender<NewEvent>,
}

impl Intake {
    /// Queues `event` for publishing.
    ///
    /// # Errors
    ///
    /// The bus is gone.
    pub fn send(&self, event: NewEvent) -> Result<(), BusClosed> {
        self.sender.send(event).map_err(|_| BusClosed)
    }
}

fn system_topic(name: &str) -> Topic {
    Topic::new(format!("system/{name}")).expect("system topics are valid")
}

impl<E, K, G, M, T> Bus<E, K, G, M, T>
where
    E: EventStore,
    K: StateStore,
    G: SessionTree,
    M: ModelCaller,
    T: TaskSink,
{
    /// A bus over `parts`, with no subscriptions.
    ///
    /// # Panics
    ///
    /// `listener_capacity` is above `usize::MAX / 2`.
    #[must_use]
    pub fn new(parts: BusParts<E, K, G, M, T>) -> Self {
        let (fan_out, _) = broadcast::channel(parts.listener_capacity.get());
        Bus {
            inner: Arc::new(Inner {
                events: parts.events,
                state: parts.state,
                sessions: parts.sessions,
                model: parts.model,
                tasks: parts.tasks,
                registry: Mutex::new(Registry::default()),
                appending: tokio::sync::Mutex::new(()),
                fan_out,
            }),
        }
    }

    /// Adds `subscription`; it hears events published from now on.
    ///
    /// # Errors
    ///
    /// A subscription with its id is already on the bus.
    pub fn add_subscription(
        &self,
        subscription: Subscription,
    ) -> Result<(), DuplicateSubscription> {
        lock(&self.inner.registry).add(subscription)
    }

    /// Removes the subscription `id`, with any match its debounce holds.
    #[must_use]
    pub fn remove_subscription(&self, id: &str) -> Option<Subscription> {
        lock(&self.inner.registry).remove(id)
    }

    /// The event log this bus appends to.
    #[must_use]
    pub fn events(&self) -> &E {
        &self.inner.events
    }

    /// A listener for every event stored from now on.
    #[must_use]
    pub fn listen(&self) -> Listener {
        Listener {
            receiver: self.inner.fan_out.subscribe(),
        }
    }

    /// An intake whose events this bus publishes. Its task holds the bus
    /// only while it publishes, and ends when every clone of the intake is
    /// dropped or the bus is.
    ///
    /// # Panics
    ///
    /// Called outside a Tokio runtime.
    #[must_use]
    pub fn intake(&self) -> Intake {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let bus = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let Some(inner) = bus.upgrade() else {
                    break;
                };
                if let Err(error) = (Bus { inner }).publish(event).await {
                    tracing::error!("an event from an intake was not published: {error:#}");
                }
            }
        });
        Intake { sender }
    }

    /// Stores `event`, fans it out, and runs the routes it sets off.
    ///
    /// # Errors
    ///
    /// The store failed to append `event` or an event a route caused.
    #[tracing::instrument(level = "debug", skip_all, fields(topic = %event.topic))]
    pub async fn publish(&self, event: NewEvent) -> anyhow::Result<Published> {
        let event = self.store(event).await?;
        let fires = self.matches(&event);
        let caused = self.cascade(fires).await?;
        Ok(Published {
            event: Arc::unwrap_or_clone(event),
            caused,
        })
    }

    async fn store(&self, event: NewEvent) -> anyhow::Result<Arc<Event>> {
        let _appending = self.inner.appending.lock().await;
        let event = Arc::new(self.inner.events.append(event).await?);
        // No listener is not an error: the event is in the log.
        let _ = self.inner.fan_out.send(Arc::clone(&event));
        Ok(event)
    }

    /// The routes `event` runs now; starts the debounce timers it arms.
    fn matches(&self, event: &Arc<Event>) -> Vec<(Arc<Subscription>, Arc<Event>)> {
        let matched = lock(&self.inner.registry).matching(event, &self.inner.sessions);
        let mut fires = Vec::new();
        for matched in matched {
            match matched {
                Matched::Fire(subscription) => fires.push((subscription, Arc::clone(event))),
                Matched::Debounce { id, serial } => {
                    tokio::spawn(debounce(Arc::downgrade(&self.inner), id, serial));
                }
            }
        }
        fires
    }

    /// Runs `fires` and every route the events they cause set off.
    async fn cascade(
        &self,
        fires: Vec<(Arc<Subscription>, Arc<Event>)>,
    ) -> anyhow::Result<Vec<Event>> {
        let mut pending = VecDeque::from(fires);
        let mut caused = Vec::new();
        while let Some((subscription, event)) = pending.pop_front() {
            let Some(next) = self.run_route(&subscription, &event).await else {
                continue;
            };
            let stored = self.store(next).await?;
            pending.extend(self.matches(&stored));
            caused.push((*stored).clone());
        }
        Ok(caused)
    }

    /// Runs `subscription`'s route for `event`; the event it publishes, if
    /// any.
    async fn run_route(&self, subscription: &Subscription, event: &Arc<Event>) -> Option<NewEvent> {
        let trust = subscription.trust.lowest(event.event.trust);
        let failed = match &subscription.route {
            Route::Set { key } => {
                let session = match &subscription.scope {
                    Scope::Local { session } | Scope::Agent { session } => Some(session.clone()),
                    Scope::Global => None,
                };
                let write = StateWrite {
                    key: StateKey {
                        session,
                        key: key.clone(),
                    },
                    value: event.event.payload.clone(),
                    trust,
                    cause: event.id.clone(),
                };
                self.inner.state.set(write).await.err()
            }
            Route::Publish { topic } => {
                return caused(subscription, event, topic, event.event.payload.clone());
            }
            Route::Call { prompt, model } => {
                let call = ModelCall {
                    subscription: subscription.id.clone(),
                    prompt: prompt.clone(),
                    model: model.clone(),
                    event: Arc::clone(event),
                    trust,
                };
                self.inner.model.call(call).await.err()
            }
            Route::Agent {
                target,
                prompt,
                priority,
            } => {
                let request = TaskRequest {
                    subscription: subscription.id.clone(),
                    target: target.clone(),
                    prompt: prompt.clone(),
                    priority: *priority,
                    event: Arc::clone(event),
                    trust,
                };
                self.inner.tasks.queue(request).await.err()
            }
        };
        failed.and_then(|error| {
            let payload = json!({
                "subscription": subscription.id,
                "error": format!("{error:#}"),
            });
            caused(subscription, event, &system_topic("route_failed"), payload)
        })
    }
}

/// The event `subscription`'s route publishes on `topic` for `event`, in
/// `event`'s session; `system/loop_dropped` in its place when the chain of
/// causes is full, and nothing when `event` is itself such a report.
fn caused(
    subscription: &Subscription,
    event: &Event,
    topic: &Topic,
    payload: serde_json::Value,
) -> Option<NewEvent> {
    let id = &subscription.id;
    let source = format!("subscription:{id}");
    match event.cause(topic.clone(), source, payload, subscription.trust) {
        Ok(next) => Some(NewEvent {
            session: event.event.session.clone(),
            ..next
        }),
        Err(limit) => {
            let loop_dropped = system_topic("loop_dropped");
            if event.event.topic == loop_dropped {
                tracing::warn!("{limit}: subscription {id:?} on a loop report");
                return None;
            }
            Some(NewEvent {
                topic: loop_dropped,
                source: "runtime".to_string(),
                session: event.event.session.clone(),
                payload: json!({
                    "cause": event.id.0,
                    "topic": topic.as_str(),
                    "subscription": id,
                }),
                trust: subscription.trust.lowest(event.event.trust),
                // At the end of the chain, so nothing it causes is published.
                cause: Some(Cause {
                    id: event.id.clone(),
                    trust: event.event.trust,
                    hops: event.hops(),
                }),
                reply: None,
            })
        }
    }
}

/// Runs a debounced route once its subscription has been quiet long enough.
async fn debounce<E, K, G, M, T>(bus: Weak<Inner<E, K, G, M, T>>, id: String, serial: u64)
where
    E: EventStore,
    K: StateStore,
    G: SessionTree,
    M: ModelCaller,
    T: TaskSink,
{
    loop {
        let Some(inner) = bus.upgrade() else {
            return;
        };
        let step = lock(&inner.registry).debounce_step(&id, serial);
        match step {
            DebounceStep::Wait(deadline) => {
                drop(inner);
                tokio::time::sleep_until(deadline).await;
            }
            DebounceStep::Fire(subscription, event) => {
                if let Err(error) = (Bus { inner }).cascade(vec![(subscription, event)]).await {
                    tracing::error!("debounced subscription {id:?} failed to publish: {error:#}");
                }
                return;
            }
            DebounceStep::Gone => return,
        }
    }
}

#[cfg(test)]
mod tests;
