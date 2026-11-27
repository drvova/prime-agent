//! The inbound recording tap (debug builds only): with `PA_TUI_RECORD_INBOUND` naming a file,
//! every frame the daemon client receives - from the supervisor socket and from a direct worker
//! link, after `message_update` deltas are restored - is appended to it as one JSON line
//! `{"via": "supervisor" | "direct" | "wire2", "frame": <the DaemonOutbound frame>}` (the wire2
//! link's frames are the old ones the compat layer made of the runtime's events).
//!
//! This is how the inbound goldens under `tests/goldens/inbound/` were recorded: the stream the
//! TUI receives today, which a wire2 client must reproduce behind `wire2_compat` (record the
//! compat path the same way and diff). Release builds carry none of it; a debug build without the
//! variable records nothing.

use std::io::Write as _;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde_json::Value;

/// The variable naming the recording file (appended to, created when missing).
const RECORD_ENV: &str = "PA_TUI_RECORD_INBOUND";

/// Which connection delivered a recorded frame.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Via {
    Supervisor,
    Direct,
    /// The wire2 link: the old frame the compat layer made of a runtime event.
    Wire2,
}

#[derive(Serialize)]
struct RecordedFrame<'a> {
    via: Via,
    frame: &'a Value,
}

/// The recording file, opened on first use when the variable is set.
static RECORDING: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

/// Append `frame` to the recording when one was asked for. The recording is a developer's
/// explicit request, so a file that cannot be opened or written fails loudly rather than leave
/// a silently partial golden.
pub(crate) fn record(via: Via, frame: &Value) {
    let recording = RECORDING.get_or_init(|| {
        std::env::var_os(RECORD_ENV).map(|path| {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap_or_else(|error| {
                    panic!("{RECORD_ENV}: cannot open {}: {error}", path.display())
                });
            Mutex::new(file)
        })
    });
    let Some(file) = recording else {
        return;
    };
    let mut line =
        serde_json::to_vec(&RecordedFrame { via, frame }).expect("a JSON value serializes");
    line.push(b'\n');
    file.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .write_all(&line)
        .unwrap_or_else(|error| panic!("{RECORD_ENV}: the recording write failed: {error}"));
}
