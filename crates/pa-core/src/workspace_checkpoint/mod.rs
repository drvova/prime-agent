//! Workspace checkpoints: the file side of a conversation rewind.
//!
//! A shadow git store per workspace (under `<agent_dir>/checkpoints/<digest>/`)
//! snapshots the workspace's files as git trees at turn boundaries, keyed by
//! session entry id, so a tree navigation can put the files back the way they
//! were when the conversation stood there. The store never touches the
//! workspace's own repository: it has its own git directory, one index per
//! session, and the user's global/system git configuration shut out.
//!
//! Two boundaries are recorded per entry:
//!
//! - [`Boundary::Before`] — the workspace as the turn that appended the entry
//!   found it (rewinding to before a prompt restores this);
//! - [`Boundary::At`] — the workspace when the conversation stood at the entry
//!   (a turn's settle records it on the new leaf; a rewind records the
//!   abandoned leaf's state there, so navigating back restores it — redo).
//!
//! A restore writes only the paths that differ, and refuses a path whose
//! current content was changed outside the session since its last recorded
//! state (the conflict guard): those paths stay as they are and come back as
//! conflicts.
//!
//! Limits: nested repositories are recorded as gitlinks (their contents are
//! not snapshotted), the workspace's `.gitignore` rules apply, and a workspace
//! that cannot be staged within the git timeout is not checkpointed.

mod git;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use git::GitEnv;

/// Why a checkpoint operation could not run. Every variant degrades the
/// rewind to conversation-only; none is fatal to the session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    /// No `git` executable on PATH.
    #[error("git is not installed")]
    GitMissing,
    /// The workspace is a system, temp, or home directory — never checkpointed.
    #[error("{0} is not a project workspace")]
    NotAWorkspace(String),
    /// A git command ran past its timeout (the workspace is too large).
    #[error("workspace is too large to checkpoint")]
    TimedOut,
    /// An earlier capture failed; this store stays off for the process.
    #[error("checkpoints are off for this workspace: {0}")]
    Disabled(String),
    /// A git command failed.
    #[error("{0}")]
    Git(String),
    /// A filesystem operation failed.
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for CheckpointError {
    fn from(error: std::io::Error) -> Self {
        CheckpointError::Io(error.to_string())
    }
}

/// Which side of an entry a recorded tree describes (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Boundary {
    Before,
    At,
}

/// A git tree id in the shadow store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TreeId(String);

impl TreeId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// The workspace as it stood before the restore (record it for redo).
    pub preimage: TreeId,
    /// Paths written or deleted.
    pub restored: usize,
    /// Paths left alone because they changed outside the session.
    pub conflicts: Vec<String>,
}

/// One line of a session's checkpoint map (`sessions/<id>.jsonl`).
#[derive(Debug, Serialize, Deserialize)]
struct MapRow {
    #[serde(rename = "entryId")]
    entry_id: String,
    boundary: Boundary,
    tree: TreeId,
}

/// The `info/attributes` every store keeps: each path's bytes exactly as on
/// disk. The workspace's own `.gitattributes` serve its repository; an eol
/// conversion or a filter driver would make a restore write different bytes
/// (and a filter driver would start a program).
const STORE_ATTRIBUTES: &str = "* -text -eol -ident -filter -diff -merge -working-tree-encoding\n";

/// The config a new store gets past `git init`'s own lines. `gc.auto = 0` is
/// load-bearing: trees are unreferenced objects, and an auto-gc between a
/// capture and a rewind could prune the tree the rewind needs.
const STORE_CONFIG: &[(&str, &str)] = &[
    ("core.bare", "false"),
    ("core.autocrlf", "false"),
    ("core.symlinks", "true"),
    ("core.fsmonitor", "false"),
    ("core.untrackedCache", "true"),
    ("gc.auto", "0"),
    ("index.version", "4"),
];

/// Directories that are never one workspace (only the folders themselves: a
/// project inside `/tmp` is a workspace like any other).
const SYSTEM_DIRECTORIES: &[&str] = &[
    "/",
    "/tmp",
    "/var/tmp",
    "/dev/shm",
    "/private/tmp",
    "/private/var/tmp",
    "/var",
    "/usr",
    "/etc",
    "/opt",
    "/srv",
    "/mnt",
    "/media",
    "/boot",
    "/run",
    "/proc",
    "/sys",
    "/dev",
    "/home",
    "/Users",
    "/Volumes",
    "/private",
];

