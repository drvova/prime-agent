//! The tree-identity concern: the content hash over a sorted file list (relative path plus bytes)
//! that names the bundled runtime source and the installed `rlm`/`dill` trees, memoized in a small
//! on-disk map inside the venv. An entry is keyed by the tree root and a stat fingerprint of the
//! same files (relative path, size, mtime, and on Unix ctime, inode, and device), so an unchanged
//! tree is identified with one stat per file instead of reading and hashing every byte, in this
//! process and in every fresh worker process after it. Every I/O error on the memo is fail-open: a
//! miss rehashes.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{Digest, Path, PathBuf};

/// The on-disk schema; an entry written under another schema reads as a miss.
const TREE_IDENTITY_SCHEMA: u64 = 1;
pub(super) const TREE_IDENTITY_FILE: &str = ".tree-identity-cache.json";
/// Same bound and clear-at-cap rule as the runtime-ready memo: entries churn only with tree roots.
const TREE_IDENTITY_CAP: usize = 16;

#[derive(Serialize, Deserialize)]
struct TreeIdentityMap {
    schema: u64,
    entries: Vec<TreeIdentityEntry>,
}

#[derive(Serialize, Deserialize)]
struct TreeIdentityEntry {
    root: String,
    fingerprint: String,
    hash: String,
}

/// The memo file one venv keeps for the trees its bootstrap identifies.
pub(in crate::kernel::bootstrap) struct TreeIdentityCache {
    path: PathBuf,
}

impl TreeIdentityCache {
    pub(in crate::kernel::bootstrap) fn in_venv(venv: &Path) -> Self {
        Self {
            path: venv.join(TREE_IDENTITY_FILE),
        }
    }

    /// The hex SHA-256 over `files` (relative to `root`, in the given order): served from the memo
    /// when the stat fingerprint still matches, else read, hashed, and published. The fingerprint is
    /// taken before the bytes are read, so a file changed mid-hash leaves a stale fingerprint that
    /// misses on the next call instead of a stale hash that hits.
    pub(super) fn content_hash(&self, root: &Path, files: &[PathBuf]) -> anyhow::Result<String> {
        let fingerprint = stat_fingerprint(root, files)?;
        let root_key = root.to_string_lossy().into_owned();
        let mut entries = read_entries(&self.path);
        if let Some(hit) = entries
            .iter()
            .find(|entry| entry.root == root_key && entry.fingerprint == fingerprint)
        {
            return Ok(hit.hash.clone());
        }
        #[cfg(test)]
        record_content_read_for_tests(root);
        let mut hasher = sha2::Sha256::new();
        for file in files {
            let relative = file.strip_prefix(root)?;
            hasher.update(relative.to_string_lossy().as_bytes());
            hasher.update([0]);
            hasher.update(&std::fs::read(file)?);
            hasher.update([0]);
        }
        let hash = format!("{:x}", hasher.finalize());
        entries.retain(|entry| entry.root != root_key);
        if entries.len() >= TREE_IDENTITY_CAP {
            entries.clear();
        }
        entries.push(TreeIdentityEntry {
            root: root_key,
            fingerprint,
            hash: hash.clone(),
        });
        write_entries(&self.path, entries);
        Ok(hash)
    }
}

/// Hex SHA-256 over each file's relative path and stat witness: any add, remove, rename, resize, or
/// rewrite that moves a timestamp (or, on Unix, replaces the inode) changes it.
fn stat_fingerprint(root: &Path, files: &[PathBuf]) -> anyhow::Result<String> {
    let mut hasher = sha2::Sha256::new();
    for file in files {
        let meta = std::fs::metadata(file)?;
        let modified = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        hasher.update(file.strip_prefix(root)?.to_string_lossy().as_bytes());
        hasher.update(format!("\0{}\0{modified}", meta.len()).as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            hasher.update(
                format!(
                    "\0{}.{}\0{}\0{}",
                    meta.ctime(),
                    meta.ctime_nsec(),
                    meta.ino(),
                    meta.dev()
                )
                .as_bytes(),
            );
        }
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// A memo is only read from a regular file: a symlink or a directory reads as empty.
fn read_entries(path: &Path) -> Vec<TreeIdentityEntry> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Vec::new();
    };
    if !meta.is_file() {
        return Vec::new();
    }
    let Ok(raw) = std::fs::read(path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<TreeIdentityMap>(&raw) {
        Ok(map) if map.schema == TREE_IDENTITY_SCHEMA => map.entries,
        Ok(_) | Err(_) => Vec::new(),
    }
}

/// Publish through a uniquely named temp file renamed over the memo path, so a concurrent reader
/// never sees a half-written map and concurrent writers never share a temp file. Fail-open: a
/// missing or unwritable venv skips persistence.
fn write_entries(path: &Path, entries: Vec<TreeIdentityEntry>) {
    static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let map = TreeIdentityMap {
        schema: TREE_IDENTITY_SCHEMA,
        entries,
    };
    let Ok(body) = serde_json::to_vec(&map) else {
        return;
    };
    let tmp = path.with_file_name(format!(
        "{TREE_IDENTITY_FILE}.tmp.{}.{}",
        std::process::id(),
        WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, &body).is_err() {
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Roots whose bytes were read and hashed, in order: the no-rehash oracle. Each test uses its own
/// temp roots, so parallel tests never see each other's reads.
#[cfg(test)]
static CONTENT_READS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn record_content_read_for_tests(root: &Path) {
    CONTENT_READS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(root.to_path_buf());
}

/// Read by the unix-only stat-fingerprint test (its interpreter fixture is a shell script).
#[cfg(all(test, unix))]
pub(super) fn content_reads_for_tests(root: &Path) -> usize {
    CONTENT_READS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|read| read.as_path() == root)
        .count()
}
