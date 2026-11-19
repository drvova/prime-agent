//! The kernel `checkpoint.create/list/restore` host requests: snapshots of the session's
//! workspace the model takes before risky edits, and the gated way back.
//!
//! The snapshots live in the embedding's [`WorkspaceCheckpointController`] (the daemon worker's
//! shadow-git store, the one `/undo` and rewind use). `checkpoint.restore` is destructive, so it
//! runs only when the user turned on the global `replCanRestoreCheckpoints` setting, only for a
//! checkpoint this session created, and only after the user confirms through the session's
//! question flow: a declined, timed-out or cancelled question aborts, and a run with nobody to
//! ask (headless, a subagent) refuses. An embedding without a controller (headless and
//! daemonless runs) answers every request with an error naming why.

use std::sync::Arc;

use anyhow::bail;
use pa_types::question::{Question, QuestionKind, QuestionStatus};
use serde_json::{json, Value};

use crate::interaction::InteractionHub;
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// The refusal of an embedding without a checkpoint store.
const NO_CHECKPOINTS: &str = "workspace checkpoints need the prime-agent daemon, which keeps \
     them; this run is headless or daemonless, so none was taken";

/// The refusal while `replCanRestoreCheckpoints` is off.
pub const RESTORE_DISABLED_MESSAGE: &str = "checkpoint.restore is off: the user can allow it by \
     setting \"replCanRestoreCheckpoints\": true in the global settings.json";

/// The confirmation's choices: restore, or keep the files as they are.
const RESTORE_CHOICE: &str = "Restore";
const KEEP_CHOICE: &str = "Keep current files";

/// The most changed paths the confirmation names.
const QUESTION_PATHS: usize = 12;

/// One checkpoint this session took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCheckpoint {
    pub id: String,
    pub label: String,
    /// Milliseconds since the epoch.
    pub created_at_ms: u64,
}

/// What a restore did: the paths it wrote or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRestore {
    pub restored: usize,
}

/// The embedding's workspace checkpoint store for one session.
///
/// Every method may block (it runs git over the workspace); the host handlers call them on a
/// blocking thread. Implementations scope everything to the session's own workspace and to the
/// checkpoints this session created: an id from another session, or from the store's per-turn
/// rewind records, is unknown. Errors are user-presentable (`anyhow` text reaches the kernel).
pub trait WorkspaceCheckpointController: Send + Sync {
    /// Snapshot the workspace now, under `label`.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace cannot be checkpointed (not a project directory, too
    /// large, git missing) or the record cannot be written.
    fn create(&self, label: &str) -> anyhow::Result<WorkspaceCheckpoint>;

    /// This session's checkpoints, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be opened.
    fn list(&self) -> anyhow::Result<Vec<WorkspaceCheckpoint>>;

    /// The workspace paths restoring `id` would write or remove, without changing anything.
    ///
    /// # Errors
    ///
    /// Returns an error when `id` is not one of this session's checkpoints or git fails.
    fn changes(&self, id: &str) -> anyhow::Result<Vec<String>>;

    /// Put the workspace back to checkpoint `id`.
    ///
    /// # Errors
    ///
    /// Returns an error when `id` is not one of this session's checkpoints or a write fails.
    fn restore(&self, id: &str) -> anyhow::Result<CheckpointRestore>;

    /// Whether the user allowed `checkpoint.restore` (`replCanRestoreCheckpoints`, global
    /// settings only, so a project cannot turn it on), read fresh per call.
    fn restore_enabled(&self) -> bool;
}

/// One `checkpoint.*` call, as adoption telemetry counts it (never a label or a path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointFact {
    Created,
    Listed,
    Restored,
    /// The user declined, the question timed out or was cancelled, or nobody could answer.
    RestoreDeclined,
    /// `replCanRestoreCheckpoints` is off.
    RestoreDisabled,
    /// Any other refusal or failure (no store, unknown id, git error).
    Failed,
}

