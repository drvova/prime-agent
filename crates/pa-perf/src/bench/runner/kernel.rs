//! The python_repl kernel scenarios: no TUI, the daemon wire only. Each
//! run creates a session whose faux script (the daemon's verification
//! engine: the real agent loop over a scripted provider) issues real
//! `python_repl` tool calls, so the measured path is the product's kernel
//! boot (interpreter start, runtime import, readiness probe, bootstrap
//! cell) plus the tool round-trip — the provider is the only fake part.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{json, Value};

use super::{millis, Measured, RunCtx};
use crate::bench::Scenario;

/// The daemon wire protocol the e2es and the shutdown path speak.
const PROTOCOL: &str = "prime-agent.daemon";
const PROTOCOL_VERSION: u64 = 7;

/// The spawned child's sibling-unique name.
const CHILD_NAME: &str = "perf-child";

/// One wire client over the run's daemon socket: command envelopes out,
/// responses and `session_event` lines in (events read while waiting for
/// a response are kept in order for the next event wait).
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    pending: VecDeque<Value>,
    deadline: Instant,
    next_id: u64,
}

impl Client {
    fn connect(socket: &Path, deadline: Instant) -> anyhow::Result<Self> {
        let stream =
            UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        let writer = stream.try_clone()?;
        let mut client = Self {
            reader: BufReader::new(stream),
            writer,
            pending: VecDeque::new(),
            deadline,
            next_id: 0,
        };
        let hello = client.read_line("the daemon hello")?;
        anyhow::ensure!(
            hello["type"] == "daemon_hello",
            "unexpected first line: {hello}"
        );
        Ok(client)
    }

