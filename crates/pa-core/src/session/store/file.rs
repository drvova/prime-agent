//! The file store: one JSONL file per transcript, at its locator.
//!
//! Creates replace the file atomically (private temp, fsync, rename), appends
//! open it `O_APPEND` and fsync, and the generation is the file's length,
//! device, inode and change times.

use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use pa_types::session::FileEntry;

use super::{
    ListedTranscript, TranscriptCompletion, TranscriptGeneration, TranscriptIdentity,
    TranscriptListing, TranscriptReader, TranscriptRemoval, TranscriptStat, TranscriptStore,
};

/// The [`TranscriptStore`] that keeps each transcript in the JSONL file at
/// its locator.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileTranscriptStore;

/// The session file write (TS `writeFileAtomicSync`): private temp, fsync,
/// rename. The fsync is deliberate (TS passes none; a non-synced rename can
/// zero the file on a hard crash).
const CREATE_OPTIONS: pa_types::platform::ReplaceOptions = pa_types::platform::ReplaceOptions {
    durability: pa_types::platform::Durability::SyncFile,
    mode: pa_types::platform::FileMode::Private,
    symlinks: pa_types::platform::Symlinks::ReplaceLink,
};

/// The bounded tail read that gates the repair scan.
const REPAIR_SUSPICION_WINDOW_BYTES: usize = 1024 * 1024;

impl TranscriptGeneration {
    #[cfg(unix)]
    fn of(meta: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_ns: meta.ctime_nsec(),
        }
    }

    #[cfg(not(unix))]
    fn of(meta: &Metadata) -> Self {
        Self {
            len: meta.len(),
            ..Self::default()
        }
    }
}

impl TranscriptIdentity {
    fn of(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self(metadata.dev(), metadata.ino())
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            Self(metadata.creation_time(), 0)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = metadata;
            Self(0, 0)
        }
    }
}

impl TranscriptStat {
    fn of(meta: &Metadata) -> Self {
        Self {
            generation: TranscriptGeneration::of(meta),
            identity: TranscriptIdentity::of(meta),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
            is_transcript: meta.is_file(),
        }
    }
}

/// An open transcript file.
struct OpenTranscript(File);

impl Read for OpenTranscript {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for OpenTranscript {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.0.seek(position)
    }
}

impl Write for OpenTranscript {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl TranscriptReader for OpenTranscript {
    fn stat(&self) -> io::Result<TranscriptStat> {
        Ok(TranscriptStat::of(&self.0.metadata()?))
    }
}

impl TranscriptCompletion for OpenTranscript {
    fn sync(&self) -> io::Result<()> {
        crate::platform::fsync(&self.0)
    }
}

impl TranscriptStore for FileTranscriptStore {
    fn open(&self, locator: &Path) -> io::Result<Box<dyn TranscriptReader>> {
        Ok(Box::new(OpenTranscript(File::open(locator)?)))
    }

    fn read(&self, locator: &Path) -> io::Result<Vec<u8>> {
        fs::read(locator)
    }

    fn stat(&self, locator: &Path) -> io::Result<TranscriptStat> {
        Ok(TranscriptStat::of(&fs::metadata(locator)?))
    }

    fn list(&self, dir: &Path) -> io::Result<TranscriptListing> {
        let mut listing = TranscriptListing::default();
        for entry in fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            let file_type = entry.file_type();
            if file_type.as_ref().is_ok_and(fs::FileType::is_dir) {
                listing.directories.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                listing.transcripts.push(ListedTranscript {
                    locator: path,
                    listed_as_transcript: file_type.is_ok_and(|kind| kind.is_file()),
                });
            }
        }
        Ok(listing)
    }

    fn create(
        &self,
        locator: &Path,
        write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        if let Some(parent) = locator.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = pa_types::platform::AtomicFile::create(locator, CREATE_OPTIONS)?;
        write(&mut file)?;
        file.commit()
    }

    fn append(&self, locator: &Path, bytes: &[u8]) -> io::Result<TranscriptGeneration> {
        let mut file = fs::OpenOptions::new().append(true).open(locator)?;
        // The torn-tail boundary (the append-only reopen's belt): an
        // unterminated final row - a crashed append the repair failed to
        // re-terminate, its errors swallowed - must not merge with the first
        // appended row: both would be lost to the index. One newline before
        // the payload keeps the torn row alone (the loaders skip it) and the
        // appended rows parseable. A no-op on a healthy file.
        if file.metadata()?.len() > 0 && !file_tail_is_newline(locator)? {
            file.write_all(b"\n")?;
        }
        file.write_all(bytes)?;
        file.flush()?;
        crate::platform::fsync(&file)?;
        Ok(TranscriptGeneration::of(&file.metadata()?))
    }

    fn open_for_completion(&self, locator: &Path) -> io::Result<Box<dyn TranscriptCompletion>> {
        let file = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(locator)?;
        Ok(Box::new(OpenTranscript(file)))
    }

