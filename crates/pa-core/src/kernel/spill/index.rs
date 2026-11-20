//! The spill store's append-only index: one JSON record per line, each
//! naming a handle's latest state. Folding the lines in order rebuilds the
//! store's view in a fresh process; a torn or malformed line (a crash
//! mid-append) is skipped.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The counts of one stored spill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredMeta {
    /// Bytes of the stored file.
    pub(crate) bytes: u64,
    /// Chars of the stored text.
    pub(crate) chars: usize,
    /// Chars of the whole output (more than `chars` when its copy was capped).
    pub(crate) total_chars: usize,
    /// Lines of the whole output.
    pub(crate) lines: usize,
}

/// One index line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum IndexRecord {
    Stored {
        id: String,
        #[serde(flatten)]
        meta: StoredMeta,
    },
    Failed {
        id: String,
        error: String,
    },
    Evicted {
        id: String,
    },
}

/// One handle's folded state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    Stored(StoredMeta),
    Failed(String),
    Evicted,
}

/// The folded index: each handle's state, and the stored ones oldest first.
#[derive(Debug, Default)]
pub(crate) struct Index {
    pub(crate) entries: HashMap<String, Entry>,
    stored_order: VecDeque<String>,
    pub(crate) stored_bytes: u64,
}

impl Index {
    /// Fold the index file at `path`; a missing or unreadable file is empty.
    pub(crate) fn load(path: &Path) -> Self {
        let mut index = Self::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return index;
        };
        for record in text
            .lines()
            .filter_map(|line| serde_json::from_str::<IndexRecord>(line).ok())
        {
            index.apply(record);
        }
        index
    }

    /// Fold one record in.
    pub(crate) fn apply(&mut self, record: IndexRecord) {
        match record {
            IndexRecord::Stored { id, meta } => {
                if let Some(Entry::Stored(_)) = self.entries.get(&id) {
                    return;
                }
                self.stored_bytes += meta.bytes;
                self.stored_order.push_back(id.clone());
                self.entries.insert(id, Entry::Stored(meta));
            }
            IndexRecord::Failed { id, error } => match self.entries.get(&id) {
                Some(Entry::Stored(_)) => {}
                Some(Entry::Failed(_) | Entry::Evicted) | None => {
                    self.entries.insert(id, Entry::Failed(error));
                }
            },
            IndexRecord::Evicted { id } => {
                if let Some(Entry::Stored(meta)) = self.entries.get(&id) {
                    self.stored_bytes -= meta.bytes;
                    self.stored_order.retain(|stored| *stored != id);
                    self.entries.insert(id, Entry::Evicted);
                }
            }
        }
    }

    /// The oldest stored handle other than `keep` (the spill just stored).
    pub(crate) fn oldest_stored_except(&self, keep: &str) -> Option<String> {
        self.stored_order.iter().find(|id| *id != keep).cloned()
    }
}
