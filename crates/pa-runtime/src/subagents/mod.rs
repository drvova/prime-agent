//! Subagents: the `rlm.*` children of the runtime's sessions, each a runtime
//! session of its own.
//!
//! Every open session has a [`Node`] in the runtime's [`Subagents`]: its
//! place in the family tree, its children, and the facts the quiescence drive
//! waits on. A spawn ([`host`]) opens the child as a runtime session built by
//! the same [`crate::SessionBuilder`] (its [`crate::SubagentSpec`] says what
//! it inherits), delivers the parent's task to it as today's `agent_message`
//! row, and publishes an empty [`AgentInput`] on `agent/<child>/input`: an
//! empty input runs the rows delivered to the session to quiescence. The
//! parent hears the child through a subscription on `agent/<child>/done`,
//! whose delivery ([`Subagents::deliver_done`]) settles the child and steers
//! the terminal or failure notice onto the parent, as the inline host did.
//! The family's `agent_message.*` ([`messages`]) publishes each message on
//! the receiver's `agent/<to>/inbox`, which the runtime's inbox subscription
//! delivers under the daemon's wake budget ([`inbox`]); the kernel's notices
//! go through the bus the same way ([`notices`]).
//!
//! A session is at rest ([`has_unsettled_rlm_work`]) when no child is
//! unsettled, no late receipt is pending, no row waits on its agent, and no
//! background command runs; [`drive_to_quiescence`] runs the rows that leave
//! until it is. A subagent's task ends there, so its `done` follows its whole
//! subtree.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use pa_core::session_engine::agent_messaging::AgentMessageWaits;
use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::goal_boundary::custom_message_to_loop_row;
use pa_core::session_engine::rlm_depth::{DelegationDepth, DepthBound};
use pa_core::session_engine::rlm_notices::{
    cap_text, compact_rlm_text, create_rlm_child_failure_message, create_rlm_child_terminal_notice,
    RlmChildTerminalNotice, ANSWER_TEXT_MAX_CHARS,
};
use pa_types::ai::{Model, ModelThinkingLevel};
use pa_types::event_log::{
    AgentDone, AgentInput, AgentTarget, Event, NewEvent, Priority, Route, Scope, Subscription,
    TaskFailure, TaskState, Topic, TopicFilter, Trust,
};
use pa_types::session::CustomMessage;
use serde_json::{json, Value};
use tokio::sync::{watch, Notify};

use crate::lock;
use crate::session::{Outbox, SessionSpec};
use crate::subscriptions::{MemorySessionTree, Registry};
use crate::trust::TaskTrust;
use crate::turn::boundary::TurnBoundary;

mod family;
mod host;
mod inbox;
mod messages;
mod notices;
#[cfg(test)]
mod tests;

pub(crate) use family::DaemonFamily;
pub(crate) use host::SessionSubagents;
pub(crate) use messages::{late_sent_agent_message_sink, FamilyMessages, FamilyObserve};
pub(crate) use notices::kernel_notice_sink;

/// What names a child's `agent/<child>/done` subscription: this, then the
/// child's session id.
pub(crate) const SUBSCRIPTION_PREFIX: &str = "subagent/";

/// The id of the subscription that delivers every session's agent messages
/// (`agent/+/inbox`, [`Subagents::deliver_inbox`]).
pub(crate) const INBOX_SUBSCRIPTION: &str = "runtime/inbox";

/// The id of the subscription that wakes every session for the kernel
/// notices it steered (`session/+/bash/+`, [`Subagents::deliver_notice`]).
pub(crate) const NOTICE_SUBSCRIPTION: &str = "runtime/bash";

/// The runtime's sessions as the subagents reach them: opening a child,
/// ending its tasks and closing it.
///
/// The runtime implements it over its sessions and task queue. `open` opens
/// a session on its own thread and returns at once (its engine builds
/// there); `cancel` cancels the session's queued tasks and aborts its
/// running one without waiting for it; `close` waits until the session's
/// thread has finished the task it runs and ended. None of them waits on the
/// bus, so a route may call them.
pub(crate) trait SessionControl: Send + Sync {
    /// Opens a session; its id.
    fn open(&self, spec: SessionSpec) -> anyhow::Result<String>;

    /// Ends `session`'s tasks.
    fn cancel(&self, session: &str) -> pa_agent::BoxFut<'_, anyhow::Result<()>>;

    /// Closes `session`.
    fn close(&self, session: &str) -> pa_agent::BoxFut<'_, ()>;
}

