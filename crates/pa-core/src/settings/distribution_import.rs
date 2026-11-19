//! A distribution build's first-run configuration import.
//!
//! A distribution build keeps its own per-user agent dir
//! ([`StateLayout::Distribution`], `~/.prime/agent-<variant>`), so it never
//! lists, resumes, or leases the sessions the official products keep in
//! `~/.prime/agent`. On its first run (its own dir does not exist yet) it
//! copies the user's hand-made configuration over once, so logins, settings,
//! providers, and skills carry over; sessions and all runtime state stay
//! behind.
//!
//! The import is local and bounded (no network, a size budget), idempotent
//! (it runs only while the target dir is missing), and crash-safe: entries are
//! copied into a hidden sibling stage dir that is renamed onto the target in
//! one step, so a crash leaves either no agent dir (the next run imports
//! again) or the complete one.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pa_types::platform::StateLayout;

/// The official agent dir's configuration entries, in copy order. Each is
/// something the user set up by hand and would otherwise redo:
/// `settings.json` (preferences, packages, MCP servers), `auth.json`
/// (provider and MCP logins), `models.json` (custom providers),
/// `keybindings.json`, `mcp-services.json` (the local MCP catalog),
/// `mcp-connections.json` (installed MCP connections; their tokens live in
/// `auth.json`), the global context and system-prompt files, and the
/// `skills/`, `prompts/`, and `themes/` dirs.
///
/// Never imported: `sessions/`, `session-artifacts/`, `session-leases/`,
/// `daemon-workers/`, `persistent-shells/`, `logs/`, `harness/`, the
/// `kernel-venv/`, update and restart state, caches, the `git`/`npm`
/// package checkouts (packages reinstall from the copied settings), and
/// `telemetry.json` (the build gets its own installation id).
const CONFIG_ENTRIES: [&str; 15] = [
    "settings.json",
    "auth.json",
    "models.json",
    "keybindings.json",
    "mcp-services.json",
    "mcp-connections.json",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    "skills",
    "prompts",
    "themes",
];

/// The import's byte budget: an entry that would push the copy past it (a
/// skill dir holding a checkout or a virtualenv) is left behind and named,
/// so the first run never stalls on a large copy.
const IMPORT_BYTE_BUDGET: u64 = 32 * 1024 * 1024;

/// The import's file-count budget, the same rule as the byte budget.
const IMPORT_ENTRY_BUDGET: u64 = 4096;

/// A stage dir this old belongs to a run that crashed mid-import.
const STALE_STAGE_AGE: Duration = Duration::from_mins(10);

/// What the first-run import did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficialConfigImport {
    /// No import applies: the official build, an agent dir overridden away
    /// from this build's own default, or an agent dir that already exists.
    NotApplicable,
    /// The official agent dir holds none of the configuration entries;
    /// nothing was created.
    NothingToImport,
    /// The agent dir was created holding `copied`; `skipped` names the
    /// entries left behind because they exceed the size budget.
    Imported {
        copied: Vec<String>,
        skipped: Vec<String>,
    },
}

