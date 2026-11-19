//! A distribution build's one-time history import.
//!
//! A distribution build keeps its own agent dir (`~/.prime/agent-<variant>`),
//! so it never shares a live file with the official products in
//! `~/.prime/agent`. Its first run copies the configuration
//! ([`super::import_official_config`]); this import copies the history once:
//! the chats (`sessions/`, `sessions-archive/`), their children and RLM
//! registries (`session-artifacts/`), and the harness memory (`harness/`, the
//! same on-disk format the official build reads), and fills in any
//! configuration entry the agent dir still lacks.
//!
//! It copies, never moves or links: the official products keep using their
//! files, and a hard link would let their appends bleed into the copy.
//! A plain file copy goes through [`std::fs::copy`], which clones the
//! file where the filesystem can (copy-on-write, on APFS, Btrfs, and XFS)
//! and copies the bytes otherwise. A JSON file
//! that names a history file of the official dir by absolute path (a fork's
//! parent session, an RLM registry's child session) is rewritten to name the
//! copy instead, so the import never hands back a path into the official dir.
//!
//! It merges: a file already in the agent dir is never overwritten (it is kept
//! and named), so it is safe over a dir that already holds its own sessions.
//! It is crash-safe and resumable: each file is copied to a hidden temp file
//! beside its target and published without replacing anything, and the marker
//! ([`HISTORY_IMPORT_MARKER`]) is written last, so an interrupted run resumes
//! where it stopped. It is local, and it takes no size budget for history: a
//! caller runs it off any path the user waits on (the daemon runs it in the
//! background after boot).
//!
//! Never copied: leases, worker descriptors, persistent shells, sockets,
//! logs, the kernel venv, update and restart state, lock dirs, and the
//! telemetry id.

use std::io::{BufRead, Read, Seek, Write};
use std::path::{Path, PathBuf};

use pa_types::platform::StateLayout;

use super::distribution_import::{
    copy_entry, measure, CONFIG_ENTRIES, IMPORT_BYTE_BUDGET, IMPORT_ENTRY_BUDGET,
};

/// Written into the agent dir once an import finished; while it exists, the
/// automatic import never runs again (`import-sessions` runs it on demand).
pub const HISTORY_IMPORT_MARKER: &str = ".imported-sessions-v1";

/// The history trees, relative to the agent dir.
const HISTORY_TREES: [&str; 4] = [
    "sessions",
    "sessions-archive",
    "session-artifacts",
    "harness",
];

/// The trees whose `.jsonl` files are chats (the count the notice reports).
const CHAT_TREES: [&str; 2] = ["sessions", "sessions-archive"];

/// Whether a run honors the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryImportRun {
    /// The automatic run: nothing happens once the marker exists.
    Once,
    /// `import-sessions`: runs again over an imported dir (still merging,
    /// never overwriting).
    Again,
}

/// What [`plan_official_history_import`] found.
#[derive(Debug)]
pub enum HistoryImportCheck {
    /// The official build, or an agent dir overridden away from this
    /// build's own default.
    NotApplicable,
    /// The marker exists (a [`HistoryImportRun::Once`] check).
    AlreadyImported,
    /// The official dir holds no history and no configuration the agent dir
    /// lacks; nothing was created.
    NothingToImport,
    /// Another process (the daemon, or `import-sessions`) is importing.
    Busy,
    /// An import to run; it holds the import lock until it is dropped.
    Pending(HistoryImportPlan),
}

/// One import, planned: what it will copy. Holds the import lock.
#[derive(Debug)]
pub struct HistoryImportPlan {
    source: PathBuf,
    target: PathBuf,
    files: Vec<PathBuf>,
    config: Vec<&'static str>,
    chats: usize,
    bytes: u64,
    _lock: std::fs::File,
}

/// What a finished import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryImportReport {
    /// Chat files copied (the `.jsonl` files of `sessions/` and `sessions-archive/`).
    pub chats_imported: usize,
    /// Every history file copied, chats included.
    pub files_imported: usize,
    /// The bytes those files hold.
    pub bytes_imported: u64,
    /// History files the agent dir already held under the same name, left as
    /// they are (relative paths).
    pub kept: Vec<String>,
    /// Configuration entries the agent dir lacked and now holds.
    pub config_added: Vec<String>,
    /// Configuration entries left behind: over the size budget, or created
    /// in the agent dir while the import ran.
    pub config_skipped: Vec<String>,
}