/// The family tree of the runtime's sessions.
pub(crate) struct Subagents {
    nodes: Mutex<HashMap<String, Arc<Node>>>,
    outbox: Outbox,
    tree: MemorySessionTree,
    attached: OnceLock<Attached>,
    /// The senders waiting to hear how their agent messages went.
    answers: inbox::InboxAnswers,
    /// The runtime daemon's family features: its spawn ledger, and with it
    /// `agent_message.wait` and `agent_observe.*`; unset in a headless run.
    daemon: OnceLock<DaemonFamily>,
}

/// What the runtime gives the subagents once its bus is up.
struct Attached {
    registry: Arc<Mutex<Registry>>,
    control: Weak<dyn SessionControl>,
}

impl Subagents {
    /// The tree of a runtime publishing through `outbox`, recording each
    /// child under its parent in `tree`.
    pub(crate) fn new(outbox: Outbox, tree: MemorySessionTree) -> Self {
        Subagents {
            nodes: Mutex::new(HashMap::new()),
            outbox,
            tree,
            attached: OnceLock::new(),
            answers: Mutex::new(HashMap::new()),
            daemon: OnceLock::new(),
        }
    }

    /// Gives the tree the runtime daemon's family features: every spawn,
    /// rename, settle and delete from now on is recorded in `family`, and
    /// the sessions built from now on serve `agent_message.wait` and
    /// `agent_observe.*`. Attached once; a second attach changes nothing.
    pub(crate) fn attach_daemon(&self, family: DaemonFamily) {
        let _ = self.daemon.set(family);
    }

    /// The runtime daemon's family features, when this runtime is one.
    pub(crate) fn daemon(&self) -> Option<&DaemonFamily> {
        self.daemon.get()
    }

    /// Every open session's wait edge, as the wait-cycle walk reads a
    /// roster: its id, name, and the session its newest named wait waits on.
    fn wait_roster(&self) -> Vec<Value> {
        let nodes: Vec<Arc<Node>> = lock(&self.nodes).values().cloned().collect();
        nodes
            .iter()
            .map(|node| {
                json!({
                    "activeSessionId": node.session_id,
                    "sessionId": node.session_id,
                    "sessionName": node.session_name(),
                    "waitingOn": node.waits.waiting_on(),
                })
            })
            .collect()
    }

    /// Gives the tree the bus's subscriptions and the runtime's sessions,
    /// and subscribes every session's inbox and kernel notices.
    pub(crate) fn attach(&self, registry: Arc<Mutex<Registry>>, control: Weak<dyn SessionControl>) {
        for (id, filter) in [
            (INBOX_SUBSCRIPTION, "agent/+/inbox"),
            (NOTICE_SUBSCRIPTION, "session/+/bash/+"),
        ] {
            let added = TopicFilter::new(filter)
                .map_err(anyhow::Error::from)
                .and_then(|filter| {
                    lock(&registry)
                        .add(Subscription {
                            id: id.to_string(),
                            scope: Scope::Global,
                            filter,
                            when: None,
                            throttle_ms: 0,
                            debounce_ms: 0,
                            route: Route::Agent {
                                target: AgentTarget::Origin,
                                prompt: String::new(),
                                priority: Priority::Steer,
                            },
                            trust: Trust::Owner,
                        })
                        .map_err(anyhow::Error::from)
                });
            if let Err(error) = added {
                tracing::error!("subscription {id} is not on the bus: {error:#}");
            }
        }
        // Attached once, by the runtime's open.
        let _ = self.attached.set(Attached { registry, control });
    }

    /// The node of the open session `id`.
    fn node(&self, id: &str) -> Option<Arc<Node>> {
        lock(&self.nodes).get(id).cloned()
    }

    /// Waits until every subagent below open session `id` has settled and
    /// no late receipt of its is pending: the subtree's settle. The session's
    /// own lane and agent are its caller's to wait on. Answers at once for a
    /// session not open.
    pub(crate) async fn subtree_settled(&self, id: &str) {
        let Some(node) = self.node(id) else {
            return;
        };
        loop {
            // The wake registers before the checks: a settle that lands
            // mid-scan still wakes the wait.
            let notified = node.activity.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut unsettled = node.pending_receipts.load(Ordering::SeqCst) > 0;
            if !unsettled {
                for child in node.children.lock().await.iter() {
                    if child_is_unsettled(child).await {
                        unsettled = true;
                        break;
                    }
                }
            }
            if !unsettled || node.is_disposed() {
                return;
            }
            notified.await;
        }
    }

    /// Whether a row delivered to open session `id` waits on its agent: no
    /// task has taken it yet. False for a session not open or not built.
    pub(crate) fn rows_waiting(&self, id: &str) -> bool {
        self.node(id)
            .and_then(|node| node.engine())
            .is_some_and(|engine| engine.session.agent().has_queued_messages())
    }

