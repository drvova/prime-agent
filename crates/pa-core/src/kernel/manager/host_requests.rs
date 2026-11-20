//! Host request handling: execute-side requests answered by the host
//! (harness/goal/etc.) and their settle/exit waits.
//!
//! Dispatch order: a request from the cell the turn is executing starts at once; background
//! requests (detached tasks, backgrounded cells, a cell's gathered fan-out) share a soft bound
//! of in-flight handlers and wait in arrival order beyond it, so a spawn flood can never starve
//! the cell. Every request is answered: a queued one starts when a background handler
//! finishes, and teardown starts whatever is still queued before it drains.

use std::collections::VecDeque;

use super::{
    anyhow, json, lock, Arc, Duration, HostRequestOrigin, HostRequestPayload, Inner, Value,
    END_TURN_REQUEST_TYPE, MAX_HANDLED_HOST_REQUEST_IDS,
};

/// Background host requests in flight past which new ones queue.
const BACKGROUND_SOFT_BOUND: usize = 32;

/// What to do with an arriving request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dispatch {
    /// Start its handler now with the payload.
    Start(Value),
    /// It waits for a background slot.
    Queue,
}

/// The background half of host-request dispatch: the count in flight and the requests waiting
/// for a slot, in arrival order.
#[derive(Debug, Default)]
pub(crate) struct HostDispatch {
    background_in_flight: usize,
    queued: VecDeque<(String, Value)>,
}

impl HostDispatch {
    /// Admit one arriving request: the cell's start at once; a background request starts while
    /// the soft bound has room and queues past it.
    fn arrive(&mut self, request_id: &str, data: Value, origin: HostRequestOrigin) -> Dispatch {
        match origin {
            HostRequestOrigin::Cell => Dispatch::Start(data),
            HostRequestOrigin::Background => {
                if self.background_in_flight < BACKGROUND_SOFT_BOUND {
                    self.background_in_flight += 1;
                    Dispatch::Start(data)
                } else {
                    self.queued.push_back((request_id.to_string(), data));
                    Dispatch::Queue
                }
            }
        }
    }

    /// A background handler finished: the next queued request takes its slot.
    fn finish_background(&mut self) -> Option<(String, Value)> {
        self.background_in_flight = self.background_in_flight.saturating_sub(1);
        let next = self.queued.pop_front();
        if next.is_some() {
            self.background_in_flight += 1;
        }
        next
    }

    /// Everything still queued, for teardown to start regardless of the bound.
    fn drain(&mut self) -> Vec<(String, Value)> {
        self.background_in_flight += self.queued.len();
        self.queued.drain(..).collect()
    }
}

impl Inner {
    /// Dispatch one typed request from kernel code and reply over the
    /// protocol: the manager answers `rlm.end_turn` itself, registered
    /// handlers answer the rest, and unhandled requests answer with an error.
    pub(crate) fn start_host_request(
        self: &Arc<Self>,
        request_id: &str,
        data: Value,
        origin: HostRequestOrigin,
    ) {
        let admitted = {
            let mut g = lock(&self.guarded);
            let (seen, order) = &mut g.handled_host_request_ids;
            if seen.contains(request_id) {
                return;
            }
            seen.insert(request_id.to_string());
            order.push_back(request_id.to_string());
            while seen.len() > MAX_HANDLED_HOST_REQUEST_IDS {
                if let Some(oldest) = order.pop_front() {
                    seen.remove(&oldest);
                } else {
                    break;
                }
            }
            g.host_dispatch.arrive(request_id, data, origin)
        };
        match admitted {
            Dispatch::Start(data) => self.spawn_host_request(request_id.to_string(), data, origin),
            Dispatch::Queue => {}
        }
    }

    /// Start every queued background request (teardown answers them all before it drains).
    pub(crate) fn start_queued_host_requests(self: &Arc<Self>) {
        let queued = lock(&self.guarded).host_dispatch.drain();
        for (request_id, data) in queued {
            self.spawn_host_request(request_id, data, HostRequestOrigin::Background);
        }
    }

    fn spawn_host_request(
        self: &Arc<Self>,
        request_id: String,
        data: Value,
        origin: HostRequestOrigin,
    ) {
        let inner = Arc::clone(self);
        let task = tokio::spawn(async move {
            let result = inner.handle_host_request(&data).await;
            let reply = match result {
                Ok(result) => json!({ "status": "ok", "result": result }),
                Err(error) => {
                    inner.append_diagnostic(&format!(
                        "host request failed for {request_id}: {error:#}"
                    ));
                    json!({ "status": "error", "error": format!("{error:#}") })
                }
            };
            let frame = json!({ "type": "host_reply", "id": request_id, "data": reply });
            if let Err(error) = inner.write_line(&frame).await {
                inner.append_diagnostic(&format!(
                    "failed to send host request reply for {request_id}: {error:#}"
                ));
            }
            match origin {
                HostRequestOrigin::Cell => {}
                HostRequestOrigin::Background => {
                    let next = lock(&inner.guarded).host_dispatch.finish_background();
                    if let Some((request_id, data)) = next {
                        inner.spawn_host_request(request_id, data, HostRequestOrigin::Background);
                    }
                }
            }
        });
        let mut g = lock(&self.guarded);
        // Completed task handles are dropped so the inflight set stays bounded.
        g.host_inflight.retain(|handle| !handle.is_finished());
        g.host_inflight.push(task);
    }

