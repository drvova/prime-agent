//! Which connections follow which sessions ([`Attached`]): a connection is
//! attached to a session while one of its subscriptions' filters matches
//! the session's `agent/<id>/done`, from its `subscribe` until its
//! `unsubscribe` or its end. The session list counts them as the session's
//! attached clients.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pa_types::event_log::{Topic, TopicFilter};

use crate::lock;

/// The live subscriptions of every connection, by connection.
#[derive(Debug, Default)]
pub(crate) struct Attached {
    next: AtomicU64,
    connections: Mutex<HashMap<u64, HashMap<String, TopicFilter>>>,
}

impl Attached {
    /// A new connection's handle; dropping it forgets its subscriptions.
    pub(super) fn connect(self: &Arc<Self>) -> Connection {
        Connection {
            attached: Arc::clone(self),
            id: self.next.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// How many connections follow session `id`.
    pub(crate) fn clients_of(&self, id: &str) -> usize {
        let Ok(done) = Topic::new(format!("agent/{id}/done")) else {
            return 0;
        };
        lock(&self.connections)
            .values()
            .filter(|filters| filters.values().any(|filter| filter.matches(&done)))
            .count()
    }
}

/// One connection's place in [`Attached`].
pub(super) struct Connection {
    attached: Arc<Attached>,
    id: u64,
}

impl Connection {
    /// The connection's id, as its calls and its end name it.
    pub(super) fn id(&self) -> super::ConnectionId {
        super::ConnectionId(self.id)
    }

    pub(super) fn subscribed(&self, sub_id: &str, filter: &TopicFilter) {
        lock(&self.attached.connections)
            .entry(self.id)
            .or_default()
            .insert(sub_id.to_string(), filter.clone());
    }

    pub(super) fn unsubscribed(&self, sub_id: &str) {
        let mut connections = lock(&self.attached.connections);
        if let Some(filters) = connections.get_mut(&self.id) {
            filters.remove(sub_id);
            if filters.is_empty() {
                connections.remove(&self.id);
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        lock(&self.attached.connections).remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection counts once however many of its filters match, and not
    /// once it unsubscribed or ended.
    #[test]
    fn a_connection_is_attached_while_a_filter_of_it_matches() {
        let attached = Arc::new(Attached::default());
        let first = attached.connect();
        let second = attached.connect();
        first.subscribed("a", &TopicFilter::new("agent/s1/#").unwrap());
        first.subscribed("b", &TopicFilter::new("agent/+/done").unwrap());
        second.subscribed("c", &TopicFilter::new("agent/s2/#").unwrap());
        let counts = |attached: &Attached| (attached.clients_of("s1"), attached.clients_of("s2"));
        let subscribed = counts(&attached);
        second.unsubscribed("c");
        let unsubscribed = counts(&attached);
        drop(first);
        assert_eq!(
            (subscribed, unsubscribed, counts(&attached)),
            ((1, 2), (1, 1), (0, 0))
        );
    }
}