    /// Delivers `row` to open session `id` ([`deliver_row`]) once its
    /// engine is built.
    ///
    /// # Errors
    ///
    /// The session is not open, its engine could not be built, or the row
    /// has no loop form.
    pub(crate) async fn deliver_row_to(&self, id: &str, row: &CustomMessage) -> anyhow::Result<()> {
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
        deliver_row(&node, serde_json::to_value(row)?)
    }

    /// The node of the session `spec` opens as `id`: the one a spawn made for
    /// it, or a new root (in the runtime daemon, whose recorded children
    /// [`Subagents::reopen_children`] opens again).
    pub(crate) fn node_for(self: &Arc<Self>, id: &str, spec: &SessionSpec) -> Arc<Node> {
        let mut nodes = lock(&self.nodes);
        if let Some(node) = nodes.get(id) {
            return Arc::clone(node);
        }
        let node = Arc::new(Node::new(
            self,
            id.to_string(),
            spec,
            /*depth*/ 0,
            DepthBound::resolved(0, "default"),
            /*parent*/ None,
        ));
        nodes.insert(id.to_string(), Arc::clone(&node));
        drop(nodes);
        // The runtime daemon opens again the family the ledger keeps under
        // the session, off the open, once its engine is built.
        if self.daemon().is_some() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let (tree, id) = (Arc::clone(self), id.to_string());
                handle.spawn(async move {
                    match tree.reopen_children(&id).await {
                        Ok(0) => {}
                        Ok(reopened) => {
                            tracing::info!("session {id}'s {reopened} subagents were opened again");
                        }
                        Err(error) => {
                            tracing::warn!(
                                "session {id}'s subagents were not opened again: {error:#}"
                            );
                        }
                    }
                });
            }
        }
        node
    }

    /// The runtime's sessions, while it runs.
    fn control(&self) -> anyhow::Result<Arc<dyn SessionControl>> {
        self.attached
            .get()
            .and_then(|attached| attached.control.upgrade())
            .ok_or_else(|| anyhow::anyhow!("the runtime is not running"))
    }

    /// Settles the child session `child` on its `done` (the subscription
    /// [`SUBSCRIPTION_PREFIX`]`<child>` routes it here): its answer and
    /// verdict, and the notice to its parent.
    pub(crate) async fn deliver_done(&self, child: &str, event: &Event) {
        let Some(node) = self.node(child) else {
            return;
        };
        let Some(parent) = node.parent_node() else {
            return;
        };
        let Some(child) = parent
            .children
            .lock()
            .await
            .iter()
            .find(|child| Arc::ptr_eq(&child.node, &node))
            .cloned()
        else {
            return;
        };
        let done = match serde_json::from_value::<AgentDone>(event.event.payload.clone()) {
            Ok(done) => done,
            Err(error) => {
                tracing::error!(
                    "subagent {}'s done is malformed: {error}",
                    child.rlm_child_id
                );
                return;
            }
        };
        // A failed reply is read from the transcript, as the turn left it.
        let run_error = match (done.outcome, done.failure) {
            (TaskState::Done, _) | (TaskState::Failed, Some(TaskFailure::Reply)) => None,
            (TaskState::Failed, Some(TaskFailure::Turn | TaskFailure::Command) | None) => {
                Some(done.error.unwrap_or_default())
            }
            (TaskState::Cancelled, _) => Some("The subagent's task was cancelled".to_string()),
            (TaskState::Queued | TaskState::Running, _) => return,
        };
        settle_child(&child, run_error, event.event.trust).await;
    }
}

/// One subagent below a session, as the state queries show it: its
/// handle, name, depth below the session, roster status, session and its
/// own subagents.
pub(crate) struct FamilyMember {
    pub(crate) child_id: String,
    pub(crate) name: String,
    pub(crate) depth: u32,
    pub(crate) status: &'static str,
    pub(crate) session_id: String,
    pub(crate) children: Vec<FamilyMember>,
}

