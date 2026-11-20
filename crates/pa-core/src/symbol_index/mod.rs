//! The kernel's persistent per-project symbol index: where each name is
//! defined and which files use it, so `fs.symbol(name)` answers definitions,
//! references, and handles to the definitions' source in one call instead of
//! a grep, a read, and another grep.
//!
//! Definitions come from per-language line patterns (Rust, Python,
//! JavaScript/TypeScript, Go, C/C++), with no parser dependency: a definition
//! is the line that introduces a name (`fn`, `class`, `def`, `type`, …).
//! References come from each file's identifier set: a query reads only the
//! files whose set holds the name, and keeps the lines where it appears as a
//! whole word.
//!
//! The index is built in the background (the walk respects `.gitignore`) and
//! kept under `<agent dir>/symbol-index/<project hash>.json`; a later session
//! loads it and re-reads only the files whose size or mtime changed. The
//! first query starts the build; no query waits for it longer than a short
//! budget: before the first full pass it answers from the files indexed so
//! far and says the answer is partial, and every query starts a background
//! refresh (at most one at a time). Files in a query's answer are
//! re-checked first, so an edited file never answers from its old lines.

mod host;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, Weak};
use std::time::UNIX_EPOCH;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) use host::{register_symbol_handler, SymbolFact, SymbolObserver};

/// Files indexed per project (the walk stops past it).
const MAX_FILES: usize = 20_000;
/// A file larger than this is not indexed.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Characters of a shown line.
const MAX_LINE_CHARS: usize = 240;
/// Bumped when the stored index changes meaning; an older file loads as empty.
const SCHEMA: u64 = 1;
const STATE_DIR: &str = "symbol-index";

/// One definition: the name, what introduces it, and its 1-based line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Definition {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) line: u32,
}

/// One indexed file: the stamp it was read at, its definitions, and the
/// identifiers it uses (sorted, each once).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileEntry {
    size: u64,
    mtime_ns: u128,
    definitions: Vec<Definition>,
    identifiers: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    schema: u64,
    files: HashMap<String, FileEntry>,
}

/// The language a file's definitions are read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    Python,
    Script,
    Go,
    C,
}

impl Language {
    fn of(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Some(Self::Script),
            "go" => Some(Self::Go),
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(Self::C),
            _ => None,
        }
    }

    fn patterns(self) -> &'static [Regex] {
        fn compile(patterns: &[&str]) -> Vec<Regex> {
            patterns
                .iter()
                .map(|pattern| Regex::new(pattern).expect("a static definition pattern compiles"))
                .collect()
        }
        static RUST: LazyLock<Vec<Regex>> = LazyLock::new(|| {
            compile(&[
                r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|extern(?:\s+"[^"]*")?)\s+)*(fn|struct|enum|trait|type|mod|union|const|static)\s+([A-Za-z_]\w*)"#,
                r"^\s*(macro_rules)!\s*([A-Za-z_]\w*)",
            ])
        });
        static PYTHON: LazyLock<Vec<Regex>> = LazyLock::new(|| {
            compile(&[
                r"^\s*(?:async\s+)?(def|class)\s+([A-Za-z_]\w*)",
                r"^()([A-Za-z_]\w*)\s*(?::[^=]+)?=(?!=)",
            ])
        });
        static SCRIPT: LazyLock<Vec<Regex>> = LazyLock::new(|| {
            compile(&[
                r"^\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(function\*?|class|interface|type|enum|const|let|var|namespace)\s+([A-Za-z_$][\w$]*)",
            ])
        });
        static GO: LazyLock<Vec<Regex>> = LazyLock::new(|| {
            compile(&[
                r"^(func)\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)",
                r"^\s*(type|var|const)\s+([A-Za-z_]\w*)",
            ])
        });
        static C: LazyLock<Vec<Regex>> = LazyLock::new(|| {
            compile(&[
                r"^\s*(?:typedef\s+)?(struct|class|enum|union|namespace)\s+([A-Za-z_]\w*)\s*(?:[:{]|$)",
                r"^\s*#\s*(define)\s+([A-Za-z_]\w*)",
                r"^(?![\s#])(?!.*\b(?:return|if|while|for|switch|else)\b)[\w\s\*&:<>,]*?\b()([A-Za-z_]\w*)\s*\([^;]*$",
            ])
        });
        match self {
            Self::Rust => &RUST,
            Self::Python => &PYTHON,
            Self::Script => &SCRIPT,
            Self::Go => &GO,
            Self::C => &C,
        }
    }
}

static IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z_$][\w$]*").expect("the identifier pattern compiles"));

