//! What one script said: a bounded log of its messages, and the replies to
//! the `rpc.exports` calls made on it.
//!
//! Frida delivers a script's messages on its own thread, so the log is
//! filled there and read from the session's threads under one lock, and a
//! reader that waits is woken through a condition variable. Nothing here
//! prints: a hook's output only ever lands in its log, so it cannot flood a
//! cell.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

use super::backend::Outcome;
use super::cancel::Cancel;

/// Messages one log keeps; past it the oldest go and `dropped` counts them.
pub(super) const LOG_CAP: usize = 1000;
/// Bytes of a message's binary data kept; `data-size` is the full length.
pub(super) const DATA_CAP_BYTES: usize = 4096;
/// Characters of one payload a tail line shows.
const LINE_CHARS: usize = 300;

/// One message a script produced.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Message {
    seq: u64,
    /// `send`, `error`, `log`, or whatever else Frida called it.
    kind: String,
    /// A `send()` payload, an error's description, a log line's text.
    payload: Value,
    /// The first [`DATA_CAP_BYTES`] of the binary data sent along.
    data: Option<Vec<u8>>,
    data_size: usize,
    /// A log line's level.
    level: Option<String>,
    /// An uncaught error's stack.
    stack: Option<String>,
}

impl Message {
    /// The message as a cell sees it.
    pub(super) fn json(&self) -> Value {
        let mut shown = json!({"seq": self.seq, "type": self.kind, "payload": self.payload});
        if let Some(data) = &self.data {
            shown["data-base64"] = json!(base64::engine::general_purpose::STANDARD.encode(data));
            shown["data-size"] = json!(self.data_size);
        }
        if let Some(level) = &self.level {
            shown["level"] = json!(level);
        }
        if let Some(stack) = &self.stack {
            shown["stack"] = json!(stack);
        }
        shown
    }

    /// The message on one line, for a tail.
    pub(super) fn line(&self) -> String {
        let text = match &self.payload {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let shown: String = text.chars().take(LINE_CHARS).collect();
        let cut = if shown.len() < text.len() { "..." } else { "" };
        let tag = self.level.as_deref().unwrap_or(&self.kind);
        let data = if self.data_size > 0 {
            format!(" (+{} bytes)", self.data_size)
        } else {
            String::new()
        };
        format!("#{} {tag}: {shown}{cut}{data}", self.seq)
    }
}

#[derive(Default)]
struct Inner {
    items: VecDeque<Message>,
    next_seq: u64,
    dropped: u64,
    closed: bool,
    /// Where the next wait begins: after the last message a wait returned.
    cursor: u64,
    next_call: u64,
    /// Calls whose reply someone is waiting for; any other reply is dropped.
    awaiting: HashSet<u64>,
    replies: HashMap<u64, Outcome<Value>>,
}

/// How a wait on the log ended.
enum Waited<T> {
    Ready(T),
    TimedOut,
    Cancelled,
}

/// A script's log. Shared between the registry (which reads it) and the
/// sink Frida calls (which fills it).
#[derive(Default)]
pub(super) struct ScriptLog {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl ScriptLog {
    /// Take one message from Frida: an `rpc.exports` reply settles its call,
    /// anything else joins the log. Runs on Frida's thread.
    pub(super) fn receive(&self, text: &str, data: Option<&[u8]>) {
        let message: Value = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let payload = message.get("payload");
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.closed {
            return;
        }
        // `["frida:rpc", id, "ok" | "error", value, ...]`
        if let (true, Some([tag, id, status, value, ..])) = (
            kind == "send",
            payload.and_then(Value::as_array).map(Vec::as_slice),
        ) {
            if let (Some("frida:rpc"), Some(id)) = (tag.as_str(), id.as_u64()) {
                if inner.awaiting.remove(&id) {
                    let reply = match (status.as_str(), data) {
                        (Some("ok"), Some(data)) => Ok(json!({
                            "data-base64": base64::engine::general_purpose::STANDARD.encode(data),
                            "data-size": data.len(),
                        })),
                        (Some("ok"), None) => Ok(value.clone()),
                        (_, _) => Err(value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_string)),
                    };
                    inner.replies.insert(id, reply);
                    drop(inner);
                    self.changed.notify_all();
                }
                return;
            }
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.items.len() == LOG_CAP {
            inner.items.pop_front();
            inner.dropped += 1;
        }
        inner.items.push_back(match kind.as_str() {
            "send" => Message {
                seq,
                kind,
                payload: payload.cloned().unwrap_or_default(),
                data: data.map(|data| data[..data.len().min(DATA_CAP_BYTES)].to_vec()),
                data_size: data.map_or(0, <[u8]>::len),
                level: None,
                stack: None,
            },
            "log" => Message {
                seq,
                kind,
                payload: payload.cloned().unwrap_or_default(),
                data: None,
                data_size: 0,
                level: message
                    .get("level")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                stack: None,
            },
            _error_or_other => Message {
                seq,
                kind,
                stack: message
                    .get("stack")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                payload: message.get("description").cloned().unwrap_or(message),
                data: None,
                data_size: 0,
                level: None,
            },
        });
        drop(inner);
        self.changed.notify_all();
    }