impl Subagents {
    /// The subagents below open session `id`, each with its own, in spawn
    /// order; none for a session the runtime does not run.
    pub(crate) async fn family_of(&self, id: &str) -> Vec<FamilyMember> {
        let Some(root) = self.node(id) else {
            return Vec::new();
        };
        // Depth first, without recursion: walked in reverse, every member
        // comes after its subtree and its siblings keep their spawn order.
        let mut levels: Vec<(Arc<Node>, u32)> = vec![(root, 0)];
        let mut members: HashMap<String, Vec<FamilyMember>> = HashMap::new();
        let mut order = Vec::new();
        while let Some((node, depth)) = levels.pop() {
            order.push((Arc::clone(&node), depth));
            for child in node.children.lock().await.iter() {
                levels.push((Arc::clone(&child.node), depth + 1));
            }
        }
        for (node, depth) in order.into_iter().rev() {
            let Some(parent) = node.parent_node() else {
                continue;
            };
            let child = parent
                .children
                .lock()
                .await
                .iter()
                .find(|child| Arc::ptr_eq(&child.node, &node))
                .cloned();
            let Some(child) = child else {
                continue;
            };
            let unsettled = child_is_unsettled(&child).await;
            let status = if unsettled {
                "running"
            } else {
                match child.state.lock().await.status {
                    "done" => "completed",
                    other => other,
                }
            };
            let member = FamilyMember {
                child_id: child.rlm_child_id.clone(),
                name: node
                    .session_name()
                    .unwrap_or_else(|| child.rlm_child_id.clone()),
                depth,
                status,
                session_id: node.session_id.clone(),
                children: members.remove(&node.session_id).unwrap_or_default(),
            };
            members
                .entry(parent.session_id.clone())
                .or_default()
                .push(member);
        }
        members.remove(id).unwrap_or_default()
    }
}

/// One session's place in the family tree.
pub(crate) struct Node {
    /// The tree it is a node of.
    pub(crate) tree: Weak<Subagents>,
    /// The session's id.
    pub(crate) session_id: String,
    /// Where it runs, and its environment: a child inherits both.
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    /// What its built engine says about it.
    adopted: Mutex<Adopted>,
    /// The recorded session name (children: the spawn name).
    session_name: Mutex<Option<String>>,
    /// Serializes same-parent spawn admissions (the name check through the
    /// children push); host requests run as detached tasks.
    spawn_gate: tokio::sync::Mutex<()>,
    /// Serializes message admission with disposal; never held across an
    /// await.
    delivery_gate: Mutex<()>,
    depth: u32,
    /// The delegation depth bound: the root's resolves when its engine is
    /// built and adapts when no setting fixes it; a child inherits its
    /// parent's bound fixed at spawn.
    delegation: Arc<DelegationDepth>,
    parent: Option<Weak<Node>>,
    children: tokio::sync::Mutex<Vec<Arc<Child>>>,
    /// Receipt callbacks must finish before the session can be at rest.
    pending_receipts: AtomicUsize,
    late_receipt_error: Mutex<Option<String>>,
    /// Fired on every event the quiescence drive waits for: a delivered
    /// row, a child's settle, a receipt, a background command's end, a
    /// disposal.
    pub(crate) activity: Notify,
    /// Set by `rlm.delete_subagent` and the run's finish: the drives
    /// observe it and stop.
    disposed: AtomicBool,
    /// The trust of the task the session runs (what the tasks it gives its
    /// children carry) and its trigger (what the messages it sends are
    /// caused by).
    pub(crate) trust: TaskTrust,
    /// The wakes each sender has spent on this session.
    wakes: inbox::WakeBudget,
    /// The outcome of the session's first engine build; `None` until then.
    built: watch::Sender<Option<Result<(), String>>>,
    /// The session's `agent_message.wait`s: the receiving side every
    /// message delivered to it reports to.
    waits: Arc<AgentMessageWaits>,
}

/// What a node learns from its session's built engine.
struct Adopted {
    engine: Weak<SessionEngine>,
    session_file: Option<String>,
    session_dir: String,
    /// The model selector (`provider/id`) and thinking level the session
    /// runs, which children inherit.
    model_selector: Option<String>,
    thinking: Option<ModelThinkingLevel>,
}

impl Node {
    fn new(
        tree: &Arc<Subagents>,
        session_id: String,
        spec: &SessionSpec,
        depth: u32,
        bound: DepthBound,
        parent: Option<Weak<Node>>,
    ) -> Self {
        Node {
            tree: Arc::downgrade(tree),
            session_id,
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            adopted: Mutex::new(Adopted {
                engine: Weak::new(),
                session_file: None,
                session_dir: String::new(),
                model_selector: None,
                thinking: None,
            }),
            session_name: Mutex::new(spec.name.clone()),
            spawn_gate: tokio::sync::Mutex::new(()),
            delivery_gate: Mutex::new(()),
            depth,
            delegation: Arc::new(DelegationDepth::new(depth, bound)),
            parent,
            children: tokio::sync::Mutex::new(Vec::new()),
            pending_receipts: AtomicUsize::new(0),
            late_receipt_error: Mutex::new(None),
            activity: Notify::new(),
            disposed: AtomicBool::new(false),
            trust: TaskTrust::default(),
            wakes: inbox::WakeBudget::default(),
            built: watch::Sender::new(None),
            waits: Arc::new(AgentMessageWaits::default()),
        }
    }