/// Create a distribution build's own agent dir from the official one's
/// configuration on its first run. `agent_dir` is the resolved agent dir
/// (env override included); the import applies only when it is `layout`'s
/// default under `home` and does not exist yet.
///
/// # Errors
///
/// Returns the I/O error of a failed copy or publish; nothing is left
/// behind then (the stage dir is removed) and the next run imports again.
pub fn import_official_config(
    layout: StateLayout,
    home: &Path,
    agent_dir: &Path,
) -> std::io::Result<OfficialConfigImport> {
    match layout {
        StateLayout::Official => return Ok(OfficialConfigImport::NotApplicable),
        StateLayout::Distribution { .. } => {}
    }
    if agent_dir != layout.home_agent_dir(home) || agent_dir.symlink_metadata().is_ok() {
        return Ok(OfficialConfigImport::NotApplicable);
    }
    let source = StateLayout::Official.home_agent_dir(home);
    let present: Vec<&str> = CONFIG_ENTRIES
        .into_iter()
        .filter(|name| source.join(name).symlink_metadata().is_ok())
        .collect();
    if present.is_empty() {
        return Ok(OfficialConfigImport::NothingToImport);
    }
    let (Some(parent), Some(leaf)) = (agent_dir.parent(), agent_dir.file_name()) else {
        return Ok(OfficialConfigImport::NotApplicable);
    };
    std::fs::create_dir_all(parent)?;
    let stage_prefix = format!(".{}.import-", leaf.to_string_lossy());
    for entry in std::fs::read_dir(parent)?.flatten() {
        let stale = entry
            .file_name()
            .to_string_lossy()
            .starts_with(&stage_prefix)
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > STALE_STAGE_AGE));
        if stale {
            // Best effort: a crashed run's leftover; a live stage is younger.
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let stage = parent.join(format!("{stage_prefix}{}-{nonce}", std::process::id()));
    std::fs::create_dir(&stage)?;
    let mut copied = Vec::new();
    let mut skipped = Vec::new();
    let (mut bytes, mut files) = (0_u64, 0_u64);
    for name in present {
        let target = stage.join(name);
        // A case-insensitive filesystem already holds `AGENTS.MD` once
        // `AGENTS.md` is copied: the first spelling wins, as on load.
        if target.symlink_metadata().is_ok() {
            continue;
        }
        let (entry_bytes, entry_files) =
            measure(&source.join(name)).map_err(|error| discard_stage(&stage, error))?;
        if bytes + entry_bytes > IMPORT_BYTE_BUDGET || files + entry_files > IMPORT_ENTRY_BUDGET {
            skipped.push(name.to_string());
            continue;
        }
        copy_entry(&source.join(name), &target).map_err(|error| discard_stage(&stage, error))?;
        bytes += entry_bytes;
        files += entry_files;
        copied.push(name.to_string());
    }
    // Best effort, like every agent-dir creation: the restriction is
    // hardening, never a reason to refuse the import.
    let _ = crate::platform::perms::restrict_dir(&stage);
    match std::fs::rename(&stage, agent_dir) {
        Ok(()) => Ok(OfficialConfigImport::Imported { copied, skipped }),
        Err(error) => {
            let error = discard_stage(&stage, error);
            // A concurrent first run published the dir first: its import stands.
            if agent_dir.symlink_metadata().is_ok() {
                Ok(OfficialConfigImport::NotApplicable)
            } else {
                Err(error)
            }
        }
    }
}

/// Remove an unpublished stage dir, passing the error that ended it on.
fn discard_stage(stage: &Path, error: std::io::Error) -> std::io::Error {
    // Best effort: a leftover is swept as stale by a later run.
    let _ = std::fs::remove_dir_all(stage);
    error
}

/// The bytes and files under `path`, symlinks unfollowed (a link counts as
/// one file of no size).
fn measure(path: &Path) -> std::io::Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok((metadata.len(), 1));
    }
    let mut total = (0, 1);
    for entry in std::fs::read_dir(path)? {
        let (bytes, files) = measure(&entry?.path())?;
        total = (total.0 + bytes, total.1 + files);
    }
    Ok(total)
}

