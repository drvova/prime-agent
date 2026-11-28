//! The kernel scenarios: no TUI, the daemon wire only. Each run creates a
//! session whose faux script (the daemon's verification engine: the real
//! agent loop over a scripted provider) issues real `eval` tool calls, so
//! the measured path is the product's own (worker spawn, session create,
//! the in-process Scheme kernel) plus the tool round-trip: the provider is
//! the only fake part. The runtime daemon is driven on wire2 (the script is
//! the daemon's own, from its environment).

use std::collections::VecDeque;
use std::io::{Read as _, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use pa_types::daemon::framing::{
    encode_private_frame, PrivateFrameDecoder, DEFAULT_PRIVATE_FRAME_LIMITS,
};
use pa_types::wire2::{ClientFrame, Outcome, ServerFrame, Since, WIRE2_VERSION};
use serde_json::{json, Value};

use super::{millis, Measured, RunCtx};
use crate::bench::Scenario;

/// The spawned child's sibling-unique name.
const CHILD_NAME: &str = "perf-child";

/// An `eval` tool call record for a faux script.
fn cell(code: &str) -> Value {
    json!({ "content": [
        { "type": "toolCall", "name": "eval", "arguments": { "code": code } },
    ] })
}

/// An in-kernel wall-clock stamp (epoch ms) a cell wrote; `None` until the
/// file holds a whole number (the cell writes it in one call).
fn read_stamp(path: &Path) -> Option<f64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

/// One run's faux scripts: the session's, and, for subagent-first-eval,
/// its child's (named [`CHILD_NAME`]) with the two stamp files the cells
/// write.
pub(super) struct Scripts {
    parent: Value,
    child: Option<Value>,
    stamps: Option<(PathBuf, PathBuf)>,
}

impl Scripts {
    /// The runtime daemon's whole script: the session's, its child's under
    /// `children`.
    pub(super) fn daemon_script(&self) -> Value {
        let mut script = self.parent.clone();
        if let Some(child) = &self.child {
            script["children"] = json!({ CHILD_NAME: child });
        }
        script
    }
}

/// A faux script of `responses`.
fn script(responses: &[Value]) -> Value {
    json!({ "engine": "faux", "responses": responses })
}

/// The faux scripts of `scenario`'s run `run`, whose daemon is started with
/// `socket` (their files go beside it).
///
/// # Errors
///
/// A stale stamp of an earlier run could not be removed.
pub(super) fn scripts(socket: &Path, scenario: Scenario, run: usize) -> anyhow::Result<Scripts> {
    let dir = socket.parent().context("a socket dir")?.to_path_buf();
    let first_eval = || {
        script(&[
            cell("(displayln \"first eval\") (+ 1 2)"),
            json!({ "text": "done" }),
        ])
    };
    match scenario {
        Scenario::FirstEval | Scenario::WarmFirstEval => Ok(Scripts {
            parent: first_eval(),
            child: None,
            stamps: None,
        }),
        Scenario::SubagentFirstEval => {
            let spawned = dir.join(format!("spawned-{run}.txt"));
            let receipt = dir.join(format!("child-cell-{run}.txt"));
            // A stamp left by an earlier invocation over the same output dir
            // would end the wait before this run's child ever ran.
            for stale in [&spawned, &receipt] {
                match std::fs::remove_file(stale) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| format!("remove {}", stale.display()))
                    }
                }
            }
            let child = script(&[
                cell(&format!(
                    "(fs-write {receipt:?} (number->string (current-milliseconds)))",
                    receipt = receipt.display().to_string(),
                )),
                json!({ "text": "child done" }),
            ]);
            let parent_cell = format!(
                "(define started (current-milliseconds))\n\
                 (rlm-spawn \"run the perf cell\" #:name {CHILD_NAME:?})\n\
                 (fs-write {spawned:?} (number->string started))",
                spawned = spawned.display().to_string(),
            );
            Ok(Scripts {
                parent: script(&[cell(&parent_cell), json!({ "text": "done" })]),
                child: Some(child),
                stamps: Some((spawned, receipt)),
            })
        }
        Scenario::ColdStart
        | Scenario::ColdReady
        | Scenario::WarmStart
        | Scenario::WarmReady
        | Scenario::AgentsView
        | Scenario::AgentsViewWarm
        | Scenario::OpenSession
        | Scenario::SessionSwitch
        | Scenario::AtCompletion
        | Scenario::TranscriptPaging
        | Scenario::TranscriptTop
        | Scenario::IdleCost => anyhow::bail!("{scenario:?} is a TUI scenario"),
    }
}