    /// Takes in the session's built engine: its identity, the model and
    /// thinking level it runs, and (for a root) its delegation bound.
    pub(crate) async fn adopt(&self, engine: &Arc<SessionEngine>, model: &Model, agent_dir: &Path) {
        let (session_file, session_dir) = {
            let persistence = engine.session.shared_persistence();
            let manager = persistence.lock().await;
            (
                manager
                    .get_session_file()
                    .map(|path| path.display().to_string()),
                manager.get_session_dir().display().to_string(),
            )
        };
        let level = engine.session.agent().state().await.thinking_level;
        // The root's bound: chat override > global setting > env > default
        // (a root inherits nothing).
        if self.parent.is_none() {
            let (max_depth, source) = pa_core::session_engine::rlm_host::resolve_rlm_max_depth(
                session_file.as_deref(),
                /*inherited*/ None,
                &self.cwd,
                agent_dir,
            );
            self.delegation
                .configure(0, DepthBound::resolved(max_depth, source));
        }
        // A message waiting in the backlog counts while its row still waits
        // on the agent, not once a turn has taken it into the context.
        let queued_on = Arc::downgrade(engine);
        // A child that starts or stops waiting wakes its parent's collect.
        let parent = self.parent.clone();
        self.waits.set_hooks(
            Arc::new(move || {
                if let Some(parent) = parent.as_ref().and_then(Weak::upgrade) {
                    parent.activity.notify_waiters();
                }
            }),
            Arc::new(move |message_id| {
                queued_on.upgrade().is_some_and(|engine| {
                    engine.session.agent().any_queued_message(|message| {
                        matches!(message, pa_agent::types::AgentMessage::Custom(custom)
                            if custom.payload.pointer("/details/id").and_then(Value::as_str)
                                == Some(message_id))
                    })
                })
            }),
        );
        *lock(&self.adopted) = Adopted {
            engine: Arc::downgrade(engine),
            session_file,
            session_dir,
            model_selector: Some(format!("{}/{}", model.provider, model.id)),
            thinking: Some(pa_core::session_engine::provider_adapter::model_thinking_level(level)),
        };
    }

    /// Records how the session's first engine build went.
    pub(crate) fn built(&self, outcome: Result<(), String>) {
        self.built.send_if_modified(|built| {
            if built.is_some() {
                return false;
            }
            *built = Some(outcome);
            true
        });
    }

    /// The session's engine while it lives.
    fn engine(&self) -> Option<Arc<SessionEngine>> {
        lock(&self.adopted).engine.upgrade()
    }

    fn session_dir(&self) -> String {
        lock(&self.adopted).session_dir.clone()
    }

    /// The live parent node, when this node is a spawned child.
    fn parent_node(&self) -> Option<Arc<Node>> {
        self.parent.as_ref().and_then(Weak::upgrade)
    }

    pub(crate) fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    /// Whether the session is a spawned subagent.
    pub(crate) fn is_subagent(&self) -> bool {
        self.parent.is_some()
    }

    fn session_name(&self) -> Option<String> {
        lock(&self.session_name).clone()
    }

    /// The endpoint block a delivered row's `from`/`target` fields carry.
    fn endpoint(&self) -> Value {
        let mut endpoint = json!({
            "activeSessionId": self.session_id,
            "sessionId": self.session_id,
        });
        if let Some(name) = self.session_name() {
            endpoint["sessionName"] = json!(name);
        }
        endpoint
    }

    /// Resolves one direct child by every selector the roster exposes.
    async fn find_child(&self, target: &str) -> Result<Arc<Child>, String> {
        let children: Vec<Arc<Child>> = self.children.lock().await.clone();
        let mut matches = Vec::new();
        for child in &children {
            let name = child.node.session_name();
            if child.rlm_child_id == target
                || child.node.session_id == target
                || name.as_deref() == Some(target)
            {
                matches.push(Arc::clone(child));
            }
        }
        match matches.len() {
            0 => Err(format!(
                "No direct RLM child matches \"{target}\" in the current parent session"
            )),
            1 => Ok(matches.into_iter().next().expect("one match")),
            _ => Err(format!(
                "RLM child selector \"{target}\" is ambiguous in the current parent session"
            )),
        }
    }
}

/// One spawned child: its node and its live state.
pub(crate) struct Child {
    rlm_child_id: String,
    /// A factory state's child: the factory reads its settle through
    /// collect, so no terminal notice reaches the parent.
    factory: bool,
    node: Arc<Node>,
    state: tokio::sync::Mutex<ChildState>,
}

