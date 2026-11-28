//! The read arm: the file-IO load surface — the streamed and windowed opens, the bounded
//! header-only readers, the file-layout helpers, and the in-memory create.

use pa_core::session::store::store as transcripts;

use super::view::is_warning_shown_row;
use super::{
    anyhow, fold_child_usage_attributions, message_text, BilledRowSinkSlot, BufRead, Context,
    HashMap, Map, Path, PathBuf, Result, SessionEntry, SessionFile, SessionHeader, SessionWindow,
    Value,
};

const CURRENT_SESSION_VERSION: u32 = 3;

fn new_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[must_use]
pub fn session_file_name(session_id: &str) -> String {
    format!("{session_id}.jsonl")
}

#[must_use]
pub fn parse_session_entries(content: &str) -> Vec<Value> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str(trimmed).ok()
        })
        .collect()
}

/// Read the transcript's first line when it ends within `max_bytes` bytes
/// ([`pa_core::session::store::read_first_line_bounded`] over a fresh open).
pub(crate) fn read_first_line_bounded(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    let mut transcript = transcripts().open(path).ok()?;
    pa_core::session::store::read_first_line_bounded(&mut *transcript, max_bytes)
}

/// The session file's header, read from the first line bounded to
/// [`SESSION_LIST_HEADER_READ_MAX_BYTES`](pa_core::session::catalog::SESSION_LIST_HEADER_READ_MAX_BYTES): `None` also covers an over-long
/// first line — the bounded read refuses to judge a truncated one.
#[must_use]
pub fn read_session_header_bounded(path: &Path) -> Option<SessionHeader> {
    let line = read_first_line_bounded(
        path,
        pa_core::session::catalog::SESSION_LIST_HEADER_READ_MAX_BYTES,
    )?;
    let text = std::str::from_utf8(&line).ok()?;
    pa_core::session::catalog::parse_session_header_line(text)
}

/// Read the first line of a session file and parse it as a header.
#[must_use]
pub fn read_session_header(path: &Path) -> Option<SessionHeader> {
    let file = transcripts().open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    pa_core::session::catalog::parse_session_header_line(&first)
}

/// A session file is valid when its first line is a `session` header, judged
/// on the bounded header read.
#[must_use]
pub fn is_valid_session_file(path: &Path) -> bool {
    read_session_header_bounded(path).is_some()
}

impl SessionFile {
    /// Load an existing session file; malformed entry lines are skipped (the TS loader).
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, is empty, or its header is invalid.
    pub fn open(path: &Path) -> Result<Self> {
        // Streamed line-by-line load: one line is resident at a time, so a grown session
        // never holds the raw file bytes alongside the parsed entries.
        let file = transcripts()
            .open(path)
            .with_context(|| format!("read session file {}", path.display()))?;
        let mut lines = std::io::BufReader::new(file).lines();
        let read_context = || format!("read session file {}", path.display());
        let mut first = None;
        for line in lines.by_ref() {
            let line = line.with_context(read_context)?;
            if !line.trim().is_empty() {
                first = Some(line);
                break;
            }
        }
        let first = first.ok_or_else(|| anyhow!("empty session file {}", path.display()))?;
        let header_value: Value = serde_json::from_str(first.trim())
            .with_context(|| format!("invalid session header in {}", path.display()))?;
        if header_value.get("type").and_then(Value::as_str) != Some("session") {
            return Err(anyhow!("missing session header in {}", path.display()));
        }
        let header: SessionHeader = serde_json::from_value(header_value)
            .with_context(|| format!("invalid session header in {}", path.display()))?;
        let mut file = SessionFile {
            path: path.to_path_buf(),
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            chain: Vec::new(),
            leaf_id: None,
            branch_roles: Some(HashMap::new()),
            window: None,
            lease: None,
            child_usage_recovery: super::child_usage::ChildUsageRecovery::default(),
            trace_upload: None,
            anthropic_warning_shown: false,
            skipped_lines: 0,
            billed_row_sink: BilledRowSinkSlot::default(),
        };
        for line in lines {
            let line = line.with_context(read_context)?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Malformed lines are skipped, matching the TS loader. The
            // count rides the store so the reopen that found damage can
            // log it (the rows now STAY on disk: an append-only reopen
            // never deletes what it skipped).
            match serde_json::from_str::<SessionEntry>(trimmed) {
                Ok(entry) => file.push_index(entry),
                Err(_) => file.skipped_lines += 1,
            }
        }
        fold_child_usage_attributions(&mut file.entries);
        // The once-per-lifecycle gate rides the ACTIVE branch, exactly
        // like the windowed walk below: a marker on an abandoned or
        // sibling branch never suppresses the warning for the open leaf
        // (fail-open — an off-path marker must not hide the warning on a
        // branch that never showed it).
        file.anthropic_warning_shown = file.branch().iter().copied().any(is_warning_shown_row);
        Ok(file)
    }

