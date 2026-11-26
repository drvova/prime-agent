//! A session's `rlm.*` surface: its children as runtime sessions.

use std::sync::Arc;
use std::time::Duration;

use pa_core::session_engine::agent_messaging::{
    create_agent_session_message_row, AgentFamilyRelationship, AgentSessionMessageRowPayload,
};
use pa_core::session_engine::rlm_depth::{DelegationDepth, DepthBound};
use pa_core::session_engine::rlm_host::{
    resolve_child_cwd, rlm_create_session_daemonless_error, rlm_depth_limit_error,
    spawn_name_unavailable_error, RlmChildResult, RlmCreateSessionHandle, RlmCreateSessionRequest,
    RlmDeleteSubagentResult, RlmHostFuture, RlmSpawnHandle, RlmSpawnRequest, RlmSubagentEntry,
    RlmSubagentHost,
};
use pa_core::session_engine::rlm_notices::{compact_rlm_text, rlm_child_label};
use pa_types::event_log::{AgentTarget, Priority, Route, Scope, Subscription, TopicFilter, Trust};

use super::family::SpawnRecord;
use super::{
    child_is_unsettled, deliver_row, dispose_child, lock, now_ms, tool_use_count, wake, Child,
    ChildState, Delivery, Node, Subagents, SUBSCRIPTION_PREFIX,
};
use crate::session::{SessionSpec, SubagentSpec};

/// One session's children: each spawn opens a runtime session, and the
/// roster, collect, delete and rename read and change them.
pub(crate) struct SessionSubagents(pub(crate) Arc<Node>);

impl RlmSubagentHost for SessionSubagents {
    fn delegation_depth(&self) -> Option<Arc<DelegationDepth>> {
        Some(Arc::clone(&self.0.delegation))
    }

