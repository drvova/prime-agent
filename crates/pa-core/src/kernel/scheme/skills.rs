//! How a skill contributes executable code to a session's Scheme kernel.
//!
//! A skill is a directory with a `SKILL.md`. It may also carry Scheme
//! source: a `skill.scm` beside the `SKILL.md`, and `*.scm` files in a
//! `scheme/` directory beside it. A skill with neither is documentation
//! only and is left alone. The `scheme/` files load first, in file-name
//! order, then `skill.scm`; each file is one program evaluated in the
//! session's engine, so what it defines is global to the session, next to
//! the host API (a skill names its functions after itself:
//! `websearch-search`).
//!
//! Sources load when the kernel starts and again after a cell that called
//! `(rlm-reload)`: a reload evaluates the files that are new or changed
//! since they were last evaluated. A file that fails to read, parse or
//! evaluate makes its skill unavailable: the skill's remaining files are
//! skipped, the other skills still load, and the failure is reported per
//! skill, once per start or reload, through the session's
//! [`UnavailableSchemeSkillsCallback`] (the model is told before its next
//! turn) and at any time by `(session-info "skills-unavailable")`. What a failed file
//! defined before it failed stays defined, and so does everything a
//! removed skill defined: the engine cannot take a definition back.
//!
//! Skills discovered in the project's own directory are never loaded: code
//! a repository ships must not run in the session by being checked out.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use pa_types::session::CustomMessage;
use serde_json::json;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::skills::{Skill, SourceScope};

#[cfg(test)]
mod tests;

/// The custom row type of the unavailable-skills notice.
pub const SCHEME_SKILLS_UNAVAILABLE_CUSTOM_TYPE: &str = "scheme_skills_unavailable";

/// One skill that may carry Scheme source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSource {
    pub name: String,
    /// The directory holding the skill's `SKILL.md`.
    pub dir: PathBuf,
}

impl SkillSource {
    /// The skills whose Scheme source a session loads: every discovered
    /// skill except those the project itself ships.
    #[must_use]
    pub fn from_skills(skills: &[Skill]) -> Vec<SkillSource> {
        skills
            .iter()
            .filter(|skill| skill.source_info.scope != SourceScope::Project)
            .map(|skill| SkillSource {
                name: skill.name.clone(),
                dir: skill.base_dir.clone(),
            })
            .collect()
    }
}

/// The skills whose Scheme source failed to load, in skill order: skill
/// name and `<file>: <error>`.
pub type UnavailableSchemeSkills = Vec<(String, String)>;

/// Names the session's current skills each time sources are loaded, so a
/// reload sees skills discovered since the kernel started.
pub type SkillSourcesProvider = Arc<dyn Fn() -> Vec<SkillSource> + Send + Sync>;

/// Told which skills failed to load, once per kernel start or reload that
/// leaves any unavailable.
pub type UnavailableSchemeSkillsCallback = Arc<dyn Fn(&UnavailableSchemeSkills) + Send + Sync>;

/// The notice text: the `[scheme-skills-unavailable]` header, the
/// per-skill load errors, and the fix hint.
#[must_use]
pub fn notice_content(errors: &UnavailableSchemeSkills) -> String {
    let mut lines = vec![
        "[scheme-skills-unavailable]".to_string(),
        String::new(),
        "These skills' Scheme source failed to load into the Scheme kernel, so the functions they define are missing or incomplete:".to_string(),
    ];
    for (name, error) in errors {
        lines.push(format!("- {name}: {error}"));
    }
    lines.push(String::new());
    lines.push(
        "Fix the source and call `(rlm-reload)` to load it again, or use another approach. `(session-info \"skills-unavailable\")` lists what is still failing; each skill's SKILL.md stays readable.".to_string(),
    );
    lines.join("\n")
}

/// The next-turn notice row: shown, with `details.skills` naming the skills
/// that failed to load.
#[must_use]
pub fn notice_message(errors: &UnavailableSchemeSkills) -> CustomMessage {
    CustomMessage {
        custom_type: SCHEME_SKILLS_UNAVAILABLE_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(notice_content(errors)),
        display: true,
        details: Some(json!({
            "skills": errors.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>(),
        })),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64),
        rest: serde_json::Map::default(),
    }
}

