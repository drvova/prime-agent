//! One rizin and the pipe protocol that drives it.
//!
//! rizin runs with `-q0`: it reads one command per line and writes a NUL
//! after the output of each. A NUL alone cannot end a reply, because a
//! command's own output may contain one (a raw print, a JSON grep), so every
//! command is followed by an `echo` of a token no output can contain, and
//! the reply ends where `token newline NUL` arrives; the NUL before it is
//! the command's own.
//!
//! A reader thread owns rizin's output. It keeps a bounded part of each
//! reply (its start and its end; what falls between is counted and
//! dropped) however much rizin writes, hands each finished reply to the
//! command waiting for it, and, when the output ends, reaps rizin and
//! records how it ended.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;

use super::program::{Process, Running};
use crate::kernel::spill::SPILL_COPY_MAX_BYTES;
use crate::tools::tool_definition::AbortSignal;

/// Bytes of a reply's start that are kept: what the spill store takes.
pub(super) const HEAD_CAP: usize = SPILL_COPY_MAX_BYTES;
/// Bytes of a reply's end that are kept when it outgrows [`HEAD_CAP`].
pub(super) const TAIL_CAP: usize = 256 * 1024;
/// Bytes of rizin's last output a dead session's reason shows.
const LAST_OUTPUT_BYTES: usize = 2 * 1024;
/// Bytes of the output's end that are searched again with what follows:
/// more than a fence is long.
const CARRY: usize = 128;
/// How long an interrupted command has to end before rizin is killed.
pub(super) const INTERRUPT_GRACE: Duration = Duration::from_secs(2);
/// How long a killed rizin has to be reaped before its command stops
/// waiting for the status.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// The kept part of one reply.
#[derive(Debug, Default)]
pub(super) struct Reply {
    pub(super) head: Vec<u8>,
    pub(super) tail: VecDeque<u8>,
    /// Bytes between the two that were not kept.
    pub(super) dropped: u64,
}

impl Reply {
    /// The last `limit` bytes that were kept, as text.
    pub(super) fn last(&self, limit: usize) -> String {
        let kept: Vec<u8> = self.head.iter().chain(&self.tail).copied().collect();
        String::from_utf8_lossy(&kept[kept.len().saturating_sub(limit)..])
            .trim()
            .to_string()
    }
}

/// How a command ended without its reply.
#[derive(Debug)]
pub(super) enum Failure {
    /// The cell was stopped; the command was interrupted and rizin lives.
    Stopped,
    /// The command outran its limit and was interrupted; rizin lives. The
    /// reply is what the command had written.
    TimedOut(Reply),
    /// rizin is gone: why.
    Dead(String),
}

/// What the reader and the commands share.
#[derive(Default)]
struct Shared {
    /// What ends the reply being read; empty while no command runs.
    fence: Mutex<Vec<u8>>,
    /// Why the session is over, once it is.
    ended: Mutex<Option<String>>,
}

/// A running rizin. Dropping it kills the program and everything it
/// started.
pub(super) struct Session {
    /// rizin's process id, for the model to see.
    pub(super) pid: u32,
    process: Arc<dyn Process>,
    input: Mutex<Box<dyn Write + Send>>,
    /// Finished replies; its lock is held by the one command in flight.
    replies: tokio::sync::Mutex<mpsc::UnboundedReceiver<Reply>>,
    shared: Arc<Shared>,
    /// What this session's fences start with: no output can contain it.
    token: String,
    commands: AtomicU64,
}

