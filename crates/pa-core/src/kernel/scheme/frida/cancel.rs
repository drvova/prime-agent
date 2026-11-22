//! The stop signal of one Frida call.
//!
//! A call into frida-core blocks its thread until Frida's own main loop
//! answers, so nothing on that thread can poll for a stopped cell. A
//! [`Cancel`] instead runs hooks at the moment it is cancelled, from the
//! thread that cancels it: the real backend hooks the `GCancellable` it
//! passed to Frida, a message wait hooks its condition variable. It needs no
//! runtime, so the kernel's teardown paths (a lifetime's end, a drop) use it
//! too, through [`Cancel::deadline`].

use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

type Hook = Box<dyn Fn() + Send>;

#[derive(Default)]
struct Hooks {
    cancelled: bool,
    next: u64,
    watching: Vec<(u64, Hook)>,
}

/// A clonable stop signal: every clone cancels and observes the same call.
#[derive(Clone, Default)]
pub(super) struct Cancel {
    hooks: Arc<Mutex<Hooks>>,
}

/// Keeps a hook registered with [`Cancel::watch`]; dropping it removes the
/// hook, after which the hook is neither running nor run again.
pub(super) struct Watch {
    hooks: Weak<Mutex<Hooks>>,
    id: u64,
}

impl Cancel {
    /// A signal that cancels itself once `limit` has passed, for calls no
    /// cell is waiting on (teardown): a Frida that does not answer cannot
    /// hold them longer than that.
    pub(super) fn deadline(limit: Duration) -> Self {
        let cancel = Cancel::default();
        let hooks = Arc::downgrade(&cancel.hooks);
        std::thread::spawn(move || {
            let until = Instant::now() + limit;
            // A park can end early: only the clock decides.
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                if hooks.strong_count() == 0 {
                    return;
                }
                std::thread::park_timeout(left.min(Duration::from_millis(100)));
            }
            if let Some(hooks) = hooks.upgrade() {
                Cancel { hooks }.cancel();
            }
        });
        cancel
    }

    /// Cancel the call: every watching hook runs now, on this thread, and a
    /// hook registered later runs as it is registered.
    pub(super) fn cancel(&self) {
        let mut hooks = self.hooks.lock().unwrap_or_else(PoisonError::into_inner);
        if !hooks.cancelled {
            hooks.cancelled = true;
            for (_, hook) in &hooks.watching {
                hook();
            }
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cancelled
    }

    /// Run `hook` when the call is cancelled (at once when it already is),
    /// for as long as the returned [`Watch`] lives. The hook runs under this
    /// signal's lock: it must not touch the signal itself.
    pub(super) fn watch(&self, hook: impl Fn() + Send + 'static) -> Watch {
        let mut hooks = self.hooks.lock().unwrap_or_else(PoisonError::into_inner);
        if hooks.cancelled {
            hook();
        }
        let id = hooks.next;
        hooks.next += 1;
        hooks.watching.push((id, Box::new(hook)));
        Watch {
            hooks: Arc::downgrade(&self.hooks),
            id,
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(hooks) = self.hooks.upgrade() {
            hooks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .watching
                .retain(|(id, _)| *id != self.id);
        }
    }
}