/// Register `checkpoint.create`, `checkpoint.list`, and `checkpoint.restore`. `None` is an
/// embedding without a store: every request answers why.
pub fn register_workspace_checkpoint_host_handlers(
    controller: Option<Arc<dyn WorkspaceCheckpointController>>,
    interaction: Arc<InteractionHub>,
    handlers: &mut HostRequestHandlers,
) {
    let host = Arc::new(CheckpointHost {
        controller,
        interaction,
    });
    for request_type in ["checkpoint.create", "checkpoint.list", "checkpoint.restore"] {
        let host = Arc::clone(&host);
        handlers.register(
            request_type,
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { host.handle(request_type, payload.data).await }
            }),
        );
    }
}

struct CheckpointHost {
    controller: Option<Arc<dyn WorkspaceCheckpointController>>,
    interaction: Arc<InteractionHub>,
}

impl CheckpointHost {
    #[tracing::instrument(skip(self, data))]
    async fn handle(&self, request_type: &'static str, data: Value) -> anyhow::Result<Value> {
        let outcome = self.run(request_type, &data).await;
        let fact = match &outcome {
            Ok((_, fact)) => *fact,
            Err(_) => CheckpointFact::Failed,
        };
        if let Some(counters) = self.interaction.counters() {
            counters.note_repl_checkpoint(fact);
        }
        outcome.map(|(response, _)| response)
    }

    async fn run(
        &self,
        request_type: &'static str,
        data: &Value,
    ) -> anyhow::Result<(Value, CheckpointFact)> {
        let Some(controller) = self.controller.clone() else {
            bail!("{NO_CHECKPOINTS}");
        };
        let text = |key: &str| {
            data.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match request_type {
            "checkpoint.create" => {
                let label = text("label");
                let checkpoint = blocking(move || controller.create(label.trim())).await?;
                Ok((
                    json!({ "checkpoint": checkpoint_response(&checkpoint) }),
                    CheckpointFact::Created,
                ))
            }
            "checkpoint.list" => {
                let checkpoints = blocking(move || controller.list()).await?;
                Ok((
                    json!({
                        "checkpoints": checkpoints.iter().map(checkpoint_response).collect::<Vec<_>>(),
                    }),
                    CheckpointFact::Listed,
                ))
            }
            "checkpoint.restore" => {
                if !controller.restore_enabled() {
                    return Ok((
                        json!({ "status": "disabled", "message": RESTORE_DISABLED_MESSAGE }),
                        CheckpointFact::RestoreDisabled,
                    ));
                }
                let id = text("id");
                let question_id = text("question_id");
                if question_id.is_empty() {
                    bail!("checkpoint.restore requires a question_id");
                }
                let (checkpoints, changes) = {
                    let controller = Arc::clone(&controller);
                    let id = id.clone();
                    blocking(move || Ok((controller.list()?, controller.changes(&id)?))).await?
                };
                let Some(checkpoint) = checkpoints.into_iter().find(|entry| entry.id == id) else {
                    bail!("this session has no checkpoint {id:?}");
                };
                if changes.is_empty() {
                    return Ok((
                        json!({ "status": "restored", "restored": 0 }),
                        CheckpointFact::Restored,
                    ));
                }
                let mut listed = changes
                    .iter()
                    .take(QUESTION_PATHS)
                    .map(|path| format!("  {path}"))
                    .collect::<Vec<_>>();
                if changes.len() > QUESTION_PATHS {
                    listed.push(format!("  ... and {} more", changes.len() - QUESTION_PATHS));
                }
                let question = Question {
                    id: question_id,
                    text: format!(
                        "The agent wants to restore workspace checkpoint \"{}\", which rewrites \
                         or removes {} path(s):\n{}",
                        checkpoint.label,
                        changes.len(),
                        listed.join("\n")
                    ),
                    kind: QuestionKind::Single,
                    choices: vec![RESTORE_CHOICE.to_string(), KEEP_CHOICE.to_string()],
                    default: vec![KEEP_CHOICE.to_string()],
                    timeout_seconds: None,
                };
                let (status, answer) = self.interaction.ask(question).await?;
                let confirmed = match status {
                    QuestionStatus::Answered => {
                        answer.first().map(String::as_str) == Some(RESTORE_CHOICE)
                    }
                    QuestionStatus::Declined
                    | QuestionStatus::TimedOut
                    | QuestionStatus::NoUser
                    | QuestionStatus::Cancelled => false,
                };
                if !confirmed {
                    let status = match status {
                        QuestionStatus::NoUser => "no_user",
                        QuestionStatus::Answered
                        | QuestionStatus::Declined
                        | QuestionStatus::TimedOut
                        | QuestionStatus::Cancelled => "declined",
                    };
                    return Ok((
                        json!({ "status": status, "restored": 0 }),
                        CheckpointFact::RestoreDeclined,
                    ));
                }
                let restore = blocking(move || controller.restore(&id)).await?;
                Ok((
                    json!({ "status": "restored", "restored": restore.restored }),
                    CheckpointFact::Restored,
                ))
            }
            other => bail!("unknown checkpoint request type {other:?}"),
        }
    }
}

/// Run one controller call on a blocking thread (git walks the workspace).
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| anyhow::anyhow!("checkpoint thread: {error}"))?
}