/// One session's view of a workspace's shadow store.
pub struct CheckpointStore {
    env: GitEnv,
    map_path: PathBuf,
    /// The recorded trees, newest row winning; also serializes captures (one
    /// walk of the session index at a time).
    state: Mutex<StoreState>,
}

struct StoreState {
    map: HashMap<(String, Boundary), TreeId>,
    /// Set by a capture that timed out or failed: later captures answer it
    /// without starting git.
    disabled: Option<String>,
}

impl CheckpointStore {
    /// Open (initializing when absent) the store for `workspace`, scoped to
    /// `session_id`'s index and map.
    ///
    /// # Errors
    /// [`CheckpointError::NotAWorkspace`] for a system, temp, or home
    /// directory; git or filesystem failures initializing the store.
    pub fn open(
        agent_dir: &Path,
        workspace: &Path,
        session_id: &str,
    ) -> Result<Self, CheckpointError> {
        let workspace = std::fs::canonicalize(workspace)?;
        if is_system_directory(&workspace) {
            return Err(CheckpointError::NotAWorkspace(
                workspace.display().to_string(),
            ));
        }
        let root = agent_dir
            .join("checkpoints")
            .join(workspace_digest(&workspace));
        let git_dir = root.join("git");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions)?;
        let empty_config = root.join("empty.gitconfig");
        if !empty_config.exists() {
            std::fs::write(&empty_config, "")?;
        }
        let session = file_safe(session_id);
        let env = GitEnv {
            git_dir: git_dir.clone(),
            work_tree: workspace.clone(),
            index: sessions.join(format!("{session}.index")),
            empty_config: empty_config.clone(),
        };
        if !git_dir.join("HEAD").exists() {
            git::init_git_dir(&git_dir, &empty_config)?;
            for (key, value) in STORE_CONFIG {
                env.run(&["config", key, value], None)?;
            }
        }
        let info = git_dir.join("info");
        std::fs::create_dir_all(&info)?;
        write_if_changed(&info.join("attributes"), STORE_ATTRIBUTES)?;
        write_if_changed(
            &info.join("exclude"),
            &store_excludes(&workspace, agent_dir),
        )?;
        let map_path = sessions.join(format!("{session}.jsonl"));
        let map = read_map(&map_path);
        Ok(CheckpointStore {
            env,
            map_path,
            state: Mutex::new(StoreState {
                map,
                disabled: None,
            }),
        })
    }

    /// The canonical workspace this store snapshots.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.env.work_tree
    }

    /// Stage the whole workspace into the session index and return its tree.
    /// An unchanged workspace yields the previous tree id (nothing new is
    /// written). A timeout or failure turns this store off for the process.
    ///
    /// # Errors
    /// The git failure or timeout, then [`CheckpointError::Disabled`] on
    /// every later call.
    pub fn capture(&self) -> Result<TreeId, CheckpointError> {
        let mut state = self.lock();
        if let Some(reason) = &state.disabled {
            return Err(CheckpointError::Disabled(reason.clone()));
        }
        let result = self
            .env
            .run(&["add", "--all", "--ignore-errors", "--", "."], None)
            .and_then(|_| self.env.line(&["write-tree"]))
            .and_then(validate_tree);
        if let Err(error) = &result {
            state.disabled = Some(error.to_string());
        }
        result
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
        let row = MapRow {
            entry_id: entry_id.to_string(),
            boundary,
            tree: tree.clone(),
        };
        let mut line =
            serde_json::to_string(&row).map_err(|error| CheckpointError::Io(error.to_string()))?;
        line.push('\n');
        let mut state = self.lock();
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.map_path)?
            .write_all(line.as_bytes())?;
        state
            .map
            .insert((entry_id.to_string(), boundary), tree.clone());
        Ok(())
    }

    /// The tree recorded for `entry_id` at `boundary`, if any.
    #[must_use]
    pub fn lookup(&self, entry_id: &str, boundary: Boundary) -> Option<TreeId> {
        self.lock()
            .map
            .get(&(entry_id.to_string(), boundary))
            .cloned()
    }

    /// The paths a restore to `target` would change, against the workspace as
    /// it stands now (one capture).
    ///
    /// # Errors
    /// The capture or the diff failed.
    pub fn changed_paths(&self, target: &TreeId) -> Result<Vec<String>, CheckpointError> {
        let current = self.capture()?;
        Ok(self
            .diff(&current, target)?
            .into_iter()
            .map(|change| change.path)
            .collect())
    }

    /// Put the workspace back to `target`. `baseline` is the session's last
    /// recorded state of the workspace: a path whose current content differs
    /// from it was changed outside the session and is left alone (reported as
    /// a conflict). Without a baseline every differing path is restored.
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
        let changes = self.diff(&preimage, target)?;
        let foreign: BTreeSet<String> = match baseline {
            Some(baseline) if baseline != &preimage => self
                .diff(baseline, &preimage)?
                .into_iter()
                .map(|change| change.path)
                .collect(),
            _ => BTreeSet::new(),
        };
        let (conflicts, changes): (Vec<Change>, Vec<Change>) = changes
            .into_iter()
            .partition(|change| foreign.contains(&change.path));
        let mut writes = Vec::new();
        for change in &changes {
            if change.in_target {
                writes.push(change.path.as_str());
            } else {
                remove_path(self.workspace(), &change.path)?;
            }
        }
        if !writes.is_empty() {
            let mut pathspec = Vec::new();
            for path in &writes {
                pathspec.extend_from_slice(path.as_bytes());
                pathspec.push(0);
            }
            let source = format!("--source={}", target.as_str());
            self.env.run(
                &[
                    "restore",
                    &source,
                    "--worktree",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ],
                Some(&pathspec),
            )?;
        }
        Ok(RestoreOutcome {
            preimage,
            restored: changes.len(),
            conflicts: conflicts.into_iter().map(|change| change.path).collect(),
        })
    }

    /// The per-path differences from `from` to `to`.
    fn diff(&self, from: &TreeId, to: &TreeId) -> Result<Vec<Change>, CheckpointError> {
        let stdout = self.env.run(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--name-status",
                from.as_str(),
                to.as_str(),
            ],
            None,
        )?;
        let mut fields = stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty());
        let mut changes = Vec::new();
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            changes.push(Change {
                path: String::from_utf8_lossy(path).into_owned(),
                in_target: status.first() != Some(&b'D'),
            });
        }
        Ok(changes)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One differing path: written from the target when the target has it,