struct ChildState {
    /// `running` | `done` | `error` | `cancelled`.
    status: &'static str,
    settled: bool,
    /// The full final-answer text, bounded by [`ANSWER_TEXT_MAX_CHARS`].
    answer_text: Option<String>,
    error: Option<String>,
    started_at_ms: u64,
    /// An agent message to the parent arrived since the task was admitted;
    /// the no-reply terminal notice is withheld once set.
    replied_since_task: bool,
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Whether one child has live work: not settled, a delivered row waiting on
/// its agent, a turn streaming, or a receipt pending. (Live background bash
/// holds the child's own drive, through [`has_unsettled_rlm_work`]: a child
/// never settles with a live background command.) The quiescence predicate,
/// the collect wait and the roster rows all read this one fact, so a row
/// delivered to a settled child reads unsettled until the child's next task
/// takes it. A disposed child keeps no live work: its verdict stands.
async fn child_is_unsettled(child: &Arc<Child>) -> bool {
    if child.node.is_disposed() {
        return false;
    }
    if child.node.pending_receipts.load(Ordering::SeqCst) > 0 || !child.state.lock().await.settled {
        return true;
    }
    match child.node.engine() {
        Some(engine) => {
            let agent = engine.session.agent();
            agent.has_queued_messages() || agent.state().await.is_streaming
        }
        None => false,
    }
}

/// Whether subagent work of `node`'s session is unsettled: a child with
/// live work, a pending receipt, a row queued on the session's own agent,
/// or live background bash. The goal and autonomous continuation gates and
/// the drive's return read this one predicate.
pub(crate) async fn has_unsettled_rlm_work(node: &Arc<Node>) -> bool {
    if node.pending_receipts.load(Ordering::SeqCst) > 0 {
        return true;
    }
    for child in node.children.lock().await.iter() {
        if child_is_unsettled(child).await {
            return true;
        }
    }
    node.engine().is_some_and(|engine| {
        engine.session.agent().has_queued_messages() || engine.has_background_bash()
    })
}

/// How far [`drive_to_quiescence`] drives a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rest {
    /// Until the session and its whole subtree are at rest
    /// ([`has_unsettled_rlm_work`]): a subagent's task, and the settle.
    Subtree,
    /// Until no row waits on the session's own agent: a root's wake in the
    /// runtime daemon, which runs the rows delivered to it as the old
    /// daemon's notice turn does and leaves its children to wake it again.
    OwnRows,
}

/// Drives `node`'s session to `rest`: runs the delivered rows as turns
/// (through the turn-boundary pair), waits out unsettled children (for
/// [`Rest::Subtree`]), and returns once the session is idle with an empty
/// queue (and a settled subtree); whether any turn ran.
///
/// # Errors
///
/// A turn, or the boundary around it, failed, or a late receipt could not
/// be recorded; the text is the reason.
pub(crate) async fn drive_to_quiescence(
    node: &Arc<Node>,
    engine: &Arc<SessionEngine>,
    boundary: &mut TurnBoundary,
    model: &Model,
    api_key: Option<String>,
    global_harness_dir: PathBuf,
    rest: Rest,
) -> Result<bool, String> {
    let mut drained = false;
    loop {
        if node.is_disposed() {
            return Ok(drained);
        }
        // The wake registers before the checks: a settle or delivery that
        // lands mid-scan still wakes the parked drive.
        let notified = node.activity.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(error) = lock(&node.late_receipt_error).clone() {
            return Err(error);
        }
        let agent = engine.session.agent();
        let streaming = agent.state().await.is_streaming;
        if !streaming && agent.has_queued_messages() {
            boundary
                .run_pre_turn(engine, model, api_key.clone())
                .await?;
            agent
                .continue_run()
                .await
                .map_err(|error| format!("{error:#}"))?;
            agent.wait_for_idle().await;
            boundary
                .run_at_settled_turn(engine, model, api_key.clone(), global_harness_dir.clone())
                .await?;
            drained = true;
            continue;
        }
        let at_rest = !streaming
            && match rest {
                Rest::Subtree => !has_unsettled_rlm_work(node).await,
                // No row waits: the branch above would have run it.
                Rest::OwnRows => true,
            };
        if at_rest {
            // A settle that landed mid-scan fired the armed wake: poll it
            // once instead of returning a false quiescence (the zero timeout
            // is the one non-blocking poll).
            if tokio::time::timeout(std::time::Duration::ZERO, notified.as_mut())
                .await
                .is_ok()
            {
                continue;
            }
            return Ok(drained);
        }
        notified.await;
    }
}

