//! The continual harness store from a cell: the `harness-*` functions.
//!
//! Memories, prompt notes, skills, subagent specs, and refinement events
//! live in `harness/harness_state.json`: the session-local store under the
//! session's artifact dir, or the cross-session one under the agent dir
//! (`#:global #t`, or an id written `global:<id>`). The file is the one
//! `/refine` and the Python kernel's `rlm.harness` write, and it is read and
//! written here through [`crate::refinement`]'s own loader and locked
//! writer, so every writer agrees on its format and its lock.
//!
//! A factory entry is written only while the `factory.enabled` setting is
//! on, and only with a spec (`arguments.dag` or `arguments.machine`) that
//! passes the factory's write-time validator; deleting one stays possible
//! while the factory is off.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use crate::refinement::ranking::{
    harness_query_term_idf, is_cjk, score_harness_entry_for_query, HarnessQueryTerms,
};
use crate::refinement::{
    get_global_harness_state_dir, get_harness_state_path, get_local_harness_state_dir,
    load_harness_state, try_update_harness_state, HarnessEntry, HarnessRefinementEvent,
    HarnessScope, HarnessState, RefinementKind,
};
use crate::session::manager::format_iso_now;

/// The kinds in the order the overview lists them.
const KINDS: [(RefinementKind, &str); 5] = [
    (RefinementKind::Prompt, "prompt"),
    (RefinementKind::Memory, "memory"),
    (RefinementKind::Skill, "skill"),
    (RefinementKind::Subagent, "subagent"),
    (RefinementKind::Factory, "factory"),
];

/// A delete that named no stored entry: the store is left as it was.
#[derive(Debug)]
struct NoSuchEntry;

impl std::fmt::Display for NoSuchEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("no such entry")
    }
}

impl std::error::Error for NoSuchEntry {}

/// How a value reads in a rejection: what it is, not what it holds.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "nothing",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(text) if text.is_empty() => "an empty string",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "a hash",
    }
}

/// The non-empty string in `field` of a write, or the rejection naming the
/// entry and the field.
fn required_text<'a>(
    kind: &str,
    entry: &str,
    field: &str,
    value: &'a Value,
) -> anyhow::Result<&'a str> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{kind} entry '{entry}' rejected: {field} must be a non-empty string, got {}",
                type_name(value)
            )
        })
}

/// The hash in `field` of a write when one was given, or the rejection.
fn optional_record(
    kind: &str,
    entry: &str,
    field: &str,
    value: &Value,
) -> anyhow::Result<Option<Map<String, Value>>> {
    match value {
        Value::Null => Ok(None),
        Value::Object(fields) => Ok(Some(fields.clone())),
        Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => Err(anyhow!(
            "{kind} entry '{entry}' rejected: {field} must be a hash when provided, got {}",
            type_name(value)
        )),
    }
}

/// `text` cut to `limit` characters, the last three of them an ellipsis.
fn clipped(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit - 3).collect();
    format!("{kept}...")
}

/// One session's harness stores.
struct Harness {
    agent_dir: Option<PathBuf>,
    artifact_dir: Option<PathBuf>,
}