/// One source file as it was last evaluated.
struct Evaluated {
    path: PathBuf,
    text: String,
    /// Why it failed, when it did.
    error: Option<String>,
}

/// Loads a session's skill sources into its engine and remembers what
/// failed.
pub(super) struct SkillLoader {
    sources: Option<SkillSourcesProvider>,
    on_unavailable: Option<UnavailableSchemeSkillsCallback>,
    reload_requested: Arc<AtomicBool>,
    evaluated: Mutex<Vec<Evaluated>>,
    unavailable: Arc<Mutex<UnavailableSchemeSkills>>,
    /// How long one source may take to evaluate before it is stopped.
    load_limit: std::time::Duration,
    /// The functions the loaded sources define, each with its skill: a cell
    /// that names one uses the skill.
    functions: Mutex<BTreeMap<String, String>>,
    /// Counts those uses.
    reuse: Option<crate::reuse::Recorder>,
}

/// Whether `letter` ends a Scheme identifier.
fn ends_identifier(letter: char) -> bool {
    letter.is_whitespace() || "()[]{}\"';`,".contains(letter)
}

/// The names `text` defines with `(define name ...)` or `(define (name
/// ...) ...)`, internal `%` names left out.
fn defined_names(text: &str) -> impl Iterator<Item = String> + '_ {
    text.match_indices("(define").filter_map(|(at, opening)| {
        // `(define-syntax ...)` and its kin define no function.
        let rest = text[at + opening.len()..].strip_prefix(char::is_whitespace)?;
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('(').unwrap_or(rest);
        let name: String = rest
            .chars()
            .take_while(|letter| !ends_identifier(*letter))
            .collect();
        (!name.is_empty() && !name.starts_with('%')).then_some(name)
    })
}

/// How long one skill source may take to evaluate before it is stopped.
const LOAD_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Overrides [`LOAD_LIMIT`] in milliseconds.
const LOAD_LIMIT_ENV: &str = "PRIME_AGENT_SKILL_LOAD_LIMIT_MS";

impl SkillLoader {
    /// A loader for the skills `sources` names. `handlers`' `rlm.reload`
    /// request, when it has one, is wrapped so a successful reload asks for
    /// the sources to be loaded again after the cell that called it.
    pub(super) fn new(
        sources: Option<SkillSourcesProvider>,
        on_unavailable: Option<UnavailableSchemeSkillsCallback>,
        handlers: &mut HostRequestHandlers,
        reuse: Option<crate::reuse::Recorder>,
    ) -> Self {
        let reload_requested = Arc::new(AtomicBool::new(false));
        if let Some(reload) = handlers.get("rlm.reload").cloned() {
            let requested = Arc::clone(&reload_requested);
            handlers.register(
                "rlm.reload",
                host_handler(move |payload: HostRequestPayload| {
                    let (reload, requested) = (reload.clone(), Arc::clone(&requested));
                    async move {
                        let reply = reload(payload).await?;
                        requested.store(true, Ordering::SeqCst);
                        Ok(reply)
                    }
                }),
            );
        }
        SkillLoader {
            sources,
            on_unavailable,
            reload_requested,
            evaluated: Mutex::new(Vec::new()),
            unavailable: Arc::new(Mutex::new(Vec::new())),
            // Read here, on the session's own thread.
            load_limit: pa_types::session_scope::var(LOAD_LIMIT_ENV)
                .ok()
                .and_then(|raw| raw.parse().ok())
                .map_or(LOAD_LIMIT, std::time::Duration::from_millis),
            functions: Mutex::new(BTreeMap::new()),
            reuse,
        }
    }

    /// Count a use of each skill whose functions `code` names, before the
    /// cell runs. A ledger that cannot be written is logged.
    pub(super) fn note_cell(&self, code: &str) {
        let Some(reuse) = &self.reuse else {
            return;
        };
        let used: BTreeSet<String> = {
            let functions = self
                .functions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if functions.is_empty() {
                return;
            }
            code.split(ends_identifier)
                .filter_map(|word| functions.get(word).cloned())
                .collect()
        };
        let keys: Vec<crate::reuse::EntryKey> = used
            .into_iter()
            .map(crate::reuse::EntryKey::Skill)
            .collect();
        if let Err(error) = reuse.record(crate::reuse::Event::Used, &keys) {
            tracing::warn!("the reuse ledger was not updated: {error:#}");
        }
    }

