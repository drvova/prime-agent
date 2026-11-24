//! Skill discovery, validation, and prompt formatting.

pub mod diagnostics;
pub mod discovery;
pub mod frontmatter;
pub mod loader;
pub mod prompt_templates;

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Max name length per the Agent Skills spec.
pub const MAX_NAME_LENGTH: usize = 64;
/// Max description length per the Agent Skills spec.
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

pub use diagnostics::{ResourceCollision, ResourceDiagnostic};
pub use discovery::load_skills_from_dir;
pub use loader::{load_skills, LoadSkillsOptions, LoadSkillsResult};
pub use pa_types::slash_commands::parse_slash_command;
pub use prompt_templates::{
    expand_prompt_template, load_prompt_templates, parse_command_args, substitute_args,
    LoadPromptTemplatesOptions, PromptTemplate,
};

/// Source provenance for a resource (the TS source-info shape).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub base_dir: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SourceScope {
    User,
    Project,
    Temporary,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SourceOrigin {
    Package,
    TopLevel,
}

/// Markdown entry test: the file name carries a lowercase `.md` extension
/// (TS `endsWith('.md')` parity: mixed-case suffixes stay undiscovered, and
/// a bare `.md` name has no extension and does not count).
#[must_use]
pub fn skill_markdown_name(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|ext| ext == "md")
}

pub fn create_synthetic_source_info(
    path: &str,
    source: &str,
    scope: SourceScope,
    base_dir: Option<&str>,
) -> SourceInfo {
    SourceInfo {
        path: path.to_string(),
        source: source.to_string(),
        scope,
        origin: SourceOrigin::TopLevel,
        base_dir: base_dir.map(str::to_string),
    }
}

/// A discovered skill.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source_info: SourceInfo,
    pub disable_model_invocation: bool,
    pub kind: SkillKind,
}

/// What a skill is made of.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SkillKind {
    /// Documentation only: its `SKILL.md`.
    Markdown,
    /// Carries Scheme source ([`scheme_source_files`]) the session's kernel
    /// loads, so its functions are callable from a cell.
    Scheme,
}

impl Skill {
    /// The kind label the prompt inventory and telemetry share
    /// (`<type>` in `format_skills_for_prompt`, `skill_kind` in events).
    #[must_use]
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            SkillKind::Markdown => "markdown",
            SkillKind::Scheme => "scheme",
        }
    }
}

/// The Scheme source files of the skill in `dir`, in load order:
/// `scheme/*.scm` by file name, then `skill.scm`.
#[must_use]
pub fn scheme_source_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("scheme"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "scm") && path.is_file()
        })
        .collect();
    files.sort();
    let entry = dir.join("skill.scm");
    if entry.is_file() {
        files.push(entry);
    }
    files
}

pub(crate) fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir_name {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir_name}\""
        ));
    }
    if name.len() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.is_empty()
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

pub(crate) fn validate_description(description: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if description.trim().is_empty() {
        errors.push("description is required".to_string());
    } else if description.len() > MAX_DESCRIPTION_LENGTH {
        errors.push(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            description.len()
        ));
    }
    errors
}

pub(crate) use validate_description as validate_skill_description;
pub(crate) use validate_name as validate_skill_name;

