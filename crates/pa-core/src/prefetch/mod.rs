//! Neighbour prefetch: when the model reads a file with `fs.read`, the files
//! it is likely to read next (its test file, the workspace files it imports,
//! and the files that call what it defines, from the symbol index) page into
//! the session's spill store, and the read's result names one handle per
//! file. Nothing enters the context: a handle costs one short line, and the
//! model pages a neighbour in with `ctx.page` only if it wants it, without
//! another `fs.read` round.
//!
//! The prefetch adapts per project: a decaying average of the share of
//! prefetched handles the model went on to use (a `ctx.*` read of the handle,
//! or an `fs.read` of the file) sets how many neighbours each read gets, down
//! to none, with an occasional single-handle probe so a project that starts
//! using them earns them back. The learned share lives under
//! `<agent dir>/prefetch/<project hash>.json`, loaded and saved off the read.
//!
//! The read never waits on the prefetch longer than a short budget: a slow
//! or hung neighbour leaves the read without its handles.

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::kernel::spill::{SpillCopy, SpillStore, HANDLE_PREFIX};
use crate::symbol_index::SymbolIndex;

/// The most neighbours one read gets.
const MAX_NEIGHBOURS: usize = 4;
/// Reads before the learned share decides anything.
const LEARNING_READS: u64 = 3;
/// Decay of the learned share per settled batch (the newest weighs 1 - DECAY).
const DECAY: f64 = 0.8;
/// A project whose share fell to none still gets one handle every this-many reads.
const PROBE_EVERY: u64 = 10;
/// Characters of one neighbour kept for its handle.
const MAX_NEIGHBOUR_CHARS: usize = 64 * 1024;
/// Defined names whose callers are looked up, per read.
const CALLER_NAMES: usize = 3;
/// How long a read waits for its prefetch.
pub(crate) const PREFETCH_BUDGET: Duration = Duration::from_millis(150);
const SCHEMA: u64 = 1;
const STATE_DIR: &str = "prefetch";

/// Why a file is a neighbour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    Test,
    Import,
    Caller,
}

impl Why {
    fn label(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Import => "import",
            Self::Caller => "caller",
        }
    }
}

/// One prefetch, settled or made (adoption counts only: never a path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PrefetchFact {
    /// Handles this read got.
    pub(crate) issued: usize,
    /// Of the previous read's handles, those used and those left unused.
    pub(crate) used: usize,
    pub(crate) unused: usize,
    /// The learned share left this read without handles.
    pub(crate) skipped: bool,
}

/// Hears each prefetch.
pub(crate) type PrefetchObserver = Arc<dyn Fn(PrefetchFact) + Send + Sync>;

/// Reads a neighbour's text (blocking; injectable so a test can hang it).
type Reader = Arc<dyn Fn(&Path) -> std::io::Result<String> + Send + Sync>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct Learned {
    share: f64,
    batches: u64,
    reads: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredLearned {
    schema: u64,
    learned: Learned,
}

impl Learned {
    /// How many neighbours the next read gets.
    fn budget(&self) -> usize {
        if self.batches < LEARNING_READS {
            return MAX_NEIGHBOURS;
        }
        let scaled = (self.share * MAX_NEIGHBOURS as f64).round() as usize;
        if scaled == 0 && self.reads.is_multiple_of(PROBE_EVERY) {
            return 1;
        }
        scaled.min(MAX_NEIGHBOURS)
    }

    fn settle(&mut self, used: usize, issued: usize) {
        if issued == 0 {
            return;
        }
        let share = used as f64 / issued as f64;
        self.share = if self.batches == 0 {
            share
        } else {
            self.share * DECAY + share * (1.0 - DECAY)
        };
        self.batches += 1;
    }
}

/// One read's prefetch: the reply entries, the text line, and the handles (id, file).
struct Batch {
    related: Vec<Value>,
    line: String,
    handles: Vec<(String, PathBuf)>,
}

struct State {
    learned: Learned,
    loaded: bool,
    /// The last read's handles (id -> path), and those used since.
    outstanding: HashMap<String, PathBuf>,
    used: usize,
}

/// One session's neighbour prefetch (cheap to clone).
#[derive(Clone)]
pub(crate) struct Prefetcher {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    index: SymbolIndex,
    store: SpillStore,
    state_path: Option<PathBuf>,
    reader: Reader,
    state: Mutex<State>,
    observer: Option<PrefetchObserver>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static RUST_MOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;").expect("pattern")
});
static PY_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:from\s+(\.*[\w.]*)\s+import|import\s+([\w.]+))").expect("pattern")
});
static JS_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:from\s+|require\(\s*|import\s*\(?\s*)["'](\.{1,2}/[^"']+)["']"#)
        .expect("pattern")
});
static C_INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*#\s*include\s+"([^"]+)""#).expect("pattern"));

const SCRIPT_EXTENSIONS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];