/// The `snake_case` checkpoint the kernel's `checkpoint` client parses.
fn checkpoint_response(checkpoint: &WorkspaceCheckpoint) -> Value {
    json!({
        "id": checkpoint.id,
        "label": checkpoint.label,
        "created_at": crate::session::manager::format_iso(
            i64::try_from(checkpoint.created_at_ms).unwrap_or(i64::MAX),
        ),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock, Weak};

    use pa_types::question::QuestionReply;
    use pa_types::todo::TodoList;

    use super::*;
    use crate::interaction::{InteractionEvent, InteractionSurface};
    use crate::kernel::shared::HostRequestPayload;

    /// An in-memory store with one checkpoint (`cp-1`) whose restore changes two paths.
    struct FakeStore {
        enabled: bool,
        restores: Mutex<Vec<String>>,
    }

    impl WorkspaceCheckpointController for FakeStore {
        fn create(&self, label: &str) -> anyhow::Result<WorkspaceCheckpoint> {
            Ok(WorkspaceCheckpoint {
                id: "cp-2".to_string(),
                label: label.to_string(),
                created_at_ms: 0,
            })
        }

        fn list(&self) -> anyhow::Result<Vec<WorkspaceCheckpoint>> {
            Ok(vec![WorkspaceCheckpoint {
                id: "cp-1".to_string(),
                label: "before refactor".to_string(),
                created_at_ms: 0,
            }])
        }

        fn changes(&self, id: &str) -> anyhow::Result<Vec<String>> {
            match id {
                "cp-1" => Ok(vec!["main.rs".to_string(), "new/added.rs".to_string()]),
                other => bail!("this session has no checkpoint {other:?}"),
            }
        }

        fn restore(&self, id: &str) -> anyhow::Result<CheckpointRestore> {
            self.restores.lock().unwrap().push(id.to_string());
            Ok(CheckpointRestore { restored: 2 })
        }

        fn restore_enabled(&self) -> bool {
            self.enabled
        }
    }

    /// A user who answers every question with `reply` and records what was asked.
    struct AnsweringUser {
        hub: OnceLock<Weak<InteractionHub>>,
        reply: QuestionReply,
        asked: Mutex<Vec<Question>>,
    }

    impl InteractionSurface for AnsweringUser {
        fn user_available(&self) -> bool {
            true
        }

        fn persisted_todos(&self) -> Option<TodoList> {
            None
        }

        fn publish(&self, event: InteractionEvent) {
            if let InteractionEvent::QuestionOpened(question) = event {
                self.asked.lock().unwrap().push(question.clone());
                let hub = self.hub.get().and_then(Weak::upgrade).expect("hub");
                hub.answer(&question.id, self.reply.clone()).unwrap();
            }
        }
    }

    fn setup(
        enabled: bool,
        user: Option<QuestionReply>,
    ) -> (
        HostRequestHandlers,
        Arc<FakeStore>,
        Option<Arc<AnsweringUser>>,
    ) {
        let store = Arc::new(FakeStore {
            enabled,
            restores: Mutex::new(Vec::new()),
        });
        let user = user.map(|reply| {
            Arc::new(AnsweringUser {
                hub: OnceLock::new(),
                reply,
                asked: Mutex::new(Vec::new()),
            })
        });
        let hub = match &user {
            Some(user) => {
                let hub =
                    InteractionHub::with_surface(Arc::clone(user) as Arc<dyn InteractionSurface>);
                let _ = user.hub.set(Arc::downgrade(&hub));
                hub
            }
            None => InteractionHub::headless(),
        };
        let mut handlers = HostRequestHandlers::default();
        register_workspace_checkpoint_host_handlers(
            Some(Arc::clone(&store) as Arc<dyn WorkspaceCheckpointController>),
            hub,
            &mut handlers,
        );
        (handlers, store, user)
    }

    async fn call(
        handlers: &HostRequestHandlers,
        request_type: &str,
        data: Value,
    ) -> anyhow::Result<Value> {
        let handler = handlers.get(request_type).expect("registered").clone();
        handler(HostRequestPayload { data }).await
    }

    fn restore_request(id: &str) -> Value {
        json!({ "id": id, "question_id": "q-1" })
    }

    /// A confirmed restore runs after the user saw the changed paths; a declined one changes
    /// nothing.
    #[tokio::test]
    async fn restore_runs_only_after_the_user_confirms() {
        let confirm = QuestionReply::Answered {
            answer: vec![RESTORE_CHOICE.to_string()],
        };
        let (handlers, store, user) = setup(true, Some(confirm));
        assert_eq!(
            call(&handlers, "checkpoint.restore", restore_request("cp-1"))
                .await
                .unwrap(),
            json!({ "status": "restored", "restored": 2 })
        );
        assert_eq!(*store.restores.lock().unwrap(), vec!["cp-1".to_string()]);
        assert_eq!(
            *user.unwrap().asked.lock().unwrap(),
            vec![Question {
                id: "q-1".to_string(),
                text: "The agent wants to restore workspace checkpoint \"before refactor\", which \
                       rewrites or removes 2 path(s):\n  main.rs\n  new/added.rs"
                    .to_string(),
                kind: QuestionKind::Single,
                choices: vec![RESTORE_CHOICE.to_string(), KEEP_CHOICE.to_string()],
                default: vec![KEEP_CHOICE.to_string()],
                timeout_seconds: None,
            }]
        );

        let (handlers, store, _) = setup(true, Some(QuestionReply::Declined));
        assert_eq!(
            call(&handlers, "checkpoint.restore", restore_request("cp-1"))
                .await
                .unwrap(),
            json!({ "status": "declined", "restored": 0 })
        );
        assert!(store.restores.lock().unwrap().is_empty());
    }

    /// Off by default, nobody to ask, or an id this session never took: nothing is restored.
    #[tokio::test]
    async fn restore_refuses_without_the_setting_a_user_or_its_own_checkpoint() {
        let (handlers, store, _) = setup(false, None);
        assert_eq!(
            call(&handlers, "checkpoint.restore", restore_request("cp-1"))
                .await
                .unwrap(),
            json!({ "status": "disabled", "message": RESTORE_DISABLED_MESSAGE })
        );
        let (headless, headless_store, _) = setup(true, None);
        assert_eq!(
            call(&headless, "checkpoint.restore", restore_request("cp-1"))
                .await
                .unwrap(),
            json!({ "status": "no_user", "restored": 0 })
        );
        assert_eq!(
            call(&headless, "checkpoint.restore", restore_request("other"))
                .await
                .unwrap_err()
                .to_string(),
            "this session has no checkpoint \"other\""
        );
        assert!(store.restores.lock().unwrap().is_empty());
        assert!(headless_store.restores.lock().unwrap().is_empty());
    }

    /// Without a store every request says why.
    #[tokio::test]
    async fn without_a_store_every_request_refuses_with_the_reason() {
        let mut handlers = HostRequestHandlers::default();
        register_workspace_checkpoint_host_handlers(
            None,
            InteractionHub::headless(),
            &mut handlers,
        );
        for (request_type, data) in [
            ("checkpoint.create", json!({ "label": "x" })),
            ("checkpoint.list", json!({})),
            ("checkpoint.restore", restore_request("cp-1")),
        ] {
            assert_eq!(
                call(&handlers, request_type, data)
                    .await
                    .unwrap_err()
                    .to_string(),
                NO_CHECKPOINTS
            );
        }
    }
}
