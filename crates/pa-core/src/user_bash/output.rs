//! A run's output: the sanitizing accumulator, the spill past the window,
//! and the tail truncation.

use std::io::Write;

use serde_json::json;

use super::{ChunkSink, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, SPILL_PREFIX, STREAM_MAX_BYTES};

/// The streaming accumulator (TS `onData`): the sanitizing decode, the
/// spill, the in-memory tail window, and the chunk callback.
pub(super) struct OutputStream {
    pub(super) chunks: Vec<String>,
    pub(super) retained_bytes: usize,
    pub(super) total_bytes: usize,
    pub(super) spill: OutputSpill,
    pub(super) on_chunk: Option<ChunkSink>,
}

impl Clone for OutputStream {
    fn clone(&self) -> Self {
        Self {
            chunks: self.chunks.clone(),
            retained_bytes: self.retained_bytes,
            total_bytes: self.total_bytes,
            spill: OutputSpill::new(),
            on_chunk: self.on_chunk.clone(),
        }
    }
}

impl OutputStream {
    /// The collected view (the shared clone path): this port never clones
    /// mid-run, but the fallback keeps the collected chunks.
    pub(super) fn clone_stream(&self) -> Self {
        self.clone()
    }

    pub(super) fn push(&mut self, data: &str) {
        let text = sanitize_output(data);
        if text.is_empty() {
            return;
        }
        self.total_bytes += text.len();
        if self.total_bytes > DEFAULT_MAX_BYTES {
            self.spill.open(&self.chunks);
        }
        self.spill.write(&text);
        self.chunks.push(text.clone());
        self.retained_bytes += self.chunks.last().map_or_default(String::len);
        // The in-memory window keeps the tail (TS `maxOutputBytes`).
        while self.retained_bytes > STREAM_MAX_BYTES && self.chunks.len() > 1 {
            let removed = self.chunks.remove(0);
            self.retained_bytes -= removed.len();
        }
        if let Some(on_chunk) = &self.on_chunk {
            on_chunk(json!({ "type": "bash_output", "chunk": text }));
        }
    }
}

/// Sanitize one output chunk (TS `strip-ansi` + `sanitizeBinaryOutput` +
/// the `\r` strip in the executor's `onData`).
pub(super) fn sanitize_output(text: &str) -> String {
    let stripped = strip_ansi(text);
    let mut out = String::with_capacity(stripped.len());
    for character in stripped.chars() {
        let code = character as u32;
        // Tab, newline, carriage return stay.
        if matches!(code, 0x09 | 0x0a | 0x0d) {
            out.push(character);
            continue;
        }
        // Control characters and the format characters that break width
        // rendering drop.
        if code <= 0x1f || (0xfff9..=0xfffb).contains(&code) {
            continue;
        }
        out.push(character);
    }
    out.replace('\r', "")
}

/// Strip ANSI escape sequences (CSI sequences and the simple two-byte
/// escapes the `strip-ansi` package handles).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates run to the final byte (0x40-0x7e).
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // A simple escape sequence: one following byte.
            Some(_) => {}
            // A trailing escape with nothing after it drops.
            None => break,
        }
    }
    out
}

/// The output spill (TS `OutputSpill`): opened once the output outgrows
/// the window; a degraded spill never advertises a path.
pub(super) struct OutputSpill {
    file: Option<std::fs::File>,
    path: Option<String>,
    failed: bool,
}

impl OutputSpill {
    pub(super) fn new() -> Self {
        Self {
            file: None,
            path: None,
            failed: false,
        }
    }

    pub(super) fn open(&mut self, replay: &[String]) {
        if self.file.is_some() || self.failed {
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "{SPILL_PREFIX}-{}.log",
            uuid::Uuid::new_v4().simple()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        crate::platform::perms::set_private_mode(&mut options);
        let Ok(mut file) = options.open(&path) else {
            self.failed = true;
            return;
        };
        for chunk in replay {
            if file.write_all(chunk.as_bytes()).is_err() {
                self.failed = true;
                return;
            }
        }
        self.path = Some(path.to_string_lossy().to_string());
        self.file = Some(file);
    }

    pub(super) fn write(&mut self, text: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.write_all(text.as_bytes()).is_err() {
            self.failed = true;
            self.file = None;
            self.path = None;
        }
    }

    pub(super) fn finalize(mut self) -> Option<String> {
        if let Some(file) = self.file.as_mut() {
            if file.flush().is_err() {
                self.failed = true;
            }
        }
        self.file = None;
        if self.failed {
            None
        } else {
            self.path
        }
    }
}

/// Tail truncation (TS `truncateTail`): the last 2000 lines within 50KB
/// win; an oversized last line keeps its tail, blank trailing lines permitting.
pub(super) fn truncate_tail(content: &str) -> (String, bool) {
    let total_bytes = content.len();
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();
    if total_lines <= DEFAULT_MAX_LINES && total_bytes <= DEFAULT_MAX_BYTES {
        return (content.to_string(), false);
    }
    let mut collected: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    for line in lines.iter().rev() {
        if collected.len() >= DEFAULT_MAX_LINES {
            break;
        }
        let line_bytes = line.len() + usize::from(!collected.is_empty());
        if output_bytes + line_bytes > DEFAULT_MAX_BYTES {
            // The oversized-line rescue: trailing blanks must not defeat
            // the rescue, so keep as many as the budget allows and unshift
            // the line's own tail.
            if collected.iter().all(|collected| collected.is_empty()) {
                let kept_blanks = collected.len().min(DEFAULT_MAX_BYTES.saturating_sub(1));
                let budget = DEFAULT_MAX_BYTES.saturating_sub(kept_blanks);
                let truncated_line = truncate_string_to_bytes_from_end(line, budget);
                let mut out = String::with_capacity(DEFAULT_MAX_BYTES);
                out.push_str(&truncated_line);
                for _ in 0..kept_blanks {
                    out.push('\n');
                }
                return (out, true);
            }
            break;
        }
        collected.push(line);
        output_bytes += line_bytes;
    }
    collected.reverse();
    (collected.join("\n"), true)
}

/// The tail `maxBytes` of one string, on char boundaries (TS `truncateStringToBytesFromEnd`).
fn truncate_string_to_bytes_from_end(text: &str, max_bytes: usize) -> String {
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[text.len() - end..].to_string()
}
