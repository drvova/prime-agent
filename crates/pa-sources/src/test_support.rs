//! A publisher the tests read back, with bounded waits.

use std::sync::Arc;
use std::time::Duration;

use pa_types::event_log::NewEvent;
use tokio::sync::mpsc;

use crate::{Publisher, PublisherClosed, StartContext};

/// The longest a test waits for something it expects.
pub(crate) const PATIENCE: Duration = Duration::from_secs(20);

struct ChannelPublisher(mpsc::UnboundedSender<NewEvent>);

impl Publisher for ChannelPublisher {
    fn publish(&self, event: NewEvent) -> Result<(), PublisherClosed> {
        self.0.send(event).map_err(|_| PublisherClosed)
    }
}

/// What the tests publish into.
pub(crate) struct Published(mpsc::UnboundedReceiver<NewEvent>);

impl Published {
    /// The next published event, or a panic after [`PATIENCE`].
    pub(crate) async fn next(&mut self) -> NewEvent {
        tokio::time::timeout(PATIENCE, self.0.recv())
            .await
            .expect("an event within the test's patience")
            .expect("the source's publisher is open")
    }

    /// The events published so far, without waiting.
    pub(crate) fn drain(&mut self) -> Vec<NewEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.0.try_recv() {
            events.push(event);
        }
        events
    }
}

/// A start context whose publisher the returned [`Published`] reads.
pub(crate) fn context(last_fired_ms: Option<u64>) -> (StartContext, Published) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let context = StartContext {
        publisher: Arc::new(ChannelPublisher(sender)),
        last_fired_ms,
    };
    (context, Published(receiver))
}
