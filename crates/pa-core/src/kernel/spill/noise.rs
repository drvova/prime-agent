//! Learned per-project noise patterns: line shapes (digits, hex, and spacing
//! normalized) that every run of a tool family prints and nobody reads —
//! build banners, progress bars, test-harness prefixes, timestamps. A preview
//! collapses a run of them into one marker.
//!
//! The estimator is a decaying count per (family, shape): each distinct
//! output a shape appears in adds one, older runs weigh [`DECAY`] less per
//! run. A shape is noise once its count reaches [`PROMOTE_AT`] (about three
//! runs), and never if it was ever seen on, or right beside, a failure line.
//! The set a session previews with is a snapshot taken at its start, so its
//! previews (and the request views built from them) never churn mid-session;
//! what the session learns is saved fire-and-forget for the next one, under
//! `<agent dir>/preview-noise/<project hash>.json`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::preview::is_failure_line;

/// Weight an older run keeps per newer run.
const DECAY: f64 = 0.9;
/// The decayed count at which a shape is noise (three runs: 1 + 0.9 + 0.81).
const PROMOTE_AT: f64 = 2.7;
/// Shapes tracked per family (the weakest are dropped past it).
const MAX_SHAPES: usize = 400;
/// Characters of a shape.
const SHAPE_CHARS: usize = 80;
/// How long a session start waits for the stored patterns.
pub(crate) const LOAD_BUDGET: Duration = Duration::from_millis(50);
const SCHEMA: u64 = 1;
const STATE_DIR: &str = "preview-noise";

static VARIABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"0x[0-9a-fA-F]+|[0-9a-f]{7,}|\d+(?:[.:]\d+)*")
        .expect("the variable pattern compiles")
});

/// A line's shape: numbers and hashes as `#`, spacing collapsed, capped.
pub(crate) fn line_shape(line: &str) -> String {
    let replaced = VARIABLE.replace_all(line.trim(), "#");
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(SHAPE_CHARS).collect()
}

/// The noise shapes one session previews with (frozen at its start).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NoiseSet {
    shapes: HashSet<String>,
}

impl NoiseSet {
    pub(crate) fn contains(&self, shape: &str) -> bool {
        !shape.is_empty() && self.shapes.contains(shape)
    }