    fn sync(&self, locator: &Path) -> io::Result<()> {
        File::open(locator)?.sync_all()
    }

    fn repair_tail(&self, locator: &Path) {
        let Ok(target_path) = pa_types::platform::canonicalize(locator) else {
            return;
        };
        let file_path = target_path.as_path();
        if !tail_looks_damaged(file_path) {
            return;
        }
        if !first_line_is_session_header(file_path) {
            return;
        }
        let temp = PathBuf::from(format!("{}.tmp{}", file_path.display(), std::process::id()));
        // Best effort by contract: a failed repair leaves the file as it was.
        let _ = (|| -> io::Result<()> {
            let source = File::open(file_path)?;
            let mut reader = io::BufReader::new(source);
            let mut options = fs::OpenOptions::new();
            options.create(true).write(true).truncate(true);
            crate::platform::perms::set_private_mode(&mut options);
            let mut output = options.open(&temp)?;
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
            if !dirty {
                return Ok(());
            }
            output.sync_all()?;
            drop(output);
            drop(reader);
            // TS repairs crash damage through `writeFileAtomicSync`: the repaired
            // file lands by rename, never as a torn in-place write.
            crate::platform::rename_onto(&temp, file_path)
        })();
        let _ = fs::remove_file(&temp);
    }

    fn import(&self, source: &Path, locator: &Path) -> io::Result<()> {
        fs::copy(source, locator).map(drop)
    }

    fn restore(&self, archived: &Path, locator: &Path) -> io::Result<()> {
        let sessions_dir = locator.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(sessions_dir)?;
        // Stage in the live directory so custom sessions dirs on another
        // filesystem work too. Publish a complete file without replacing any
        // live path, even one the catalog could not parse.
        let temp = sessions_dir.join(format!(".restore-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        crate::platform::perms::set_private_mode(&mut options);
        let mut target = options.open(&temp)?;
        let result = (|| -> io::Result<()> {
            let mut source = File::open(archived)?;
            io::copy(&mut source, &mut target)?;
            target.set_times(fs::FileTimes::new().set_modified(source.metadata()?.modified()?))?;
            target.sync_all()
        })();
        drop(target);
        let result = result.and_then(|()| {
            fs::hard_link(&temp, locator).map_err(|error| {
                io::Error::new(error.kind(), format!("publish restored session: {error}"))
            })
        });
        let cleanup = fs::remove_file(&temp);
        result?;
        cleanup.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("remove restore staging file {}: {error}", temp.display()),
            )
        })?;
        fs::remove_file(archived).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("unlink restored archive {}: {error}", archived.display()),
            )
        })
    }

    fn remove(&self, locator: &Path) -> io::Result<TranscriptRemoval> {
        let trash = std::process::Command::new("trash")
            .arg("--")
            .arg(locator)
            .output();
        let removed_by_trash = match trash {
            Ok(output) => output.status.success() || !locator.exists(),
            Err(_) => false,
        };
        if removed_by_trash {
            return Ok(TranscriptRemoval::Trash);
        }
        fs::remove_file(locator).map(|()| TranscriptRemoval::Unlink)
    }
}

/// Whether the file's last byte is a newline: the append boundary probe.
fn file_tail_is_newline(path: &Path) -> io::Result<bool> {
    let mut probe = File::open(path)?;
    let mut tail = [0u8; 1];
    probe.seek(SeekFrom::End(-1))?;
    probe.read_exact(&mut tail)?;
    Ok(tail[0] == b'\n')
}

fn parses_as_json(line: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(line).is_ok()
}

/// A bounded tail read gates the repair scan: clean opens never copy the file.
fn tail_looks_damaged(target_path: &Path) -> bool {
    let Ok(mut file) = File::open(target_path) else {
        return false;
    };
    let Ok(size) = file.metadata().map(|meta| meta.len()) else {
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
        if let Some(position) = window[..search_end].iter().rposition(|byte| *byte == b'\n') {
            let last_line_start = start + position as u64 + 1;
            return !last_line_is_json(&mut file, last_line_start, size - 1);
        }
        end = start;
    }
    !last_line_is_json(&mut file, 0, size - 1)
}

fn last_line_is_json(file: &mut File, start: u64, end: u64) -> bool {
    if start == end {
        return true;
    }
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let reader = io::BufReader::new(file.take(end - start));
    serde_json::from_reader::<_, serde::de::IgnoredAny>(reader).is_ok()
}

fn first_line_is_session_header(file_path: &Path) -> bool {
    let Ok(file) = File::open(file_path) else {
        return false;
    };
    let mut first_line = String::new();
    if io::BufReader::new(file).read_line(&mut first_line).is_err() {
        return false;
    }
    crate::session::parse_session_entries(&first_line)
        .first()
        .is_some_and(|entry| matches!(entry, FileEntry::Header { .. }))
}