    /// Admits one child: the depth gate, name uniqueness, the child's
    /// session (opened in the runtime and built by its builder, which
    /// resolves the child's model), its `done` subscription, the task row
    /// and the empty input that runs it. Returns at admission; the task
    /// runs as the child's.
    #[expect(
        clippy::too_many_lines,
        reason = "the admission reads linearly, gate by gate, as the inline host's did"
    )]
    fn spawn(&self, request: RlmSpawnRequest) -> RlmHostFuture<RlmSpawnHandle> {
        let node = Arc::clone(&self.0);
        Box::pin(async move {
            let max_depth = node.delegation.max_depth();
            if node.depth >= max_depth {
                return Err(rlm_depth_limit_error(node.depth, max_depth));
            }
            let tree = node
                .tree
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("the runtime is not running"))?;
            let cwd = resolve_child_cwd(&node.cwd, request.cwd.as_deref(), "rlm.spawn")?;
            let child_id = format!("sub-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            let name = request.name.clone().unwrap_or_else(|| {
                pa_core::kernel::rlm_runtime::create_default_rlm_subagent_session_name(
                    &request.prompt,
                    &child_id,
                )
            });
            // Host requests run as detached tasks: same-parent spawns
            // serialize on the node's spawn gate so the name check and the
            // push are atomic.
            let spawn_gate = node.spawn_gate.lock().await;
            if node.is_disposed() {
                anyhow::bail!("Cannot spawn from a disposed inline session");
            }
            for child in node.children.lock().await.iter() {
                if child.node.session_name().as_deref() == Some(name.as_str()) {
                    return Err(spawn_name_unavailable_error(&name, node.depth + 1));
                }
            }
            let control = tree.control()?;
            let session_id = uuid::Uuid::now_v7().to_string();
            let spec = {
                let adopted = lock(&node.adopted);
                SessionSpec {
                    id: Some(session_id.clone()),
                    name: Some(name.clone()),
                    cwd,
                    env: node.env.clone(),
                    subagent: Some(SubagentSpec {
                        parent: node.session_id.clone(),
                        parent_session_file: adopted.session_file.clone(),
                        parent_model: adopted.model_selector.clone(),
                        parent_thinking: adopted.thinking,
                        child_id: child_id.clone(),
                        depth: node.depth + 1,
                        model: request.model.clone(),
                        thinking: request.thinking.clone(),
                        spawned_by_request_id: request.spawned_by_request_id.clone(),
                        factory: request.factory.is_some(),
                    }),
                    transcript: None,
                    options: crate::SessionOptions::default(),
                    ephemeral: false,
                }
            };
            let child_node = Arc::new(Node::new(
                &tree,
                session_id.clone(),
                &spec,
                node.depth + 1,
                DepthBound::Fixed(max_depth),
                Some(Arc::downgrade(&node)),
            ));
            lock(&tree.nodes).insert(session_id.clone(), Arc::clone(&child_node));
            let mut built = child_node.built.subscribe();
            let waited = std::time::Instant::now();
            let opened = control.open(spec);
            // The child's engine builds on its own thread; its builder's
            // error is the spawn's.
            let outcome = match opened {
                Ok(_) => match built.wait_for(Option::is_some).await {
                    Ok(outcome) => outcome.clone().unwrap_or(Ok(())),
                    Err(_) => Err("the subagent's session ended before it was built".to_string()),
                },
                Err(error) => Err(format!("{error:#}")),
            };
            let queued_ms = u64::try_from(waited.elapsed().as_millis()).unwrap_or(u64::MAX);
            let child = Arc::new(Child {
                rlm_child_id: child_id.clone(),
                factory: request.factory.is_some(),
                label: rlm_child_label(&request.prompt),
                queued_ms,
                node: Arc::clone(&child_node),
                state: tokio::sync::Mutex::new(ChildState {
                    status: "running",
                    settled: false,
                    answer_text: None,
                    error: None,
                    started_at_ms: now_ms(),
                    replied_since_task: false,
                }),
                published: std::sync::Mutex::new(None),
            });
            // Teardown can run while the engine is being built. Checked
            // while the roster is held so disposal either takes this child
            // or admission rejects it; no child lands after the teardown.
            let mut children = node.children.lock().await;
            let admitted = outcome.and_then(|()| {
                if node.is_disposed() {
                    Err("Cannot spawn from a disposed inline session".to_string())
                } else {
                    Ok(())
                }
            });
            // The runtime daemon records the edge before the child is given
            // its task; one it cannot record is refused.
            let recorded = match (&admitted, tree.daemon()) {
                (Ok(()), Some(family)) => {
                    let parent_file = lock(&node.adopted).session_file.clone();
                    let (child_file, model) = {
                        let adopted = lock(&child_node.adopted);
                        (adopted.session_file.clone(), adopted.model_selector.clone())
                    };
                    match (parent_file, child_file) {
                        (Some(parent_file), Some(child_file)) => family
                            .spawned(SpawnRecord {
                                child_id: child_id.clone(),
                                parent_file,
                                child_file,
                                child_dir: child_node.session_dir(),
                                depth: node.depth + 1,
                                name: name.clone(),
                                prompt: request.prompt.clone(),
                                model: model.unwrap_or_default(),
                                factory: request.factory.clone(),
                                created_at: now_ms(),
                            })
                            .await
                            .map_err(|error| format!("{error:#}")),
                        (None, _) | (_, None) => Ok(()),
                    }
                }
                (Ok(()), None) | (Err(_), _) => Ok(()),
            };
            if let Err(error) = admitted.and(recorded) {
                drop(children);
                dispose_child(&child).await;
                anyhow::bail!(error);
            }
            children.push(Arc::clone(&child));
            drop(children);
            drop(spawn_gate);
            child.publish_update().await;
            tree.tree
                .insert(session_id.clone(), node.session_id.clone());
            subscribe_done(&tree, &session_id, &node.session_id)?;
            // The task rides the parent's `agent_message` row (TS
            // `spawnMessage`), delivered through the one steer path; the
            // empty input runs it.
            let kickoff_content = format!("[task from parent]\n\n{}", request.prompt);
            let kickoff_row = create_agent_session_message_row(&AgentSessionMessageRowPayload {
                id: &format!("spawn:{child_id}"),
                prompt: &kickoff_content,
                message: &request.prompt,
                from: &node.endpoint(),
                from_relationship: Some(AgentFamilyRelationship::Parent),
                target: None,
                timestamp: now_ms(),
            });
            deliver_row(&child_node, kickoff_row, Delivery::Steer)?;
            wake(&child_node, node.trust.current(), /*priority*/ None)?;
            let selector = lock(&child_node.adopted)
                .model_selector
                .clone()
                .unwrap_or_default();
            Ok(RlmSpawnHandle {
                rlm_child_id: child_id,
                name,
                session_dir: child_node.session_dir(),
                model: selector,
                queued_ms,
            })
        })
    }

    fn create_session(
        &self,
        _request: RlmCreateSessionRequest,
    ) -> RlmHostFuture<RlmCreateSessionHandle> {
        Box::pin(async { Err(rlm_create_session_daemonless_error()) })
    }

    fn list_subagents(&self) -> RlmHostFuture<Vec<RlmSubagentEntry>> {
        let node = Arc::clone(&self.0);
        Box::pin(async move {
            let children = node.children.lock().await.clone();
            let mut entries = Vec::with_capacity(children.len());
            for child in &children {
                let unsettled = child_is_unsettled(child).await;
                let tools = tool_use_count(&child.node).await;
                let state = child.state.lock().await;
                entries.push(roster_entry(child, &state, unsettled, tools));
            }
            Ok(entries)
        })
    }

    fn delete_subagent(&self, target: String) -> RlmHostFuture<RlmDeleteSubagentResult> {
        let node = Arc::clone(&self.0);
        Box::pin(async move {
            let child = node.find_child(&target).await.map_err(anyhow::Error::msg)?;
            let was_running = !child.state.lock().await.settled;
            // The child and its subtree end: runs abort, kernels get their
            // final snapshots, sessions close.
            dispose_child(&child).await;
            let unsettled = child_is_unsettled(&child).await;
            let tools = tool_use_count(&child.node).await;
            let entry = {
                let mut state = child.state.lock().await;
                state.settled = true;
                if was_running {
                    state.status = "cancelled";
                    state.error = Some("Deleted by parent orchestrator".to_string());
                }
                roster_entry(&child, &state, unsettled, tools)
            };
            child.publish_update().await;
            node.children
                .lock()
                .await
                .retain(|other| !Arc::ptr_eq(other, &child));
            let child_file = lock(&child.node.adopted).session_file.clone();
            if let (Some(family), Some(child_file)) = (
                node.tree.upgrade().as_ref().and_then(|tree| tree.daemon()),
                child_file,
            ) {
                family
                    .deleted(
                        child.rlm_child_id.clone(),
                        child_file,
                        child.node.session_dir(),
                    )
                    .await;
            }
            // The settle the parked parent drive waits for.
            node.activity.notify_waiters();
            Ok(RlmDeleteSubagentResult {
                subagent: entry,
                outcome: Some("deleted"),
            })
        })
    }

    /// Waits until every target settles or the deadline passes, on the
    /// parent's activity (which each child's settle fires). The kernel drops
    /// the wait when its cell is stopped, so it ends with the cell.
    fn collect(&self, targets: Vec<String>, timeout_ms: u64) -> RlmHostFuture<Vec<RlmChildResult>> {
        let node = Arc::clone(&self.0);
        Box::pin(async move {
            let children: Vec<Arc<Child>> = if targets.is_empty() {
                node.children.lock().await.clone()
            } else {
                let mut resolved = Vec::with_capacity(targets.len());
                for target in &targets {
                    resolved.push(node.find_child(target).await.map_err(anyhow::Error::msg)?);
                }
                resolved
            };
            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                // The wake registers before the snapshot: a settle that
                // lands mid-scan still wakes the collect.
                let notified = node.activity.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let mut all_settled = true;
                let mut waiting_on_parent = false;
                let mut results = Vec::with_capacity(children.len());
                for child in &children {
                    let unsettled = child_is_unsettled(child).await;
                    let waiting_on_you = unsettled
                        && child.node.waits.waiting_on().as_deref()
                            == Some(node.session_id.as_str());
                    waiting_on_parent |= waiting_on_you;
                    let tools = tool_use_count(&child.node).await;
                    let state = child.state.lock().await;
                    all_settled &= !unsettled;
                    results.push(RlmChildResult {
                        rlm_child_id: child.rlm_child_id.clone(),
                        session_name: Some(
                            child
                                .node
                                .session_name()
                                .unwrap_or_else(|| child.rlm_child_id.clone()),
                        ),
                        session_dir: Some(child.node.session_dir()),
                        status: if unsettled { "running" } else { state.status },
                        settled: !unsettled,
                        waiting_on_you,
                        answer_preview: state.answer_text.as_deref().map(compact_rlm_text),
                        answer_text: state.answer_text.clone(),
                        error: state.error.clone(),
                        duration_ms: Some(now_ms().saturating_sub(state.started_at_ms)),
                        tool_use_count: tools,
                        replied_since_task: Some(state.replied_since_task),
                        queued_ms: Some(child.queued_ms).filter(|queued_ms| *queued_ms > 0),
                        check_status: child
                            .node
                            .engine()
                            .map(|engine| engine.better.check_status().as_str()),
                    });
                }
                if all_settled || waiting_on_parent || deadline <= tokio::time::Instant::now() {
                    return Ok(results);
                }
                let _ = tokio::time::timeout_at(deadline, notified.as_mut()).await;
            }
        })
    }

    fn rename(&self, name: String, session_id: Option<String>) -> RlmHostFuture<String> {
        let node = Arc::clone(&self.0);
        Box::pin(async move {
            let self_target = session_id
                .as_deref()
                .is_none_or(|target| target == node.session_id);
            if self_target {
                rename_session(&node, &name).await?;
                return Ok(name);
            }
            let target = session_id.unwrap_or_default();
            let children = node.children.lock().await.clone();
            let mut by_ids = None;
            let mut by_name = false;
            for child in &children {
                if child.node.session_id == target || child.rlm_child_id == target {
                    by_ids = Some(Arc::clone(child));
                    break;
                }
                if child.node.session_name().as_deref() == Some(target.as_str()) {
                    by_name = true;
                }
            }
            let Some(child) = by_ids else {
                if by_name {
                    anyhow::bail!(
                        "rlm.rename session_id \"{target}\" must be the full session id or a child handle, not a session name or id suffix"
                    );
                }
                anyhow::bail!(
                    "rlm.rename can only rename the current session or one of its direct children"
                );
            };
            rename_session(&child.node, &name).await?;
            Ok(name)
        })
    }
}