    #[cfg(test)]
    pub(crate) fn from_shapes(shapes: &[&str]) -> Self {
        Self {
            shapes: shapes.iter().map(|shape| (*shape).to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct ShapeStats {
    count: f64,
    /// Seen on or beside a failure line: never noise.
    tainted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Learned {
    schema: u64,
    /// Family -> shape -> stats.
    families: BTreeMap<String, BTreeMap<String, ShapeStats>>,
}

impl Learned {
    /// Fold one output of `family` in; returns the shapes it promoted to noise.
    fn observe(&mut self, family: &str, text: &str) -> usize {
        let lines: Vec<&str> = text.lines().collect();
        let failing: Vec<bool> = lines.iter().map(|line| is_failure_line(line)).collect();
        let mut present: HashMap<String, bool> = HashMap::new();
        for (at, line) in lines.iter().enumerate() {
            let shape = line_shape(line);
            if shape.is_empty() {
                continue;
            }
            let near_failure = failing[at]
                || (at > 0 && failing[at - 1])
                || failing.get(at + 1).copied().unwrap_or(false);
            *present.entry(shape).or_default() |= near_failure;
        }
        let shapes = self.families.entry(family.to_string()).or_default();
        let mut promoted = 0;
        for stats in shapes.values_mut() {
            stats.count *= DECAY;
        }
        for (shape, near_failure) in present {
            let stats = shapes.entry(shape).or_default();
            let was_noise = !stats.tainted && stats.count >= PROMOTE_AT;
            stats.count += 1.0;
            stats.tainted |= near_failure;
            if !was_noise && !stats.tainted && stats.count >= PROMOTE_AT {
                promoted += 1;
            }
        }
        if shapes.len() > MAX_SHAPES {
            let mut ranked: Vec<(String, f64)> = shapes
                .iter()
                .map(|(shape, stats)| (shape.clone(), stats.count))
                .collect();
            ranked.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            for (shape, _) in ranked.into_iter().take(shapes.len() - MAX_SHAPES) {
                shapes.remove(&shape);
            }
        }
        promoted
    }

    fn noise(&self, family: &str) -> NoiseSet {
        NoiseSet {
            shapes: self
                .families
                .get(family)
                .map(|shapes| {
                    shapes
                        .iter()
                        .filter(|(_, stats)| !stats.tainted && stats.count >= PROMOTE_AT)
                        .map(|(shape, _)| shape.clone())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// One project's noise learner: the session's frozen snapshot, and the live
/// estimator saved for the next session.
#[derive(Clone)]
pub(crate) struct NoiseLearner {
    inner: Arc<Inner>,
}

struct Inner {
    path: Option<PathBuf>,
    live: Mutex<Learned>,
    snapshot: Mutex<Learned>,
    /// Outputs already folded in (an old result re-previewed by every
    /// request counts once).
    seen: Mutex<HashSet<String>>,
    /// Held across one save, which snapshots the live state inside it.
    saving: Mutex<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl NoiseLearner {
    /// The learner of the project at `project`, stored under `agent_dir`.
    pub(crate) fn for_project(agent_dir: &Path, project: &Path) -> Self {
        let digest = Sha256::digest(project.to_string_lossy().as_bytes());
        let name = digest[..8].iter().fold(String::new(), |mut name, byte| {
            let _ = write!(name, "{byte:02x}");
            name
        });
        Self::at(Some(agent_dir.join(STATE_DIR).join(format!("{name}.json"))))
    }

    fn at(path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                path,
                live: Mutex::new(Learned::default()),
                snapshot: Mutex::new(Learned::default()),
                seen: Mutex::new(HashSet::new()),
                saving: Mutex::new(()),
            }),
        }
    }

    /// Load the stored patterns (waiting at most `budget`) and freeze the
    /// session's snapshot from them.
    pub(crate) async fn load(&self, budget: Duration) {
        if let Some(path) = self.inner.path.clone() {
            let read = tokio::task::spawn_blocking(move || {
                let raw = std::fs::read(path).ok()?;
                let stored: Learned = serde_json::from_slice(&raw).ok()?;
                (stored.schema == SCHEMA).then_some(stored)
            });
            if let Ok(Ok(Some(stored))) = tokio::time::timeout(budget, read).await {
                *lock(&self.inner.live) = stored;
            }
        }
        let live = lock(&self.inner.live).clone();
        *lock(&self.inner.snapshot) = live;
    }

    /// The noise set this session previews `family` with (the start snapshot).
    pub(crate) fn snapshot(&self, family: &str) -> NoiseSet {
        lock(&self.inner.snapshot).noise(family)
    }

    /// Learn from one output of `family` (once per distinct output, keyed by
    /// `id`); returns the patterns it promoted. The save runs on a background
    /// thread; the snapshot is untouched.
    pub(crate) fn observe(&self, family: &str, id: &str, text: &str) -> usize {
        if !lock(&self.inner.seen).insert(format!("{family}\u{0}{id}")) {
            return 0;
        }
        let promoted = lock(&self.inner.live).observe(family, text);
        if self.inner.path.is_some() {
            let inner = Arc::clone(&self.inner);
            let _ = std::thread::Builder::new()
                .name("preview-noise-save".to_string())
                .spawn(move || inner.save());
        }
        promoted
    }
}

impl Inner {
    fn save(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let _saving = lock(&self.saving);
        let mut stored = lock(&self.live).clone();
        stored.schema = SCHEMA;
        let written = (|| -> std::io::Result<()> {
            let body = serde_json::to_vec(&stored)?;
            let dir = path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(dir)?;
            let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
            std::fs::write(&temp, body)?;
            crate::platform::rename_onto(&temp, path)
        })();
        if let Err(error) = written {
            tracing::debug!(%error, "preview noise patterns not saved");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(build: u32, failing: bool) -> String {
        let mut text = format!(
            "   Compiling serde v1.0.{build}\n[2026-11-20T10:00:{build:02}Z] started\ntest result: ok. {build} passed\n"
        );
        if failing {
            text.push_str("thread 'x' panicked at src/lib.rs:3:5:\ntest result: ok. 1 passed\n");
        }
        text
    }

    /// A shape is noise only after it shows up in three runs, and a shape
    /// ever seen right beside a failure line never is.
    #[test]
    fn noise_is_learned_after_three_runs_unless_it_sits_beside_a_failure() {
        let mut learned = Learned::default();
        let mut after = Vec::new();
        for build in 0..4 {
            // The third run's panic sits right above a `test result` line.
            learned.observe("cargo", &run(build, build == 2));
            let mut shapes: Vec<String> = learned.noise("cargo").shapes.into_iter().collect();
            shapes.sort();
            after.push(shapes);
        }
        let banner = "Compiling serde v#".to_string();
        let stamp = "[#-#-#T#Z] started".to_string();
        assert_eq!(
            (after, learned.noise("pytest")),
            (
                vec![
                    vec![],
                    vec![],
                    vec![banner.clone(), stamp.clone()],
                    vec![banner, stamp]
                ],
                NoiseSet::default(),
            )
        );
    }

    /// A session previews with the set stored at its start, however much it
    /// learns; the next session starts from what this one learned.
    #[tokio::test]
    async fn the_session_snapshot_is_stable_and_the_next_session_inherits() {
        let agent = tempfile::tempdir().expect("agent dir");
        let project = Path::new("/work/app");
        let first = NoiseLearner::for_project(agent.path(), project);
        first.load(LOAD_BUDGET).await;
        let promoted: usize = (0..3)
            .map(|build| first.observe("cargo", &format!("run-{build}"), &run(build, false)))
            .sum();
        // Folding the same output again counts nothing.
        let again = first.observe("cargo", "run-2", &run(2, false));
        let path = first.inner.path.clone().expect("a stored learner");
        // Wait for the background save to land (observable readiness, bounded).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let saved = std::fs::read(&path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<Learned>(&raw).ok())
                .is_some_and(|stored| stored.noise("cargo").contains("Compiling serde v#"));
            if saved || std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let next = NoiseLearner::for_project(agent.path(), project);
        next.load(Duration::from_secs(5)).await;
        assert_eq!(
            (
                promoted,
                again,
                first.snapshot("cargo"),
                next.snapshot("cargo").contains("Compiling serde v#"),
            ),
            (3, 0, NoiseSet::default(), true)
        );
    }

    /// A stored set the disk never delivers (a hung mount) never holds the
    /// session start: the load gives up at its budget with an empty snapshot.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hanging_store_never_holds_the_session_start() {
        let agent = tempfile::tempdir().expect("agent dir");
        let learner = NoiseLearner::for_project(agent.path(), Path::new("/work/app"));
        let path = learner.inner.path.clone().expect("a stored learner");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).expect("fifo");
        tokio::time::timeout(Duration::from_secs(5), learner.load(LOAD_BUDGET))
            .await
            .expect("the load returned at its budget");
        assert_eq!(learner.snapshot("cargo"), NoiseSet::default());
        // Release the blocked reader so its thread ends.
        let _ = std::fs::OpenOptions::new().write(true).open(&path);
    }
}
