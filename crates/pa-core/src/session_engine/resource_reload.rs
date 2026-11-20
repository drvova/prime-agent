//! Re-reading a live session's resources: the skills, prompt templates, and
//! context files (`AGENTS.md`) the session assembled at creation, read again
//! from disk with the same discovery inputs. The kernel's `rlm.reload()`
//! (host request `rlm.reload`) and the embedding's `/reload` both land here.
//!
//! A reload:
//! - rebuilds the system prompt from the same inputs with the new skills and
//!   context files; the static layers are unchanged, so only the dynamic tail
//!   (and the prompt cache's tail) moves, from the next model call;
//! - makes `/skill:<name>`, the command catalog, and the resource snapshot
//!   read the new skill and template lists ([`ResourceReload::skills`]);
//! - installs Python skills that are new since the kernel booted into the
//!   kernel venv and records them, so a kernel restart binds them too; the
//!   reply names them so the kernel client binds them into the live REPL.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde_json::{json, Value};

use crate::kernel::bootstrap::KernelPythonSkill;
use crate::kernel::provisioner::IpythonKernelProvisioner;
use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::resources::{load_resources, ResourceLoaderOptions};
use crate::skills::{PromptTemplate, Skill};

/// The creation-time inputs a reload discovers and rebuilds from.
pub(crate) struct ReloadInputs {
    pub(crate) cwd: PathBuf,
    pub(crate) agent_dir: PathBuf,
    pub(crate) extra_builtin_skill_overrides: Vec<String>,
    pub(crate) additional_skill_paths: Vec<String>,
    pub(crate) additional_prompt_paths: Vec<String>,
    pub(crate) custom_system_prompt: Option<String>,
    pub(crate) prompt_guidelines: Vec<String>,
    /// Controller-backed skills dropped from the model's view at creation
    /// (their host request never registered); a reload drops them again.
    pub(crate) hidden_skills: Vec<String>,
    pub(crate) prompt: PromptInputs,
}

/// The system-prompt inputs a reload keeps (everything but the resources).
pub(crate) struct PromptInputs {
    pub(crate) model_prompt_extras: Option<String>,
    pub(crate) messages_path: Option<String>,
    pub(crate) selected_tools: Vec<String>,
    pub(crate) allow_recursion: Option<bool>,
    pub(crate) rlm_depth: Option<u32>,
    pub(crate) factory_child: bool,
    pub(crate) daemonless: bool,
    pub(crate) can_spawn: bool,
    pub(crate) generic_mcp_servers: Vec<String>,
    /// The project card the session started with (a reload keeps it).
    pub(crate) project_card: Option<String>,
}

/// The session's current resources.
#[derive(Clone)]
struct Current {
    skills: Vec<Skill>,
    prompt_templates: Vec<PromptTemplate>,
    /// The assembled prompt before the image-input line follows the model.
    base_prompt: String,
    python_skills: Vec<KernelPythonSkill>,
}

/// What one reload changed (the `rlm.reload` reply).
#[derive(Debug, Clone, PartialEq)]
pub struct ReloadReport {
    pub skills_added: Vec<String>,
    pub skills_removed: Vec<String>,
    pub skills: usize,
    pub prompt_templates: usize,
    pub context_files: Vec<String>,
    pub system_prompt_changed: bool,
    /// Python skills new since the kernel booted, to bind into the live REPL.
    pub python_skills: Vec<KernelPythonSkill>,
    /// Discovery warnings (bad names, collisions) and failed Python skill installs.
    pub diagnostics: Vec<String>,
}

impl ReloadReport {
    fn to_json(&self) -> Value {
        json!({
            "skillsAdded": self.skills_added,
            "skillsRemoved": self.skills_removed,
            "skills": self.skills,
            "promptTemplates": self.prompt_templates,
            "contextFiles": self.context_files,
            "systemPromptChanged": self.system_prompt_changed,
            "pythonSkills": self.python_skills.iter().map(|skill| json!({
                "name": skill.name,
                "importName": skill.import_name,
                "packagePath": skill.package_path,
            })).collect::<Vec<_>>(),
            "diagnostics": self.diagnostics,
        })
    }
}

/// Observes each finished reload (adoption telemetry): the count of skills
/// it added, never a name.
pub(crate) type ReloadObserver = Arc<dyn Fn(usize) + Send + Sync>;