impl HistoryImportPlan {
    /// The chats the import will copy (for the notice before it starts).
    #[must_use]
    pub fn chats(&self) -> usize {
        self.chats
    }

    /// The configuration entries the import will add.
    #[must_use]
    pub fn config_entries(&self) -> &[&'static str] {
        &self.config
    }

    /// The bytes of the history files to copy.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Copy everything planned, then write the marker. `before_file` runs
    /// before each history file with its relative path (a caller's progress
    /// or pacing hook).
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the import. Every file published before it
    /// stays, the marker is not written, and the next run resumes.
    pub fn run(self, before_file: &mut dyn FnMut(&Path)) -> std::io::Result<HistoryImportReport> {
        let mut report = HistoryImportReport::default();
        let (mut config_bytes, mut config_files) = (0_u64, 0_u64);
        for name in &self.config {
            let (entry_bytes, entry_files) = measure(&self.source.join(name))?;
            if config_bytes + entry_bytes > IMPORT_BYTE_BUDGET
                || config_files + entry_files > IMPORT_ENTRY_BUDGET
            {
                report.config_skipped.push((*name).to_string());
                continue;
            }
            let stage = self.target.join(format!(".{name}.history-import"));
            remove_leftover(&stage)?;
            copy_entry(&self.source.join(name), &stage)?;
            if publish_new(&stage, &self.target.join(name))? {
                config_bytes += entry_bytes;
                config_files += entry_files;
                report.config_added.push((*name).to_string());
            } else {
                report.config_skipped.push((*name).to_string());
            }
        }
        let rewrites = PathRewrites::new(&self.source, &self.target);
        for relative in &self.files {
            before_file(relative);
            let target = self.target.join(relative);
            if target.symlink_metadata().is_ok() {
                report.kept.push(relative.to_string_lossy().into_owned());
                continue;
            }
            let Some(parent) = target.parent() else {
                continue;
            };
            create_private_dirs(&self.target, parent)?;
            let Some(file_name) = target.file_name() else {
                continue;
            };
            let stage = parent.join(format!(".{}.history-import", file_name.to_string_lossy()));
            let bytes = match copy_history_file(&self.source.join(relative), &stage, &rewrites) {
                Ok(Some(bytes)) => bytes,
                // Gone, or no longer a regular file, since the plan: the
                // official product deleted or replaced it.
                Ok(None) => continue,
                Err(error) => {
                    let _ = std::fs::remove_file(&stage);
                    return Err(error);
                }
            };
            if publish_new(&stage, &target)? {
                report.files_imported += 1;
                report.bytes_imported += bytes;
                if is_chat(relative) {
                    report.chats_imported += 1;
                }
            } else {
                report.kept.push(relative.to_string_lossy().into_owned());
            }
        }
        write_marker(&self.target, &report)?;
        Ok(report)
    }
}

/// Plan the history import into `agent_dir`, the resolved agent dir (env
/// override included): it applies only to a distribution build's own default
/// dir. A pending plan holds the import lock.
///
/// # Errors
///
/// The I/O error of a failed walk, dir creation, or lock.
pub fn plan_official_history_import(
    layout: StateLayout,
    home: &Path,
    agent_dir: &Path,
    run: HistoryImportRun,
) -> std::io::Result<HistoryImportCheck> {
    match layout {
        StateLayout::Official => return Ok(HistoryImportCheck::NotApplicable),
        StateLayout::Distribution { .. } => {}
    }
    if agent_dir != layout.home_agent_dir(home) {
        return Ok(HistoryImportCheck::NotApplicable);
    }
    let marker = agent_dir.join(HISTORY_IMPORT_MARKER);
    if run == HistoryImportRun::Once && marker.symlink_metadata().is_ok() {
        return Ok(HistoryImportCheck::AlreadyImported);
    }
    let source = StateLayout::Official.home_agent_dir(home);
    let mut files = Vec::new();
    for tree in HISTORY_TREES {
        collect_files(&source, Path::new(tree), &mut files)?;
    }
    let config: Vec<&'static str> = CONFIG_ENTRIES
        .into_iter()
        .filter(|name| {
            source.join(name).symlink_metadata().is_ok()
                && agent_dir.join(name).symlink_metadata().is_err()
        })
        .collect();
    if files.is_empty() && config.is_empty() {
        return Ok(HistoryImportCheck::NothingToImport);
    }
    std::fs::create_dir_all(agent_dir)?;
    let _ = crate::platform::perms::restrict_dir(agent_dir);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    crate::platform::perms::set_private_mode(&mut options);
    let lock = options.open(agent_dir.join(format!("{HISTORY_IMPORT_MARKER}.lock")))?;
    match crate::platform::try_lock_exclusive(&lock) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Ok(HistoryImportCheck::Busy);
        }
        Err(error) => return Err(error),
    }
    // A concurrent run may have finished between the first check and the lock.
    if run == HistoryImportRun::Once && marker.symlink_metadata().is_ok() {
        return Ok(HistoryImportCheck::AlreadyImported);
    }
    let chats = files.iter().filter(|relative| is_chat(relative)).count();
    let bytes = files
        .iter()
        .filter_map(|relative| source.join(relative).symlink_metadata().ok())
        .map(|metadata| metadata.len())
        .sum();
    Ok(HistoryImportCheck::Pending(HistoryImportPlan {
        source,
        target: agent_dir.to_path_buf(),
        files,
        config,
        chats,
        bytes,
        _lock: lock,
    }))
}