impl Session {
    /// Take over a started rizin: its output is read from now on.
    pub(super) fn start(running: Running) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let (finished, replies) = mpsc::unbounded_channel();
        let (reading, process, mut output) = (
            Arc::clone(&shared),
            Arc::clone(&running.process),
            running.output,
        );
        std::thread::Builder::new()
            .name("rizin-output".to_string())
            .spawn(pa_types::session_scope::propagate(move || {
                let mut reply = Reply::default();
                // The end of what was read before, where a fence may begin.
                let mut carry: Vec<u8> = Vec::new();
                let mut chunk = vec![0_u8; 64 * 1024];
                loop {
                    let read = match output.read(&mut chunk) {
                        Ok(read @ 1..) => read,
                        // A signal's arrival is not the output's end.
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Ok(0) | Err(_) => break,
                    };
                    let mut rest = &chunk[..read];
                    while !rest.is_empty() {
                        let fence = reading
                            .fence
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .clone();
                        let window = [carry.as_slice(), rest].concat();
                        let ending = (!fence.is_empty())
                            .then(|| {
                                window
                                    .windows(fence.len())
                                    .position(|bytes| bytes == fence.as_slice())
                            })
                            .flatten();
                        // Of `rest`, what belongs to the reply being read.
                        let taken = ending.map_or(rest.len(), |at| {
                            (at + fence.len()).saturating_sub(carry.len())
                        });
                        let (to_head, to_tail) =
                            rest[..taken].split_at(taken.min(HEAD_CAP - reply.head.len()));
                        reply.head.extend_from_slice(to_head);
                        reply.tail.extend(to_tail);
                        let over = reply.tail.len().saturating_sub(TAIL_CAP);
                        reply.tail.drain(..over);
                        reply.dropped += over as u64;
                        rest = &rest[taken..];
                        match ending {
                            Some(_) => {
                                // The fence is not part of the reply, nor
                                // is the NUL rizin ended the command with.
                                for _ in 0..fence.len() {
                                    if reply.tail.pop_back().is_none() {
                                        reply.head.pop();
                                    }
                                }
                                if reply.tail.back().or(reply.head.last()) == Some(&0)
                                    && reply.tail.pop_back().is_none()
                                {
                                    reply.head.pop();
                                }
                                carry.clear();
                                reading
                                    .fence
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .clear();
                                // The command stopped waiting only when its
                                // session is being dropped.
                                let _ = finished.send(std::mem::take(&mut reply));
                            }
                            None => {
                                // Kept whatever the fence is now: the next
                                // command's may be set after its first bytes.
                                carry = window[window.len().saturating_sub(CARRY)..].to_vec();
                            }
                        }
                    }
                }
                let status = process.wait();
                let last = reply.last(LAST_OUTPUT_BYTES);
                let mut ended = reading.ended.lock().unwrap_or_else(PoisonError::into_inner);
                let cause = ended
                    .take()
                    .map_or_else(String::new, |cause| format!("{cause}; "));
                let shown = if last.is_empty() {
                    String::new()
                } else {
                    format!("; its last output:\n{last}")
                };
                *ended = Some(format!("{cause}rizin ended ({status}){shown}"));
                // Dropping `finished` here tells a waiting command.
            }))?;
        Ok(Session {
            pid: running.process.pid(),
            process: running.process,
            input: Mutex::new(running.input),
            replies: tokio::sync::Mutex::new(replies),
            shared,
            token: format!("PA{}", uuid::Uuid::new_v4().simple()),
            commands: AtomicU64::new(0),
        })
    }

    /// Why the session is over, or `None` while rizin runs.
    pub(super) fn ended(&self) -> Option<String> {
        self.shared
            .ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Run `command` (one line; empty to read what rizin wrote on its own,
    /// as it does while it starts) and return its reply. The command ends
    /// with its reply, with `limit`, or with `stop`: on the last two it is
    /// interrupted, and a rizin that does not end it within
    /// [`INTERRUPT_GRACE`] is killed.
    pub(super) async fn run(
        &self,
        command: &str,
        limit: Option<Duration>,
        stop: &AbortSignal,
    ) -> Result<Reply, Failure> {
        let dead = || {
            Failure::Dead(
                self.ended()
                    .unwrap_or_else(|| "rizin stopped answering".to_string()),
            )
        };
        let mut replies = tokio::select! {
            biased;
            () = stop.cancelled() => return Err(Failure::Stopped),
            replies = self.replies.lock() => replies,
        };
        if self.ended().is_some() {
            return Err(dead());
        }
        let mark = format!(
            "{}x{}",
            self.token,
            self.commands.fetch_add(1, Ordering::SeqCst)
        );
        *self
            .shared
            .fence
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = format!("{mark}\n\0").into_bytes();
        let written = {
            let mut input = self.input.lock().unwrap_or_else(PoisonError::into_inner);
            input
                .write_all(
                    if command.is_empty() {
                        format!("echo {mark}\n")
                    } else {
                        format!("{command}\necho {mark}\n")
                    }
                    .as_bytes(),
                )
                .and_then(|()| input.flush())
        };
        if written.is_err() {
            // rizin stopped reading: it has ended, and the reader says how
            // once it has reaped it.
            while let Ok(Some(_)) = tokio::time::timeout(REAP_GRACE, replies.recv()).await {}
            return Err(dead());
        }
        let limited = async {
            match limit {
                Some(limit) => tokio::time::sleep(limit).await,
                None => std::future::pending().await,
            }
        };
        let stopped = tokio::select! {
            biased;
            () = stop.cancelled() => true,
            reply = replies.recv() => return reply.ok_or_else(dead),
            () = limited => false,
        };
        self.process.interrupt();
        match tokio::time::timeout(INTERRUPT_GRACE, replies.recv()).await {
            Ok(Some(_)) if stopped => Err(Failure::Stopped),
            Ok(Some(reply)) => Err(Failure::TimedOut(reply)),
            Ok(None) => Err(dead()),
            Err(_) => {
                self.end(&format!(
                    "a command did not end within {} s of its interrupt and rizin was killed",
                    INTERRUPT_GRACE.as_secs()
                ));
                // The reader adds how rizin ended; a kill that reaps
                // nothing in time leaves the reason as it is.
                while let Ok(Some(_)) = tokio::time::timeout(REAP_GRACE, replies.recv()).await {}
                Err(dead())
            }
        }
    }

    /// End the session for `cause`: rizin and everything it started are
    /// killed. The first cause given is the one a later command reports.
    pub(super) fn end(&self, cause: &str) {
        self.shared
            .ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| cause.to_string());
        self.process.kill();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.end("the session was closed");
    }
}