/// A connection to the run's runtime daemon.
struct Wire(Wire2Client);

impl Wire {
    fn connect(socket: &Path, deadline: Instant) -> anyhow::Result<Self> {
        Ok(Wire(Wire2Client::connect(
            &super::listen_socket(socket),
            deadline,
        )?))
    }

    fn deadline(&self) -> Instant {
        self.0.deadline
    }

    /// How many built engines wait for the next sessions (the `sessions.list`
    /// answer's `spareKernels`).
    fn spare_kernels(&mut self) -> anyhow::Result<u64> {
        let list = self.0.request("sessions.list", |id| ClientFrame::Query {
            id,
            name: "sessions.list".to_string(),
            args: json!({ "open": true }),
        })?;
        Ok(list["spareKernels"].as_u64().unwrap_or(0))
    }

    /// Creates a session in the corpus project and sends the one prompt that
    /// starts its scripted turn.
    fn start_session(&mut self, ctx: &RunCtx) -> anyhow::Result<()> {
        let client = &mut self.0;
        let cwd = ctx.project_dir.display().to_string();
        let created = client.request("session.create", |id| ClientFrame::Call {
            id,
            method: "session.create".to_string(),
            args: json!({ "cwd": cwd }),
        })?;
        let session = created["session"]
            .as_str()
            .with_context(|| format!("no session id in the create reply: {created}"))?
            .to_string();
        client.subscribe(&format!("agent/{session}/stream"))?;
        client.request("the prompt", |id| ClientFrame::Publish {
            id,
            topic: pa_types::event_log::Topic::new(format!("agent/{session}/input"))
                .expect("a session's input topic"),
            payload: json!({ "text": "run the perf cell" }),
        })?;
        Ok(())
    }

    /// The instant the next `eval` call's end event was read (the wire's end
    /// event carries only the call id, so the start's id pairs them). A
    /// failed cell fails the run: a measured error path is not the metric.
    fn kernel_cell(&mut self, what: &str) -> anyhow::Result<Instant> {
        let mut event = |what: &str, wanted: &dyn Fn(&Value) -> bool| self.0.event(what, wanted);
        let (start, _) = event(&format!("{what} start"), &|event| {
            event["type"] == "tool_execution_start" && event["toolName"] == "eval"
        })?;
        let id = start["toolCallId"].clone();
        let (end, ended) = event(&format!("{what} result"), &|event| {
            event["type"] == "tool_execution_end" && event["toolCallId"] == id
        })?;
        // A raised cell reports `details.status: "error"` with `isError`
        // false (the tool ran; the code failed): both fail the run.
        anyhow::ensure!(
            end["isError"] != true && end["result"]["details"]["status"] != "error",
            "{what} failed: {end}"
        );
        Ok(ended)
    }
}

/// One wire2 client over the runtime daemon's socket: requests out, their
/// replies and the subscribed session's stream events in (events read while
/// waiting for a reply are kept in order for the next event wait).
struct Wire2Client {
    stream: UnixStream,
    decoder: PrivateFrameDecoder,
    frames: VecDeque<ServerFrame>,
    pending: VecDeque<Value>,
    deadline: Instant,
    next_id: u64,
}

impl Wire2Client {
    fn connect(socket: &Path, deadline: Instant) -> anyhow::Result<Self> {
        let stream =
            UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut client = Self {
            stream,
            decoder: PrivateFrameDecoder::new(DEFAULT_PRIVATE_FRAME_LIMITS),
            frames: VecDeque::new(),
            pending: VecDeque::new(),
            deadline,
            next_id: 0,
        };
        client.send(&ClientFrame::Hello {
            version: WIRE2_VERSION,
            client: "pa-perf".to_string(),
        })?;
        loop {
            match client.frame("the daemon welcome")? {
                ServerFrame::Welcome { .. } => return Ok(client),
                ServerFrame::Closing { reason } => anyhow::bail!("the daemon closed: {reason}"),
                ServerFrame::Event { .. }
                | ServerFrame::Backfilled { .. }
                | ServerFrame::Reply { .. } => {}
            }
        }
    }

    fn send(&mut self, frame: &ClientFrame) -> anyhow::Result<()> {
        let bytes = encode_private_frame(
            &serde_json::to_value(frame)?,
            &[],
            DEFAULT_PRIVATE_FRAME_LIMITS,
        )?;
        self.stream.write_all(&bytes)?;
        self.stream.flush()?;
        Ok(())
    }

