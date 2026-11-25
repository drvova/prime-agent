//! The load concern: the serialized-entry wire and the header-validating load
//! (the torn-tail repair it runs first is the store's
//! [`repair_tail`](crate::session::store::TranscriptStore::repair_tail)).

use super::{parse_session_entries, FileEntry, Path};

pub(super) fn serialize_entry(entry: &FileEntry) -> String {
    serde_json::to_string(entry).unwrap_or_default()
}

/// Load entries from a session file (repairing damage first when persisting).
#[must_use]
pub fn load_entries_from_file(file_path: &Path, repair: bool) -> Vec<FileEntry> {
    let store = crate::session::store::store();
    if !store.exists(file_path) {
        return Vec::new();
    }
    if repair {
        store.repair_tail(file_path);
    }
    let Ok(content) = crate::session::store::read_to_string(file_path) else {
        return Vec::new();
    };
    finalize_loaded_entries(parse_session_entries(&content))
}

/// Finalize: entries need a valid header first; attributions fold in.
fn finalize_loaded_entries(entries: Vec<FileEntry>) -> Vec<FileEntry> {
    if entries.is_empty() {
        return entries;
    }
    let valid_header = matches!(&entries[0], FileEntry::Header { .. });
    if !valid_header {
        return Vec::new();
    }
    entries
}
