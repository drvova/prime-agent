//! Isolated git worktrees for subagents: `worktree-create`, `worktree-list`,
//! `worktree-diff`, `worktree-merge`, and `worktree-remove`.
//!
//! A worktree is a private checkout of the session's repository under the
//! agent dir (`worktrees/<hash of the repository path>/<session id>/<name>`),
//! with a record beside it (`<name>.json`) holding the commit it was made
//! from and the tree it started with. The layout and the records are the
//! ones the Python kernel's `worktree` module wrote, so either kernel finds
//! the other's worktrees.
//!
//! Each operation reports its outcome to the session's `worktree.record`
//! handler (adoption counts, and the ledger behind the session-end notice of
//! unremoved worktrees). Every git command ends with the signal it is given:
//! the calling cell's, so an aborted cell is not held by a slow command.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;
use tokio::io::AsyncWriteExt;

use crate::kernel::shared::HostRequestHandlers;
use crate::tools::tool_definition::AbortSignal;

/// One worktree: its checkout `path`, the parent `repo`, the `base` commit
/// it was made from, and whether the checkout still `exists`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Tree {
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) repo: PathBuf,
    pub(super) base: String,
    pub(super) exists: bool,
}

/// A worktree's record on disk, as both kernels write it.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    name: String,
    path: PathBuf,
    repo: PathBuf,
    base: String,
    /// The tree the worktree started with: its base plus the carried changes.
    baseline: String,
    /// The tree a merge last brought home; removing the worktree at it
    /// loses nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    merged: Option<String>,
}

/// A finished git command.
struct GitOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Whether a failing git command is an error or an outcome the caller reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    Raises,
    Returned,
}

/// The directory name a repository's worktrees are kept under: the first 16
/// hex digits of its path's SHA-256, as the Python kernel named it.
fn repo_key(repo: &Path) -> String {
    let digest = Sha256::digest(repo.to_string_lossy().as_bytes());
    format!("{digest:x}")[..16].to_string()
}

/// The paths a patch touches, in the patch's order.
pub(super) fn patch_files(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| line.strip_prefix("diff --git a/"))
        .filter_map(|rest| rest.find(" b/").map(|end| rest[..end].to_string()))
        .collect()
}

/// A session's worktrees of the repository its working directory is in.
pub(super) struct Worktrees {
    handlers: HostRequestHandlers,
    cwd: PathBuf,
    agent_dir: Option<PathBuf>,
    /// The session's durable id, which keeps its worktrees apart from other
    /// sessions' of the same repository.
    session_key: String,
}

impl Worktrees {
    pub(super) fn new(
        handlers: HostRequestHandlers,
        session: &crate::kernel::scheme::SessionEnvironment,
    ) -> Self {
        Worktrees {
            handlers,
            cwd: session.cwd.clone(),
            agent_dir: session.agent_dir.clone(),
            session_key: session
                .artifact_dir
                .as_deref()
                .and_then(Path::file_name)
                .map_or_else(
                    || "no-session".to_string(),
                    |id| id.to_string_lossy().into_owned(),
                ),
        }
    }

