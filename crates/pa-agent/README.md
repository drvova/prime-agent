# pa-agent

The agent loop.

## Scope
Turn loop: drives a provider stream, dispatches tool calls, enforces max-turn and sequencing semantics, handles partial streams, retries on stream failure, propagates aborts.

## Non-goals
No tool implementations (pa-core), no providers (pa-ai), no session persistence, no UI. The loop is tool-agnostic: it receives a `ToolDispatcher` boundary.

## Public API
`run_loop` (session inputs -> stream of loop events), `ToolDispatcher` trait. Loop internals `pub(crate)`. `Agent::steering_batches` and `Agent::follow_up_batches` copy the queued batches, in order (the runtime daemon's queue view lists the rows waiting on its agent). `Agent::steer_ahead_of` queues a steering batch ahead of the run of queued batches at the end that a predicate matches (the runtime daemon puts a client's steer ahead of the custom rows waiting, as the old daemon's steering lane ranks a user row above a custom one). `Agent::any_queued_message` asks whether a queued steering or follow-up message matches, leaving the queues as they are (the runtime daemon's `agent_message.wait` backlog reads it). `Agent::model` reads the session model without the transcript clone `Agent::state` makes (the billing stamp reads it per recorded message). `Agent::reserve_idle` reserves the boundary after a
complete run (including tools and internal follow-ups) against new prompt or
continuation admission. Its guard is for short context commits; callers must
release it before waiting on model planning or contended storage locks.

## Depends on
No workspace dependencies.