    async fn handle_host_request(&self, data: &Value) -> anyhow::Result<Value> {
        let Some(obj) = data.as_object() else {
            return Err(anyhow!("host request payload must be an object"));
        };
        let request_type = obj
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("host request payload must have a string type"))?;
        if request_type == END_TURN_REQUEST_TYPE {
            // The manager's own answer: mark the executing cell, and the
            // flag rides the settled result so the turn ends once the
            // batch settles.
            let execution = lock(&self.guarded)
                .active_execution
                .clone()
                .ok_or_else(|| anyhow!("rlm.end_turn has no running cell to end the turn for"))?;
            let mut buffers = lock(&execution.buffers);
            // A cell that settled between the clone and this write could
            // not carry the flag: answer the error, never a silent no-op.
            if buffers.settled {
                return Err(anyhow!("rlm.end_turn arrived after its cell settled"));
            }
            buffers.ended_turn = true;
            return Ok(json!({ "endedTurn": true }));
        }
        let handler = self
            .options
            .host_handlers
            .get(request_type)
            .ok_or_else(|| {
                anyhow!("host request type \"{request_type}\" is not available in this session")
            })?
            .clone();
        handler(HostRequestPayload { data: data.clone() }).await
    }

    /// Wait (bounded) for the in-flight host request tasks to settle.
    pub(crate) async fn wait_for_host_requests_to_settle(
        &self,
        tasks: Vec<tokio::task::JoinHandle<()>>,
        timeout_ms: u64,
    ) {
        let all = async {
            for task in tasks {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(Duration::from_millis(timeout_ms), all)
            .await
            .is_err()
        {
            self.append_diagnostic(&format!(
                "timed out waiting {timeout_ms}ms for host request task(s) during shutdown"
            ));
        }
    }

    pub(crate) async fn wait_for_kernel_exit(&self) {
        let exit_rx = match lock(&self.child).as_ref() {
            Some(child) => child.exit_rx.clone(),
            None => return,
        };
        let mut exit_rx = exit_rx;
        if exit_rx.borrow().is_some() {
            return;
        }
        loop {
            if exit_rx.borrow().is_some() {
                return;
            }
            if exit_rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cells_requests_start_at_once_and_background_ones_queue_past_the_bound_in_order() {
        let mut dispatch = HostDispatch::default();
        let mut decisions = Vec::new();
        for index in 0..BACKGROUND_SOFT_BOUND + 2 {
            decisions.push(dispatch.arrive(
                &format!("bg{index}"),
                json!(index),
                HostRequestOrigin::Background,
            ));
        }
        // Past the bound, the cell's request still starts.
        decisions.push(dispatch.arrive("cell", json!("cell"), HostRequestOrigin::Cell));
        let mut expected: Vec<Dispatch> = (0..BACKGROUND_SOFT_BOUND)
            .map(|index| Dispatch::Start(json!(index)))
            .collect();
        expected.extend([
            Dispatch::Queue,
            Dispatch::Queue,
            Dispatch::Start(json!("cell")),
        ]);
        assert_eq!(decisions, expected);
        // Finishing background handlers starts the queued ones in arrival order, then frees.
        let bound = BACKGROUND_SOFT_BOUND;
        assert_eq!(
            [
                dispatch.finish_background(),
                dispatch.finish_background(),
                dispatch.finish_background(),
            ],
            [
                Some((format!("bg{bound}"), json!(bound))),
                Some((format!("bg{}", bound + 1), json!(bound + 1))),
                None,
            ]
        );
        assert_eq!(dispatch.background_in_flight, BACKGROUND_SOFT_BOUND - 1);
    }

    #[test]
    fn teardown_starts_every_queued_request() {
        let mut dispatch = HostDispatch {
            background_in_flight: BACKGROUND_SOFT_BOUND,
            queued: VecDeque::new(),
        };
        for index in 0..3 {
            dispatch.arrive(
                &format!("q{index}"),
                json!(index),
                HostRequestOrigin::Background,
            );
        }
        assert_eq!(
            dispatch.drain(),
            vec![
                ("q0".to_string(), json!(0)),
                ("q1".to_string(), json!(1)),
                ("q2".to_string(), json!(2)),
            ]
        );
        assert_eq!(dispatch.finish_background(), None);
    }
}
