//! The session credential pin at create: `--credential` writes it into a fresh header, a resume
//! restores the header's pin as-is, and a subagent inherits its parent's.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pa_types::session::CredentialPin;

use super::*;
use crate::engine::{
    BranchSummaryOutcome, BranchSummaryRequest, CompactionOutcome, CompactionRequest, EngineEvent,
    PromptRequest, ScriptedEngine,
};
use crate::worker::WorkerConfig;

/// A scripted engine that keeps the pin the create hands it.
#[derive(Default)]
struct PinEngine {
    pin: Mutex<Option<CredentialPin>>,
}

impl SessionEngine for PinEngine {
    fn run_prompt(
        &self,
        _: usize,
        _: PromptRequest,
        _: &dyn Fn() -> bool,
        _: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
    }
    fn run_side_question(
        &self,
        request: crate::engine::SideQuestionRequest,
        signal: &pa_agent::abort::AbortSignal,
        sink: &pa_core::session_engine::side_question::SideQuestionSink,
    ) -> crate::engine::SideQuestionOutcome {
        ScriptedEngine::default().run_side_question(request, signal, sink)
    }
    fn run_compaction(
        &self,
        request: CompactionRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> CompactionOutcome {
        ScriptedEngine::default().run_compaction(request, signal)
    }
    fn run_branch_summary(
        &self,
        request: BranchSummaryRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> BranchSummaryOutcome {
        ScriptedEngine::default().run_branch_summary(request, signal)
    }
    fn rebuild_session_context(
        &self,
        _: Vec<pa_types::session::FileEntry>,
        _: pa_core::session_engine::goal_driver::GoalBranchReload,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_credential_pin(&self, pin: Option<CredentialPin>) {
        *self.pin.lock().unwrap() = pin;
    }
    fn credential_pin(&self) -> Option<CredentialPin> {
        self.pin.lock().unwrap().clone()
    }
}

/// One create on a fresh worker (killed afterwards): the response and the pin its engine was
/// handed.
async fn create(dir: &Path, payload: Value) -> (DaemonResponse, Option<CredentialPin>) {
    let mut worker = Worker::new(
        WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "pin-test".into(),
            worker_instance_id: String::new(),
            active_session_id: "pin-test".into(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: Some(true),
            script: Some(json!({"responses":[]})),
        },
        None,
    );
    let engine = std::sync::Arc::new(PinEngine::default());
    worker.engine = engine.clone();
    let response = worker.dispatch("create", &payload).await;
    let pin = engine.credential_pin();
    // Release the session's runtime lease so the next create may reopen the file (a failed
    // create holds none).
    let _killed = worker.dispatch("kill", &json!({})).await;
    (response, pin)
}

#[tokio::test]
async fn the_pin_is_written_at_create_kept_on_resume_and_inherited_by_a_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let accounts = pa_core::auth::AuthAccounts::create(dir.path().join("agent"));
    let id = accounts
        .add(
            "anthropic",
            "work",
            pa_core::auth::AuthCredential::ApiKey {
                key: "key-A".to_string(),
                prime_team: None,
            },
            0,
        )
        .unwrap();
    let pin = CredentialPin {
        provider: "anthropic".to_string(),
        id: id.clone(),
        label: "work".to_string(),
    };
    let parent = dir.path().join("parent.jsonl").display().to_string();
    let (created, created_pin) = create(
        dir.path(),
        json!({"cwd": "/tmp", "sessionPath": parent, "credential": "WORK"}),
    )
    .await;
    assert!(created.success, "{created:?}");
    assert_eq!(created_pin.as_ref(), Some(&pin));
    let header = pa_core::session::manager::read_session_header(Path::new(&parent)).unwrap();
    assert_eq!(header.credential_pin().as_ref(), Some(&pin));

    // The resume restores the header's pin as-is: even a credential deleted since stays pinned
    // (its requests then fail with the missing-credential sentence), never re-chosen.
    assert!(accounts.remove("anthropic", &id).unwrap());
    let (resumed, resumed_pin) =
        create(dir.path(), json!({"cwd": "/tmp", "sessionPath": parent})).await;
    assert!(resumed.success, "{resumed:?}");
    assert_eq!(resumed_pin.as_ref(), Some(&pin));

    let child = dir.path().join("child.jsonl").display().to_string();
    let (spawned, child_pin) = create(
        dir.path(),
        json!({"cwd": "/tmp", "sessionPath": child, "parentSessionPath": parent}),
    )
    .await;
    assert!(spawned.success, "{spawned:?}");
    assert_eq!(child_pin.as_ref(), Some(&pin));
}

#[tokio::test]
async fn an_unknown_credential_selector_fails_the_create_before_any_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let (response, pin) = create(
        dir.path(),
        json!({"cwd": "/tmp", "sessionPath": path.display().to_string(), "credential": "nope"}),
    )
    .await;
    assert_eq!(
        (
            response.success,
            response.error.as_deref(),
            pin,
            path.exists()
        ),
        (
            false,
            Some(
                "no stored credential is named 'nope'; /login and choose Add another to store one"
            ),
            None,
            false
        )
    );
}