    fn read_line(&mut self, what: &str) -> anyhow::Result<Value> {
        let mut line = String::new();
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => anyhow::bail!("the daemon closed the connection while awaiting {what}"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => {
                    return serde_json::from_str(line.trim())
                        .with_context(|| format!("parse daemon line {line:?}"));
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

    /// Send one command and return its successful response's `data`.
    fn command(&mut self, command: &Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = format!("perf-{}", self.next_id);
        let mut line = serde_json::to_string(&json!({
            "type": "command",
            "id": id,
            "protocol": { "name": PROTOCOL, "version": PROTOCOL_VERSION },
            "command": command,
        }))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        loop {
            let line = self.read_line(&format!("the {} response", command["type"]))?;
            if line["id"].as_str() == Some(id.as_str()) {
                anyhow::ensure!(
                    line["success"] == true,
                    "{} failed: {line}",
                    command["type"]
                );
                return Ok(line["data"].clone());
            }
            self.pending.push_back(line);
        }
    }

    /// The next session event matching `wanted` (earlier events are
    /// dropped), with the instant this client read it.
    fn event(
        &mut self,
        what: &str,
        wanted: impl Fn(&Value) -> bool,
    ) -> anyhow::Result<(Value, Instant)> {
        loop {
            let line = match self.pending.pop_front() {
                Some(line) => line,
                None => self.read_line(what)?,
            };
            let at = Instant::now();
            if line["type"] == "session_event" && wanted(&line["event"]) {
                return Ok((line["event"].clone(), at));
            }
        }
    }
}

/// A `python_repl` tool call record for a faux script.
fn cell(code: &str) -> Value {
    json!({ "content": [
        { "type": "toolCall", "name": "python_repl", "arguments": { "code": code } },
    ] })
}

/// One `python_repl` call as the client saw it: the start and end events'
/// read instants (the wire's end event carries only the call id, so the
/// start's id pairs them).
#[derive(Debug)]
struct CellEvents {
    started: Instant,
    ended: Instant,
}

impl Client {
    /// The next `python_repl` call, start -> end. A failed cell fails the
    /// run: a measured error path is not the metric.
    fn python_cell(&mut self, what: &str) -> anyhow::Result<CellEvents> {
        let (start, started) = self.event(&format!("{what} start"), |event| {
            event["type"] == "tool_execution_start" && event["toolName"] == "python_repl"
        })?;
        let id = start["toolCallId"].clone();
        let (end, ended) = self.event(&format!("{what} result"), |event| {
            event["type"] == "tool_execution_end" && event["toolCallId"] == id
        })?;
        // A raised cell reports `details.status: "error"` with `isError`
        // false (the tool ran; the code failed): both fail the run.
        anyhow::ensure!(
            end["isError"] != true && end["result"]["details"]["status"] != "error",
            "{what} failed: {end}"
        );
        Ok(CellEvents { started, ended })
    }
}

/// An in-kernel wall-clock stamp (epoch ms) a cell wrote; `None` until the
/// file holds a whole number (the cell writes it in one call).
fn read_stamp(path: &Path) -> Option<f64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

/// Run one kernel scenario against the daemon listening on `socket`.
///
/// # Errors
///
/// Propagates wire, create/prompt, cell-failure, and deadline errors.
pub(super) fn measure(
    ctx: &RunCtx,
    socket: &Path,
    scenario: Scenario,
    run: usize,
) -> anyhow::Result<Measured> {
    let run_dir = socket.parent().context("a socket dir")?;
    let mut client = Client::connect(socket, Instant::now() + ctx.liveness)?;
    match scenario {
        Scenario::FirstCell => first_cell(ctx, &mut client, run_dir),
        Scenario::SubagentFirstCell => subagent_first_cell(ctx, &mut client, run_dir, run),
        Scenario::PandasImport => pandas_import(ctx, &mut client, run_dir),
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

/// Write a faux script into the run dir and return its path.
fn write_script(run_dir: &Path, name: &str, responses: &[Value]) -> anyhow::Result<String> {
    let path = run_dir.join(name);
    std::fs::write(
        &path,
        json!({ "engine": "faux", "responses": responses }).to_string(),
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(path.display().to_string())
}

/// Create + attach a scripted session in the corpus project, then send the
/// one prompt that starts its scripted turn.
fn start_session(
    ctx: &RunCtx,
    client: &mut Client,
    script: &str,
    child_script: Option<&str>,
) -> anyhow::Result<()> {
    let mut config = json!({
        "cwd": ctx.project_dir.display().to_string(),
        "script": script,
    });
    if let Some(child_script) = child_script {
        config["childScript"] = Value::from(child_script);
    }
    let created = client.command(&json!({ "type": "create", "config": config }))?;
    let session = created["id"]
        .as_str()
        .with_context(|| format!("no session id in the create response: {created}"))?
        .to_string();
    client.command(&json!({ "type": "attach", "activeSessionId": session }))?;
    client.command(&json!({
        "type": "prompt",
        "activeSessionId": session,
        "message": "run the perf cell",
    }))?;
    Ok(())
}

/// first-cell: the create command -> the first `python_repl` result event.
/// The span is everything a new session's first cell waits on: worker
/// spawn, session create (which starts the kernel prewarm), the scripted
/// tool call, and the kernel boot the cell awaits.
fn first_cell(ctx: &RunCtx, client: &mut Client, run_dir: &Path) -> anyhow::Result<Measured> {
    let script = write_script(
        run_dir,
        "first-cell.json",
        &[cell("print('first cell')"), json!({ "text": "done" })],
    )?;
    let started = Instant::now();
    start_session(ctx, client, &script, None)?;
    let cell = client.python_cell("the first cell")?;
    Ok(Measured {
        settled: vec![millis(cell.ended - started)],
        ..Measured::default()
    })
}

/// subagent-first-cell: the parent's `rlm.spawn` call -> the child's first
/// cell finishing. Both stamps are the kernels' own wall clocks (the
/// parent's before the spawn, the child's at the end of its first cell; one
/// host, one clock), so the span is the child create + its kernel boot +
/// its first cell, without the parent's own boot. The product delivers the
/// child's task once the spawning cell returns, so the parent cell ends
/// right after the spawn and the child's receipt file is the end mark.
fn subagent_first_cell(
    ctx: &RunCtx,
    client: &mut Client,
    run_dir: &Path,
    run: usize,
) -> anyhow::Result<Measured> {
    let spawned = run_dir.join(format!("spawned-{run}.txt"));
    let receipt = run_dir.join(format!("child-cell-{run}.txt"));
    // A stamp left by an earlier invocation over the same output dir
    // would end the wait before this run's child ever ran.
    for stale in [&spawned, &receipt] {
        match std::fs::remove_file(stale) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", stale.display())),
        }
    }
    let child_script = write_script(
        run_dir,
        "child.json",
        &[
            cell(&format!(
                "import time\nopen({receipt:?}, 'w').write(repr(time.time() * 1000))",
                receipt = receipt.display().to_string(),
            )),
            json!({ "text": "child done" }),
        ],
    )?;
    let parent_cell = format!(
        "import time\n\
         started = time.time() * 1000\n\
         await rlm.spawn('run the perf cell', name={CHILD_NAME:?})\n\
         open({spawned:?}, 'w').write(repr(started))",
        spawned = spawned.display().to_string(),
    );
    let script = write_script(
        run_dir,
        "parent.json",
        &[cell(&parent_cell), json!({ "text": "done" })],
    )?;
    start_session(ctx, client, &script, Some(&child_script))?;
    client.python_cell("the parent's spawn cell")?;
    let spawned_at = read_stamp(&spawned)
        .with_context(|| format!("the spawn cell wrote no {}", spawned.display()))?;
    let finished_at = loop {
        if let Some(stamp) = read_stamp(&receipt) {
            break stamp;
        }
        anyhow::ensure!(
            Instant::now() < client.deadline,
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

/// pandas-import: a session's first `import pandas` cell, start event ->
/// result event. A warm-up cell runs first so the span is the import, not
/// the kernel boot.
fn pandas_import(ctx: &RunCtx, client: &mut Client, run_dir: &Path) -> anyhow::Result<Measured> {
    let script = write_script(
        run_dir,
        "pandas.json",
        &[
            cell("print('kernel up')"),
            cell("import pandas\nprint(pandas.__version__)"),
            json!({ "text": "done" }),
        ],
    )?;
    start_session(ctx, client, &script, None)?;
    client.python_cell("the warm-up cell")?;
    let cell = client.python_cell("the import cell")?;
    Ok(Measured {
        settled: vec![millis(cell.ended - cell.started)],
        ..Measured::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client over one end of a socket pair, the other end scripted (a
    /// writer thread, so a small socket buffer never blocks the test).
    fn scripted(lines: Vec<Value>) -> Client {
        let (ours, mut theirs) = UnixStream::pair().expect("socket pair");
        std::thread::spawn(move || {
            for line in lines {
                if writeln!(theirs, "{line}").is_err() {
                    return;
                }
            }
        });
        ours.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("read timeout");
        Client {
            reader: BufReader::new(ours.try_clone().expect("clone")),
            writer: ours,
            pending: VecDeque::new(),
            deadline: Instant::now() + Duration::from_secs(5),
            next_id: 0,
        }
    }

    fn event(event: Value) -> Value {
        json!({ "type": "session_event", "event": event })
    }

    /// The wire's end event names no tool: the start's call id pairs it
    /// (another tool's end is skipped), and a raised cell — `isError` false,
    /// `details.status` error — fails the run.
    #[test]
    fn cells_pair_by_call_id_and_a_raised_cell_fails_the_run() {
        let start = |id: &str, tool: &str| {
            event(json!({ "type": "tool_execution_start", "toolCallId": id, "toolName": tool }))
        };
        let end = |id: &str, status: &str| {
            event(json!({
                "type": "tool_execution_end",
                "toolCallId": id,
                "isError": false,
                "result": { "details": { "status": status } },
            }))
        };
        let mut client = scripted(vec![
            start("bash-1", "bash"),
            start("py-1", "python_repl"),
            end("bash-1", "error"),
            end("py-1", "ok"),
            start("py-2", "python_repl"),
            end("py-2", "error"),
        ]);
        client.python_cell("the first cell").expect("the ok cell");
        let raised = client
            .python_cell("the second cell")
            .expect_err("a raised cell fails the run");
        assert!(
            raised.to_string().starts_with("the second cell failed"),
            "{raised}"
        );
    }
}