    /// Run `git args` in `cwd`, feeding it `stdin` and the extra `env`.
    async fn git(
        stop: &AbortSignal,
        cwd: &Path,
        args: &[&str],
        stdin: Option<&[u8]>,
        env: Option<(&str, &Path)>,
        failure: Failure,
    ) -> anyhow::Result<GitOutput> {
        let shown = format!("git {}", args.join(" "));
        let mut command = pa_types::session_scope::tokio_command("git");
        command
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A command dropped by the stop below is killed, not left behind.
            .kill_on_drop(true);
        if let Some((name, value)) = env {
            command.env(name, value);
        }
        #[cfg(not(unix))]
        crate::platform::set_no_window(command.as_std_mut());
        let mut child = command
            .spawn()
            .with_context(|| format!("could not start {shown}"))?;
        let input = child.stdin.take();
        let run = async {
            if let (Some(mut pipe), Some(bytes)) = (input, stdin) {
                // A command that exits without reading all of it closes the
                // pipe: its exit status tells the caller why.
                let _ = pipe.write_all(bytes).await;
            }
            child.wait_with_output().await
        };
        let output = tokio::select! {
            biased;
            () = stop.cancelled() => bail!("{shown} was stopped with its cell"),
            output = run => output.with_context(|| format!("could not run {shown}"))?,
        };
        if failure == Failure::Raises && !output.status.success() {
            bail!(
                "{shown} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(GitOutput {
            code: crate::platform::normalized_exit_code(&output.status),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    /// The trimmed output of a git command that must succeed.
    async fn text(stop: &AbortSignal, cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
        let output = Self::git(stop, cwd, args, None, None, Failure::Raises).await?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// The repository the session works in.
    async fn repo(&self, stop: &AbortSignal) -> anyhow::Result<PathBuf> {
        let top = Self::text(stop, &self.cwd, &["rev-parse", "--show-toplevel"])
            .await
            .map_err(|error| {
                anyhow!("worktree needs a git repository at the session cwd: {error:#}")
            })?;
        pa_types::platform::canonicalize(&top).with_context(|| format!("could not resolve {top}"))
    }

    /// Where this session's worktrees of `repo` and their records live.
    fn root(&self, repo: &Path) -> anyhow::Result<PathBuf> {
        let agent_dir = self.agent_dir.as_deref().ok_or_else(|| {
            anyhow!("this kernel was started without the session's agent dir, where worktrees live")
        })?;
        let key = repo_key(repo);
        Ok(agent_dir
            .join("worktrees")
            .join(key)
            .join(&self.session_key))
    }

    fn load(record: &Path) -> anyhow::Result<(Tree, Record)> {
        let raw = std::fs::read_to_string(record)
            .with_context(|| format!("could not read {}", record.display()))?;
        let record: Record = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not a worktree record", record.display()))?;
        let tree = Tree {
            name: record.name.clone(),
            path: record.path.clone(),
            repo: record.repo.clone(),
            base: record.base.clone(),
            exists: record.path.is_dir(),
        };
        Ok((tree, record))
    }

    /// The worktree `target` names: its name, or a worktree carrying one.
    async fn resolve(
        &self,
        stop: &AbortSignal,
        target: &Value,
    ) -> anyhow::Result<(Tree, Record, PathBuf)> {
        let name = target
            .as_str()
            .or_else(|| target.get("name").and_then(Value::as_str))
            .filter(|name| valid_name(name))
            .ok_or_else(|| anyhow!("unknown worktree: {target}"))?;
        let record = self
            .root(&self.repo(stop).await?)?
            .join(format!("{name}.json"));
        if !record.is_file() {
            bail!("no worktree named '{name}' in this session");
        }
        let (tree, loaded) = Self::load(&record)?;
        Ok((tree, loaded, record))
    }

    /// The checkout's full content (tracked and untracked, ignores honored)
    /// as a tree object, written through a throwaway index so the checkout's
    /// own index never changes.
    async fn snapshot(stop: &AbortSignal, path: &Path) -> anyhow::Result<String> {
        let scratch = std::env::temp_dir().join(format!(
            "prime-agent-worktree-index-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&scratch).context("could not make a scratch index")?;
        let index = scratch.join("index");
        let env = Some(("GIT_INDEX_FILE", index.as_path()));
        let written = async {
            Self::git(
                stop,
                path,
                &["read-tree", "HEAD"],
                None,
                env,
                Failure::Raises,
            )
            .await?;
            Self::git(stop, path, &["add", "-A", "."], None, env, Failure::Raises).await?;
            Self::git(stop, path, &["write-tree"], None, env, Failure::Raises).await
        }
        .await;
        std::fs::remove_dir_all(&scratch).context("could not remove the scratch index")?;
        Ok(String::from_utf8_lossy(&written?.stdout).trim().to_string())
    }

    /// Report one operation's outcome to the host. A session without the
    /// handler counts nothing, and a count that fails never fails the
    /// operation it counts.
    async fn record(&self, op: &str, outcome: &str, tree: Option<&Tree>) {
        let Some(handler) = self.handlers.get("worktree.record") else {
            return;
        };
        let mut data = json!({ "op": op, "outcome": outcome });
        if let Some(tree) = tree {
            data["name"] = json!(tree.name);
            data["path"] = json!(tree.path);
        }
        if let Err(error) = handler(crate::kernel::shared::HostRequestPayload { data }).await {
            tracing::warn!("worktree.record ({op}, {outcome}) failed: {error:#}");
        }
    }

    /// Check `base` out into a new worktree `name`; `carry_changes` copies
    /// the parent's uncommitted changes (staged, unstaged, and untracked
    /// files) into it.
    pub(super) async fn create(
        &self,
        stop: &AbortSignal,
        name: &str,
        base: &str,
        carry_changes: bool,
    ) -> anyhow::Result<Tree> {
        if !valid_name(name) {
            bail!("name must be 1-64 letters, digits, '.', '_' or '-', starting alphanumeric");
        }
        let repo = self.repo(stop).await?;
        let root = self.root(&repo)?;
        let path = root.join(name);
        let record = root.join(format!("{name}.json"));
        if record.exists() || path.exists() {
            bail!("a worktree named '{name}' already exists in this session");
        }
        let base_commit = Self::text(
            stop,
            &repo,
            &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
        )
        .await?;
        std::fs::create_dir_all(&root)
            .with_context(|| format!("could not create {}", root.display()))?;
        let checkout = path.to_string_lossy().into_owned();
        Self::git(
            stop,
            &repo,
            &["worktree", "add", "--detach", &checkout, &base_commit],
            None,
            None,
            Failure::Raises,
        )
        .await?;
        let filled = async {
            if carry_changes {
                let carried = Self::git(
                    stop,
                    &repo,
                    &["diff", "--binary", "HEAD"],
                    None,
                    None,
                    Failure::Raises,
                )
                .await?
                .stdout;
                if !carried.is_empty() {
                    Self::git(
                        stop,
                        &path,
                        &["apply", "--binary"],
                        Some(&carried),
                        None,
                        Failure::Raises,
                    )
                    .await?;
                }
                let untracked = Self::git(
                    stop,
                    &repo,
                    &["ls-files", "--others", "--exclude-standard", "-z"],
                    None,
                    None,
                    Failure::Raises,
                )
                .await?
                .stdout;
                for relative in String::from_utf8_lossy(&untracked)
                    .split('\0')
                    .filter(|relative| !relative.is_empty())
                {
                    let target = path.join(relative);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(repo.join(relative), &target)
                        .with_context(|| format!("could not carry {relative}"))?;
                }
            }
            Self::snapshot(stop, &path).await
        }
        .await;
        let baseline = match filled {
            Ok(baseline) => baseline,
            Err(error) => {
                // The half-made checkout goes even when the cell was stopped.
                let _ = Self::git(
                    &AbortSignal::new(),
                    &repo,
                    &["worktree", "remove", "--force", &checkout],
                    None,
                    None,
                    Failure::Returned,
                )
                .await;
                self.record("create", "failed", None).await;
                return Err(error);
            }
        };
        let tree = Tree {
            name: name.to_string(),
            path: path.clone(),
            repo: repo.clone(),
            base: base_commit.clone(),
            exists: true,
        };
        pa_types::platform::replace_file(
            &record,
            serde_json::to_string(&Record {
                name: name.to_string(),
                path,
                repo,
                base: base_commit,
                baseline,
                merged: None,
            })?
            .as_bytes(),
            crate::platform::REPLACE_KEEPING_MODE,
        )
        .with_context(|| format!("could not write {}", record.display()))?;
        self.record("create", "ok", Some(&tree)).await;
        Ok(tree)
    }

    /// This session's worktrees of the current repository, by name.
    async fn list(&self, stop: &AbortSignal) -> anyhow::Result<Vec<Tree>> {
        let root = self.root(&self.repo(stop).await?)?;
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut records: Vec<PathBuf> = std::fs::read_dir(&root)
            .with_context(|| format!("could not list {}", root.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        records.sort();
        records
            .iter()
            .map(|record| Self::load(record).map(|(tree, _)| tree))
            .collect()
    }

    /// The worktree's changes since it was made (the carried changes
    /// excluded), as a binary-safe patch, with the tree they end at.
    async fn delta(
        stop: &AbortSignal,
        tree: &Tree,
        baseline: &str,
    ) -> anyhow::Result<(Vec<u8>, String)> {
        if !tree.exists {
            bail!(
                "worktree '{}' no longer exists at {}",
                tree.name,
                tree.path.display()
            );
        }
        let current = Self::snapshot(stop, &tree.path).await?;
        let patch = Self::git(
            stop,
            &tree.path,
            &["diff", "--binary", baseline, &current],
            None,
            None,
            Failure::Raises,
        )
        .await?
        .stdout;
        Ok((patch, current))
    }

    /// The worktree's changes since it was made, as a patch.
    pub(super) async fn diff(&self, stop: &AbortSignal, target: &Value) -> anyhow::Result<String> {
        let (tree, record, _) = self.resolve(stop, target).await?;
        let (patch, _) = Self::delta(stop, &tree, &record.baseline).await?;
        self.record("diff", "ok", None).await;
        Ok(String::from_utf8_lossy(&patch).into_owned())
    }

    /// Bring the worktree's delta home: `patch` applies it to the parent's
    /// checkout once a `git apply` check passes, as is or three-way (a
    /// conflict is listed and nothing is applied); `branch` commits it to
    /// `prime-agent/worktree/<name>` without touching the checkout.
    async fn merge(
        &self,
        stop: &AbortSignal,
        target: &Value,
        strategy: &str,
        dry_run: bool,
    ) -> anyhow::Result<Value> {
        if !matches!(strategy, "patch" | "branch") {
            bail!("strategy must be \"patch\" or \"branch\"");
        }
        let (tree, mut record, record_path) = self.resolve(stop, target).await?;
        let (patch, current) = Self::delta(stop, &tree, &record.baseline).await?;
        let files = patch_files(&String::from_utf8_lossy(&patch));
        let result = |status: &str, conflicts: Vec<String>, branch: Option<&str>| {
            json!({
                "status": status,
                "files": files,
                "conflicts": conflicts,
                // Absent values are #f in Scheme, never null.
                "branch": branch.map_or(json!(false), Value::from),
                "ok": status != "conflicts",
            })
        };
        if patch.is_empty() {
            self.record("merge", "empty", None).await;
            return Ok(result("empty", Vec::new(), None));
        }
        if strategy == "branch" {
            let branch = format!("prime-agent/worktree/{}", tree.name);
            if dry_run {
                return Ok(result("clean", Vec::new(), Some(&branch)));
            }
            // The commits carry the repository's own configured identity.
            let start = Self::text(
                stop,
                &tree.path,
                &[
                    "commit-tree",
                    &record.baseline,
                    "-p",
                    &tree.base,
                    "-m",
                    &format!("worktree {}: baseline", tree.name),
                ],
            )
            .await?;
            let commit = Self::text(
                stop,
                &tree.path,
                &[
                    "commit-tree",
                    &current,
                    "-p",
                    &start,
                    "-m",
                    &format!("worktree {}: changes", tree.name),
                ],
            )
            .await?;
            Self::text(stop, &tree.repo, &["branch", "--force", &branch, &commit]).await?;
            record.merged = Some(current);
            pa_types::platform::replace_file(
                &record_path,
                serde_json::to_string(&record)?.as_bytes(),
                crate::platform::REPLACE_KEEPING_MODE,
            )?;
            self.record("merge", "branch", None).await;
            return Ok(result("branch", Vec::new(), Some(&branch)));
        }
        // A patch that applies as is lands in the working tree only, whatever
        // the index holds. One that does not is tried three-way, which git
        // stages; some git versions exit 0 from that check while saying the
        // patch would apply "with conflicts", so both are read.
        let plain = Self::git(
            stop,
            &tree.repo,
            &["apply", "--check"],
            Some(&patch),
            None,
            Failure::Returned,
        )
        .await?;
        let apply: &[&str] = if plain.code == Some(0) {
            &["apply"]
        } else {
            let check = Self::git(
                stop,
                &tree.repo,
                &["apply", "--3way", "--check"],
                Some(&patch),
                None,
                Failure::Returned,
            )
            .await?;
            let said = String::from_utf8_lossy(&check.stderr);
            if check.code != Some(0) || said.contains("with conflicts") {
                /// A refused path's line: the text before the path and the
                /// text after it (`None`: a colon and a line number follow).
                const SHAPES: [(&str, Option<&str>); 8] = [
                    ("error: patch failed: ", None),
                    ("Applied patch to '", Some("' with conflicts")),
                    ("error: ", Some(": does not match index")),
                    ("error: ", Some(": does not exist in index")),
                    ("error: ", Some(": already exists in working directory")),
                    ("error: ", Some(": patch does not apply")),
                    ("conflict in ", Some("")),
                    ("conflicts in ", Some("")),
                ];
                let mut refused: Vec<String> = Vec::new();
                for (before, after) in SHAPES {
                    for line in said.lines() {
                        let Some(rest) = line.find(before).map(|at| &line[at + before.len()..])
                        else {
                            continue;
                        };
                        let named = match after {
                            None => rest
                                .char_indices()
                                .find(|(at, letter)| {
                                    *letter == ':'
                                        && rest[at + 1..]
                                            .starts_with(|next: char| next.is_ascii_digit())
                                })
                                .map(|(at, _)| &rest[..at]),
                            Some("") => Some(rest),
                            Some(after) => rest.find(after).map(|at| &rest[..at]),
                        };
                        if let Some(named) = named.map(str::trim).filter(|named| !named.is_empty())
                        {
                            if !refused.iter().any(|known| known == named) {
                                refused.push(named.to_string());
                            }
                        }
                    }
                }
                if refused.is_empty() {
                    refused.push("(git apply refused the patch)".to_string());
                }
                self.record("merge", "conflicts", None).await;
                return Ok(result("conflicts", refused, None));
            }
            &["apply", "--3way"]
        };
        if dry_run {
            return Ok(result("clean", Vec::new(), None));
        }
        Self::git(stop, &tree.repo, apply, Some(&patch), None, Failure::Raises).await?;
        record.merged = Some(current);
        pa_types::platform::replace_file(
            &record_path,
            serde_json::to_string(&record)?.as_bytes(),
            crate::platform::REPLACE_KEEPING_MODE,
        )?;
        self.record("merge", "applied", None).await;
        Ok(result("applied", Vec::new(), None))
    }

    /// Delete the worktree. Unmerged changes refuse the removal unless
    /// `force`.
    pub(super) async fn remove(
        &self,
        stop: &AbortSignal,
        target: &Value,
        force: bool,
    ) -> anyhow::Result<()> {
        let (tree, record, record_path) = self.resolve(stop, target).await?;
        if tree.exists && !force {
            let (patch, current) = Self::delta(stop, &tree, &record.baseline).await?;
            if !patch.is_empty() && record.merged.as_deref() != Some(current.as_str()) {
                bail!(
                    "worktree '{}' has changes that were not merged; merge it first or pass #:force #t",
                    tree.name
                );
            }
        }
        if tree.exists {
            Self::git(
                stop,
                &tree.repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &tree.path.to_string_lossy(),
                ],
                None,
                None,
                Failure::Raises,
            )
            .await?;
        }
        Self::git(
            stop,
            &tree.repo,
            &["worktree", "prune"],
            None,
            None,
            Failure::Returned,
        )
        .await?;
        match std::fs::remove_file(&record_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not remove {}", record_path.display()))
            }
        }
        self.record("remove", "ok", Some(&tree)).await;
        Ok(())
    }
}

/// 1 to 64 letters, digits, `.`, `_` or `-`, starting with a letter or digit.
fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && name.starts_with(|first: char| first.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|letter| letter.is_ascii_alphanumeric() || matches!(letter, '.' | '_' | '-'))
}

/// Define `%worktree-call` in a session's engine: `(operation payload-json)`
/// to the reply JSON its Scheme wrapper decodes. Must be called from within
/// a tokio runtime.
pub(super) fn register(engine: &mut Engine, worktrees: Arc<Worktrees>) {
    let runtime = tokio::runtime::Handle::current();
    engine.register_fn(
        "%worktree-call",
        move |operation: String, payload: String| -> String {
            // Outside a cell there is nothing to stop the call.
            let stop = crate::kernel::scheme::cell::current()
                .map_or_else(AbortSignal::new, |cell| cell.signal());
            let outcome = serde_json::from_str::<Value>(&payload)
                .map_err(|error| anyhow!("the payload is not JSON: {error}"))
                .and_then(|options| {
                    let flag =
                        |name: &str| options.get(name).and_then(Value::as_bool) == Some(true);
                    let target = &options["target"];
                    runtime.block_on(async {
                        match operation.as_str() {
                            "worktree-create" => {
                                let tree = worktrees
                                    .create(
                                        &stop,
                                        options["name"].as_str().unwrap_or_default(),
                                        options["base"].as_str().unwrap_or("HEAD"),
                                        flag("carry_changes"),
                                    )
                                    .await?;
                                Ok(serde_json::to_value(tree)?)
                            }
                            "worktree-list" => {
                                Ok(serde_json::to_value(worktrees.list(&stop).await?)?)
                            }
                            "worktree-diff" => Ok(json!(worktrees.diff(&stop, target).await?)),
                            "worktree-merge" => {
                                worktrees
                                    .merge(
                                        &stop,
                                        target,
                                        options["strategy"].as_str().unwrap_or_default(),
                                        flag("dry_run"),
                                    )
                                    .await
                            }
                            "worktree-remove" => {
                                worktrees.remove(&stop, target, flag("force")).await?;
                                Ok(json!(true))
                            }
                            unknown => Err(anyhow!("unknown worktree operation: {unknown}")),
                        }
                    })
                });
            super::reply_json(&operation, outcome)
        },
    );
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
    use crate::kernel::shared::{host_handler, HostRequestPayload};
    use crate::tools::kernel_cell::{KernelExecuteOptions, KernelExecutor, KernelProvisioner};

    /// The value of `expression` in `kernel` as JSON, or the error it raised.
    pub(in crate::kernel::scheme::native) async fn evaluate(
        kernel: &dyn KernelExecutor,
        expression: &str,
    ) -> Result<Value, String> {
        let options = KernelExecuteOptions {
            signal: None,
            timeout_ms: None,
            on_stream: None,
            on_late_sent_agent_message: None,
            tool_call_id: None,
        };
        let cell = format!("(display (value->jsexpr-string {expression}))");
        let result = kernel.execute(&cell, options).await.unwrap();
        match result.error {
            None => Ok(serde_json::from_str(&result.stdout).unwrap()),
            Some(error) => Err(error.evalue),
        }
    }

    /// Run git in `cwd` and return what it printed.
    pub(in crate::kernel::scheme::native) fn git(cwd: &Path, args: &[&str]) -> String {
        let output = pa_types::session_scope::std_command("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A throwaway repository holding one committed file, `notes.txt`.
    pub(in crate::kernel::scheme::native) fn repository(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch", "main"]);
        git(&repo, &["config", "user.name", "Worktree Test"]);
        git(&repo, &["config", "user.email", "worktree@test.invalid"]);
        // Files keep the bytes the test writes, whatever line-ending
        // conversion the machine's git config asks for (Windows git
        // installs with core.autocrlf=true).
        git(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "start"]);
        repo
    }

    /// A kernel working in `repo` for the saved session `s1`, and the
    /// `worktree.record` payloads its host received.
    async fn kernel_in(
        root: &Path,
        repo: &Path,
    ) -> (Box<dyn KernelExecutor>, Arc<Mutex<Vec<Value>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        let heard = Arc::clone(&recorded);
        handlers.register(
            "worktree.record",
            host_handler(move |payload: HostRequestPayload| {
                heard.lock().unwrap().push(payload.data);
                async move { Ok(json!({})) }
            }),
        );
        let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
            host_handlers: handlers,
            cwd: repo.to_path_buf(),
            agent_dir: Some(root.join("agent")),
            artifact_dir: Some(root.join("sessions").join("s1")),
            ..SchemeKernelOptions::default()
        })
        .ensure(None, None)
        .await
        .unwrap();
        (kernel, recorded)
    }

    /// A worktree starts with the parent's uncommitted work, its diff holds
    /// only what changed in it afterwards, and a merge brings exactly that
    /// home, after which it is removed without force.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_worktrees_own_changes_are_diffed_merged_home_and_then_it_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = pa_types::platform::canonicalize(dir.path()).unwrap();
        let repo = repository(&root);
        std::fs::write(repo.join("notes.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(repo.join("draft.txt"), "untracked\n").unwrap();
        let (kernel, recorded) = kernel_in(&root, &repo).await;
        let created = evaluate(kernel.as_ref(), r#"(worktree-create "lane-a")"#)
            .await
            .unwrap();
        let checkout = PathBuf::from(created["path"].as_str().unwrap());
        let carried = (
            std::fs::read_to_string(checkout.join("notes.txt")).unwrap(),
            std::fs::read_to_string(checkout.join("draft.txt")).unwrap(),
        );
        std::fs::write(checkout.join("notes.txt"), "ONE\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(checkout.join("added.txt"), "new\n").unwrap();
        let listed = evaluate(kernel.as_ref(), "(worktree-list)").await.unwrap();
        let diff = evaluate(kernel.as_ref(), r#"(worktree-diff "lane-a")"#)
            .await
            .unwrap();
        let dry = evaluate(kernel.as_ref(), r#"(worktree-merge "lane-a" #:dry-run #t)"#)
            .await
            .unwrap();
        let untouched = std::fs::read_to_string(repo.join("notes.txt")).unwrap();
        // The merge replaces the record whole: a hard link made before it
        // keeps the old record, and no temp file is left beside it.
        let records = root
            .join("agent")
            .join("worktrees")
            .join(repo_key(&repo))
            .join("s1");
        let record_before = std::fs::read_to_string(records.join("lane-a.json")).unwrap();
        std::fs::hard_link(records.join("lane-a.json"), root.join("lane-a.before")).unwrap();
        let merged = evaluate(kernel.as_ref(), "(worktree-merge (car (worktree-list)))")
            .await
            .unwrap();
        let mut record_names: Vec<String> = std::fs::read_dir(&records)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        record_names.sort();
        assert_eq!(
            (
                std::fs::read_to_string(root.join("lane-a.before")).unwrap(),
                record_names,
            ),
            (
                record_before,
                vec!["lane-a".to_string(), "lane-a.json".to_string()]
            )
        );
        let removed = evaluate(kernel.as_ref(), r#"(worktree-remove "lane-a")"#).await;
        let left = evaluate(kernel.as_ref(), "(worktree-list)").await;
        let outcome = |status: &str| {
            json!({
                "status": status,
                "files": ["added.txt", "notes.txt"],
                "conflicts": [],
                "branch": false,
                "ok": true,
            })
        };
        let key = repo_key(&repo);
        let expected_tree = json!({
            "name": "lane-a",
            "path": root.join("agent").join("worktrees").join(key).join("s1").join("lane-a"),
            "repo": repo,
            "base": git(&repo, &["rev-parse", "HEAD"]),
            "exists": true,
        });
        let own_lines: Vec<&str> = diff
            .as_str()
            .unwrap()
            .lines()
            .filter(|line| {
                (line.starts_with('+') || line.starts_with('-'))
                    && !line.starts_with("+++")
                    && !line.starts_with("---")
            })
            .collect();
        assert_eq!(
            (
                created,
                carried,
                listed,
                own_lines,
                dry,
                untouched,
                merged,
                (
                    std::fs::read_to_string(repo.join("notes.txt")).ok(),
                    std::fs::read_to_string(repo.join("added.txt")).ok(),
                ),
                removed,
                left,
                checkout.exists(),
                recorded.lock().unwrap().clone(),
            ),
            (
                expected_tree.clone(),
                (
                    "one\ntwo\nthree\nfour\n".to_string(),
                    "untracked\n".to_string()
                ),
                json!([expected_tree]),
                vec!["+new", "-one", "+ONE"],
                outcome("clean"),
                "one\ntwo\nthree\nfour\n".to_string(),
                outcome("applied"),
                (
                    Some("ONE\ntwo\nthree\nfour\n".to_string()),
                    Some("new\n".to_string())
                ),
                Ok(json!(true)),
                Ok(json!([])),
                false,
                vec![
                    json!({"op": "create", "outcome": "ok", "name": "lane-a", "path": checkout}),
                    json!({"op": "diff", "outcome": "ok"}),
                    json!({"op": "merge", "outcome": "applied"}),
                    json!({"op": "remove", "outcome": "ok", "name": "lane-a", "path": checkout}),
                ],
            )
        );
    }

    /// A delta that no longer applies is reported with its paths and leaves
    /// the checkout alone; the unmerged worktree refuses removal until its
    /// work is kept on a branch.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_conflicting_merge_applies_nothing_and_the_branch_strategy_keeps_the_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = pa_types::platform::canonicalize(dir.path()).unwrap();
        let repo = repository(&root);
        let (kernel, _) = kernel_in(&root, &repo).await;
        let created = evaluate(
            kernel.as_ref(),
            r#"(worktree-create "lane-b" #:carry-changes #f)"#,
        )
        .await
        .unwrap();
        let checkout = PathBuf::from(created["path"].as_str().unwrap());
        std::fs::write(checkout.join("notes.txt"), "one\nTWO-child\nthree\n").unwrap();
        std::fs::write(repo.join("notes.txt"), "one\nTWO-parent\nthree\n").unwrap();
        git(&repo, &["commit", "--quiet", "--all", "-m", "parent"]);
        let conflicted = evaluate(kernel.as_ref(), r#"(worktree-merge "lane-b")"#)
            .await
            .unwrap();
        let refused = evaluate(kernel.as_ref(), r#"(worktree-remove "lane-b")"#).await;
        let branched = evaluate(
            kernel.as_ref(),
            r#"(worktree-merge "lane-b" #:strategy "branch")"#,
        )
        .await
        .unwrap();
        let removed = evaluate(kernel.as_ref(), r#"(worktree-remove "lane-b")"#).await;
        let unknown = evaluate(kernel.as_ref(), r#"(worktree-diff "lane-b")"#).await;
        assert_eq!(
            (
                conflicted,
                std::fs::read_to_string(repo.join("notes.txt")).unwrap(),
                refused,
                branched,
                git(&repo, &["show", "prime-agent/worktree/lane-b:notes.txt"]),
                git(
                    &repo,
                    &["rev-list", "--count", "main~1..prime-agent/worktree/lane-b"]
                ),
                removed,
                unknown,
            ),
            (
                json!({
                    "status": "conflicts",
                    "files": ["notes.txt"],
                    "conflicts": ["notes.txt"],
                    "branch": false,
                    "ok": false,
                }),
                "one\nTWO-parent\nthree\n".to_string(),
                Err("Error: Generic:  worktree-remove: worktree 'lane-b' has changes that were not merged; merge it first or pass #:force #t".to_string()),
                json!({
                    "status": "branch",
                    "files": ["notes.txt"],
                    "conflicts": [],
                    "branch": "prime-agent/worktree/lane-b",
                    "ok": true,
                }),
                "one\nTWO-child\nthree".to_string(),
                "2".to_string(),
                Ok(json!(true)),
                Err("Error: Generic:  worktree-diff: no worktree named 'lane-b' in this session".to_string()),
            )
        );
    }
}