/// Write the marker without importing (`import-sessions --skip`): the
/// automatic import never runs for this agent dir.
///
/// # Errors
///
/// The I/O error of the dir creation or the marker write.
pub fn skip_official_history_import(agent_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
    write_marker(agent_dir, &HistoryImportReport::default())
}

/// The regular files under `source/relative`, depth first, as relative
/// paths. Lock dirs and files (`*.lock`), hidden temp files (a leading dot),
/// symlinks, sockets, and other special files are left out.
fn collect_files(source: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let dir = source.join(relative);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if name_text.starts_with('.') || name_text.ends_with(".lock") {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_files(source, &relative.join(&name), files)?;
        } else if kind.is_file() {
            files.push(relative.join(&name));
        }
    }
    Ok(())
}

/// Whether a relative history path is a chat file.
fn is_chat(relative: &Path) -> bool {
    relative
        .extension()
        .is_some_and(|extension| extension == "jsonl")
        && relative
            .components()
            .next()
            .is_some_and(|first| CHAT_TREES.iter().any(|tree| first.as_os_str() == *tree))
}

/// Create `dir` and its missing parents under `root`, each private.
fn create_private_dirs(root: &Path, dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    // Best effort, like every agent-dir creation.
    let mut current = dir;
    while current != root {
        let _ = crate::platform::perms::restrict_dir(current);
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    Ok(())
}

/// Remove a crashed run's leftover stage (file or dir).
fn remove_leftover(stage: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(stage) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(stage),
        Ok(_) => std::fs::remove_file(stage),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Publish a complete stage at `target` without replacing anything: a file
/// by a hard link that fails on an existing target (then the stage goes),
/// a dir by a rename. False when the target already exists; the stage is
/// gone either way.
fn publish_new(stage: &Path, target: &Path) -> std::io::Result<bool> {
    let is_dir = std::fs::symlink_metadata(stage)?.is_dir();
    if !is_dir {
        match std::fs::hard_link(stage, target) {
            Ok(()) => {
                std::fs::remove_file(stage)?;
                return Ok(true);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(stage)?;
                return Ok(false);
            }
            // A filesystem without hard links: the checked rename below.
            Err(_) => {}
        }
    }
    if target.symlink_metadata().is_ok() {
        remove_leftover(stage)?;
        return Ok(false);
    }
    std::fs::rename(stage, target)?;
    Ok(true)
}

/// The absolute history-path prefixes of the official dir, JSON-escaped and
/// led by the opening quote of a JSON string, each with its replacement in
/// the agent dir.
struct PathRewrites {
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl PathRewrites {
    fn new(source: &Path, target: &Path) -> Self {
        let prefix = |root: &Path, tree: &str| {
            let text = format!("{}{}", root.join(tree).display(), std::path::MAIN_SEPARATOR);
            let mut quoted = serde_json::to_string(&text)
                .unwrap_or_default()
                .into_bytes();
            // The opening quote stays (a JSON string starts with the path);
            // the closing one goes (the path continues).
            quoted.pop();
            quoted
        };
        Self {
            pairs: HISTORY_TREES
                .iter()
                .map(|tree| (prefix(source, tree), prefix(target, tree)))
                .collect(),
        }
    }

    /// The line with every prefix replaced; `None` when none occurs.
    fn apply(&self, line: &[u8]) -> Option<Vec<u8>> {
        let mut current: Option<Vec<u8>> = None;
        for (from, to) in &self.pairs {
            let haystack = current.as_deref().unwrap_or(line);
            if find(haystack, from).is_none() {
                continue;
            }
            let mut out = Vec::with_capacity(haystack.len());
            let mut rest = haystack;
            while let Some(at) = find(rest, from) {
                out.extend_from_slice(&rest[..at]);
                out.extend_from_slice(to);
                rest = &rest[at + from.len()..];
            }
            out.extend_from_slice(rest);
            current = Some(out);
        }
        current
    }

    /// Whether any prefix occurs in `reader`, read in bounded chunks.
    fn occurs_in(&self, reader: &mut impl Read) -> std::io::Result<bool> {
        let overlap = self
            .pairs
            .iter()
            .map(|(from, _)| from.len())
            .max()
            .unwrap_or(1)
            - 1;
        let mut window: Vec<u8> = Vec::new();
        let mut chunk = vec![0_u8; 1 << 20];
        loop {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                return Ok(false);
            }
            window.extend_from_slice(&chunk[..read]);
            if self
                .pairs
                .iter()
                .any(|(from, _)| find(&window, from).is_some())
            {
                return Ok(true);
            }
            let keep = window.len().min(overlap);
            window.drain(..window.len() - keep);
        }
    }
}

/// The first index of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, _) = needle.split_first()?;
    let mut offset = 0;
    while let Some(at) = haystack[offset..].iter().position(|&byte| byte == first) {
        let start = offset + at;
        if haystack[start..].starts_with(needle) {
            return Some(start);
        }
        offset = start + 1;
    }
    None
}

/// Copy one history file to `stage`: a JSON file naming official history
/// paths is rewritten line by line, any other file is copied whole (a clone
/// where the filesystem can). A chat file being appended to right now ends at
/// its last complete line. The copy is private and keeps the source's
/// modification time (the resume list orders by it). `None` when the source
/// is gone or no longer a regular file.
fn copy_history_file(
    source: &Path,
    stage: &Path,
    rewrites: &PathRewrites,
) -> std::io::Result<Option<u64>> {
    let mut file = match std::fs::File::open(source) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let extension = source.extension().and_then(|extension| extension.to_str());
    let lines = extension == Some("jsonl");
    let json = lines || extension == Some("json");
    let rewrite = json && rewrites.occurs_in(&mut file)?;
    if rewrite {
        file.rewind()?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        crate::platform::perms::set_private_mode(&mut options);
        let mut out = std::io::BufWriter::new(options.open(stage)?);
        let mut reader = std::io::BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if lines && line.last() != Some(&b'\n') {
                // A torn last line: the writer is mid-append.
                break;
            }
            match rewrites.apply(&line) {
                Some(rewritten) => out.write_all(&rewritten)?,
                None => out.write_all(&line)?,
            }
        }
        out.flush()?;
    } else {
        drop(file);
        std::fs::copy(source, stage)?;
        if lines {
            trim_torn_tail(stage)?;
        }
    }
    let _ = crate::platform::perms::restrict_file(stage);
    let copy = std::fs::OpenOptions::new().write(true).open(stage)?;
    copy.set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
    copy.sync_all()?;
    Ok(Some(copy.metadata()?.len()))
}