/// Settles one child after its task: captures its last assistant text as
/// the answer, publishes the verdict, delivers the no-reply notice or the
/// failure row to the parent, and fires the parent's activity. A terminal
/// assistant row that stopped on a provider error settles failed like a
/// failed task. A row delivered to the child since its task took its last
/// one starts its next task, with the trust of the one that ended.
async fn settle_child(child: &Arc<Child>, run_error: Option<String>, trust: Trust) {
    let engine = child.node.engine();
    // The last assistant row: its text is the answer the collect envelope
    // and the no-reply notice carry; a provider-error stop on it is the
    // terminal error.
    let last = match &engine {
        Some(engine) => engine.session.last_assistant_message().await,
        None => None,
    };
    let (answer, terminal_error) =
        if let Some(pa_types::session::AgentMessage::Assistant(assistant)) = last {
            let text = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
                    pa_types::ai::AssistantContentBlock::Thinking(_)
                    | pa_types::ai::AssistantContentBlock::ToolCall(_) => None,
                })
                .collect::<String>();
            let terminal_error =
                (assistant.stop_reason == pa_types::ai::StopReason::Error).then(|| {
                    assistant
                        .error_message
                        .unwrap_or_else(|| "Assistant response failed".to_string())
                });
            ((!text.is_empty()).then_some(text), terminal_error)
        } else {
            (None, None)
        };
    let name = child
        .node
        .session_name()
        .unwrap_or_else(|| child.rlm_child_id.clone());
    let parent = child.node.parent_node();
    // Whether the parent was given a notice: it takes it as a delivered row.
    let notified = {
        let mut state = child.state.lock().await;
        if state.settled || child.node.is_disposed() {
            return;
        }
        state.settled = true;
        state.answer_text = answer.map(|text| cap_text(&text, ANSWER_TEXT_MAX_CHARS));
        let notice = if let Some(error) = run_error.or(terminal_error) {
            state.status = "error";
            state.error = Some(error.clone());
            Some(create_rlm_child_failure_message(
                &child.rlm_child_id,
                &name,
                &error,
                now_ms(),
            ))
        } else {
            state.status = "done";
            // The runtime daemon's display entry reads completed before the
            // parent hears the settle (the old daemon's order).
            if let Some(family) = child
                .node
                .tree
                .upgrade()
                .as_ref()
                .and_then(|tree| tree.daemon())
            {
                family
                    .completed(child.node.session_dir(), child.rlm_child_id.clone())
                    .await;
            }
            (!state.replied_since_task).then(|| {
                create_rlm_child_terminal_notice(
                    &RlmChildTerminalNotice::CompletedWithoutReply {
                        child_id: child.rlm_child_id.clone(),
                        session_name: name.clone(),
                        last_assistant_text_preview: state
                            .answer_text
                            .as_deref()
                            .map(compact_rlm_text),
                    },
                    now_ms(),
                )
            })
        };
        // The notice queues on the parent's agent while this lock is held:
        // every reader of `settled` reads it under this same lock, so none
        // can observe the settled child before the parent's queue holds its
        // row.
        let notified = match (&parent, notice.filter(|_| !child.factory)) {
            (Some(parent), Some(notice)) => serde_json::to_value(notice)
                .map_err(|error| anyhow::anyhow!("the notice failed its wire conversion: {error}"))
                .and_then(|value| deliver_row(parent, value))
                .inspect_err(|error| {
                    tracing::error!("an RLM child notice was not delivered: {error:#}");
                })
                .is_ok(),
            (None, _) | (_, None) => false,
        };
        if engine
            .as_ref()
            .is_some_and(|engine| engine.session.agent().has_queued_messages())
        {
            retask(child, &mut state, trust);
        }
        notified
    };
    if let Some(parent) = parent {
        parent.activity.notify_waiters();
        if notified {
            wake_on_delivery(&parent, trust).await;
        }
    }
}

/// Starts a settled child's next task over the rows delivered to it: its
/// state reads running again, and an empty input on `agent/<child>/input`
/// (with `trust`) queues the task. The caller holds the child's state.
fn retask(child: &Child, state: &mut ChildState, trust: Trust) {
    state.settled = false;
    state.status = "running";
    state.started_at_ms = now_ms();
    state.error = None;
    if let Err(error) = wake(&child.node, trust, /*priority*/ None) {
        tracing::error!(
            "subagent {} was not given its next task: {error:#}",
            child.rlm_child_id
        );
    }
}