/// deleted otherwise.
struct Change {
    path: String,
    in_target: bool,
}

fn validate_tree(line: String) -> Result<TreeId, CheckpointError> {
    let valid = matches!(line.len(), 40 | 64) && line.bytes().all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(TreeId(line))
    } else {
        Err(CheckpointError::Git(format!(
            "write-tree returned {line:?}"
        )))
    }
}

/// Delete a path the target tree does not have, then any parent directories
/// the deletion left empty (up to, never including, the workspace root).
fn remove_path(workspace: &Path, relative: &str) -> Result<(), CheckpointError> {
    let path = workspace.join(relative);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let mut parent = path.parent();
    while let Some(dir) = parent.filter(|dir| *dir != workspace && dir.starts_with(workspace)) {
        // A non-empty directory refuses the removal, which ends the walk.
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        parent = dir.parent();
    }
    Ok(())
}

fn read_map(path: &Path) -> HashMap<(String, Boundary), TreeId> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        // A torn last line (a crash mid-append) is skipped, not fatal.
        .filter_map(|line| serde_json::from_str::<MapRow>(line).ok())
        .map(|row| ((row.entry_id, row.boundary), row.tree))
        .collect()
}

fn write_if_changed(path: &Path, text: &str) -> Result<(), CheckpointError> {
    if std::fs::read_to_string(path).ok().as_deref() != Some(text) {
        std::fs::write(path, text)?;
    }
    Ok(())
}

/// The store's `info/exclude`: the agent directory when it sits inside the
/// workspace (a rewind must never roll back the session log that recorded it).
fn store_excludes(workspace: &Path, agent_dir: &Path) -> String {
    let agent_dir = std::fs::canonicalize(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
    match agent_dir.strip_prefix(workspace) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            format!("/{}/\n", relative.to_string_lossy().replace('\\', "/"))
        }
        _ => String::new(),
    }
}

fn is_system_directory(workspace: &Path) -> bool {
    let same = |candidate: &Path| {
        std::fs::canonicalize(candidate).is_ok_and(|canonical| canonical == workspace)
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    SYSTEM_DIRECTORIES.iter().any(|dir| same(Path::new(dir)))
        || same(&std::env::temp_dir())
        || home
            .as_deref()
            .is_some_and(|home| same(home) || home.parent().is_some_and(same))
}

fn workspace_digest(workspace: &Path) -> String {
    let digest = Sha256::digest(workspace.to_string_lossy().as_bytes());
    digest.iter().take(16).fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// A session id as a file-name stem.
fn file_safe(session_id: &str) -> String {
    session_id
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || char == '-' || char == '_' {
                char
            } else {
                '_'
            }
        })
        .collect()
}