/// Cut a copied JSONL file back to its last complete line.
fn trim_torn_tail(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut end = file.metadata()?.len();
    let mut chunk = vec![0_u8; 64 * 1024];
    let mut first = true;
    while end > 0 {
        let start = end.saturating_sub(chunk.len() as u64);
        let length = (end - start) as usize;
        file.seek(std::io::SeekFrom::Start(start))?;
        file.read_exact(&mut chunk[..length])?;
        if first && chunk[length - 1] == b'\n' {
            return Ok(());
        }
        first = false;
        if let Some(at) = chunk[..length].iter().rposition(|&byte| byte == b'\n') {
            return file.set_len(start + at as u64 + 1);
        }
        end = start;
    }
    file.set_len(0)
}

/// Write the marker (the import's summary, for the curious) via a temp file.
fn write_marker(agent_dir: &Path, report: &HistoryImportReport) -> std::io::Result<()> {
    let body = serde_json::json!({
        "version": 1,
        "chatsImported": report.chats_imported,
        "filesImported": report.files_imported,
        "kept": report.kept.len(),
        "configAdded": report.config_added,
    });
    let temp = agent_dir.join(format!(".{HISTORY_IMPORT_MARKER}.tmp"));
    std::fs::write(&temp, format!("{body}\n"))?;
    crate::platform::rename_onto(&temp, &agent_dir.join(HISTORY_IMPORT_MARKER))
}

#[cfg(test)]
#[path = "history_import_tests.rs"]
mod tests;