/// Has `node`'s session take a row just delivered to it, with `trust`: a
/// settled child is given its next task over it; a root in the runtime
/// daemon is woken by a `Steer` empty input (a running turn that still
/// reads steering takes it, otherwise it runs as its own task over the
/// rows, the old daemon's notice turn). A root in a headless run leaves it
/// to its settle.
async fn wake_on_delivery(node: &Arc<Node>, trust: Trust) {
    let Some(parent) = node.parent_node() else {
        let in_daemon = node
            .tree
            .upgrade()
            .is_some_and(|tree| tree.daemon().is_some());
        if in_daemon && !node.is_disposed() {
            if let Err(error) = wake(node, trust, Some(Priority::Steer)) {
                tracing::error!(
                    "session {} was not woken for its delivered row: {error:#}",
                    node.session_id
                );
            }
        }
        return;
    };
    for child in parent.children.lock().await.iter() {
        if Arc::ptr_eq(&child.node, node) {
            let mut state = child.state.lock().await;
            if state.settled && !node.is_disposed() {
                retask(child, &mut state, trust);
            }
        }
    }
}

/// Queues a task on `node`'s session that runs the rows delivered to it: an
/// empty input on its `agent/<id>/input`, with `trust` and `priority` (the
/// route's when `None`).
fn wake(node: &Node, trust: Trust, priority: Option<Priority>) -> anyhow::Result<()> {
    let tree = node
        .tree
        .upgrade()
        .ok_or_else(|| anyhow::anyhow!("the runtime is not running"))?;
    let input = AgentInput {
        text: String::new(),
        images: Vec::new(),
        priority,
    };
    tree.outbox.send(NewEvent {
        topic: Topic::new(format!("agent/{}/input", node.session_id))?,
        source: node.parent_node().map_or_else(
            || node.session_id.clone(),
            |parent| parent.session_id.clone(),
        ),
        session: Some(node.session_id.clone()),
        payload: serde_json::to_value(input)?,
        trust,
        cause: None,
        reply: None,
    });
    Ok(())
}

/// Delivers one custom row to a node's session: steers its loop form into
/// the agent (a streaming session takes it at its next turn boundary; the
/// quiescence drive runs it when idle), then fires the node's activity.
/// One delivery path for tasks, replies and notices alike.
fn deliver_row(node: &Arc<Node>, row: Value) -> anyhow::Result<()> {
    let _delivery_guard = lock(&node.delivery_gate);
    if node.is_disposed() {
        anyhow::bail!("Cannot deliver to a disposed inline session");
    }
    let custom: CustomMessage = serde_json::from_value(row).map_err(|error| {
        anyhow::anyhow!("the delivered row failed its wire conversion: {error}")
    })?;
    let engine = node
        .engine()
        .ok_or_else(|| anyhow::anyhow!("the target session is not running"))?;
    let loop_row = custom_message_to_loop_row(&custom)
        .ok_or_else(|| anyhow::anyhow!("the delivered row failed its loop conversion"))?;
    engine.session.agent().steer(loop_row);
    node.activity.notify_waiters();
    Ok(())
}

/// Ends one child and its subtree: each is marked disposed, its tasks end
/// (a running turn is aborted), its kernel is disposed with a final
/// namespace snapshot, and its session closes. No child kernel outlives
/// the run.
async fn dispose_child(child: &Arc<Child>) {
    let mut pending = vec![Arc::clone(child)];
    while let Some(current) = pending.pop() {
        let node = &current.node;
        {
            let _delivery_guard = lock(&node.delivery_gate);
            node.disposed.store(true, Ordering::SeqCst);
        }
        node.activity.notify_waiters();
        let tree = node.tree.upgrade();
        let control = tree.as_ref().and_then(|tree| tree.control().ok());
        if let Some(control) = &control {
            if let Err(error) = control.cancel(&node.session_id).await {
                tracing::error!(
                    "subagent {}'s tasks did not end: {error:#}",
                    current.rlm_child_id
                );
            }
        }
        if let Some(engine) = node.engine() {
            engine.dispose_kernel().await;
        }
        if let Some(control) = &control {
            control.close(&node.session_id).await;
        }
        if let Some(tree) = &tree {
            if let Some(attached) = tree.attached.get() {
                lock(&attached.registry)
                    .remove(&format!("{SUBSCRIPTION_PREFIX}{}", node.session_id));
            }
            lock(&tree.nodes).remove(&node.session_id);
        }
        pending.extend(std::mem::take(&mut *node.children.lock().await));
    }
}

/// Ends every subagent of `root`'s session; nothing is delivered to it
/// afterwards.
pub(crate) async fn dispose(root: &Arc<Node>) {
    {
        let _delivery_guard = lock(&root.delivery_gate);
        root.disposed.store(true, Ordering::SeqCst);
    }
    root.activity.notify_waiters();
    let children = std::mem::take(&mut *root.children.lock().await);
    for child in children {
        dispose_child(&child).await;
    }
}
