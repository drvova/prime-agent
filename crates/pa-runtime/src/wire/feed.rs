//! One connection's subscriptions: replay from the log, then live events
//! from the bus, with no gap and no duplicate between them, and catch-up
//! from the log when the connection falls behind the bus.
//!
//! The connection's [`Listener`] is registered before it says `welcome`, so
//! every event stored after that is either in the listener or still to
//! come. A subscription reads the log's last sequence number `last` and
//! replays from the log only up to it; its `through` is `last` (or the
//! client's own `After { seq }` when that is higher), and it hears a live
//! event only when the event's sequence number is above `through`. An event
//! the replay and the listener both hold is therefore sent once, and one
//! stored after `last` is never missed.
//!
//! Every event goes through [`Feed::deliver`], which also keeps the highest
//! sequence number the connection has read (`cursor`) and skips anything at
//! or below it: after a lag, the feed reads the log from `cursor` up to its
//! then-last event, and the events the listener still holds from before that
//! point are skipped as they arrive.
//!
//! A live-only topic (stream and tool deltas, once the bus stops storing
//! them) has nothing in the log: a subscription to one replays nothing, and
//! a connection that lags loses the live-only events the bus dropped for it.
//! Today the bus stores every event, so catch-up recovers everything.

use pa_types::event_log::{Event, TopicFilter};
use pa_types::wire2::{ServerFrame, Since};
use tokio::sync::mpsc;

use crate::bus::{Heard, Listener};
use crate::store::EventStore;

/// How many events one read of the log asks for.
const PAGE: usize = 256;

/// The most events a `Tail` replays; a larger count replays this many.
pub const MAX_TAIL: u32 = 10_000;

/// What the connection's reader asks of its feed.
#[derive(Debug)]
pub(super) enum Control {
    /// Starts (or, for an id already in use, restarts) a subscription.
    Subscribe {
        sub_id: String,
        filter: TopicFilter,
        since: Since,
    },
    Unsubscribe {
        sub_id: String,
    },
}

/// The client stopped taking frames.
#[derive(Debug, thiserror::Error)]
#[error("the client connection is closed")]
pub(super) struct ClientGone;

struct Subscribed {
    id: String,
    filter: TopicFilter,
    /// Live events at or below this were replayed, or are before the
    /// subscription's start.
    through: u64,
}

pub(super) struct Feed<'a, E> {
    events: &'a E,
    out: mpsc::Sender<ServerFrame>,
    subscriptions: Vec<Subscribed>,
    /// The highest sequence number this connection has read.
    cursor: u64,
}

/// The last stored event's sequence number; 0 for an empty log.
pub(super) async fn last_seq<E: EventStore>(events: &E) -> anyhow::Result<u64> {
    Ok(events
        .before(u64::MAX, 1)
        .await?
        .first()
        .map_or(0, |event| event.seq))
}

impl<'a, E: EventStore> Feed<'a, E> {
    /// A feed that has read the log up to `cursor`.
    pub(super) fn new(events: &'a E, out: mpsc::Sender<ServerFrame>, cursor: u64) -> Self {
        Feed {
            events,
            out,
            subscriptions: Vec::new(),
            cursor,
        }
    }

    /// Serves `control` and `listener` until either ends or the client is
    /// gone. A failed read of the log is told to the client with `closing`
    /// and ends the feed.
    ///
    /// # Errors
    ///
    /// The log could not be read, or the client is gone.
    pub(super) async fn run(
        mut self,
        mut listener: Listener,
        mut control: mpsc::Receiver<Control>,
    ) -> anyhow::Result<()> {
        loop {
            let step = tokio::select! {
                command = control.recv() => match command {
                    Some(Control::Subscribe { sub_id, filter, since }) => {
                        self.subscribe(sub_id, filter, since).await
                    }
                    Some(Control::Unsubscribe { sub_id }) => {
                        self.subscriptions.retain(|subscribed| subscribed.id != sub_id);
                        Ok(())
                    }
                    None => return Ok(()),
                },
                heard = listener.recv() => match heard {
                    Some(Heard::Event(event)) => self.deliver(&event).await.map_err(anyhow::Error::from),
                    Some(Heard::Lagged { missed }) => {
                        tracing::debug!("a wire client lost {missed} events; catching up from the log");
                        self.catch_up().await
                    }
                    None => return Ok(()),
                },
            };
            if let Err(error) = step {
                if !error.is::<ClientGone>() {
                    let reason = format!("the runtime could not read its event log: {error:#}");
                    // The client may be gone too; the error is returned either way.
                    let _ = self.out.send(ServerFrame::Closing { reason }).await;
                }
                return Err(error);
            }
        }
    }