impl Harness {
    /// The store a call addresses: its directory (`None`: this session has
    /// no local store) and scope. An id written `global:<id>` or
    /// `local:<id>`, as the overview shows it, is returned bare, and its
    /// `global:` prefix selects the global store.
    fn store<'a>(
        &self,
        options: &'a Value,
    ) -> anyhow::Result<(Option<PathBuf>, HarnessScope, Option<&'a str>)> {
        let mut global = options.get("global").and_then(Value::as_bool) == Some(true);
        let id = options
            .get("id")
            .and_then(Value::as_str)
            .map(|id| match id.split_once(':') {
                Some(("global", rest)) if !rest.is_empty() => {
                    global = true;
                    rest
                }
                Some(("local", rest)) if !rest.is_empty() => rest,
                Some(_) | None => id,
            });
        if !global {
            return Ok((
                get_local_harness_state_dir(self.artifact_dir.as_deref()),
                HarnessScope::Local,
                id,
            ));
        }
        let agent_dir = self.agent_dir.as_deref().ok_or_else(|| {
            anyhow!("this kernel was started without the session's agent dir, where the global harness store lives")
        })?;
        Ok((
            Some(get_global_harness_state_dir(agent_dir)),
            HarnessScope::Global,
            id,
        ))
    }

    /// The kind `name` names.
    fn kind_named(name: &Value) -> anyhow::Result<RefinementKind> {
        KINDS
            .iter()
            .find(|(_, known)| name == *known)
            .map(|(kind, _)| *kind)
            .ok_or_else(|| {
                anyhow!("unknown harness kind {name}; expected one of prompt, memory, skill, subagent, factory")
            })
    }

    /// The store a write addresses: an unsaved session has no local one.
    fn writable(dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
        dir.ok_or_else(|| {
            anyhow!(
                "this session is not saved, so it has no local harness store; pass #:global #t to persist across sessions"
            )
        })
    }

    /// Every entry of `kind` (or of every kind), ordered by kind, path,
    /// title, then id.
    fn listed(state: &HarnessState, kind: Option<RefinementKind>) -> Vec<HarnessEntry> {
        let mut entries: Vec<HarnessEntry> = KINDS
            .iter()
            .filter(|(known, _)| kind.is_none_or(|wanted| wanted == *known))
            .flat_map(|(known, _)| state.entries[known].values().cloned())
            .collect();
        entries.sort_by(|left, right| {
            (left.kind, &left.path, &left.title, &left.id).cmp(&(
                right.kind,
                &right.path,
                &right.title,
                &right.id,
            ))
        });
        entries
    }

    /// Create or update one entry: `{action, kind, title, content, id?,
    /// path?, reference?, arguments?, metadata?, global?}`. A field an
    /// update omits keeps its stored value.
    fn write(&self, options: &Value) -> anyhow::Result<Value> {
        let (dir, scope, given_id) = self.store(options)?;
        let dir = Self::writable(dir)?;
        let creating = options["action"] == "create";
        let (kind, kind_name) = match options["kind"].as_str() {
            Some("prompt") => (RefinementKind::Prompt, "prompt"),
            Some("memory") => (RefinementKind::Memory, "memory"),
            Some("skill") => (RefinementKind::Skill, "skill"),
            Some("subagent") => (RefinementKind::Subagent, "subagent"),
            Some("factory") => (RefinementKind::Factory, "factory"),
            Some(_) | None => bail!(
                "unknown harness kind {}; expected one of prompt, memory, skill, subagent, factory",
                options["kind"]
            ),
        };
        // The opt-in gate comes before any spec work: while the factory is
        // off every factory write refuses with the one message.
        if kind == RefinementKind::Factory
            && !self
                .agent_dir
                .as_deref()
                .is_some_and(crate::refinement::factory_enabled)
        {
            bail!(crate::refinement::FACTORY_DISABLED_MESSAGE);
        }
        let shown = given_id
            .filter(|id| !id.is_empty())
            .or_else(|| options["title"].as_str().filter(|title| !title.is_empty()))
            .unwrap_or("<unnamed>");
        let title = required_text(kind_name, shown, "title", &options["title"])?;
        let id = if options["id"].is_null() {
            // An entry created without an id is named after its title.
            let slug: Vec<String> = title
                .trim()
                .split(|letter: char| !letter.is_alphanumeric())
                .filter(|part| !part.is_empty())
                .map(str::to_lowercase)
                .collect();
            let slug: String = slug.join("_").chars().take(80).collect();
            if slug.is_empty() {
                kind_name.to_string()
            } else {
                slug
            }
        } else {
            required_text(kind_name, shown, "id", &options["id"])?;
            given_id.unwrap_or_default().to_string()
        };
        let content = required_text(kind_name, &id, "content", &options["content"])?;
        let path = if options["path"].is_null() {
            None
        } else {
            Some(required_text(kind_name, &id, "path", &options["path"])?)
        };
        let reference = optional_record(kind_name, &id, "reference", &options["reference"])?;
        let arguments = optional_record(kind_name, &id, "arguments", &options["arguments"])?;
        let metadata = optional_record(kind_name, &id, "metadata", &options["metadata"])?;
        if let Some(reference) = reference.as_ref().filter(|_| kind == RefinementKind::Skill) {
            let named = |keys: [&str; 2]| {
                keys.iter().any(|key| {
                    reference
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                })
            };
            let problem = if reference.get("type").and_then(Value::as_str) != Some("python") {
                Some("skill reference.type must be 'python'")
            } else if !named(["import", "python_import"]) {
                Some("skill reference requires a Python import")
            } else if !named(["callable", "call_pattern"]) {
                Some("skill reference requires a callable or call_pattern")
            } else {
                None
            };
            if let Some(problem) = problem {
                bail!("skill entry '{id}' rejected: {problem}");
            }
        }
        if kind == RefinementKind::Factory {
            // An invalid spec never reaches the store through any writer. A
            // new factory needs its spec; an update that omits the
            // arguments keeps the stored one.
            let spec = arguments.as_ref().map(|arguments| {
                let given = |key: &str| arguments.get(key).filter(|spec| !spec.is_null());
                (given("dag"), given("machine"))
            });
            let problem = match spec {
                None if creating => {
                    Some("factory entries require a dag or machine object in arguments".to_string())
                }
                None => None,
                Some((Some(_), Some(_))) => {
                    Some("pass either dag or machine, not both".to_string())
                }
                Some((dag, machine)) => match machine.or(dag).filter(|spec| spec.is_object()) {
                    None => Some(
                        "factory entries require a dag or machine object in arguments".to_string(),
                    ),
                    Some(spec) => {
                        Some(crate::kernel::scheme::validate_factory_spec(spec).join("; "))
                            .filter(|errors| !errors.is_empty())
                    }
                },
            };
            if let Some(problem) = problem {
                bail!("factory entry '{id}' rejected: {problem}");
            }
        }
        let (entry, _) = try_update_harness_state(&dir, scope, |state| {
            let records = state.entries.entry(kind).or_default();
            let entry = match (records.get(&id), creating) {
                (Some(_), true) => bail!("{kind_name} entry '{id}' already exists"),
                (None, false) => bail!("{kind_name} entry '{id}' does not exist"),
                (Some(stored), false) => HarnessEntry {
                    title: title.to_string(),
                    content: content.to_string(),
                    path: path.map_or_else(|| stored.path.clone(), str::to_string),
                    reference: reference.unwrap_or_else(|| stored.reference.clone()),
                    arguments: arguments.unwrap_or_else(|| stored.arguments.clone()),
                    metadata: metadata.unwrap_or_else(|| stored.metadata.clone()),
                    source: "agent".to_string(),
                    updated_at: format_iso_now(),
                    version: stored.version + 1,
                    ..stored.clone()
                },
                (None, true) => {
                    if kind == RefinementKind::Skill && reference.is_none() {
                        bail!(
                            "skill entry '{id}' rejected: skill entries require a Python reference"
                        );
                    }
                    let now = format_iso_now();
                    HarnessEntry {
                        id: id.clone(),
                        kind,
                        title: title.to_string(),
                        content: content.to_string(),
                        path: path.unwrap_or("general").to_string(),
                        scope: Some(scope),
                        reference: reference.unwrap_or_default(),
                        arguments: arguments.unwrap_or_default(),
                        metadata: metadata.unwrap_or_default(),
                        source: "agent".to_string(),
                        created_at: now.clone(),
                        updated_at: now,
                        version: 1,
                    }
                }
            };
            records.insert(id.clone(), entry.clone());
            Ok(entry)
        })?;
        Ok(serde_json::to_value(entry)?)
    }

    /// Delete one entry: `{kind, id, global?}`; whether it was stored.
    fn delete(&self, options: &Value) -> anyhow::Result<Value> {
        let (dir, scope, id) = self.store(options)?;
        let dir = Self::writable(dir)?;
        let id = id.unwrap_or_default();
        let kind = Self::kind_named(&options["kind"])?;
        let removed = try_update_harness_state(&dir, scope, |state| {
            state
                .entries
                .get_mut(&kind)
                .and_then(|records| records.remove(id))
                .map(|_| ())
                .ok_or_else(|| anyhow!(NoSuchEntry))
        });
        match removed {
            Ok(_) => Ok(json!(true)),
            Err(error) if error.is::<NoSuchEntry>() => Ok(json!(false)),
            Err(error) => Err(error),
        }
    }

    /// Record one refinement pass: `{trigger, changes, evidence, outcome,
    /// id?, global?}`; the stored event.
    fn record_refinement(&self, options: &Value) -> anyhow::Result<Value> {
        let (dir, scope, id) = self.store(options)?;
        let dir = Self::writable(dir)?;
        let trigger = options["trigger"]
            .as_str()
            .filter(|trigger| !trigger.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "refinement event rejected: trigger must be a non-empty string, got {}",
                    type_name(&options["trigger"])
                )
            })?;
        let changes: Vec<String> = match &options["changes"] {
            Value::String(change) if !change.is_empty() => vec![change.clone()],
            Value::Array(changes)
                if changes
                    .iter()
                    .all(|change| change.as_str().is_some_and(|text| !text.is_empty())) =>
            {
                changes
                    .iter()
                    .filter_map(|change| change.as_str().map(str::to_string))
                    .collect()
            }
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)
            | Value::Object(_) => bail!(
                "refinement event rejected: changes must be a non-empty string or a list of non-empty strings, got {}",
                type_name(&options["changes"])
            ),
        };
        let text = |field: &str| {
            options[field].as_str().map(str::to_string).ok_or_else(|| {
                anyhow!(
                    "refinement event rejected: {field} must be a string when provided, got {}",
                    type_name(&options[field])
                )
            })
        };
        let (evidence, outcome) = (text("evidence")?, text("outcome")?);
        if !options["id"].is_null() && id.is_none_or(str::is_empty) {
            bail!(
                "refinement event rejected: id must be a non-empty string when provided, got {}",
                type_name(&options["id"])
            );
        }
        let (event, _) = try_update_harness_state(&dir, scope, |state| {
            let event = HarnessRefinementEvent {
                id: id.map_or_else(
                    || format!("refine_{:04}", state.refinements.len() + 1),
                    str::to_string,
                ),
                trigger: trigger.to_string(),
                changes,
                evidence,
                outcome,
                created_at: format_iso_now(),
            };
            state.refinements.push(event.clone());
            Ok(event)
        })?;
        Ok(serde_json::to_value(event)?)
    }

    /// A readable summary of one store: `{max_entries_per_kind, global?}`.
    fn overview(&self, options: &Value) -> anyhow::Result<Value> {
        let (dir, scope, _) = self.store(options)?;
        let limit = options["max_entries_per_kind"]
            .as_u64()
            .and_then(|limit| usize::try_from(limit).ok())
            .ok_or_else(|| anyhow!("#:max-entries-per-kind must be a non-negative integer"))?;
        let (state, file, scope_name) = Self::read(dir.as_deref(), scope);
        let mut lines = vec![
            format!("Harness state ({scope_name}): {file}"),
            "Call contract: ids read [scope:id] and are accepted as written. Skill entries describe Python REPL skills (a Python reference plus arguments); the Scheme kernel does not call them. Spawn a subagent spec by composing a concise task prompt for (rlm-spawn task #:name \"worker\"): admission returns its handle at once, never the child's answer, and results arrive through agent messages or files. Factory entries declare state-machine workflows of subagent states and are written and run through the factory functions.".to_string(),
        ];
        for (kind, name) in KINDS {
            let entries = Self::listed(&state, Some(kind));
            lines.push(format!("{name}: {}", entries.len()));
            for entry in entries.iter().take(limit) {
                let summary = clipped(&entry.content.trim().replace('\n', " "), 120);
                let described = |label: &str, fields: &Map<String, Value>| {
                    if kind != RefinementKind::Skill || fields.is_empty() {
                        return String::new();
                    }
                    let mut sorted: Vec<(&String, &Value)> = fields.iter().collect();
                    sorted.sort_by_key(|(key, _)| *key);
                    let sorted: Map<String, Value> = sorted
                        .into_iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    format!(
                        " {label}={}",
                        clipped(&Value::Object(sorted).to_string(), 120)
                    )
                };
                lines.push(format!(
                    "  - [{scope_name}:{}] {} ({}, v{}){}{}: {summary}",
                    entry.id,
                    entry.title,
                    entry.path,
                    entry.version,
                    described("ref", &entry.reference),
                    described("args", &entry.arguments),
                ));
            }
            if entries.len() > limit {
                lines.push(format!("  - +{} more", entries.len() - limit));
            }
        }
        lines.push(format!("refinements: {}", state.refinements.len()));
        for event in state.refinements.iter().rev().take(5).rev() {
            lines.push(format!(
                "  - [{}] {}: {}",
                event.id,
                event.trigger,
                event.changes.join(", ")
            ));
        }
        Ok(json!(lines.join("\n")))
    }

    /// One store's state, where it is kept, and its scope's name. A session
    /// without a local store reads an empty one.
    fn read(dir: Option<&Path>, scope: HarnessScope) -> (HarnessState, String, &'static str) {
        let scope_name = match scope {
            HarnessScope::Local => "local",
            HarnessScope::Global => "global",
        };
        match dir {
            Some(dir) => (
                load_harness_state(dir, scope),
                get_harness_state_path(dir).to_string_lossy().into_owned(),
                scope_name,
            ),
            None => (
                crate::refinement::empty_harness_state(),
                "(none: this session is not saved)".to_string(),
                scope_name,
            ),
        }
    }

    /// Entries ranked by weighted term overlap with a query: `{query,
    /// kind?, limit, global?}`. A term scores in an entry's title, content,
    /// and path with id; matching more of them counts more, and a term rare
    /// among the entries outweighs one most of them hold. Ties go to the
    /// entry updated last.
    fn search(&self, options: &Value) -> anyhow::Result<Value> {
        let (dir, scope, _) = self.store(options)?;
        let query = options["query"]
            .as_str()
            .ok_or_else(|| anyhow!("the query must be a string"))?;
        let limit = options["limit"]
            .as_u64()
            .filter(|limit| *limit >= 1)
            .and_then(|limit| usize::try_from(limit).ok())
            .ok_or_else(|| anyhow!("#:limit must be a positive integer"))?;
        let kind = if options["kind"].is_null() {
            None
        } else {
            Some(Self::kind_named(&options["kind"])?)
        };
        // The query's lowercase runs of letters, digits, and marks of any
        // script: punctuation only separates them, and a run breaks where
        // CJK meets another script.
        let mut runs: Vec<Vec<char>> = Vec::new();
        let mut open = false;
        for letter in query.to_lowercase().chars() {
            if !letter.is_alphanumeric() && !unicode_normalization::char::is_combining_mark(letter)
            {
                open = false;
                continue;
            }
            match runs.last_mut() {
                Some(run)
                    if open
                        && run
                            .last()
                            .is_some_and(|last| is_cjk(*last) == is_cjk(letter)) =>
                {
                    run.push(letter);
                }
                Some(_) | None => runs.push(vec![letter]),
            }
            open = true;
        }
        // A CJK run becomes overlapping bigrams (it has no spaces between
        // words); an ASCII run is a term from three characters, a run of
        // another script from two. Each term counts once.
        let mut terms = HarnessQueryTerms::new();
        for run in runs {
            if run.iter().copied().any(is_cjk) && run.len() > 1 {
                terms.extend(run.windows(2).map(|pair| (pair.iter().collect(), 1.0)));
            } else if run.iter().copied().any(is_cjk)
                || run.len() >= if run.iter().all(char::is_ascii) { 3 } else { 2 }
            {
                terms.insert(run.iter().collect(), 1.0);
            }
        }
        let entries = Self::listed(&Self::read(dir.as_deref(), scope).0, kind);
        let idf = harness_query_term_idf(&entries, &terms);
        let mut ranked: Vec<(f64, HarnessEntry)> = entries
            .into_iter()
            .map(|entry| (score_harness_entry_for_query(&entry, &idf, None), entry))
            .filter(|(score, _)| *score > 0.0)
            .collect();
        ranked.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .total_cmp(left_score)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
        });
        let found: Vec<HarnessEntry> = ranked
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry)
            .collect();
        Ok(serde_json::to_value(found)?)
    }

    /// One store in full: `{state_dir?, global?}` to `{scope, file_path,
    /// entries, refinements}`. `state_dir` reads another agent's store.
    fn state(&self, options: &Value) -> anyhow::Result<Value> {
        let (dir, scope, _) = self.store(options)?;
        let dir = match options["state_dir"].as_str() {
            Some(elsewhere) => Some(PathBuf::from(elsewhere)),
            None => dir,
        };
        let (state, file, scope_name) = Self::read(dir.as_deref(), scope);
        Ok(json!({
            "scope": scope_name,
            "file_path": file,
            "entries": state.entries,
            "refinements": state.refinements,
        }))
    }
}

