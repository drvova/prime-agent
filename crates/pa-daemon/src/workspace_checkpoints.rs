//! Workspace checkpoints: the file side of a session-tree rewind.
//!
//! A shadow git store per workspace (`<agent_dir>/checkpoints/<digest>/`: its
//! own git dir, one index per session, the user's git config shut out, so the
//! project's repository is never touched and no hook or filter can start)
//! snapshots the files as trees. Every turn records the workspace before it ran,
//! keyed `before:<first appended entry>`, and after it settled, keyed
//! `at:<new leaf>` (the conflict baseline and the redo target). A restore writes
//! only the differing paths and leaves any path changed outside the session.
//! Every failure degrades to a conversation-only rewind.
//!
//! The kernel's `checkpoint.*` snapshots (the `repl` submodule) share the store: their trees
//! sit in a separate per-session log, so they never mix with the rewind records.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::session_store::SessionEntry;
use crate::worker::SessionCore;

mod repl;
pub(crate) use repl::ReplCheckpoints;

/// A git command past this is killed: the workspace is too large to checkpoint.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// The checkpoint store of the worker's current session, opened lazily and
/// reopened when a fork or switch changes the session.
pub(crate) struct WorkspaceCheckpoints {
    agent_dir: PathBuf,
    /// The session the slot was opened for and its store; `None` after a failed
    /// open or capture, so a failure is reported once, not per turn.
    slot: Mutex<Option<(String, Option<Arc<Store>>)>>,
}

/// A turn's opening checkpoint, closed by [`TurnCheckpoint::settle`].
pub(crate) struct TurnCheckpoint {
    store: Arc<Store>,
    leaf_before: Option<String>,
    tree: String,
}

impl TurnCheckpoint {
    /// Record the opening tree on the turn's first entry and the settled
    /// workspace on the new leaf. Blocking (git).
    pub(crate) fn settle(self, core: &Mutex<SessionCore>) {
        let (first, leaf) = {
            let core = core.lock().unwrap();
            let Some(session) = core.store.as_ref() else {
                return;
            };
            let branch = session.branch();
            let start = match self.leaf_before.as_deref() {
                Some(leaf) => branch
                    .iter()
                    .position(|entry| entry.id == leaf)
                    .map(|at| at + 1),
                None => Some(0),
            };
            let first = start
                .and_then(|at| branch.get(at))
                .map(|entry| entry.id.clone());
            (first, session.leaf_id().map(str::to_string))
        };
        // A turn that appended nothing (aborted before delivery) has no entry to key.
        let (Some(first), Some(leaf)) = (first, leaf) else {
            return;
        };
        let recorded = self
            .store
            .record(&format!("before:{first}"), &self.tree)
            .and_then(|()| {
                let settled = self.store.capture()?;
                self.store.record(&format!("at:{leaf}"), &settled)
            });
        if let Err(error) = recorded {
            eprintln!("pa-daemon: workspace checkpoint skipped: {error:#}");
        }
    }
}

impl WorkspaceCheckpoints {
    pub(crate) fn new(agent_dir: PathBuf) -> Self {
        WorkspaceCheckpoints {
            agent_dir,
            slot: Mutex::new(None),
        }
    }

    /// The store for the core's session, or `None` for an in-memory session,
    /// an RLM child (its writes land in the parent turn's settle capture), or
    /// a workspace that cannot be checkpointed.
    fn store(&self, core: &SessionCore) -> Option<Arc<Store>> {
        let session = core.store.as_ref()?;
        if session.path.as_os_str().is_empty() || core.rlm_depth > 0 {
            return None;
        }
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, opened)) = slot.as_ref().filter(|(id, _)| id == session.session_id()) {
            return opened.clone();
        }
        let opened = Store::open(&self.agent_dir, Path::new(&core.cwd), session.session_id())
            .map_err(|error| {
                eprintln!(
                    "pa-daemon: workspace checkpoints off for {}: {error:#}",
                    core.cwd
                );
            })
            .ok()
            .map(Arc::new);
        *slot = Some((session.session_id().to_string(), opened.clone()));
        opened
    }

    /// Capture the workspace before a turn runs. Blocking (git).
    pub(crate) fn begin_turn(&self, core: &Mutex<SessionCore>) -> Option<TurnCheckpoint> {
        let (store, leaf_before, session_id) = {
            let core = core.lock().unwrap();
            let session = core.store.as_ref()?;
            let leaf = session.leaf_id().map(str::to_string);
            (self.store(&core)?, leaf, session.session_id().to_string())
        };
        match store.capture() {
            Ok(tree) => Some(TurnCheckpoint {
                store,
                leaf_before,
                tree,
            }),
            Err(error) => {
                eprintln!("pa-daemon: workspace checkpoints off for this session: {error:#}");
                *self
                    .slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((session_id, None));
                None
            }
        }
    }

    /// Restore the files for a navigation that already moved the leaf off
    /// `old_leaf`, recording the abandoned state at `old_leaf` so
    /// navigating back restores it. Answers the response's `files` value, or
    /// `None` when the target has no checkpoint. Blocking (git).
    pub(crate) fn rewind_files(
        &self,
        core: &Mutex<SessionCore>,
        target: &SessionEntry,
        old_leaf: Option<&str>,
    ) -> Option<Value> {
        let (store, new_leaf) = {
            let core = core.lock().unwrap();
            let new_leaf = core.store.as_ref()?.leaf_id().map(str::to_string);
            (self.store(&core)?, new_leaf)
        };
        let tree = rewind_tree(&store, target)?;
        let baseline = old_leaf.and_then(|leaf| store.lookup(&format!("at:{leaf}")));
        let result = store.restore(&tree, baseline.as_deref()).and_then(
            |(preimage, restored, conflicts)| {
                if let Some(leaf) = old_leaf {
                    store.record(&format!("at:{leaf}"), &preimage)?;
                }
                if let Some(leaf) = &new_leaf {
                    store.record(&format!("at:{leaf}"), &tree)?;
                }
                Ok(json!({ "restored": restored, "conflicts": conflicts }))
            },
        );
        Some(result.unwrap_or_else(|error| json!({ "error": format!("{error:#}") })))
    }
}