    /// The script is gone: nothing more joins the log, and every wait ends.
    pub(super) fn close(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.changed.notify_all();
    }

    /// The buffered messages, oldest first (the newest `newest` when given),
    /// and how many older ones were dropped.
    pub(super) fn snapshot(&self, newest: Option<usize>) -> (Vec<Message>, u64) {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let skip = newest.map_or(0, |newest| inner.items.len().saturating_sub(newest));
        (
            inner.items.iter().skip(skip).cloned().collect(),
            inner.dropped,
        )
    }

    /// Wait up to `limit` for `ready` to answer, looking again whenever the
    /// log changes; also returns what is left of `limit`.
    fn wait_for<T>(
        self: &Arc<Self>,
        limit: Duration,
        cancel: &Cancel,
        mut ready: impl FnMut(&mut Inner) -> Option<T>,
    ) -> (Waited<T>, Duration) {
        let until = Instant::now() + limit;
        let stopped = Arc::new(AtomicBool::new(false));
        // Declared before the guard, so dropped after it: dropping the
        // watch takes the signal's lock, which a cancelling thread holds
        // while its hook waits for the log's.
        let _watch = cancel.watch({
            let stopped = Arc::clone(&stopped);
            let log = Arc::clone(self);
            move || {
                // Under the log's lock, so a waiter is either before its
                // check of `stopped` or already parked.
                let inner = log.inner.lock().unwrap_or_else(PoisonError::into_inner);
                stopped.store(true, Ordering::SeqCst);
                drop(inner);
                log.changed.notify_all();
            }
        });
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if let Some(value) = ready(&mut inner) {
                return (Waited::Ready(value), left);
            }
            if stopped.load(Ordering::SeqCst) {
                return (Waited::Cancelled, left);
            }
            if left.is_zero() {
                return (Waited::TimedOut, left);
            }
            inner = self
                .changed
                .wait_timeout(inner, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The next message after the last one a wait returned, waiting up to
    /// `limit` for it; `None` on timeout or once the script is gone and its
    /// log is read to the end. Also returns what is left of `limit`.
    ///
    /// # Errors
    ///
    /// The wait was cancelled.
    pub(super) fn next_message(
        self: &Arc<Self>,
        limit: Duration,
        cancel: &Cancel,
    ) -> Outcome<(Option<Message>, Duration)> {
        let (waited, left) = self.wait_for(limit, cancel, |inner| {
            let found = inner
                .items
                .iter()
                .find(|item| item.seq >= inner.cursor)
                .cloned();
            if let Some(found) = &found {
                inner.cursor = found.seq + 1;
            }
            // A closed log still hands out what it holds.
            found.map(Some).or_else(|| inner.closed.then_some(None))
        });
        match waited {
            Waited::Ready(found) => Ok((found, left)),
            Waited::TimedOut => Ok((None, left)),
            Waited::Cancelled => Err("the wait was abandoned: its cell was stopped".to_string()),
        }
    }

    /// Announce an `rpc.exports` call and return the id its request carries.
    pub(super) fn begin_call(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.next_call += 1;
        let id = inner.next_call;
        inner.awaiting.insert(id);
        id
    }

    /// The reply to call `id`, waiting up to `limit` for it.
    ///
    /// # Errors
    ///
    /// The export threw (its message), the script went away, the reply did
    /// not come in time, or the wait was cancelled. The call itself is not
    /// taken back: a reply that comes later is dropped.
    pub(super) fn reply(
        self: &Arc<Self>,
        id: u64,
        limit: Duration,
        cancel: &Cancel,
    ) -> Outcome<Value> {
        let (waited, _) = self.wait_for(limit, cancel, |inner| {
            inner
                .replies
                .remove(&id)
                .or_else(|| inner.closed.then(|| Err("the script is gone".to_string())))
        });
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .awaiting
            .remove(&id);
        match waited {
            Waited::Ready(reply) => reply,
            Waited::TimedOut => Err(format!(
                "no reply within {} ms (the export may still be running in the target)",
                limit.as_millis()
            )),
            Waited::Cancelled => Err("the call was abandoned: its cell was stopped".to_string()),
        }
    }
}
