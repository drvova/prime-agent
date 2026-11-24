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
//!
//! The estimator belongs to the project, not to a session: the sessions of
//! one project hosted in this process share one (as they share its file), so
//! none of them saves over what another learned. Each keeps its own start
//! snapshot.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::time::Duration;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::lock;
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

/// One session's view of its project's noise learner: the snapshot frozen at
/// the session's start, and the project's live estimator, which every
/// session of the project in this process feeds and saves.
pub(crate) struct NoiseLearner {
    project: Arc<Project>,
    snapshot: Learned,
}

/// One project's live estimator, shared by its sessions in this process.
struct Project {
    path: PathBuf,
    live: Mutex<Learned>,
    /// Set once the stored patterns were read (or found absent or
    /// unreadable). Until then nothing is saved: a store that did not answer
    /// in time is never overwritten by the little one session learned.
    read: tokio::sync::OnceCell<()>,
    /// Outputs already folded in (an old result re-previewed by every
    /// request counts once).
    seen: Mutex<HashSet<String>>,
    /// Held across one save, which snapshots the live state inside it.
    saving: Mutex<()>,
}

impl NoiseLearner {
    /// The learner of the project at `project`, stored under `agent_dir`.
    /// Every call for one project in this process shares one estimator. Its
    /// snapshot is empty until [`Self::load`].
    pub(crate) fn for_project(agent_dir: &Path, project: &Path) -> Self {
        static PROJECTS: OnceLock<Mutex<HashMap<PathBuf, Weak<Project>>>> = OnceLock::new();
        let digest = Sha256::digest(project.to_string_lossy().as_bytes());
        let name = digest[..8].iter().fold(String::new(), |mut name, byte| {
            let _ = write!(name, "{byte:02x}");
            name
        });
        let path = agent_dir.join(STATE_DIR).join(format!("{name}.json"));
        let mut projects = lock(PROJECTS.get_or_init(|| Mutex::new(HashMap::new())));
        let project = if let Some(project) = projects.get(&path).and_then(Weak::upgrade) {
            project
        } else {
            projects.retain(|_, project| project.strong_count() > 0);
            let project = Arc::new(Project {
                path: path.clone(),
                live: Mutex::new(Learned::default()),
                read: tokio::sync::OnceCell::new(),
                seen: Mutex::new(HashSet::new()),
                saving: Mutex::new(()),
            });
            projects.insert(path, Arc::downgrade(&project));
            project
        };
        Self {
            project,
            snapshot: Learned::default(),
        }
    }

    /// Freeze the session's snapshot from the project's patterns, reading
    /// the stored ones first when no session of the project in this process
    /// has (waiting at most `budget`). A read that outlives the budget is
    /// left behind (its thread ends when the disk answers) and the next
    /// session to start asks again.
    pub(crate) async fn load(&mut self, budget: Duration) {
        let project = &self.project;
        let read = project.read.get_or_init(|| async {
            let path = project.path.clone();
            let stored = tokio::task::spawn_blocking(move || {
                let raw = std::fs::read(path).ok()?;
                let stored: Learned = serde_json::from_slice(&raw).ok()?;
                (stored.schema == SCHEMA).then_some(stored)
            })
            .await;
            // What a session learned while the store did not answer was
            // never saved: the stored patterns replace it.
            if let Ok(Some(stored)) = stored {
                *lock(&project.live) = stored;
            }
        });
        if tokio::time::timeout(budget, read).await.is_err() {
            tracing::debug!("preview noise patterns not read within the session-start budget");
        }
        self.snapshot = lock(&project.live).clone();
    }

    /// The noise set this session previews `family` with (the start snapshot).
    pub(crate) fn snapshot(&self, family: &str) -> NoiseSet {
        self.snapshot.noise(family)
    }

