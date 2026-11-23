//! Skill sources on a live kernel: loading at start, the per-skill failure
//! report, and loading again after a reload.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::{notice_content, notice_message, SkillSource, UnavailableSchemeSkills};
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::skills::{create_synthetic_source_info, Skill, SkillKind, SourceScope};
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelExecutor, KernelProvisioner};

/// Skill directories under a temporary root, the list of skills a kernel
/// is told about, and every unavailable-skills report it made.
struct Skills {
    root: tempfile::TempDir,
    listed: Arc<Mutex<Vec<SkillSource>>>,
    reports: Arc<Mutex<Vec<UnavailableSchemeSkills>>>,
}

impl Skills {
    fn new() -> Self {
        Skills {
            root: tempfile::tempdir().unwrap(),
            listed: Arc::default(),
            reports: Arc::default(),
        }
    }

    /// Write one file of a skill, listing the skill if it is new.
    fn write(&self, skill: &str, file: &str, text: &str) {
        let dir = self.root.path().join(skill);
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        let mut listed = self.listed.lock().unwrap();
        if !listed.iter().any(|known| known.name == skill) {
            listed.push(SkillSource {
                name: skill.to_string(),
                dir,
            });
        }
    }

    /// A kernel that loads these skills and answers `rlm.reload`.
    async fn kernel(&self) -> Box<dyn KernelExecutor> {
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "rlm.reload",
            host_handler(|_payload: HostRequestPayload| async { Ok(json!({"skills": 0})) }),
        );
        let (listed, reports) = (Arc::clone(&self.listed), Arc::clone(&self.reports));
        SchemeKernelProvisioner::with_options(SchemeKernelOptions {
            host_handlers: handlers,
            cwd: PathBuf::from("/"),
            skill_sources: Some(Arc::new(move || listed.lock().unwrap().clone())),
            on_unavailable_skills: Some(Arc::new(move |unavailable: &UnavailableSchemeSkills| {
                reports.lock().unwrap().push(unavailable.clone());
            })),
            ..SchemeKernelOptions::default()
        })
        .ensure(None, None)
        .await
        .unwrap()
    }

    fn reports(&self) -> Vec<UnavailableSchemeSkills> {
        self.reports.lock().unwrap().clone()
    }
}

