//! The reuse listing the kernel's `(harness-query #:view "reuse")` and `prime-agent reuse`
//! show, the inventory's marks, and the user's prune.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail};
use serde::Serialize;

use super::{EntryKey, EntryRecord, Observed, Standing};
use crate::refinement::{
    get_global_harness_state_dir, load_harness_state, update_harness_state, HarnessScope,
    DEFAULT_OVERVIEW_CONTENT_LIMIT,
};
use crate::skills::{Skill, SourceScope};

/// One entry's reuse. The counts leave out the session that created it,
/// except `own_uses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReuseRow {
    /// [`EntryKey::text`]: what `prime-agent reuse prune` takes.
    pub key: String,
    /// `skill` (an installed skill) or `harness` (a global harness entry).
    pub store: &'static str,
    /// A skill's type (`markdown`, `scheme`) or a harness entry's kind.
    pub kind: String,
    /// The skill's name or the entry's title.
    pub name: String,
    pub standing: Standing,
    /// When the ledger first knew it; `None` before any session saw it.
    pub created_at: Option<String>,
    /// Whether the creating session was seen creating it (otherwise the
    /// first session that saw it stands in).
    pub creator_known: bool,
    /// Whether the session asking created it.
    pub created_here: bool,
    /// Later sessions that were offered it.
    pub offered_sessions: u64,
    /// Later sessions that used it.
    pub used_sessions: u64,
    /// Uses in later sessions.
    pub uses: u64,
    /// Uses in the creating session.
    pub own_uses: u64,
    pub last_used_at: Option<String>,
    /// What it costs each prompt that carries it: its inventory block or
    /// digest line.
    pub prompt_bytes: u64,
    /// `prompt_bytes` as tokens, at about four bytes a token.
    pub prompt_tokens: u64,
    /// Whether `prime-agent reuse prune` can remove it (a skill outside the
    /// user's skills directory belongs to a project or a package).
    pub removable: bool,
}

/// What a prune removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pruned {
    pub key: String,
    /// The skill's file or directory, or the harness store file.
    pub removed: String,
}

/// One listed entry, before the ledger's counts.
struct Listed {
    key: EntryKey,
    store: &'static str,
    kind: String,
    name: String,
    prompt_bytes: usize,
    removable: bool,
}

/// The row of one entry from its ledger record.
fn row(listed: Listed, record: Option<&EntryRecord>, session: Option<&str>) -> ReuseRow {
    let prompt_bytes = listed.prompt_bytes as u64;
    ReuseRow {
        key: listed.key.text(),
        store: listed.store,
        kind: listed.kind,
        name: listed.name,
        standing: record.map_or(Standing::New, EntryRecord::standing),
        created_at: record.map(|record| record.created.at.clone()),
        creator_known: record.is_some_and(|record| record.created.observed == Observed::Created),
        created_here: record
            .zip(session)
            .is_some_and(|(record, session)| record.created.session == session),
        offered_sessions: record.map_or(0, |record| record.offered_elsewhere),
        used_sessions: record.map_or(0, |record| record.used_elsewhere),
        uses: record.map_or(0, |record| record.uses_elsewhere),
        own_uses: record.map_or(0, |record| record.uses - record.uses_elsewhere),
        last_used_at: record.and_then(|record| record.last_used_at.clone()),
        prompt_bytes,
        prompt_tokens: prompt_bytes.div_ceil(4),
        removable: listed.removable,
    }
}

/// Whether `skill` lives in the user's own skills directory.
fn removable(agent_dir: &Path, skill: &Skill) -> bool {
    let skills_dir = crate::skills::loader::canonicalize_path(&agent_dir.join("skills"));
    skill.source_info.scope == SourceScope::User
        && crate::skills::loader::canonicalize_path(&skill.file_path).starts_with(skills_dir)
}

