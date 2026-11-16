//! Workspace checkpoints: the file side of a conversation rewind.
//!
//! A shadow git store per workspace (`<agent_dir>/checkpoints/<digest>/`)
//! snapshots the files as git trees, keyed by session entry id and
//! [`Boundary`]. It never touches the workspace's own repository: it has its
//! own git directory, one index per session, and the user's global/system git
//! config shut out (no hook, filter, or fsmonitor can start). The workspace's
//! `.gitignore` rules apply; nested repositories are kept as gitlinks only.

#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Why a checkpoint operation failed; every failure degrades the rewind to
/// conversation-only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CheckpointError(String);

impl From<std::io::Error> for CheckpointError {
    fn from(error: std::io::Error) -> Self {
        CheckpointError(error.to_string())
    }
}

/// Which side of an entry a recorded tree describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Boundary {
    /// The workspace as the turn that appended the entry found it.
    Before,
    /// The workspace while the conversation stood at the entry.
    At,
}

/// A git tree id in the shadow store.
pub type TreeId = String;

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// The workspace before the restore (record it to come back).
    pub preimage: TreeId,
    /// Paths written or deleted.
    pub restored: usize,
    /// Paths left alone because they changed outside the session.
    pub conflicts: Vec<String>,
}

/// A git command past this is killed: the workspace is too large to checkpoint.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Each path's bytes exactly as on disk: the workspace's `.gitattributes`
/// conversions and filter drivers never apply.
const STORE_ATTRIBUTES: &str = "* -text -eol -ident -filter -diff -merge -working-tree-encoding\n";

/// `gc.auto = 0` is load-bearing: trees are unreferenced objects an auto-gc
/// could prune before the rewind that needs them.
const STORE_CONFIG: &[(&str, &str)] = &[("core.autocrlf", "false"), ("gc.auto", "0")];

/// One session's view of a workspace's shadow store.
pub struct CheckpointStore {
    git_dir: PathBuf,
    workspace: PathBuf,
    index: PathBuf,
    empty_config: PathBuf,
    map_path: PathBuf,
    /// The recorded trees, newest row winning; the lock also keeps one walk of
    /// the session index at a time.
    map: Mutex<HashMap<(String, Boundary), TreeId>>,
}

