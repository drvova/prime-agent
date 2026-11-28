//! Workspace checkpoints: the file side of a session-tree rewind, shared by
//! both daemons.
//!
//! A shadow git store per workspace (`<agent_dir>/checkpoints/<digest>/`: its
//! own git dir, one index per session, the user's git config shut out, so the
//! project's repository is never touched and no hook or filter can start)
//! snapshots the files as trees. Every turn records the workspace before it ran,
//! keyed `before:<first appended entry>`, and after it settled, keyed
//! `at:<new leaf>` (the conflict baseline and the redo target)
//! ([`CheckpointStore::record_turn`]). A rewind ([`CheckpointStore::rewind`])
//! writes only the differing paths and leaves any path changed outside the
//! session. Every failure degrades to a conversation-only rewind.
//!
//! The kernel's `checkpoint.*` snapshots share the store: their trees sit in a
//! separate per-session log ([`CheckpointStore::repl_log`]), so they never mix
//! with the rewind records.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// A git command past this is killed: the workspace is too large to checkpoint.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// The entry a rewind lands on, as its tree is keyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindTarget<'a> {
    /// A prompt-like entry (a user message, a custom message): the
    /// navigation lands on its parent, so the workspace before its turn.
    BeforePrompt(&'a str),
    /// Any other entry: the workspace as its turn settled.
    At(&'a str),
}

impl CheckpointStore {
    /// Records a turn: `opening`, the tree captured before it ran, under its
    /// first appended entry `first`, and the workspace as it settled under
    /// its new leaf `leaf`. Blocking (git).
    ///
    /// # Errors
    ///
    /// The capture or a record failed.
    pub fn record_turn(&self, opening: &str, first: &str, leaf: &str) -> Result<()> {
        self.record(&format!("before:{first}"), opening)?;
        let settled = self.capture()?;
        self.record(&format!("at:{leaf}"), &settled)
    }

    /// Restores the files for a navigation that already moved the leaf from
    /// `old_leaf` to `new_leaf` onto `target`, recording the abandoned state
    /// at `old_leaf` so navigating back restores it. Answers the navigation
    /// reply's `files` (`{restored, conflicts}`, or `{error}` when the
    /// restore failed), or `None` when the target has no checkpoint.
    /// Blocking (git).
    #[must_use]
    pub fn rewind(
        &self,
        target: RewindTarget<'_>,
        old_leaf: Option<&str>,
        new_leaf: Option<&str>,
    ) -> Option<Value> {
        let tree = self.lookup(&match target {
            RewindTarget::BeforePrompt(id) => format!("before:{id}"),
            RewindTarget::At(id) => format!("at:{id}"),
        })?;
        let baseline = old_leaf.and_then(|leaf| self.lookup(&format!("at:{leaf}")));
        let result =
            self.restore(&tree, baseline.as_deref())
                .and_then(|(preimage, restored, conflicts)| {
                    if let Some(leaf) = old_leaf {
                        self.record(&format!("at:{leaf}"), &preimage)?;
                    }
                    if let Some(leaf) = new_leaf {
                        self.record(&format!("at:{leaf}"), &tree)?;
                    }
                    Ok(json!({ "restored": restored, "conflicts": conflicts }))
                });
        Some(result.unwrap_or_else(|error| json!({ "error": format!("{error:#}") })))
    }

    /// The session's kernel-checkpoint log, one JSON row each.
    #[must_use]
    pub fn repl_log(&self) -> &Path {
        &self.repl_path
    }
}

/// One session's view of a workspace's shadow store (see the module
/// documentation).
pub struct CheckpointStore {
    git_dir: PathBuf,
    workspace: PathBuf,
    index: PathBuf,
    empty_config: PathBuf,
    map_path: PathBuf,
    /// The kernel-taken checkpoints of this session, one JSON row each.
    repl_path: PathBuf,
    /// The recorded trees by key, newest row winning; the lock also keeps one
    /// walk of the session index at a time.
    map: Mutex<HashMap<String, String>>,
}

impl CheckpointStore {
    /// The store of session `session_id` (its own index, record log and
    /// kernel-checkpoint log) over `workspace`'s shadow repository under
    /// `agent_dir`, made on first use.
    ///
    /// # Errors
    ///
    /// The workspace is no project workspace (the root, a top-level, temp or
    /// home directory), or the store cannot be made.
    pub fn open(agent_dir: &Path, workspace: &Path, session_id: &str) -> Result<Self> {
        let workspace = pa_types::platform::canonicalize(workspace)?;
        // Never one workspace: the root, a top-level directory (`/tmp`, `/home`,
        // ...), the temp directory, the home directory, or its parent.
        let same =
            |dir: &Path| pa_types::platform::canonicalize(dir).is_ok_and(|dir| dir == workspace);
        let home = pa_types::session_scope::var_os("HOME")
            .or_else(|| pa_types::session_scope::var_os("USERPROFILE"))
            .map(PathBuf::from);
        if workspace.components().count() <= 2
            || same(&std::env::temp_dir())
            || home
                .as_deref()
                .is_some_and(|home| same(home) || home.parent().is_some_and(same))
        {
            bail!("{} is not a project workspace", workspace.display());
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
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let index = sessions.join(format!("{session}.index"));
        // A session's first capture into an empty index hashes every file of
        // the workspace, and the turn waits on it. Start from the workspace's
        // newest index instead: `git add --all` then re-reads only the paths
        // whose stat moved. The copy keeps the source's mtime, read before the
        // bytes, so git's racy-entry check is at least as strict as it is for
        // the session that wrote the index. A failed seed leaves no index, the
        // behavior without one.
        if !index.exists() {
            let newest = std::fs::read_dir(&sessions)?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "index")
                })
                .filter_map(|path| {
                    let modified = std::fs::metadata(&path).and_then(|meta| meta.modified());
                    modified.ok().map(|modified| (modified, path))
                })
                .max();
            if let Some((modified, source)) = newest {
                let temp = sessions.join(format!("{session}.index-seed.tmp"));
                let seeded = std::fs::copy(&source, &temp)
                    .and_then(|_| std::fs::File::options().write(true).open(&temp))
                    .and_then(|file| file.set_modified(modified))
                    .and_then(|()| std::fs::rename(&temp, &index));
                if seeded.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
            }
        }
        let store = CheckpointStore {
            git_dir: root.join("git"),
            workspace,
            index,
            empty_config: root.join("empty.gitconfig"),
            repl_path: sessions.join(format!("{session}.repl.jsonl")),
            map_path,
            map: Mutex::new(map),
        };
        // Replaced whole, so a git that a concurrent capture runs never reads
        // half a config (a torn attributes file would turn conversions on).
        let config = pa_types::platform::ReplaceOptions {
            durability: pa_types::platform::Durability::Unsynced,
            mode: pa_types::platform::FileMode::KeepTarget,
            symlinks: pa_types::platform::Symlinks::ReplaceLink,
        };
        if !store.git_dir.join("HEAD").exists() {
            pa_types::platform::replace_file(&store.empty_config, b"", config)?;
            store.git(&["init", "--quiet"])?;
            store.git(&["config", "core.autocrlf", "false"])?;
            // Load-bearing: trees are unreferenced objects an auto-gc could
            // prune before the rewind that needs them.
            store.git(&["config", "gc.auto", "0"])?;
        }
        let info = store.git_dir.join("info");
        std::fs::create_dir_all(&info)?;
        // Each path's bytes as on disk: no `.gitattributes` conversion or filter.
        pa_types::platform::replace_file(
            &info.join("attributes"),
            b"* -text -eol -ident -filter -diff -merge -working-tree-encoding\n",
            config,
        )?;
        Ok(store)
    }

    /// Stage the whole workspace into the session index and return its tree.
    /// Blocking (git).
    ///
    /// # Errors
    ///
    /// A git command failed or ran past its timeout.
    pub fn capture(&self) -> Result<String> {
        let _walk = self.lock();
        self.git(&["add", "--all", "--ignore-errors", "--", "."])?;
        Ok(String::from_utf8_lossy(&self.git(&["write-tree"])?)
            .trim()
            .to_string())
    }

    /// Records `tree` under `key` (`before:<entry>` or `at:<entry>`).
    ///
    /// # Errors
    ///
    /// The record log could not be written.
    pub fn record(&self, key: &str, tree: &str) -> Result<()> {
        let mut map = self.lock();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.map_path)?;
        writeln!(file, "{}", json!([key, tree]))?;
        map.insert(key.to_string(), tree.to_string());
        Ok(())
    }

    /// The tree recorded under `key`, the newest record winning.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<String> {
        self.lock().get(key).cloned()
    }

    /// Put the workspace back to `target`, writing only the differing paths and
    /// leaving those whose content changed since `baseline` (the session's last
    /// recorded state). Answers the preimage, the restored count, and the
    /// paths left alone. Blocking (git).
    ///
    /// # Errors
    ///
    /// A git command or a file removal failed.
    pub fn restore(
        &self,
        target: &str,
        baseline: Option<&str>,
    ) -> Result<(String, usize, Vec<String>)> {
        let preimage = self.capture()?;
        let foreign: BTreeSet<String> = match baseline.filter(|baseline| *baseline != preimage) {
            Some(baseline) => self
                .diff(baseline, &preimage)?
                .into_iter()
                .map(|(path, _)| path)
                .collect(),
            None => BTreeSet::new(),
        };
        let (conflicts, changes): (Vec<_>, Vec<_>) = self
            .diff(&preimage, target)?
            .into_iter()
            .partition(|(path, _)| foreign.contains(path));
        let mut writes = Vec::new();
        for (path, in_target) in &changes {
            if *in_target {
                writes.push(path.as_str());
                continue;
            }
            let path = self.workspace.join(path);
            match std::fs::remove_file(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                result => result?,
            }
            // Drop the directories the deletion emptied; a non-empty one ends the walk.
            for dir in path
                .ancestors()
                .skip(1)
                .take_while(|dir| *dir != self.workspace)
            {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
            }
        }
        let source = format!("--source={target}");
        for chunk in writes.chunks(256) {
            self.git(&[&["restore", source.as_str(), "--worktree", "--"], chunk].concat())?;
        }
        Ok((
            preimage,
            changes.len(),
            conflicts.into_iter().map(|(path, _)| path).collect(),
        ))
    }

    /// The differing paths from `from` to `to`, each with whether `to` has it.
    ///
    /// # Errors
    ///
    /// The git command failed.
    pub fn diff(&self, from: &str, to: &str) -> Result<Vec<(String, bool)>> {
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
        let (pairs, _) = fields.as_chunks::<2>();
        Ok(pairs
            .iter()
            .map(|pair| {
                (
                    String::from_utf8_lossy(pair[1]).into_owned(),
                    pair[0] != b"D",
                )
            })
            .collect())
    }

    /// Run `git <args>` against the shadow store; stdout on success.
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut child = pa_types::session_scope::std_command("git")
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
            .spawn()?;
        // Drained on its own thread so a full pipe never stalls the child.
        let mut pipe = child.stdout.take().expect("stdout is piped");
        let stdout = std::thread::spawn(pa_types::session_scope::propagate(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        }));
        let deadline = Instant::now() + GIT_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                // A kill failure means it already exited; the wait reaps either way.
                let _ = child.kill();
                let _ = child.wait();
                bail!("workspace is too large to checkpoint");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = stdout.join().unwrap_or_else(|_| Ok(Vec::new()))?;
        if !status.success() {
            bail!("git {} failed ({status})", args[0]);
        }
        Ok(stdout)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new session's first capture starts from the workspace's last index
    /// (so it re-reads only what changed) and still records the workspace as
    /// it is: the tree equals the one an unseeded store captures.
    #[test]
    fn a_new_session_seeds_its_index_from_the_workspace_and_captures_the_same_tree() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/kept.rs"), "kept\n").unwrap();
        std::fs::write(workspace.join("src/edited.rs"), "v1\n").unwrap();
        std::fs::write(workspace.join("deleted.txt"), "gone\n").unwrap();
        let agent = root.path().join("agent");
        let first = CheckpointStore::open(&agent, &workspace, "first").unwrap();
        first.capture().unwrap();
        std::fs::write(workspace.join("src/edited.rs"), "v2 is longer\n").unwrap();
        std::fs::remove_file(workspace.join("deleted.txt")).unwrap();
        std::fs::write(workspace.join("added.txt"), "new\n").unwrap();

        let second = CheckpointStore::open(&agent, &workspace, "second").unwrap();
        let seeded = std::fs::read(&second.index).ok();
        let unseeded =
            CheckpointStore::open(&root.path().join("other-agent"), &workspace, "only").unwrap();
        assert_eq!(
            (seeded, second.capture().unwrap()),
            (
                std::fs::read(&first.index).ok(),
                unseeded.capture().unwrap()
            )
        );
    }

    /// A rewind before a turn's prompt undoes the turn's files but leaves
    /// (and names) the paths changed outside the session, and the rewind back
    /// to the leaf it left returns the workspace as it was abandoned.
    #[test]
    fn a_rewind_restores_a_turn_and_a_redo_returns_it() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("main.rs"), "v1\n").unwrap();
        std::fs::write(workspace.join("user.txt"), "v1\n").unwrap();
        let store = CheckpointStore::open(&root.path().join("agent"), &workspace, "s").unwrap();
        let opening = store.capture().unwrap();
        std::fs::write(workspace.join("main.rs"), "v2\n").unwrap();
        std::fs::write(workspace.join("added.rs"), "new\n").unwrap();
        store.record_turn(&opening, "u1", "a1").unwrap();
        std::fs::write(workspace.join("user.txt"), "changed outside\n").unwrap();
        std::fs::write(workspace.join("main.rs"), "outside too\n").unwrap();

        let rewound = store.rewind(RewindTarget::BeforePrompt("u1"), Some("a1"), None);
        let files = |name: &str| std::fs::read_to_string(workspace.join(name)).ok();
        let after_rewind = (files("main.rs"), files("added.rs"), files("user.txt"));
        let redone = store.rewind(RewindTarget::At("a1"), None, Some("a1"));
        assert_eq!(
            (
                rewound,
                after_rewind,
                redone,
                files("main.rs"),
                files("added.rs")
            ),
            (
                Some(json!({ "restored": 1, "conflicts": ["main.rs", "user.txt"] })),
                (
                    Some("outside too\n".to_string()),
                    None,
                    Some("changed outside\n".to_string())
                ),
                Some(json!({ "restored": 1, "conflicts": [] })),
                Some("outside too\n".to_string()),
                Some("new\n".to_string()),
            )
        );
    }
}