/// Every tracked skill in `skills` ([`super::tracked`]) and every global harness entry under
/// `agent_dir`, with what the ledger knows of its reuse; `session` is the
/// session asking, when one is.
#[must_use]
pub fn report(agent_dir: &Path, skills: &[Skill], session: Option<&str>) -> Vec<ReuseRow> {
    let ledger = super::load(agent_dir);
    let mut rows: Vec<ReuseRow> = skills
        .iter()
        .filter(|skill| super::tracked(skill))
        .map(|skill| {
            let key = EntryKey::Skill(skill.name.clone());
            let record = ledger.record(&key);
            let mark = record
                .map(EntryRecord::standing)
                .filter(|standing| *standing != Standing::New);
            let prompt_bytes = if skill.disable_model_invocation {
                0
            } else {
                crate::skills::skill_inventory_block(skill, mark).len()
            };
            let listed = Listed {
                key: key.clone(),
                store: "skill",
                kind: skill.kind_label().to_string(),
                name: skill.name.clone(),
                prompt_bytes,
                removable: removable(agent_dir, skill),
            };
            row(listed, record, session)
        })
        .collect();
    let harness = load_harness_state(
        &get_global_harness_state_dir(agent_dir),
        HarnessScope::Global,
    );
    for (kind, entries) in &harness.entries {
        for entry in entries.values() {
            let key = EntryKey::Harness(*kind, entry.id.clone());
            let record = ledger.record(&key);
            let listed = Listed {
                key,
                store: "harness",
                kind: crate::refinement::kind_name(*kind).to_string(),
                name: entry.title.clone(),
                prompt_bytes: crate::refinement::ranking::digest_entry_line(
                    entry,
                    DEFAULT_OVERVIEW_CONTENT_LIMIT,
                )
                .len(),
                removable: true,
            };
            rows.push(row(listed, record, session));
        }
    }
    rows
}

/// The standing of each of `skills` the ledger has a record of, by name:
/// what the prompt's inventory marks.
#[must_use]
pub fn skill_standings(agent_dir: &Path, skills: &[Skill]) -> BTreeMap<String, Standing> {
    let ledger = super::load(agent_dir);
    skills
        .iter()
        .filter(|skill| super::tracked(skill))
        .filter_map(|skill| {
            ledger
                .record(&EntryKey::Skill(skill.name.clone()))
                .map(|record| (skill.name.clone(), record.standing()))
        })
        .collect()
}

/// What one pruned key removes.
enum Removal<'a> {
    Skill(&'a Skill),
    Harness(crate::refinement::RefinementKind, String),
}

/// Remove the entries the user named by key: a skill in their own skills
/// directory (its directory, or its file for a single-file skill) or a
/// global harness entry, and their ledger records. Every key is checked
/// before anything is removed.
///
/// # Errors
///
/// A key that names no current entry, or a skill outside the user's skills
/// directory (nothing is removed then); a removal or the ledger write that
/// failed.
pub fn prune(agent_dir: &Path, skills: &[Skill], keys: &[String]) -> anyhow::Result<Vec<Pruned>> {
    let global_dir = get_global_harness_state_dir(agent_dir);
    let harness = load_harness_state(&global_dir, HarnessScope::Global);
    let mut planned: Vec<(EntryKey, Removal)> = Vec::new();
    for text in keys {
        let key = EntryKey::parse(text).ok_or_else(|| {
            anyhow!("{text:?} is not a reuse key (skill/<name> or harness/<kind>/<id>)")
        })?;
        match &key {
            EntryKey::Skill(name) => {
                let skill = skills
                    .iter()
                    .find(|skill| &skill.name == name)
                    .ok_or_else(|| anyhow!("no skill named {name:?}"))?;
                if !removable(agent_dir, skill) {
                    bail!(
                        "skill {name:?} is not in your skills directory ({}): remove it where it is installed",
                        skill.file_path.display()
                    );
                }
                planned.push((key.clone(), Removal::Skill(skill)));
            }
            EntryKey::Harness(kind, id) => {
                if !harness
                    .entries
                    .get(kind)
                    .is_some_and(|entries| entries.contains_key(id))
                {
                    bail!("no global harness entry {text:?}");
                }
                planned.push((key.clone(), Removal::Harness(*kind, id.clone())));
            }
        }
    }
    let mut pruned = Vec::new();
    for (key, removal) in &planned {
        let removed = match removal {
            Removal::Skill(skill) => {
                let whole_dir = skill
                    .file_path
                    .file_name()
                    .is_some_and(|name| name == "SKILL.md");
                if whole_dir {
                    std::fs::remove_dir_all(&skill.base_dir)?;
                    skill.base_dir.display().to_string()
                } else {
                    std::fs::remove_file(&skill.file_path)?;
                    skill.file_path.display().to_string()
                }
            }
            Removal::Harness(kind, id) => {
                let ((), file) =
                    update_harness_state(&global_dir, HarnessScope::Global, |state| {
                        if let Some(entries) = state.entries.get_mut(kind) {
                            entries.remove(id);
                        }
                    })?;
                file.display().to_string()
            }
        };
        pruned.push(Pruned {
            key: key.text(),
            removed,
        });
    }
    let forgotten: Vec<EntryKey> = planned.into_iter().map(|(key, _)| key).collect();
    super::forget(agent_dir, &forgotten)?;
    Ok(pruned)
}