    async fn send(&self, frame: ServerFrame) -> Result<(), ClientGone> {
        self.out.send(frame).await.map_err(|_| ClientGone)
    }

    /// Sends `event` to every subscription that matches it and has not seen
    /// it, unless the connection has read it already.
    async fn deliver(&mut self, event: &Event) -> Result<(), ClientGone> {
        if event.seq <= self.cursor {
            return Ok(());
        }
        self.cursor = event.seq;
        for subscribed in &self.subscriptions {
            if event.seq > subscribed.through && subscribed.filter.matches(&event.event.topic) {
                self.send(ServerFrame::Event {
                    sub_id: subscribed.id.clone(),
                    event: event.clone(),
                })
                .await?;
            }
        }
        Ok(())
    }

    /// Reads the log from `cursor` to its last event, delivering each.
    async fn catch_up(&mut self) -> anyhow::Result<()> {
        let last = last_seq(self.events).await?;
        while self.cursor < last {
            let read = self.cursor;
            for event in self.events.since(read, PAGE).await? {
                if event.seq > last {
                    break;
                }
                self.deliver(&event).await?;
            }
            if self.cursor == read {
                // Nothing at or below `last` above `cursor` any more.
                break;
            }
        }
        Ok(())
    }

    /// Replays what `since` asks for, says where the replay ended, and adds
    /// the subscription.
    async fn subscribe(
        &mut self,
        sub_id: String,
        filter: TopicFilter,
        since: Since,
    ) -> anyhow::Result<()> {
        self.subscriptions
            .retain(|subscribed| subscribed.id != sub_id);
        let last = last_seq(self.events).await?;
        let through = match since {
            Since::Now => last,
            Since::After { seq } => {
                let mut read = seq;
                'pages: while read < last {
                    let page = self.events.since(read, PAGE).await?;
                    if page.is_empty() {
                        break;
                    }
                    for event in page {
                        if event.seq > last {
                            break 'pages;
                        }
                        read = event.seq;
                        if filter.matches(&event.event.topic) {
                            self.send(ServerFrame::Event {
                                sub_id: sub_id.clone(),
                                event,
                            })
                            .await?;
                        }
                    }
                }
                seq.max(last)
            }
            Since::Tail { count } => {
                let count = usize::try_from(count.min(MAX_TAIL)).unwrap_or(usize::MAX);
                // Newest first while reading back.
                let mut tail = Vec::new();
                let mut below = last.saturating_add(1);
                while tail.len() < count {
                    let page = self.events.before(below, PAGE).await?;
                    let Some(oldest) = page.last() else {
                        break;
                    };
                    below = oldest.seq;
                    let wanted = count - tail.len();
                    tail.extend(
                        page.into_iter()
                            .filter(|event| filter.matches(&event.event.topic))
                            .take(wanted),
                    );
                }
                for event in tail.into_iter().rev() {
                    self.send(ServerFrame::Event {
                        sub_id: sub_id.clone(),
                        event,
                    })
                    .await?;
                }
                last
            }
        };
        self.send(ServerFrame::Backfilled {
            sub_id: sub_id.clone(),
            through_seq: through,
        })
        .await?;
        self.subscriptions.push(Subscribed {
            id: sub_id,
            filter,
            through,
        });
        Ok(())
    }
}