/// Appends one durable `session_info` name row to a node's session.
async fn rename_session(node: &Arc<Node>, name: &str) -> anyhow::Result<()> {
    let parent = node.parent_node();
    let _name_guard = match &parent {
        Some(parent) => Some(parent.spawn_gate.lock().await),
        None => None,
    };
    if let Some(parent) = &parent {
        for sibling in parent.children.lock().await.iter() {
            if !Arc::ptr_eq(&sibling.node, node)
                && sibling.node.session_name().as_deref() == Some(name)
            {
                return Err(spawn_name_unavailable_error(name, node.depth));
            }
        }
    }
    let engine = node
        .engine()
        .ok_or_else(|| anyhow::anyhow!("the target session is not running"))?;
    let persistence = engine.session.shared_persistence();
    let mut session = persistence.lock().await;
    session
        .append_session_info(name)
        .map_err(anyhow::Error::from)?;
    drop(session);
    *lock(&node.session_name) = Some(name.to_string());
    // A child's rename is its ledger edge's.
    let tree = node.tree.upgrade();
    if let (Some(parent), Some(family)) = (&parent, tree.as_ref().and_then(|tree| tree.daemon())) {
        let child_id = parent
            .children
            .lock()
            .await
            .iter()
            .find(|child| Arc::ptr_eq(&child.node, node))
            .map(|child| child.rlm_child_id.clone());
        let child_file = lock(&node.adopted).session_file.clone();
        if let (Some(child_id), Some(child_file)) = (child_id, child_file) {
            family.renamed(child_id, child_file, name.to_string()).await;
        }
    }
    Ok(())
}

