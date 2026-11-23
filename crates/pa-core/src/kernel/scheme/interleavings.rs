//! The kernel's thread hand-offs, each checked over the interleavings of its
//! threads (`loom`).
//!
//! `loom` runs a closure once per schedule, on its own locks, atomics and
//! threads, so it cannot run the kernel itself (a Steel engine, a tokio
//! runtime). Each test here is the hand-off alone: the same shared words,
//! written and read in the same order and under the same locks as the code
//! it names, with everything that is not shared left out. A change to the
//! order of those reads and writes in the kernel belongs here too.
//!
//! A model that can stop making progress fails: `loom` reports the
//! deadlock, or a thread that would have to run forever for the others.
//!
//! The two larger models are checked over every schedule with at most
//! [`PREEMPTIONS`] preemptions, which keeps them to about a second; the
//! others over every schedule.

use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// How many times a schedule of the two larger models may take a thread off
/// the processor while it could go on.
const PREEMPTIONS: usize = 4;

/// Steel's states of an engine's thread (`ThreadState`).
const RUNNING: u8 = 0;
const INTERRUPTED: u8 = 1;
const PAUSED_AT_SAFEPOINT: u8 = 2;

/// Steel's `ThreadStateController`: two words, each call writing one after
/// the other.
struct Controller {
    paused: AtomicBool,
    state: AtomicU8,
}

impl Controller {
    fn write(&self, paused: bool, state: u8) {
        self.paused.store(paused, Ordering::Relaxed);
        self.state.store(state, Ordering::SeqCst);
    }
}

/// `cell::interrupt_on_engine` over `cell::interrupt_engine`: under the
/// lock, the interrupt's two writes and then the unpark, for a cell still on
/// the engine.
fn interrupt_on_engine(running: &Mutex<Option<thread::Thread>>, controller: &Controller) -> bool {
    let running = running.lock().unwrap();
    if let Some(engine_thread) = running.as_ref() {
        controller.write(/*paused*/ true, INTERRUPTED);
        engine_thread.unpark();
    }
    running.is_some()
}

/// A cell is stopped while its engine collects and returns from a host
/// function: `cell::Cells::interrupt` (the first raise and its repeats)
/// against Steel's `stop_threads`/`resume_threads` and the wait at the end
/// of `SteelThread::enter_safepoint`, then the cell's end in
/// `cell::execute`. Whatever the two threads read of each other's writes,
/// the cell leaves the engine, and leaves it resumed for the next cell.
/// Without the unpark it does not: a cell that read the engine as paused but
/// not interrupted parks, and no later raise brings it back.
#[test]
fn a_stopped_cell_leaves_the_engine_however_its_interrupt_lands() {
    let mut schedules = loom::model::Builder::new();
    schedules.preemption_bound = Some(PREEMPTIONS);
    schedules.check(|| {
        let controller = Arc::new(Controller {
            paused: AtomicBool::new(false),
            state: AtomicU8::new(RUNNING),
        });
        let running = Arc::new(Mutex::new(None));
        let (on_engine, engine) = (Arc::clone(&running), Arc::clone(&controller));
        let cell = thread::spawn(move || {
            *on_engine.lock().unwrap() = Some(thread::current());
            // A collection, on the engine's own thread.
            engine.write(/*paused*/ true, PAUSED_AT_SAFEPOINT);
            engine.write(/*paused*/ false, RUNNING);
            // A host function returns: a paused engine waits here unless it
            // was interrupted.
            while engine.paused.load(Ordering::Relaxed) {
                if engine.state.load(Ordering::SeqCst) == INTERRUPTED {
                    break;
                }
                thread::park();
            }
            // The cell's end, under the lock its interrupt takes.
            let mut on_engine = on_engine.lock().unwrap();
            *on_engine = None;
            engine.write(/*paused*/ false, RUNNING);
        });
        while interrupt_on_engine(&running, &controller) {
            thread::yield_now();
        }
        cell.join().unwrap();
        assert_eq!(
            (
                controller.paused.load(Ordering::Relaxed),
                controller.state.load(Ordering::SeqCst)
            ),
            (false, RUNNING)
        );
    });
}

/// `SessionScope::end` against `SessionScope::at_end` (`pa-types`
/// `session_scope`), as a kernel that starts while its session's lifetime
/// ends registers its stop there (`SchemeKernelProvisioner::ensure`): the
/// hook runs exactly once, by whichever of the two comes second.
#[test]
fn a_hook_registered_while_its_lifetime_ends_runs_once() {
    loom::model(|| {
        let ended = Arc::new(AtomicBool::new(false));
        let hooks = Arc::new(Mutex::new(0_usize));
        let ran = Arc::new(AtomicUsize::new(0));
        let (ending, registered, stopped) =
            (Arc::clone(&ended), Arc::clone(&hooks), Arc::clone(&ran));
        let host = thread::spawn(move || {
            ending.store(true, Ordering::SeqCst);
            let taken = std::mem::take(&mut *registered.lock().unwrap());
            stopped.fetch_add(taken, Ordering::SeqCst);
        });
        {
            let mut registered = hooks.lock().unwrap();
            if ended.load(Ordering::SeqCst) {
                drop(registered);
                ran.fetch_add(1, Ordering::SeqCst);
            } else {
                *registered += 1;
            }
        }
        host.join().unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    });
}

/// The process's spare engines (`SPARES`, `SPARE_TARGET`, `FIRST_TAKEN`,
/// `REFILLING` in `mod.rs`). The two flags are sequentially consistent
/// atomics there, and the hand-off needs that order (each side writes its
/// flag, then reads the other's). `loom` orders such an atomic no more
/// strictly than an acquire/release one, so here each is a locked word.
struct Spares {
    built: Mutex<usize>,
    first_taken: Mutex<bool>,
    refilling: Mutex<bool>,
    /// Every refill thread started, for the model to wait for.
    refills: Mutex<Vec<thread::JoinHandle<()>>>,
}