/// The definitions and identifiers of one file's text.
fn parse(language: Language, text: &str) -> (Vec<Definition>, Vec<String>) {
    let mut definitions = Vec::new();
    let mut identifiers = BTreeSet::new();
    for (line, number) in text.lines().zip(1u32..) {
        for pattern in language.patterns() {
            if let Ok(Some(found)) = pattern.captures(line) {
                let kind = found.get(1).map_or("", |m| m.as_str());
                if let Some(name) = found.get(2) {
                    // The patterns without a keyword: a Python assignment, a C function.
                    let kind = match (kind, language) {
                        ("", Language::Python) => "variable",
                        ("", Language::Rust | Language::Script | Language::Go | Language::C) => {
                            "function"
                        }
                        (kind, _) => kind,
                    };
                    definitions.push(Definition {
                        name: name.as_str().to_string(),
                        kind: kind.to_string(),
                        line: number,
                    });
                    break;
                }
            }
        }
        for found in IDENTIFIER.find_iter(line).flatten() {
            if found.as_str().len() > 1 {
                identifiers.insert(found.as_str().to_string());
            }
        }
    }
    (definitions, identifiers.into_iter().collect())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A file's size and mtime, or `None` when it is gone or unreadable.
fn stamp(path: &Path) -> Option<(u64, u128)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((metadata.len(), mtime))
}

/// Read and parse one file, or `None` when it is too large, not text, or gone.
fn index_file(path: &Path) -> Option<FileEntry> {
    let language = Language::of(path)?;
    let (size, mtime_ns) = stamp(path)?;
    if size > MAX_FILE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let (definitions, identifiers) = parse(language, &text);
    Some(FileEntry {
        size,
        mtime_ns,
        definitions,
        identifiers,
    })
}

struct State {
    /// Indexed files by path relative to the project, with `/`.
    files: HashMap<String, FileEntry>,
    /// The first full pass (stored or fresh) has finished.
    complete: bool,
}

/// One project's index (cheap to clone; every clone shares the state).
#[derive(Clone)]
pub(crate) struct SymbolIndex {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    store: Option<PathBuf>,
    state: Mutex<State>,
    /// A refresh is running.
    refreshing: AtomicBool,
    /// Woken when a refresh ends (tests wait on it).
    refreshed: tokio::sync::Notify,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<Inner>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<Inner>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One definition as a query answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) path: String,
    pub(crate) line: u32,
    pub(crate) kind: Option<String>,
    pub(crate) text: String,
}

/// A query's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub(crate) definitions: Vec<Found>,
    pub(crate) references: Vec<Found>,
    /// More references exist than were returned.
    pub(crate) truncated: bool,
    /// The first full pass has not finished: files not yet indexed are missing.
    pub(crate) partial: bool,
    /// Each definition's source from its line on (for its spill handle), in order.
    pub(crate) sources: Vec<String>,
}

/// Lines of a definition's source kept for its handle.
const SOURCE_LINES: usize = 80;

impl SymbolIndex {
    /// The index of the project at `root`, stored under `agent_dir`. Every
    /// call for one project in this process shares one index. No disk work.
    pub(crate) fn for_project(root: &Path, agent_dir: Option<&Path>) -> Self {
        let mut indexes = lock(registry());
        if let Some(inner) = indexes.get(root).and_then(Weak::upgrade) {
            return Self { inner };
        }
        indexes.retain(|_, index| index.strong_count() > 0);
        let store = agent_dir.map(|dir| {
            let digest = Sha256::digest(root.to_string_lossy().as_bytes());
            let name: String = digest[..8].iter().fold(String::new(), |mut name, byte| {
                let _ = write!(name, "{byte:02x}");
                name
            });
            dir.join(STATE_DIR).join(format!("{name}.json"))
        });
        let index = Self {
            inner: Arc::new(Inner {
                root: root.to_path_buf(),
                store,
                state: Mutex::new(State {
                    files: HashMap::new(),
                    complete: false,
                }),
                refreshing: AtomicBool::new(false),
                refreshed: tokio::sync::Notify::new(),
            }),
        };
        indexes.insert(root.to_path_buf(), Arc::downgrade(&index.inner));
        index
    }

    /// Start a background refresh unless one is running: load the stored
    /// index (first time), walk the project, re-read changed files, drop
    /// removed ones, then save. Never waits.
    pub(crate) fn refresh(&self) {
        if self.inner.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let spawned = std::thread::Builder::new()
            .name("symbol-index".to_string())
            .spawn(move || {
                inner.run_refresh();
                inner.refreshing.store(false, Ordering::Release);
                inner.refreshed.notify_waiters();
            });
        if spawned.is_err() {
            self.inner.refreshing.store(false, Ordering::Release);
        }
    }

