//! One cell of a session's Scheme kernel: what it shows on its result card,
//! the output it prints, and how it runs, stops, or moves to the background.
//!
//! # Recording into the running cell (the API for host functions)
//!
//! A host function called from Scheme runs on the cell's own thread, so it
//! finds its cell with [`current`] and needs nothing passed to it:
//!
//! ```ignore
//! if let Some(cell) = cell::current() {
//!     cell.record(CellRecord::Diff(diff));     // shown on the result card
//!     let stopped = cell.signal();             // cancelled when the cell is stopped
//!     let shell = &cell.session().shell_path;  // the session's settings
//! }
//! ```
//!
//! [`CellRecord`] lists everything a result card can carry; the records land
//! on the cell's [`ExecuteResult`] in the order they were made. `current()`
//! is `None` on any other thread (one Scheme started itself, a test calling a
//! host function directly): there is no card to record on then.
//!
//! Scheme-side wrappers record through `(%cell operation argument ...)`:
//! `"todo"`, `"question"`, `"agent-message"` and `"transcript-read"` each
//! take the display record the Python kernel emitted for it.
//!
//! # Long cells
//!
//! A session has one engine, and it runs one cell at a time. In a session
//! with a cell yield window (daemon-backed; see
//! [`crate::kernel::cell_window`]), a cell still running when its window ends
//! is not stopped: the call returns with the cell's id, the cell keeps the
//! engine, and its end is announced by a `cell.completed` host request (the
//! `[cell-done]` notice). Until then every other cell waits for the engine
//! for its own window and is then refused without having run, except the
//! four control forms `(cell-status id)`, `(cell-wait id seconds)`,
//! `(cell-cancel id)` and `(cell-list)`, which the kernel answers itself
//! when one of them is the whole cell. Without a window, the model's
//! `timeout` interrupts the cell instead.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use steel::rerrs::{ErrorKind, SteelErr};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::ThreadStateController;

use super::{native, SessionEngine, SessionEnvironment};
use crate::kernel::shared::{
    parse_sent_agent_message, BackgroundCellFact, CellWaitFact, HostRequestHandlers,
    HostRequestPayload, KernelBashCommands, KernelCommandFailure, KernelDiffDisplay,
    KernelSentAgentMessage, LateSentAgentMessageCallback, DEFAULT_MAX_OUTPUT_CHARS,
    MAX_ATTACHMENT_DATA_CHARS,
};
use crate::tools::kernel_cell::{
    ExecuteResult, ExecuteStatus, KernelAttachment, KernelErrorInfo, KernelExecuteOptions,
};
use crate::tools::tool_definition::AbortSignal;

/// Bytes of a cell's output kept for its result: enough for the result's
/// character cap whatever the characters are.
const OUTPUT_KEPT_BYTES: usize = DEFAULT_MAX_OUTPUT_CHARS * 4;
/// Bytes of a background cell's newest output kept for a later read.
const BACKGROUND_OUTPUT_BYTES: usize = 1_000_000;
/// Finished background cells kept for a later read before the oldest is dropped.
const FINISHED_KEPT: usize = 64;
/// The newest output a `[cell-done]` notice carries.
const NOTICE_TAIL_LINES: usize = 20;
const NOTICE_TAIL_CHARS: usize = 2000;
/// Characters of a cell's first command kept for its card.
const COMMAND_SHOWN_CHARS: usize = 1000;
/// The shortest time between two pieces of a cell's output on its call's
/// stream: what the cell prints in between goes out as one piece.
const STREAM_INTERVAL: Duration = Duration::from_millis(100);
/// How long `(cell-cancel id)` waits for the cell to end before answering.
const CANCEL_WAIT: Duration = Duration::from_secs(5);
/// How long the cell of a session whose lifetime ended is interrupted for
/// before its thread is given up on.
const END_WATCH: Duration = Duration::from_secs(60);
/// The longest `(cell-wait id seconds)`.
const MAX_WAIT_SECONDS: f64 = 600.0;

/// Run before each cell in the session's engine and in the one its macro
/// bodies run in: the cell's own ports, whatever the last cell left there.
const CELL_PORTS: &str = "(current-output-port %cell-output) (current-error-port %cell-errors) \
                          (current-input-port (open-input-string \"\"))";

/// The Scheme side of the cell API, defined in every session engine after
/// its `%cell` primitive is registered and the host bridge is loaded.
const CELL_API: &str = r#"
;; The id of the cell that is running: every `eval` call is one cell.
(define (cell-id) (%cell "id"))