/// The tree a navigation to `target` restores: the workspace before a
/// prompt-like entry (the navigation lands on its parent), or at any other.
fn rewind_tree(store: &Store, target: &SessionEntry) -> Option<String> {
    let prompt =
        crate::session_tree::user_entry_text(target).is_some() || target.type_ == "custom_message";
    store.lookup(&format!(
        "{}:{}",
        if prompt { "before" } else { "at" },
        target.id
    ))
}

/// One session's view of a workspace's shadow store.
struct Store {
    git_dir: PathBuf,
    workspace: PathBuf,
    index: PathBuf,
    empty_config: PathBuf,
    map_path: PathBuf,
    /// The kernel-taken checkpoints of this session (`repl` submodule), one JSON row each.
    repl_path: PathBuf,
    /// The recorded trees by key, newest row winning; the lock also keeps one
    /// walk of the session index at a time.
    map: Mutex<HashMap<String, String>>,
}

impl Store {
    fn open(agent_dir: &Path, workspace: &Path, session_id: &str) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace)?;
        // Never one workspace: the root, a top-level directory (`/tmp`, `/home`,
        // ...), the temp directory, the home directory, or its parent.
        let same = |dir: &Path| std::fs::canonicalize(dir).is_ok_and(|dir| dir == workspace);
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
        let store = Store {
            git_dir: root.join("git"),
            workspace,
            index,
            empty_config: root.join("empty.gitconfig"),
            repl_path: sessions.join(format!("{session}.repl.jsonl")),
            map_path,
            map: Mutex::new(map),
        };
        if !store.git_dir.join("HEAD").exists() {
            std::fs::write(&store.empty_config, "")?;
            store.git(&["init", "--quiet"])?;
            store.git(&["config", "core.autocrlf", "false"])?;
            // Load-bearing: trees are unreferenced objects an auto-gc could
            // prune before the rewind that needs them.
            store.git(&["config", "gc.auto", "0"])?;
        }
        let info = store.git_dir.join("info");
        std::fs::create_dir_all(&info)?;
        // Each path's bytes as on disk: no `.gitattributes` conversion or filter.
        std::fs::write(
            info.join("attributes"),
            "* -text -eol -ident -filter -diff -merge -working-tree-encoding\n",
        )?;
        Ok(store)
    }

    /// Stage the whole workspace into the session index and return its tree.
    fn capture(&self) -> Result<String> {
        let _walk = self.lock();
        self.git(&["add", "--all", "--ignore-errors", "--", "."])?;
        Ok(String::from_utf8_lossy(&self.git(&["write-tree"])?)
            .trim()
            .to_string())
    }

    fn record(&self, key: &str, tree: &str) -> Result<()> {
        let mut map = self.lock();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.map_path)?;
        writeln!(file, "{}", json!([key, tree]))?;
        map.insert(key.to_string(), tree.to_string());
        Ok(())
    }

    fn lookup(&self, key: &str) -> Option<String> {
        self.lock().get(key).cloned()
    }

    /// Put the workspace back to `target`, writing only the differing paths and
    /// leaving those whose content changed since `baseline` (the session's last
    /// recorded state). Answers the preimage, the restored count, and the
    /// paths left alone.
    fn restore(
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
    fn diff(&self, from: &str, to: &str) -> Result<Vec<(String, bool)>> {
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
    use crate::session_store::SessionFile;

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
        let first = Store::open(&agent, &workspace, "first").unwrap();
        first.capture().unwrap();
        std::fs::write(workspace.join("src/edited.rs"), "v2 is longer\n").unwrap();
        std::fs::remove_file(workspace.join("deleted.txt")).unwrap();
        std::fs::write(workspace.join("added.txt"), "new\n").unwrap();

        let second = Store::open(&agent, &workspace, "second").unwrap();
        let seeded = std::fs::read(&second.index).ok();
        let unseeded = Store::open(&root.path().join("other-agent"), &workspace, "only").unwrap();
        assert_eq!(
            (seeded, second.capture().unwrap()),
            (
                std::fs::read(&first.index).ok(),
                unseeded.capture().unwrap()
            )
        );
    }

    /// A file-backed session over a fresh project (itself a git repository)
    /// and one turn that edits, deletes, and adds files; answers the
    /// checkpoints, the core, the workspace, and the turn's prompt/answer ids.
    fn one_turn(
        root: &Path,
    ) -> (
        WorkspaceCheckpoints,
        Mutex<SessionCore>,
        PathBuf,
        String,
        String,
    ) {
        let workspace = root.join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let init = pa_types::session_scope::std_command("git")
            .args(["init", "-q"])
            .current_dir(&workspace)
            .status();
        assert!(init.unwrap().success());
        std::fs::write(workspace.join("main.rs"), "v1\n").unwrap();
        std::fs::write(workspace.join("user.txt"), "v1\n").unwrap();
        let mut session = SessionFile::create(&workspace.to_string_lossy(), None, 0);
        session.set_path(root.join("session.jsonl"));
        let cwd = workspace.to_string_lossy().into_owned();
        let core = Mutex::new(SessionCore::test_core(Some(session), cwd));
        let checkpoints = WorkspaceCheckpoints::new(root.join("agent"));
        let turn = checkpoints.begin_turn(&core).unwrap();
        let (prompt, answer) = {
            let mut core = core.lock().unwrap();
            let store = core.store.as_mut().unwrap();
            let prompt =
                store.append_message(&json!({"role": "user", "content": "edit", "timestamp": 1}));
            std::fs::write(workspace.join("main.rs"), "v2\n").unwrap();
            std::fs::write(workspace.join("user.txt"), "v2\n").unwrap();
            std::fs::create_dir_all(workspace.join("new/dir")).unwrap();
            std::fs::write(workspace.join("new/dir/added.rs"), "new\n").unwrap();
            let answer =
                store.append_message(&json!({"role": "assistant", "content": [], "timestamp": 2}));
            (prompt, answer)
        };
        turn.settle(&core);
        (checkpoints, core, workspace, prompt, answer)
    }

    fn navigate(
        checkpoints: &WorkspaceCheckpoints,
        core: &Mutex<SessionCore>,
        (target, from, to): (&str, Option<&str>, Option<&str>),
    ) -> Option<Value> {
        let entry = core
            .lock()
            .unwrap()
            .store
            .as_ref()
            .unwrap()
            .entry(target)
            .cloned()
            .unwrap();
        core.lock()
            .unwrap()
            .store
            .as_mut()
            .unwrap()
            .branch_to(to)
            .unwrap();
        checkpoints.rewind_files(core, &entry, from)
    }

    /// Rewinding before the prompt restores the files (edits undone, the added
    /// file and its directories gone, the project's own index untouched), and
    /// navigating back (redo), from a reopened store, returns the edits.
    #[test]
    fn a_rewind_restores_the_turn_and_a_redo_returns_it() {
        let root = tempfile::tempdir().unwrap();
        let (checkpoints, core, workspace, prompt, answer) = one_turn(root.path());
        let file = |name: &str| std::fs::read_to_string(workspace.join(name)).ok();

        let undo = navigate(&checkpoints, &core, (&prompt, Some(&answer), None));
        assert_eq!(undo, Some(json!({ "restored": 3, "conflicts": [] })));
        assert_eq!(
            (file("main.rs"), file("user.txt")),
            (Some("v1\n".into()), Some("v1\n".into()))
        );
        assert!(!workspace.join("new").exists());
        assert!(!workspace.join(".git/index").exists());

        let reopened = WorkspaceCheckpoints::new(root.path().join("agent"));
        let redo = navigate(&reopened, &core, (&answer, None, Some(&answer)));
        assert_eq!(redo, Some(json!({ "restored": 3, "conflicts": [] })));
        assert_eq!(file("new/dir/added.rs"), Some("new\n".into()));
    }

    /// A file edited outside the session after the turn settled is left alone.
    #[test]
    fn a_rewind_keeps_files_changed_outside_the_session() {
        let root = tempfile::tempdir().unwrap();
        let (checkpoints, core, workspace, prompt, answer) = one_turn(root.path());
        std::fs::write(workspace.join("user.txt"), "user edit\n").unwrap();

        let undo = navigate(&checkpoints, &core, (&prompt, Some(&answer), None));
        assert_eq!(
            undo,
            Some(json!({ "restored": 2, "conflicts": ["user.txt"] }))
        );
        let file = |name: &str| std::fs::read_to_string(workspace.join(name)).unwrap();
        assert_eq!(
            (file("main.rs"), file("user.txt")),
            ("v1\n".to_string(), "user edit\n".to_string())
        );
    }
}