    /// Wait until the running refresh (if any) ends.
    pub(crate) async fn refreshed(&self) {
        loop {
            let notified = self.inner.refreshed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.inner.refreshing.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// The first full pass has finished.
    pub(crate) fn is_complete(&self) -> bool {
        lock(&self.inner.state).complete
    }

    /// Answer `name` from the files indexed so far (blocking: reads the files
    /// that mention it, at most `max_references` references). Run it off the
    /// async threads.
    pub(crate) fn query(&self, name: &str, max_references: usize) -> Answer {
        let (candidates, partial) = {
            let state = lock(&self.inner.state);
            let candidates: Vec<String> = state
                .files
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .identifiers
                        .binary_search_by(|id| id.as_str().cmp(name))
                        .is_ok()
                        || entry.definitions.iter().any(|d| d.name == name)
                })
                .map(|(path, _)| path.clone())
                .collect();
            (candidates, !state.complete)
        };
        let mut paths = candidates;
        paths.sort();
        let word = Regex::new(&format!(
            r"(?<![\w$]){}(?![\w$])",
            fancy_regex::escape(name)
        ))
        .expect("an escaped name compiles");
        let mut answer = Answer {
            definitions: Vec::new(),
            references: Vec::new(),
            truncated: false,
            partial,
            sources: Vec::new(),
        };
        for relative in paths {
            let path = self.inner.root.join(&relative);
            // An answer never comes from a file's old lines: re-read it if it changed.
            let current = {
                let state = lock(&self.inner.state);
                state.files.get(&relative).cloned()
            };
            let entry = match (current, stamp(&path)) {
                (Some(entry), Some((size, mtime)))
                    if entry.size == size && entry.mtime_ns == mtime =>
                {
                    Some(entry)
                }
                (_, Some(_)) => index_file(&path),
                (_, None) => None,
            };
            let mut state = lock(&self.inner.state);
            match &entry {
                Some(entry) => {
                    state.files.insert(relative.clone(), entry.clone());
                }
                None => {
                    state.files.remove(&relative);
                }
            }
            drop(state);
            let Some(entry) = entry else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            let shown = |line: u32| {
                let text = lines
                    .get(line as usize - 1)
                    .copied()
                    .unwrap_or_default()
                    .trim();
                match text.char_indices().nth(MAX_LINE_CHARS) {
                    Some((end, _)) => format!("{}…", &text[..end]),
                    None => text.to_string(),
                }
            };
            let defined: Vec<&Definition> = entry
                .definitions
                .iter()
                .filter(|d| d.name == name)
                .collect();
            for definition in &defined {
                answer.definitions.push(Found {
                    path: relative.clone(),
                    line: definition.line,
                    kind: Some(definition.kind.clone()),
                    text: shown(definition.line),
                });
                let start = definition.line as usize - 1;
                answer
                    .sources
                    .push(lines[start..(start + SOURCE_LINES).min(lines.len())].join("\n"));
            }
            for (line, number) in lines.iter().zip(1u32..) {
                if defined.iter().any(|d| d.line == number) || !word.is_match(line).unwrap_or(false)
                {
                    continue;
                }
                if answer.references.len() == max_references {
                    answer.truncated = true;
                    break;
                }
                answer.references.push(Found {
                    path: relative.clone(),
                    line: number,
                    kind: None,
                    text: shown(number),
                });
            }
        }
        answer
    }
}

impl Inner {
    fn run_refresh(&self) {
        let loaded = lock(&self.state).complete;
        if !loaded {
            if let Some(stored) = self.store.as_deref().and_then(|path| {
                let raw = std::fs::read(path).ok()?;
                let stored: Stored = serde_json::from_slice(&raw).ok()?;
                (stored.schema == SCHEMA).then_some(stored.files)
            }) {
                let mut state = lock(&self.state);
                for (path, entry) in stored {
                    state.files.entry(path).or_insert(entry);
                }
            }
        }
        let mut seen = BTreeSet::new();
        let walk = ignore::WalkBuilder::new(&self.root)
            .require_git(false)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build();
        for entry in walk.flatten() {
            if seen.len() == MAX_FILES {
                break;
            }
            let path = entry.path();
            if !entry.file_type().is_some_and(|kind| kind.is_file()) || Language::of(path).is_none()
            {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            let relative = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            seen.insert(relative.clone());
            let fresh = {
                let state = lock(&self.state);
                let current = state.files.get(&relative).map(|e| (e.size, e.mtime_ns));
                current.is_some() && current == stamp(path)
            };
            if fresh {
                continue;
            }
            let parsed = index_file(path);
            let mut state = lock(&self.state);
            match parsed {
                Some(parsed) => {
                    state.files.insert(relative, parsed);
                }
                None => {
                    state.files.remove(&relative);
                }
            }
        }
        let snapshot = {
            let mut state = lock(&self.state);
            state.files.retain(|path, _| seen.contains(path));
            state.complete = true;
            self.store.as_ref().map(|_| state.files.clone())
        };
        if let (Some(path), Some(files)) = (self.store.as_deref(), snapshot) {
            let written = (|| -> std::io::Result<()> {
                let body = serde_json::to_vec(&Stored {
                    schema: SCHEMA,
                    files,
                })?;
                let dir = path.parent().unwrap_or_else(|| Path::new("."));
                std::fs::create_dir_all(dir)?;
                let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
                std::fs::write(&temp, body)?;
                crate::platform::rename_onto(&temp, path)
            })();
            if let Err(error) = written {
                tracing::debug!(%error, "symbol index not saved");
            }
        }
    }
}