;; A background cell's state: {cell, status, text, output, duration-ms}.
;; status is "running", "ok", "error", or "interrupted"; text is the value or
;; the error once it ended (#f before); output is what it printed since it
;; went to the background.
(define (cell-status id) (%cell "status" id))

;; Wait up to `seconds` for a background cell to end; its state as in
;; `cell-status`. Inside a cell this never waits: the engine runs one cell at
;; a time, so every other cell has already ended.
(define (cell-wait id seconds) (%cell "wait" id seconds))

;; Stop a background cell that is still running; its state.
(define (cell-cancel id) (%cell "cancel" id))

;; The background cells of this session, oldest first: {cell, status,
;; duration-ms} each.
(define (cell-list) (%cell "list"))

;; Load image files (PNG, JPEG, GIF, WebP) into the model's context so it can
;; see them; a line saying what was loaded. Large images are scaled down and
;; re-encoded as JPEG; the files are not changed. Raises when the session's
;; model does not take images, or a path is not a readable image.
(define (attach-image . paths)
  (when (null? paths)
    (error "attach-image: pass at least one image path"))
  (let* ([model (host-request "model.info" (hash 'type "model.info"))]
         [input (hash-try-get model 'input)])
    (unless (and (list? input) (member "image" input))
      (error (string-append
              "attach-image: "
              (if (string? (hash-try-get model 'id)) (hash-ref model 'id) "the current model")
              " does not take images, so the image cannot be loaded into its context."
              " To learn what it shows, spawn a child on a model whose (rlm-find-models) row has"
              " image_input #t and ask it to attach the file and describe it,"
              " or tell the user to switch to a model that takes images.")))
    (%cell "attach-images" paths)))
"#;

thread_local! {
    /// The cell this thread is running.
    static CURRENT: RefCell<Option<CellHandle>> = const { RefCell::new(None) };
}

/// The cell the calling thread is running: `Some` inside a host function
/// that Scheme called from a cell, `None` anywhere else.
pub(crate) fn current() -> Option<CellHandle> {
    CURRENT.with(|current| current.borrow().clone())
}

/// One thing a cell shows on its result card, as the Python kernel's display
/// events carried it.
#[derive(Debug, Clone)]
pub(crate) enum CellRecord {
    /// The session todo list after a change (`{items}`): the last one wins.
    TodoList(Value),
    /// One settled question: `{text, status, answer}`.
    Question(Value),
    /// One sent agent message's receipt. Sent after the cell's call returned
    /// (a background cell), it goes to the session's late-message handler.
    SentAgentMessage(KernelSentAgentMessage),
    /// One transcript read: `{returned, first, total, roles}` and `agent`.
    TranscriptRead(Value),
    /// One image loaded into the model's context.
    Attachment(KernelAttachment),
    /// One file edit.
    Diff(KernelDiffDisplay),
    /// One shell command the cell started.
    BashCommand { command: String },
    /// One bounded wait on a command (the Python kernel's `bash-wait`
    /// record): `completed` when the command finished inside the wait, not
    /// when the wait ended first and the command runs on.
    BashWait { completed: bool },
    /// One command the cell waited for that failed: its output is where the
    /// cell's error context looks for source locations.
    CommandFailure(KernelCommandFailure),
}

/// A running cell, shared by the kernel and the host functions it calls.
#[derive(Clone)]
pub(crate) struct CellHandle(Arc<Cell>);

struct Cell {
    id: String,
    signal: AbortSignal,
    session: Arc<SessionEnvironment>,
    /// What the cell prints, for the call's stream, in pieces of whole characters.
    stream: tokio::sync::mpsc::UnboundedSender<(Stream, String)>,
    state: Mutex<CellState>,
    /// The thread the cell runs on, set before the cell is on the engine: an
    /// interrupt wakes it ([`interrupt_engine`]).
    thread: std::sync::OnceLock<std::thread::Thread>,
    /// Set when the cell was reported as not leaving the engine
    /// ([`StuckCellSink`]): its session's end does not wait for it.
    stuck: AtomicBool,
    /// The cell's waits inside its turn, which its yield window does not count.
    hold: Mutex<Hold>,
    /// Told when a wait starts or ends.
    hold_changed: tokio::sync::Notify,
}

/// A cell's waits inside its turn: for the user's answer (`ask`) or for a
/// condition (`wait-until`). The yield window does not run while the cell
/// waits, so a cell that waits ends inside its call instead of moving to the
/// background while the user is still being asked.
#[derive(Default)]
struct Hold {
    /// How many waits are open.
    open: usize,
    /// When the first open wait started.
    since: Option<Instant>,
    /// The latest time an open wait ends by itself; `None` while one waits
    /// with no bound (a question).
    bound: Option<Instant>,
    /// What the waits that ended took.
    total: Duration,
}

/// Which of a cell's two ports a piece of text was printed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// `(current-output-port)`.
    Output,
    /// `(current-error-port)`.
    Errors,
}

/// What a cell printed to one port while its call waited.
#[derive(Default)]
struct Printed {
    /// The first [`OUTPUT_KEPT_BYTES`], for the result.
    kept: Vec<u8>,
    /// Bytes printed after `kept` was full: not kept, only counted.
    dropped: usize,
    /// Bytes not yet streamed: the start of a split character.
    unstreamed: Vec<u8>,
}

#[derive(Default)]
struct CellState {
    /// What the cell has shown so far; taken when its call returns.
    shown: ExecuteResult,
    output: Printed,
    errors: Printed,
    /// The failed commands the cell waited for; taken when its call returns.
    command_failures: Vec<KernelCommandFailure>,
    /// The call has returned: nothing more reaches its result.
    settled: bool,
    late: Option<LateSentAgentMessageCallback>,
    /// The files the cell edited after its call returned, in order.
    late_edits: Vec<String>,
    /// The newest [`BACKGROUND_OUTPUT_BYTES`] printed to either port after
    /// the call returned.
    background_output: VecDeque<u8>,
    background_output_dropped: bool,
}

impl CellHandle {
    /// The cell's id: unique in the process, the same for the whole cell.
    pub(crate) fn id(&self) -> &str {
        &self.0.id
    }

    /// Cancelled when the cell is aborted, timed out, cancelled, or its
    /// kernel is killed. A host function that blocks must return when it is.
    pub(crate) fn signal(&self) -> AbortSignal {
        self.0.signal.clone()
    }

    /// Start a wait inside the turn that ends by itself at `bound` (`None`:
    /// when [`Self::release`] ends it): the cell's yield window stops
    /// running until the wait ends. Each call needs one [`Self::release`].
    pub(crate) fn hold(&self, bound: Option<Instant>) {
        let mut hold = self.0.hold.lock().unwrap_or_else(PoisonError::into_inner);
        hold.bound = match (hold.open, hold.bound, bound) {
            (0, _, bound) => bound,
            (_, Some(open), Some(bound)) => Some(open.max(bound)),
            (_, None, _) | (_, _, None) => None,
        };
        hold.open += 1;
        hold.since.get_or_insert_with(Instant::now);
        drop(hold);
        self.0.hold_changed.notify_one();
    }

    /// End a wait [`Self::hold`] started.
    pub(crate) fn release(&self) {
        let mut hold = self.0.hold.lock().unwrap_or_else(PoisonError::into_inner);
        hold.open = hold.open.saturating_sub(1);
        if hold.open == 0 {
            if let Some(since) = hold.since.take() {
                hold.total += since.elapsed();
            }
            hold.bound = None;
        }
        drop(hold);
        self.0.hold_changed.notify_one();
    }

    /// When the yield window that would end at `window_end` without waits
    /// ends now: later by what the cell's waits took, and by an open wait
    /// up to its bound. `None` while a wait with no bound is open.
    fn window_end(&self, window_end: Instant) -> Option<Instant> {
        let hold = self.0.hold.lock().unwrap_or_else(PoisonError::into_inner);
        let window_end = window_end + hold.total;
        match (hold.since, hold.bound) {
            (None, _) => Some(window_end),
            (Some(_), None) => None,
            (Some(since), Some(bound)) => Some(window_end + bound.saturating_duration_since(since)),
        }
    }

    /// The settings of the session the cell belongs to.
    pub(crate) fn session(&self) -> &SessionEnvironment {
        &self.0.session
    }

    /// Put `record` on the cell's result card. After the cell's call
    /// returned there is no card: a sent agent message goes to the session's
    /// late-message handler, an edited file is named by the cell's
    /// completion notice, and any other record is dropped. A command teaches
    /// the project card either way.
    pub(crate) fn record(&self, record: CellRecord) {
        // The project card learns the build and test commands cells run,
        // whether or not the cell's call is still waiting.
        if let CellRecord::BashCommand { command } = &record {
            if let Some(card) = crate::project_card::ProjectCard::registered(&self.0.session.cwd) {
                card.observe_command(command);
            }
        }
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.settled {
            let late = state.late.clone();
            match (record, late) {
                (CellRecord::SentAgentMessage(message), Some(late)) => {
                    drop(state);
                    late(message);
                }
                // The card is gone, so the cell's completion notice names
                // the files it edited since.
                (CellRecord::Diff(diff), _) => {
                    if !state.late_edits.contains(&diff.path) {
                        state.late_edits.push(diff.path);
                    }
                }
                (
                    CellRecord::SentAgentMessage(_)
                    | CellRecord::TodoList(_)
                    | CellRecord::Question(_)
                    | CellRecord::TranscriptRead(_)
                    | CellRecord::Attachment(_)
                    | CellRecord::BashCommand { .. }
                    | CellRecord::BashWait { .. }
                    | CellRecord::CommandFailure(_),
                    _,
                ) => {}
            }
            return;
        }
        let state = &mut *state;
        let shown = &mut state.shown;
        match record {
            CellRecord::TodoList(list) => shown.todo_list = Some(list),
            CellRecord::Question(question) => shown.questions.push(question),
            CellRecord::SentAgentMessage(message) => shown.sent_agent_messages.push(message),
            CellRecord::TranscriptRead(read) => shown.transcript_reads.push(read),
            CellRecord::Attachment(attachment)
                if attachment.data.len() > MAX_ATTACHMENT_DATA_CHARS =>
            {
                // As the Python kernel's manager: the image is dropped, the
                // cell fails, and its stderr says why.
                if !shown.stderr.is_empty() {
                    shown.stderr.push('\n');
                }
                // Writing to a string cannot fail.
                let _ = write!(
                    shown.stderr,
                    "attachment dropped: exceeds {MAX_ATTACHMENT_DATA_CHARS} base64 chars"
                );
                shown.status = ExecuteStatus::Error;
            }
            CellRecord::Attachment(attachment) => shown.attachments.push(attachment),
            CellRecord::Diff(diff) => shown.diffs.push(diff),
            CellRecord::BashCommand { command } => {
                let lines = command
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .count();
                if let Some(commands) = shown.bash_commands.as_mut() {
                    commands.count += 1;
                    commands.lines += lines;
                } else {
                    let first = match command.char_indices().nth(COMMAND_SHOWN_CHARS) {
                        Some((end, _)) => {
                            format!("{}\n... [command truncated]", &command[..end])
                        }
                        None => command,
                    };
                    shown.bash_commands = Some(KernelBashCommands {
                        first,
                        count: 1,
                        lines,
                    });
                }
            }
            CellRecord::BashWait { completed: true } => shown.bash_waits.completed += 1,
            CellRecord::BashWait { completed: false } => shown.bash_waits.yielded += 1,
            CellRecord::CommandFailure(failure) => state.command_failures.push(failure),
        }
    }

    /// Mark the cell as the one that ends the turn (`rlm.end_turn`).
    ///
    /// # Errors
    ///
    /// The cell's call already returned, so its result cannot carry the mark.
    pub(super) fn end_turn(&self) -> anyhow::Result<Value> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.settled {
            bail!("rlm.end_turn arrived after its cell settled");
        }
        state.shown.ended_turn = true;
        Ok(json!({ "endedTurn": true }))
    }
}

/// How a background cell ended.
#[derive(Debug, Clone)]
struct CellEnding {
    /// `ok`, `error`, or `interrupted`: the `cell.completed` words.
    status: &'static str,
    /// The value's text, or the error.
    text: String,
    duration_ms: u64,
}

/// Where a background cell's completion notice stands.
#[derive(Default)]
struct Notice {
    /// The model read the cell's end itself.
    read: bool,
    /// The host took the `cell.completed` request.
    accepted: bool,
    /// `cell.consumed` was sent for it.
    withdrawn: bool,
}

/// A cell that outlived its call.
struct BackgroundCell {
    cell: CellHandle,
    started: Instant,
    ended: tokio::sync::watch::Sender<Option<CellEnding>>,
    notice: Mutex<Notice>,
}
/// How long an interrupted cell's thread may take to return before the cell
/// is reported as stuck.
const STUCK_CELL_GRACE: Duration = Duration::from_secs(20);

/// Overrides [`STUCK_CELL_GRACE`] in milliseconds.
const STUCK_CELL_GRACE_ENV: &str = "PRIME_AGENT_STUCK_CELL_GRACE_MS";

/// Told when a cell's thread outlives its interrupt, with one line saying
/// so. A process that hosts several sessions sets one on each session's
/// [`pa_types::session_scope::SessionScope`] and ends the session it names:
/// the engine the stuck thread holds can never run another cell.
pub struct StuckCellSink(pub Box<dyn Fn(&str) + Send + Sync>);

/// Why the kernel stopped a cell its call was waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Aborted,
    TimedOut,
}

/// How a call's wait for its cell ended.
enum Waited {
    /// The cell ended: its value as text, or its error.
    Ended(Result<Result<Option<String>, String>, tokio::task::JoinError>),
    Stopped(Stop),
    /// The yield window ended first: the cell goes on in the background.
    Outlived,
}

/// One of the forms that reads or steers a background cell.
enum Control {
    Status(String),
    Wait(String, f64),
    Cancel(String),
    List,
}

/// Stops the cell a call was waiting for when the call is dropped mid-wait.
struct StopOnDrop {
    cells: Arc<Cells>,
    /// The cell to stop; `None` once the wait has ended by itself.
    cell: Option<CellHandle>,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(cell) = &self.cell {
            self.cells.interrupt(cell);
        }
    }
}

/// A session's cells: the one its engine is running, and those that outlived
/// their calls.
pub(super) struct Cells {
    session: Arc<SessionEnvironment>,
    handlers: HostRequestHandlers,
    /// The engine's controller and its macro engine's ([`super::confinement::controllers`]).
    controllers: Vec<ThreadStateController>,
    runtime: tokio::runtime::Handle,
    /// The cell on the engine; cleared, with the engine's interrupt, when it ends.
    running: Arc<Mutex<Option<CellHandle>>>,
    /// Oldest first.
    background: Mutex<VecDeque<Arc<BackgroundCell>>>,
    killed: AtomicBool,
}

/// One of the session's two ports: what Scheme prints goes to the running cell.
struct CellOutput {
    cells: Arc<Cells>,
    stream: Stream,
}

impl std::io::Write for CellOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let running = self
            .cells
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(cell) = running {
            let mut state = cell.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.settled {
                state.background_output.extend(bytes);
                let excess = state
                    .background_output
                    .len()
                    .saturating_sub(BACKGROUND_OUTPUT_BYTES);
                state.background_output.drain(..excess);
                state.background_output_dropped |= excess > 0;
            } else {
                let printed = match self.stream {
                    Stream::Output => &mut state.output,
                    Stream::Errors => &mut state.errors,
                };
                let kept = OUTPUT_KEPT_BYTES
                    .saturating_sub(printed.kept.len())
                    .min(bytes.len());
                printed.kept.extend_from_slice(&bytes[..kept]);
                printed.dropped += bytes.len() - kept;
                printed.unstreamed.extend_from_slice(bytes);
                let whole = match std::str::from_utf8(&printed.unstreamed) {
                    Ok(text) => text.len(),
                    Err(error) => error.valid_up_to(),
                };
                if whole > 0 {
                    let text = String::from_utf8_lossy(&printed.unstreamed[..whole]).into_owned();
                    printed.unstreamed.drain(..whole);
                    // No one listens once the call has returned.
                    let _ = cell.0.stream.send((self.stream, text));
                }
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Interrupt the engine `controllers` belong to, which `thread` is running:
/// its own, and the one its macro bodies run in.
///
/// Steel's interrupt is two separate writes (`paused`, then the state), and
/// its own collections write the same two from the engine's thread. A thread
/// that returns from a host function between them finds the engine paused
/// but not interrupted, and parks; Steel never unparks the thread that runs
/// the engine, so without the unpark here it would stay parked whatever was
/// raised afterwards. Woken, it reads both again. The unpark comes after the
/// writes: a thread that parks later finds the token and does not wait.
pub(super) fn interrupt_engine(
    controllers: &[ThreadStateController],
    thread: &std::thread::Thread,
) {
    for controller in controllers {
        controller.interrupt();
    }
    thread.unpark();
}

/// Interrupt the engine if `cell` is the one on it; whether it was. Under the
/// lock the cell's end takes to clear the interrupt: none is raised for a
/// cell that has left the engine, where it would stop the next one.
fn interrupt_on_engine(
    running: &Mutex<Option<CellHandle>>,
    controllers: &[ThreadStateController],
    cell: &CellHandle,
) -> bool {
    let running = running.lock().unwrap_or_else(PoisonError::into_inner);
    let thread = running
        .as_ref()
        .filter(|running| Arc::ptr_eq(&running.0, &cell.0))
        .and_then(|running| running.0.thread.get());
    if let Some(thread) = thread {
        interrupt_engine(controllers, thread);
    }
    thread.is_some()
}

impl Cells {
    pub(super) fn new(
        session: Arc<SessionEnvironment>,
        handlers: HostRequestHandlers,
        controllers: Vec<ThreadStateController>,
    ) -> Arc<Self> {
        Arc::new(Cells {
            session,
            handlers,
            controllers,
            runtime: tokio::runtime::Handle::current(),
            running: Arc::new(Mutex::new(None)),
            background: Mutex::new(VecDeque::new()),
            killed: AtomicBool::new(false),
        })
    }

    /// Stop `cell`: host functions see its signal, and the engine is
    /// interrupted if the cell is the one on it.
    fn interrupt(&self, cell: &CellHandle) {
        // Cancelled first: a cell that has not reached the engine yet sees
        // it there and never starts.
        cell.0.signal.cancel();
        let raise = {
            let (running, controllers, cell) = (
                Arc::clone(&self.running),
                self.controllers.clone(),
                cell.clone(),
            );
            // Whether the cell is still on the engine, where it was interrupted.
            move || interrupt_on_engine(&running, &controllers, &cell)
        };
        if raise() {
            // Steel resumes its threads after a collection, which drops an
            // interrupt still pending: it is raised again until the cell has
            // left the engine.
            // The interrupt stops Scheme code and every host function that
            // watches the cell's signal. One that does not can only be
            // reported: the session's host decides what to do with a thread
            // that will not come back. The report rides this loop, so it
            // does not depend on anyone still waiting for the cell.
            let stuck = pa_types::session_scope::current()
                .and_then(|scope| scope.slot::<StuckCellSink>())
                .map(|sink| {
                    let grace = pa_types::session_scope::var(STUCK_CELL_GRACE_ENV)
                        .ok()
                        .and_then(|raw| raw.parse().ok())
                        .map_or(STUCK_CELL_GRACE, Duration::from_millis);
                    (sink, grace, tokio::time::Instant::now() + grace)
                });
            let reported = cell.clone();
            self.runtime.spawn(async move {
                let mut again = tokio::time::interval(Duration::from_millis(20));
                again.tick().await;
                loop {
                    again.tick().await;
                    if !raise() {
                        return;
                    }
                    if let Some((sink, grace, _)) = stuck
                        .as_ref()
                        .filter(|(_, _, due)| tokio::time::Instant::now() >= *due)
                    {
                        reported.0.stuck.store(true, Ordering::SeqCst);
                        (sink.0)(&format!(
                            "a kernel cell did not stop within {} ms of its interrupt: a host call is not observing the cell's cancellation",
                            grace.as_millis()
                        ));
                        return;
                    }
                }
            });
        }
    }

    /// The session's kernel is going away: stop what it runs and announce
    /// nothing more.
    pub(super) fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        let running = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(cell) = running {
            self.interrupt(&cell);
        }
    }

    /// The session's lifetime ended under its kernel (its host stopped
    /// serving it: a crash, a forced stop). The runtime the kernel worked on
    /// is going away, and with it everything that would have stopped the
    /// cell on the engine, so it is stopped from a thread of its own: its
    /// signal is cancelled, and the engine is interrupted until the cell has
    /// left it or [`END_WATCH`] has passed (a native call that never returns
    /// keeps its thread; that thread writes nothing, see
    /// [`pa_types::session_scope::ensure_live`]). Returns whether a cell is
    /// now leaving the engine: `false` when none is on it, or the one on it
    /// was already reported as stuck.
    pub(super) fn end(self: &Arc<Self>) -> bool {
        self.killed.store(true, Ordering::SeqCst);
        let running = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(cell) = running else {
            return false;
        };
        cell.0.signal.cancel();
        if !interrupt_on_engine(&self.running, &self.controllers, &cell) {
            return false;
        }
        let leaving = !cell.0.stuck.load(Ordering::SeqCst);
        let cells = Arc::clone(self);
        let watching = std::thread::Builder::new()
            .name("scheme-cell-end".to_string())
            .spawn(move || {
                let until = Instant::now() + END_WATCH;
                while Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(20));
                    // Steel drops a pending interrupt after a collection.
                    if !interrupt_on_engine(&cells.running, &cells.controllers, &cell) {
                        return;
                    }
                }
            });
        if let Err(error) = watching {
            tracing::warn!("an ended session's cell is interrupted once only: {error}");
        }
        leaving
    }

    /// Answer one `(%cell operation argument ...)` call from the running cell.
    fn call(&self, arguments: &[Value]) -> anyhow::Result<Value> {
        let cell = current().ok_or_else(|| anyhow!("%cell: no cell is running on this thread"))?;
        let [Value::String(operation), rest @ ..] = arguments else {
            bail!("%cell: the operation must be a string");
        };
        match (operation.as_str(), rest) {
            ("id", []) => Ok(json!(cell.id())),
            ("todo", [list]) => {
                cell.record(CellRecord::TodoList(list.clone()));
                Ok(Value::Null)
            }
            ("question", [question]) => {
                cell.record(CellRecord::Question(question.clone()));
                Ok(Value::Null)
            }
            ("transcript-read", [read]) => {
                cell.record(CellRecord::TranscriptRead(read.clone()));
                Ok(Value::Null)
            }
            ("agent-message", [receipt]) => {
                // A receipt without a delivery status or a target shows
                // nothing, as with the Python kernel.
                if let Some(message) = parse_sent_agent_message(receipt) {
                    cell.record(CellRecord::SentAgentMessage(message));
                }
                Ok(Value::Null)
            }
            ("attach-images", [Value::Array(paths)]) => super::attach::attach(&cell, paths),
            ("hold", [seconds]) => {
                let seconds = seconds
                    .as_f64()
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .ok_or_else(|| anyhow!("%cell: hold takes a number of seconds"))?;
                cell.hold(Some(Instant::now() + Duration::from_secs_f64(seconds)));
                if let Some(observer) = &self.session.on_cell_wait {
                    observer(CellWaitFact::Started);
                }
                Ok(Value::Null)
            }
            ("release", [Value::String(status)]) => {
                cell.release();
                let fact = match status.as_str() {
                    "met" => CellWaitFact::Met,
                    "timed_out" => CellWaitFact::TimedOut,
                    other => bail!("%cell: a wait ends met or timed_out, not {other}"),
                };
                if let Some(observer) = &self.session.on_cell_wait {
                    observer(fact);
                }
                Ok(Value::Null)
            }
            ("status" | "wait", [Value::String(id), ..]) => {
                self.control(&Control::Status(id.clone()))
            }
            ("cancel", [Value::String(id)]) => self.control(&Control::Cancel(id.clone())),
            ("list", []) => self.control(&Control::List),
            (operation, _) => bail!("%cell: malformed {operation} call"),
        }
    }

    /// Carry out `control` as far as it goes without waiting.
    fn control(&self, control: &Control) -> anyhow::Result<Value> {
        let cells = self
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let state = |background: &BackgroundCell| match &*background.ended.borrow() {
            Some(ending) => (ending.status, json!(ending.text), ending.duration_ms),
            None => (
                "running",
                Value::Null,
                background.started.elapsed().as_millis() as u64,
            ),
        };
        let id = match control {
            Control::List => {
                return Ok(Value::Array(
                    cells
                        .iter()
                        .map(|background| {
                            let (status, _, duration_ms) = state(background);
                            json!({
                                "cell": background.cell.id(),
                                "status": status,
                                "duration-ms": duration_ms,
                            })
                        })
                        .collect(),
                ));
            }
            Control::Status(id) | Control::Wait(id, _) | Control::Cancel(id) => id,
        };
        let background = cells
            .iter()
            .find(|background| background.cell.id() == id)
            .ok_or_else(|| {
                anyhow!("no background cell has the id {id}; (cell-list) shows the ones there are")
            })?;
        match control {
            Control::Cancel(_) if background.ended.borrow().is_none() => {
                self.interrupt(&background.cell);
            }
            Control::Cancel(_) | Control::Status(_) | Control::Wait(..) | Control::List => {}
        }
        let (status, text, duration_ms) = state(background);
        if status != "running" {
            // The model has the cell's end now: a notice not yet delivered is stale.
            let withdraw = {
                let mut notice = background
                    .notice
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                notice.read = true;
                let withdraw = notice.accepted && !notice.withdrawn;
                notice.withdrawn |= withdraw;
                withdraw
            };
            if withdraw {
                self.withdraw_notice(id);
            }
        }
        let output = {
            let state = background
                .cell
                .0
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let bytes: Vec<u8> = state.background_output.iter().copied().collect();
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if state.background_output_dropped {
                format!(
                    "[... older output dropped: kept the newest {BACKGROUND_OUTPUT_BYTES} bytes ...]\n{text}"
                )
            } else {
                text
            }
        };
        Ok(json!({
            "cell": id,
            "status": status,
            "text": text,
            "output": output,
            "duration-ms": duration_ms,
        }))
    }

    /// Tell the host the model already read cell `id`'s end (`cell.consumed`).
    fn withdraw_notice(&self, id: &str) {
        if let Some(handler) = self.handlers.get("cell.consumed").cloned() {
            let data = json!({ "type": "cell.consumed", "cell": id });
            self.runtime.spawn(async move {
                if let Err(error) = handler(HostRequestPayload { data }).await {
                    tracing::warn!("the host refused cell.consumed: {error:#}");
                }
            });
        }
    }
}

/// Define the cell API in a session's engine, whose host bridge is loaded:
/// the session's two ports, the `%cell` primitive, and the functions over it.
///
/// # Errors
///
/// The Scheme definitions failed to load.
pub(super) fn register(engine: &mut Engine, cells: &Arc<Cells>) -> anyhow::Result<()> {
    for (name, stream) in [
        ("%cell-output", Stream::Output),
        ("%cell-errors", Stream::Errors),
    ] {
        let port = || {
            SteelVal::new_dyn_writer_port(CellOutput {
                cells: Arc::clone(cells),
                stream,
            })
        };
        engine.register_value(name, port());
        // What a macro body prints is its cell's output too.
        engine.with_macro_engine(|macros| {
            macros.register_value(name, port());
        });
    }
    let answering = Arc::clone(cells);
    engine.register_value(
        "%cell",
        SteelVal::anonymous_boxed_function(Arc::new(move |arguments: &[SteelVal]| {
            let arguments = arguments
                .iter()
                .cloned()
                .map(super::limits::plain_data)
                .collect::<Result<Vec<Value>, SteelErr>>()?;
            answering
                .call(&arguments)
                .map(native::scheme_value)
                .map_err(|error| SteelErr::new(ErrorKind::Generic, format!("{error:#}")))
        })),
    );
    super::contracts::load(engine, CELL_API)
        .map_err(|error| anyhow!("the Scheme cell API failed to load: {error}"))?;
    Ok(())
}

/// Run `code` as one cell of `session`: wait for the engine, run it there,
/// stream what it prints, and return when it ends, is stopped, or outlives
/// its yield window.
///
/// # Errors
///
/// The cell's thread failed.
pub(super) async fn execute(
    session: Arc<SessionEngine>,
    code: String,
    options: KernelExecuteOptions<'_>,
) -> anyhow::Result<ExecuteResult> {
    let started = Instant::now();
    let elapsed = || Some(started.elapsed().as_millis() as u64);
    let KernelExecuteOptions {
        signal,
        timeout_ms,
        on_stream,
        on_late_sent_agent_message,
    } = options;
    if signal.as_ref().is_some_and(AbortSignal::is_cancelled) {
        return Ok(ExecuteResult {
            status: ExecuteStatus::Aborted,
            ..ExecuteResult::default()
        });
    }
    let cells = &session.cells;
    // With a window the cell moves to the background when it ends; without
    // one the model's own timeout interrupts it.
    let window = cells.session.cell_window.arm(timeout_ms);
    let limit = window.or(timeout_ms).map(Duration::from_millis);
    let deadline = async {
        match limit {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending().await,
        }
    };
    let aborted = async {
        match &signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline, aborted);
    let failed = |ename: &str, evalue: String| ExecuteResult {
        status: ExecuteStatus::Error,
        error: Some(KernelErrorInfo {
            ename: ename.to_string(),
            traceback: vec![evalue.clone()],
            evalue,
        }),
        duration_ms: elapsed(),
        ..ExecuteResult::default()
    };

    let turn = if let Ok(turn) = Arc::clone(&session.turn).try_lock_owned() {
        turn
    } else {
        // The engine is busy. A control form needs no engine.
        let words: Vec<&str> = code
            .trim()
            .strip_prefix('(')
            .and_then(|form| form.strip_suffix(')'))
            .map(|form| form.split_whitespace().collect())
            .unwrap_or_default();
        let named = |word: &str| {
            word.strip_prefix('"')
                .and_then(|word| word.strip_suffix('"'))
                .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
                .map(str::to_string)
        };
        let control = match words.as_slice() {
            ["cell-list"] => Some(Control::List),
            ["cell-status", id] => named(id).map(Control::Status),
            ["cell-cancel", id] => named(id).map(Control::Cancel),
            ["cell-wait", id, seconds] => named(id)
                .zip(seconds.parse::<f64>().ok())
                .map(|(id, seconds)| Control::Wait(id, seconds)),
            _ => None,
        };
        if let Some(control) = control {
            let answer = match cells.control(&control) {
                Ok(first) if first["status"] == json!("running") => {
                    let wait = match &control {
                        Control::Wait(_, seconds) if seconds.is_finite() => {
                            Duration::from_secs_f64(seconds.clamp(0.0, MAX_WAIT_SECONDS))
                        }
                        Control::Cancel(_) => CANCEL_WAIT,
                        Control::Wait(..) | Control::Status(_) | Control::List => Duration::ZERO,
                    };
                    let ended = cells
                        .background
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .iter()
                        .find(|background| first["cell"] == json!(background.cell.id()))
                        .map(|background| background.ended.subscribe());
                    if let Some(mut ended) = ended {
                        tokio::select! {
                            () = async {
                                // An error means the cell's record is gone: nothing to wait for.
                                let _ = ended.wait_for(Option::is_some).await;
                            } => {}
                            () = tokio::time::sleep(wait) => {}
                            () = &mut aborted => {}
                            () = &mut deadline => {}
                        }
                    }
                    cells.control(&Control::Status(
                        first["cell"].as_str().unwrap_or_default().to_string(),
                    ))
                }
                answer => answer,
            };
            return Ok(match answer {
                Ok(answer) => ExecuteResult {
                    result: Some(native::scheme_value(answer).to_string()),
                    duration_ms: elapsed(),
                    ..ExecuteResult::default()
                },
                Err(error) => failed("SchemeError", format!("{error:#}")),
            });
        }
        tokio::select! {
            turn = Arc::clone(&session.turn).lock_owned() => turn,
            () = &mut aborted => {
                return Ok(ExecuteResult {
                    status: ExecuteStatus::Aborted,
                    duration_ms: elapsed(),
                    ..ExecuteResult::default()
                });
            }
            () = &mut deadline => {
                let holder = cells
                    .background
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .rev()
                    .find(|background| background.ended.borrow().is_none())
                    .map(|background| {
                        (
                            background.cell.id().to_string(),
                            background.started.elapsed().as_secs(),
                        )
                    });
                return Ok(failed(
                    "KernelBusy",
                    match holder {
                        Some((id, seconds)) => format!(
                            "This cell did not run. The session's Scheme engine runs one cell at a \
        time, and cell {id} still holds it in the background ({seconds} s so far). Its [cell-done] notice \
        arrives when it ends. Until then the kernel answers only these, each as a whole cell: \
        (cell-status \"{id}\") for its state and what it has printed, (cell-wait \"{id}\" seconds) to wait \
        for it, (cell-cancel \"{id}\") to stop it."
                        ),
                        None => "This cell did not run: the session's Scheme engine was still running \
        another cell when this cell's wait ended. Run it again."
                            .to_string(),
                    },
                ));
            }
        }
    };

    let (stream, mut streamed) = tokio::sync::mpsc::unbounded_channel();
    let cell = CellHandle(Arc::new(Cell {
        id: uuid::Uuid::new_v4().simple().to_string(),
        signal: AbortSignal::new(),
        session: Arc::clone(&cells.session),
        stream,
        state: Mutex::new(CellState::default()),
        thread: std::sync::OnceLock::new(),
        stuck: AtomicBool::new(false),
        hold: Mutex::new(Hold::default()),
        hold_changed: tokio::sync::Notify::new(),
    }));
    let running = Arc::clone(&session);
    let run = cell.clone();
    // The cell's value as text, or its error.
    let mut thread = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
        // Held to the end: the next cell gets the engine when this one is done.
        let _turn = turn;
        // The cell runs on a thread of its own, whose stack is known and far
        // larger than a runtime thread's: the depth limits of [`limits`] are
        // what a cell meets first, never the end of the stack (which would
        // end the process, not the cell). The stack is address space; only
        // the pages a cell touches are memory, and they go with the thread.
        let on_cell_thread = pa_types::session_scope::propagate(|| {
            let _runtime = running.cells.runtime.enter();
            let cells = &running.cells;
            let mut engine = running
                .engine
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *running.cell.lock().unwrap_or_else(PoisonError::into_inner) = run.signal();
            // Set once, here: a cell has one thread.
            let _ = run.0.thread.set(std::thread::current());
            *cells.running.lock().unwrap_or_else(PoisonError::into_inner) = Some(run.clone());
            CURRENT.with(|current| *current.borrow_mut() = Some(run.clone()));
            // Only a cell without a window is stopped at its limit: a command it
            // waits for hands over before that.
            running.jobs.cell_started(
                limit
                    .filter(|_| window.is_none())
                    .map(|limit| started + limit),
            );
            let outcome = if run.0.signal.is_cancelled() {
                Err("the cell was stopped before it started".to_string())
            } else if code.len() > super::limits::SOURCE_BYTES {
                Err(format!(
                    "the cell is {} bytes of code and the kernel compiles at most {} in one cell: \
                 run it as several cells, or keep its data in a file and read it",
                    code.len(),
                    super::limits::SOURCE_BYTES
                ))
            } else if let Some(refusal) = super::limits::source_refusal(&code) {
                Err(refusal)
            } else {
                engine
                    .with_macro_engine(|macros| macros.run(CELL_PORTS))
                    .transpose()
                    .and_then(|_| engine.run(CELL_PORTS))
                    .and_then(|_| engine.run(code))
                    .map(|values| {
                        values
                            .into_iter()
                            .rev()
                            .find(|value| !matches!(value, SteelVal::Void))
                            .map(|value| value.to_string())
                    })
                    .map_err(|error| error.to_string())
            };
            CURRENT.with(|current| current.borrow_mut().take());
            running.jobs.cell_finished();
            // Under the lock an interrupt takes: one that came for this cell is
            // cleared here, and none can arrive for it afterwards.
            let mut on_engine = cells.running.lock().unwrap_or_else(PoisonError::into_inner);
            *on_engine = None;
            for controller in &cells.controllers {
                controller.resume();
            }
            drop(on_engine);
            // A cell that reloaded the session's resources gets its skills'
            // changed sources evaluated before the next cell.
            running.skills.load_if_reloaded(&mut engine);
            outcome
        });
        std::thread::scope(|scope| {
            match std::thread::Builder::new()
                .name("scheme-cell".to_string())
                .stack_size(super::limits::cell_stack_bytes())
                .spawn_scoped(scope, on_cell_thread)
            {
                Ok(cell_thread) => cell_thread
                    .join()
                    // The panic was recorded where it was raised: this thread
                    // ends as the cell's thread did.
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                Err(error) => Err(format!("the cell's thread could not start: {error}")),
            }
        })
    });

    let report = |(stream, text): (Stream, String)| {
        if let Some(on_stream) = on_stream {
            on_stream(
                &text,
                match stream {
                    Stream::Output => "stdout",
                    Stream::Errors => "stderr",
                },
            );
        }
    };
    // An aborted tool call's future may be dropped instead of polled again:
    // the cell is stopped then too, or it would hold the engine to its end.
    let mut dropped = StopOnDrop {
        cells: Arc::clone(cells),
        cell: Some(cell.clone()),
    };
    // Printed but not yet reported, and when the last report went out.
    let mut unsent: Vec<(Stream, String)> = Vec::new();
    let mut reported: Option<tokio::time::Instant> = None;
    // The window's first end has passed: from then on it ends when the
    // cell's waits inside the turn let it.
    let mut window_passed = false;
    let waited = loop {
        let due = match (unsent.is_empty(), reported) {
            (true, _) => None,
            (false, Some(reported)) => Some(reported + STREAM_INTERVAL),
            (false, None) => Some(tokio::time::Instant::now()),
        };
        tokio::select! {
            joined = &mut thread => break Waited::Ended(joined),
            Some((stream, text)) = streamed.recv() => match unsent.last_mut() {
                Some((last, held)) if *last == stream => held.push_str(&text),
                Some(_) | None => unsent.push((stream, text)),
            },
            () = async {
                match due {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => std::future::pending().await,
                }
            } => {
                unsent.drain(..).for_each(&report);
                reported = Some(tokio::time::Instant::now());
            }
            () = &mut aborted => break Waited::Stopped(Stop::Aborted),
            () = &mut deadline, if !window_passed => match window {
                Some(_) => window_passed = true,
                None => break Waited::Stopped(Stop::TimedOut),
            },
            // Whether the window ended, or a wait started or ended first.
            ended = async {
                let changed = cell.0.hold_changed.notified();
                match limit.and_then(|limit| cell.window_end(started + limit)) {
                    Some(end) if end <= Instant::now() => true,
                    Some(end) => tokio::select! {
                        () = tokio::time::sleep_until(end.into()) => true,
                        () = changed => false,
                    },
                    None => {
                        changed.await;
                        false
                    }
                }
            }, if window_passed => if ended {
                break Waited::Outlived;
            },
        }
    };
    dropped.cell = None;
    let take_shown = |late: Option<LateSentAgentMessageCallback>| {
        let mut state = cell.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.settled = true;
        state.late = late;
        let mut shown = std::mem::take(&mut state.shown);
        // A saved session keeps what is cut in its spill store, so its cap
        // may follow the context the session has left; without a store a
        // tighter cap would only destroy more.
        let store = cells
            .session
            .artifact_dir
            .as_deref()
            .map(crate::kernel::spill::SpillStore::for_session_dir);
        let cap = match &store {
            Some(_) => cells.session.output_budget.max_output_chars(),
            None => DEFAULT_MAX_OUTPUT_CHARS,
        };
        // A preview's noise patterns are learned per command family: the
        // program of the cell's first `(bash ...)` command, else the kernel
        // tool.
        let family = shown
            .bash_commands
            .as_ref()
            .and_then(|commands| {
                let last = commands.first.rsplit(['&', ';', '|']).next()?;
                last.split_whitespace().next()
            })
            .unwrap_or(pa_types::ai::KERNEL_TOOL_NAME)
            .to_string();
        let capped = |printed: &mut Printed| {
            let text = String::from_utf8_lossy(&std::mem::take(&mut printed.kept)).into_owned();
            let mut cut = match (text.char_indices().nth(cap), &store) {
                // Paged out: within the cap, the lines of what was kept that
                // carry information (its start when no preview applies),
                // then the handle that reads all of it.
                (Some((end, _)), Some(store)) => {
                    let preview = store
                        .preview(&text, cap, &family)
                        .unwrap_or_else(|| text[..end].to_string());
                    let handle = store.spill(crate::kernel::spill::SpillCopy::of_text(&text));
                    let not_shown = handle.total_chars.saturating_sub(preview.chars().count());
                    format!("{preview}\n{}", handle.marker(not_shown))
                }
                (Some((end, _)), None) => {
                    format!(
                        "{}\n[... output truncated at {cap} chars ...]",
                        &text[..end]
                    )
                }
                (None, _) => text,
            };
            // The preview and the handle cover what was kept: say what was not.
            let dropped = std::mem::take(&mut printed.dropped);
            if dropped > 0 {
                let _ = write!(
                    cut,
                    "\n[... {dropped} more bytes were printed after the first {OUTPUT_KEPT_BYTES} and were not kept ...]"
                );
            }
            cut
        };
        shown.stdout = capped(&mut state.output);
        // What the cell printed comes before a note the kernel added.
        let errors = capped(&mut state.errors);
        if !errors.is_empty() {
            shown.stderr = if shown.stderr.is_empty() {
                errors
            } else {
                format!("{errors}\n{}", shown.stderr)
            };
        }
        shown.duration_ms = elapsed();
        shown
    };
    let (joined, stopped) = match waited {
        Waited::Ended(joined) => (joined, None),
        Waited::Stopped(stop) => {
            cells.interrupt(&cell);
            (thread.await, Some(stop))
        }
        Waited::Outlived => {
            let shown = take_shown(on_late_sent_agent_message);
            let background = Arc::new(BackgroundCell {
                cell: cell.clone(),
                started,
                ended: tokio::sync::watch::Sender::new(None),
                notice: Mutex::new(Notice::default()),
            });
            {
                let mut kept = cells
                    .background
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                kept.push_back(Arc::clone(&background));
                while kept.len() > FINISHED_KEPT {
                    kept.pop_front();
                }
            }
            if let Some(observer) = &cells.session.on_background_cell {
                observer(BackgroundCellFact::Backgrounded);
            }
            // Live background work from here until its notice is handed over.
            let live = cells.session.background.enter();
            let cells = Arc::clone(cells);
            tokio::spawn(async move {
                let _live = live;
                let ending = match thread.await {
                    Ok(Ok(value)) => ("ok", value.unwrap_or_default()),
                    Ok(Err(_)) if background.cell.0.signal.is_cancelled() => {
                        ("interrupted", "the cell was stopped".to_string())
                    }
                    Ok(Err(error)) => ("error", error),
                    Err(error) => ("error", format!("the cell's thread failed: {error}")),
                };
                let ran_for = Some(background.started.elapsed());
                // Reported before the end is published: whoever reads the
                // cell's end finds its fact already counted.
                if let Some(observer) = &cells.session.on_background_cell {
                    observer(match ending.0 {
                        "ok" => BackgroundCellFact::EndedOk { ran_for },
                        "error" => BackgroundCellFact::EndedWithError { ran_for },
                        _ => BackgroundCellFact::Interrupted,
                    });
                }
                background.ended.send_replace(Some(CellEnding {
                    status: ending.0,
                    text: ending.1.clone(),
                    duration_ms: background.started.elapsed().as_millis() as u64,
                }));
                let unread = !background
                    .notice
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .read;
                let handler = cells.handlers.get("cell.completed").cloned();
                if let (true, false, Some(handler)) =
                    (unread, cells.killed.load(Ordering::SeqCst), handler)
                {
                    let (output, edited) = {
                        let state = background
                            .cell
                            .0
                            .state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let bytes: Vec<u8> = state.background_output.iter().copied().collect();
                        (
                            String::from_utf8_lossy(&bytes).into_owned(),
                            state.late_edits.clone(),
                        )
                    };
                    let lines: Vec<&str> = output.lines().collect();
                    let tail = lines[lines.len().saturating_sub(NOTICE_TAIL_LINES)..].join("\n");
                    let tail_chars = tail.chars().count();
                    let cut = lines.len() > NOTICE_TAIL_LINES || tail_chars > NOTICE_TAIL_CHARS;
                    let shown: String = tail
                        .chars()
                        .skip(tail_chars.saturating_sub(NOTICE_TAIL_CHARS))
                        .collect();
                    let text_chars = ending.1.chars().count();
                    let text: String = ending
                        .1
                        .chars()
                        .skip(text_chars.saturating_sub(NOTICE_TAIL_CHARS))
                        .collect();
                    let id = background.cell.id().to_string();
                    // No card showed the edits the cell made in the
                    // background: the notice names their files.
                    let shown = if edited.is_empty() {
                        shown
                    } else {
                        format!(
                            "[files edited after the call returned: {}]\n{shown}",
                            edited.join(", ")
                        )
                    };
                    // Cut output says here where the rest is, so the notice
                    // adds no wording of its own.
                    let data = json!({
                        "type": "cell.completed",
                        "cell": id,
                        "status": ending.0,
                        "text": text,
                        "output": if cut {
                            format!("[... newest lines only; (cell-status \"{id}\") has the rest ...]\n{shown}")
                        } else {
                            shown
                        },
                        "outputTruncated": false,
                    });
                    match handler(HostRequestPayload { data }).await {
                        Ok(_) => {
                            let withdraw = {
                                let mut notice = background
                                    .notice
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner);
                                notice.accepted = true;
                                let withdraw = notice.read && !notice.withdrawn;
                                notice.withdrawn |= withdraw;
                                withdraw
                            };
                            if withdraw {
                                cells.withdraw_notice(&id);
                            }
                        }
                        Err(error) => {
                            tracing::warn!("the host refused cell.completed: {error:#}");
                        }
                    }
                }
            });
            return Ok(ExecuteResult {
                backgrounded_cell: Some(cell.id().to_string()),
                ..shown
            });
        }
    };
    while let Ok(printed) = streamed.try_recv() {
        unsent.push(printed);
    }
    unsent.into_iter().for_each(&report);
    let outcome = joined.map_err(|error| anyhow!("the Scheme cell's thread failed: {error}"))?;
    let shown = take_shown(None);
    let error = |ename: &str, evalue: String| {
        Some(KernelErrorInfo {
            ename: ename.to_string(),
            traceback: vec![evalue.clone()],
            evalue,
        })
    };
    let result = match (outcome, stopped) {
        (Ok(value), _) => ExecuteResult {
            result: value,
            ..shown
        },
        (Err(_), Some(Stop::Aborted)) => ExecuteResult {
            status: ExecuteStatus::Aborted,
            ..shown
        },
        (Err(_), Some(Stop::TimedOut)) => ExecuteResult {
            status: ExecuteStatus::Error,
            error: error(
                "Timeout",
                format!(
                    "the cell was interrupted after {} ms",
                    limit.unwrap_or_default().as_millis()
                ),
            ),
            ..shown
        },
        (Err(message), None) => ExecuteResult {
            status: ExecuteStatus::Error,
            error: error("SchemeError", message),
            ..shown
        },
    };
    // A failed command the cell waited for carries the source lines its
    // output names, as the Python kernel's cells did.
    let failures = std::mem::take(
        &mut cell
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .command_failures,
    );
    let error_context = cells
        .session
        .error_context
        .enrich(
            crate::kernel::error_context::CellFailure {
                traceback: None,
                streams: ["", ""],
                commands: &failures,
            },
            cells
                .session
                .artifact_dir
                .as_deref()
                .map(crate::kernel::spill::SpillStore::for_session_dir)
                .as_ref(),
        )
        .await;
    let result = ExecuteResult {
        error_context,
        ..result
    };
    // A cell that ended on its own teaches the learned window its time.
    match (&result.status, &result.error) {
        (ExecuteStatus::Ok, _) => cells.session.cell_window.observe(started.elapsed()),
        (ExecuteStatus::Error, Some(error)) if error.ename == "SchemeError" => {
            cells.session.cell_window.observe(started.elapsed());
        }
        (ExecuteStatus::Error | ExecuteStatus::Aborted, _) => {}
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