    /// Define `%skills-unavailable-json` (what `(session-info
    /// "skills-unavailable")` reads) in the engine and load every skill's
    /// source: the kernel's start. A skill's failure is reported, and the
    /// kernel starts.
    pub(super) fn start(&self, engine: &mut Engine) {
        let unavailable = Arc::clone(&self.unavailable);
        engine.register_fn("%skills-unavailable-json", move || -> String {
            let rows: Vec<serde_json::Value> = unavailable
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|(skill, error)| json!({"skill": skill, "error": error}))
                .collect();
            json!(rows).to_string()
        });
        self.load(engine);
    }

    /// Load the sources again when a reload asked for it since the last
    /// call: after the cell that called `(rlm-reload)`.
    pub(super) fn load_if_reloaded(&self, engine: &mut Engine) {
        if self.reload_requested.swap(false, Ordering::SeqCst) {
            self.load(engine);
        }
    }

    /// Evaluate every skill source that is new or changed since it was
    /// last evaluated, and report the skills left unavailable.
    fn load(&self, engine: &mut Engine) {
        let Some(sources) = &self.sources else {
            return;
        };
        let mut evaluated = self
            .evaluated
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut unavailable: UnavailableSchemeSkills = Vec::new();
        let mut functions: BTreeMap<String, String> = BTreeMap::new();
        // What a source displays while loading belongs to no cell: it goes
        // to a port nothing reads (never the process's standard output), and
        // the next cell installs its own as every cell does.
        if let Err(error) = engine.run("(current-output-port (open-output-string))") {
            tracing::warn!("the skill loader's output port could not be replaced: {error}");
        }
        for skill in sources() {
            for path in crate::skills::scheme_source_files(&skill.dir) {
                let shown = path
                    .strip_prefix(&skill.dir)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(error) => {
                        unavailable.push((skill.name.clone(), format!("{shown}: {error}")));
                        break;
                    }
                };
                let names: Vec<String> = defined_names(&text).collect();
                let known = evaluated.iter().position(|known| known.path == path);
                let error =
                    if let Some(unchanged) = known.filter(|known| evaluated[*known].text == text) {
                        evaluated[unchanged].error.clone()
                    } else {
                        // No cell is running, so nothing else would stop a
                        // source that never returns: it is interrupted at
                        // its limit, and again until it has left the engine.
                        let controllers = super::confinement::controllers(engine);
                        let interrupted = controllers.clone();
                        let limit = self.load_limit;
                        let engine_thread = std::thread::current();
                        let error = std::thread::scope(|scope| {
                            let (loaded, loading) = std::sync::mpsc::channel::<()>();
                            scope.spawn(move || {
                                let mut wait = limit;
                                while loading.recv_timeout(wait)
                                    == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                                {
                                    super::cell::interrupt_engine(&interrupted, &engine_thread);
                                    wait = std::time::Duration::from_millis(20);
                                }
                            });
                            let error = match super::limits::source_refusal(&text) {
                                Some(refusal) => Some(refusal),
                                None => super::contracts::load(engine, &text)
                                    .err()
                                    .map(|error| error.to_string()),
                            };
                            drop(loaded);
                            error
                        });
                        for controller in controllers {
                            controller.resume();
                        }
                        let record = Evaluated {
                            path,
                            text,
                            error: error.clone(),
                        };
                        match known {
                            Some(known) => evaluated[known] = record,
                            None => evaluated.push(record),
                        }
                        error
                    };
                for name in names {
                    functions.entry(name).or_insert_with(|| skill.name.clone());
                }
                if let Some(error) = error {
                    // The skill's later files build on this one.
                    unavailable.push((skill.name.clone(), format!("{shown}: {error}")));
                    break;
                }
            }
        }
        *self
            .functions
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = functions;
        let changed = {
            let mut current = self
                .unavailable
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let changed = *current != unavailable;
            current.clone_from(&unavailable);
            changed
        };
        if let Some(report) = self
            .on_unavailable
            .as_ref()
            .filter(|_| changed && !unavailable.is_empty())
        {
            report(&unavailable);
        }
    }
}