    fn frame(&mut self, what: &str) -> anyhow::Result<ServerFrame> {
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            if let Some(frame) = self.frames.pop_front() {
                return Ok(frame);
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => anyhow::bail!("the daemon closed the connection while awaiting {what}"),
                Ok(read) => {
                    for frame in self.decoder.push(&chunk[..read])? {
                        self.frames.push_back(
                            serde_json::from_value(frame.header).context("a wire2 server frame")?,
                        );
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    anyhow::ensure!(Instant::now() < self.deadline, "timed out awaiting {what}");
                }
                Err(error) => return Err(error).with_context(|| format!("read {what}")),
            }
        }
    }

    /// Sends the request `frame` builds with a fresh id and returns its ok
    /// reply's result.
    fn request(
        &mut self,
        what: &str,
        frame: impl FnOnce(String) -> ClientFrame,
    ) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = format!("perf-{}", self.next_id);
        self.send(&frame(id.clone()))?;
        loop {
            match self.frame(&format!("the {what} reply"))? {
                ServerFrame::Reply {
                    id: replied,
                    outcome,
                } if replied == id => {
                    return match outcome {
                        Outcome::Ok { result } => Ok(result),
                        Outcome::Error { message, .. } => anyhow::bail!("{what} failed: {message}"),
                    }
                }
                ServerFrame::Event { event, .. } => self.pending.push_back(event.event.payload),
                ServerFrame::Closing { reason } => anyhow::bail!("the daemon closed: {reason}"),
                ServerFrame::Reply { .. }
                | ServerFrame::Welcome { .. }
                | ServerFrame::Backfilled { .. } => {}
            }
        }
    }

    /// Subscribes to `filter`'s live events, once its (empty) replay is done.
    fn subscribe(&mut self, filter: &str) -> anyhow::Result<()> {
        self.send(&ClientFrame::Subscribe {
            sub_id: "perf".to_string(),
            filter: pa_types::event_log::TopicFilter::new(filter)?,
            since: Since::Now,
        })?;
        loop {
            match self.frame("the subscription")? {
                ServerFrame::Backfilled { .. } => return Ok(()),
                ServerFrame::Event { event, .. } => self.pending.push_back(event.event.payload),
                ServerFrame::Closing { reason } => anyhow::bail!("the daemon closed: {reason}"),
                ServerFrame::Reply { .. } | ServerFrame::Welcome { .. } => {}
            }
        }
    }

    /// The next stream event matching `wanted` (earlier events are dropped),
    /// with the instant this client read it.
    fn event(
        &mut self,
        what: &str,
        wanted: impl Fn(&Value) -> bool,
    ) -> anyhow::Result<(Value, Instant)> {
        loop {
            let payload = match self.pending.pop_front() {
                Some(payload) => payload,
                None => match self.frame(what)? {
                    ServerFrame::Event { event, .. } => event.event.payload,
                    ServerFrame::Closing { reason } => anyhow::bail!("the daemon closed: {reason}"),
                    ServerFrame::Reply { .. }
                    | ServerFrame::Welcome { .. }
                    | ServerFrame::Backfilled { .. } => continue,
                },
            };
            let at = Instant::now();
            if wanted(&payload) {
                return Ok((payload, at));
            }
        }
    }
}

/// The runtime daemon's stop (`daemon.shutdown`), its reply read; bounded,
/// and nothing to do when it does not answer.
pub(super) fn runtime_shutdown(socket: &Path) {
    let Ok(mut client) = Wire2Client::connect(socket, Instant::now() + Duration::from_secs(5))
    else {
        return;
    };
    let _ = client.request("daemon.shutdown", |id| ClientFrame::Call {
        id,
        method: "daemon.shutdown".to_string(),
        args: json!({}),
    });
}