impl Prefetcher {
    pub(crate) fn new(
        root: &Path,
        agent_dir: Option<&Path>,
        index: SymbolIndex,
        store: SpillStore,
        observer: Option<PrefetchObserver>,
    ) -> Self {
        let state_path = agent_dir.map(|dir| {
            let digest = Sha256::digest(root.to_string_lossy().as_bytes());
            let name = digest[..8].iter().fold(String::new(), |mut name, byte| {
                let _ = write!(name, "{byte:02x}");
                name
            });
            dir.join(STATE_DIR).join(format!("{name}.json"))
        });
        Self::with_reader(
            root,
            state_path,
            index,
            store,
            observer,
            Arc::new(|path: &Path| std::fs::read_to_string(path)),
        )
    }

    fn with_reader(
        root: &Path,
        state_path: Option<PathBuf>,
        index: SymbolIndex,
        store: SpillStore,
        observer: Option<PrefetchObserver>,
        reader: Reader,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: root.to_path_buf(),
                index,
                store,
                state_path,
                reader,
                state: Mutex::new(State {
                    learned: Learned::default(),
                    loaded: false,
                    outstanding: HashMap::new(),
                    used: 0,
                }),
                observer,
            }),
        }
    }

    /// A `ctx.*` read of handle `id`: counts as a use of a prefetched handle.
    pub(crate) fn note_handle_read(&self, id: &str) {
        let mut state = lock(&self.inner.state);
        if state.outstanding.remove(id).is_some() {
            state.used += 1;
        }
    }

    /// Prefetch the neighbours of `file` (just read) for the read's reply:
    /// `(the reply's related entries, the text line naming them)`. Waits at
    /// most [`PREFETCH_BUDGET`]; a prefetch that runs past it is dropped.
    pub(crate) async fn prefetch(&self, file: PathBuf) -> Option<(Value, String)> {
        let inner = Arc::clone(&self.inner);
        let work = tokio::task::spawn_blocking(move || inner.run(&file));
        let batch = tokio::time::timeout(PREFETCH_BUDGET, work)
            .await
            .ok()?
            .ok()?;
        // Only handles the read actually returns are waited on for their use.
        lock(&self.inner.state).outstanding.extend(batch.handles);
        (!batch.related.is_empty()).then(|| {
            (
                Value::Array(batch.related),
                format!("[related, paged out (read with ctx.page): {}]", batch.line),
            )
        })
    }
}

impl Inner {
    fn run(&self, file: &Path) -> Batch {
        self.load();
        let (budget, settled) = {
            let mut state = lock(&self.state);
            // Reading a prefetched file is a use of its handle too.
            let read_ids: Vec<String> = state
                .outstanding
                .iter()
                .filter(|(_, path)| path.as_path() == file)
                .map(|(id, _)| id.clone())
                .collect();
            for id in read_ids {
                state.outstanding.remove(&id);
                state.used += 1;
            }
            let used = std::mem::take(&mut state.used);
            let unused = std::mem::take(&mut state.outstanding).len();
            state.learned.settle(used, used + unused);
            state.learned.reads += 1;
            (state.learned.budget(), (used, unused))
        };
        self.save();
        let mut fact = PrefetchFact {
            used: settled.0,
            unused: settled.1,
            skipped: budget == 0,
            ..PrefetchFact::default()
        };
        let mut related = Vec::new();
        let mut handles = Vec::new();
        let mut line = String::new();
        if budget > 0 {
            for (path, why) in self.neighbours(file).into_iter().take(budget) {
                let Ok(text) = (self.reader)(&path) else {
                    continue;
                };
                let kept: String = text.chars().take(MAX_NEIGHBOUR_CHARS).collect();
                let id = self.store.spill(SpillCopy::of_text(&kept)).id;
                let shown = self.shown(&path);
                let handle = format!("{HANDLE_PREFIX}{id}");
                let _ = write!(
                    line,
                    "{}{} {shown} {handle}",
                    if line.is_empty() { "" } else { " · " },
                    why.label()
                );
                related.push(json!({"path": shown, "why": why.label(), "handle": handle}));
                handles.push((id, path));
            }
        }
        fact.issued = related.len();
        if let Some(observer) = &self.observer {
            observer(fact);
        }
        Batch {
            related,
            line,
            handles,
        }
    }