impl CheckpointStore {
    /// Open (initializing when absent) the store for `workspace`, scoped to
    /// `session_id`'s index and map.
    ///
    /// # Errors
    /// The workspace is a system, temp, or home directory, or initializing the
    /// store failed.
    pub fn open(
        agent_dir: &Path,
        workspace: &Path,
        session_id: &str,
    ) -> Result<Self, CheckpointError> {
        let workspace = std::fs::canonicalize(workspace)?;
        if is_system_directory(&workspace) {
            return Err(CheckpointError(format!(
                "{} is not a project workspace",
                workspace.display()
            )));
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(workspace.to_string_lossy().as_bytes())
        );
        let root = agent_dir.join("checkpoints").join(&digest[..32]);
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions)?;
        let session = session_id.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_");
        let map_path = sessions.join(format!("{session}.jsonl"));
        let map = std::fs::read_to_string(&map_path)
            .unwrap_or_default()
            .lines()
            // A torn last line (a crash mid-append) is skipped, not fatal.
            .filter_map(|line| serde_json::from_str::<(String, Boundary, TreeId)>(line).ok())
            .map(|(entry, boundary, tree)| ((entry, boundary), tree))
            .collect();
        let store = CheckpointStore {
            git_dir: root.join("git"),
            workspace,
            index: sessions.join(format!("{session}.index")),
            empty_config: root.join("empty.gitconfig"),
            map_path,
            map: Mutex::new(map),
        };
        if !store.git_dir.join("HEAD").exists() {
            std::fs::write(&store.empty_config, "")?;
            store.git(&["init", "--quiet"])?;
            for (key, value) in STORE_CONFIG {
                store.git(&["config", key, value])?;
            }
        }
        let info = store.git_dir.join("info");
        std::fs::create_dir_all(&info)?;
        std::fs::write(info.join("attributes"), STORE_ATTRIBUTES)?;
        Ok(store)
    }

    /// Stage the whole workspace into the session index and return its tree
    /// (an unchanged workspace yields the previous id).
    ///
    /// # Errors
    /// A git failure or the timeout.
    pub fn capture(&self) -> Result<TreeId, CheckpointError> {
        let _walk = self.lock();
        self.git(&["add", "--all", "--ignore-errors", "--", "."])?;
        let tree = String::from_utf8_lossy(&self.git(&["write-tree"])?)
            .trim()
            .to_string();
        Ok(tree)
    }

    /// Record `tree` for `entry_id` at `boundary` (appended to the session map).
    ///
    /// # Errors
    /// The map file could not be appended.
    pub fn record(
        &self,
        entry_id: &str,
        boundary: Boundary,
        tree: &TreeId,
    ) -> Result<(), CheckpointError> {
        let mut line = serde_json::to_string(&(entry_id, boundary, tree))
            .map_err(|error| CheckpointError(error.to_string()))?;
        line.push('\n');
        let mut map = self.lock();
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.map_path)?
            .write_all(line.as_bytes())?;
        map.insert((entry_id.to_string(), boundary), tree.clone());
        Ok(())
    }

    /// The tree recorded for `entry_id` at `boundary`, if any.
    #[must_use]
    pub fn lookup(&self, entry_id: &str, boundary: Boundary) -> Option<TreeId> {
        self.lock().get(&(entry_id.to_string(), boundary)).cloned()
    }

    /// Put the workspace back to `target`, writing only the differing paths.
    /// `baseline` is the session's last recorded state: a path whose current
    /// content differs from it changed outside the session and is left alone
    /// (reported as a conflict).
    ///
    /// # Errors
    /// The capture, the diff, a deletion, or the `git restore` failed; paths
    /// already written stay written.
    pub fn restore(
        &self,
        target: &TreeId,
        baseline: Option<&TreeId>,
    ) -> Result<RestoreOutcome, CheckpointError> {
        let preimage = self.capture()?;
        let foreign: BTreeSet<String> = match baseline {
            Some(baseline) if *baseline != preimage => self
                .diff(baseline, &preimage)?
                .into_iter()
                .map(|(path, _)| path)
                .collect(),
            _ => BTreeSet::new(),
        };
        let (conflicts, changes): (Vec<_>, Vec<_>) = self
            .diff(&preimage, target)?
            .into_iter()
            .partition(|(path, _)| foreign.contains(path));
        let mut writes = Vec::new();
        for (path, in_target) in &changes {
            if *in_target {
                writes.push(path.as_str());
            } else {
                remove_path(&self.workspace, path)?;
            }
        }
        let source = format!("--source={target}");
        for chunk in writes.chunks(256) {
            let mut args = vec!["restore", source.as_str(), "--worktree", "--"];
            args.extend_from_slice(chunk);
            self.git(&args)?;
        }
        Ok(RestoreOutcome {
            preimage,
            restored: changes.len(),
            conflicts: conflicts.into_iter().map(|(path, _)| path).collect(),
        })
    }

    /// The differing paths from `from` to `to`, each with whether `to` has it.
    fn diff(&self, from: &str, to: &str) -> Result<Vec<(String, bool)>, CheckpointError> {
        let stdout = self.git(&[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-status",
            from,
            to,
        ])?;
        let fields: Vec<&[u8]> = stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect();
        Ok(fields
            .chunks_exact(2)
            .map(|pair| {
                (
                    String::from_utf8_lossy(pair[1]).into_owned(),
                    pair[0] != b"D",
                )
            })
            .collect())
    }

    /// Run `git <args>` against the shadow store; stdout on success.
    fn git(&self, args: &[&str]) -> Result<Vec<u8>, CheckpointError> {
        let label = args[0];
        let mut child = Command::new("git")
            .args(args)
            .current_dir(&self.workspace)
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.workspace)
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.empty_config)
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| CheckpointError(format!("git {label}: {error}")))?;
        // Drained on its own thread so a full pipe never stalls the child.
        let mut pipe = child.stdout.take().expect("stdout is piped");
        let stdout = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        });
        let deadline = Instant::now() + GIT_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                // A kill failure means it already exited; the wait reaps either way.
                let _ = child.kill();
                let _ = child.wait();
                return Err(CheckpointError(
                    "workspace is too large to checkpoint".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = stdout.join().unwrap_or_else(|_| Ok(Vec::new()))?;
        if status.success() {
            Ok(stdout)
        } else {
            Err(CheckpointError(format!("git {label} failed ({status})")))
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, Boundary), TreeId>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Delete a path the target does not have, then the parent directories the
/// deletion left empty (never the workspace root).
fn remove_path(workspace: &Path, relative: &str) -> Result<(), CheckpointError> {
    let path = workspace.join(relative);
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    }
    for dir in path.ancestors().skip(1).take_while(|dir| *dir != workspace) {
        // A non-empty directory refuses the removal, which ends the walk.
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
    }
    Ok(())
}

/// Never one workspace: the root, a top-level directory (`/tmp`, `/home`,
/// `/usr`, ...), the temp directory, the home directory, or its parent.
fn is_system_directory(workspace: &Path) -> bool {
    let same =
        |candidate: &Path| std::fs::canonicalize(candidate).is_ok_and(|path| path == workspace);
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    workspace.components().count() <= 2
        || same(&std::env::temp_dir())
        || home
            .as_deref()
            .is_some_and(|home| same(home) || home.parent().is_some_and(same))
}
