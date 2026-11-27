//! Where a `Call` or `Agent` route hands its work: the later runtime's model
//! caller and task queue.

use std::future::Future;
use std::sync::Arc;

use pa_types::event_log::{AgentTarget, Event, Priority, Trust};

/// The one model call without tools a `Call` route asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCall {
    /// The subscription whose route this is.
    pub subscription: String,
    pub prompt: String,
    pub model: Option<String>,
    /// The event that matched; its `reply` topic is where the answer goes.
    pub event: Arc<Event>,
    /// The lower of the subscription's and the event's trust.
    pub trust: Trust,
}

/// Makes the model calls `Call` routes ask for.
///
/// `call` queues the call and returns: the bus awaits it while it runs
/// routes, so it must not await the model. The implementation makes the call
/// on its own task and publishes the answer as an event caused by
/// `call.event` (`Event::cause`, so the answer carries at most `call.trust`
/// and counts against the hop limit) on the event's `reply` topic. An error
/// from `call` (the call could not be queued) is reported as a
/// `system/route_failed` event; a failure of the call itself is the
/// implementation's to publish.
pub trait ModelCaller: Send + Sync + 'static {
    /// Queues `call`.
    fn call(&self, call: ModelCall) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// The turn an `Agent` route asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRequest {
    /// The subscription whose route this is.
    pub subscription: String,
    pub target: AgentTarget,
    pub prompt: String,
    pub priority: Priority,
    /// The event that matched: the task's trigger.
    pub event: Arc<Event>,
    /// The lower of the subscription's and the event's trust: the task runs
    /// with at most this trust.
    pub trust: Trust,
}

/// Turns `Agent` routes into tasks.
///
/// `queue` stores the task (starting a session first for
/// `AgentTarget::Spawn`) and returns once it is queued, without waiting for
/// it to run: the bus awaits it while it runs routes. The task's trust is
/// never above `request.trust`. An error is reported as a
/// `system/route_failed` event.
pub trait TaskSink: Send + Sync + 'static {
    /// Queues the task `request` asks for.
    fn queue(&self, request: TaskRequest) -> impl Future<Output = anyhow::Result<()>> + Send;
}