/// `spare_goal` with a target of two.
fn spare_goal(spares: &Spares) -> usize {
    if *spares.first_taken.lock().unwrap() {
        2
    } else {
        1
    }
}

/// `prewarm_scheme_kernel`: one refill thread at a time, which looks again
/// after it has marked itself as done.
fn prewarm(spares: &Arc<Spares>) {
    if std::mem::replace(&mut *spares.refilling.lock().unwrap(), true) {
        return;
    }
    let pool = Arc::clone(spares);
    let refill = thread::spawn(move || {
        while *pool.built.lock().unwrap() < spare_goal(&pool) {
            *pool.built.lock().unwrap() += 1;
        }
        *pool.refilling.lock().unwrap() = false;
        if *pool.built.lock().unwrap() < spare_goal(&pool) {
            prewarm(&pool);
        }
    });
    spares.refills.lock().unwrap().push(refill);
}

/// The first session starts (`SchemeKernelProvisioner::ensure`: take a
/// spare, mark the first take, ask for a refill) while the refill started at
/// boot is still at work. A take the refill thread did not see before it
/// stopped is seen by the next look, its own or the session's: once every
/// thread is done the pool holds its target.
#[test]
fn a_spare_taken_while_the_pool_refills_is_always_replaced() {
    let mut schedules = loom::model::Builder::new();
    schedules.preemption_bound = Some(PREEMPTIONS);
    schedules.check(|| {
        let spares = Arc::new(Spares {
            built: Mutex::new(0),
            first_taken: Mutex::new(false),
            refilling: Mutex::new(false),
            refills: Mutex::new(Vec::new()),
        });
        prewarm(&spares);
        {
            let mut built = spares.built.lock().unwrap();
            *built = built.saturating_sub(1);
        }
        *spares.first_taken.lock().unwrap() = true;
        prewarm(&spares);
        loop {
            let refill = spares.refills.lock().unwrap().pop();
            match refill {
                Some(refill) => refill.join().unwrap(),
                None => break,
            }
        }
        assert_eq!(*spares.built.lock().unwrap(), 2);
    });
}

/// Where a completion notice stands (`cell::Notice`).
#[derive(Default)]
struct CellNotice {
    read: bool,
    accepted: bool,
    withdrawn: bool,
}

/// A background cell ends and announces itself (the task `cell::execute`
/// leaves behind) while the model reads its end (`cell::Cells::control`).
/// A notice the host took for an end the model has read is withdrawn
/// exactly once, by whichever of the two comes second, and a notice that
/// was never sent is not withdrawn.
#[test]
fn a_cell_notice_read_while_it_is_sent_is_withdrawn_once() {
    loom::model(|| {
        let notice = Arc::new(Mutex::new(CellNotice::default()));
        let withdrawals = Arc::new(AtomicUsize::new(0));
        let (announced, withdrawn) = (Arc::clone(&notice), Arc::clone(&withdrawals));
        let announcing = thread::spawn(move || {
            let unread = !announced.lock().unwrap().read;
            if unread {
                // The host takes `cell.completed` here, outside the lock.
                let mut notice = announced.lock().unwrap();
                notice.accepted = true;
                let withdraw = notice.read && !notice.withdrawn;
                notice.withdrawn |= withdraw;
                drop(notice);
                withdrawn.fetch_add(usize::from(withdraw), Ordering::SeqCst);
            }
            unread
        });
        {
            let mut notice = notice.lock().unwrap();
            notice.read = true;
            let withdraw = notice.accepted && !notice.withdrawn;
            notice.withdrawn |= withdraw;
            drop(notice);
            withdrawals.fetch_add(usize::from(withdraw), Ordering::SeqCst);
        }
        let sent = announcing.join().unwrap();
        assert_eq!(withdrawals.load(Ordering::SeqCst), usize::from(sent));
    });
}

/// Where a command's notice stands (`native::bash_job::Notice`).
const DUE: u8 = 0;
const WAIVED: u8 = 1;
const SENT: u8 = 2;

/// A command's end is announced (`native::bash::announce`) while a cell
/// reads its result (`native::bash::Session::consume`): as for a cell's
/// notice, one withdrawal for a notice that was sent, none for one that was
/// not.
#[test]
fn a_command_notice_read_while_it_is_sent_is_withdrawn_once() {
    loom::model(|| {
        let notice = Arc::new(Mutex::new(DUE));
        let withdrawals = Arc::new(AtomicUsize::new(0));
        let (announced, withdrawn) = (Arc::clone(&notice), Arc::clone(&withdrawals));
        let announcing = thread::spawn(move || {
            let due = *announced.lock().unwrap() == DUE;
            if due {
                // The host takes `bash.completed` here, outside the lock.
                let mut notice = announced.lock().unwrap();
                let read_meanwhile = *notice == WAIVED;
                if !read_meanwhile {
                    *notice = SENT;
                }
                drop(notice);
                withdrawn.fetch_add(usize::from(read_meanwhile), Ordering::SeqCst);
            }
            due
        });
        let sent_before = std::mem::replace(&mut *notice.lock().unwrap(), WAIVED) == SENT;
        withdrawals.fetch_add(usize::from(sent_before), Ordering::SeqCst);
        let sent = announcing.join().unwrap();
        assert_eq!(withdrawals.load(Ordering::SeqCst), usize::from(sent));
    });
}