/// Format skills for a system prompt (Agent Skills XML standard); skills
/// with disableModelInvocation are excluded. `standings` marks each skill
/// whose reuse is known (by name) with a `<reuse>` element; a skill it does
/// not name, or names as [`crate::reuse::Standing::New`], is unmarked. The
/// order is the skills' own, whatever their standing.
#[must_use]
pub fn format_skills_for_prompt(
    skills: &[Skill],
    standings: &std::collections::BTreeMap<String, crate::reuse::Standing>,
) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let marks: Vec<Option<crate::reuse::Standing>> = visible
        .iter()
        .map(|skill| {
            standings
                .get(&skill.name)
                .copied()
                .filter(|standing| *standing != crate::reuse::Standing::New)
        })
        .collect();
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Read a skill's file when the task matches its description: `(displayln (hash-ref (fs-read \"<location>\") 'text))`.".to_string(),
        "A skill of type scheme defines Scheme functions you can call from `eval`; its file describes them.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
    ];
    if marks.iter().any(Option::is_some) {
        lines.push(format!(
            "`<reuse>` says how sessions other than the one that made a skill used it: `reused` (two or more), `used-once`, or `unused` (listed in {} or more, read by none); `(reuse-report)` has the counts and each skill's prompt cost.",
            crate::reuse::UNUSED_AFTER_SESSIONS
        ));
    }
    lines.push(String::new());
    lines.push("<available_skills>".to_string());
    for (skill, mark) in visible.into_iter().zip(marks) {
        lines.push(skill_inventory_block(skill, mark));
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// One skill's `<skill>` block in the inventory: what listing it costs
/// every session's prompt.
#[must_use]
pub fn skill_inventory_block(skill: &Skill, mark: Option<crate::reuse::Standing>) -> String {
    let mut lines = vec![
        "  <skill>".to_string(),
        format!("    <name>{}</name>", escape_xml(&skill.name)),
        format!("    <type>{}</type>", skill.kind_label()),
    ];
    if let Some(standing) = mark {
        lines.push(format!("    <reuse>{}</reuse>", standing.label()));
    }
    lines.push(format!(
        "    <description>{}</description>",
        escape_xml(&skill.description)
    ));
    lines.push(format!(
        "    <location>{}</location>",
        escape_xml(&skill.file_path.display().to_string())
    ));
    lines.push("  </skill>".to_string());
    lines.join("\n")
}

pub(crate) fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Expand skill commands (`/skill:<name> [args]`) into the `<skill ...>` message block: a non-skill
/// input, an unknown skill name (the surfaces show their own unknown-command notice), and a skill
/// file that fails to read all pass through. Returns the skill the expansion used.
#[must_use]
pub fn expand_skill_command<'a>(text: &str, skills: &'a [Skill]) -> (String, Option<&'a Skill>) {
    let Some((name, args)) = parse_slash_command(text) else {
        return (text.to_string(), None);
    };
    let Some(skill_name) = name.strip_prefix("skill:") else {
        return (text.to_string(), None);
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == skill_name) else {
        return (text.to_string(), None);
    };
    let Ok(content) = std::fs::read_to_string(&skill.file_path) else {
        return (text.to_string(), None);
    };
    let body = frontmatter::strip_frontmatter(&content).trim().to_string();
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display(),
        body
    );
    let expanded = if args.is_empty() {
        block
    } else {
        format!("{block}\n\n{args}")
    };
    (expanded, Some(skill))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_rules() {
        assert!(validate_name("web-search", "web-search").is_empty());
        assert!(!validate_name("web_search", "web_search").is_empty());
        assert!(!validate_name("-lead", "-lead").is_empty());
        assert!(!validate_name("a--b", "a--b").is_empty());
        assert!(!validate_name("web", "other").is_empty());
    }

    /// Skills load in name order whatever order the filesystem lists their
    /// directories in, so the skills list and `reuse-report` are the same
    /// on every machine.
    #[test]
    fn skills_load_in_name_order_not_listing_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let names: Vec<String> = (0..24).map(|index| format!("skill-{index:02}")).collect();
        for name in names.iter().rev() {
            let skill_dir = dir.path().join(name);
            std::fs::create_dir(&skill_dir).expect("skill dir");
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: test skill\n---\nUse it."),
            )
            .expect("write skill file");
        }
        let loaded: Vec<String> = load_skills_from_dir(dir.path(), "user")
            .skills
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(loaded, names);
    }

    #[test]
    fn description_validation_rules() {
        assert_eq!(validate_description(""), vec!["description is required"]);
        assert!(validate_description("ok").is_empty());
        let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 1);
        assert_eq!(validate_description(&long).len(), 1);
    }

    fn temp_skill(name: &str, dir: &std::path::Path) -> Skill {
        let file_path = dir.join("SKILL.md");
        std::fs::write(
            &file_path,
            format!("---\nname: {name}\ndescription: test skill\n---\nUse {name} well."),
        )
        .expect("write skill file");
        Skill {
            name: name.to_string(),
            description: "test skill".to_string(),
            file_path: file_path.clone(),
            base_dir: dir.to_path_buf(),
            source_info: create_synthetic_source_info(
                &file_path.display().to_string(),
                "user",
                SourceScope::User,
                None,
            ),
            disable_model_invocation: false,
            kind: SkillKind::Markdown,
        }
    }

    #[test]
    fn expands_a_known_skill_command_with_and_without_args() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill = temp_skill("web-search", dir.path());
        let skills = [skill];
        let (expanded, used) = expand_skill_command("/skill:web-search", &skills);
        assert_eq!(used.map(|skill| skill.name.as_str()), Some("web-search"));
        let parsed = pa_types::skill_blocks::parse_skill_block(&expanded).expect("parses");
        assert_eq!(parsed.name, "web-search");
        // TS block shape: the references note rides inside the body,
        // ahead of the frontmatter-stripped skill content.
        assert_eq!(
            parsed.content,
            format!(
                "References are relative to {}.\n\nUse web-search well.",
                dir.path().display()
            )
        );
        assert_eq!(parsed.user_message, None);
        assert!(parsed.location.ends_with("SKILL.md"));

        let (expanded, used) = expand_skill_command("/skill:web-search find rust tuis", &skills);
        assert_eq!(used.map(|skill| skill.name.as_str()), Some("web-search"));
        let parsed = pa_types::skill_blocks::parse_skill_block(&expanded).expect("parses");
        assert_eq!(
            parsed.user_message.as_deref(),
            Some("find rust tuis"),
            "args follow the block after a blank line"
        );
    }

    #[test]
    fn an_empty_frontmatter_block_is_a_warned_skip_not_a_panic() {
        // TS no-frontmatter semantics: the whole document is the body, the
        // description is missing, the skill is NOT loaded (warning).
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join("bare");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(skill_dir.join("SKILL.md"), "---\n---\nUse it well.").expect("write");
        let result = crate::skills::load_skills_from_dir(dir.path(), "user");
        assert!(result.skills.is_empty());
        assert!(result.diagnostics.iter().any(
            |d| matches!(d, crate::skills::ResourceDiagnostic::Warning { message, .. }
                if message == "description is required")
        ));
    }

    #[test]
    fn an_empty_frontmatter_name_falls_back_to_the_parent_directory() {
        // TS `frontmatter.name || parentDirName`.
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join("named-dir");
        std::fs::create_dir_all(&skill_dir).expect("mkdir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: \"\"\ndescription: The named dir skill\n---\nbody",
        )
        .expect("write");
        let result = crate::skills::load_skills_from_dir(dir.path(), "user");
        assert_eq!(
            result
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["named-dir"]
        );
    }

    #[test]
    fn non_skill_and_unknown_inputs_pass_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills = [temp_skill("web-search", dir.path())];
        for text in [
            "hello world",
            "/compact",
            "/skill:missing do something",
            "/skill:",
        ] {
            let (expanded, used) = expand_skill_command(text, &skills);
            assert_eq!(expanded, text, "{text} must pass through");
            assert!(used.is_none(), "{text} must not report a skill");
        }
    }

    #[test]
    fn an_unreadable_skill_file_passes_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut skill = temp_skill("gone", dir.path());
        std::fs::remove_file(&skill.file_path).expect("remove skill file");
        skill.file_path = dir.path().join("missing-SKILL.md");
        let skills = [skill];
        let (expanded, used) = expand_skill_command("/skill:gone", &skills);
        assert_eq!(expanded, "/skill:gone");
        assert!(used.is_none());
    }

    #[test]
    fn prompt_formatting_excludes_hidden() {
        let skill = |name: &str, disable: bool| Skill {
            name: name.to_string(),
            description: format!("Does {name}"),
            file_path: PathBuf::from("/s/SKILL.md"),
            base_dir: PathBuf::from("/s"),
            source_info: create_synthetic_source_info("/s", "user", SourceScope::User, None),
            disable_model_invocation: disable,
            kind: SkillKind::Markdown,
        };
        let hidden =
            format_skills_for_prompt(&[skill("hidden", true)], &std::collections::BTreeMap::new());
        assert_eq!(hidden, "");
        let formatted = format_skills_for_prompt(
            &[skill("web-search", false)],
            &std::collections::BTreeMap::new(),
        );
        assert!(formatted.contains("<name>web-search</name>"));
        assert!(formatted.contains("<type>markdown</type>"));
        assert!(formatted.contains("<description>Does web-search</description>"));
    }

    /// The inventory marks a skill whose reuse is known, in the skills' own
    /// order, and leaves a new one unmarked.
    #[test]
    fn the_inventory_marks_known_reuse_without_reordering() {
        let skill = |name: &str| Skill {
            name: name.to_string(),
            description: format!("Does {name}"),
            file_path: PathBuf::from(format!("/s/{name}/SKILL.md")),
            base_dir: PathBuf::from(format!("/s/{name}")),
            source_info: create_synthetic_source_info("/s", "user", SourceScope::User, None),
            disable_model_invocation: false,
            kind: SkillKind::Markdown,
        };
        let standings = [
            ("stale".to_string(), crate::reuse::Standing::Unused),
            ("fresh".to_string(), crate::reuse::Standing::New),
            ("handy".to_string(), crate::reuse::Standing::Reused),
        ]
        .into_iter()
        .collect();
        let formatted = format_skills_for_prompt(
            &[skill("stale"), skill("fresh"), skill("handy")],
            &standings,
        );
        let marked: Vec<&str> = formatted
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("<name>") || line.starts_with("<reuse>"))
            .collect();
        assert_eq!(
            marked,
            vec![
                "<name>stale</name>",
                "<reuse>unused</reuse>",
                "<name>fresh</name>",
                "<name>handy</name>",
                "<reuse>reused</reuse>",
            ]
        );
    }
}
