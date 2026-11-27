//! The subscriptions the bus matches events against, and the gates in front
//! of their routes: scope, `when`, throttle and debounce.
//!
//! Throttle lets through at most one match per window: a match inside the
//! window after the one let through is dropped, not queued. Debounce holds a
//! match and replaces it with each later one; the route runs once, with the
//! last, when no match has followed for the debounce time. With both, the
//! throttle decides which matches reach the debounce. Both run on Tokio's
//! clock, so tests drive them with paused time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_types::event_log::{Event, Scope, Subscription, TopicFilter, TopicTrie};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::lock;

/// Which sessions are subagents of which, for `Agent`-scope subscriptions.
///
/// The bus asks while it matches an event, holding its subscription lock:
/// answer from memory without blocking, and never call back into the bus.
pub trait SessionTree: Send + Sync + 'static {
    /// Whether `session` is `ancestor` itself or a subagent spawned below
    /// it, at any depth.
    fn descends_from(&self, session: &str, ancestor: &str) -> bool;
}

/// A session tree in memory, for tests and headless runs. Clones share the
/// same tree.
#[derive(Debug, Clone, Default)]
pub struct MemorySessionTree {
    parents: Arc<Mutex<HashMap<String, String>>>,
}

impl MemorySessionTree {
    /// Records that `child` was spawned by `parent`.
    pub fn insert(&self, child: impl Into<String>, parent: impl Into<String>) {
        lock(&self.parents).insert(child.into(), parent.into());
    }
}

impl SessionTree for MemorySessionTree {
    fn descends_from(&self, session: &str, ancestor: &str) -> bool {
        let parents = lock(&self.parents);
        let mut current = session;
        // A walk longer than the tree is a cycle: stop rather than loop.
        for _ in 0..=parents.len() {
            if current == ancestor {
                return true;
            }
            match parents.get(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
        false
    }
}

/// A subscription with this id is already on the bus.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a subscription {0:?} already exists")]
pub struct DuplicateSubscription(pub String);

/// What matching one event against one subscription asks of the bus.
pub(crate) enum Matched {
    /// Run the route now.
    Fire(Arc<Subscription>),
    /// Start the debounce timer of the subscription added as `serial`.
    Debounce { id: String, serial: u64 },
}

/// Where a debounce timer stands when it wakes.
pub(crate) enum DebounceStep {
    /// A later match moved the deadline.
    Wait(Instant),
    /// Quiet long enough: run the route with the last match.
    Fire(Arc<Subscription>, Arc<Event>),
    /// The subscription was removed (or replaced).
    Gone,
}

struct Entry {
    subscription: Arc<Subscription>,
    /// Which addition this is, so a timer of a removed subscription never
    /// fires one re-added under the same id.
    serial: u64,
    last_let_through: Option<Instant>,
    held: Option<Held>,
}

struct Held {
    event: Arc<Event>,
    deadline: Instant,
}

/// A subscription that came onto the bus or left it, as the registry tells
/// its observer (the runtime's sources, [`crate::sources`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SubscriptionChange {
    Added { id: String, filter: TopicFilter },
    Removed { id: String },
}

#[derive(Default)]
pub(crate) struct Registry {
    trie: TopicTrie<String>,
    entries: HashMap<String, Entry>,
    next_serial: u64,
    /// Told of every subscription added and removed, without blocking: the
    /// registry's lock is held while it is told.
    observer: Option<mpsc::UnboundedSender<SubscriptionChange>>,
}

impl Registry {
    /// Tells `observer` of every subscription added and removed from now on,
    /// after telling it of each one on the bus now.
    pub(crate) fn observe(&mut self, observer: mpsc::UnboundedSender<SubscriptionChange>) {
        self.observer = Some(observer);
        let current: Vec<SubscriptionChange> = self
            .entries
            .values()
            .map(|entry| SubscriptionChange::Added {
                id: entry.subscription.id.clone(),
                filter: entry.subscription.filter.clone(),
            })
            .collect();
        for change in current {
            self.tell(change);
        }
    }