/// Routes `child`'s `agent/<child>/done` to its parent `parent`: the parent
/// hears the child's settle through it.
fn subscribe_done(tree: &Subagents, child: &str, parent: &str) -> anyhow::Result<()> {
    if let Some(attached) = tree.attached.get() {
        lock(&attached.registry).add(Subscription {
            id: format!("{SUBSCRIPTION_PREFIX}{child}"),
            scope: Scope::Global,
            filter: TopicFilter::new(format!("agent/{child}/done"))?,
            when: None,
            throttle_ms: 0,
            debounce_ms: 0,
            route: Route::Agent {
                target: AgentTarget::Session {
                    session: parent.to_string(),
                },
                prompt: String::new(),
                priority: Priority::Steer,
            },
            trust: Trust::Owner,
        })?;
    }
    Ok(())
}

impl Subagents {
    /// Opens again, as settled children of open session `id`, the children
    /// the runtime daemon's ledger records under its transcript and the
    /// runtime does not run: each a runtime session on the transcript its
    /// spawn wrote, so the parent's roster, collect, messages and observe
    /// reach a family that outlived the daemon that spawned it. Waits for
    /// the parent's engine; a child that cannot be opened is left out and
    /// logged. Answers how many were opened.
    ///
    /// # Errors
    ///
    /// The session is not open, its engine was not built, or the ledger
    /// could not be read.
    #[expect(
        clippy::too_many_lines,
        reason = "the reopen reads linearly, child by child, as the spawn's admission does"
    )]
    pub(crate) async fn reopen_children(self: &Arc<Self>, id: &str) -> anyhow::Result<usize> {
        let Some(family) = self.daemon() else {
            return Ok(0);
        };
        let node = self
            .node(id)
            .ok_or_else(|| anyhow::anyhow!("session {id} is not open"))?;
        let mut built = node.built.subscribe();
        match built.wait_for(Option::is_some).await {
            Ok(outcome) => {
                if let Some(Err(error)) = outcome.as_ref() {
                    anyhow::bail!("session {id}'s engine was not built: {error}");
                }
            }
            Err(_) => anyhow::bail!("session {id} ended before its engine was built"),
        }
        let (parent_file, parent_model, parent_thinking) = {
            let adopted = lock(&node.adopted);
            (
                adopted.session_file.clone(),
                adopted.model_selector.clone(),
                adopted.thinking,
            )
        };
        let Some(parent_file) = parent_file else {
            return Ok(0);
        };
        let edges = family
            .children_of(std::path::PathBuf::from(&parent_file))
            .await?;
        let control = self.control()?;
        let max_depth = node.delegation.max_depth();
        let mut reopened = 0;
        for edge in edges {
            let child_file = std::path::PathBuf::from(&edge.child);
            let Some(session_id) = child_file
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
            else {
                continue;
            };
            if self.node(&session_id).is_some() {
                continue;
            }
            let cwd = pa_core::session::manager::read_session_header(&child_file)
                .map_or_else(|| node.cwd.clone(), |header| header.cwd.into());
            let spec = SessionSpec {
                id: Some(session_id.clone()),
                name: Some(edge.name.clone()),
                cwd,
                env: node.env.clone(),
                subagent: Some(SubagentSpec {
                    parent: node.session_id.clone(),
                    parent_session_file: Some(parent_file.clone()),
                    parent_model: parent_model.clone(),
                    parent_thinking,
                    child_id: edge.child_id.clone(),
                    depth: edge.depth,
                    model: None,
                    thinking: None,
                    spawned_by_request_id: None,
                    factory: edge.factory.is_some(),
                }),
                transcript: Some(child_file),
                options: crate::SessionOptions::default(),
                ephemeral: false,
            };
            let child_node = Arc::new(Node::new(
                self,
                session_id.clone(),
                &spec,
                edge.depth,
                DepthBound::Fixed(max_depth),
                Some(Arc::downgrade(&node)),
            ));
            lock(&self.nodes).insert(session_id.clone(), Arc::clone(&child_node));
            let mut built = child_node.built.subscribe();
            let outcome = match control.open(spec) {
                Ok(_) => match built.wait_for(Option::is_some).await {
                    Ok(outcome) => outcome.clone().unwrap_or(Ok(())),
                    Err(_) => Err("the subagent's session ended before it was built".to_string()),
                },
                Err(error) => Err(format!("{error:#}")),
            };
            // Its label from the prompt its spawn recorded beside it.
            let label = rlm_child_label(
                pa_core::session_engine::rlm_ledger::read_rlm_subagent_display(
                    std::path::Path::new(&child_node.session_dir()),
                )
                .and_then(|display| display.prompt)
                .as_deref()
                .unwrap_or_default(),
            );
            let child = Arc::new(Child {
                rlm_child_id: edge.child_id.clone(),
                factory: edge.factory.is_some(),
                label,
                queued_ms: 0,
                node: Arc::clone(&child_node),
                state: tokio::sync::Mutex::new(ChildState {
                    status: "done",
                    settled: true,
                    answer_text: None,
                    error: None,
                    started_at_ms: now_ms(),
                    replied_since_task: false,
                }),
                published: std::sync::Mutex::new(None),
            });
            if let Err(error) = outcome {
                tracing::warn!(
                    "subagent {} of session {id} was not opened again: {error}",
                    edge.child_id
                );
                dispose_child(&child).await;
                continue;
            }
            node.children.lock().await.push(Arc::clone(&child));
            self.tree
                .insert(session_id.clone(), node.session_id.clone());
            subscribe_done(self, &session_id, &node.session_id)?;
            reopened += 1;
        }
        Ok(reopened)
    }
}

fn roster_entry(
    child: &Arc<Child>,
    state: &ChildState,
    unsettled: bool,
    tool_use_count: Option<u64>,
) -> RlmSubagentEntry {
    RlmSubagentEntry {
        rlm_child_id: child.rlm_child_id.clone(),
        active_session_id: Some(child.node.session_id.clone()),
        session_id: Some(child.node.session_id.clone()),
        session_name: child
            .node
            .session_name()
            .unwrap_or_else(|| child.rlm_child_id.clone()),
        session_dir: child.node.session_dir(),
        status: if unsettled {
            "running"
        } else {
            match state.status {
                "done" => "completed",
                other => other,
            }
        },
        activity: None,
        tool_use_count,
        duration_ms: Some(now_ms().saturating_sub(state.started_at_ms)),
        answer_preview: state.answer_text.as_deref().map(compact_rlm_text),
        replied_since_task: Some(state.replied_since_task),
        progress_note: None,
        label: Some(child.label.clone()),
        last_activity_at: Some(state.started_at_ms),
        activity_stale_ms: None,
    }
}