/// Copy one entry: files by content (permissions kept, so `auth.json` stays
/// private), dirs recursively, symlinks as links to the same target (Unix;
/// Windows skips them, its link creation needs a privilege). Sockets and
/// other special files are not configuration and are skipped.
fn copy_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    let kind = std::fs::symlink_metadata(from)?.file_type();
    if kind.is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
        return Ok(());
    }
    if kind.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_entry(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if kind.is_file() {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    const FORK: StateLayout = StateLayout::Distribution { variant: "fork" };

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Every file under `dir`, relative, with its content.
    fn tree(dir: &Path) -> BTreeMap<PathBuf, String> {
        let mut files = BTreeMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in std::fs::read_dir(&next).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path.strip_prefix(dir).unwrap().to_path_buf();
                    files.insert(relative, std::fs::read_to_string(&path).unwrap());
                }
            }
        }
        files
    }

    /// An official agent dir as a long-time user has it: configuration plus
    /// sessions and the runtime state around them.
    fn official_dir(home: &Path) -> PathBuf {
        let official = home.join(".prime/agent");
        for (name, body) in [
            ("settings.json", r#"{"defaultModel":"m"}"#),
            ("auth.json", r#"{"prime":{"type":"api_key","key":"k"}}"#),
            ("models.json", r#"{"providers":{}}"#),
            ("keybindings.json", "{}"),
            ("AGENTS.md", "global rules"),
            ("skills/review/SKILL.md", "# review"),
            ("prompts/fix.md", "fix it"),
            ("sessions/--repo--/2026-01-01_a.jsonl", "{}"),
            ("session-artifacts/a/sub/child.jsonl", "{}"),
            ("session-leases/a.lock", "1"),
            ("daemon-workers/w.json", "{}"),
            ("logs/agent.jsonl", "{}"),
            ("harness/harness_state.json", "{}"),
            ("kernel-venv/bin/python", ""),
            ("telemetry.json", r#"{"version":1}"#),
        ] {
            write(&official.join(name), body);
        }
        official
    }

    /// The first run copies the configuration, never sessions or runtime
    /// state, publishes the dir in one step, and never runs again.
    #[test]
    fn the_first_run_copies_the_configuration_and_never_the_sessions() {
        let home = tempfile::tempdir().unwrap();
        let official = official_dir(home.path());
        let before = tree(&official);
        let fork_dir = FORK.home_agent_dir(home.path());

        let outcome = import_official_config(FORK, home.path(), &fork_dir).unwrap();

        assert_eq!(
            outcome,
            OfficialConfigImport::Imported {
                copied: [
                    "settings.json",
                    "auth.json",
                    "models.json",
                    "keybindings.json",
                    "AGENTS.md",
                    "skills",
                    "prompts"
                ]
                .map(String::from)
                .to_vec(),
                skipped: Vec::new(),
            }
        );
        let expected: BTreeMap<PathBuf, String> = [
            ("settings.json", r#"{"defaultModel":"m"}"#),
            ("auth.json", r#"{"prime":{"type":"api_key","key":"k"}}"#),
            ("models.json", r#"{"providers":{}}"#),
            ("keybindings.json", "{}"),
            ("AGENTS.md", "global rules"),
            ("skills/review/SKILL.md", "# review"),
            ("prompts/fix.md", "fix it"),
        ]
        .into_iter()
        .map(|(name, body)| (PathBuf::from(name), body.to_string()))
        .collect();
        assert_eq!(tree(&fork_dir), expected);
        assert_eq!(tree(&official), before, "the official dir is untouched");
        let mut siblings: Vec<String> = std::fs::read_dir(home.path().join(".prime"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        siblings.sort();
        assert_eq!(siblings, ["agent", "agent-fork"], "no stage dir is left");

        // Idempotent: the dir exists now, so a later run (or a later edit to
        // the official settings) never imports again.
        write(
            &official.join("settings.json"),
            r#"{"defaultModel":"other"}"#,
        );
        assert_eq!(
            import_official_config(FORK, home.path(), &fork_dir).unwrap(),
            OfficialConfigImport::NotApplicable
        );
        assert_eq!(tree(&fork_dir), expected);
    }

    /// The official build, an overridden agent dir, and a home without an
    /// official configuration create nothing.
    #[test]
    fn only_a_distribution_builds_own_missing_dir_imports() {
        let home = tempfile::tempdir().unwrap();
        official_dir(home.path());
        let fork_dir = FORK.home_agent_dir(home.path());
        let overridden = home.path().join("elsewhere");
        let empty_home = tempfile::tempdir().unwrap();
        assert_eq!(
            [
                import_official_config(
                    StateLayout::Official,
                    home.path(),
                    &StateLayout::Official.home_agent_dir(home.path())
                )
                .unwrap(),
                import_official_config(FORK, home.path(), &overridden).unwrap(),
                import_official_config(
                    FORK,
                    empty_home.path(),
                    &FORK.home_agent_dir(empty_home.path())
                )
                .unwrap(),
            ],
            [
                OfficialConfigImport::NotApplicable,
                OfficialConfigImport::NotApplicable,
                OfficialConfigImport::NothingToImport,
            ]
        );
        assert!(!fork_dir.exists() && !overridden.exists());
        assert!(!FORK.home_agent_dir(empty_home.path()).exists());
    }

    /// An entry past the size budget stays behind and is named; the rest
    /// still imports.
    #[test]
    fn an_entry_past_the_budget_is_skipped_and_named() {
        let home = tempfile::tempdir().unwrap();
        let official = home.path().join(".prime/agent");
        write(&official.join("settings.json"), "{}");
        let checkout = official.join("skills/big/blob.bin");
        write(&checkout, "");
        std::fs::File::options()
            .write(true)
            .open(&checkout)
            .unwrap()
            .set_len(IMPORT_BYTE_BUDGET + 1)
            .unwrap();
        let fork_dir = FORK.home_agent_dir(home.path());

        assert_eq!(
            import_official_config(FORK, home.path(), &fork_dir).unwrap(),
            OfficialConfigImport::Imported {
                copied: vec!["settings.json".to_string()],
                skipped: vec!["skills".to_string()],
            }
        );
        assert!(!fork_dir.join("skills").exists());
    }
}