    /// Learn from one output of `family` (once per distinct output, keyed by
    /// `id`); returns the patterns it promoted. The save runs on a background
    /// thread; the snapshot is untouched.
    pub(crate) fn observe(&self, family: &str, id: &str, text: &str) -> usize {
        if !lock(&self.project.seen).insert(format!("{family}\u{0}{id}")) {
            return 0;
        }
        let promoted = lock(&self.project.live).observe(family, text);
        if self.project.read.initialized() {
            let project = Arc::clone(&self.project);
            let spawned = std::thread::Builder::new()
                .name("preview-noise-save".to_string())
                .spawn(pa_types::session_scope::propagate(move || {
                    let _saving = lock(&project.saving);
                    let mut stored = lock(&project.live).clone();
                    stored.schema = SCHEMA;
                    let written = (|| -> std::io::Result<()> {
                        // What is left of an ended session lifetime writes nothing.
                        pa_types::session_scope::ensure_live()?;
                        let body = serde_json::to_vec(&stored)?;
                        let dir = project.path.parent().unwrap_or_else(|| Path::new("."));
                        std::fs::create_dir_all(dir)?;
                        pa_types::platform::replace_file(
                            &project.path,
                            &body,
                            crate::platform::REPLACE_KEEPING_MODE,
                        )
                    })();
                    if let Err(error) = written {
                        tracing::debug!(%error, "preview noise patterns not saved");
                    }
                }));
            if let Err(error) = spawned {
                tracing::warn!(%error, "preview noise save thread did not start");
            }
        }
        promoted
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

    /// A session previews with the set known at its start, however much it
    /// learns. A session of the same project starting beside it starts from
    /// what the first has learned (they share one estimator), and so does one
    /// starting after both are gone (it reads what they saved).
    #[tokio::test]
    async fn the_session_snapshot_is_stable_and_later_sessions_inherit() {
        let agent = tempfile::tempdir().expect("agent dir");
        let project = Path::new("/work/app");
        let mut first = NoiseLearner::for_project(agent.path(), project);
        first.load(LOAD_BUDGET).await;
        let promoted: usize = (0..3)
            .map(|build| first.observe("cargo", &format!("run-{build}"), &run(build, false)))
            .sum();
        let mut beside = NoiseLearner::for_project(agent.path(), project);
        beside.load(LOAD_BUDGET).await;
        // Folding the same output again counts nothing, whichever session does.
        let again = beside.observe("cargo", "run-2", &run(2, false));
        let unchanged = first.snapshot("cargo");
        let shared = beside.snapshot("cargo").contains("Compiling serde v#");
        drop(beside);
        // Wait for the background saves to land and their threads to let go
        // of the estimator (observable readiness, bounded).
        let path = first.project.path.clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let saved = std::fs::read(&path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<Learned>(&raw).ok())
                .is_some_and(|stored| stored.noise("cargo").contains("Compiling serde v#"));
            if (saved && Arc::strong_count(&first.project) == 1)
                || std::time::Instant::now() > deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(first);
        let mut next = NoiseLearner::for_project(agent.path(), project);
        let from_disk = !next.project.read.initialized();
        next.load(Duration::from_secs(5)).await;
        assert_eq!(
            (
                promoted,
                again,
                unchanged,
                shared,
                from_disk,
                next.snapshot("cargo").contains("Compiling serde v#"),
            ),
            (3, 0, NoiseSet::default(), true, true, true)
        );
    }

    /// A stored set the disk never delivers (a hung mount) never holds the
    /// session start: the load gives up at its budget with an empty snapshot,
    /// and what the session then learns is not saved over the store it could
    /// not read.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hanging_store_never_holds_the_session_start() {
        let agent = tempfile::tempdir().expect("agent dir");
        let mut learner = NoiseLearner::for_project(agent.path(), Path::new("/work/app"));
        let path = learner.project.path.clone();
        std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).expect("fifo");
        tokio::time::timeout(Duration::from_secs(5), learner.load(LOAD_BUDGET))
            .await
            .expect("the load returned at its budget");
        learner.observe("cargo", "run-0", &run(0, false));
        assert_eq!(
            (
                learner.snapshot("cargo"),
                learner.project.read.initialized(),
                Arc::strong_count(&learner.project),
            ),
            (NoiseSet::default(), false, 1)
        );
        // Release the blocked reader so its thread ends.
        let _ = std::fs::OpenOptions::new().write(true).open(&path);
    }
}
