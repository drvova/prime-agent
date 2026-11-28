//! Crash-damage repair of a transcript's tail, the same for every store: a
//! bounded tail read decides whether to look closer, and the repair rewrites
//! the lines (the store then replaces the transcript with them).

use std::io::{self, BufRead, Read, SeekFrom, Write};

use pa_types::session::FileEntry;

use super::TranscriptReader;

/// The bounded tail read that gates the repair scan.
const REPAIR_SUSPICION_WINDOW_BYTES: usize = 1024 * 1024;

/// Whether the transcript's end looks damaged (zero bytes, an unterminated
/// or unparseable last line), read from its last megabytes only: clean opens
/// never copy the transcript.
pub(super) fn tail_looks_damaged(file: &mut dyn TranscriptReader) -> bool {
    let Ok(size) = file.stat().map(|stat| stat.len()) else {
        return true;
    };
    if size == 0 {
        return false;
    }
    let mut window = vec![0u8; size.min(REPAIR_SUSPICION_WINDOW_BYTES as u64) as usize];
    let mut end = size;
    while end > 0 {
        let start = end.saturating_sub(REPAIR_SUSPICION_WINDOW_BYTES as u64);
        let chunk = (end - start) as usize;
        if file.seek(SeekFrom::Start(start)).is_err()
            || file.read_exact(&mut window[..chunk]).is_err()
        {
            return true;
        }
        if window[..chunk].contains(&0) {
            return true;
        }
        if end == size && window[chunk - 1] != b'\n' {
            return true;
        }
        let search_end = if end == size { chunk - 1 } else { chunk };
        if let Some(position) = memchr::memrchr(b'\n', &window[..search_end]) {
            let last_line_start = start + position as u64 + 1;
            return !last_line_is_json(file, last_line_start, size - 1);
        }
        end = start;
    }
    !last_line_is_json(file, 0, size - 1)
}

fn last_line_is_json(file: &mut dyn TranscriptReader, start: u64, end: u64) -> bool {
    if start == end {
        return true;
    }
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let reader = io::BufReader::new(file.take(end - start));
    serde_json::from_reader::<_, serde::de::IgnoredAny>(reader).is_ok()
}

/// Whether the transcript starts with a `session` header line.
pub(super) fn first_line_is_session_header(file: &mut dyn TranscriptReader) -> bool {
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    let mut first_line = String::new();
    if io::BufReader::new(file).read_line(&mut first_line).is_err() {
        return false;
    }
    crate::session::parse_session_entries(&first_line)
        .first()
        .is_some_and(|entry| matches!(entry, FileEntry::Header { .. }))
}

/// Write the repaired lines of the transcript `source` reads (from its
/// start) to `output`: a zero-filled prefix is dropped, an unterminated line
/// is re-terminated when it parses and dropped when it does not, and an
/// unparseable last line is dropped. Whether anything changed; when nothing
/// did, `output` holds the same bytes and need not replace anything.
pub(super) fn repair_lines(source: &mut dyn Read, output: &mut dyn Write) -> io::Result<bool> {
    let mut reader = io::BufReader::new(source);
    let mut line = Vec::new();
    let mut dirty = false;
    while reader.read_until(b'\n', &mut line)? != 0 {
        let terminated = line.last() == Some(&b'\n');
        let end = line.len() - usize::from(terminated);
        let last = reader.fill_buf()?.is_empty();
        let prefix = line[..end].iter().take_while(|byte| **byte == 0).count();
        let content = &line[prefix..end];
        let keep = if prefix > 0 {
            // Zero-filled prefix: recover what parses, drop the rest.
            dirty = true;
            !content.is_empty() && parses_as_json(content)
        } else if !terminated {
            // Unterminated tail merges with the next append: re-terminate.
            dirty = true;
            !content.is_empty() && parses_as_json(content)
        } else if last && !content.is_empty() && !parses_as_json(content) {
            dirty = true;
            false
        } else {
            true
        };
        if keep {
            output.write_all(String::from_utf8_lossy(content).as_bytes())?;
            output.write_all(b"\n")?;
        }
        line.clear();
    }
    Ok(dirty)
}

fn parses_as_json(line: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(line).is_ok()
}
