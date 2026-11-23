//! The kernel's side of the reuse ledger ([`crate::reuse`]): the skill
//! files a cell reads and writes, and `(reuse-report)`.

use std::path::{Path, PathBuf};

use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::reuse::{Recorder, VisibleSkills};

/// The file a file-tool payload names, absolute.
fn payload_path(payload: &HostRequestPayload) -> Option<PathBuf> {
    let path = payload.data.get("path")?.as_str()?;
    let cwd = payload.data.get("cwd").and_then(serde_json::Value::as_str);
    Some(cwd.map_or_else(|| PathBuf::from(path), |cwd| Path::new(cwd).join(path)))
}

/// Wrap `handlers`' `fs.read` and `fs.write` so a read of a skill's file
/// counts as a use of the skill and a written `SKILL.md` as its creation.
/// Only a request that succeeded counts; a ledger that cannot be written
/// is logged and never fails the request.
pub(in crate::kernel::scheme) fn watch_skill_files(
    handlers: &mut HostRequestHandlers,
    recorder: &Recorder,
    skills: &VisibleSkills,
) {
    for request in ["fs.read", "fs.write"] {
        let Some(inner) = handlers.get(request).cloned() else {
            continue;
        };
        let (recorder, skills) = (recorder.clone(), skills.clone());
        handlers.register(
            request,
            host_handler(move |payload: HostRequestPayload| {
                let (inner, recorder, skills) = (inner.clone(), recorder.clone(), skills.clone());
                async move {
                    let path = payload_path(&payload);
                    let reply = inner(payload).await?;
                    if let Some(path) = path {
                        let noted = if request == "fs.read" {
                            recorder.note_read(&path, &skills())
                        } else {
                            recorder.note_write(&path)
                        };
                        if let Err(error) = noted {
                            tracing::warn!("the reuse ledger was not updated: {error:#}");
                        }
                    }
                    Ok(reply)
                }
            }),
        );
    }
}

/// Define `%reuse-report`: the rows of [`crate::reuse::report`] for this
/// session's skills and the global harness store, as Scheme hashes.
pub(super) fn register(engine: &mut Engine, session: &crate::kernel::scheme::SessionEnvironment) {
    let agent_dir = session.agent_dir.clone();
    let skills = session.visible_skills.clone();
    let session_id = session.session_id.clone();
    engine.register_fn("%reuse-report", move || -> SteelVal {
        let rows = agent_dir.as_deref().map_or_else(Vec::new, |agent_dir| {
            let skills = skills.as_ref().map_or_else(Vec::new, |skills| skills());
            crate::reuse::report(agent_dir, &skills, session_id.as_deref())
        });
        super::scheme_value(serde_json::to_value(rows).unwrap_or_default())
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::super::worktree::tests::evaluate;
    use super::*;
    use crate::kernel::scheme::skills::SkillSource;
    use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
    use crate::tools::kernel_cell::{KernelExecutor, KernelProvisioner};

    /// Session `id`'s kernel in `root`: the file tools, and the skills
    /// under `<root>/agent/skills` both listed and loaded.
    async fn session(root: &Path, id: &str) -> Box<dyn KernelExecutor> {
        let mut handlers = HostRequestHandlers::new();
        crate::fs_edit::FsEditHost {
            observer: None,
            seen: None,
            prefetch: None,
        }
        .register(&mut handlers);
        let skills_dir = root.join("agent").join("skills");
        let visible: VisibleSkills =
            Arc::new(move || crate::skills::load_skills_from_dir(&skills_dir, "user").skills);
        let sources = Arc::clone(&visible);
        SchemeKernelProvisioner::with_options(SchemeKernelOptions {
            host_handlers: handlers,
            cwd: root.to_path_buf(),
            agent_dir: Some(root.join("agent")),
            session_id: Some(id.to_string()),
            visible_skills: Some(visible),
            skill_sources: Some(Arc::new(move || SkillSource::from_skills(&sources()))),
            ..SchemeKernelOptions::default()
        })
        .ensure(None, None)
        .await
        .unwrap()
    }

    /// Session A builds a skill and a memory and uses both itself; sessions
    /// B and C read the skill and call its function. The report counts the
    /// skill's reuse from B and C alone, A's own use apart, and a skill and
    /// a memory used only in A, the session that made them, as no reuse.
    #[tokio::test(flavor = "multi_thread")]
    async fn reuse_counts_only_the_sessions_that_did_not_build_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("agent").join("skills");
        let skill_file = |name: &str| skills.join(name).join("SKILL.md").display().to_string();
        let a = session(dir.path(), "session-a").await;
        for cell in [
            format!(
                r#"(fs-write "{}" "---\nname: deploy-notes\ndescription: How to deploy\n---\nRun (deploy-notes-steps).\n")"#,
                skill_file("deploy-notes")
            ),
            format!(
                r#"(fs-write "{}" "(define (deploy-notes-steps) (list \"build\" \"ship\"))\n")"#,
                skills.join("deploy-notes").join("skill.scm").display()
            ),
            format!(
                r#"(fs-write "{}" "---\nname: scratch-only\ndescription: A one-off\n---\nOnce.\n")"#,
                skill_file("scratch-only")
            ),
            format!(r#"(fs-read "{}")"#, skill_file("deploy-notes")),
            format!(r#"(fs-read "{}")"#, skill_file("scratch-only")),
            r#"(harness-create-memory "Dev port" "the dev server listens on 8080" #:id "dev_port" #:global #t)"#.to_string(),
            r#"(harness-search "port" #:global #t)"#.to_string(),
        ] {
            evaluate(a.as_ref(), &cell).await.unwrap();
        }
        let b = session(dir.path(), "session-b").await;
        evaluate(
            b.as_ref(),
            &format!(r#"(fs-read "{}")"#, skill_file("deploy-notes")),
        )
        .await
        .unwrap();
        evaluate(b.as_ref(), "(deploy-notes-steps)").await.unwrap();
        let c = session(dir.path(), "session-c").await;
        evaluate(c.as_ref(), "(deploy-notes-steps)").await.unwrap();
        let reported = evaluate(
            a.as_ref(),
            "(map (lambda (row) (map (lambda (field) (hash-ref row field))
                                  '(key standing used-sessions uses own-uses created-here creator-known)))
                  (reuse-report))",
        )
        .await
        .unwrap();
        assert_eq!(
            reported,
            json!([
                ["skill/deploy-notes", "reused", 2, 3, 1, true, true],
                ["skill/scratch-only", "new", 0, 0, 1, true, true],
                ["harness/memory/dev_port", "new", 0, 0, 1, true, true],
            ])
        );
    }
}
