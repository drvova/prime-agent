//! The project card: a compact summary of how to build, test, and check the
//! project, its key directories, and the commands its sessions ran, put in
//! the system prompt's dynamic tail so a session starts knowing what it
//! would otherwise spend its first rounds discovering.
//!
//! The card is derived from the repository (Cargo, npm, Python, and Go
//! manifests, the Makefile, CI workflow `run:` steps, the top-level
//! directories) plus learned outcomes (the build/test commands this
//! project's cells ran, counted). The full detail behind it pages out to the
//! session's spill store, and the card names its handle.
//!
//! It never sits on the first-paint path: a session reads the card the
//! previous refresh stored (`<agent dir>/project-cards/<project hash>.json`)
//! under a short budget, and a background thread re-derives and stores it for
//! the next session. A project seen for the first time gets no card until
//! then. The card stays in the dynamic tail, never the cached static prefix.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::kernel::spill::{SpillCopy, SpillStore, HANDLE_PREFIX};

/// Characters of the rendered card.
const MAX_CARD_CHARS: usize = 1_500;
/// Commands shown per line, and learned commands kept.
const SHOWN_COMMANDS: usize = 4;
const KEPT_COMMANDS: usize = 32;
/// Key directories shown, and files counted per project.
const SHOWN_DIRS: usize = 8;
const MAX_COUNTED_FILES: usize = 20_000;
/// How long a session start waits for the stored card.
pub(crate) const LOAD_BUDGET: Duration = Duration::from_millis(50);
const SCHEMA: u64 = 1;
const STATE_DIR: &str = "project-cards";

/// What the repository says: commands by purpose, key directories, and the
/// full detail the card summarizes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Derived {
    pub(crate) build: Vec<String>,
    pub(crate) test: Vec<String>,
    pub(crate) check: Vec<String>,
    /// `dir/ (N files)`, largest first.
    pub(crate) dirs: Vec<String>,
    pub(crate) details: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Stored {
    schema: u64,
    derived: Option<Derived>,
    /// Build/test commands cells here ran, with how often.
    commands: BTreeMap<String, u64>,
}

static LEARNABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:cargo|make|just|npm|pnpm|yarn|bun|npx|pytest|uv|tox|go|tsc|ruff|mypy|python[\d.]*\s+-m\s+(?:pytest|unittest|mypy))\s+\S",
    )
    .expect("the learnable-command pattern compiles")
});
static MAKE_TARGET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z][\w.-]*)\s*:(?!=)").expect("the make-target pattern compiles")
});
static CI_RUN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:-\s+)?run:\s*([^|>\s].*?)\s*$").expect("the CI run pattern compiles")
});

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn push_unique(list: &mut Vec<String>, command: String) {
    if !list.contains(&command) {
        list.push(command);
    }
}

