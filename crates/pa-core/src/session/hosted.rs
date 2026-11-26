//! Hosted session lifetimes' identities: what a session hosted inside a
//! daemon process records where a worker process recorded its pid and start
//! id.
//!
//! Each hosted lifetime gets a pid-shaped id from a range no operating
//! system hands out ([`is_hosted_pid`]) and a generation string. Descriptors,
//! leases and registrations record that pair; "alive" means the generation
//! is registered as running in this process ([`hosted_alive`]), never that
//! some pid exists. The host that runs the lifetime registers it
//! ([`register`]) for as long as it runs and puts its [`HostedId`] in the
//! lifetime's scope, so the session's own code finds its id
//! ([`session_pid`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use pa_types::session_scope;

/// Hosted ids start here: above every real pid (Linux caps pids at 2^22,
/// macOS at 99,998), and positive as an `i32` so a signal sent to one by
/// mistake names no process and no process group. Windows documents no cap,
/// but its process and thread ids are indexes into one kernel handle table
/// of at most 2^24 entries, times four: below `0x0400_0000`, a sixteenth of
/// this base. So no platform gate: an id at or above the base is never a
/// process on any of the three.
pub const HOSTED_PID_BASE: u32 = 0x4000_0000;

/// The hosted lifetime a scope belongs to (a
/// [`session_scope::SessionScope`] slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostedId(pub u32);

/// The hosted lifetimes running in this process: id -> generation.
fn live() -> &'static Mutex<HashMap<u32, String>> {
    static LIVE: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A fresh hosted lifetime's id and generation, not yet registered.
#[must_use]
pub fn new_lifetime() -> (u32, String) {
    static NEXT: OnceLock<AtomicU32> = OnceLock::new();
    // A random start keeps a restarted daemon from reissuing the ids its
    // predecessor's descriptors still name.
    let next = NEXT.get_or_init(|| AtomicU32::new(uuid::Uuid::new_v4().as_fields().0));
    let pid = HOSTED_PID_BASE | (next.fetch_add(1, Ordering::Relaxed) & (HOSTED_PID_BASE - 1));
    let generation = format!("hosted:{}:{}", std::process::id(), uuid::Uuid::new_v4());
    (pid, generation)
}

/// Registers the hosted lifetime `pid` as running in this process.
pub fn register(pid: u32, generation: String) {
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(pid, generation);
}

/// The hosted lifetime `pid` no longer runs in this process.
pub fn unregister(pid: u32) {
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&pid);
}

/// Whether `pid` is a hosted session's id rather than a process id.
#[must_use]
pub fn is_hosted_pid(pid: u32) -> bool {
    pid >= HOSTED_PID_BASE
}

/// Whether the hosted lifetime `pid` is running in this process.
#[must_use]
pub fn hosted_alive(pid: u32) -> bool {
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains_key(&pid)
}

/// The generation of the hosted lifetime `pid`, while it runs in this process.
#[must_use]
pub fn hosted_generation(pid: u32) -> Option<String> {
    live()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&pid)
        .cloned()
}

/// The id the calling session records as its owner pid: its hosted id inside
/// a hosted session, the process id anywhere else.
#[must_use]
pub fn session_pid() -> u32 {
    session_scope::current()
        .and_then(|scope| scope.slot::<HostedId>())
        .map_or_else(std::process::id, |id| id.0)
}