/// Run one kernel scenario against the run's daemon (started with
/// `socket`), on `scripts`.
///
/// # Errors
///
/// Propagates wire, create/prompt, cell-failure, and deadline errors.
pub(super) fn measure(
    ctx: &RunCtx,
    socket: &Path,
    scenario: Scenario,
    scripts: &Scripts,
) -> anyhow::Result<Measured> {
    let mut wire = Wire::connect(socket, Instant::now() + ctx.liveness)?;
    match scenario {
        Scenario::FirstEval => {
            // The create command -> the first `eval` result: what a new
            // session's first Scheme cell waits on (worker spawn, session
            // create, the scripted call, and the engine taken from the spare).
            let started = Instant::now();
            wire.start_session(ctx)?;
            let ended = wire.kernel_cell("the first eval")?;
            Ok(Measured {
                settled: vec![millis(ended - started)],
                ..Measured::default()
            })
        }
        Scenario::SubagentFirstEval => subagent_first_eval(ctx, &mut wire, scripts),
        Scenario::WarmFirstEval => {
            // The daemon builds its spare engines off every session's path:
            // the measurement starts once it reports one.
            let deadline = Instant::now() + ctx.liveness;
            while wire.spare_kernels()? == 0 {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "the daemon never reported a spare engine"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let started = Instant::now();
            wire.start_session(ctx)?;
            let ended = wire.kernel_cell("the first eval on a warm daemon")?;
            Ok(Measured {
                settled: vec![millis(ended - started)],
                ..Measured::default()
            })
        }
        Scenario::ColdStart
        | Scenario::ColdReady
        | Scenario::WarmStart
        | Scenario::WarmReady
        | Scenario::AgentsView
        | Scenario::AgentsViewWarm
        | Scenario::OpenSession
        | Scenario::SessionSwitch
        | Scenario::AtCompletion
        | Scenario::TranscriptPaging
        | Scenario::TranscriptTop
        | Scenario::IdleCost => anyhow::bail!("{scenario:?} is a TUI scenario"),
    }
}

/// subagent-first-eval: the parent's `rlm-spawn` call -> the child's first
/// cell finishing. Both stamps are the kernels' own wall clocks (the
/// parent's before the spawn, the child's at the end of its first cell; one
/// host, one clock), so the span is the child create + its kernel + its
/// first cell, without the parent's own start. The product delivers the
/// child's task once the spawning cell returns, so the parent cell ends
/// right after the spawn and the child's receipt file is the end mark.
fn subagent_first_eval(
    ctx: &RunCtx,
    wire: &mut Wire,
    scripts: &Scripts,
) -> anyhow::Result<Measured> {
    let (spawned, receipt) = scripts
        .stamps
        .as_ref()
        .context("subagent-first-eval's scripts name its stamps")?;
    wire.start_session(ctx)?;
    wire.kernel_cell("the parent's spawn cell")?;
    let spawned_at = read_stamp(spawned)
        .with_context(|| format!("the spawn cell wrote no {}", spawned.display()))?;
    let finished_at = loop {
        if let Some(stamp) = read_stamp(receipt) {
            break stamp;
        }
        anyhow::ensure!(
            Instant::now() < wire.deadline(),
            "the child's first cell never wrote {}",
            receipt.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    Ok(Measured {
        settled: vec![finished_at - spawned_at],
        ..Measured::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection whose daemon already sent `payloads` (its socket's
    /// other end sends nothing more).
    fn scripted(payloads: Vec<Value>) -> Wire {
        let (ours, _theirs) = UnixStream::pair().expect("socket pair");
        Wire(Wire2Client {
            stream: ours,
            decoder: PrivateFrameDecoder::new(DEFAULT_PRIVATE_FRAME_LIMITS),
            frames: VecDeque::new(),
            pending: payloads.into(),
            deadline: Instant::now() + Duration::from_secs(5),
            next_id: 0,
        })
    }

    /// The wire's end event names no tool: the start's call id pairs it
    /// (another tool's end is skipped), and a raised cell — `isError` false,
    /// `details.status` error — fails the run.
    #[test]
    fn cells_pair_by_call_id_and_a_raised_cell_fails_the_run() {
        let start = |id: &str, tool: &str| json!({ "type": "tool_execution_start", "toolCallId": id, "toolName": tool });
        let end = |id: &str, status: &str| {
            json!({
                "type": "tool_execution_end",
                "toolCallId": id,
                "isError": false,
                "result": { "details": { "status": status } },
            })
        };
        let mut client = scripted(vec![
            start("bash-1", "bash"),
            start("cell-1", "eval"),
            end("bash-1", "error"),
            end("cell-1", "ok"),
            start("cell-2", "eval"),
            end("cell-2", "error"),
        ]);
        client.kernel_cell("the first cell").expect("the ok cell");
        let raised = client
            .kernel_cell("the second cell")
            .expect_err("a raised cell fails the run");
        assert!(
            raised.to_string().starts_with("the second cell failed"),
            "{raised}"
        );
    }
}