/// Derive the card from the repository at `root` (blocking: reads a few files and counts files).
pub(crate) fn derive(root: &Path) -> Derived {
    let mut card = Derived::default();
    let mut details = String::new();
    let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();
    if root.join("Cargo.toml").is_file() {
        push_unique(&mut card.build, "cargo build".to_string());
        push_unique(&mut card.test, "cargo test".to_string());
    }
    if let Some(package) = read("package.json") {
        let runner = if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else {
            "npm"
        };
        if let Ok(serde_json::Value::Object(json)) = serde_json::from_str(&package) {
            if let Some(serde_json::Value::Object(scripts)) = json.get("scripts") {
                for (name, body) in scripts {
                    let _ = writeln!(
                        details,
                        "{runner} run {name}: {}",
                        body.as_str().unwrap_or_default()
                    );
                    let command = format!("{runner} run {name}");
                    match name.as_str() {
                        "build" => push_unique(&mut card.build, command),
                        "test" => push_unique(&mut card.test, format!("{runner} test")),
                        "lint" | "check" | "typecheck" | "format:check" => {
                            push_unique(&mut card.check, command);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    if let Some(pyproject) = read("pyproject.toml") {
        if pyproject.contains("pytest") || root.join("tests").is_dir() {
            let command = if root.join("uv.lock").is_file() {
                "uv run pytest"
            } else {
                "pytest"
            };
            push_unique(&mut card.test, command.to_string());
        }
    }
    if root.join("go.mod").is_file() {
        push_unique(&mut card.build, "go build ./...".to_string());
        push_unique(&mut card.test, "go test ./...".to_string());
        push_unique(&mut card.check, "go vet ./...".to_string());
    }
    if let Some(makefile) = read("Makefile") {
        for line in makefile.lines() {
            let Ok(Some(target)) = MAKE_TARGET.captures(line) else {
                continue;
            };
            let name = target.get(1).map_or("", |m| m.as_str());
            if name.starts_with('.') {
                continue;
            }
            let command = format!("make {name}");
            let _ = writeln!(details, "{command}");
            if name.contains("test") {
                push_unique(&mut card.test, command);
            } else if name.contains("build") || name == "all" {
                push_unique(&mut card.build, command);
            } else if ["check", "lint", "fmt", "clippy", "deny", "typecheck"]
                .iter()
                .any(|word| name.contains(word))
            {
                push_unique(&mut card.check, command);
            }
        }
    }
    let workflows = root.join(".github/workflows");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&workflows)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    files.sort();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(Some(run)) = CI_RUN.captures(line) {
                let command = run
                    .get(1)
                    .map_or("", |m| m.as_str())
                    .trim_matches(['"', '\'']);
                let _ = writeln!(details, "ci: {command}");
                if command.len() <= 100 && !command.contains("${{") {
                    push_unique(&mut card.check, command.to_string());
                }
            }
        }
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    let walk = ignore::WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walk.flatten().take(MAX_COUNTED_FILES) {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let mut parts = relative.components();
        if let (Some(first), Some(_)) = (parts.next(), parts.next()) {
            *counts
                .entry(first.as_os_str().to_string_lossy().to_string())
                .or_default() += 1;
        }
    }
    let mut dirs: Vec<(String, usize)> = counts.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    card.dirs = dirs
        .iter()
        .take(SHOWN_DIRS)
        .map(|(dir, files)| format!("{dir}/ ({files} files)"))
        .collect();
    for (dir, files) in &dirs {
        let _ = writeln!(details, "dir: {dir}/ ({files} files)");
    }
    card.details = details;
    card
}

/// One project's card (cheap to clone; every clone shares the state).
#[derive(Clone)]
pub(crate) struct ProjectCard {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    store: PathBuf,
    state: Mutex<Stored>,
    /// Held across one save, which snapshots the state inside it, so the last
    /// save to run always writes the newest state.
    saving: Mutex<()>,
}

/// The process's cards by project. A worker serves one session, so the few
/// cards it ever registers stay for its lifetime.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Inner>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<Inner>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

impl ProjectCard {
    /// The card of the project at `root`, stored under `agent_dir`. Every call
    /// for one project in this process shares one card. No disk work.
    pub(crate) fn for_project(root: &Path, agent_dir: &Path) -> Self {
        let mut cards = lock(registry());
        if let Some(inner) = cards.get(root) {
            return Self {
                inner: Arc::clone(inner),
            };
        }
        let digest = Sha256::digest(root.to_string_lossy().as_bytes());
        let name = digest[..8].iter().fold(String::new(), |mut name, byte| {
            let _ = write!(name, "{byte:02x}");
            name
        });
        let card = Self {
            inner: Arc::new(Inner {
                root: root.to_path_buf(),
                store: agent_dir.join(STATE_DIR).join(format!("{name}.json")),
                state: Mutex::new(Stored::default()),
                saving: Mutex::new(()),
            }),
        };
        cards.insert(root.to_path_buf(), Arc::clone(&card.inner));
        card
    }

    /// The card already registered for `root` in this process, if any (the
    /// kernel executor teaches it the commands cells run).
    pub(crate) fn registered(root: &Path) -> Option<Self> {
        lock(registry()).get(root).map(|inner| Self {
            inner: Arc::clone(inner),
        })
    }

    /// Read the stored card, waiting at most `budget` (a slow disk leaves the
    /// session without a card, never later).
    pub(crate) async fn load(&self, budget: Duration) {
        let path = self.inner.store.clone();
        let read = tokio::task::spawn_blocking(move || {
            let raw = std::fs::read(path).ok()?;
            let stored: Stored = serde_json::from_slice(&raw).ok()?;
            (stored.schema == SCHEMA).then_some(stored)
        });
        if let Ok(Ok(Some(stored))) = tokio::time::timeout(budget, read).await {
            let mut state = lock(&self.inner.state);
            for (command, count) in stored.commands {
                *state.commands.entry(command).or_default() += count;
            }
            if state.derived.is_none() {
                state.derived = stored.derived;
            }
        }
    }

    /// Re-derive the card on a background thread and store it for the next
    /// session (never for the filesystem root or the home directory, which are
    /// no project).
    pub(crate) fn refresh(&self) {
        let root = self.inner.root.as_path();
        if root.parent().is_none()
            || pa_types::session_scope::var_os("HOME").is_some_and(|home| root == Path::new(&home))
        {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let _ = std::thread::Builder::new()
            .name("project-card".to_string())
            .spawn(pa_types::session_scope::propagate(move || {
                let derived = derive(&inner.root);
                lock(&inner.state).derived = Some(derived);
                inner.save();
            }));
    }

    /// Learn one command a cell ran (only build/test/check shapes count).
    /// Never waits: the store is written on a background thread.
    pub(crate) fn observe_command(&self, command: &str) {
        let command = command.lines().next().unwrap_or_default().trim();
        if command.len() > 120 || !LEARNABLE.is_match(command).unwrap_or(false) {
            return;
        }
        {
            let mut state = lock(&self.inner.state);
            *state.commands.entry(command.to_string()).or_default() += 1;
            while state.commands.len() > KEPT_COMMANDS {
                let rarest = state
                    .commands
                    .iter()
                    .min_by_key(|(_, count)| **count)
                    .map(|(command, _)| command.clone());
                if let Some(rarest) = rarest {
                    state.commands.remove(&rarest);
                }
            }
        }
        let inner = Arc::clone(&self.inner);
        let _ = std::thread::Builder::new()
            .name("project-card-save".to_string())
            .spawn(pa_types::session_scope::propagate(move || inner.save()));
    }

    /// The card for the prompt's dynamic tail, or `None` when nothing is known
    /// yet. Its full detail pages out to `spill` (when the session has a store).
    pub(crate) fn render(&self, spill: Option<&SpillStore>) -> Option<String> {
        let state = lock(&self.inner.state).clone();
        let derived = state.derived.unwrap_or_default();
        let mut learned: Vec<(&String, &u64)> = state.commands.iter().collect();
        learned.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        if derived == Derived::default() && learned.is_empty() {
            return None;
        }
        let mut card = String::from("# Project card\n\nDerived from this repository's manifests, Makefile, and CI, plus the commands sessions here ran");
        if let (Some(store), false) = (spill, derived.details.is_empty()) {
            let handle = store.spill(SpillCopy::of_text(&derived.details));
            let _ = write!(
                card,
                "; the full detail is {HANDLE_PREFIX}{} (page it with ctx.page)",
                handle.id
            );
        }
        card.push('.');
        let commands = |list: &[String]| {
            list.iter()
                .take(SHOWN_COMMANDS)
                .map(|command| format!("`{command}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        for (label, list) in [
            ("Build", &derived.build),
            ("Test", &derived.test),
            ("Checks", &derived.check),
        ] {
            if !list.is_empty() {
                let _ = write!(card, "\n- {label}: {}", commands(list));
            }
        }
        if !derived.dirs.is_empty() {
            let _ = write!(card, "\n- Key dirs: {}", derived.dirs.join(", "));
        }
        if !learned.is_empty() {
            let shown: Vec<String> = learned
                .iter()
                .take(SHOWN_COMMANDS)
                .map(|(command, count)| format!("`{command}` ({count}×)"))
                .collect();
            let _ = write!(card, "\n- Run here before: {}", shown.join(", "));
        }
        if let Some((end, _)) = card.char_indices().nth(MAX_CARD_CHARS) {
            card.truncate(end);
            card.push('…');
        }
        Some(card)
    }
}

impl Inner {
    fn save(&self) {
        let path = self.store.as_path();
        let _saving = lock(&self.saving);
        let mut stored = lock(&self.state).clone();
        stored.schema = SCHEMA;
        let written = (|| -> std::io::Result<()> {
            pa_types::session_scope::ensure_live()?;
            let body = serde_json::to_vec(&stored)?;
            let dir = path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(dir)?;
            let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
            std::fs::write(&temp, body)?;
            crate::platform::rename_onto(&temp, path)
        })();
        if let Err(error) = written {
            tracing::debug!(%error, "project card not saved");
        }
    }
}