    fn shown(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The likely next reads of `file`, in order: its test file, the
    /// workspace files it imports, then the files calling what it defines.
    fn neighbours(&self, file: &Path) -> Vec<(PathBuf, Why)> {
        let mut found: Vec<(PathBuf, Why)> = Vec::new();
        let mut add = |path: PathBuf, why: Why| {
            if path != file
                && path.starts_with(&self.root)
                && path.is_file()
                && !found.iter().any(|(known, _)| *known == path)
            {
                found.push((path, why));
            }
        };
        let dir = file.parent().unwrap_or(&self.root).to_path_buf();
        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let extension = file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        match extension {
            "rs" => {
                add(dir.join(stem).join("tests.rs"), Why::Test);
                if stem == "mod" || stem == "lib" {
                    add(dir.join("tests.rs"), Why::Test);
                }
                let mut crate_dir = dir.clone();
                while crate_dir.starts_with(&self.root) && !crate_dir.join("Cargo.toml").is_file() {
                    if !crate_dir.pop() {
                        break;
                    }
                }
                add(
                    crate_dir.join("tests").join(format!("{stem}.rs")),
                    Why::Test,
                );
            }
            "py" => {
                add(dir.join(format!("test_{stem}.py")), Why::Test);
                add(dir.join(format!("{stem}_test.py")), Why::Test);
                let mut up = dir.clone();
                while up.starts_with(&self.root) {
                    add(up.join("tests").join(format!("test_{stem}.py")), Why::Test);
                    add(up.join("test").join(format!("test_{stem}.py")), Why::Test);
                    if !up.pop() {
                        break;
                    }
                }
            }
            "go" => add(dir.join(format!("{stem}_test.go")), Why::Test),
            other if SCRIPT_EXTENSIONS.contains(&other) => {
                for kind in ["test", "spec"] {
                    add(dir.join(format!("{stem}.{kind}.{other}")), Why::Test);
                    add(
                        dir.join("__tests__").join(format!("{stem}.{kind}.{other}")),
                        Why::Test,
                    );
                }
            }
            _ => {}
        }
        if let Ok(text) = (self.reader)(file) {
            for line in text.lines() {
                if let Ok(Some(found)) = RUST_MOD.captures(line) {
                    let name = found.get(1).map_or("", |m| m.as_str());
                    let base = if matches!(stem, "mod" | "lib" | "main") {
                        dir.clone()
                    } else {
                        dir.join(stem)
                    };
                    if name != "tests" {
                        add(base.join(format!("{name}.rs")), Why::Import);
                        add(base.join(name).join("mod.rs"), Why::Import);
                    }
                } else if let Ok(Some(found)) = PY_IMPORT.captures(line) {
                    let module = found
                        .get(1)
                        .or_else(|| found.get(2))
                        .map_or("", |m| m.as_str());
                    let dots = module.chars().take_while(|c| *c == '.').count();
                    let mut base = if dots > 0 {
                        dir.clone()
                    } else {
                        self.root.clone()
                    };
                    for _ in 1..dots {
                        base.pop();
                    }
                    let relative = module[dots..].replace('.', "/");
                    if !relative.is_empty() {
                        add(base.join(format!("{relative}.py")), Why::Import);
                        add(base.join(&relative).join("__init__.py"), Why::Import);
                    }
                } else if let Ok(Some(found)) = JS_IMPORT.captures(line) {
                    let target = dir.join(found.get(1).map_or("", |m| m.as_str()));
                    add(target.clone(), Why::Import);
                    for ext in SCRIPT_EXTENSIONS {
                        add(target.with_extension(ext), Why::Import);
                        add(target.join(format!("index.{ext}")), Why::Import);
                    }
                } else if let Ok(Some(found)) = C_INCLUDE.captures(line) {
                    add(
                        dir.join(found.get(1).map_or("", |m| m.as_str())),
                        Why::Import,
                    );
                }
            }
        }
        let relative = self.shown(file);
        let mut callers: HashMap<String, usize> = HashMap::new();
        for definition in self
            .index
            .definitions_in(&relative)
            .iter()
            .take(CALLER_NAMES)
        {
            for reference in self.index.query(&definition.name, 50).references {
                if reference.path != relative {
                    *callers.entry(reference.path).or_default() += 1;
                }
            }
        }
        let mut ranked: Vec<(String, usize)> = callers.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (path, _) in ranked {
            add(self.root.join(path), Why::Caller);
        }
        found
    }

    fn load(&self) {
        let mut state = lock(&self.state);
        if state.loaded {
            return;
        }
        state.loaded = true;
        if let Some(stored) = self.state_path.as_deref().and_then(|path| {
            let raw = std::fs::read(path).ok()?;
            let stored: StoredLearned = serde_json::from_slice(&raw).ok()?;
            (stored.schema == SCHEMA && stored.learned.share.is_finite()).then_some(stored.learned)
        }) {
            state.learned = stored;
        }
    }

    fn save(&self) {
        let Some(path) = self.state_path.as_deref() else {
            return;
        };
        let learned = lock(&self.state).learned;
        let written = (|| -> std::io::Result<()> {
            let body = serde_json::to_vec(&StoredLearned {
                schema: SCHEMA,
                learned,
            })?;
            let dir = path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(dir)?;
            let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
            std::fs::write(&temp, body)?;
            crate::platform::rename_onto(&temp, path)
        })();
        if let Err(error) = written {
            tracing::debug!(%error, "learned prefetch share not saved");
        }
    }
}