    /// Load the verified compacted context without decoding old message bodies.
    ///
    /// # Errors
    ///
    /// Returns an error when the windowed load fails or holds no header; a
    /// malformed retained row falls back to [`SessionFile::open`].
    pub fn open_windowed(path: &Path) -> Result<Self> {
        let Some(mut window) = pa_core::session::window::WindowedSessionStore::open(path)? else {
            return Self::open(path);
        };
        let header = window
            .entries()
            .iter()
            .find_map(|entry| match entry {
                pa_types::session::FileEntry::Header { header } => Some(header.clone()),
                _ => None,
            })
            .ok_or_else(|| anyhow!("window has no session header"))?;
        // The window's walk already hydrated the once-per-session-lifecycle
        // warning gate from the whole active branch (the row may sit in the
        // discarded prefix, far outside this store's retained rows).
        let anthropic_warning_shown = window.anthropic_warning_shown();
        let mut file = Self {
            path: path.to_owned(),
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            chain: Vec::new(),
            leaf_id: None,
            // Stale until the first read: the recount adds the unloaded prefix's counts.
            branch_roles: None,
            window: None,
            lease: None,
            child_usage_recovery: super::child_usage::ChildUsageRecovery::default(),
            trace_upload: None,
            anthropic_warning_shown,
            skipped_lines: 0,
            billed_row_sink: BilledRowSinkSlot::default(),
        };
        // The raw rows are consumed in place: each line String drops as soon as its
        // parsed entry joins the store.
        let raw_count = window.raw_entries().len();
        for line in window
            .take_metadata_entries()
            .into_iter()
            .chain(window.take_raw_entries())
        {
            let Ok(entry) = serde_json::from_str(&line) else {
                return Self::open(path);
            };
            file.push_index(entry);
        }
        // The window parses the retained-target attributions BEFORE the raw
        // retained rows, so the fold runs once every row is in.
        fold_child_usage_attributions(&mut file.entries);
        file.set_leaf(Some(window.leaf_id().to_owned()));
        let settings = window.settings();
        file.window = Some(SessionWindow {
            message_count: window.message_count(),
            first_message: window
                .first_user_message()
                .map(message_text)
                .filter(|text| !text.is_empty()),
            loaded_entries: file.entries.len(),
            compaction_count: window.compaction_count(),
            has_thinking_level: window.has_thinking_level(),
            has_service_tier: window.has_service_tier(),
            model: settings.model.clone(),
            boundary_model: window.boundary_model().cloned(),
            thinking_level: settings.thinking_level.clone(),
            service_tier: settings.service_tier,
            // The retained rows joined the store verbatim above, so their ids are
            // exactly the trailing `raw_count` store ids — no third parse pass.
            retained_ids: file.entries[file.entries.len() - raw_count..]
                .iter()
                .map(|entry| entry.id.clone())
                .collect(),
            older_path_stats: window.older_path_stats().clone(),
        });
        Ok(file)
    }

    #[cfg(all(test, unix))] // only the unix-only windowed-reader tests use it
    pub(super) fn ensure_full_history(&mut self) -> Result<()> {
        if self.window.is_some() {
            let full = Self::open(&self.path)?;
            self.install_full_history(full);
        }
        Ok(())
    }

    /// Merge appends made while the disk snapshot loaded without holding the store lock.
    pub(crate) fn install_full_history(&mut self, mut full: Self) {
        let Some(window) = &self.window else {
            return;
        };
        for entry in &self.entries[window.loaded_entries..] {
            if !full.by_id.contains_key(&entry.id) {
                full.push_index(entry.clone());
            }
        }
        full.set_leaf(self.leaf_id.clone());
        // Same leaf, same branch: the window's counts already hold the unloaded prefix.
        full.branch_roles = self.branch_roles.take();
        full.lease.clone_from(&self.lease);
        full.trace_upload.clone_from(&self.trace_upload);
        full.billed_row_sink.clone_from(&self.billed_row_sink);
        full.child_usage_recovery
            .clone_from(&self.child_usage_recovery);
        // The merged store's leaf is the window's leaf: re-derive the gate
        // from the merged ACTIVE branch — the full open's own-leaf answer
        // can disagree, a post-snapshot marker must hydrate, and an
        // off-path one must not (the full scan never wins here).
        full.anthropic_warning_shown = full.branch().iter().copied().any(is_warning_shown_row);
        *self = full;
    }

    /// Create a new in-memory session; persisted with the first flush.
    pub fn create(cwd: &str, parent_session: Option<&str>, rlm_depth: u32) -> Self {
        let header = SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: new_session_id(),
            timestamp: crate::util::now_iso(),
            cwd: cwd.to_string(),
            parent_session: parent_session.map(str::to_string),
            rlm_depth: Some(u64::from(rlm_depth)),
            git: None,
            rest: Map::default(),
        };
        SessionFile {
            path: PathBuf::new(),
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            chain: Vec::new(),
            leaf_id: None,
            branch_roles: Some(HashMap::new()),
            window: None,
            lease: None,
            child_usage_recovery: super::child_usage::ChildUsageRecovery::default(),
            trace_upload: None,
            anthropic_warning_shown: false,
            skipped_lines: 0,
            billed_row_sink: BilledRowSinkSlot::default(),
        }
    }
}