/// What an expression evaluates to, as JSON, or the error it raised.
async fn value(kernel: &dyn KernelExecutor, expression: &str) -> Result<Value, String> {
    let cell = format!("(display (value->jsexpr-string {expression}))");
    let result = kernel
        .execute(
            &cell,
            KernelExecuteOptions {
                signal: None,
                timeout_ms: None,
                on_stream: None,
                on_late_sent_agent_message: None,
            },
        )
        .await
        .unwrap();
    match result.error {
        None => Ok(serde_json::from_str(&result.stdout).unwrap()),
        Some(error) => Err(error.evalue),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skills_scheme_source_is_loaded_at_kernel_start_and_a_markdown_skill_is_left_alone() {
    let skills = Skills::new();
    // The `scheme/` files load first, in name order, then `skill.scm`.
    skills.write(
        "measure",
        "scheme/20-area.scm",
        "(define (measure-area side) (measure-square side))",
    );
    skills.write(
        "measure",
        "scheme/10-square.scm",
        "(define (measure-square n) (* n n))",
    );
    skills.write(
        "measure",
        "skill.scm",
        "(displayln \"loading measure\") (define measure-unit-area (measure-area 1))",
    );
    skills.write(
        "measure",
        "SKILL.md",
        "---\nname: measure\ndescription: Measures.\n---\n",
    );
    skills.write(
        "notes",
        "SKILL.md",
        "---\nname: notes\ndescription: Documentation only.\n---\n",
    );
    skills.write("notes", "reference.md", "(this is not Scheme source");
    let kernel = skills.kernel().await;
    let kernel = kernel.as_ref();
    assert_eq!(
        (
            value(kernel, "(list (measure-area 12) measure-unit-area)").await,
            value(kernel, "(skills-unavailable)").await,
            skills.reports(),
        ),
        (Ok(json!([144, 1])), Ok(json!([])), Vec::new())
    );
}

/// A skill's functions get the kernel's call contracts; one that takes a
/// rest list reads it as it likes.
#[tokio::test(flavor = "multi_thread")]
async fn a_skills_function_refuses_an_option_it_does_not_declare() {
    let skills = Skills::new();
    skills.write(
        "lint",
        "skill.scm",
        "(define (lint-run path #:fix [fix #f]) (list path fix))
         (define (lint-all . paths) paths)",
    );
    let kernel = skills.kernel().await;
    let kernel = kernel.as_ref();
    assert_eq!(
        (
            value(kernel, "(lint-run \"a.rs\" #:fix #t)").await,
            value(kernel, "(lint-run \"a.rs\" #:fxi #t)").await,
            value(kernel, "(lint-all \"a.rs\" \"#:fxi\")").await,
        ),
        (
            Ok(json!(["a.rs", true])),
            Err(
                "Error: Generic: lint-run: unknown option #:fxi. Did you mean #:fix? lint-run accepts #:fix"
                    .to_string()
            ),
            Ok(json!(["a.rs", "#:fxi"])),
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_whose_source_fails_is_reported_by_name_and_the_other_skills_still_load() {
    let skills = Skills::new();
    skills.write(
        "first",
        "skill.scm",
        "(define (first-ready) \"yes\")\n(car 5)",
    );
    skills.write("torn", "scheme/10-open.scm", "(define (torn-open");
    skills.write("torn", "skill.scm", "(define (torn-never-loaded) 1)");
    skills.write("last", "skill.scm", "(define (last-ready) \"yes\")");
    let kernel = skills.kernel().await;
    let kernel = kernel.as_ref();
    let reports = skills.reports();
    let unavailable = reports.first().cloned().unwrap_or_default();
    let named: Vec<(&str, bool, bool)> = unavailable
        .iter()
        .map(|(skill, error)| {
            (
                skill.as_str(),
                error.starts_with("skill.scm: "),
                error.starts_with("scheme/10-open.scm: "),
            )
        })
        .collect();
    let listed = value(
        kernel,
        "(map (lambda (row) (hash-ref row 'skill)) (skills-unavailable))",
    )
    .await;
    assert_eq!(
        (
            reports.len(),
            named,
            unavailable[0].1.contains("car expected a list or pair"),
            listed,
            value(kernel, "(last-ready)").await,
            value(kernel, "(torn-never-loaded)").await.is_err(),
        ),
        (
            1,
            vec![("first", true, false), ("torn", false, true)],
            true,
            Ok(json!(["first", "torn"])),
            Ok(json!("yes")),
            true,
        )
    );
}

/// A source that never returns holds no cell, so no cell's stop reaches it:
/// it is interrupted at the loader's limit, its skill is reported, and the
/// kernel starts with the skills after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_skill_source_that_never_returns_is_stopped_and_the_kernel_still_starts() {
    let skills = Skills::new();
    skills.write(
        "endless",
        "skill.scm",
        "(define (spin n) (spin (+ n 1)))\n(spin 0)",
    );
    skills.write("after", "skill.scm", "(define (after-ready) \"yes\")");
    // This test's future stays on this thread, where the loader is made.
    let _scope = pa_types::session_scope::enter(Some(pa_types::session_scope::SessionScope::new(
        "endless-skill".to_string(),
        std::env::temp_dir(),
        std::collections::BTreeMap::from([(
            "PRIME_AGENT_SKILL_LOAD_LIMIT_MS".to_string(),
            "100".to_string(),
        )]),
        std::collections::BTreeSet::new(),
    )));
    let kernel = skills.kernel().await;
    let unavailable: Vec<String> = skills
        .reports()
        .into_iter()
        .flatten()
        .map(|(skill, _)| skill)
        .collect();
    assert_eq!(
        (unavailable, value(kernel.as_ref(), "(after-ready)").await),
        (vec!["endless".to_string()], Ok(json!("yes")))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_loads_new_and_changed_sources_after_its_cell_and_reports_what_still_fails() {
    let skills = Skills::new();
    skills.write("greet", "skill.scm", "(define (greet-word) \"hello\")");
    skills.write("torn", "skill.scm", "(define (torn-open");
    let kernel = skills.kernel().await;
    let kernel = kernel.as_ref();
    let before = value(kernel, "(greet-word)").await;
    // Changed, fixed, and new since the kernel started.
    skills.write("greet", "skill.scm", "(define (greet-word) \"good day\")");
    skills.write("torn", "skill.scm", "(define (torn-open) \"mended\")");
    skills.write(
        "added",
        "skill.scm",
        "(define (added-here) \"new\")\n(vector-ref (vector) 3)",
    );
    let unloaded_until_reload = value(kernel, "(greet-word)").await;
    let reloaded = value(kernel, "(hash-ref (rlm-reload) (quote skills))").await;
    let after = value(kernel, "(list (greet-word) (torn-open) (added-here))").await;
    let still_failing = value(
        kernel,
        "(map (lambda (row) (hash-ref row 'skill)) (skills-unavailable))",
    )
    .await;
    let reported: Vec<Vec<String>> = skills
        .reports()
        .iter()
        .map(|report| report.iter().map(|(skill, _)| skill.clone()).collect())
        .collect();
    assert_eq!(
        (
            before,
            unloaded_until_reload,
            reloaded,
            after,
            still_failing,
            reported
        ),
        (
            Ok(json!("hello")),
            Ok(json!("hello")),
            Ok(json!(0)),
            Ok(json!(["good day", "mended", "new"])),
            Ok(json!(["added"])),
            vec![vec!["torn".to_string()], vec!["added".to_string()]],
        )
    );
}

#[test]
fn a_skill_the_project_ships_is_never_a_scheme_source() {
    let skill = |name: &str, scope: SourceScope| Skill {
        name: name.to_string(),
        description: String::new(),
        file_path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        base_dir: PathBuf::from(format!("/skills/{name}")),
        source_info: create_synthetic_source_info("SKILL.md", "test", scope, None),
        disable_model_invocation: false,
        kind: SkillKind::Markdown,
    };
    assert_eq!(
        SkillSource::from_skills(&[
            skill("mine", SourceScope::User),
            skill("checked-out", SourceScope::Project),
            skill("bundled", SourceScope::Temporary),
        ]),
        vec![
            SkillSource {
                name: "mine".to_string(),
                dir: Path::new("/skills/mine").to_path_buf()
            },
            SkillSource {
                name: "bundled".to_string(),
                dir: Path::new("/skills/bundled").to_path_buf()
            },
        ]
    );
}

#[test]
fn the_notice_names_each_unavailable_skill_with_its_error_and_the_way_to_load_it_again() {
    let errors = vec![
        (
            "websearch".to_string(),
            "skill.scm: FreeIdentifier: web-get".to_string(),
        ),
        (
            "lint".to_string(),
            "scheme/10-rules.scm: Parse: unexpected EOF".to_string(),
        ),
    ];
    let message = notice_message(&errors);
    assert_eq!(
        (
            notice_content(&errors),
            message.custom_type,
            message.display,
            message.details,
        ),
        (
            "[scheme-skills-unavailable]\n\nThese skills' Scheme source failed to load into the Scheme kernel, so the functions they define are missing or incomplete:\n- websearch: skill.scm: FreeIdentifier: web-get\n- lint: scheme/10-rules.scm: Parse: unexpected EOF\n\nFix the source and call `(rlm-reload)` to load it again, or use another approach. `(skills-unavailable)` lists what is still failing; each skill's SKILL.md stays readable.".to_string(),
            "scheme_skills_unavailable".to_string(),
            true,
            Some(json!({"skills": ["websearch", "lint"]})),
        )
    );
}