    fn tell(&self, change: SubscriptionChange) {
        if let Some(observer) = &self.observer {
            // The observer ends only with the runtime that owns this bus.
            let _ = observer.send(change);
        }
    }

    pub(crate) fn add(&mut self, subscription: Subscription) -> Result<(), DuplicateSubscription> {
        if self.entries.contains_key(&subscription.id) {
            return Err(DuplicateSubscription(subscription.id));
        }
        self.trie
            .insert(&subscription.filter, subscription.id.clone());
        self.next_serial += 1;
        let added = SubscriptionChange::Added {
            id: subscription.id.clone(),
            filter: subscription.filter.clone(),
        };
        self.entries.insert(
            subscription.id.clone(),
            Entry {
                subscription: Arc::new(subscription),
                serial: self.next_serial,
                last_let_through: None,
                held: None,
            },
        );
        self.tell(added);
        Ok(())
    }

    pub(crate) fn remove(&mut self, id: &str) -> Option<Subscription> {
        let entry = self.entries.remove(id)?;
        self.trie
            .remove(&entry.subscription.filter, &entry.subscription.id);
        self.tell(SubscriptionChange::Removed { id: id.to_string() });
        Some(Arc::unwrap_or_clone(entry.subscription))
    }

    /// What `event` asks of each subscription it matches, in subscription
    /// id order.
    pub(crate) fn matching(
        &mut self,
        event: &Arc<Event>,
        sessions: &impl SessionTree,
    ) -> Vec<Matched> {
        let now = Instant::now();
        let mut ids: Vec<String> = self
            .trie
            .matches(&event.event.topic)
            .into_iter()
            .cloned()
            .collect();
        ids.sort();
        let mut matched = Vec::new();
        for id in ids {
            let Some(entry) = self.entries.get_mut(&id) else {
                continue;
            };
            let subscription = &entry.subscription;
            let heard = match &subscription.scope {
                Scope::Local { session } => event.event.session.as_ref() == Some(session),
                Scope::Agent { session } => event
                    .event
                    .session
                    .as_deref()
                    .is_some_and(|from| sessions.descends_from(from, session)),
                Scope::Global => true,
            };
            let wanted = subscription
                .when
                .as_ref()
                .is_none_or(|when| when.holds(&event.event.payload));
            if !heard || !wanted {
                continue;
            }
            if subscription.throttle_ms > 0 {
                let window = Duration::from_millis(subscription.throttle_ms);
                if entry
                    .last_let_through
                    .is_some_and(|last| now < last + window)
                {
                    continue;
                }
                entry.last_let_through = Some(now);
            }
            if subscription.debounce_ms == 0 {
                matched.push(Matched::Fire(Arc::clone(subscription)));
                continue;
            }
            let deadline = now + Duration::from_millis(subscription.debounce_ms);
            let timer_running = entry.held.is_some();
            entry.held = Some(Held {
                event: Arc::clone(event),
                deadline,
            });
            if !timer_running {
                matched.push(Matched::Debounce {
                    id,
                    serial: entry.serial,
                });
            }
        }
        matched
    }

    /// Where the debounce timer of subscription `id`, added as `serial`,
    /// stands now.
    pub(crate) fn debounce_step(&mut self, id: &str, serial: u64) -> DebounceStep {
        let Some(entry) = self
            .entries
            .get_mut(id)
            .filter(|entry| entry.serial == serial)
        else {
            return DebounceStep::Gone;
        };
        let Some(held) = entry.held.take_if(|held| held.deadline <= Instant::now()) else {
            return entry
                .held
                .as_ref()
                .map_or(DebounceStep::Gone, |held| DebounceStep::Wait(held.deadline));
        };
        DebounceStep::Fire(Arc::clone(&entry.subscription), held.event)
    }
}
