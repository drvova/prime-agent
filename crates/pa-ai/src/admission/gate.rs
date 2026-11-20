//! The admission gate on the provider request path: the provider is invoked only once the
//! request holds a permit, and the permit goes back with what the request observed (outcome,
//! time to the first event, the provider's rate-limit headers).

use std::sync::{Arc, Mutex};

use pa_types::control::{LimitReport, Observation, Outcome};

use super::{
    current_source, note_outcome, note_wait, now_ms, parse_limit_report, process_priority,
    AdmissionKey, AdmissionRequest, Grant,
};
use crate::event_stream::{
    initial_assistant_message, AssistantMessageEventStream, AssistantMessageEventWriter,
};
use crate::registry::Provider;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, ErrorStopReason, Model, ProviderResponse,
    SimpleStreamOptions, StopReason, StreamOptions,
};
use crate::utils_inner::prewarm::OriginActivity;

/// A server-requested wait longer than this is a quota reset, not pacing: the turn fails with
/// the provider's error, so the admission controller does not block the key for it.
const PACING_RETRY_AFTER_CAP_MS: u64 = 60_000;

/// The options of one provider call, in either facade's shape.
pub(crate) enum CallOptions {
    Native(Option<StreamOptions>),
    Simple(SimpleStreamOptions),
}

impl CallOptions {
    fn base(&self) -> Option<&StreamOptions> {
        match self {
            Self::Native(options) => options.as_ref(),
            Self::Simple(options) => Some(&options.base),
        }
    }

    fn base_mut(&mut self) -> Option<&mut StreamOptions> {
        match self {
            Self::Native(options) => options.as_mut(),
            Self::Simple(options) => Some(&mut options.base),
        }
    }

    fn start(
        &self,
        provider: &dyn Provider,
        model: &Model,
        context: &Context,
    ) -> AssistantMessageEventStream {
        match self {
            Self::Native(options) => provider.stream(model, context, options.as_ref()),
            Self::Simple(options) => provider.stream_simple(model, context, Some(options)),
        }
    }
}

/// Start `provider` on `model` once the request holds a provider permit. Unpaced providers
/// (scripted fixtures) start at once. The returned stream carries the provider's events
/// verbatim; an abort while waiting ends it as an aborted message without a request.
pub(crate) fn admitted_stream(
    provider: Arc<dyn Provider>,
    model: &Model,
    context: &Context,
    mut options: CallOptions,
) -> AssistantMessageEventStream {
    if !provider.paced() {
        return options.start(provider.as_ref(), model, context);
    }
    let request = AdmissionRequest {
        key: AdmissionKey::for_request(
            model,
            options.base().and_then(|base| base.api_key.as_deref()),
        ),
        priority: options
            .base()
            .and_then(|base| base.priority)
            .unwrap_or_else(process_priority),
        enqueued_at_ms: now_ms(),
    };
    // The response hook records the status and headers for the observation; the caller's own
    // hook still runs.
    let response: Arc<Mutex<Option<ProviderResponse>>> = Arc::new(Mutex::new(None));
    let signal = options.base().and_then(|base| base.signal.clone());
    if let Some(base) = options.base_mut() {
        let caller_hook = base.on_response.take();
        let captured = Arc::clone(&response);
        base.on_response = Some(Arc::new(move |seen: ProviderResponse, model: &Model| {
            *captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(seen.clone());
            if let Some(hook) = &caller_hook {
                hook(seen, model);
            }
        }));
    }
    let (writer, outer) = AssistantMessageEventStream::new();
    let source = current_source();
    // The origin's pooled connection is in use until the request settles; its idle clock (the
    // turn-start re-warm's input) restarts then.
    let activity = OriginActivity::start(&model.api, &model.base_url);
    // The common case waits for nothing: the provider starts here, as it would unpaced, and
    // only the forwarding (which reads the outcome back) runs on a task.
    if let Some(grant) = source.try_acquire(&request) {
        let started = std::time::Instant::now();
        let inner = options.start(provider.as_ref(), model, context);
        tokio::spawn(forward(inner, writer, grant, response, started, activity));
        return outer;
    }
    let model = model.clone();
    let context = context.clone();
    tokio::spawn(async move {
        let mut acquire = source.acquire(request);
        let grant = match &signal {
            Some(signal) => tokio::select! {
                grant = &mut acquire => grant,
                () = signal.cancelled() => {
                    // Let the acquire settle and hand its permit straight back: a remote
                    // source may already be granting a lease that only a release returns.
                    tokio::spawn(async move { drop(acquire.await) });
                    let mut aborted = initial_assistant_message(&model.api, &model.provider, &model.id);
                    aborted.stop_reason = StopReason::Aborted;
                    writer.push(AssistantMessageEvent::Error {
                        reason: ErrorStopReason::Aborted,
                        error: aborted,
                    });
                    return;
                }
            },
            None => acquire.await,
        };
        note_wait(grant.waited_ms);
        let started = std::time::Instant::now();
        let inner = options.start(provider.as_ref(), &model, &context);
        forward(inner, writer, grant, response, started, activity).await;
    });
    outer
}

/// Forward the provider's events verbatim, then return the permit with what the request
/// observed.
async fn forward(
    mut inner: AssistantMessageEventStream,
    writer: AssistantMessageEventWriter,
    grant: Grant,
    response: Arc<Mutex<Option<ProviderResponse>>>,
    started: std::time::Instant,
    activity: OriginActivity,
) {
    let mut first_output_ms = None;
    while let Some(event) = inner.next_event().await {
        // The latency sample is the time to the first model output: providers push `Start`
        // before the response arrives.
        if !matches!(event, AssistantMessageEvent::Start { .. }) {
            first_output_ms.get_or_insert_with(|| {
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
        }
        writer.push(event);
    }
    let result = inner.result().await;
    writer.end(Some(result.clone()));
    let response = response
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let observation = observe(&result, response.as_ref(), first_output_ms);
    note_outcome(observation.outcome);
    if grant.finish(observation) {
        super::note_decrease();
    }
    drop(activity);
}

/// What one finished request tells the controller.
fn observe(
    result: &AssistantMessage,
    response: Option<&ProviderResponse>,
    first_event_ms: Option<u64>,
) -> Observation {
    let mut limits = response.map_or_else(LimitReport::default, |response| {
        parse_limit_report(&response.headers)
    });
    limits.retry_after_ms = limits
        .retry_after_ms
        .filter(|wait| *wait <= PACING_RETRY_AFTER_CAP_MS);
    let outcome = match result.stop_reason {
        StopReason::Error => {
            let failure = result
                .diagnostics
                .iter()
                .flatten()
                .find(|diagnostic| diagnostic.type_ == "provider_stream_failure")
                .and_then(|diagnostic| diagnostic.details.as_ref());
            let kind = failure
                .and_then(|details| details.get("kind"))
                .and_then(serde_json::Value::as_str);
            let status = failure
                .and_then(|details| details.get("status"))
                .and_then(serde_json::Value::as_u64)
                .or_else(|| response.map(|response| u64::from(response.status)));
            match (kind, status) {
                (Some("rate_limit"), _) | (_, Some(429)) => Outcome::Throttled,
                (Some("overloaded"), _) | (_, Some(503 | 529)) => Outcome::Overloaded,
                _ => Outcome::Failed,
            }
        }
        StopReason::Aborted => Outcome::Failed,
        StopReason::Stop | StopReason::Length | StopReason::ToolUse => Outcome::Success,
    };
    Observation {
        outcome,
        latency_ms: first_event_ms.filter(|_| outcome == Outcome::Success),
        limits,
    }
}
