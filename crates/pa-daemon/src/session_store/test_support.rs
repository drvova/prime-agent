//! Test fixtures for the session store's callers.

use std::path::Path;

/// Make the transcript at `path` a JSONL file on disk instead: an original
/// the transcript store has not imported, read as it is, whose bytes and
/// modification time a test can set directly (a session from before the
/// store kept its transcripts, or one another build wrote).
pub(crate) fn into_original_file(path: &Path) {
    let store = pa_core::session::store::store();
    let bytes = store.read(path).expect("read the stored transcript");
    store.remove(path).expect("remove the stored transcript");
    std::fs::write(path, bytes).expect("write the original file");
}