/// One session's reload state, owned by the engine. Built before the kernel
/// (its handler registers into the kernel's registry) and seeded once the
/// session has assembled its prompt.
pub struct ResourceReload {
    inputs: OnceLock<ReloadInputs>,
    current: Mutex<Current>,
    /// Serializes reloads: two concurrent ones must not diff against the same state.
    running: tokio::sync::Mutex<()>,
    provisioner: OnceLock<Weak<IpythonKernelProvisioner>>,
    turn_boundary: Weak<super::turn_boundary::TurnBoundaryRequests>,
    observer: Option<ReloadObserver>,
}

impl ResourceReload {
    pub(crate) fn new(
        turn_boundary: &Arc<super::turn_boundary::TurnBoundaryRequests>,
        observer: Option<ReloadObserver>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inputs: OnceLock::new(),
            current: Mutex::new(Current {
                skills: Vec::new(),
                prompt_templates: Vec::new(),
                base_prompt: String::new(),
                python_skills: Vec::new(),
            }),
            running: tokio::sync::Mutex::new(()),
            provisioner: OnceLock::new(),
            turn_boundary: Arc::downgrade(turn_boundary),
            observer,
        })
    }

    /// Seed the creation-time inputs and resources (first seed wins).
    pub(crate) fn seed(
        &self,
        inputs: ReloadInputs,
        skills: Vec<Skill>,
        prompt_templates: Vec<PromptTemplate>,
        base_prompt: String,
        python_skills: Vec<KernelPythonSkill>,
    ) {
        if self.inputs.set(inputs).is_ok() {
            *self.current() = Current {
                skills,
                prompt_templates,
                base_prompt,
                python_skills,
            };
        }
    }

    fn current(&self) -> std::sync::MutexGuard<'_, Current> {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Bind the session's kernel (first bind wins): new Python skills install
    /// into its venv and join its restart bootstrap.
    pub(crate) fn bind_provisioner(&self, provisioner: &Arc<IpythonKernelProvisioner>) {
        let _ = self.provisioner.set(Arc::downgrade(provisioner));
    }

    /// The session's current skills (every discovered one, as `/skill:` and the catalog list them).
    #[must_use]
    pub fn skills(&self) -> Vec<Skill> {
        self.current().skills.clone()
    }

    /// The session's current prompt templates.
    #[must_use]
    pub fn prompt_templates(&self) -> Vec<PromptTemplate> {
        self.current().prompt_templates.clone()
    }

    /// The assembled system prompt before the image-input line follows the model.
    #[must_use]
    pub fn base_prompt(&self) -> String {
        self.current().base_prompt.clone()
    }

    /// Register `rlm.reload`.
    pub(crate) fn register(self: &Arc<Self>, handlers: &mut HostRequestHandlers) {
        // Weak: the kernel holds its handlers for its whole life.
        let reload = Arc::downgrade(self);
        handlers.register(
            "rlm.reload",
            host_handler(move |_payload| {
                let reload = reload.clone();
                async move {
                    let Some(reload) = reload.upgrade() else {
                        anyhow::bail!("the session ended before the request could be served");
                    };
                    Ok(reload.reload().await?.to_json())
                }
            }),
        );
    }

    /// Re-read the resources from disk and apply them (see the module docs).
    ///
    /// # Errors
    ///
    /// Resource discovery failing (an unreadable settings file), or the
    /// discovery thread panicking.
    #[tracing::instrument(skip_all)]
    pub async fn reload(&self) -> anyhow::Result<ReloadReport> {
        let _running = self.running.lock().await;
        let Some(seeded) = self.inputs.get() else {
            anyhow::bail!("the session is still starting; try again");
        };
        let inputs = ResourceLoaderOptions {
            cwd: seeded.cwd.clone(),
            agent_dir: seeded.agent_dir.clone(),
            // Fresh settings: a `skills` path added since creation counts.
            settings: None,
            extra_builtin_skill_overrides: seeded.extra_builtin_skill_overrides.clone(),
            additional_skill_paths: seeded.additional_skill_paths.clone(),
            additional_prompt_paths: seeded.additional_prompt_paths.clone(),
            system_prompt: seeded.custom_system_prompt.clone(),
            append_system_prompt: seeded.prompt_guidelines.clone(),
            ..Default::default()
        };
        let resources = tokio::task::spawn_blocking(move || load_resources(inputs))
            .await
            .map_err(|error| anyhow::anyhow!("resource discovery failed: {error}"))??;
        let mut diagnostics: Vec<String> = resources
            .skill_diagnostics
            .iter()
            .map(|diagnostic| match diagnostic {
                crate::skills::ResourceDiagnostic::Warning { message, .. }
                | crate::skills::ResourceDiagnostic::Error { message, .. }
                | crate::skills::ResourceDiagnostic::Collision { message, .. } => message.clone(),
            })
            .collect();
        let visible: Vec<Skill> = resources
            .skills
            .iter()
            .filter(|skill| !seeded.hidden_skills.contains(&skill.name))
            .cloned()
            .collect();
        let bound = self
            .turn_boundary
            .upgrade()
            .and_then(|requests| requests.bound());
        let vision_capable = bound.as_ref().map(|runtime| {
            runtime
                .model_info
                .input
                .contains(&pa_types::ai::ModelInput::Image)
        });
        let prompt = &seeded.prompt;
        let base_prompt = crate::prompts::system_prompt::build_system_prompt(
            &crate::prompts::system_prompt::BuildSystemPromptOptions {
                custom_prompt: resources.system_prompt.clone(),
                model_prompt_extras: prompt.model_prompt_extras.as_deref(),
                vision_capable,
                cwd: seeded.cwd.display().to_string(),
                messages_path: prompt.messages_path.clone(),
                context_files: resources
                    .agents_files
                    .iter()
                    .map(|file| (file.path.display().to_string(), file.content.clone()))
                    .collect(),
                skills: visible.clone(),
                selected_tools: Some(prompt.selected_tools.iter().map(String::as_str).collect()),
                allow_recursion: prompt.allow_recursion,
                rlm_depth: prompt.rlm_depth,
                factory_child: prompt.factory_child,
                daemonless: prompt.daemonless,
                can_spawn: prompt.can_spawn,
                generic_mcp_servers: prompt.generic_mcp_servers.clone(),
                project_card: prompt.project_card.clone(),
                append_system_prompt: Some(resources.append_system_prompt.join("\n\n")),
                ..Default::default()
            },
        );
        let python_skills = super::runtime_wiring::kernel_python_skills(&visible);
        let previous = self.current().clone();
        let new_python: Vec<KernelPythonSkill> = python_skills
            .iter()
            .filter(|skill| {
                !previous
                    .python_skills
                    .iter()
                    .any(|known| known.import_name == skill.import_name)
            })
            .cloned()
            .collect();
        if !new_python.is_empty() {
            if let Some(provisioner) = self.provisioner.get().and_then(Weak::upgrade) {
                if let Err(error) = provisioner
                    .add_python_skills(&python_skills, &new_python)
                    .await
                {
                    diagnostics.push(format!(
                        "installing the new Python skills failed: {error:#}"
                    ));
                }
            }
        }
        let names = |skills: &[Skill]| {
            skills
                .iter()
                .map(|skill| skill.name.clone())
                .collect::<Vec<_>>()
        };
        let (old_names, new_names) = (names(&previous.skills), names(&resources.skills));
        let report = ReloadReport {
            skills_added: new_names
                .iter()
                .filter(|name| !old_names.contains(name))
                .cloned()
                .collect(),
            skills_removed: old_names
                .iter()
                .filter(|name| !new_names.contains(name))
                .cloned()
                .collect(),
            skills: resources.skills.len(),
            prompt_templates: resources.prompts.len(),
            context_files: resources
                .agents_files
                .iter()
                .map(|file| file.path.display().to_string())
                .collect(),
            system_prompt_changed: base_prompt != previous.base_prompt,
            python_skills: new_python,
            diagnostics,
        };
        if let Some(runtime) = &bound {
            if report.system_prompt_changed {
                runtime.agent.set_system_prompt(base_prompt.clone()).await;
            }
        }
        *self.current() = Current {
            skills: resources.skills,
            prompt_templates: resources.prompts,
            base_prompt,
            python_skills: previous
                .python_skills
                .into_iter()
                .chain(report.python_skills.iter().cloned())
                .collect(),
        };
        if let Some(observer) = &self.observer {
            observer(report.skills_added.len());
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
