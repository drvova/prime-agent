//! The deferred transcript search: a query that rests for [`CONTENT_SEARCH_DEBOUNCE`] also
//! matches inside each listed session's conversation text. The scan reads the session files on a
//! blocking worker, never on the paint path; its keyed answer lands as a `ContentSearchLoaded`
//! input, and only the newest query's answer applies.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use super::UiInput;

/// How long the query rests before the transcript scan starts.
pub(super) const CONTENT_SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Session files past this size are skipped, so one huge transcript cannot hold the scan.
const CONTENT_SEARCH_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// The transcript-match state of the current query.
#[derive(Debug, Default)]
pub(super) struct ContentSearch {
    /// Matches per record identity for the newest landed scan.
    hits: HashMap<String, usize>,
    /// The debounce deadline of the newest query edit; `None` when no scan is due.
    due: Option<tokio::time::Instant>,
    /// Bumped by every query edit: a scan answers only the generation it was started for.
    generation: u64,
}

/// One scan's work order: the session files to read, keyed by record identity.
pub(super) struct ContentSearchRequest {
    pub(super) generation: u64,
    needle: String,
    files: Vec<(String, PathBuf)>,
}

impl ContentSearch {
    /// A query edit: the previous answer drops and a scan is due after the debounce. An empty
    /// query or a `re:` pattern (title-only) schedules none.
    pub(super) fn query_changed(&mut self, query: &str) {
        self.generation += 1;
        self.hits.clear();
        let query = query.trim();
        self.due = (!query.is_empty() && !query.starts_with("re:"))
            .then(|| tokio::time::Instant::now() + CONTENT_SEARCH_DEBOUNCE);
    }

    pub(super) fn due(&self) -> Option<tokio::time::Instant> {
        self.due
    }

    pub(super) fn hits(&self) -> &HashMap<String, usize> {
        &self.hits
    }

    pub(super) fn hit_count(&self, identity: &str) -> Option<usize> {
        self.hits.get(identity).copied()
    }

    /// The due scan's work order (the debounce elapsed); `None` when none is due.
    pub(super) fn take_due(
        &mut self,
        query: &str,
        files: Vec<(String, PathBuf)>,
    ) -> Option<ContentSearchRequest> {
        self.due.take()?;
        Some(ContentSearchRequest {
            generation: self.generation,
            needle: query.trim().to_lowercase(),
            files,
        })
    }

    /// A landed scan: applies only when no query edit followed its start.
    pub(super) fn land(&mut self, generation: u64, hits: HashMap<String, usize>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.hits = hits;
        true
    }
}

/// Run one scan on a blocking worker; its answer re-enters the loop as a `ContentSearchLoaded`.
pub(super) fn spawn_content_search(
    request: ContentSearchRequest,
    ui_tx: mpsc::UnboundedSender<UiInput>,
) {
    tokio::spawn(async move {
        let generation = request.generation;
        let hits = tokio::task::spawn_blocking(move || request.scan())
            .await
            .unwrap_or_default();
        let _ = ui_tx.send(UiInput::ContentSearchLoaded { generation, hits });
    });
}

impl ContentSearchRequest {
    /// Read every file and count the case-insensitive occurrences of the query in its user and
    /// assistant text; sessions without one are left out. An unreadable or oversized file counts
    /// none. Blocking: callers run it on a blocking worker.
    pub(super) fn scan(&self) -> HashMap<String, usize> {
        let needle = self.needle.as_str();
        self.files
            .iter()
            .filter_map(|(identity, path)| {
                let readable = std::fs::metadata(path)
                    .is_ok_and(|meta| meta.len() <= CONTENT_SEARCH_MAX_FILE_BYTES);
                let text = readable
                    .then(|| std::fs::read_to_string(path).ok())
                    .flatten()?;
                let count: usize = text
                    .lines()
                    // The raw-line check skips the JSON decode for every line that cannot match.
                    .filter(|line| line.to_lowercase().contains(needle))
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("message"))
                    .filter_map(|entry| entry.get("message").cloned())
                    .filter(|message| {
                        matches!(
                            message.get("role").and_then(Value::as_str),
                            Some("user" | "assistant")
                        )
                    })
                    .map(|message| {
                        crate::snapshot::message_text(&message)
                            .to_lowercase()
                            .matches(needle)
                            .count()
                    })
                    .sum();
                (count > 0).then(|| (identity.clone(), count))
            })
            .collect()
    }
}

impl super::AgentsViewMode {
    /// The due scan's work order over every listed session's file (debounce elapsed).
    pub(super) fn take_content_search(&mut self) -> Option<ContentSearchRequest> {
        let files = self
            .records()
            .iter()
            .filter_map(|record| {
                let summary = crate::agents_view_state::summary_for_record(record);
                let path = summary.get("sessionFile").and_then(Value::as_str)?;
                Some((record.identity.clone(), PathBuf::from(path)))
            })
            .collect();
        self.content_search.take_due(&self.query, files)
    }

    /// A landed scan: transcript-only matches join the list, each row shows its count.
    pub(super) fn content_search_loaded(&mut self, generation: u64, hits: HashMap<String, usize>) {
        let found = !hits.is_empty();
        if !self.content_search.land(generation, hits) {
            return;
        }
        // One adoption action per view run that found transcript matches.
        if found && !self.actions.contains(&CONTENT_SEARCH_ACTION) {
            self.actions.push(CONTENT_SEARCH_ACTION);
        }
        self.rebuild_rows();
        self.sync_selected_row_state();
    }
}

/// The `tui_agents_action_count` action a transcript match reports.
const CONTENT_SEARCH_ACTION: &str = "content_search";