/// Define `%harness-call` in a session's engine: `(operation payload-json)`
/// to the reply JSON its Scheme wrapper decodes.
pub(super) fn register(engine: &mut Engine, session: &crate::kernel::scheme::SessionEnvironment) {
    let harness = Harness {
        agent_dir: session.agent_dir.clone(),
        artifact_dir: session.artifact_dir.clone(),
    };
    engine.register_fn(
        "%harness-call",
        move |operation: String, payload: String| -> String {
            let outcome = serde_json::from_str::<Value>(&payload)
                .map_err(|error| anyhow!("the payload is not JSON: {error}"))
                .and_then(|options| {
                    // The user's master switch, read on every call so a
                    // change in /settings applies at once.
                    let disabled = harness
                        .agent_dir
                        .as_deref()
                        .and_then(|dir| std::fs::read_to_string(dir.join("settings.json")).ok())
                        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                        .is_some_and(|document| {
                            document.get("harnessRefinements") == Some(&Value::Bool(false))
                        });
                    if disabled {
                        bail!("Harness refinements are disabled in /settings");
                    }
                    match operation
                        .split_once(':')
                        .map_or(operation.as_str(), |(verb, _)| verb)
                    {
                        "write" => harness.write(&options),
                        "delete" => harness.delete(&options),
                        "record-refinement" => harness.record_refinement(&options),
                        "overview" => harness.overview(&options),
                        "search" => harness.search(&options),
                        "state" => harness.state(&options),
                        unknown => Err(anyhow!("unknown harness operation: {unknown}")),
                    }
                });
            let who = operation
                .split_once(':')
                .map_or(operation.as_str(), |(_, who)| who);
            super::reply_json(who, outcome)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::super::worktree::tests::evaluate;
    use super::*;
    use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
    use crate::kernel::shared::HostRequestHandlers;
    use crate::tools::kernel_cell::{KernelExecutor, KernelProvisioner};

    /// A kernel for the saved session `s1` under `root`.
    async fn kernel_under(root: &Path) -> Box<dyn KernelExecutor> {
        SchemeKernelProvisioner::with_options(SchemeKernelOptions {
            cwd: root.to_path_buf(),
            agent_dir: Some(root.join("agent")),
            artifact_dir: Some(root.join("sessions").join("s1")),
            ..SchemeKernelOptions::default()
        })
        .ensure(None, None)
        .await
        .unwrap()
    }

    /// The global store as the Python kernel's `rlm.harness` wrote it.
    const PYTHON_WRITTEN: &str = r#"{
  "schema": 1,
  "entries": {
    "prompt": {},
    "memory": {
      "deploy_steps": {
        "id": "deploy_steps",
        "kind": "memory",
        "title": "Deploy steps",
        "content": "run make deploy",
        "path": "ops",
        "scope": "global",
        "reference": {},
        "arguments": {},
        "metadata": {
          "tag": "release"
        },
        "source": "agent",
        "created_at": "2026-01-05T10:00:00.123456+00:00",
        "updated_at": "2026-01-05T10:00:00.123456+00:00",
        "version": 1
      }
    },
    "skill": {},
    "subagent": {},
    "factory": {}
  },
  "refinements": []
}"#;

    /// An entry the Python kernel wrote is updated in place (its omitted
    /// fields kept), new entries land in the store their scope names, and
    /// the file stays the one `/refine`'s loader reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn entries_are_written_to_the_store_the_other_writers_share() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("agent").join("harness");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(get_harness_state_path(&global), PYTHON_WRITTEN).unwrap();
        let kernel = kernel_under(dir.path()).await;
        let seen = |expression: &str| {
            format!(
                "(let ([entry {expression}]) (map (lambda (field) (hash-ref entry field)) '(id kind path scope source version created_at)))"
            )
        };
        let mut returned = Vec::new();
        for expression in [
            seen(r#"(harness-update-memory "global:deploy_steps" "Deploy steps" "run make release")"#),
            seen(r#"(harness-create-memory "Build notes!" "use make check" #:metadata (hash 'tag "ci"))"#),
            seen(r#"(harness-create-prompt-note "Tone" "be brief" #:id "tone" #:global #t)"#),
            seen(
                r#"(harness-create-skill "Lint" "runs the linter" #:reference (hash 'type "python" 'import "lint" 'callable "run") #:arguments (hash 'path "str"))"#,
            ),
            r#"(harness-delete-memory "build_notes")"#.to_string(),
            r#"(harness-delete-memory "build_notes")"#.to_string(),
            r"(map symbol->string (hash-keys->list (hash-ref (hash-ref (harness-state) 'entries) 'skill)))"
                .to_string(),
        ] {
            returned.push(evaluate(kernel.as_ref(), &expression).await.map(|mut value| {
                // The time of a new entry's creation is this run's.
                if value[5] == 1 {
                    value[6] = Value::Null;
                }
                value
            }));
        }
        let stored = load_harness_state(&global, HarnessScope::Global);
        let mut updated = stored.entries[&RefinementKind::Memory]["deploy_steps"].clone();
        updated.updated_at = String::new();
        let created = "2026-01-05T10:00:00.123456+00:00";
        assert_eq!(
            (
                returned,
                updated,
                stored.entries[&RefinementKind::Prompt]
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            ),
            (
                vec![
                    Ok(json!([
                        "deploy_steps",
                        "memory",
                        "ops",
                        "global",
                        "agent",
                        2,
                        created
                    ])),
                    Ok(json!([
                        "build_notes",
                        "memory",
                        "general",
                        "local",
                        "agent",
                        1,
                        null
                    ])),
                    Ok(json!([
                        "tone", "prompt", "policy", "global", "agent", 1, null
                    ])),
                    Ok(json!([
                        "lint", "skill", "general", "local", "agent", 1, null
                    ])),
                    Ok(json!(true)),
                    Ok(json!(false)),
                    Ok(json!(["lint"])),
                ],
                HarnessEntry {
                    id: "deploy_steps".to_string(),
                    kind: RefinementKind::Memory,
                    title: "Deploy steps".to_string(),
                    content: "run make release".to_string(),
                    path: "ops".to_string(),
                    scope: Some(HarnessScope::Global),
                    reference: Map::new(),
                    arguments: Map::new(),
                    metadata: json!({"tag": "release"}).as_object().unwrap().clone(),
                    source: "agent".to_string(),
                    created_at: created.to_string(),
                    updated_at: String::new(),
                    version: 2,
                },
                vec!["tone"],
            )
        );
    }

    /// A write that would break the store or its contract is refused with
    /// the entry and the reason, and leaves the file as it was.
    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_writes_are_refused_and_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("agent").join("harness");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(get_harness_state_path(&global), PYTHON_WRITTEN).unwrap();
        let kernel = kernel_under(dir.path()).await;
        let unsaved = SchemeKernelProvisioner::new(HostRequestHandlers::new(), dir.path())
            .ensure(None, None)
            .await
            .unwrap();
        let mut refused = Vec::new();
        for expression in [
            r#"(harness-create-memory "Deploy steps" "again" #:global #t)"#,
            r#"(harness-update-memory "missing" "Title" "text" #:global #t)"#,
            r#"(harness-create-memory "Title" (list "not" "text") #:global #t)"#,
            r#"(harness-create-skill "Lint" "runs the linter" #:global #t)"#,
            r#"(harness-create-skill "Lint" "runs the linter" #:reference (hash 'type "shell") #:global #t)"#,
            r#"(harness-record-refinement "" "a change" #:global #t)"#,
            r#"(harness-delete-memory "missing" #:global #t)"#,
        ] {
            refused.push(evaluate(kernel.as_ref(), expression).await);
        }
        refused.push(
            evaluate(
                unsaved.as_ref(),
                r#"(harness-create-memory "Title" "text")"#,
            )
            .await,
        );
        let untouched = std::fs::read_to_string(get_harness_state_path(&global)).unwrap();
        std::fs::write(
            dir.path().join("agent").join("settings.json"),
            r#"{"harnessRefinements": false}"#,
        )
        .unwrap();
        refused.push(evaluate(kernel.as_ref(), "(harness-overview)").await);
        let raised = |reason: &str| Err(format!("Error: Generic:  {reason}"));
        assert_eq!(
            (refused, untouched),
            (
                vec![
                    raised("harness-create-memory: memory entry 'deploy_steps' already exists"),
                    raised("harness-update-memory: memory entry 'missing' does not exist"),
                    raised("harness-create-memory: memory entry 'title' rejected: content must be a non-empty string, got a list"),
                    raised("harness-create-skill: skill entry 'lint' rejected: skill entries require a Python reference"),
                    raised("harness-create-skill: skill entry 'lint' rejected: skill reference.type must be 'python'"),
                    raised("harness-record-refinement: refinement event rejected: trigger must be a non-empty string, got an empty string"),
                    Ok(json!(false)),
                    raised("harness-create-memory: this session is not saved, so it has no local harness store; pass #:global #t to persist across sessions"),
                    raised("harness-overview: Harness refinements are disabled in /settings"),
                ],
                PYTHON_WRITTEN.to_string(),
            )
        );
    }

    /// Search ranks by the query's words (a rare word outweighs a common
    /// one, punctuation is no word), and the overview lists what is stored.
    #[tokio::test(flavor = "multi_thread")]
    async fn entries_are_searched_by_term_overlap_and_summarized() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = kernel_under(dir.path()).await;
        let mut returned = Vec::new();
        for expression in [
            r#"(begin
                 (harness-create-memory "Deploy checklist" "run the deploy script, then verify" #:path "ops")
                 (harness-create-memory "Deploy rollback" "revert the deploy with the rollback script")
                 (harness-create-subagent "Reviewer" "review the diff for 修复登录 regressions")
                 (hash-ref (harness-record-refinement "the deploy failed twice" "create memory:deploy_rollback") 'id))"#,
            r#"(map (lambda (entry) (hash-ref entry 'id)) (harness-search "rollback deploy?"))"#,
            r#"(map (lambda (entry) (hash-ref entry 'id)) (harness-search "deploy" #:kind "subagent"))"#,
            r#"(map (lambda (entry) (hash-ref entry 'id)) (harness-search "登录 to" #:limit 1))"#,
            r#"(harness-plan-refinement "tests flake" #:failing-component "the retry skill" #:next-step "rerun twice")"#,
            "(harness-overview #:max-entries-per-kind 1)",
        ] {
            returned.push(evaluate(kernel.as_ref(), expression).await);
        }
        let file = dir.path().join("sessions/s1/harness/harness_state.json");
        let overview = returned.pop().unwrap().unwrap();
        let lines: Vec<&str> = overview.as_str().unwrap().lines().collect();
        assert_eq!(
            (returned, lines[0], &lines[2..]),
            (
                vec![
                    Ok(json!("refine_0001")),
                    Ok(json!(["deploy_rollback", "deploy_checklist"])),
                    Ok(json!([])),
                    Ok(json!(["reviewer"])),
                    Ok(json!([
                        "Diagnose the repeated failure or opportunity for the retry skill: tests flake",
                        "Update the smallest useful prompt note, memory item, skill, or subagent spec.",
                        "Run the next action with the changed harness state, then record the outcome.",
                        "Immediate validation step: rerun twice",
                    ])),
                ],
                format!("Harness state (local): {}", file.display()).as_str(),
                &[
                    "prompt: 0",
                    "memory: 2",
                    "  - [local:deploy_rollback] Deploy rollback (general, v1): revert the deploy with the rollback script",
                    "  - +1 more",
                    "skill: 0",
                    "subagent: 1",
                    "  - [local:reviewer] Reviewer (general, v1): review the diff for 修复登录 regressions",
                    "factory: 0",
                    "refinements: 1",
                    "  - [refine_0001] the deploy failed twice: create memory:deploy_rollback",
                ][..],
            )
        );
    }
}
